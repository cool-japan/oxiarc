//! Bounded, truly incremental Zstandard decoding.
//!
//! [`ZstdStream`] is a *push* decoder: the caller hands it whatever compressed
//! bytes it has and whatever output space it has, and the decoder reports how
//! much of each it used. It never buffers the whole compressed input, never
//! materialises the whole decompressed output, and is resumable at every point
//! where the input runs dry.
//!
//! # Memory bound
//!
//! Per-call memory is `window + one block`, constant in the length of the
//! stream:
//!
//! * the sliding window ring ([`crate::window::ZstdWindow`]), sized lazily to
//!   `min(declared Window_Size, dictionary + bytes actually produced)` and
//!   refused outright above [`ZstdStream::with_max_window`] (default 8 MiB);
//! * one block carry — a block is length-prefixed, so the decoder always knows
//!   exactly how many bytes it needs before it needs them. The carry holds at
//!   most one 128 KiB block payload plus, transiently, the not-yet-reclaimed
//!   prefix of the previous one: dead bytes are only compacted away once they
//!   are at least half the buffer (compacting on every call would be O(n²) under
//!   a byte-at-a-time feed), so the buffered length stays strictly below
//!   **256 KiB**;
//! * one literals buffer and one sequence vector, reused across blocks.
//!
//! # Resumability
//!
//! Every *structural* boundary is resumable byte by byte: the frame magic, the
//! frame header, block headers, block payloads, the drain of a decoded block
//! into the caller's slice, and the frame checksum. A block's interior —
//! the Huffman/FSE table descriptions and the backward sequence bitstream — is
//! decoded atomically once the whole (≤ 128 KiB) block payload is buffered.
//! That is a deliberate, bounded choice: the alternative is a mid-bitstream
//! state machine whose worst-case memory is the same, and RFC 8878 guarantees
//! the bound. Bytes are moved out of `input` exactly once; anything held back
//! for a partially received structure lives in the carry and is never
//! re-consumed.
//!
//! # Output budget
//!
//! [`ZstdStream::with_max_output`] is enforced **before** decoding wherever the
//! format declares a size: `Frame_Content_Size` bounds a whole frame, and a
//! `Raw`/`RLE` block header bounds that block exactly. A `Compressed` block
//! does not declare its regenerated size, so it is decoded into the window
//! (bounded by `Block_Maximum_Decompressed_Size`, at most 128 KiB, regardless
//! of the budget) and charged before a single byte of it is handed back.
//!
//! The number of bytes the caller ever *receives* is therefore capped exactly:
//! [`ZstdStream::total_out`] never exceeds the budget, and a stream that would
//! go past it fails on the call that decodes the offending block. The bounded
//! overshoot is one block of decode work and ring space — never an allocation
//! the decoder was not going to make anyway, and never delivered output.
//!
//! A budget overrun is always [`OxiArcError::MemoryBudgetExceeded`], never a
//! corrupted-data error: a caller has to be able to tell a compression bomb
//! from a malformed frame.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_zstd::{ZstdStatus, ZstdStream, compress_with_level};
//! use oxiarc_core::traits::FlushMode;
//!
//! let original = b"streaming zstd, one byte at a time".repeat(64);
//! let frame = compress_with_level(&original, 3).expect("compress");
//!
//! let mut stream = ZstdStream::new().with_max_output(1 << 20);
//! let mut out = Vec::new();
//! let mut scratch = [0u8; 64];
//! let mut pos = 0;
//!
//! loop {
//!     // Feed one byte at a time; take at most 64 bytes back.
//!     let end = (pos + 1).min(frame.len());
//!     let flush = if end == frame.len() { FlushMode::Finish } else { FlushMode::None };
//!     let p = stream.decode(&frame[pos..end], &mut scratch, flush).expect("decode");
//!     pos += p.consumed;
//!     out.extend_from_slice(&scratch[..p.produced]);
//!     if p.status == ZstdStatus::StreamEnd {
//!         break;
//!     }
//! }
//! assert_eq!(out, original);
//! ```

use crate::frame::{FrameHeader, frame_header_len, parse_frame_header};
use crate::literals::LiteralsDecoder;
use crate::sequences::{Sequence, SequencesDecoder};
use crate::window::ZstdWindow;
use crate::xxhash::XxHash64;
use crate::{BlockType, MAX_BLOCK_SIZE, MAX_WINDOW_SIZE, ZSTD_MAGIC};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::FlushMode;

/// Largest frame header, magic included (RFC 8878 §3.1.1).
const MAX_FRAME_HEADER: usize = 18;

/// Zstandard frame magic as a little-endian `u32`.
const ZSTD_MAGIC_U32: u32 = 0xFD2F_B528;

/// Window ceiling used by the derived, back-compatible entry points.
///
/// [`ZstdStream`] itself defaults to [`MAX_WINDOW_SIZE`] and refuses a frame
/// declaring more, which is the right default for untrusted input. The legacy
/// [`crate::ZstdStreamDecoder`] and the one-shot helpers never had such a
/// limit, and reference frames produced with `zstd --long` routinely declare
/// 16-128 MiB, so those paths keep the declaration unrestricted and rely on the
/// output budget plus lazy window growth (the ring never exceeds the number of
/// bytes actually produced) for their memory bound.
pub(crate) const UNRESTRICTED_MAX_WINDOW: usize = usize::MAX;

/// Outcome of one [`ZstdStream::decode`] call.
///
/// Mirrors `oxiarc_deflate`'s `InflateProgress` field for field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZstdProgress {
    /// Bytes taken from `input` by this call.
    pub consumed: usize,
    /// Bytes written into `output` by this call.
    pub produced: usize,
    /// What the decoder needs next.
    pub status: ZstdStatus,
}

/// What a [`ZstdStream`] needs in order to make further progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ZstdStatus {
    /// More compressed input is required.
    NeedInput,
    /// The output slice is full; call again with space.
    NeedOutput,
    /// The stream is complete. No further input will be consumed.
    StreamEnd,
}

/// Internal decoder position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Waiting for a 4-byte frame magic (or a clean end of stream).
    FrameMagic,
    /// Waiting for the rest of the frame header.
    FrameHeader,
    /// Waiting for a skippable frame's 4-byte size field.
    SkippableSize,
    /// Discarding a skippable frame's payload.
    SkippableSkip {
        /// Payload bytes still to discard.
        remaining: u64,
    },
    /// Waiting for a 3-byte block header.
    BlockHeader,
    /// Waiting for a block payload.
    BlockPayload {
        /// Whether this is the frame's last block.
        last: bool,
        /// Block type from the header.
        kind: BlockType,
        /// `Block_Size` field (regenerated size for RLE blocks).
        block_size: usize,
        /// Bytes of payload to buffer (1 for RLE).
        payload_len: usize,
    },
    /// Handing a decoded block to the caller.
    Drain {
        /// Whether the drained block was the frame's last.
        last: bool,
    },
    /// Waiting for the 4-byte content checksum.
    Checksum,
    /// A frame finished; decide whether another follows.
    FrameEnd,
    /// The stream is complete.
    Done,
    /// A fault was latched; every call returns the same error.
    Failed,
}

/// A bounded, resumable Zstandard push decoder.
///
/// See the [module documentation](self) for the memory bound, the resumability
/// contract and the output-budget rules.
#[derive(Debug)]
pub struct ZstdStream {
    /// Bytes held back from previous calls (partial header or block payload).
    carry: Vec<u8>,
    /// Read cursor inside `carry`.
    carry_pos: usize,
    /// Current decoder position.
    state: State,
    /// Header of the frame being decoded.
    header: Option<FrameHeader>,
    /// Largest regenerated size a block of the current frame may have per
    /// RFC 8878: `Block_Maximum_Decompressed_Size` = `min(Window_Size, 128 KiB)`,
    /// further bounded by a declared `Frame_Content_Size`. Derived only from
    /// what the *format* declares, so exceeding it is always a format error.
    block_rfc_max: usize,
    /// Effective ceiling used while a block is decoded into the window:
    /// [`Self::block_rfc_max`] cut down to what the output budget still allows,
    /// so the ring never has to hold more than the caller agreed to receive.
    /// Exceeding it while still inside `block_rfc_max` is a budget overrun, not
    /// corruption.
    block_max: usize,
    /// Literals decoder (holds the Huffman table for `Treeless` sections).
    literals: LiteralsDecoder,
    /// Sequences decoder (holds the FSE tables and repeat offsets).
    sequences: SequencesDecoder,
    /// Sliding-window ring holding the LZ77 history.
    window: ZstdWindow,
    /// Reusable literals buffer.
    lit_buf: Vec<u8>,
    /// Reusable sequence buffer.
    seq_buf: Vec<Sequence>,
    /// Running content checksum, when the frame declares one.
    hasher: Option<XxHash64>,
    /// Optional dictionary content, re-seeded at every frame start.
    dict: Option<Vec<u8>>,
    /// Output budget for the whole stream.
    max_output: Option<u64>,
    /// Largest `Window_Size` a frame may declare.
    max_window: usize,
    /// Whether concatenated frames are decoded.
    multi_frame: bool,
    /// Bytes produced by the current frame.
    frame_out: u64,
    /// Bytes taken from the caller's input across the whole stream.
    total_in: u64,
    /// Bytes handed to the caller across the whole stream.
    total_out: u64,
    /// Number of Zstandard frames fully decoded.
    frames_decoded: u32,
    /// Sticky fault message; set once, cleared only by [`ZstdStream::reset`].
    fault: Option<String>,
}

impl Default for ZstdStream {
    fn default() -> Self {
        Self::new()
    }
}

impl ZstdStream {
    /// Create a decoder with the default configuration.
    ///
    /// Multi-frame decoding is on (matching
    /// [`crate::decompress_multi_frame`]), the window ceiling is
    /// [`MAX_WINDOW_SIZE`] (8 MiB) and there is no output budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            carry: Vec::new(),
            carry_pos: 0,
            state: State::FrameMagic,
            header: None,
            block_rfc_max: MAX_BLOCK_SIZE,
            block_max: MAX_BLOCK_SIZE,
            literals: LiteralsDecoder::new(),
            sequences: SequencesDecoder::new(),
            window: ZstdWindow::new(),
            lit_buf: Vec::new(),
            seq_buf: Vec::new(),
            hasher: None,
            dict: None,
            max_output: None,
            max_window: MAX_WINDOW_SIZE,
            multi_frame: true,
            frame_out: 0,
            total_in: 0,
            total_out: 0,
            frames_decoded: 0,
            fault: None,
        }
    }

    /// Cap the total number of bytes the stream may produce.
    ///
    /// Enforced before decoding wherever the format declares a size, and
    /// otherwise immediately after a compressed block is decoded but *before*
    /// any of it is handed back, so [`ZstdStream::total_out`] never exceeds
    /// `limit`. An overrun is always [`OxiArcError::MemoryBudgetExceeded`]; see
    /// the [module documentation](self#output-budget).
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.max_output = Some(limit);
        self
    }

    /// Refuse frames declaring a `Window_Size` larger than `bytes`.
    ///
    /// Checked against the frame header before a single window byte is
    /// allocated. The default is [`MAX_WINDOW_SIZE`] (8 MiB); raise it to
    /// accept `zstd --long` frames, which declare 16-128 MiB.
    #[must_use]
    pub fn with_max_window(mut self, bytes: usize) -> Self {
        self.max_window = bytes;
        self
    }

    /// Decode concatenated frames (default `true`).
    ///
    /// With `false`, the stream reports [`ZstdStatus::StreamEnd`] as soon as
    /// the first frame is complete and stops consuming: the bytes that follow
    /// stay in the caller's own input slice, which `consumed` accounts for
    /// exactly, and an empty input is an error rather than an empty payload.
    /// (Bytes that ended up *inside* the decoder are a separate, much rarer
    /// case; [`ZstdStream::unused_input`] reports those.)
    #[must_use]
    pub fn with_multi_frame(mut self, yes: bool) -> Self {
        self.multi_frame = yes;
        self
    }

    /// Decode with a raw-content dictionary.
    ///
    /// The dictionary tail seeds the window at the start of *every* frame, so
    /// a multi-frame stream behaves exactly like
    /// [`crate::decompress_multi_frame_with_dict`].
    #[must_use]
    pub fn with_dictionary(mut self, dict: Vec<u8>) -> Self {
        self.dict = if dict.is_empty() { None } else { Some(dict) };
        self
    }

    /// Bytes taken from the caller's input so far.
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Bytes handed to the caller so far.
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Number of Zstandard frames fully decoded so far.
    pub fn frames_decoded(&self) -> u32 {
        self.frames_decoded
    }

    /// Current window allocation in bytes.
    ///
    /// Grows lazily; it is never larger than the number of bytes the stream has
    /// actually produced (plus the dictionary), whatever the frame declares.
    pub fn window_size(&self) -> usize {
        self.window.capacity()
    }

    /// `true` once the stream has reported [`ZstdStatus::StreamEnd`].
    pub fn is_finished(&self) -> bool {
        self.state == State::Done
    }

    /// Input bytes taken into the decoder but not part of the stream.
    ///
    /// After [`ZstdStatus::StreamEnd`] this holds the up-to-3 bytes or the
    /// 4-byte non-Zstandard magic that ended the stream, so a caller that must
    /// know exactly where the compressed data stopped can recover them.
    pub fn unused_input(&self) -> &[u8] {
        &self.carry[self.carry_pos.min(self.carry.len())..]
    }

    /// Clear all state, keeping the configuration (budget, window ceiling,
    /// multi-frame flag and dictionary) **and the buffers**.
    ///
    /// The window ring, the carry and the literals/sequence buffers keep their
    /// allocations, so a decoder reset between frames (or between HTTP
    /// responses) does no further allocation in the steady state. This also
    /// clears the sticky fault latch, so a faulted decoder becomes usable
    /// again.
    pub fn reset(&mut self) {
        self.carry.clear();
        self.carry_pos = 0;
        self.state = State::FrameMagic;
        self.header = None;
        self.block_rfc_max = MAX_BLOCK_SIZE;
        self.block_max = MAX_BLOCK_SIZE;
        self.literals.reset();
        self.sequences.reset();
        self.window.reset_keep_allocation();
        self.lit_buf.clear();
        self.seq_buf.clear();
        self.hasher = None;
        self.frame_out = 0;
        self.total_in = 0;
        self.total_out = 0;
        self.frames_decoded = 0;
        self.fault = None;
    }

    /// Assert that the stream ended cleanly.
    ///
    /// # Errors
    ///
    /// Returns the latched fault if one occurred, or a corrupted-data error if
    /// the stream stopped anywhere other than a frame boundary (a truncated
    /// frame). Note that a checksum failure surfaces here *after* the bytes
    /// have already been handed to the caller — a streaming decoder cannot
    /// un-send output, so a caller that must not act on unverified data has to
    /// treat a `finish()` error as invalidating everything it received.
    pub fn finish(&mut self) -> Result<()> {
        if let Some(msg) = &self.fault {
            return Err(OxiArcError::corrupted(self.total_in, msg.clone()));
        }
        if self.state == State::Done {
            return Ok(());
        }
        // A zero-length output slice: `finish` must never consume decoded bytes
        // the caller has not seen. Undrained output therefore surfaces as an
        // error rather than being silently dropped.
        let progress = self.decode(&[], &mut [], FlushMode::Finish)?;
        if progress.status == ZstdStatus::StreamEnd {
            return Ok(());
        }
        let message = if self.window.pending() > 0 {
            "zstd stream has decoded output that was never drained"
        } else {
            "zstd stream ended mid-frame"
        };
        Err(self.fail(OxiArcError::corrupted(self.total_in, message)))
    }

    /// Decode as much as possible from `input` into `output`.
    ///
    /// `flush` is [`FlushMode::Finish`] when `input` is the last input the
    /// caller will ever supply; any other value means more may follow. Running
    /// out of input under `Finish` anywhere but a frame boundary is an error.
    ///
    /// # Errors
    ///
    /// Any format violation, budget overrun, oversized declared window or
    /// checksum mismatch. The error is latched: every later call returns it
    /// again until [`ZstdStream::reset`].
    ///
    /// An error is terminal, so it reports no counts: bytes this particular
    /// call had already written into `output` before the fault are void and
    /// must be discarded along with the rest of the stream. Bytes handed back
    /// by *earlier*, successful calls are real — a checksum failure, for one,
    /// can only be detected after the content it covers has been streamed out
    /// — so a caller that must not act on unverified data has to buffer until
    /// [`ZstdStream::finish`] returns `Ok`.
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<ZstdProgress> {
        if let Some(msg) = &self.fault {
            return Err(OxiArcError::corrupted(self.total_in, msg.clone()));
        }

        let mut consumed = 0usize;
        let mut produced = 0usize;

        let status = loop {
            // Output space is the scarce resource: always drain first.
            if self.window.pending() > 0 {
                let hasher = &mut self.hasher;
                let n = self.window.drain_into(&mut output[produced..], |chunk| {
                    if let Some(h) = hasher.as_mut() {
                        h.update(chunk);
                    }
                });
                produced += n;
                self.total_out += n as u64;
                if self.window.pending() > 0 {
                    break ZstdStatus::NeedOutput;
                }
            }

            match self.step(input, flush, &mut consumed) {
                Ok(Some(status)) => break status,
                Ok(None) => continue,
                Err(e) => {
                    self.total_in += consumed as u64;
                    return Err(self.fail(e));
                }
            }
        };

        self.total_in += consumed as u64;
        Ok(ZstdProgress {
            consumed,
            produced,
            status,
        })
    }

    /// Run one state transition.
    ///
    /// `Ok(None)` means "made progress, go round again"; `Ok(Some(status))`
    /// means "return this to the caller".
    fn step(
        &mut self,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        match self.state {
            State::Done => Ok(Some(ZstdStatus::StreamEnd)),
            State::Failed => Err(OxiArcError::corrupted(
                self.total_in,
                "zstd stream is in a failed state",
            )),
            State::FrameMagic => self.step_frame_magic(input, flush, consumed),
            State::FrameHeader => self.step_frame_header(input, flush, consumed),
            State::SkippableSize => self.step_skippable_size(input, flush, consumed),
            State::SkippableSkip { remaining } => {
                self.step_skippable_skip(remaining, input, flush, consumed)
            }
            State::BlockHeader => self.step_block_header(input, flush, consumed),
            State::BlockPayload {
                last,
                kind,
                block_size,
                payload_len,
            } => {
                self.step_block_payload(last, kind, block_size, payload_len, input, flush, consumed)
            }
            State::Drain { last } => {
                // `decode`'s drain loop runs before every `step`, so the window
                // is empty by the time this state is reached.
                debug_assert_eq!(self.window.pending(), 0);
                self.state = if last {
                    self.after_last_block()?
                } else {
                    State::BlockHeader
                };
                Ok(None)
            }
            State::Checksum => self.step_checksum(input, flush, consumed),
            State::FrameEnd => {
                self.frames_decoded = self.frames_decoded.saturating_add(1);
                if self.multi_frame {
                    self.state = State::FrameMagic;
                    Ok(None)
                } else {
                    self.state = State::Done;
                    Ok(Some(ZstdStatus::StreamEnd))
                }
            }
        }
    }

    // -- individual states ------------------------------------------------

    /// Read the 4-byte magic that starts a frame, or end the stream cleanly.
    fn step_frame_magic(
        &mut self,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        if !self.fill(4, input, consumed) {
            if flush != FlushMode::Finish {
                return Ok(Some(ZstdStatus::NeedInput));
            }
            // At EOF: a short tail after a complete frame is a clean end, and
            // so is a completely empty multi-frame stream (this is exactly what
            // `decompress_multi_frame` does). A single-frame decoder, though,
            // must see one frame: `decompress(&[])` is an error, and so is this,
            // or an empty response body would decode to an empty payload.
            if self.frames_decoded > 0 || (self.multi_frame && self.available() == 0) {
                self.state = State::Done;
                return Ok(Some(ZstdStatus::StreamEnd));
            }
            return Err(OxiArcError::corrupted(
                self.total_in,
                if self.available() == 0 {
                    "empty input where a Zstandard frame was expected"
                } else {
                    "truncated Zstandard frame magic"
                },
            ));
        }

        let bytes = self.peek(4);
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic == ZSTD_MAGIC_U32 {
            self.state = State::FrameHeader;
            return Ok(None);
        }
        if (crate::SKIPPABLE_MAGIC_LOW..=crate::SKIPPABLE_MAGIC_HIGH).contains(&magic) {
            self.carry_pos += 4;
            self.state = State::SkippableSize;
            return Ok(None);
        }
        if self.frames_decoded > 0 {
            // Trailing garbage after at least one complete frame is tolerated
            // and left unconsumed; see `unused_input`.
            self.state = State::Done;
            return Ok(Some(ZstdStatus::StreamEnd));
        }
        Err(OxiArcError::invalid_magic(ZSTD_MAGIC, bytes.to_vec()))
    }

    /// Parse the frame header and set the frame up.
    fn step_frame_header(
        &mut self,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        if !self.fill(5, input, consumed) {
            return self.need_input(flush, "truncated Zstandard frame header");
        }
        let header_len = frame_header_len(self.peek(5))
            .ok_or_else(|| OxiArcError::corrupted(self.total_in, "malformed frame header"))?;
        debug_assert!(header_len <= MAX_FRAME_HEADER);
        if !self.fill(header_len, input, consumed) {
            return self.need_input(flush, "truncated Zstandard frame header");
        }
        let header = parse_frame_header(self.peek(header_len))?;
        self.carry_pos += header_len;
        self.begin_frame(header)?;
        self.state = State::BlockHeader;
        Ok(None)
    }

    /// Read a skippable frame's 4-byte size field.
    fn step_skippable_size(
        &mut self,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        if !self.fill(4, input, consumed) {
            return self.need_input(flush, "truncated skippable frame size");
        }
        let b = self.peek(4);
        let size = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64;
        self.carry_pos += 4;
        self.state = State::SkippableSkip { remaining: size };
        Ok(None)
    }

    /// Discard a skippable frame's payload without buffering it.
    fn step_skippable_skip(
        &mut self,
        remaining: u64,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        // Drop whatever is already in the carry first.
        let from_carry = (self.available() as u64).min(remaining) as usize;
        self.carry_pos += from_carry;
        let mut left = remaining - from_carry as u64;
        let from_input = ((input.len() - *consumed) as u64).min(left) as usize;
        *consumed += from_input;
        left -= from_input as u64;

        if left == 0 {
            // A skippable frame is not a Zstandard frame and carries no blocks:
            // the next thing on the wire is another frame magic.
            self.state = State::FrameMagic;
            return Ok(None);
        }
        self.state = State::SkippableSkip { remaining: left };
        if flush == FlushMode::Finish {
            return Err(OxiArcError::corrupted(
                self.total_in,
                "truncated skippable frame payload",
            ));
        }
        Ok(Some(ZstdStatus::NeedInput))
    }

    /// Read the 3-byte block header.
    fn step_block_header(
        &mut self,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        if !self.fill(3, input, consumed) {
            return self.need_input(flush, "truncated Zstandard block header");
        }
        let b = self.peek(3);
        let raw = u32::from_le_bytes([b[0], b[1], b[2], 0]);
        self.carry_pos += 3;

        let last = (raw & 1) != 0;
        let kind = BlockType::from_bits(((raw >> 1) & 0x03) as u8)?;
        let block_size = ((raw >> 3) & 0x1F_FFFF) as usize;
        if block_size > MAX_BLOCK_SIZE {
            return Err(OxiArcError::corrupted(
                self.total_in,
                format!("block size {block_size} exceeds maximum"),
            ));
        }
        if kind == BlockType::Reserved {
            return Err(OxiArcError::corrupted(self.total_in, "reserved block type"));
        }

        // Exact pre-decode budget check for the block types that declare their
        // regenerated size in the header.
        if matches!(kind, BlockType::Raw | BlockType::Rle) {
            self.check_budget(block_size as u64)?;
            if block_size > self.block_rfc_max {
                return Err(OxiArcError::corrupted(
                    self.total_in,
                    format!(
                        "block regenerated size {} exceeds the frame maximum {}",
                        block_size, self.block_rfc_max
                    ),
                ));
            }
        } else if self.budget_remaining() == Some(0) {
            return Err(self.budget_error(self.total_out.saturating_add(1)));
        }

        let payload_len = if kind == BlockType::Rle {
            1
        } else {
            block_size
        };
        self.state = State::BlockPayload {
            last,
            kind,
            block_size,
            payload_len,
        };
        Ok(None)
    }

    /// Buffer a whole block payload and decode it into the window.
    #[allow(clippy::too_many_arguments)]
    fn step_block_payload(
        &mut self,
        last: bool,
        kind: BlockType,
        block_size: usize,
        payload_len: usize,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        // Fast path: nothing carried over and the whole payload is already in
        // `input`, so decode straight from the caller's slice with no copy.
        let direct = self.available() == 0 && input.len() - *consumed >= payload_len;
        if !direct && !self.fill(payload_len, input, consumed) {
            return self.need_input(flush, "truncated Zstandard block payload");
        }

        let limits = self.block_limits();
        let payload_start = *consumed;
        let regenerated = {
            let Self {
                carry,
                carry_pos,
                literals,
                sequences,
                window,
                lit_buf,
                seq_buf,
                ..
            } = self;
            let data: &[u8] = if direct {
                &input[payload_start..payload_start + payload_len]
            } else {
                &carry[*carry_pos..*carry_pos + payload_len]
            };
            decode_block(
                literals, sequences, window, lit_buf, seq_buf, kind, block_size, data, &limits,
            )
        }?;

        if direct {
            *consumed += payload_len;
        } else {
            self.carry_pos += payload_len;
        }

        // Compressed blocks do not declare their regenerated size; the bytes
        // are now sitting in the window's pending suffix, so charging them is
        // a projected-total check with no further allowance.
        if kind == BlockType::Compressed {
            self.check_budget(0)?;
        }
        self.frame_out += regenerated as u64;
        if let Some(expected) = self.header.as_ref().and_then(|h| h.content_size) {
            if self.frame_out > expected {
                return Err(OxiArcError::corrupted(
                    self.total_in,
                    format!(
                        "content size mismatch: declared {expected}, already produced {}",
                        self.frame_out
                    ),
                ));
            }
        }

        self.state = State::Drain { last };
        Ok(None)
    }

    /// Verify the 4-byte content checksum.
    fn step_checksum(
        &mut self,
        input: &[u8],
        flush: FlushMode,
        consumed: &mut usize,
    ) -> Result<Option<ZstdStatus>> {
        if !self.fill(4, input, consumed) {
            return self.need_input(flush, "missing Zstandard content checksum");
        }
        let b = self.peek(4);
        let expected = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        self.carry_pos += 4;
        let computed = self
            .hasher
            .as_ref()
            .map(XxHash64::finish_checksum)
            .unwrap_or(expected);
        if expected != computed {
            return Err(OxiArcError::CrcMismatch { expected, computed });
        }
        self.state = State::FrameEnd;
        Ok(None)
    }

    /// Decide what follows the frame's last block.
    fn after_last_block(&mut self) -> Result<State> {
        if let Some(expected) = self.header.as_ref().and_then(|h| h.content_size) {
            if self.frame_out != expected {
                return Err(OxiArcError::corrupted(
                    self.total_in,
                    format!(
                        "content size mismatch: expected {expected}, got {}",
                        self.frame_out
                    ),
                ));
            }
        }
        let has_checksum = self.header.as_ref().is_some_and(|h| h.has_checksum);
        Ok(if has_checksum {
            State::Checksum
        } else {
            State::FrameEnd
        })
    }

    /// Configure the window, hasher and budgets for a freshly parsed header.
    fn begin_frame(&mut self, header: FrameHeader) -> Result<()> {
        if header.declared_window_size > self.max_window as u64 {
            return Err(OxiArcError::MemoryBudgetExceeded {
                budget: self.max_window,
                requested: header.declared_window_size.try_into().unwrap_or(usize::MAX),
            });
        }
        if let Some(size) = header.content_size {
            // Exact, pre-decode budget check for the whole frame.
            self.check_budget(size)?;
        }

        let dict_len = self.dict.as_ref().map_or(0, Vec::len);
        // How far back a match in this frame can legitimately point.
        let mut reach = header.declared_window_size;
        let addressable = match (header.content_size, self.budget_remaining()) {
            (Some(cs), Some(rem)) => Some(cs.min(rem)),
            (Some(cs), None) => Some(cs),
            (None, Some(rem)) => Some(rem),
            (None, None) => None,
        };
        if let Some(bound) = addressable {
            reach = reach.min(bound.saturating_add(dict_len as u64));
        }
        let reach = usize::try_from(reach).unwrap_or(usize::MAX);

        // RFC 8878: Block_Maximum_Decompressed_Size = min(Window_Size, 128 KiB).
        // A frame that declares its content size can never exceed that either.
        // This ceiling is derived from the *format* alone: folding the caller's
        // output budget into it would report a compression bomb as corrupt data
        // instead of as a budget overrun.
        let declared = usize::try_from(header.declared_window_size).unwrap_or(usize::MAX);
        let mut rfc_max = MAX_BLOCK_SIZE.min(declared.max(1));
        if let Some(cs) = header.content_size {
            rfc_max = rfc_max.min(usize::try_from(cs).unwrap_or(usize::MAX));
        }
        self.block_rfc_max = rfc_max;
        // The budget still bounds how much of a block is worth decoding, and so
        // how large the ring has to be; a block cut short here is reported as a
        // budget overrun by `charge`.
        let allowance = self
            .budget_remaining()
            .map_or(usize::MAX, |r| usize::try_from(r).unwrap_or(usize::MAX));
        let block_max = rfc_max.min(allowance);
        self.block_max = block_max;

        let cap_limit = reach.max(block_max).max(1);
        // The first allocation must never be driven by a number the frame
        // *declares*: `Frame_Content_Size` and `Window_Size` are both
        // attacker-controlled, and an 18-byte frame declaring a terabyte of
        // content would otherwise force a full-window allocation up front.
        // Start at one block (which `block_max` guarantees is enough for any
        // single block) and let `grow_for` double as real bytes arrive, so the
        // ring never exceeds what the stream has actually produced.
        let initial = cap_limit.min(MAX_BLOCK_SIZE);
        debug_assert!(initial >= block_max);

        self.literals.reset();
        self.sequences.reset();
        self.window.begin_frame(cap_limit, initial)?;
        if let Some(dict) = self.dict.take() {
            let seeded = self.window.seed_dictionary(&dict);
            self.dict = Some(dict);
            seeded?;
        }
        self.hasher = if header.has_checksum {
            Some(XxHash64::new())
        } else {
            None
        };
        self.frame_out = 0;
        self.header = Some(header);
        Ok(())
    }

    // -- carry / budget helpers ------------------------------------------

    /// The ceilings that apply while the current frame's next block is decoded.
    fn block_limits(&self) -> BlockLimits {
        BlockLimits {
            rfc_max: self.block_rfc_max,
            budget_max: self.block_max,
            max_output: self.max_output,
            delivered: self.total_out,
        }
    }

    /// Bytes currently buffered in the carry.
    fn available(&self) -> usize {
        self.carry.len() - self.carry_pos
    }

    /// The first `n` buffered bytes.
    fn peek(&self, n: usize) -> &[u8] {
        &self.carry[self.carry_pos..self.carry_pos + n]
    }

    /// Move bytes from `input` into the carry until `n` are buffered.
    ///
    /// Returns `false` when `input` ran out first. Only bytes actually moved
    /// out of `input` are added to `*consumed`, so a byte is never counted
    /// twice however often it is re-examined inside the carry.
    fn fill(&mut self, n: usize, input: &[u8], consumed: &mut usize) -> bool {
        if self.available() >= n {
            return true;
        }
        self.compact();
        let want = n - self.available();
        let take = want.min(input.len() - *consumed);
        self.carry
            .extend_from_slice(&input[*consumed..*consumed + take]);
        *consumed += take;
        self.available() >= n
    }

    /// Drop already-consumed bytes from the front of the carry.
    fn compact(&mut self) {
        if self.carry_pos == 0 {
            return;
        }
        if self.carry_pos == self.carry.len() {
            self.carry.clear();
            self.carry_pos = 0;
            return;
        }
        // Only pay for the move once the dead prefix is worth reclaiming.
        if self.carry_pos * 2 >= self.carry.len() {
            self.carry.copy_within(self.carry_pos.., 0);
            self.carry.truncate(self.carry.len() - self.carry_pos);
            self.carry_pos = 0;
        }
    }

    /// Build the "need more input" answer, turning it into an error under
    /// [`FlushMode::Finish`].
    fn need_input(&self, flush: FlushMode, what: &str) -> Result<Option<ZstdStatus>> {
        if flush == FlushMode::Finish {
            return Err(OxiArcError::corrupted(self.total_in, what.to_string()));
        }
        Ok(Some(ZstdStatus::NeedInput))
    }

    /// Remaining output budget, or `None` when unlimited.
    fn budget_remaining(&self) -> Option<u64> {
        self.max_output.map(|m| m.saturating_sub(self.total_out))
    }

    /// Error for a projected total of `total` output bytes.
    fn budget_error(&self, total: u64) -> OxiArcError {
        OxiArcError::MemoryBudgetExceeded {
            budget: usize::try_from(self.max_output.unwrap_or(0)).unwrap_or(usize::MAX),
            requested: usize::try_from(total).unwrap_or(usize::MAX),
        }
    }

    /// Reject `additional` further output bytes if they would push the stream
    /// past its budget.
    ///
    /// The projected total counts bytes already handed to the caller *and*
    /// bytes sitting in the window waiting to be drained, so calling this with
    /// `additional == 0` after decoding a block charges exactly that block.
    fn check_budget(&self, additional: u64) -> Result<()> {
        if let Some(max) = self.max_output {
            let pending = self.window.pending() as u64;
            let total = self
                .total_out
                .saturating_add(pending)
                .saturating_add(additional);
            if total > max {
                return Err(self.budget_error(total));
            }
        }
        Ok(())
    }

    /// Latch `error` as the sticky fault and return it.
    fn fail(&mut self, error: OxiArcError) -> OxiArcError {
        if self.fault.is_none() {
            self.fault = Some(error.to_string());
        }
        self.state = State::Failed;
        error
    }
}

/// Ceilings applied while one block is decoded into the window.
///
/// Two limits, deliberately kept apart so that a bomb and a malformed block do
/// not report the same error: `rfc_max` is what the *format* allows, and
/// `budget_max` (never larger) is what the caller's output budget still allows.
#[derive(Debug, Clone, Copy)]
struct BlockLimits {
    /// RFC 8878 `Block_Maximum_Decompressed_Size` for this frame. Exceeding it
    /// is a format violation.
    rfc_max: usize,
    /// Cut-off imposed by the remaining output budget (`usize::MAX` when the
    /// stream is unbounded). Exceeding it while still inside `rfc_max` is a
    /// budget overrun.
    budget_max: usize,
    /// The configured `max_output`, reported in the budget error.
    max_output: Option<u64>,
    /// Bytes already handed to the caller, reported in the budget error.
    delivered: u64,
}

/// Decode one block into `window`, returning its regenerated size.
#[allow(clippy::too_many_arguments)]
fn decode_block(
    literals: &mut LiteralsDecoder,
    sequences: &mut SequencesDecoder,
    window: &mut ZstdWindow,
    lit_buf: &mut Vec<u8>,
    seq_buf: &mut Vec<Sequence>,
    kind: BlockType,
    block_size: usize,
    data: &[u8],
    limits: &BlockLimits,
) -> Result<usize> {
    match kind {
        BlockType::Raw => {
            window.push(data)?;
            Ok(data.len())
        }
        BlockType::Rle => {
            let byte = *data
                .first()
                .ok_or_else(|| OxiArcError::corrupted(0, "missing RLE block byte"))?;
            window.push_repeat(byte, block_size)?;
            Ok(block_size)
        }
        BlockType::Compressed => {
            let literals_size = literals.decode_into(data, lit_buf)?;
            if literals_size > data.len() {
                return Err(OxiArcError::corrupted(
                    0,
                    "literals section overruns the block",
                ));
            }
            sequences.decode_into(&data[literals_size..], seq_buf)?;
            execute_sequences(window, lit_buf, seq_buf, limits)
        }
        BlockType::Reserved => Err(OxiArcError::corrupted(0, "reserved block type")),
    }
}

/// Replay the decoded sequences into the sliding window.
///
/// Every write is bounds-checked against [`BlockLimits`] *before* it happens,
/// so the window's pending suffix can never exceed one block and the caller's
/// drain always sees a complete block.
fn execute_sequences(
    window: &mut ZstdWindow,
    literals: &[u8],
    sequences: &[Sequence],
    limits: &BlockLimits,
) -> Result<usize> {
    let mut lit_pos = 0usize;
    let mut produced = 0usize;

    for seq in sequences {
        if seq.literal_length > 0 {
            let end = lit_pos
                .checked_add(seq.literal_length)
                .filter(|e| *e <= literals.len())
                .ok_or_else(|| {
                    OxiArcError::corrupted(0, "literal length exceeds available literals")
                })?;
            produced = charge(produced, seq.literal_length, limits)?;
            window.push(&literals[lit_pos..end])?;
            lit_pos = end;
        }
        if seq.match_length > 0 {
            produced = charge(produced, seq.match_length, limits)?;
            window.copy_match(seq.offset, seq.match_length)?;
        }
    }

    if lit_pos < literals.len() {
        let rest = literals.len() - lit_pos;
        produced = charge(produced, rest, limits)?;
        window.push(&literals[lit_pos..])?;
    }

    Ok(produced)
}

/// Add `more` to `produced`, refusing to exceed either of `limits`.
///
/// The format ceiling is checked first: a block that regenerates more than
/// `Block_Maximum_Decompressed_Size` is malformed whatever the caller's budget
/// is. Only a block that is *valid* but would take the stream past its output
/// budget is reported as [`OxiArcError::MemoryBudgetExceeded`].
fn charge(produced: usize, more: usize, limits: &BlockLimits) -> Result<usize> {
    let total = produced.checked_add(more).ok_or_else(|| {
        OxiArcError::corrupted(0, "block regenerated size overflowed".to_string())
    })?;
    if total > limits.rfc_max {
        return Err(OxiArcError::corrupted(
            0,
            format!(
                "block regenerated size {total} exceeds the frame maximum {}",
                limits.rfc_max
            ),
        ));
    }
    if total > limits.budget_max {
        return Err(OxiArcError::MemoryBudgetExceeded {
            budget: usize::try_from(limits.max_output.unwrap_or(0)).unwrap_or(usize::MAX),
            requested: usize::try_from(limits.delivered.saturating_add(total as u64))
                .unwrap_or(usize::MAX),
        });
    }
    Ok(total)
}

// ---------------------------------------------------------------------------
// Bounded one-shot helpers
// ---------------------------------------------------------------------------

/// Build a stream configured for a bounded one-shot decode.
///
/// The declared-window ceiling is left unrestricted because `max_output`
/// already bounds the memory: the window ring never grows past the number of
/// bytes actually produced, which the budget caps.
fn bounded_stream(max_output: u64, multi_frame: bool) -> ZstdStream {
    ZstdStream::new()
        .with_max_output(max_output)
        .with_max_window(UNRESTRICTED_MAX_WINDOW)
        .with_multi_frame(multi_frame)
}

/// Drive `stream` over the whole of `src`, appending to `out`.
fn run_to_end(stream: &mut ZstdStream, src: &[u8], out: &mut Vec<u8>, cap: usize) -> Result<()> {
    let mut scratch = vec![0u8; 64 * 1024];
    let mut pos = 0usize;
    loop {
        let progress = stream.decode(&src[pos..], &mut scratch, FlushMode::Finish)?;
        pos += progress.consumed;
        if progress.produced > 0 {
            if out.len() + progress.produced > cap {
                return Err(OxiArcError::MemoryBudgetExceeded {
                    budget: cap,
                    requested: out.len() + progress.produced,
                });
            }
            out.try_reserve(progress.produced)
                .map_err(|e| OxiArcError::corrupted(0, format!("output allocation failed: {e}")))?;
            out.extend_from_slice(&scratch[..progress.produced]);
        }
        match progress.status {
            ZstdStatus::StreamEnd => return Ok(()),
            ZstdStatus::NeedOutput => continue,
            ZstdStatus::NeedInput => {
                if progress.consumed == 0 && progress.produced == 0 {
                    return Err(OxiArcError::corrupted(
                        pos as u64,
                        "zstd decoder made no progress",
                    ));
                }
            }
        }
    }
}

/// Decompress one Zstandard frame from `src` directly into `dst`.
///
/// Returns the number of bytes written. `dst` is the entire output budget: a
/// frame that would produce more is rejected before the extra bytes are
/// decoded, so this is safe on untrusted input (unlike
/// [`crate::decompress`], which grows a `Vec` without limit).
///
/// Bytes after the first frame are ignored — TIFF and other containers pad
/// their per-strip payloads, and a padded strip must not become an error.
///
/// # Errors
///
/// Returns [`OxiArcError::MemoryBudgetExceeded`] when the frame regenerates
/// more than `dst.len()` bytes, or a corrupted-data error for a malformed or
/// truncated frame.
///
/// # Example
///
/// ```rust
/// use oxiarc_zstd::{compress_with_level, decompress_into};
///
/// let frame = compress_with_level(b"tiff strip payload", 3).expect("compress");
/// let mut out = [0u8; 64];
/// let n = decompress_into(&frame, &mut out).expect("decompress");
/// assert_eq!(&out[..n], b"tiff strip payload");
/// ```
pub fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let mut stream = bounded_stream(dst.len() as u64, false);
    let mut written = 0usize;
    let mut pos = 0usize;
    loop {
        let progress = stream.decode(&src[pos..], &mut dst[written..], FlushMode::Finish)?;
        pos += progress.consumed;
        written += progress.produced;
        match progress.status {
            ZstdStatus::StreamEnd => return Ok(written),
            ZstdStatus::NeedOutput => {
                return Err(OxiArcError::buffer_too_small(written + 1, dst.len()));
            }
            ZstdStatus::NeedInput => {
                if progress.consumed == 0 && progress.produced == 0 {
                    return Err(OxiArcError::corrupted(
                        pos as u64,
                        "zstd decoder made no progress",
                    ));
                }
            }
        }
    }
}

/// Decompress a single Zstandard frame with a hard output cap.
///
/// The cap is enforced *while* decoding, so a compression bomb is rejected
/// after at most one extra block rather than after it has exhausted memory.
///
/// # Errors
///
/// [`OxiArcError::MemoryBudgetExceeded`] when the frame would produce more than
/// `max_output` bytes; otherwise the usual corrupted-data errors.
///
/// # Example
///
/// ```rust
/// use oxiarc_zstd::{compress_with_level, decompress_with_limit};
///
/// let frame = compress_with_level(&vec![b'A'; 1 << 20], 3).expect("compress");
/// assert!(decompress_with_limit(&frame, 1024).is_err());
/// let out = decompress_with_limit(&frame, 4 << 20).expect("decompress");
/// assert_eq!(out.len(), 1 << 20);
/// ```
pub fn decompress_with_limit(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut stream = bounded_stream(max_output as u64, false);
    let mut out = Vec::new();
    run_to_end(&mut stream, data, &mut out, max_output)?;
    Ok(out)
}

/// Decompress one or more concatenated Zstandard frames with a hard output cap.
///
/// Behaves like [`crate::decompress_multi_frame`] — skippable frames are
/// dropped and trailing non-Zstandard bytes end the stream gracefully — but the
/// total output is bounded by `max_output`.
///
/// # Errors
///
/// [`OxiArcError::MemoryBudgetExceeded`] when the frames would produce more
/// than `max_output` bytes in total; otherwise the usual corrupted-data errors.
///
/// # Example
///
/// ```rust
/// use oxiarc_zstd::{compress_with_level, decompress_multi_frame_with_limit};
///
/// let mut stream = compress_with_level(b"one ", 3).expect("compress");
/// stream.extend_from_slice(&compress_with_level(b"two", 3).expect("compress"));
/// let out = decompress_multi_frame_with_limit(&stream, 1 << 20).expect("decompress");
/// assert_eq!(out, b"one two");
/// ```
pub fn decompress_multi_frame_with_limit(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut stream = bounded_stream(max_output as u64, true);
    let mut out = Vec::new();
    run_to_end(&mut stream, data, &mut out, max_output)?;
    Ok(out)
}

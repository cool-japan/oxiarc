//! Streaming LZH decoder state machine and convenience wrappers.
//!
//! Contains the decoder phases, the main [`StreamingLzhDecoder`] (a resumable,
//! poll-based decoder for `-lh4-`/`-lh5-`/`-lh6-`/`-lh7-` and stored `-lh0-`),
//! the `Read`-wrapping [`LzhStreamDecoder`], and the public convenience
//! functions [`decode_lzh_streaming`] and [`create_streaming_decoder`].
//!
//! # Format
//!
//! This decoder speaks the **canonical** LHA bitstream — MSB-first, with the
//! same block/table/position encoding as the (non-streaming)
//! [`crate::decode::LzhDecoder`] it is validated against. See [`crate::encode`]
//! for the full specification.
//!
//! # Resumability model
//!
//! [`StreamingLzhDecoder::decompress`] is push-based: each call is handed a
//! chunk of compressed input and an output buffer, and returns
//! `(bytes_consumed, bytes_produced, status)`. To make **guaranteed forward
//! progress even with one-byte chunks** while still parsing whole Huffman
//! symbols/tables atomically, every chunk is appended to an internal `carry`
//! buffer and reported as fully consumed. Decoding then reads from `carry`
//! through a resumable [`StreamingBitReader`]; a header or command that runs out
//! of bits is rolled back (the bit cursor is rewound — the bytes stay in
//! `carry`) and retried once more input has arrived. This replaces the previous
//! fine-grained sub-phase state machine (whose spin-loops caused hangs) with a
//! single, always-terminating loop.
//!
//! ## Memory: `carry` is compacted, not retained forever
//!
//! `carry` does **not** grow for the life of the decode. Once the bit reader
//! has fully consumed (or rolled back past) at least
//! `CARRY_COMPACT_THRESHOLD` (64 KiB) of it, [`StreamingLzhDecoder`]'s
//! internal `compact_carry` drops that prefix and rebases the bit reader's
//! cursor to match — every call to
//! [`decompress`](StreamingLzhDecoder::decompress), not only `reset()`.
//! Compaction runs once, at the end of each call, so `carry`'s steady state
//! (between calls) is bounded by the threshold rather than by the whole
//! compressed stream; a caller that feeds reasonably-sized chunks — one
//! byte at a time, in the strictest test in this module's suite — never
//! sees `carry` grow past a small multiple of the threshold. A caller that
//! hands the *entire* compressed stream to a single call gets no benefit
//! from compaction until that one call returns, since a call's newly
//! appended bytes cannot be trimmed before they are read.

use crate::methods::LzhMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::DecompressStatus;

use super::huffman::{
    StreamingBitReader, StreamingHuffmanTree, read_code_tree, read_offset_tree, read_temp_tree,
};

// ============================================================================
// History ring (canonical, space-prefilled)
// ============================================================================

/// Sliding-window history ring, mirroring [`crate::decode`]'s `History`.
///
/// The buffer holds `1 << history_bits` bytes, pre-filled with ASCII space
/// (`0x20`) exactly as canonical LHA (`init_ring_buffer`): real encoders emit
/// copies that reference this initial fill, so it must be reproduced
/// byte-for-byte. Copies address the buffer modulo its (power-of-two) size, so
/// they reach the full window regardless of how much has been emitted.
#[derive(Debug)]
struct StreamHistory {
    /// Ring storage; length is a power of two.
    buf: Vec<u8>,
    /// Write cursor (`ringbuf_pos`).
    pos: usize,
    /// `buf.len() - 1`, for fast modular indexing.
    mask: usize,
}

impl StreamHistory {
    /// Create a space-filled ring of `1 << history_bits` bytes.
    fn new(history_bits: u8) -> Self {
        let size = 1usize << history_bits;
        Self {
            buf: vec![b' '; size],
            pos: 0,
            mask: size - 1,
        }
    }

    /// Ring capacity in bytes.
    #[inline]
    fn size(&self) -> usize {
        self.mask + 1
    }

    /// Append one byte to the ring (`output_byte`).
    #[inline]
    fn push(&mut self, byte: u8) {
        self.buf[self.pos] = byte;
        self.pos = (self.pos + 1) & self.mask;
    }

    /// Read the byte `offset + 1` positions back from the write cursor.
    ///
    /// `offset` is `distance - 1`; `offset == 0` is the most recently pushed
    /// byte. Computed against the *current* cursor so that an overlapping copy
    /// reading its own freshly-pushed output resolves correctly.
    #[inline]
    fn byte_at_offset(&self, offset: usize) -> u8 {
        let size = self.mask + 1;
        let idx = (self.pos + size - offset - 1) & self.mask;
        self.buf[idx]
    }

    /// Reset to the initial space-filled state.
    fn clear(&mut self) {
        self.buf.fill(b' ');
        self.pos = 0;
    }
}

// ============================================================================
// Decoder phases and state
// ============================================================================

/// Coarse state of the streaming decoder (exposed via
/// [`StreamingLzhDecoder::phase`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DecoderPhase {
    /// Initial state, ready to start a new block.
    Ready,
    /// Between blocks: the next thing to read is a 16-bit command count + tables.
    ReadBlockSize,
    /// Reading the C-tree (character/length codes).
    ReadCTree,
    /// Reading the P-tree (position/distance codes).
    ReadPTree,
    /// Decoding the commands of the current block.
    DecodeBlock,
    /// Decompression complete.
    Done,
    /// Error state.
    Error,
}

/// `carry` compaction threshold (64 KiB): once the bit reader has consumed
/// at least this many bytes of `carry`, [`StreamingLzhDecoder::compact_carry`]
/// drops that prefix instead of retaining it for the life of the decode.
const CARRY_COMPACT_THRESHOLD: usize = 64 * 1024;

/// A back-reference copy that overflowed the caller's output buffer and must be
/// continued on the next [`StreamingLzhDecoder::decompress`] call.
#[derive(Debug, Clone, Copy)]
struct PendingMatch {
    /// `distance - 1` of the copy source.
    offset: usize,
    /// Bytes of the copy still to be produced.
    remaining: usize,
}

/// A single decoded command (one C-tree symbol's worth of work).
#[derive(Debug, Clone, Copy)]
enum Command {
    /// A literal byte.
    Literal(u8),
    /// A back-reference copy of `count` bytes at `distance = offset + 1`.
    Match { offset: usize, count: usize },
}

/// Streaming LZH decoder with full state preservation.
pub struct StreamingLzhDecoder {
    /// Compression method.
    method: LzhMethod,
    /// Sliding-window history ring.
    history: StreamHistory,
    /// Resumable MSB-first bit reader over `carry`.
    bit_reader: StreamingBitReader,
    /// Compressed bytes not yet consumed by the bit reader, plus a
    /// [`CARRY_COMPACT_THRESHOLD`]-sized trailing margin of already-consumed
    /// bytes retained until the next [`compact_carry`](Self::compact_carry)
    /// call. Bounded by that threshold plus the largest in-flight block
    /// header/symbol plus one caller-supplied chunk — **not** by the whole
    /// compressed stream (see `compact_carry`).
    carry: Vec<u8>,
    /// Expected uncompressed size.
    uncompressed_size: u64,
    /// Bytes decoded so far.
    bytes_decoded: u64,
    /// Current decoder phase.
    phase: DecoderPhase,
    /// Width, in bits, of the offset-tree code-count field (canonical `pbit`).
    offset_bits: u8,
    /// Maximum number of offset codes (`(1 << offset_bits) - 1`).
    max_offset_codes: usize,
    /// Commands left in the current block (the 16-bit field is a *command*
    /// count, not a byte count).
    block_remaining: u64,
    /// C-tree (character/length codes) for the current block.
    c_tree: Option<StreamingHuffmanTree>,
    /// P-tree (position/distance codes) for the current block.
    p_tree: Option<StreamingHuffmanTree>,
    /// A copy that overflowed the output buffer, to be continued.
    pending_match: Option<PendingMatch>,
    /// Last error message, if any.
    last_error: Option<String>,
    /// Optional progress sink, invoked at block boundaries.
    progress: Option<ProgressHandle>,
}

impl std::fmt::Debug for StreamingLzhDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingLzhDecoder")
            .field("method", &self.method)
            .field("uncompressed_size", &self.uncompressed_size)
            .field("bytes_decoded", &self.bytes_decoded)
            .field("phase", &self.phase)
            .field("block_remaining", &self.block_remaining)
            .field(
                "progress",
                &self.progress.as_ref().map(|_| "<ProgressHandle>"),
            )
            .finish()
    }
}

impl StreamingLzhDecoder {
    /// Create a new streaming LZH decoder.
    pub fn new(method: LzhMethod, uncompressed_size: u64) -> Self {
        // `np` feeds the canonical offset-count-field width helper; lh4/lh5 use
        // 4 bits, lh6/lh7 use 5 (verified against lhasa).
        let np = match method {
            LzhMethod::Lh4 | LzhMethod::Lh5 => 14usize,
            LzhMethod::Lh6 => 16,
            LzhMethod::Lh7 => 17,
            // Every other method is either stored or uses a codec this
            // streaming decoder does not implement (see `decompress`, which
            // returns a typed `unsupported_method` for them); `np` is unused
            // in both cases.
            LzhMethod::Lh0
            | LzhMethod::Lh1
            | LzhMethod::Lh2
            | LzhMethod::Lh3
            | LzhMethod::Lhd
            | LzhMethod::Lzs
            | LzhMethod::Lz4
            | LzhMethod::Lz5
            | LzhMethod::Pm0
            | LzhMethod::Unknown(_) => 0,
        };
        let offset_bits = crate::methods::p_tree_count_bits(np);
        let max_offset_codes = (1usize << offset_bits).saturating_sub(1);

        Self {
            method,
            history: StreamHistory::new(method.history_bits()),
            bit_reader: StreamingBitReader::new(),
            carry: Vec::new(),
            uncompressed_size,
            bytes_decoded: 0,
            phase: if method.is_stored() {
                DecoderPhase::DecodeBlock
            } else {
                DecoderPhase::ReadBlockSize
            },
            offset_bits,
            max_offset_codes,
            block_remaining: 0,
            c_tree: None,
            p_tree: None,
            pending_match: None,
            last_error: None,
            progress: None,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink is called with `on_progress(bytes_decoded, Some(uncompressed_size))`
    /// at each block boundary during decoding.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Reset the decoder to its initial state.
    pub fn reset(&mut self) {
        self.history.clear();
        self.bit_reader = StreamingBitReader::new();
        self.carry.clear();
        self.bytes_decoded = 0;
        self.phase = if self.method.is_stored() {
            DecoderPhase::DecodeBlock
        } else {
            DecoderPhase::ReadBlockSize
        };
        self.block_remaining = 0;
        self.c_tree = None;
        self.p_tree = None;
        self.pending_match = None;
        self.last_error = None;
    }

    /// Whether decoding has finished.
    pub fn is_finished(&self) -> bool {
        self.phase == DecoderPhase::Done
    }

    /// Bytes decoded so far.
    pub fn bytes_decoded(&self) -> u64 {
        self.bytes_decoded
    }

    /// Expected uncompressed size.
    pub fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Current phase.
    pub fn phase(&self) -> DecoderPhase {
        self.phase
    }

    /// Last error message, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// Decompress `input` into `output`.
    ///
    /// Returns `(bytes_consumed, bytes_produced, status)`. For compressed
    /// methods the whole `input` chunk is always consumed (retained internally),
    /// so a caller may advance its source cursor by `bytes_consumed`
    /// unconditionally.
    pub fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.method.is_stored() {
            return self.decompress_stored(input, output);
        }

        if matches!(
            self.method,
            LzhMethod::Lh1
                | LzhMethod::Lh2
                | LzhMethod::Lh3
                | LzhMethod::Lzs
                | LzhMethod::Lz5
                | LzhMethod::Unknown(_)
        ) {
            return Err(OxiArcError::unsupported_method(format!(
                "streaming decode not supported for {}",
                self.method
            )));
        }

        // Accept the whole chunk into the retained buffer; report it consumed.
        let consumed = input.len();
        self.carry.extend_from_slice(input);

        let mut output_pos = 0usize;

        // Continue a copy that overflowed a previous output buffer.
        if let Some(pending) = self.pending_match.take() {
            let left = self.emit_match(pending.offset, pending.remaining, output, &mut output_pos);
            if left > 0 {
                self.pending_match = Some(PendingMatch {
                    offset: pending.offset,
                    remaining: left,
                });
                self.compact_carry();
                return Ok((consumed, output_pos, DecompressStatus::NeedsOutput));
            }
        }

        let status = self.run_loop(output, &mut output_pos)?;
        self.compact_carry();
        Ok((consumed, output_pos, status))
    }

    /// Drop the fully-consumed prefix of `carry`, once it has grown past
    /// [`CARRY_COMPACT_THRESHOLD`].
    ///
    /// `run_loop`/`try_read_block_header` decode transactionally: each
    /// attempt saves the bit reader's state, and on running out of input
    /// mid-header/mid-command, rewinds it before returning — so by the time
    /// any call to `decompress` returns control to the caller, the bit
    /// reader's cursor never points back into a discarded checkpoint from a
    /// *previous* call. That makes `bit_reader.bytes_consumed()` at this
    /// point safe to treat as "never needed again": every byte before it,
    /// across every call so far, has either been fully decoded or rewound
    /// past, never both.
    ///
    /// Thresholded (rather than compacting on every call) so a steady stream
    /// of tiny chunks does not pay an O(n) `Vec::drain` on every single
    /// `decompress` call.
    ///
    /// # Bound this gives `carry`
    ///
    /// Compaction runs once, at the *end* of each `decompress` call, so
    /// `carry` can transiently hold as much as **the largest single input
    /// slice any one call is ever given**, plus a little slack (the
    /// threshold, plus whatever partially-decoded block header/command is
    /// still in flight and therefore not yet compactable) — steady state
    /// after that call returns is back down to roughly the threshold. That
    /// is a **per-call, chunk-size-bounded** guarantee, not a fixed constant
    /// independent of how the caller chunks its input: a caller that always
    /// hands over reasonably-sized chunks (as every caller in this crate's
    /// own test suite does, down to one byte at a time) gets memory bounded
    /// by the threshold, not by the whole compressed stream; a caller that
    /// hands the entire compressed stream to a single `decompress` call gets
    /// no benefit from compaction *during* that one call, only after it
    /// returns.
    fn compact_carry(&mut self) {
        let trim = self.bit_reader.bytes_consumed();
        if trim >= CARRY_COMPACT_THRESHOLD {
            self.carry.drain(..trim);
            self.bit_reader.rebase(trim);
        }
    }

    /// The single, always-terminating decode loop.
    fn run_loop(&mut self, output: &mut [u8], output_pos: &mut usize) -> Result<DecompressStatus> {
        loop {
            if self.bytes_decoded >= self.uncompressed_size {
                self.phase = DecoderPhase::Done;
                return Ok(DecompressStatus::Done);
            }

            // Start a new block if the previous one is exhausted.
            if self.block_remaining == 0 {
                self.phase = DecoderPhase::ReadBlockSize;
                let checkpoint = self.bit_reader.save_state();
                match self.try_read_block_header()? {
                    Some(true) => self.phase = DecoderPhase::DecodeBlock,
                    Some(false) => {
                        // A 16-bit command count of 0 marks end-of-stream.
                        // Reaching it here means the target size has not
                        // been produced yet (a completed decode returns
                        // `Done` at the top of the loop before reading
                        // another header), so the stream is short: refuse
                        // rather than return truncated data, mirroring the
                        // serial lh4-7 decoder's exhaustion guard.
                        if self.bytes_decoded >= self.uncompressed_size {
                            self.phase = DecoderPhase::Done;
                            return Ok(DecompressStatus::Done);
                        }
                        self.phase = DecoderPhase::Error;
                        let msg = format!(
                            "lzh streaming: end-of-stream marker after {} of {} bytes",
                            self.bytes_decoded, self.uncompressed_size
                        );
                        self.last_error = Some(msg.clone());
                        return Err(OxiArcError::corrupted(0, msg));
                    }
                    None => {
                        self.bit_reader.restore_state(checkpoint);
                        return Ok(DecompressStatus::NeedsInput);
                    }
                }
            }

            if *output_pos >= output.len() {
                return Ok(DecompressStatus::NeedsOutput);
            }

            // Decode one command transactionally: on underflow, rewind and wait.
            let checkpoint = self.bit_reader.save_state();
            let command = match Self::decode_command(
                self.c_tree
                    .as_ref()
                    .ok_or_else(|| OxiArcError::corrupted(0, "missing code tree"))?,
                self.p_tree
                    .as_ref()
                    .ok_or_else(|| OxiArcError::corrupted(0, "missing offset tree"))?,
                &mut self.bit_reader,
                &self.carry,
            )? {
                Some(cmd) => cmd,
                None => {
                    self.bit_reader.restore_state(checkpoint);
                    return Ok(DecompressStatus::NeedsInput);
                }
            };
            self.block_remaining -= 1;

            match command {
                Command::Literal(byte) => {
                    self.history.push(byte);
                    output[*output_pos] = byte;
                    *output_pos += 1;
                    self.bytes_decoded += 1;
                }
                Command::Match { offset, count } => {
                    let size = self.history.size();
                    if offset >= size {
                        return Err(OxiArcError::invalid_distance(offset + 1, size));
                    }
                    let left = self.emit_match(offset, count, output, output_pos);
                    if left > 0 {
                        self.pending_match = Some(PendingMatch {
                            offset,
                            remaining: left,
                        });
                        return Ok(DecompressStatus::NeedsOutput);
                    }
                }
            }

            if self.block_remaining == 0 {
                if let Some(ref sink) = self.progress {
                    sink.on_progress(self.bytes_decoded, Some(self.uncompressed_size));
                }
            }
        }
    }

    /// Read a block header (16-bit command count + temp/code/offset tables).
    ///
    /// * `Ok(Some(true))` — a full block header was read (`block_remaining > 0`).
    /// * `Ok(Some(false))` — the command count was 0 (end-of-stream marker).
    /// * `Ok(None)` — input exhausted mid-header; nothing was committed.
    ///
    /// State is committed only on full success, so a rolled-back reader plus an
    /// unchanged `block_remaining == 0` cleanly re-reads the header next call.
    fn try_read_block_header(&mut self) -> Result<Option<bool>> {
        let offset_bits = self.offset_bits;
        let max_codes = self.max_offset_codes;

        let count = match self.bit_reader.read_bits(&self.carry, 16) {
            Some(v) => u64::from(v),
            None => return Ok(None),
        };
        if count == 0 {
            self.block_remaining = 0;
            return Ok(Some(false));
        }

        let temp = match read_temp_tree(&mut self.bit_reader, &self.carry)? {
            Some(t) => t,
            None => return Ok(None),
        };
        let c = match read_code_tree(&mut self.bit_reader, &self.carry, &temp)? {
            Some(t) => t,
            None => return Ok(None),
        };
        let p = match read_offset_tree(&mut self.bit_reader, &self.carry, offset_bits, max_codes)? {
            Some(t) => t,
            None => return Ok(None),
        };

        self.block_remaining = count;
        self.c_tree = Some(c);
        self.p_tree = Some(p);
        Ok(Some(true))
    }

    /// Decode one command (a C-tree symbol, plus an offset for a copy).
    ///
    /// Returns `Ok(None)` if input is exhausted mid-command; the caller rewinds
    /// the reader and retries. Takes the trees and reader by disjoint reference
    /// so no per-command clone is needed.
    fn decode_command(
        c_tree: &StreamingHuffmanTree,
        p_tree: &StreamingHuffmanTree,
        reader: &mut StreamingBitReader,
        carry: &[u8],
    ) -> Result<Option<Command>> {
        let code = match c_tree.decode(reader, carry)? {
            Some(c) => c,
            None => return Ok(None),
        };

        if code < 256 {
            return Ok(Some(Command::Literal(code as u8)));
        }

        let count = code as usize - 256 + 3;

        // Offset: the tree yields a bit length; 0 -> 0, 1 -> 1, else
        // `(1 << (len-1)) + get_bits(len-1)`.  The result is `distance - 1`.
        let len = match p_tree.decode(reader, carry)? {
            Some(l) => l,
            None => return Ok(None),
        };
        let offset = if len == 0 {
            0
        } else if len == 1 {
            1
        } else {
            let extra = match reader.read_bits(carry, (len - 1) as u8) {
                Some(e) => e as usize,
                None => return Ok(None),
            };
            (1usize << (len - 1)) + extra
        };

        Ok(Some(Command::Match { offset, count }))
    }

    /// Copy up to `count` bytes of a match into `output`, updating the ring.
    ///
    /// Returns the number of bytes that could **not** be emitted because the
    /// output buffer filled (0 means the copy is complete). Stops early once the
    /// full uncompressed size has been produced (the serial decoder overshoots
    /// then truncates; producing exactly the target here is byte-identical).
    fn emit_match(
        &mut self,
        offset: usize,
        count: usize,
        output: &mut [u8],
        output_pos: &mut usize,
    ) -> usize {
        let mut remaining = count;
        while remaining > 0 {
            if self.bytes_decoded >= self.uncompressed_size {
                return 0;
            }
            if *output_pos >= output.len() {
                return remaining;
            }
            let byte = self.history.byte_at_offset(offset);
            self.history.push(byte);
            output[*output_pos] = byte;
            *output_pos += 1;
            self.bytes_decoded += 1;
            remaining -= 1;
        }
        0
    }

    /// Decompress stored (`-lh0-` / `-lhd-`) data: a straight copy.
    fn decompress_stored(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        let remaining = (self.uncompressed_size - self.bytes_decoded) as usize;
        let to_copy = input.len().min(output.len()).min(remaining);

        output[..to_copy].copy_from_slice(&input[..to_copy]);
        self.bytes_decoded += to_copy as u64;

        let status = if self.bytes_decoded >= self.uncompressed_size {
            self.phase = DecoderPhase::Done;
            DecompressStatus::Done
        } else if to_copy == input.len() && to_copy < remaining {
            DecompressStatus::NeedsInput
        } else {
            DecompressStatus::NeedsOutput
        };

        if to_copy > 0 {
            if let Some(ref sink) = self.progress {
                sink.on_progress(self.bytes_decoded, Some(self.uncompressed_size));
            }
        }

        Ok((to_copy, to_copy, status))
    }
}

// ============================================================================
// Decompressor trait implementation
// ============================================================================

impl oxiarc_core::traits::Decompressor for StreamingLzhDecoder {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        StreamingLzhDecoder::decompress(self, input, output)
    }

    fn reset(&mut self) {
        StreamingLzhDecoder::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.is_finished()
    }
}

// ============================================================================
// Convenience functions
// ============================================================================

/// Decompress a complete LZH buffer using the streaming decoder.
///
/// A convenience wrapper that drives [`StreamingLzhDecoder`] to completion. For
/// true incremental decompression with partial input/output, use the decoder
/// directly.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzhuf::{LzhMethod, decode_lzh_streaming};
///
/// let data = b"Hello, World!";
/// let result = decode_lzh_streaming(data, LzhMethod::Lh0, data.len() as u64)
///     .expect("decompression failed");
/// assert_eq!(result, data);
/// ```
pub fn decode_lzh_streaming(
    data: &[u8],
    method: LzhMethod,
    uncompressed_size: u64,
) -> Result<Vec<u8>> {
    let mut decoder = StreamingLzhDecoder::new(method, uncompressed_size);
    // `uncompressed_size` is header-supplied and therefore untrusted: cap
    // the up-front reservation and let the vector grow as bytes are
    // actually produced, so a bogus multi-GiB claim cannot pre-commit the
    // allocation.
    let prealloc = usize::try_from(uncompressed_size)
        .unwrap_or(usize::MAX)
        .min(1 << 20);
    let mut output = Vec::with_capacity(prealloc);
    let mut input_pos = 0usize;
    let mut buffer = vec![0u8; 65536];

    loop {
        let (consumed, produced, status) = decoder.decompress(&data[input_pos..], &mut buffer)?;
        input_pos += consumed;
        output.extend_from_slice(&buffer[..produced]);

        match status {
            DecompressStatus::Done => break,
            _ => {
                // No input consumed and no output produced means the stream
                // is exhausted before the target size was reached: refuse
                // rather than return silently truncated data (mirrors the
                // serial lh4-7 decoder's exhaustion guard, LZHUF-01).
                if consumed == 0 && produced == 0 {
                    return Err(OxiArcError::corrupted(
                        input_pos as u64,
                        format!(
                            "lzh streaming: compressed stream exhausted after {} of {} bytes",
                            output.len(),
                            uncompressed_size
                        ),
                    ));
                }
            }
        }
    }

    // Belt-and-braces: `Done` must coincide with exactly the target size.
    if output.len() as u64 != uncompressed_size {
        return Err(OxiArcError::corrupted(
            input_pos as u64,
            format!(
                "lzh streaming: produced {} bytes, expected {}",
                output.len(),
                uncompressed_size
            ),
        ));
    }

    Ok(output)
}

/// Create a streaming decoder for the given method and expected output size.
pub fn create_streaming_decoder(method: LzhMethod, uncompressed_size: u64) -> StreamingLzhDecoder {
    StreamingLzhDecoder::new(method, uncompressed_size)
}

// ============================================================================
// LzhStreamDecoder<R: Read> — Reader-wrapping streaming decompressor
// ============================================================================

/// Size of the chunk appended to the input staging buffer on each `reader.read()`.
const STREAM_READ_CHUNK: usize = 4096;

/// Size of the reusable decode scratch buffer held by [`LzhStreamDecoder`].
const STREAM_SCRATCH_LEN: usize = 32768;

/// A streaming LZH decompressor that implements [`std::io::Read`].
///
/// Given any `R: Read` yielding LZH-compressed bytes, this decompresses on the
/// fly as bytes arrive — without requiring all compressed data in memory at
/// once. Internally it owns a [`StreamingLzhDecoder`] plus a staging buffer of
/// compressed bytes; each [`std::io::Read::read`] drives the state machine until
/// `buf` is filled or the stream ends.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzhuf::{LzhMethod, LzhEncoder, LzhStreamDecoder};
/// use std::io::{Read, Cursor};
///
/// let original = b"Hello, streaming LZH!";
/// let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
/// let compressed = encoder.compress_to_vec(original).expect("encode failed");
///
/// let cursor = Cursor::new(compressed);
/// let mut stream_dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);
///
/// let mut output = Vec::new();
/// stream_dec.read_to_end(&mut output).expect("decode failed");
/// assert_eq!(output, original);
/// ```
pub struct LzhStreamDecoder<R: std::io::Read> {
    /// Source of compressed bytes.
    reader: R,
    /// The underlying symbol-level streaming decoder.
    decoder: StreamingLzhDecoder,
    /// Staging buffer of compressed bytes not yet handed to the state machine.
    staging: Vec<u8>,
    /// Number of valid bytes in `staging`.
    staging_len: usize,
    /// `true` once `reader` has reported EOF.
    reader_eof: bool,
    /// Produced output not yet copied to the caller.
    output_buf: Vec<u8>,
    /// How many bytes of `output_buf` were already returned.
    output_pos: usize,
    /// Set once the decoder reports `Done` and staging drains.
    finished: bool,
    /// Reusable scratch buffer the state machine decodes into.
    ///
    /// Owned by the struct rather than allocated inside every pump, so a
    /// `Read` loop over a large stream does not allocate (and free)
    /// [`STREAM_SCRATCH_LEN`] bytes on every single `read()` call — audit
    /// finding 5.10.
    out_scratch: Vec<u8>,
}

impl<R: std::io::Read> LzhStreamDecoder<R> {
    /// Create a new `LzhStreamDecoder` wrapping `reader`.
    ///
    /// `original_size` is the exact uncompressed length (from the LHA header).
    pub fn new(reader: R, method: LzhMethod, original_size: u64) -> Self {
        Self {
            reader,
            decoder: StreamingLzhDecoder::new(method, original_size),
            staging: vec![0u8; STREAM_READ_CHUNK * 2],
            staging_len: 0,
            reader_eof: false,
            output_buf: Vec::new(),
            output_pos: 0,
            finished: false,
            out_scratch: vec![0u8; STREAM_SCRATCH_LEN],
        }
    }

    /// Attach a progress sink (see [`StreamingLzhDecoder::with_progress`]).
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.decoder = self.decoder.with_progress(handle);
        self
    }

    /// `true` if decoding has finished and all output has been consumed.
    pub fn is_finished(&self) -> bool {
        self.finished && self.output_pos >= self.output_buf.len()
    }

    /// Pull more bytes from `reader` into the staging buffer.
    fn refill_staging(&mut self) -> std::io::Result<bool> {
        if self.reader_eof {
            return Ok(self.staging_len > 0);
        }

        let needed_capacity = self.staging_len + STREAM_READ_CHUNK;
        if self.staging.len() < needed_capacity {
            self.staging.resize(needed_capacity, 0u8);
        }

        let n = self
            .reader
            .read(&mut self.staging[self.staging_len..])
            .map_err(|e| std::io::Error::new(e.kind(), format!("lzh reader error: {e}")))?;

        if n == 0 {
            self.reader_eof = true;
        } else {
            self.staging_len += n;
        }

        Ok(self.staging_len > 0 || !self.reader_eof)
    }

    /// Drive the state machine once with whatever is in the staging buffer.
    ///
    /// The decode scratch buffer is moved out of `self` for the duration of
    /// the call (the state machine needs `&mut self.decoder` and
    /// `&self.staging` at the same time) and always moved back, on the error
    /// path too, so it is allocated exactly once per decoder.
    fn drive_once(&mut self) -> std::io::Result<DecompressStatus> {
        let mut scratch = std::mem::take(&mut self.out_scratch);
        if scratch.len() < STREAM_SCRATCH_LEN {
            scratch.resize(STREAM_SCRATCH_LEN, 0u8);
        }

        let outcome = match self
            .decoder
            .decompress(&self.staging[..self.staging_len], &mut scratch)
        {
            Ok((consumed, produced, status)) => {
                self.output_buf.extend_from_slice(&scratch[..produced]);
                if consumed > 0 && consumed <= self.staging_len {
                    self.staging.copy_within(consumed..self.staging_len, 0);
                    self.staging_len -= consumed;
                }
                Ok(status)
            }
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("lzh decode error: {e}"),
            )),
        };

        self.out_scratch = scratch;
        outcome
    }

    /// Pump the state machine until `output_buf` has bytes or the stream ends.
    fn pump_decoder(&mut self) -> std::io::Result<bool> {
        if self.output_pos < self.output_buf.len() {
            return Ok(true);
        }

        self.output_buf.clear();
        self.output_pos = 0;

        loop {
            if self.finished {
                return Ok(!self.output_buf.is_empty());
            }

            // Bring in more input when the staging buffer is empty.
            if self.staging_len == 0 {
                if self.reader_eof {
                    // Flush any bits still buffered inside the decoder.
                    let status = self.drive_once()?;
                    if matches!(status, DecompressStatus::Done) {
                        self.finished = true;
                    } else if self.output_buf.is_empty() {
                        if self.decoder.is_finished() {
                            self.finished = true;
                        } else {
                            // Source exhausted before the decoder reached
                            // its target size: surface truncation as an
                            // error instead of a silently short `Ok` read.
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                format!(
                                    "lzh stream truncated: {} of {} bytes decoded",
                                    self.decoder.bytes_decoded(),
                                    self.decoder.uncompressed_size()
                                ),
                            ));
                        }
                    }
                    return Ok(!self.output_buf.is_empty());
                }
                self.refill_staging()?;
                if self.staging_len == 0 && self.reader_eof {
                    continue;
                }
            }

            let status = self.drive_once()?;

            match status {
                DecompressStatus::Done => {
                    self.finished = true;
                    return Ok(!self.output_buf.is_empty());
                }
                DecompressStatus::NeedsOutput => continue,
                DecompressStatus::NeedsInput if !self.output_buf.is_empty() => {
                    return Ok(true);
                }
                DecompressStatus::NeedsInput => {
                    // Loop: staging was fully consumed into the decoder, so the
                    // next iteration refills from the reader (or hits EOF).
                }
                DecompressStatus::BlockEnd => {}
                // `DecompressStatus` is `#[non_exhaustive]`; treat any future
                // status like a block boundary (continue driving the loop).
                _ => {}
            }
        }
    }
}

impl<R: std::io::Read> std::io::Read for LzhStreamDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        if self.finished && self.output_pos >= self.output_buf.len() {
            return Ok(0);
        }

        if self.output_pos < self.output_buf.len() {
            let available = self.output_buf.len() - self.output_pos;
            let to_copy = available.min(buf.len());
            buf[..to_copy]
                .copy_from_slice(&self.output_buf[self.output_pos..self.output_pos + to_copy]);
            self.output_pos += to_copy;
            return Ok(to_copy);
        }

        if !self.pump_decoder()? {
            return Ok(0);
        }

        let available = self.output_buf.len().saturating_sub(self.output_pos);
        if available == 0 {
            return Ok(0);
        }
        let to_copy = available.min(buf.len());
        buf[..to_copy]
            .copy_from_slice(&self.output_buf[self.output_pos..self.output_pos + to_copy]);
        self.output_pos += to_copy;
        Ok(to_copy)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::super::huffman::{StreamingBitReader, StreamingHuffmanTree};
    use super::*;

    #[test]
    fn test_streaming_bit_reader_basic() {
        // MSB-first: the high nibble of 0xAB (0xA) comes first.
        let data = [0xAB, 0xCD, 0xEF];
        let mut reader = StreamingBitReader::new();

        assert_eq!(reader.read_bits(&data, 4), Some(0xA));
        assert_eq!(reader.read_bits(&data, 4), Some(0xB));
        assert_eq!(reader.read_bits(&data, 8), Some(0xCD));
        assert_eq!(reader.bytes_consumed(), 2);
    }

    #[test]
    fn test_streaming_bit_reader_not_enough_input() {
        let data = [0xAB];
        let mut reader = StreamingBitReader::new();

        assert_eq!(reader.read_bits(&data, 12), None);
        // The one available byte was buffered while trying to satisfy the read.
        assert_eq!(reader.bits_available(), 8);
    }

    #[test]
    fn test_streaming_bit_reader_state_save_restore() {
        let data = [0xAB, 0xCD];
        let mut reader = StreamingBitReader::new();

        reader.read_bits(&data, 4); // consumes the high nibble 0xA
        let state = reader.save_state();

        reader.read_bits(&data, 8);
        assert_eq!(reader.bytes_consumed(), 2);

        reader.restore_state(state);
        // MSB-first: the nibble following 0xA is the low nibble 0xB.
        assert_eq!(reader.read_bits(&data, 4), Some(0xB));
    }

    #[test]
    fn test_streaming_huffman_tree_basic() {
        let mut lengths = vec![0u8; 256];
        lengths[b'A' as usize] = 2;
        lengths[b'B' as usize] = 2;
        lengths[b'C' as usize] = 2;
        lengths[b'D' as usize] = 2;

        let tree = StreamingHuffmanTree::from_lengths(&lengths, 8).expect("Failed to create tree");
        assert_eq!(tree.max_length(), 2);
    }

    #[test]
    fn test_streaming_decoder_stored() {
        let data = b"Hello, World!";
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
        let mut output = vec![0u8; data.len()];

        let (consumed, produced, status) = decoder
            .decompress(data, &mut output)
            .expect("Decompress failed");

        assert_eq!(consumed, data.len());
        assert_eq!(produced, data.len());
        assert_eq!(status, DecompressStatus::Done);
        assert_eq!(&output, data);
    }

    #[test]
    fn test_streaming_decoder_stored_chunked() {
        let data = b"Hello, World!";
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
        let mut output = Vec::new();
        let mut input_pos = 0;

        while input_pos < data.len() {
            let chunk_size = 3.min(data.len() - input_pos);
            let mut chunk_output = vec![0u8; chunk_size];

            let (consumed, produced, status) = decoder
                .decompress(&data[input_pos..input_pos + chunk_size], &mut chunk_output)
                .expect("Decompress failed");

            input_pos += consumed;
            output.extend_from_slice(&chunk_output[..produced]);

            if status == DecompressStatus::Done {
                break;
            }
        }

        assert_eq!(output, data);
    }

    #[test]
    fn test_streaming_decoder_phases() {
        let decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, 100);
        assert_eq!(decoder.phase(), DecoderPhase::ReadBlockSize);

        let stored_decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, 100);
        assert_eq!(stored_decoder.phase(), DecoderPhase::DecodeBlock);
    }

    #[test]
    fn test_streaming_decoder_reset() {
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, 100);

        decoder.bytes_decoded = 50;
        decoder.phase = DecoderPhase::DecodeBlock;

        decoder.reset();

        assert_eq!(decoder.bytes_decoded(), 0);
        assert_eq!(decoder.phase(), DecoderPhase::ReadBlockSize);
        assert!(!decoder.is_finished());
    }

    #[test]
    fn test_streaming_decoder_stored_small_output_buffer() {
        let data = b"Hello, World! This is a longer test string.";
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh0, data.len() as u64);
        let mut output = Vec::new();
        let mut input_pos = 0;

        loop {
            let mut chunk_output = vec![0u8; 5];
            let input_slice = &data[input_pos..];

            let (consumed, produced, status) = decoder
                .decompress(input_slice, &mut chunk_output)
                .expect("Decompress failed");

            input_pos += consumed;
            output.extend_from_slice(&chunk_output[..produced]);

            match status {
                DecompressStatus::Done => break,
                DecompressStatus::NeedsInput if input_pos >= data.len() => break,
                DecompressStatus::NeedsOutput => {}
                _ => {}
            }
        }

        assert_eq!(output, data);
    }

    /// Truncation guard (LZHUF-01 follow-up): `decode_lzh_streaming` must
    /// return `Err` — never a short `Ok` — when an lh5 stream is truncated
    /// at any byte offset.
    #[test]
    fn test_decode_lzh_streaming_truncated_lh5_is_error() {
        use crate::encode::LzhEncoder;

        let original: Vec<u8> = b"streaming truncation guard "
            .iter()
            .cycle()
            .take(4000)
            .copied()
            .collect();
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");
        assert!(compressed.len() > 8, "fixture too small to truncate");

        // Sweep truncation points, including deep cuts near the end.
        let cuts = [
            0usize,
            1,
            2,
            compressed.len() / 4,
            compressed.len() / 2,
            compressed.len() - 2,
            compressed.len() - 1,
        ];
        for &cut in &cuts {
            let result =
                decode_lzh_streaming(&compressed[..cut], LzhMethod::Lh5, original.len() as u64);
            match result {
                Err(_) => {}
                Ok(out) => panic!(
                    "truncation to {cut}/{} bytes must not return Ok ({} bytes produced)",
                    compressed.len(),
                    out.len()
                ),
            }
        }

        // The untruncated stream still decodes exactly.
        let full = decode_lzh_streaming(&compressed, LzhMethod::Lh5, original.len() as u64)
            .expect("full stream decode");
        assert_eq!(full, original);
    }

    /// Truncated stored (`-lh0-`) input must also be an error.
    #[test]
    fn test_decode_lzh_streaming_truncated_stored_is_error() {
        let data = b"stored payload for truncation";
        let result = decode_lzh_streaming(&data[..10], LzhMethod::Lh0, data.len() as u64);
        assert!(result.is_err(), "short stored stream must be an error");
    }

    /// A crafted stream whose block header declares a command count of 0
    /// before the target size is produced must be rejected (previously the
    /// decoder returned `Done` with short output).
    #[test]
    fn test_streaming_decoder_early_end_marker_is_error() {
        // 16 zero bits = command count 0 straight away, target size 100.
        let data = [0u8, 0u8];
        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, 100);
        let mut out = vec![0u8; 256];
        let result = decoder.decompress(&data, &mut out);
        assert!(
            result.is_err(),
            "early end-of-stream marker must be an error, got {result:?}"
        );
        assert_eq!(decoder.phase(), DecoderPhase::Error);
        assert!(decoder.last_error().is_some());
    }

    /// The `Read`-wrapping `LzhStreamDecoder` must surface truncation as an
    /// I/O error from `read`, not silently end the stream short.
    #[test]
    fn test_lzh_stream_decoder_truncated_source_is_error() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = (0u16..3000).map(|i| (i % 251) as u8).collect();
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let truncated = compressed[..compressed.len() / 2].to_vec();
        let cursor = Cursor::new(truncated);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh5, original.len() as u64);

        let mut output = Vec::new();
        let result = dec.read_to_end(&mut output);
        assert!(
            result.is_err(),
            "truncated source must error, got Ok with {} bytes",
            output.len()
        );
    }

    /// A simple progress sink that counts calls and records the last `processed`.
    struct CountingSink {
        calls: std::sync::atomic::AtomicU64,
        last_processed: std::sync::atomic::AtomicU64,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicU64::new(0),
                last_processed: std::sync::atomic::AtomicU64::new(0),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn last_processed(&self) -> u64 {
            self.last_processed
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl oxiarc_core::progress::ProgressSink for CountingSink {
        fn on_progress(&self, processed: u64, _total: Option<u64>) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.last_processed
                .store(processed, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn make_counting_sink() -> std::sync::Arc<CountingSink> {
        std::sync::Arc::new(CountingSink::new())
    }

    #[test]
    fn test_lzh_stream_decoder_basic() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = (0u8..=255).cycle().take(1000).collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);

        let mut output = Vec::new();
        let mut buf = [0u8; 50];
        loop {
            let n = dec.read(&mut buf).expect("decode failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original, "basic Lh0 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_lh0() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = (0u8..=255).cycle().take(300).collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "Lh0 (stored) stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_lh5_roundtrip() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        // Exercise the compressed path end-to-end through the Read wrapper.
        let original: Vec<u8> = b"the quick brown fox "
            .iter()
            .cycle()
            .take(5000)
            .copied()
            .collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh5, original.len() as u64);

        let mut output = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = dec.read(&mut buf).expect("decode failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original, "Lh5 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_empty() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = Vec::new();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh5, 0u64);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "empty stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_large() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = (0u16..)
            .flat_map(|i| [(i >> 8) as u8, (i & 0xFF) as u8])
            .take(50_000)
            .collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64);

        let mut output = Vec::new();
        let mut buf = vec![0u8; 1024];
        loop {
            let n = dec.read(&mut buf).expect("decode failed");
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original, "large Lh0 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_single_byte() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original = vec![0x42u8];

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, 1u64);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "single-byte Lh0 stream roundtrip failed");
    }

    #[test]
    fn test_lzh_stream_decoder_with_progress() {
        use crate::encode::LzhEncoder;
        use std::io::{Cursor, Read};

        let original: Vec<u8> = (0u8..=255).cycle().take(500).collect();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh0);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");

        let sink = make_counting_sink();
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        let cursor = Cursor::new(compressed);
        let mut dec = LzhStreamDecoder::new(cursor, LzhMethod::Lh0, original.len() as u64)
            .with_progress(handle);

        let mut output = Vec::new();
        dec.read_to_end(&mut output).expect("decode failed");

        assert_eq!(output, original, "progress Lh0 stream roundtrip failed");
        assert!(
            sink.call_count() >= 1,
            "on_progress must be called at least once, got {}",
            sink.call_count()
        );
    }

    #[test]
    fn test_progress_callbacks_decode() {
        use std::sync::Arc;

        let input: Vec<u8> = vec![b'A'; 40];
        let input_size = input.len();
        let encoded = input.clone();

        let sink = Arc::new(CountingSink::new());
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        let mut decoder =
            StreamingLzhDecoder::new(LzhMethod::Lh0, input_size as u64).with_progress(handle);

        let mut output = Vec::with_capacity(input_size);
        let mut input_pos = 0;
        let mut buf = vec![0u8; 32768];

        loop {
            let (consumed, produced, status) = decoder
                .decompress(&encoded[input_pos..], &mut buf)
                .expect("decompress failed");
            input_pos += consumed;
            output.extend_from_slice(&buf[..produced]);
            match status {
                DecompressStatus::Done => break,
                DecompressStatus::NeedsInput if input_pos >= encoded.len() => break,
                _ => {}
            }
        }

        assert_eq!(output, input, "decoded output must match original input");
        assert!(
            sink.call_count() >= 1,
            "on_progress must be called at least once; calls = {}",
            sink.call_count()
        );
        assert_eq!(
            sink.last_processed(),
            input_size as u64,
            "last processed ({}) should equal input size ({})",
            sink.last_processed(),
            input_size
        );
    }

    /// Regression for the unbounded-`carry`-growth defect: `carry` must be
    /// periodically compacted, not retained for the life of the decode.
    ///
    /// Feeds the compressed stream **one byte at a time** — the strictest
    /// possible chunking, since it exercises every single possible split
    /// point, including a block header or command split landing exactly at
    /// the moment the consumed prefix first crosses the compaction
    /// threshold (the one ordering — a checkpoint rollback racing a
    /// compaction — that could silently corrupt decoding if `compact_carry`
    /// ever ran before a rollback restored the pre-header bit-reader
    /// position; see `compact_carry`'s doc comment for why it does not).
    #[test]
    fn test_streaming_decoder_carry_is_bounded_not_whole_stream() {
        use crate::encode::LzhEncoder;

        // High-entropy pseudo-random (SplitMix64), not merely repetitive: a
        // real LZH archiver compresses highly repetitive test fixtures down
        // to almost nothing, which would never accumulate enough `carry` to
        // exercise compaction at all. A weaker generator has the same
        // problem in disguise: an earlier version of this test emitted only
        // the top byte of a linear congruential step per index, which
        // varies slowly enough between neighbouring indices that LZSS still
        // found abundant matches (400 KiB compressed to under 22 KiB).
        // SplitMix64 decorrelates every output byte, so LZSS finds
        // essentially no matches and the compressed size stays close to the
        // original — the scenario a decompression-bomb defense actually
        // needs to hold up against.
        fn make_incompressible(size: usize) -> Vec<u8> {
            let mut seed: u64 = 0x1234_5678_9abc_def0;
            let mut out = Vec::with_capacity(size + 8);
            while out.len() < size {
                seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = seed;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                out.extend_from_slice(&z.to_le_bytes());
            }
            out.truncate(size);
            out
        }

        let original = make_incompressible(400 * 1024);
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .expect("compression failed");
        assert!(
            compressed.len() > 2 * CARRY_COMPACT_THRESHOLD,
            "fixture must compress to more than 2x the {}-byte compaction \
             threshold to actually exercise repeated compaction; got {} bytes \
             (increase `original`'s size if this fails)",
            CARRY_COMPACT_THRESHOLD,
            compressed.len()
        );

        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, original.len() as u64);
        let mut output = Vec::with_capacity(original.len());
        let mut scratch = vec![0u8; 64 * 1024];
        let mut compactions_observed = 0usize;
        let mut previous_carry_len = 0usize;
        // Generous ceiling: the compaction threshold plus headroom far
        // larger than any single LZH block header (16-bit count + temp-tree
        // + up to NC c-tree codes + a handful of p-tree codes is at most a
        // few KiB) could ever add before a compaction gets a chance to run.
        // Still far below the full stream size, so a regression back to
        // "retain everything" fails this assertion long before the loop
        // finishes.
        let carry_ceiling = CARRY_COMPACT_THRESHOLD * 3;

        for byte in &compressed {
            let mut remaining: &[u8] = std::slice::from_ref(byte);
            loop {
                let (consumed, produced, status) = decoder
                    .decompress(remaining, &mut scratch)
                    .expect("decompress failed");
                output.extend_from_slice(&scratch[..produced]);
                remaining = &remaining[consumed..];

                assert!(
                    decoder.carry.len() <= carry_ceiling,
                    "carry grew to {} bytes (ceiling {}, threshold {}) \u{2014} \
                     compaction is not bounding memory",
                    decoder.carry.len(),
                    carry_ceiling,
                    CARRY_COMPACT_THRESHOLD
                );
                if decoder.carry.len() < previous_carry_len {
                    compactions_observed += 1;
                }
                previous_carry_len = decoder.carry.len();

                match status {
                    DecompressStatus::NeedsOutput => continue,
                    _ => break,
                }
            }
        }

        assert_eq!(
            output, original,
            "byte-at-a-time decode with periodic carry compaction must still be byte-exact"
        );
        assert!(
            compactions_observed >= 2,
            "expected at least 2 compaction events over a {}-byte compressed \
             stream with a {}-byte threshold, observed {}",
            compressed.len(),
            CARRY_COMPACT_THRESHOLD,
            compactions_observed
        );
    }
}

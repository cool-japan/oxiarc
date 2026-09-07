//! Bounded, truly incremental Brotli decoding.
//!
//! [`BrotliStream`] is a *push* decoder: you hand it whatever compressed bytes
//! you have and whatever output space you have, and it makes as much progress
//! as both allow. It never buffers the whole compressed input, never
//! materialises the whole decompressed output, and never needs the stream to
//! arrive in any particular chunking — feeding one byte at a time into a
//! one-byte output slice produces exactly the same bytes as one call with
//! everything.
//!
//! This is what [`crate::decompress()`] cannot do: that decoder borrows a
//! complete `&[u8]`, resolves backward references against the growing output
//! `Vec`, and has no way to say "I need more input" as distinct from "this
//! stream is truncated". [`BrotliStream`] is built for HTTP bodies, pipes and
//! any other source that arrives in pieces.
//!
//! # Memory
//!
//! Peak memory is `O(window)`, not `O(stream)`:
//!
//! * a sliding-window ring of at most `1 << WBITS` bytes, allocated lazily and
//!   grown on demand, so a 40-byte stream that declares `lgwin = 22` costs
//!   4 KiB rather than 4 MiB. Refuse over-large windows *before* allocation
//!   with [`BrotliStream::with_max_window`] (default 16 MiB, which admits every
//!   RFC 7932 window including `WBITS = 24`);
//! * one meta-block's prefix codes and context maps, replaced per meta-block;
//! * a small input carry, described below.
//!
//! [`BrotliStream::with_max_output`] additionally caps the *total* output.
//! Because every meta-block declares its exact `MLEN`, the check runs before
//! the offending meta-block is decoded — so a decompression bomb is refused
//! without any of its expansion being produced.
//!
//! # The input carry
//!
//! A meta-block prelude (up to 256 prefix codes per category plus two context
//! maps) is parsed atomically: the bit cursor is rewound and the prelude
//! re-parsed when it does not fit in the bytes received so far. Those bytes
//! are therefore held in an internal carry until the prelude completes. The
//! carry is compacted in place (never `remove(0)`), and is bounded by
//! `MAX_METABLOCK_HEADER` (1 MiB) — a prelude that does not complete within it
//! is corruption, not slow input. Inside the command loop the carry drains
//! down to a few bytes, because every command is individually rewindable.
//!
//! Bytes are consumed exactly once: [`BrotliStream::decode`] reports every
//! input byte it took as `consumed`, and never asks for it again.
//!
//! # Strictness
//!
//! The stream is as strict as the one-shot decoder, and rejects the same
//! things with the same error variants: truncation, non-zero padding,
//! incomplete prefix codes, invalid distances, meta-block length overruns —
//! and, per RFC 7932, any trailing byte after the final meta-block (Brotli
//! defines no stream concatenation). The first error latches: every later call
//! returns it rather than resuming from a half-decoded state.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_brotli::{compress, BrotliStream, BrotliStatus};
//! use oxiarc_core::traits::FlushMode;
//!
//! let original = b"streaming brotli, one byte at a time".repeat(64);
//! let compressed = compress(&original, 5).expect("compress");
//!
//! let mut stream = BrotliStream::new().with_max_output(1 << 20);
//! let mut decoded = Vec::new();
//! let mut out = [0u8; 7];
//! let mut fed = 0;
//!
//! loop {
//!     // One compressed byte in, at most seven decompressed bytes out.
//!     let chunk = &compressed[fed..(fed + 1).min(compressed.len())];
//!     let flush = if fed >= compressed.len() { FlushMode::Finish } else { FlushMode::None };
//!     let progress = stream.decode(chunk, &mut out, flush).expect("decode");
//!     fed += progress.consumed;
//!     decoded.extend_from_slice(&out[..progress.produced]);
//!     if progress.status == BrotliStatus::StreamEnd {
//!         break;
//!     }
//! }
//! stream.finish().expect("complete stream");
//! assert_eq!(decoded, original);
//! ```

mod budget;
mod command;
mod meta;
mod window;

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::FlushMode;

use crate::bit_reader::{BitCursorState, BitReader};
use crate::decompress::{DecoderState, MetaBlockShape};
use crate::error::{BrotliError, BrotliResult};

use budget::OutputBudget;
use command::{CmdState, CommandCtx, CommandStatus, MetaBlockState, run_commands};
use meta::{MAX_METABLOCK_HEADER, MetaBlockStart, parse_meta_block_start, read_stream_header};
use window::BrotliWindow;

/// Default ceiling on the declared window a stream may ask for: 16 MiB, which
/// admits every RFC 7932 window size including `WBITS = 24`.
pub const DEFAULT_MAX_WINDOW: usize = 16 * 1024 * 1024;

/// Hard ceiling on the internal input carry.
///
/// The carry only grows while a meta-block prelude is being re-parsed
/// atomically; [`MAX_METABLOCK_HEADER`] bounds that. Beyond this ceiling
/// [`BrotliStream::decode`] takes fewer bytes than offered — the caller sees a
/// short `consumed` — so a caller that ignores [`BrotliStatus::NeedOutput`]
/// still cannot make the decoder buffer without bound.
///
/// It is deliberately **twice** `MAX_METABLOCK_HEADER`, not just a little more:
/// the geometric retry schedule waits for the buffered input to grow by up to
/// an eighth before re-attempting a prelude, so the carry must have room for
/// `1.125 x MAX_METABLOCK_HEADER` for that attempt to happen at all. Without
/// the headroom an oversized header would be waited on forever instead of
/// reported.
const MAX_CARRY: usize = MAX_METABLOCK_HEADER * 2;

/// Smallest growth in buffered input that re-triggers an atomic prelude parse.
///
/// Sets the floor on the geometric retry schedule, and so the worst-case extra
/// latency before the first byte of a stream whose prelude is tiny.
const MIN_HEADER_RETRY_STRIDE: usize = 64;

/// Why [`BrotliStream::decode`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrotliStatus {
    /// All available input was consumed and more is needed to continue.
    ///
    /// This is the idle answer to an empty input slice too; it is not an
    /// error, and it does not mean the stream is truncated. Only
    /// [`FlushMode::Finish`] (or [`BrotliStream::finish`]) turns "no more
    /// input" into an error.
    NeedInput,
    /// The output slice is full; call again with more room.
    ///
    /// Never returned with `produced == 0` unless the output slice was empty.
    NeedOutput,
    /// The final meta-block and its padding have been decoded. The stream is
    /// complete; [`BrotliStream::finish`] will succeed.
    StreamEnd,
}

/// What one [`BrotliStream::decode`] call achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrotliProgress {
    /// Bytes taken from the `input` slice. These are never asked for again.
    pub consumed: usize,
    /// Bytes written to the start of the `output` slice.
    pub produced: usize,
    /// Why the call returned.
    pub status: BrotliStatus,
}

/// Position of the stream-level state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the `WBITS` stream header.
    StreamHeader,
    /// At a meta-block boundary.
    MetaBlockStart,
    /// Discarding a metadata meta-block's payload.
    MetadataSkip { remaining: usize, is_last: bool },
    /// Streaming an uncompressed meta-block's raw bytes.
    Uncompressed { remaining: usize },
    /// Inside a compressed meta-block's command loop (state in `meta`).
    Command,
    /// After the last meta-block: the final padding bits must be zero.
    FinalPadding,
    /// The stream is complete.
    Done,
}

/// A bounded, resumable Brotli decoder.
///
/// See the [module documentation](self) for the memory model, the input carry
/// and the strictness guarantees. The short version:
///
/// * [`decode`](BrotliStream::decode) makes as much progress as the input and
///   output slices allow and tells you which one to grow;
/// * [`finish`](BrotliStream::finish) asserts the stream really ended;
/// * [`reset`](BrotliStream::reset) returns the decoder to its initial state,
///   clearing the fault latch, so one instance can decode many streams.
///
/// # Example
///
/// ```rust
/// use oxiarc_brotli::{compress, BrotliStream, BrotliStatus};
/// use oxiarc_core::traits::FlushMode;
///
/// let compressed = compress(b"hello, bounded world", 4).expect("compress");
/// let mut stream = BrotliStream::new();
/// let mut out = vec![0u8; 64];
/// let progress = stream.decode(&compressed, &mut out, FlushMode::Finish).expect("decode");
/// assert_eq!(progress.status, BrotliStatus::StreamEnd);
/// assert_eq!(&out[..progress.produced], b"hello, bounded world");
/// stream.finish().expect("complete stream");
/// ```
pub struct BrotliStream {
    /// Input bytes received but not yet consumed, plus whatever the current
    /// atomic parse may need to re-read. Compacted in place.
    carry: Vec<u8>,
    /// Bit position inside `carry`.
    cursor: BitCursorState,
    /// Stream-level state machine position.
    state: State,
    /// Sliding window, allocated once `WBITS` is known.
    window: Option<BrotliWindow>,
    /// Distance ring and window size; persist across meta-blocks.
    dist: DecoderState,
    /// Current compressed meta-block, when inside one.
    meta: Option<MetaBlockState>,
    /// Most recently produced byte (literal context `p1`).
    p1: u8,
    /// Second most recently produced byte (literal context `p2`).
    p2: u8,
    /// Scratch for the transformed static-dictionary word being emitted.
    dict_buf: Vec<u8>,
    /// Total output cap.
    budget: OutputBudget,
    /// Largest window allocation this stream will accept.
    max_window: usize,
    /// Total bytes accepted from callers.
    total_in: u64,
    /// Total bytes produced.
    total_out: u64,
    /// Recorded meta-block shapes, when enabled.
    shapes: Vec<MetaBlockShape>,
    /// Whether to record meta-block shapes.
    record_shapes: bool,
    /// Unconsumed input at the last failed atomic prelude parse, or 0 when no
    /// retry is pending. Drives the geometric retry schedule that keeps
    /// re-parsing linear rather than quadratic in the prelude's size.
    header_retry_at: usize,
    /// How many times an atomic prelude parse has been attempted. Diagnostic:
    /// it is what the retry-schedule test measures.
    header_attempts: u64,
    /// Sticky fault: once set, every call returns it.
    fault: Option<BrotliError>,
    /// Optional per-meta-block progress sink.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token, checked at meta-block boundaries.
    cancel: Option<CancellationToken>,
}

impl std::fmt::Debug for BrotliStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrotliStream")
            .field("state", &self.state)
            .field("carry_len", &self.carry.len())
            .field("total_in", &self.total_in)
            .field("total_out", &self.total_out)
            .field("window_size", &self.window_size())
            .field("faulted", &self.fault.is_some())
            .finish()
    }
}

impl Default for BrotliStream {
    fn default() -> Self {
        Self::new()
    }
}

impl BrotliStream {
    /// Create a decoder with the crate's default 256 MB output guard and a
    /// 16 MiB window ceiling.
    #[must_use]
    pub fn new() -> Self {
        BrotliStream {
            carry: Vec::new(),
            cursor: BitCursorState::start(),
            state: State::StreamHeader,
            window: None,
            dist: DecoderState::new(0),
            meta: None,
            p1: 0,
            p2: 0,
            dict_buf: Vec::new(),
            budget: OutputBudget::default_guard(),
            max_window: DEFAULT_MAX_WINDOW,
            total_in: 0,
            total_out: 0,
            shapes: Vec::new(),
            record_shapes: false,
            header_retry_at: 0,
            header_attempts: 0,
            fault: None,
            progress: None,
            cancel: None,
        }
    }

    /// Refuse to produce more than `limit` bytes in total.
    ///
    /// The check is exact and runs before each meta-block is decoded (a
    /// meta-block produces exactly its declared `MLEN`), so an over-budget
    /// stream fails with [`BrotliError::MemoryBudgetExceeded`] without any of
    /// the offending expansion being produced. Without this the crate's
    /// built-in 256 MB guard applies and reports
    /// [`BrotliError::OutputTooLarge`].
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.budget = OutputBudget::caller(limit);
        self
    }

    /// Refuse a stream whose declared window would allocate more than `bytes`.
    ///
    /// The declared window is `1 << WBITS`; the refusal happens while reading
    /// the stream header, *before* any allocation, and reports
    /// [`BrotliError::WindowTooLarge`]. The default,
    /// [`DEFAULT_MAX_WINDOW`] (16 MiB), admits every RFC 7932 window.
    /// `Content-Encoding: br` in practice uses `lgwin <= 22` (4 MiB).
    #[must_use]
    pub fn with_max_window(mut self, bytes: usize) -> Self {
        self.max_window = bytes;
        self
    }

    /// Record the [`MetaBlockShape`] of every compressed meta-block, readable
    /// with [`BrotliStream::recorded_shapes`].
    ///
    /// Off by default; intended for tests and diagnostics, where it is the
    /// differential oracle against
    /// [`crate::decompress_reporting_shapes`] — identical output bytes do not
    /// prove that a resumable parser read the header fields at the right
    /// positions, but an identical shape sequence does.
    #[must_use]
    pub fn with_shape_recording(mut self, enabled: bool) -> Self {
        self.record_shapes = enabled;
        self
    }

    /// Attach a progress sink, notified with the running output total after
    /// every meta-block.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token, checked at every meta-block boundary.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Total bytes accepted from callers so far.
    #[must_use]
    pub fn total_in(&self) -> u64 {
        self.total_in
    }

    /// Total bytes produced so far.
    #[must_use]
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Whether the final meta-block has been decoded.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state == State::Done && self.fault.is_none()
    }

    /// The stream's window size, `(1 << WBITS) - 16`, once the stream header
    /// has been read.
    #[must_use]
    pub fn window_size(&self) -> Option<usize> {
        if self.state == State::StreamHeader {
            None
        } else {
            Some(self.dist.window_size)
        }
    }

    /// The meta-block shapes recorded so far; empty unless
    /// [`BrotliStream::with_shape_recording`] was enabled.
    #[must_use]
    pub fn recorded_shapes(&self) -> &[MetaBlockShape] {
        &self.shapes
    }

    /// Return the decoder to its initial state, keeping the configured output
    /// cap, window ceiling, progress sink and cancellation token, and clearing
    /// the fault latch.
    ///
    /// The window allocation is released, so a reset decoder costs no more
    /// than a fresh one.
    pub fn reset(&mut self) {
        self.carry.clear();
        self.cursor = BitCursorState::start();
        self.state = State::StreamHeader;
        self.window = None;
        self.dist = DecoderState::new(0);
        self.meta = None;
        self.p1 = 0;
        self.p2 = 0;
        self.dict_buf.clear();
        self.total_in = 0;
        self.total_out = 0;
        self.shapes.clear();
        self.header_retry_at = 0;
        self.header_attempts = 0;
        self.fault = None;
    }

    /// How many times an atomic meta-block prelude parse has been attempted.
    ///
    /// Preludes are re-parsed from a rolled-back bit cursor until they fit in
    /// the received input, so this is the decoder's re-parse work. It is the
    /// number the retry schedule exists to bound; tests assert it stays far
    /// below the input length.
    #[cfg(test)]
    fn header_attempts(&self) -> u64 {
        self.header_attempts
    }

    /// Assert that the stream really ended.
    ///
    /// # Errors
    ///
    /// Returns the latched fault if one occurred, or
    /// [`BrotliError::UnexpectedEof`] when the stream stopped before its final
    /// meta-block — which is how a truncated body is caught. A stream that has
    /// reached [`BrotliStatus::StreamEnd`] always succeeds, and `finish` may be
    /// called more than once.
    pub fn finish(&mut self) -> BrotliResult<()> {
        if let Some(err) = &self.fault {
            return Err(duplicate_error(err));
        }
        if self.state == State::Done {
            return Ok(());
        }
        self.fault = Some(BrotliError::UnexpectedEof);
        Err(BrotliError::UnexpectedEof)
    }

    /// Decode as much as `input` and `output` allow.
    ///
    /// `consumed` bytes are taken from the front of `input` — never asked for
    /// again — and `produced` bytes are written to the front of `output`. In
    /// the steady state `consumed` is `input.len()`; it is short only when the
    /// bounded carry is already holding an atomic meta-block prelude, in which
    /// case the caller simply offers the remainder on the next call.
    ///
    /// Pass [`FlushMode::Finish`] when `input` is the last data that will ever
    /// arrive: running out of input then becomes
    /// [`BrotliError::UnexpectedEof`] instead of
    /// [`BrotliStatus::NeedInput`]. Any other flush mode means "more may
    /// follow"; the mode has no other effect, because a Brotli decoder cannot
    /// flush partial state the way an encoder can.
    ///
    /// # Errors
    ///
    /// Any corruption the one-shot decoder would report, the output cap
    /// ([`BrotliError::MemoryBudgetExceeded`] / [`BrotliError::OutputTooLarge`]),
    /// an over-large declared window ([`BrotliError::WindowTooLarge`]), a
    /// prelude exceeding the 1 MiB carry cap, cancellation, and — with
    /// [`FlushMode::Finish`] — truncation. The first error latches: every later
    /// call returns it until [`BrotliStream::reset`].
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_brotli::{compress, BrotliStream, BrotliStatus};
    /// use oxiarc_core::traits::FlushMode;
    ///
    /// let compressed = compress(b"chunked input, chunked output", 6).expect("compress");
    /// let mut stream = BrotliStream::new();
    /// let mut decoded = Vec::new();
    /// let mut out = [0u8; 8];
    /// for (i, chunk) in compressed.chunks(3).enumerate() {
    ///     let last = (i + 1) * 3 >= compressed.len();
    ///     let flush = if last { FlushMode::Finish } else { FlushMode::None };
    ///     let mut progress = stream.decode(chunk, &mut out, flush).expect("decode");
    ///     decoded.extend_from_slice(&out[..progress.produced]);
    ///     // Drain whatever the small output slice could not hold.
    ///     while progress.status == BrotliStatus::NeedOutput {
    ///         progress = stream.decode(&[], &mut out, flush).expect("decode");
    ///         decoded.extend_from_slice(&out[..progress.produced]);
    ///     }
    /// }
    /// assert_eq!(decoded, b"chunked input, chunked output");
    /// ```
    pub fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> BrotliResult<BrotliProgress> {
        if let Some(err) = &self.fault {
            return Err(duplicate_error(err));
        }

        // Take only as much as the bounded carry can hold. In the steady
        // state the carry has drained to a few bytes and this is all of
        // `input`; it matters only while an atomic meta-block prelude is being
        // retried, which is exactly where the bound belongs.
        let mut carry = std::mem::take(&mut self.carry);
        let unconsumed = carry.len() - self.cursor.bits_consumed() / 8;
        let take = input.len().min(MAX_CARRY.saturating_sub(unconsumed));
        carry.extend_from_slice(&input[..take]);
        self.total_in += take as u64;

        // "More input may still arrive" is true when the caller said so *or*
        // when the bounded carry made us leave some of this call's input
        // behind. Only when neither holds does the decoder switch to the
        // one-shot reader semantics, where running short is truncation.
        let more_possible = flush != FlushMode::Finish || take < input.len();
        let outcome = self.run(&carry, output, more_possible);

        // Compact the carry in place up to the resume point, amortised: never
        // a `remove(0)`, and never past a position the decoder may rewind to.
        let consumed_bytes = self.cursor.bits_consumed() / 8;
        if consumed_bytes > 0 && consumed_bytes * 2 > carry.len() {
            let remaining = carry.len() - consumed_bytes;
            carry.copy_within(consumed_bytes.., 0);
            carry.truncate(remaining);
            self.cursor = BitReader::rebase_state(self.cursor, consumed_bytes);
        }
        self.carry = carry;

        match outcome {
            Ok((produced, status)) => Ok(BrotliProgress {
                consumed: take,
                produced,
                status,
            }),
            Err(err) => {
                self.fault = Some(duplicate_error(&err));
                Err(err)
            }
        }
    }

    /// The state machine proper. `carry` is the whole received-but-unconsumed
    /// input; the bit cursor indexes into it.
    fn run(
        &mut self,
        carry: &[u8],
        output: &mut [u8],
        more_possible: bool,
    ) -> BrotliResult<(usize, BrotliStatus)> {
        let mut reader = BitReader::resume(carry, self.cursor, more_possible);
        let mut written = 0usize;

        let status = loop {
            match self.state {
                State::StreamHeader => {
                    let cursor = reader.save();
                    match read_stream_header(&mut reader) {
                        Ok(wbits) => {
                            let declared = 1usize << wbits;
                            if declared > self.max_window {
                                return Err(BrotliError::WindowTooLarge {
                                    declared,
                                    max: self.max_window,
                                });
                            }
                            self.dist = DecoderState::new(declared - 16);
                            self.window = Some(BrotliWindow::with_target(declared));
                            self.state = State::MetaBlockStart;
                        }
                        Err(BrotliError::UnexpectedEof) => {
                            reader.restore(cursor);
                            break out_of_input(more_possible)?;
                        }
                        Err(e) => return Err(e),
                    }
                }

                State::MetaBlockStart => {
                    if let Some(token) = &self.cancel {
                        token.check().map_err(BrotliError::from)?;
                    }
                    let cursor = reader.save();
                    // Bytes buffered from the start of this prelude. Note this
                    // is how much input *happens* to be available, not how much
                    // the prelude needs — so it is only evidence of an
                    // oversized header once a parse has actually run short.
                    let pending = carry.len() - cursor.bits_consumed() / 8;
                    // Re-parsing a prelude from scratch is only free if it is
                    // not done once per newly arrived byte. After a failed
                    // attempt, wait for the input to grow by a stride
                    // proportional to what that attempt already had: the
                    // attempts are then geometric and the total re-parse work
                    // stays linear in the prelude's size rather than quadratic.
                    // The extra latency is at most one stride, and a prelude
                    // cannot produce output before it is complete anyway.
                    //
                    // `MAX_CARRY` is twice `MAX_METABLOCK_HEADER`, which is
                    // what guarantees this gate cannot stall: an attempt is
                    // always let through by `1.125 * MAX_METABLOCK_HEADER`,
                    // well before the carry stops accepting input, so an
                    // oversized header is always reported rather than waited on.
                    if more_possible && self.header_retry_at > 0 {
                        let stride = (self.header_retry_at / 8).max(MIN_HEADER_RETRY_STRIDE);
                        if pending < self.header_retry_at.saturating_add(stride) {
                            break BrotliStatus::NeedInput;
                        }
                    }
                    self.header_attempts += 1;
                    match parse_meta_block_start(&mut reader, &self.budget, self.total_out) {
                        Ok(MetaBlockStart::LastEmpty) => {
                            self.header_retry_at = 0;
                            self.state = State::FinalPadding;
                        }
                        Ok(MetaBlockStart::Metadata { skip, is_last }) => {
                            self.header_retry_at = 0;
                            self.state = State::MetadataSkip {
                                remaining: skip,
                                is_last,
                            };
                        }
                        Ok(MetaBlockStart::Uncompressed { mlen }) => {
                            self.header_retry_at = 0;
                            self.state = State::Uncompressed { remaining: mlen };
                        }
                        Ok(MetaBlockStart::Compressed {
                            mlen,
                            is_last,
                            header,
                        }) => {
                            self.header_retry_at = 0;
                            self.meta = Some(MetaBlockState {
                                header,
                                mlen,
                                produced: 0,
                                is_last,
                                cmd: CmdState::Begin,
                            });
                            self.state = State::Command;
                        }
                        Err(BrotliError::UnexpectedEof) => {
                            reader.restore(cursor);
                            // The parse really did run short of `pending`
                            // bytes, so this is now evidence about the header's
                            // own size rather than about how much input
                            // happened to be buffered.
                            if pending > MAX_METABLOCK_HEADER {
                                return Err(BrotliError::CorruptedData(format!(
                                    "meta-block header exceeds {MAX_METABLOCK_HEADER} bytes"
                                )));
                            }
                            self.header_retry_at = pending.max(1);
                            break out_of_input(more_possible)?;
                        }
                        // Any other error is a genuine format violation.
                        Err(e) => return Err(e),
                    }
                }

                State::MetadataSkip { remaining, is_last } => {
                    let skipped = reader.skip_aligned_upto(remaining)?;
                    let left = remaining - skipped;
                    if left > 0 {
                        self.state = State::MetadataSkip {
                            remaining: left,
                            is_last,
                        };
                        break out_of_input(more_possible)?;
                    }
                    self.state = if is_last {
                        State::FinalPadding
                    } else {
                        State::MetaBlockStart
                    };
                }

                State::Uncompressed { remaining } => {
                    if remaining == 0 {
                        self.state = State::MetaBlockStart;
                        if let Some(handle) = &self.progress {
                            handle.on_progress(self.total_out, None);
                        }
                        continue;
                    }
                    if written == output.len() {
                        break BrotliStatus::NeedOutput;
                    }
                    let space = (output.len() - written).min(remaining);
                    let taken = reader.read_aligned_into(&mut output[written..written + space])?;
                    if taken > 0 {
                        let chunk = &output[written..written + taken];
                        if let Some(w) = self.window.as_mut() {
                            w.push_slice(chunk);
                        }
                        note_output(&mut self.p1, &mut self.p2, chunk);
                        self.total_out += taken as u64;
                        written += taken;
                        self.state = State::Uncompressed {
                            remaining: remaining - taken,
                        };
                    }
                    if taken < space {
                        break out_of_input(more_possible)?;
                    }
                }

                State::Command => {
                    let (Some(meta), Some(window)) = (self.meta.as_mut(), self.window.as_mut())
                    else {
                        return Err(BrotliError::CorruptedData(
                            "internal error: command state without a meta-block".to_string(),
                        ));
                    };
                    let mut ctx = CommandCtx {
                        window,
                        dist: &mut self.dist,
                        total_out: &mut self.total_out,
                        p1: &mut self.p1,
                        p2: &mut self.p2,
                        dict_buf: &mut self.dict_buf,
                    };
                    let (produced, status) =
                        run_commands(&mut reader, meta, &mut ctx, &mut output[written..])?;
                    written += produced;
                    let is_last = meta.is_last;
                    let shape = meta.header.shape;
                    match status {
                        CommandStatus::Finished => {
                            if self.record_shapes {
                                self.shapes.push(shape);
                            }
                            self.meta = None;
                            if let Some(handle) = &self.progress {
                                handle.on_progress(self.total_out, None);
                            }
                            self.state = if is_last {
                                State::FinalPadding
                            } else {
                                State::MetaBlockStart
                            };
                        }
                        CommandStatus::NeedInput => break out_of_input(more_possible)?,
                        CommandStatus::NeedOutput => break BrotliStatus::NeedOutput,
                    }
                }

                State::FinalPadding => {
                    let cursor = reader.save();
                    match reader.align_to_byte() {
                        Ok(0) => self.state = State::Done,
                        Ok(_) => {
                            return Err(BrotliError::CorruptedData(
                                "non-zero padding after last meta-block".to_string(),
                            ));
                        }
                        Err(BrotliError::UnexpectedEof) => {
                            reader.restore(cursor);
                            break out_of_input(more_possible)?;
                        }
                        Err(e) => return Err(e),
                    }
                }

                State::Done => {
                    // RFC 7932 defines no stream concatenation: a byte after
                    // the final meta-block is corruption, exactly as the
                    // one-shot decoder treats it.
                    if reader.has_more() {
                        return Err(BrotliError::CorruptedData(
                            "trailing data after last meta-block".to_string(),
                        ));
                    }
                    break BrotliStatus::StreamEnd;
                }
            }
        };

        self.cursor = reader.save();
        Ok((written, status))
    }
}

/// Update the two literal-context bytes from the most recent output.
fn note_output(p1: &mut u8, p2: &mut u8, bytes: &[u8]) {
    match bytes.len() {
        0 => {}
        1 => {
            *p2 = *p1;
            *p1 = bytes[0];
        }
        n => {
            *p2 = bytes[n - 2];
            *p1 = bytes[n - 1];
        }
    }
}

/// Decide what "the buffered input ran out" means right now.
///
/// It is truncation only when no further bytes can arrive — the caller passed
/// [`FlushMode::Finish`] *and* the bounded carry accepted everything offered.
fn out_of_input(more_possible: bool) -> BrotliResult<BrotliStatus> {
    if more_possible {
        Ok(BrotliStatus::NeedInput)
    } else {
        Err(BrotliError::UnexpectedEof)
    }
}

/// Re-create a latched error for a repeat call.
///
/// [`BrotliError`] is not `Clone` (it can wrap an [`std::io::Error`]), so the
/// latch reproduces the variant and message rather than the original value.
fn duplicate_error(err: &BrotliError) -> BrotliError {
    match err {
        BrotliError::CorruptedData(msg) => BrotliError::CorruptedData(msg.clone()),
        BrotliError::InvalidHuffmanCode(msg) => BrotliError::InvalidHuffmanCode(msg.clone()),
        BrotliError::InvalidDistance {
            distance,
            max_distance,
        } => BrotliError::InvalidDistance {
            distance: *distance,
            max_distance: *max_distance,
        },
        BrotliError::InvalidParameter(msg) => BrotliError::InvalidParameter(msg.clone()),
        BrotliError::UnexpectedEof => BrotliError::UnexpectedEof,
        BrotliError::OutputTooLarge(n) => BrotliError::OutputTooLarge(*n),
        BrotliError::MemoryBudgetExceeded { budget, requested } => {
            BrotliError::MemoryBudgetExceeded {
                budget: *budget,
                requested: *requested,
            }
        }
        BrotliError::Io(e) => BrotliError::Io(std::io::Error::new(e.kind(), e.to_string())),
        BrotliError::InvalidWindowSize(n) => BrotliError::InvalidWindowSize(*n),
        BrotliError::WindowTooLarge { declared, max } => BrotliError::WindowTooLarge {
            declared: *declared,
            max: *max,
        },
        BrotliError::InvalidBlockType(n) => BrotliError::InvalidBlockType(*n),
        BrotliError::DictionaryError(msg) => BrotliError::DictionaryError(msg.clone()),
        BrotliError::InvalidContextMap(msg) => BrotliError::InvalidContextMap(msg.clone()),
        BrotliError::InvalidPrefixCode(msg) => BrotliError::InvalidPrefixCode(msg.clone()),
        BrotliError::Cancelled => BrotliError::Cancelled,
    }
}

/// Decode a complete Brotli stream through [`BrotliStream`], growing `out`.
///
/// The push decoder driven to completion over a slice that is already fully
/// available — not a second decoder. Used by the crate's own tests.
#[cfg(test)]
pub(crate) fn decode_all_with(stream: &mut BrotliStream, input: &[u8]) -> BrotliResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut pos = 0usize;
    loop {
        // `Finish` only once every byte has been offered: the bounded carry
        // means one call may take less than the whole slice, and declaring the
        // end early would turn "more to come" into a spurious truncation.
        let flush = if pos == input.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.decode(&input[pos..], &mut buf, flush)?;
        pos += progress.consumed;
        out.extend_from_slice(&buf[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
        if progress.consumed == 0 && progress.produced == 0 {
            // `flush == Finish` would have errored, so this can only be an
            // idle call the caller cannot escape.
            return Err(BrotliError::CorruptedData(
                "decoder made no progress".to_string(),
            ));
        }
    }
    stream.finish()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress;

    fn decode_chunked(data: &[u8], in_chunk: usize, out_chunk: usize) -> BrotliResult<Vec<u8>> {
        let mut stream = BrotliStream::new();
        let mut out = Vec::new();
        let mut buf = vec![0u8; out_chunk];
        let mut pos = 0usize;
        loop {
            let end = (pos + in_chunk).min(data.len());
            let chunk = &data[pos..end];
            let flush = if end == data.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = stream.decode(chunk, &mut buf, flush)?;
            pos += progress.consumed;
            out.extend_from_slice(&buf[..progress.produced]);
            if progress.status == BrotliStatus::StreamEnd {
                break;
            }
        }
        stream.finish()?;
        Ok(out)
    }

    #[test]
    fn empty_input_is_idle_not_an_error() {
        let mut stream = BrotliStream::new();
        let mut out = [0u8; 16];
        for _ in 0..1000 {
            let p = stream
                .decode(&[], &mut out, FlushMode::None)
                .expect("idle decode");
            assert_eq!(p.consumed, 0);
            assert_eq!(p.produced, 0);
            assert_eq!(p.status, BrotliStatus::NeedInput);
        }
    }

    #[test]
    fn byte_at_a_time_matches_one_shot() {
        let data = b"the quick brown fox jumps over the lazy dog".repeat(40);
        let compressed = compress(&data, 6).expect("compress");
        assert_eq!(decode_chunked(&compressed, 1, 1).expect("decode"), data);
    }

    #[test]
    fn fault_latches() {
        let mut broken = compress(b"latch me", 5).expect("compress");
        let last = broken.len() - 1;
        broken[last] ^= 0xFF;
        broken.push(0x77);
        let mut stream = BrotliStream::new();
        let mut out = [0u8; 64];
        let first = stream.decode(&broken, &mut out, FlushMode::Finish);
        assert!(first.is_err(), "corrupt stream must fail");
        let second = stream.decode(&[], &mut out, FlushMode::Finish);
        assert!(second.is_err(), "fault must latch");
        stream.reset();
        assert!(stream.finish().is_err(), "reset stream is not finished");
    }

    #[test]
    fn window_ceiling_is_checked_before_allocation() {
        // lgwin 22 -> a 4 MiB declared window.
        let params = crate::BrotliParams {
            quality: 1,
            lgwin: 22,
            ..crate::BrotliParams::default()
        };
        let compressed = crate::compress_with_params(b"window policy", &params).expect("compress");
        let mut stream = BrotliStream::new().with_max_window(1 << 20);
        let mut out = [0u8; 64];
        let err = stream
            .decode(&compressed, &mut out, FlushMode::Finish)
            .expect_err("over-large window must be refused");
        assert!(
            matches!(err, BrotliError::WindowTooLarge { .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reset_allows_a_second_stream() {
        let a = compress(b"first stream", 5).expect("compress");
        let b = compress(b"second stream, longer", 5).expect("compress");
        let mut stream = BrotliStream::new();
        assert_eq!(
            decode_all_with(&mut stream, &a).expect("a"),
            b"first stream"
        );
        stream.reset();
        assert_eq!(
            decode_all_with(&mut stream, &b).expect("b"),
            b"second stream, longer"
        );
    }

    #[test]
    fn trailing_data_is_rejected() {
        let mut compressed = compress(b"no concatenation", 5).expect("compress");
        compressed.push(0x00);
        let mut stream = BrotliStream::new();
        let err = decode_all_with(&mut stream, &compressed).expect_err("trailing byte");
        assert!(
            err.to_string().contains("trailing data"),
            "unexpected error: {err}"
        );
    }

    /// Feeding a stream one byte at a time must not re-parse its meta-block
    /// prelude once per byte.
    ///
    /// The prelude is parsed atomically — rolled back and retried until it fits
    /// in the received input — so a naive schedule costs `O(prelude^2)` work on
    /// a byte-at-a-time source, which is exactly the shape a hostile stream with
    /// a large prelude would exploit. The geometric retry schedule turns that
    /// into `O(prelude)`; this pins it by counting attempts.
    #[test]
    fn header_reparsing_is_not_quadratic() {
        // Structured data at q11, where the encoder splits blocks and emits
        // real context maps, giving the largest preludes this crate produces.
        let mut data = Vec::new();
        for i in 0..4000u32 {
            data.extend_from_slice(&i.to_le_bytes());
            data.extend_from_slice(
                format!(
                    "record-{i:06} the time of the public
"
                )
                .as_bytes(),
            );
        }
        let compressed = crate::compress(&data, 11).expect("compress");

        let mut stream = BrotliStream::new();
        let mut out = vec![0u8; 4096];
        let mut decoded = Vec::new();
        let mut pos = 0usize;
        loop {
            let end = (pos + 1).min(compressed.len());
            let flush = if end == compressed.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = stream
                .decode(&compressed[pos..end], &mut out, flush)
                .expect("decode");
            pos += progress.consumed;
            decoded.extend_from_slice(&out[..progress.produced]);
            if progress.status == BrotliStatus::StreamEnd {
                break;
            }
        }
        stream.finish().expect("complete");
        assert_eq!(decoded, data, "byte-at-a-time decode must be exact");

        let attempts = stream.header_attempts();
        // Calibration: with the geometric schedule this stream needs 4 prelude
        // parses (one per meta-block, none retried more than a couple of
        // times). Retrying on every newly arrived byte instead needs 71 — that
        // count scales with the *input length*, which is the shape that turns a
        // hostile stream carrying a large prelude into quadratic re-parse work.
        // The bound is therefore absolute, not relative to the input: scaling
        // with the input is exactly what is being ruled out.
        assert!(
            attempts < 32,
            "{attempts} prelude parses for {} input bytes: the retry schedule \
             is scaling with the input instead of with the prelude",
            compressed.len()
        );
    }

    #[test]
    fn total_counters_track_the_stream() {
        let data = vec![7u8; 5000];
        let compressed = compress(&data, 4).expect("compress");
        let mut stream = BrotliStream::new();
        let out = decode_all_with(&mut stream, &compressed).expect("decode");
        assert_eq!(out.len(), data.len());
        assert_eq!(stream.total_out(), data.len() as u64);
        assert_eq!(stream.total_in(), compressed.len() as u64);
        assert!(stream.is_finished());
        assert_eq!(stream.window_size(), Some((1 << 22) - 16));
    }
}

//! Resumable raw-DEFLATE (RFC 1951) decoding.
//!
//! [`InflateStream`] is a *push* decoder: every call takes a prefix of the
//! compressed input and fills a prefix of the caller's output buffer, and
//! **all** decoder state — the partially consumed byte in the bit
//! accumulator, a half-parsed dynamic Huffman header, a match that is
//! half-copied — survives across calls. An arbitrary byte split of the same
//! compressed stream therefore produces byte-identical output.
//!
//! This is the piece the rest of the workspace builds on: `WrappedInflate`
//! (gzip/zlib framing), the `Read`/`AsyncRead` adapters, the HTTP
//! content-coding decoders, and the PNG/TIFF image codecs all drive it.
//!
//! # Example
//!
//! ```
//! use oxiarc_core::traits::FlushMode;
//! use oxiarc_deflate::{deflate, InflateStatus, InflateStream};
//!
//! let original = b"push decoding, one byte at a time".repeat(64);
//! let compressed = deflate(&original, 6).expect("deflate");
//!
//! // Feed the compressed stream one byte per call into a 16-byte window.
//! let mut stream = InflateStream::new();
//! let mut out = Vec::new();
//! let mut scratch = [0u8; 16];
//! let mut fed = 0usize;
//! loop {
//!     let chunk = compressed.get(fed..(fed + 1).min(compressed.len()))
//!         .unwrap_or_default();
//!     let last = fed + chunk.len() >= compressed.len();
//!     let flush = if last { FlushMode::Finish } else { FlushMode::None };
//!     let p = stream.inflate(chunk, &mut scratch, flush).expect("inflate");
//!     fed += p.consumed;
//!     out.extend_from_slice(&scratch[..p.produced]);
//!     if p.status == InflateStatus::StreamEnd {
//!         break;
//!     }
//! }
//! assert_eq!(out, original);
//! ```
//!
//! # Flush modes
//!
//! `flush` is [`oxiarc_core::traits::FlushMode`]:
//!
//! | Mode | Meaning for the decoder |
//! |---|---|
//! | `None` (default) | More input may follow; running dry yields [`InflateStatus::NeedInput`]. |
//! | `Finish` | This is the last input; running dry before the end of the stream is `UnexpectedEof`. |
//! | `Sync` | As `None`, but return as soon as an empty stored block (an RFC 1951 sync flush) completes. |
//! | `Full`, `Partial` | Treated as `None` — they are encoder-side concepts. |
//!
//! `FlushMode` is `#[non_exhaustive]`, so anything not listed above is
//! handled by the same wildcard arm as `Full`/`Partial`.
//!
//! ## Which mode each consumer passes
//!
//! The choice is not a preference: it is fixed by whether the caller can
//! still obtain more input. Passing `None` when no more input exists turns a
//! truncated stream into a silent short read; passing `Finish` when more
//! input *is* coming turns a legal chunk boundary into `UnexpectedEof`.
//!
//! | Consumer | `flush` |
//! |---|---|
//! | TIFF strip / tile (the whole compressed slice is in hand) | `Finish` on the single call |
//! | APNG frame / PNG `fdAT` run (complete slice) | `Finish`, then [`InflateStream::reset`] |
//! | PNG main `IDAT` chain, fed chunk by chunk | `None` until the `IDAT` run ends, then `Finish` |
//! | HTTP response body | `None` until transport EOF, then `Finish` |
//! | `RawInflateReader` (RFC 4978 — the peer may still send more) | `None` **always** |
//! | `Decompressor::decompress` (whole-remaining-input contract) | `Finish` |
//!
//! The `Read`/`AsyncRead` adapters implement the "`None` until EOF, then
//! `Finish`" rule by switching to `Finish` the moment the inner reader
//! returns `Ok(0)`, which is what makes a truncated stream an error rather
//! than a short read.
//!
//! # Limits
//!
//! [`InflateStream::with_max_output`] and
//! [`InflateStream::with_ratio_guard`] are enforced **inside** a block, not
//! merely between blocks: a single dynamic block can legally expand a few
//! hundred kilobytes into hundreds of megabytes, so a between-blocks check
//! is no protection at all. Both bound **one stream** and are cleared by
//! [`InflateStream::reset`]; a container that resets the decoder per frame
//! or per strip (APNG, TIFF) must carry its own file-level budget.

use oxiarc_core::BitCache;
use oxiarc_core::error::Result;
use oxiarc_core::traits::FlushMode;

use crate::inflate_core::{ActiveTrees, InflateCore, InflateState, Trees, drive};
use crate::sink::{BoundedSink, GrowSink, History, InflateSink};
use crate::window::InflateWindow;
use crate::zlib::Adler32;

/// Default growable-output capacity, matching `Inflater::new`.
const DEFAULT_OUTPUT_CAPACITY: usize = 65536;

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Outcome of one [`InflateStream::inflate`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InflateProgress {
    /// Bytes taken from `input`. These must never be fed again.
    ///
    /// A byte counts as consumed the moment it enters the decoder's bit
    /// accumulator, which can be several bytes before its bits are used.
    pub consumed: usize,
    /// Bytes written to the front of `output`.
    pub produced: usize,
    /// Why the call returned.
    pub status: InflateStatus,
}

/// Why an [`InflateStream::inflate`] call returned.
///
/// `#[non_exhaustive]` so a future status can be added in a minor release,
/// mirroring [`oxiarc_core::traits::DecompressStatus`]. Match with a
/// wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InflateStatus {
    /// All of `input` was absorbed and the stream is not finished.
    ///
    /// One documented exception: under [`FlushMode::Sync`] the call also
    /// returns at an RFC 1951 sync-flush boundary (an empty stored block),
    /// which can happen with input still unconsumed. A caller driving
    /// `Sync` therefore distinguishes the two with
    /// [`InflateStream::at_sync_flush`] and must resume from
    /// [`InflateProgress::consumed`] rather than assume the slice was
    /// absorbed whole:
    ///
    /// ```text
    /// NeedInput && !at_sync_flush()  =>  every byte of `input` was absorbed
    /// NeedInput &&  at_sync_flush()  =>  a sync-flush unit ended at `consumed`
    /// ```
    NeedInput,
    /// `output` is full and the stream is not finished.
    NeedOutput,
    /// The final block's end-of-block marker has been decoded.
    StreamEnd,
}

// ---------------------------------------------------------------------------
// InflateStream
// ---------------------------------------------------------------------------

/// A resumable raw-DEFLATE (RFC 1951) decoder.
///
/// See the [module documentation](self) for the push-decoding example, the
/// flush-mode table and the limit semantics.
#[derive(Debug)]
pub struct InflateStream {
    core: InflateCore,
    trees: Trees,
    /// 32 KiB linear history, used by the bounded front end. It lives here
    /// rather than in `InflateCore` so a sink borrowing it and the symbol
    /// loop borrowing the core are disjoint field borrows.
    history: History,
    dict_checksum: Option<u32>,
}

impl Default for InflateStream {
    fn default() -> Self {
        Self::new()
    }
}

impl InflateStream {
    /// A decoder with the full 32 KiB history window.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::InflateStream;
    ///
    /// let stream = InflateStream::new();
    /// assert_eq!(stream.total_out(), 0);
    /// assert!(!stream.is_finished());
    /// ```
    pub fn new() -> Self {
        Self::with_window_capacity(crate::sink::WINDOW)
    }

    /// A decoder that retains at most `bytes` of history (clamped to the
    /// 32 KiB DEFLATE window).
    ///
    /// Nothing is pre-allocated; this only bounds the memory a long-lived
    /// stream can hold when the producer is known to use a small window.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::InflateStream;
    ///
    /// // A stream known to be produced with an 8 KiB window.
    /// let stream = InflateStream::with_window_capacity(8 * 1024);
    /// assert_eq!(stream.total_in(), 0);
    /// ```
    pub fn with_window_capacity(bytes: usize) -> Self {
        Self {
            core: InflateCore::new(),
            trees: Trees::new(),
            history: History::with_capacity(bytes),
            dict_checksum: None,
        }
    }

    /// Cap the total number of decoded bytes this stream may produce.
    ///
    /// Enforced during decoding — before each write, not merely between
    /// blocks — so a single expanding block cannot overshoot. Exceeding it
    /// is [`MemoryBudgetExceeded`](oxiarc_core::error::OxiArcError::MemoryBudgetExceeded).
    ///
    /// Hitting the cap is a *clean* stop: the call that reaches it returns
    /// `Ok` with the bytes decoded up to the cap and
    /// [`InflateStatus::NeedOutput`], and the **next** call raises the
    /// error. A caller that asked for at most `n` bytes therefore receives
    /// exactly `n` of them before the error, rather than losing them to a
    /// bare `Err`. (A *corrupt* stream is different: those bytes are not
    /// valid and are not reported — see [`InflateStream::inflate`].)
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::traits::FlushMode;
    /// use oxiarc_deflate::{deflate, InflateStream};
    ///
    /// // One megabyte of zeros is a single expanding block: the cap has to
    /// // fire inside it, not merely at the next block boundary.
    /// let compressed = deflate(&vec![0u8; 1 << 20], 6).expect("deflate");
    /// let mut stream = InflateStream::new().with_max_output(4096);
    /// let mut out = vec![0u8; 1024];
    /// let mut fed = 0usize;
    /// let mut produced = 0u64;
    /// let err = loop {
    ///     match stream.inflate(&compressed[fed..], &mut out, FlushMode::Finish) {
    ///         Ok(p) => {
    ///             fed += p.consumed;
    ///             produced += p.produced as u64;
    ///         }
    ///         Err(e) => break e,
    ///     }
    /// };
    /// assert_eq!(produced, 4096);
    /// assert!(err.to_string().contains("memory budget"));
    /// ```
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.core.max_output = Some(limit);
        self
    }

    /// Reject the stream once it expands by more than `ratio`, checked only
    /// after `min_output` bytes have been produced.
    ///
    /// Defence in depth for callers that cannot know the decoded size in
    /// advance; [`InflateStream::with_max_output`] is the load-bearing
    /// limit. The ratio is measured against the input consumed so far.
    /// Tripping it is [`ZipBomb`](oxiarc_core::error::OxiArcError::ZipBomb).
    #[must_use]
    pub fn with_ratio_guard(mut self, ratio: f64, min_output: u64) -> Self {
        self.core.ratio_guard = Some((ratio, min_output));
        self
    }

    /// In-place form of [`InflateStream::with_max_output`], for framing
    /// layers that own a core rather than build one.
    pub(crate) fn set_max_output(&mut self, limit: u64) {
        self.core.max_output = Some(limit);
    }

    /// In-place form of [`InflateStream::with_ratio_guard`].
    pub(crate) fn set_ratio_guard(&mut self, ratio: f64, min_output: u64) {
        self.core.ratio_guard = Some((ratio, min_output));
    }

    /// Decode a prefix of `input` into a prefix of `output`.
    ///
    /// Returns how much of each was used and why the call returned. All
    /// decoder state survives, so any byte split of the same compressed
    /// stream produces byte-identical output.
    ///
    /// # Errors
    ///
    /// Any RFC 1951 violation, a back-reference beyond the retained history,
    /// a configured limit being exceeded, or — under
    /// [`FlushMode::Finish`] — the stream ending mid-symbol. The error is
    /// latched: every later call replays it until [`InflateStream::reset`].
    ///
    /// Bytes decoded during a call that then fails on **corrupt input** are
    /// not reported: they are not trustworthy, and reporting them would
    /// tempt a caller into keeping them. A [`InflateStream::with_max_output`]
    /// or [`InflateStream::with_ratio_guard`] stop is the exception — those
    /// bytes *are* valid, so they come back with `Ok(NeedOutput)` and the
    /// error arrives on the following call.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::traits::FlushMode;
    /// use oxiarc_deflate::{deflate, InflateStatus, InflateStream};
    ///
    /// let compressed = deflate(b"resumable", 6).expect("deflate");
    /// let mut stream = InflateStream::new();
    /// let mut out = [0u8; 32];
    /// let progress = stream
    ///     .inflate(&compressed, &mut out, FlushMode::Finish)
    ///     .expect("inflate");
    /// assert_eq!(progress.status, InflateStatus::StreamEnd);
    /// assert_eq!(&out[..progress.produced], b"resumable");
    /// ```
    pub fn inflate(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<InflateProgress> {
        if let Some(fault) = &self.core.fault {
            return Err(fault.to_error());
        }
        self.core.sync_flush = false;
        if self.core.state == InflateState::Done {
            return Ok(InflateProgress {
                consumed: 0,
                produced: 0,
                status: InflateStatus::StreamEnd,
            });
        }

        let mut in_pos = 0usize;
        self.core.bits_base = self.core.cache.consumed();
        let (produced, status) = {
            let mut sink = BoundedSink::new(output, Some(&self.history));
            let status = drive(
                &mut self.core,
                &mut self.trees,
                &mut sink,
                input,
                &mut in_pos,
                flush,
            )?;
            (sink.position(), status)
        };
        self.core.bits_used += self.core.cache.consumed() - self.core.bits_base;
        self.core.bits_base = self.core.cache.consumed();
        self.core.total_in += in_pos as u64;
        // One bounded memcpy per call, never per symbol.
        if let Some(fresh) = output.get(..produced) {
            self.history.append(fresh);
        }
        Ok(InflateProgress {
            consumed: in_pos,
            produced,
            status,
        })
    }

    /// Sink-generic form used by the crate's own front ends.
    ///
    /// Returns `(bytes consumed from input, status)`; the sink reports how
    /// much it received. Unlike [`InflateStream::inflate`] this does **not**
    /// touch the stream's history window — a growable sink is its own
    /// history, and `inflate_into`-style callers deliberately have none.
    pub(crate) fn inflate_sink<S: InflateSink>(
        &mut self,
        input: &[u8],
        sink: &mut S,
        flush: FlushMode,
    ) -> Result<(usize, InflateStatus)> {
        if let Some(fault) = &self.core.fault {
            return Err(fault.to_error());
        }
        self.core.sync_flush = false;
        if self.core.state == InflateState::Done {
            return Ok((0, InflateStatus::StreamEnd));
        }
        let mut in_pos = 0usize;
        self.core.bits_base = self.core.cache.consumed();
        let status = drive(
            &mut self.core,
            &mut self.trees,
            sink,
            input,
            &mut in_pos,
            flush,
        )?;
        self.core.bits_used += self.core.cache.consumed() - self.core.bits_base;
        self.core.bits_base = self.core.cache.consumed();
        self.core.total_in += in_pos as u64;
        Ok((in_pos, status))
    }

    /// Decode from the current position to the end of the stream, growing
    /// the output as needed.
    ///
    /// This is the growable counterpart of [`InflateStream::inflate`]: it
    /// drives the same state machine with an unbounded sink, so no history
    /// copy is performed at all (the accumulated output *is* the history).
    /// The stream's configured limits still apply.
    ///
    /// Only the bytes produced by this call are returned; a preset
    /// dictionary set with [`InflateStream::set_dictionary`] is used as
    /// history but is not part of the result.
    ///
    /// # Errors
    ///
    /// As [`InflateStream::inflate`] with [`FlushMode::Finish`]: `input`
    /// must contain the rest of the stream.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::{deflate, InflateStream};
    ///
    /// let compressed = deflate(b"grow me", 6).expect("deflate");
    /// let mut stream = InflateStream::new();
    /// assert_eq!(stream.inflate_to_vec(&compressed).expect("inflate"), b"grow me");
    /// ```
    pub fn inflate_to_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut window = InflateWindow::with_capacity(DEFAULT_OUTPUT_CAPACITY);
        if self.history.len() > 0 {
            window.preload_dictionary(self.history.as_slice());
        }
        {
            let mut sink = GrowSink::new(&mut window);
            self.inflate_sink(input, &mut sink, FlushMode::Finish)?;
        }
        Ok(window.into_output())
    }

    /// Install a preset dictionary (RFC 1950 `FDICT`, MSZIP, a CAB folder
    /// continuation), returning its Adler-32.
    ///
    /// Only the trailing window is retained, matching zlib.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_deflate::InflateStream;
    ///
    /// let mut stream = InflateStream::new();
    /// let checksum = stream.set_dictionary(b"shared prefix");
    /// assert_eq!(Some(checksum), stream.dictionary_checksum());
    /// ```
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        self.history.set_dictionary(dictionary);
        let checksum = Adler32::checksum(dictionary);
        self.dict_checksum = Some(checksum);
        checksum
    }

    /// Adler-32 of the dictionary installed by
    /// [`InflateStream::set_dictionary`], if any.
    pub fn dictionary_checksum(&self) -> Option<u32> {
        self.dict_checksum
    }

    /// Full reset: decoding state, bit accumulator, history, counters and
    /// the latched fault. Equivalent to a fresh stream that kept its
    /// allocations; configured limits are preserved.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::traits::FlushMode;
    /// use oxiarc_deflate::{deflate, InflateStream};
    ///
    /// let mut stream = InflateStream::new();
    /// let mut out = [0u8; 64];
    /// // A corrupt stream latches its error...
    /// assert!(stream.inflate(&[0xFF, 0xFF, 0xFF], &mut out, FlushMode::Finish).is_err());
    /// assert!(stream.inflate(&[], &mut out, FlushMode::None).is_err());
    /// // ...until it is reset.
    /// stream.reset();
    /// let good = deflate(b"fresh", 6).expect("deflate");
    /// let p = stream.inflate(&good, &mut out, FlushMode::Finish).expect("inflate");
    /// assert_eq!(&out[..p.produced], b"fresh");
    /// ```
    pub fn reset(&mut self) {
        self.core.clear_decode_state();
        self.core.cache = BitCache::default();
        self.core.bits_used = 0;
        self.core.bits_base = 0;
        self.core.total_in = 0;
        self.core.total_out = 0;
        self.trees.which = ActiveTrees::Fixed;
        self.history.clear();
        self.dict_checksum = None;
    }

    /// As [`InflateStream::reset`], but the history window is kept so
    /// back-references can reach across the boundary (RFC 4978 §3, MSZIP).
    pub fn reset_keep_history(&mut self) {
        let checksum = self.dict_checksum;
        self.core.clear_decode_state();
        self.core.cache = BitCache::default();
        self.core.bits_used = 0;
        self.core.bits_base = 0;
        self.core.total_in = 0;
        self.core.total_out = 0;
        self.trees.which = ActiveTrees::Fixed;
        self.dict_checksum = checksum;
    }

    /// Reset for the next member of a concatenated stream, **preserving the
    /// bit accumulator verbatim**.
    ///
    /// This is mandatory, not an optimisation. A zlib member ends with a
    /// 4-byte trailer, so after aligning and draining it the accumulator can
    /// still hold 1-3 bytes that belong to the *next* member; those bytes
    /// were already reported as consumed and can never be re-fed from
    /// `input`. Using [`InflateStream::reset`] here loses them and every
    /// member after the first decodes from the wrong offset.
    ///
    /// `total_in`/`total_out` stay cumulative so a configured limit bounds
    /// the whole concatenated stream rather than each member separately.
    pub fn reset_for_next_member(&mut self) {
        self.core.clear_decode_state();
        self.trees.which = ActiveTrees::Fixed;
        self.history.clear();
    }

    /// Discard the sub-byte remainder so the accumulator holds whole bytes
    /// only — the framing layer's equivalent of
    /// `BitReader::align_to_byte`.
    pub fn align_to_byte(&mut self) {
        let discarded = self.core.cache.align_to_byte();
        self.core.bits_used += discarded as u64;
        self.core.bits_base = self.core.cache.consumed();
    }

    /// Whether the final block's end-of-block marker has been decoded.
    pub fn is_finished(&self) -> bool {
        self.core.state == InflateState::Done
    }

    /// Whether the most recently completed block was an empty stored block,
    /// i.e. an RFC 1951 sync flush / RFC 4978 boundary.
    ///
    /// Cleared at the start of the next [`InflateStream::inflate`] call.
    pub fn at_sync_flush(&self) -> bool {
        self.core.sync_flush
    }

    /// Take one whole byte still sitting in the bit accumulator, in stream
    /// order.
    ///
    /// The framing layer drains these before touching `input` again: they
    /// were counted as consumed when they were absorbed, so they are not
    /// available anywhere else. An implicit byte alignment is performed
    /// first, because a byte read across a partial-bit boundary would be
    /// shifted garbage.
    ///
    /// Because of that alignment this can advance
    /// [`InflateStream::bits_consumed`]; sample that value *before* the
    /// first call if you need the DEFLATE payload's exact bit length.
    pub fn take_buffered_byte(&mut self) -> Option<u8> {
        self.align_to_byte();
        let byte = self.core.cache.take_byte();
        if byte.is_some() {
            self.core.bits_used += 8;
            self.core.bits_base = self.core.cache.consumed();
        }
        byte
    }

    /// Bits still held in the accumulator.
    ///
    /// Exposed for tests and framing layers that need to tell
    /// [`InflateStream::reset`] apart from
    /// [`InflateStream::reset_for_next_member`].
    #[doc(hidden)]
    pub fn buffered_bits(&self) -> u8 {
        self.core.cache.available()
    }

    /// Bits of the DEFLATE stream actually consumed (not merely absorbed
    /// into the accumulator).
    ///
    /// `bits_consumed().div_ceil(8)` reproduces the byte count
    /// `Inflater::inflate_consumed` reports.
    pub fn bits_consumed(&self) -> u64 {
        self.core.bits_used + self.core.cache.consumed() - self.core.bits_base
    }

    /// Total bytes taken from callers' `input` slices.
    pub fn total_in(&self) -> u64 {
        self.core.total_in
    }

    /// Total bytes decoded.
    pub fn total_out(&self) -> u64 {
        self.core.total_out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::deflate;

    /// Drive the push decoder to completion, asserting a call-count bound so
    /// a state machine that stops making progress fails instead of hanging.
    fn decode_all(
        compressed: &[u8],
        chunk: usize,
        out_size: usize,
        expected_out: usize,
    ) -> Result<Vec<u8>> {
        let mut stream = InflateStream::new();
        let mut out = Vec::new();
        let mut scratch = vec![0u8; out_size];
        let mut fed = 0usize;
        let mut calls = 0usize;
        // Each call must absorb a chunk of input or fill the output buffer;
        // a factor of four leaves room for the interleaving of the two.
        let bound = 4 * (compressed.len() / chunk.max(1) + expected_out / out_size.max(1) + 8);
        loop {
            calls += 1;
            assert!(
                calls < bound,
                "decoder made no progress after {calls} calls"
            );
            let end = (fed + chunk).min(compressed.len());
            let slice = compressed.get(fed..end).unwrap_or_default();
            let flush = if end >= compressed.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = stream.inflate(slice, &mut scratch, flush)?;
            fed += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            if progress.status == InflateStatus::StreamEnd {
                return Ok(out);
            }
        }
    }

    #[test]
    fn byte_at_a_time_matches_one_shot() {
        let original = b"the quick brown fox jumps over the lazy dog. ".repeat(200);
        for level in [0u8, 1, 6, 9] {
            let compressed = deflate(&original, level).expect("deflate");
            let decoded = decode_all(&compressed, 1, 1, original.len()).expect("decode");
            assert_eq!(decoded, original, "level {level}");
        }
    }

    #[test]
    fn reset_clears_the_accumulator_but_next_member_keeps_it() {
        let compressed = deflate(b"accumulator residue check", 6).expect("deflate");
        let mut padded = compressed.clone();
        padded.extend_from_slice(&[0xAA, 0xBB, 0xCC]);

        let mut stream = InflateStream::new();
        let mut out = [0u8; 128];
        let progress = stream
            .inflate(&padded, &mut out, FlushMode::None)
            .expect("inflate");
        assert_eq!(progress.status, InflateStatus::StreamEnd);
        // The trailing bytes were absorbed into the accumulator.
        let buffered = stream.buffered_bits();
        assert!(buffered >= 8, "expected residue, got {buffered} bits");

        let mut kept = InflateStream::new();
        kept.inflate(&padded, &mut out, FlushMode::None)
            .expect("inflate");
        kept.reset_for_next_member();
        assert_eq!(kept.buffered_bits(), buffered);

        stream.reset();
        assert_eq!(stream.buffered_bits(), 0);
    }

    #[test]
    fn zero_length_input_and_output_calls_are_stable() {
        let compressed = deflate(b"zero length probes", 6).expect("deflate");
        let mut stream = InflateStream::new();
        let mut out = [0u8; 64];

        let p = stream
            .inflate(&[], &mut out, FlushMode::None)
            .expect("empty input");
        assert_eq!(p.consumed, 0);
        assert_eq!(p.produced, 0);
        assert_eq!(p.status, InflateStatus::NeedInput);

        let p = stream
            .inflate(&compressed, &mut [], FlushMode::None)
            .expect("empty output");
        assert_eq!(p.produced, 0);
        assert_eq!(p.status, InflateStatus::NeedOutput);
    }

    #[test]
    fn sticky_fault_replays_the_same_error() {
        let mut stream = InflateStream::new();
        let mut out = [0u8; 32];
        // BTYPE = 3 is reserved.
        let first = stream
            .inflate(&[0b0000_0111], &mut out, FlushMode::Finish)
            .expect_err("reserved block type");
        let message = first.to_string();
        for _ in 0..3 {
            let again = stream
                .inflate(&[], &mut out, FlushMode::None)
                .expect_err("latched");
            assert_eq!(again.to_string(), message);
        }
    }

    /// `InflateStatus::NeedInput` documents "all of `input` was absorbed",
    /// with one exception: under [`FlushMode::Sync`] the call stops at a
    /// sync-flush boundary, which can leave input unconsumed. The exception
    /// is real, so it is pinned here rather than left to a reader's trust.
    ///
    /// A fourth status variant was considered and rejected: the Phase 8
    /// interface contract fixes the set at
    /// `{NeedInput, NeedOutput, StreamEnd}`, and
    /// [`InflateStream::at_sync_flush`] already carries the distinction.
    #[test]
    fn sync_flush_returns_need_input_with_bytes_left_over() {
        // Two sync-flushed units, so the first boundary lands well before
        // the end of the slice.
        let mut encoder = crate::Deflater::new(6);
        let mut stream_bytes = Vec::new();
        encoder
            .deflate_sync(b"first unit", &mut stream_bytes)
            .expect("sync flush");
        encoder
            .deflate_sync(b"second unit", &mut stream_bytes)
            .expect("sync flush");
        encoder
            .deflate(b"tail", &mut stream_bytes, true)
            .expect("finish");

        let mut stream = InflateStream::new();
        let mut out = [0u8; 256];
        let progress = stream
            .inflate(&stream_bytes, &mut out, FlushMode::Sync)
            .expect("inflate");
        assert_eq!(progress.status, InflateStatus::NeedInput);
        assert!(stream.at_sync_flush(), "the call stopped at a boundary");
        assert!(
            progress.consumed < stream_bytes.len(),
            "the documented exception: {} of {} bytes consumed",
            progress.consumed,
            stream_bytes.len()
        );
        assert_eq!(&out[..progress.produced], b"first unit");

        // Resuming from `consumed` decodes the rest, so the status is a
        // stopping point and not a lost byte.
        let mut fed = progress.consumed;
        let mut decoded = out[..progress.produced].to_vec();
        loop {
            let p = stream
                .inflate(
                    stream_bytes.get(fed..).unwrap_or_default(),
                    &mut out,
                    FlushMode::Sync,
                )
                .expect("resume");
            fed += p.consumed;
            decoded.extend_from_slice(&out[..p.produced]);
            if p.status == InflateStatus::StreamEnd {
                break;
            }
            assert!(
                p.consumed > 0 || p.produced > 0 || stream.at_sync_flush(),
                "no progress"
            );
        }
        assert_eq!(decoded, b"first unitsecond unittail");
    }

    /// Without `Sync` the promise is unconditional: every byte of a
    /// well-formed slice is absorbed before `NeedInput` comes back.
    #[test]
    fn need_input_without_sync_means_the_slice_was_absorbed() {
        let compressed = deflate(&b"absorbed whole".repeat(50), 6).expect("deflate");
        let partial = compressed.get(..compressed.len() - 3).unwrap_or_default();
        let mut stream = InflateStream::new();
        let mut out = [0u8; 4096];
        let progress = stream
            .inflate(partial, &mut out, FlushMode::None)
            .expect("inflate");
        assert_eq!(progress.status, InflateStatus::NeedInput);
        assert_eq!(progress.consumed, partial.len());
        assert!(!stream.at_sync_flush());
    }

    #[test]
    fn bits_consumed_is_byte_exact_for_the_payload() {
        let compressed = deflate(b"exact bit accounting", 6).expect("deflate");
        let mut trailing = compressed.clone();
        trailing.extend_from_slice(b"TRAILER!");
        let mut stream = InflateStream::new();
        let mut out = [0u8; 128];
        stream
            .inflate(&trailing, &mut out, FlushMode::None)
            .expect("inflate");
        assert_eq!(stream.bits_consumed().div_ceil(8), compressed.len() as u64);
    }
}

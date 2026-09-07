//! Blocking [`Read`] adapter over the resumable inflate core.
//!
//! [`InflateReader`] turns any [`Read`] source of compressed bytes into a
//! [`Read`] source of decompressed bytes. It is parameterised by
//! [`InflateWrapper`], so gzip, zlib, raw DEFLATE and a
//! detect-from-the-first-two-bytes mode are the same type — which is what
//! an HTTP `Content-Encoding: gzip | deflate | x-gzip` layer needs.
//!
//! Unlike the eager decoders it replaces, nothing is read ahead: memory is
//! `64 KiB` of compressed staging + `64 KiB` of decoded staging + the
//! 32 KiB LZ77 history, whatever the size of the stream.
//!
//! # Example
//!
//! ```
//! use std::io::Read;
//! use oxiarc_deflate::{InflateReader, gzip_compress};
//!
//! let compressed = gzip_compress(b"streamed through a socket", 6).expect("gzip");
//! let mut reader = InflateReader::gzip(&compressed[..]);
//! let mut plain = String::new();
//! reader.read_to_string(&mut plain).expect("inflate");
//! assert_eq!(plain, "streamed through a socket");
//! assert_eq!(reader.total_in(), compressed.len() as u64);
//! ```
//!
//! # I/O contract
//!
//! * [`io::ErrorKind::Interrupted`] from the inner reader is retried.
//! * [`io::ErrorKind::WouldBlock`] is **propagated unchanged** and never
//!   turned into `Ok(0)`: on a non-blocking source "no bytes right now" and
//!   "end of stream" must not be confused. Already-decoded bytes are served
//!   first, so `WouldBlock` only reaches the caller when the staging buffer
//!   is empty, and the reader can be polled again afterwards.
//! * `Ok(0)` from the inner reader switches the decoder's flush mode to
//!   [`FlushMode::Finish`], so a stream that ends in the middle of a member
//!   is an [`io::Error`] rather than a short read.
//! * A decoder that still asks for input *after* that switch is
//!   [`io::ErrorKind::UnexpectedEof`] ("decoder still needs input after the
//!   end of the stream"). Under `Finish` every framing state raises
//!   `UnexpectedEof` itself, so this is unreachable — it exists so that
//!   "the source ended early" can never become a clean `Ok(0)` by a route
//!   nobody was looking at. The one adapter that legitimately ends on a
//!   silent source is `RawInflateReader` (RFC 4978), which stays in
//!   [`FlushMode::None`] and is excluded by that.
//! * A decoder that neither consumes nor produces anything twice in a row
//!   is reported as [`io::ErrorKind::InvalidData`] ("decoder made no
//!   progress"), never a silent spin.
//! * Errors are **sticky**: a reader that has failed keeps returning an
//!   error of the same kind, and never decays into `Ok(0)` or into bytes
//!   decoded by the call that failed.

use std::io::{self, Read};

use oxiarc_core::error::OxiArcError;
use oxiarc_core::traits::FlushMode;

use crate::stream::InflateStatus;
use crate::wrapper::{GzipHeaderInfo, InflateWrapper, TrailingPolicy, WrappedInflate};

/// Compressed-input and decoded-output staging buffer size.
///
/// The output buffer is **not** an optimisation. Decoding straight into a
/// caller's `buf` costs one history update per call, so a caller reading
/// three bytes at a time would pay a 32 KiB window update per three bytes.
/// At 64 KiB the update amortises to at most half a byte copied per
/// produced byte.
pub(crate) const STAGING_BUFFER: usize = 64 * 1024;

/// Shortest tail after a complete zlib member that is still worth trying to
/// decode as another member.
///
/// The smallest possible zlib member is 8 bytes (2 header + 2 for an empty
/// final stored block + 4 Adler-32), so anything below that cannot be one.
/// The threshold is 6 rather than 8 because that is the number the
/// whole-slice decoder this rule reproduces used (`streaming.rs`'s
/// `if remaining.len() < 6 { break; }`), and widening it would change which
/// inputs a `ZlibStreamDecoder` accepts. A 6- or 7-byte tail therefore still
/// reaches the framing layer and fails there, which is the correct answer
/// either way.
const MIN_ZLIB_MEMBER_TAIL: usize = 6;

/// Whether a flush mode means "this is the last input there will ever be".
///
/// Only [`FlushMode::Finish`] does; `FlushMode` is `#[non_exhaustive]`, so
/// anything added later is encoder-side and reads as `None`.
#[inline]
fn is_finish(flush: FlushMode) -> bool {
    match flush {
        FlushMode::Finish => true,
        FlushMode::None | FlushMode::Sync | FlushMode::Full | FlushMode::Partial => false,
        _ => false,
    }
}

/// Translate a decoder error into the `io::Error` a [`Read`] impl must
/// return, keeping "the stream stopped early" distinguishable from "the
/// stream is corrupt".
pub(crate) fn to_io_error(error: OxiArcError) -> io::Error {
    match error {
        OxiArcError::Io(inner) => inner,
        OxiArcError::UnexpectedEof { .. } => {
            io::Error::new(io::ErrorKind::UnexpectedEof, error.to_string())
        }
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// The framing-independent half of the `Read`/`AsyncRead` adapters: two
/// staging buffers, the [`WrappedInflate`] core and the rules that decide
/// when a stream has ended.
///
/// It performs no I/O itself — the caller fills [`InflatePump::input_slot`]
/// and reports the result — so the blocking and async adapters share one
/// implementation and therefore one set of end-of-stream semantics.
#[derive(Debug)]
pub(crate) struct InflatePump {
    core: WrappedInflate,
    in_buf: Box<[u8]>,
    in_pos: usize,
    in_len: usize,
    out_buf: Box<[u8]>,
    out_pos: usize,
    out_len: usize,
    src_eof: bool,
    finished: bool,
    /// Flush mode used once the source has signalled EOF. `Finish` for
    /// every framed decoder; `None` for RFC 4978, where the peer may still
    /// send more and EOF is a closed connection rather than truncation.
    eof_flush: FlushMode,
    /// Extend the zlib short-tail rule to a stream that has not decoded a
    /// single member yet. Only the legacy `ZlibStreamDecoder` sets this: it
    /// is what makes an empty source an empty result instead of an error.
    lenient_short_tail: bool,
    /// Consecutive calls that neither consumed nor produced a byte.
    stalls: u8,
}

impl InflatePump {
    /// Build a pump around a configured decoder.
    pub(crate) fn new(core: WrappedInflate) -> Self {
        Self {
            core,
            in_buf: vec![0u8; STAGING_BUFFER].into_boxed_slice(),
            in_pos: 0,
            in_len: 0,
            out_buf: vec![0u8; STAGING_BUFFER].into_boxed_slice(),
            out_pos: 0,
            out_len: 0,
            src_eof: false,
            finished: false,
            eof_flush: FlushMode::Finish,
            lenient_short_tail: false,
            stalls: 0,
        }
    }

    /// Never treat source EOF as the end of the compressed stream.
    ///
    /// Only RFC 4978 needs this, and its reader is async-only.
    #[cfg(feature = "async-io")]
    pub(crate) fn set_eof_flush(&mut self, flush: FlushMode) {
        self.eof_flush = flush;
    }

    /// Allow the zlib short-tail rule before the first member.
    pub(crate) fn set_lenient_short_tail(&mut self, yes: bool) {
        self.lenient_short_tail = yes;
    }

    /// The configured decoder.
    pub(crate) fn core(&self) -> &WrappedInflate {
        &self.core
    }

    /// Apply one of [`WrappedInflate`]'s consuming builders in place.
    ///
    /// The builders take `self` by value, so the adapters' own builders
    /// route through this rather than reaching into the field.
    pub(crate) fn replace_core<F>(&mut self, build: F)
    where
        F: FnOnce(WrappedInflate) -> WrappedInflate,
    {
        let core = std::mem::replace(&mut self.core, WrappedInflate::new(InflateWrapper::Raw));
        self.core = build(core);
    }

    /// Whether the stream has ended **and** every decoded byte has been
    /// handed to the caller.
    ///
    /// Both halves matter: the end of the compressed stream is detected
    /// while the last staging buffer is still full, so `finished` alone
    /// would report completion with bytes still owed to the caller.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished && self.out_pos >= self.out_len
    }

    /// Whether the pump can make progress without more input.
    pub(crate) fn wants_input(&self) -> bool {
        !self.src_eof && self.in_pos >= self.in_len
    }

    /// Whether nothing further will ever be produced. Same condition as
    /// [`InflatePump::is_finished`], named for the read loop that uses it.
    pub(crate) fn is_done(&self) -> bool {
        self.is_finished()
    }

    /// The buffer the caller must read compressed bytes into.
    pub(crate) fn input_slot(&mut self) -> &mut [u8] {
        &mut self.in_buf
    }

    /// Record the result of filling [`InflatePump::input_slot`].
    pub(crate) fn note_read(&mut self, filled: usize) {
        if filled == 0 {
            self.src_eof = true;
            return;
        }
        self.in_pos = 0;
        self.in_len = filled.min(self.in_buf.len());
    }

    /// Decoded bytes not yet handed out.
    pub(crate) fn staged(&self) -> &[u8] {
        self.out_buf.get(self.out_pos..self.out_len).unwrap_or(&[])
    }

    /// Mark `count` staged bytes as delivered.
    pub(crate) fn consume_staged(&mut self, count: usize) {
        self.out_pos = self.out_pos.saturating_add(count).min(self.out_len);
    }

    /// Copy as many staged bytes as fit into `dst`.
    pub(crate) fn take_staged(&mut self, dst: &mut [u8]) -> usize {
        let available = self.staged();
        let count = available.len().min(dst.len());
        if count == 0 {
            return 0;
        }
        if let Some(src) = available.get(..count) {
            if let Some(slot) = dst.get_mut(..count) {
                slot.copy_from_slice(src);
            }
        }
        self.consume_staged(count);
        count
    }

    /// The zlib rule from `streaming.rs`: a 1-5 byte tail left after a
    /// complete member is dropped without attempting another member.
    ///
    /// It lives here rather than in [`WrappedInflate`] because only this
    /// layer knows that the source has reached EOF. Without it a crafted
    /// two-byte tail that satisfies `CM == 8` and `(CMF*256+FLG) % 31 == 0`
    /// starts a member and fails with `UnexpectedEof`, where a whole-slice
    /// decoder ignores it.
    ///
    /// # What is measured
    ///
    /// The whole-slice decoder this reproduces tested `remaining.len()`,
    /// i.e. **the bytes after the last complete member**. This pump's own
    /// buffer level is not that number: the core absorbs every byte it is
    /// given whenever it returns `NeedInput`, so at the moment `src_eof`
    /// becomes true the unconsumed count is always `0` — mid-block, mid-
    /// trailer or wherever else. Measuring it would end *any* truncated
    /// stream cleanly at `Ok(0)`, which is the silent-truncation defect
    /// this whole layer exists to remove. The count therefore comes from
    /// [`WrappedInflate::member_in`], which the wrapper re-bases at every
    /// member boundary, plus whatever this pump has not handed over yet.
    ///
    /// # When it may fire
    ///
    /// Dropping bytes silently is a *leniency*, so it is confined to
    /// [`TrailingPolicy::Stop`]. Under `Reject` and `AllowZeros` the tail
    /// reaches [`WrappedInflate`] and is judged there — which is what makes
    /// a crafted 1-5 byte tail an error for a strict reader, exactly as one
    /// trailing byte already is for raw framing.
    ///
    /// It is confined further, to a fragment that has decoded **nothing**.
    /// The whole-slice decoder tested `remaining.len() < 6` *before* it
    /// touched the bytes, so a short fragment never contributed a single
    /// output byte. A push decoder cannot look ahead: it decodes the
    /// fragment as it arrives and only learns at EOF that no more is
    /// coming, by which time any bytes it produced have already been
    /// served. Ending the stream cleanly at that point would hand the
    /// caller payload from a member whose Adler-32 will never be checked —
    /// the silent-truncation class this layer exists to remove — and the
    /// bytes cannot be taken back, so the only honest answer is the error
    /// the truncation itself deserves. Four bytes are enough to reach it:
    /// `78 9c 72 04` is a valid zlib header plus a fixed-Huffman block
    /// holding one literal.
    ///
    /// [`WrappedInflate::member_out`] is zero between members and while the
    /// next member's header is being read, so every fragment the
    /// whole-slice decoder ignored — padding, garbage, a crafted header,
    /// even a header followed by an unfinished block length — still ends
    /// the stream cleanly here.
    fn short_tail_ends_stream(&self) -> bool {
        if !self.src_eof || self.finished || self.core.is_finished() {
            return false;
        }
        if self.core.active_wrapper() != InflateWrapper::Zlib {
            return false;
        }
        if self.core.trailing_policy_in_force() != TrailingPolicy::Stop {
            return false;
        }
        if self.core.member_out() > 0 {
            return false;
        }
        let staged_in = self.in_len.saturating_sub(self.in_pos) as u64;
        let tail = self.core.member_in().saturating_add(staged_in);
        if tail >= MIN_ZLIB_MEMBER_TAIL as u64 {
            return false;
        }
        self.core.members_decoded() >= 1 || self.lenient_short_tail
    }

    /// Run the decoder once, refilling the output staging buffer.
    pub(crate) fn step(&mut self) -> io::Result<()> {
        if self.short_tail_ends_stream() {
            self.finished = true;
            return Ok(());
        }
        let flush = if self.src_eof {
            self.eof_flush
        } else {
            FlushMode::None
        };
        let progress = self
            .core
            .inflate(
                self.in_buf
                    .get(self.in_pos..self.in_len)
                    .unwrap_or_default(),
                &mut self.out_buf,
                flush,
            )
            .map_err(to_io_error)?;

        self.in_pos = self.in_pos.saturating_add(progress.consumed);
        self.out_pos = 0;
        self.out_len = progress.produced;

        match progress.status {
            InflateStatus::StreamEnd => self.finished = true,
            // The source is exhausted and the decoder still wants input.
            //
            // Under `eof_flush == FlushMode::None` — RFC 4978 only, where a
            // silent peer is not truncation — that is the end of the stream.
            //
            // Under `FlushMode::Finish` it must not be: the decoder was told
            // this was the last input, so "give me more" contradicts the
            // flush mode and every framing state reaches it only by raising
            // `UnexpectedEof` first. Treating it as a clean end here would
            // be a second, structural silent-truncation path behind the
            // first (the short-tail rule above), so it is reported instead.
            InflateStatus::NeedInput if self.src_eof => {
                if is_finish(self.eof_flush) {
                    // Bytes decoded by a call that then reports truncation
                    // are dropped rather than served, matching
                    // `InflateStream::inflate`'s rule for a failing call:
                    // they belong to a member that was never completed, so
                    // handing them over would tempt a caller into keeping
                    // them and would make an `Err` be followed by an `Ok`.
                    self.out_pos = 0;
                    self.out_len = 0;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "decoder still needs input after the end of the stream",
                    ));
                }
                self.finished = true;
            }
            _ => {}
        }

        if progress.consumed == 0 && progress.produced == 0 && !self.finished {
            self.stalls = self.stalls.saturating_add(1);
            if self.stalls >= 2 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "decoder made no progress",
                ));
            }
        } else {
            self.stalls = 0;
        }
        Ok(())
    }
}

/// A [`Read`] adapter that decompresses gzip, zlib or raw DEFLATE on the fly.
///
/// See the [module documentation](self) for the I/O contract (`Interrupted`,
/// `WouldBlock`, truncation) and a worked example.
///
/// # Defaults
///
/// [`InflateReader::new`] is **strict**: a bad header at offset 0 is an
/// error, both checksums are verified, concatenated members are decoded
/// (RFC 1952 §2.2 requires it for gzip and it is the norm for zlib), and
/// anything after the last member that does not start a member is rejected.
/// Relax any of that with the builder methods.
#[derive(Debug)]
pub struct InflateReader<R> {
    inner: R,
    pump: InflatePump,
}

impl<R: Read> InflateReader<R> {
    /// A strict reader for `wrapper`.
    ///
    /// Concatenated members are decoded for [`InflateWrapper::Gzip`] and
    /// [`InflateWrapper::Zlib`] (RFC 1952 §2.2 requires it for gzip, and it
    /// is the norm for zlib). Raw DEFLATE has no header to find a second
    /// member by, so for [`InflateWrapper::Raw`] the setting only decides
    /// how bytes after the final block are treated — and by default they are
    /// rejected. Pass
    /// [`trailing_policy(TrailingPolicy::Stop)`](InflateReader::trailing_policy)
    /// for a raw stream that may be followed by padding.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::Read;
    /// use oxiarc_deflate::{InflateReader, InflateWrapper, zlib_compress};
    ///
    /// let compressed = zlib_compress(b"zlib body", 6).expect("zlib");
    /// let mut reader = InflateReader::new(&compressed[..], InflateWrapper::Zlib);
    /// let mut out = Vec::new();
    /// reader.read_to_end(&mut out).expect("inflate");
    /// assert_eq!(out, b"zlib body");
    /// ```
    pub fn new(inner: R, wrapper: InflateWrapper) -> Self {
        Self::with_core(inner, WrappedInflate::new(wrapper).multi_member(true))
    }

    /// A reader for `Content-Encoding: gzip` (and `x-gzip`).
    pub fn gzip(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Gzip)
    }

    /// A reader for zlib-framed DEFLATE (RFC 1950).
    pub fn zlib(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Zlib)
    }

    /// A reader for raw DEFLATE (RFC 1951) with no container.
    pub fn raw(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Raw)
    }

    /// A reader that picks the framing from the first two bytes.
    ///
    /// The sniff happens at offset 0 only, with no re-try: gzip magic wins,
    /// then a structurally valid zlib header, then raw. Name the framing
    /// when it is known — a raw stream whose first two bytes happen to pass
    /// the zlib check is read as zlib.
    pub fn auto(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Auto)
    }

    /// Wrap a fully configured decoder.
    fn with_core(inner: R, core: WrappedInflate) -> Self {
        Self {
            inner,
            pump: InflatePump::new(core),
        }
    }

    /// Whether a concatenated stream of members is decoded (default `true`).
    ///
    /// Only gzip and zlib framing can start a second member; see
    /// [`InflateReader::new`] for what this means for raw DEFLATE.
    #[must_use]
    pub fn multi_member(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.multi_member(yes));
        self
    }

    /// What to do with bytes after the last member (default
    /// [`TrailingPolicy::Reject`]).
    #[must_use]
    pub fn trailing_policy(mut self, policy: TrailingPolicy) -> Self {
        self.pump.replace_core(|core| core.trailing_policy(policy));
        self
    }

    /// Fail once more than `limit` bytes would be produced.
    ///
    /// Enforced *inside* a DEFLATE block, so a single block that expands a
    /// few hundred kilobytes into hundreds of megabytes is stopped at the
    /// limit rather than after it. Bytes decoded up to the limit are still
    /// delivered; the error surfaces on the following read.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.pump.replace_core(|core| core.with_max_output(limit));
        self
    }

    /// Fail once the output exceeds `ratio` times the input, ignoring the
    /// first `min_output` bytes.
    #[must_use]
    pub fn with_ratio_guard(mut self, ratio: f64, min_output: u64) -> Self {
        self.pump
            .replace_core(|core| core.with_ratio_guard(ratio, min_output));
        self
    }

    /// Supply the preset dictionary a zlib `FDICT` stream asks for (also
    /// used as history for raw DEFLATE).
    #[must_use]
    pub fn with_dictionary(mut self, dictionary: &[u8]) -> Self {
        self.pump
            .replace_core(|core| core.with_dictionary(dictionary));
        self
    }

    /// Whether a gzip `FHCRC` header checksum is verified (default `true`).
    #[must_use]
    pub fn verify_header_crc(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.verify_header_crc(yes));
        self
    }

    /// Whether the trailer checksum is verified (default `true`).
    #[must_use]
    pub fn verify_checksum(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.verify_checksum(yes));
        self
    }

    /// Whether a bad header at offset 0 is an error (default `true`).
    #[must_use]
    pub fn strict_first_member(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.strict_first_member(yes));
        self
    }

    /// Apply the zlib short-tail rule before the first member too.
    pub(crate) fn with_lenient_short_tail(mut self, yes: bool) -> Self {
        self.pump.set_lenient_short_tail(yes);
        self
    }

    /// Borrow the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrow the inner reader.
    ///
    /// Reading from it directly loses bytes already staged in the decoder.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Consume the adapter and return the inner reader.
    ///
    /// Compressed bytes already staged are discarded.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Compressed bytes consumed by the decoder so far.
    pub fn total_in(&self) -> u64 {
        self.pump.core().total_in()
    }

    /// Decompressed bytes produced so far.
    ///
    /// This counts bytes the decoder has produced, some of which may still
    /// be staged inside the adapter rather than delivered.
    pub fn total_out(&self) -> u64 {
        self.pump.core().total_out()
    }

    /// Members fully decoded so far.
    pub fn members_decoded(&self) -> u32 {
        self.pump.core().members_decoded()
    }

    /// gzip header fields of the member most recently started.
    ///
    /// `None` until the first header has been parsed, and for non-gzip
    /// framing.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::io::Read;
    /// use oxiarc_deflate::{GzipEncoder, InflateReader};
    ///
    /// let compressed = GzipEncoder::new(6).compress(b"body").expect("gzip");
    /// let mut reader = InflateReader::gzip(&compressed[..]);
    /// let mut out = Vec::new();
    /// reader.read_to_end(&mut out).expect("inflate");
    /// assert!(reader.gzip_header().is_some());
    /// ```
    pub fn gzip_header(&self) -> Option<&GzipHeaderInfo> {
        self.pump.core().gzip_header()
    }

    /// Whether the stream has ended and every decoded byte has been read.
    pub fn is_finished(&self) -> bool {
        self.pump.is_finished()
    }
}

impl<R: Read> Read for InflateReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let served = self.pump.take_staged(buf);
            if served > 0 {
                return Ok(served);
            }
            if self.pump.is_done() {
                return Ok(0);
            }
            if self.pump.wants_input() {
                let filled = match self.inner.read(self.pump.input_slot()) {
                    Ok(filled) => filled,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    // WouldBlock included: propagated, never Ok(0).
                    Err(error) => return Err(error),
                };
                self.pump.note_read(filled);
                continue;
            }
            self.pump.step()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{gzip_compress, zlib_compress};

    #[test]
    fn take_staged_copies_a_prefix() {
        let mut pump = InflatePump::new(WrappedInflate::new(InflateWrapper::Raw));
        pump.out_buf[..5].copy_from_slice(b"hello");
        pump.out_len = 5;
        let mut dst = [0u8; 3];
        assert_eq!(pump.take_staged(&mut dst), 3);
        assert_eq!(&dst, b"hel");
        assert_eq!(pump.staged(), b"lo");
    }

    #[test]
    fn reader_reports_counters() {
        let compressed = gzip_compress(b"counter check", 6).expect("gzip");
        let mut reader = InflateReader::gzip(&compressed[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("inflate");
        assert_eq!(out, b"counter check");
        assert_eq!(reader.total_in(), compressed.len() as u64);
        assert_eq!(reader.total_out(), 13);
        assert_eq!(reader.members_decoded(), 1);
        assert!(reader.is_finished());
    }

    #[test]
    fn empty_read_buffer_is_not_eof() {
        let compressed = zlib_compress(b"payload", 6).expect("zlib");
        let mut reader = InflateReader::zlib(&compressed[..]);
        assert_eq!(reader.read(&mut []).expect("read"), 0);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("inflate");
        assert_eq!(out, b"payload");
    }

    /// The no-progress guard is an `Err`, not a `debug_assert`: in a
    /// release build a decoder that neither consumes nor produces would
    /// otherwise spin forever inside `read`.
    #[test]
    fn two_zero_progress_steps_are_an_error_not_a_spin() {
        let mut pump = InflatePump::new(WrappedInflate::new(InflateWrapper::Raw));
        // No input, and the source has not reached EOF, so the core has
        // nothing to do and reports `NeedInput` with nothing consumed.
        pump.step().expect("first idle step is tolerated");
        let error = pump.step().expect_err("a second idle step must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("no progress"));
    }

    /// One idle step followed by real progress resets the counter, so a
    /// slow source cannot be mistaken for a stalled decoder.
    #[test]
    fn progress_resets_the_stall_counter() {
        let compressed = zlib_compress(b"reset the counter", 6).expect("zlib");
        let mut pump = InflatePump::new(
            WrappedInflate::new(InflateWrapper::Zlib).trailing_policy(TrailingPolicy::Stop),
        );
        pump.step().expect("idle");
        let slot = pump.input_slot();
        let n = compressed.len().min(slot.len());
        slot[..n].copy_from_slice(&compressed[..n]);
        pump.note_read(n);
        pump.step().expect("progress");
        assert_eq!(pump.staged(), b"reset the counter");
        pump.consume_staged(pump.staged().len());
        pump.step().expect("idle again after progress");
    }

    #[test]
    fn into_inner_returns_the_source() {
        let reader = InflateReader::raw(&b"anything"[..]);
        assert_eq!(reader.into_inner(), b"anything");
    }
}

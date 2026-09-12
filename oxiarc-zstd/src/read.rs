//! `Read` adapter over the bounded [`ZstdStream`] push decoder.
//!
//! [`ZstdStreamDecoder`] is re-exported from [`crate::streaming`], which is
//! where it has always lived in the public API; it is implemented here so that
//! neither file grows past the workspace's file-size budget.

use crate::stream::{UNRESTRICTED_MAX_WINDOW, ZstdStatus, ZstdStream};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::FlushMode;
use std::io::{self, Read};

/// Staging buffer size for both the compressed input and the decompressed
/// output.
///
/// Decoding straight into a caller's three-byte `buf` would cost one window
/// update per three bytes; 64 KiB amortises the push-decoder call overhead to
/// roughly one call per 64 KiB of output.
const STAGING: usize = 64 * 1024;

/// Streaming Zstandard decoder that implements [`Read`].
///
/// Truly incremental: the first byte is served without reading the whole input,
/// and the decoder holds one 128 KiB block carry plus two 64 KiB staging
/// buffers plus the sliding window — never the compressed input and never the
/// decompressed output.
///
/// # What bounds the memory
///
/// Everything except the window is a fixed ~320 KiB. The window is allocated
/// lazily and never exceeds `min(the frame's declared Window_Size,
/// max(Block_Maximum_Decompressed_Size, the bytes actually produced))` — the
/// block-maximum floor (at most 128 KiB) is inherent, since one whole block has
/// to fit before it is drained. This type deliberately places **no ceiling on
/// the declared window** — that is its historical behaviour, and reference
/// frames compressed with `zstd --long` declare 16-128 MiB. So on a long stream
/// from an untrusted source the ring can still grow to whatever the frame
/// declares.
/// Two builders make it constant, and untrusted input should use at least one:
/// [`ZstdStreamDecoder::with_max_output`] bounds the ring by the bytes the
/// caller agreed to receive, and [`ZstdStreamDecoder::with_max_window`] refuses
/// an over-large declaration outright, before anything is allocated.
///
/// Concatenated frames are decoded as one logical stream and skippable frames
/// are dropped, matching [`crate::decompress_multi_frame`].
///
/// Supports optional progress reporting via [`ProgressHandle`] and cooperative
/// cancellation via [`CancellationToken`] using the
/// [`ZstdStreamDecoder::with_progress`] / [`ZstdStreamDecoder::with_cancel`]
/// builders, and an optional hard output cap via
/// [`ZstdStreamDecoder::with_max_output`].
///
/// # I/O behaviour
///
/// * [`io::ErrorKind::Interrupted`] from the inner reader is retried.
/// * [`io::ErrorKind::WouldBlock`] propagates unchanged — it never becomes a
///   spurious `Ok(0)`.
/// * `Ok(0)` from the inner reader switches the decoder to
///   [`FlushMode::Finish`], so a truncated stream is an error rather than a
///   short read.
/// * A decode step that consumes nothing and produces nothing at end of input
///   is reported as an error instead of spinning.
///
/// # Example
///
/// ```rust
/// use std::io::Read;
/// use oxiarc_zstd::{ZstdStreamDecoder, compress_with_level};
///
/// let frame = compress_with_level(b"bounded streaming decode", 3).expect("compress");
/// let mut decoder = ZstdStreamDecoder::new(&frame[..]).with_max_output(1 << 20);
/// let mut out = String::new();
/// decoder.read_to_string(&mut out).expect("read");
/// assert_eq!(out, "bounded streaming decode");
/// ```
pub struct ZstdStreamDecoder<R: Read> {
    /// The wrapped reader providing compressed input.
    inner: R,
    /// The bounded push decoder doing the actual work.
    stream: ZstdStream,
    /// Compressed input staging buffer.
    in_buf: Vec<u8>,
    /// Valid prefix length of `in_buf`.
    in_len: usize,
    /// Read cursor inside `in_buf`.
    in_pos: usize,
    /// Decompressed output staging buffer.
    out_buf: Vec<u8>,
    /// Valid prefix length of `out_buf`.
    out_len: usize,
    /// Read cursor inside `out_buf`.
    out_pos: usize,
    /// Whether the inner reader has reported end of file.
    src_eof: bool,
    /// Whether the push decoder has reported `StreamEnd`.
    stream_done: bool,
    /// Whether `on_finish` has already been reported.
    reported_finish: bool,
    /// Compressed bytes read from the inner reader that turned out not to be
    /// part of the Zstandard stream. Filled once, when the push decoder reports
    /// `StreamEnd`; empty before that.
    unused: Vec<u8>,
    /// Optional progress sink.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token.
    cancel: Option<CancellationToken>,
}

impl<R: Read> ZstdStreamDecoder<R> {
    /// Create a new streaming decoder wrapping `reader`.
    pub fn new(reader: R) -> Self {
        Self::build(
            reader,
            ZstdStream::new().with_max_window(UNRESTRICTED_MAX_WINDOW),
        )
    }

    /// Create a new streaming decoder with a dictionary.
    ///
    /// Dictionary-based decompression requires the same dictionary that was
    /// used during compression. The dictionary seeds the window at the start of
    /// every frame, so a multi-frame stream produced by
    /// [`crate::ZstdStreamEncoder::with_dictionary`] decodes correctly.
    ///
    /// Raw content dictionaries only: see [`ZstdStream::with_dictionary`], which
    /// refuses a *formatted* (RFC 8878 §5) dictionary by name rather than
    /// mistaking its header and entropy tables for content.
    pub fn with_dictionary(reader: R, dict: Vec<u8>) -> Self {
        Self::build(
            reader,
            ZstdStream::new()
                .with_max_window(UNRESTRICTED_MAX_WINDOW)
                .with_dictionary(dict),
        )
    }

    /// Shared constructor.
    fn build(reader: R, stream: ZstdStream) -> Self {
        Self {
            inner: reader,
            stream,
            in_buf: vec![0u8; STAGING],
            in_len: 0,
            in_pos: 0,
            out_buf: vec![0u8; STAGING],
            out_len: 0,
            out_pos: 0,
            src_eof: false,
            stream_done: false,
            reported_finish: false,
            unused: Vec::new(),
            progress: None,
            cancel: None,
        }
    }

    /// Cap the total number of decompressed bytes this decoder will produce.
    ///
    /// Enforced *while* decoding (see [`ZstdStream::with_max_output`]), so a
    /// compression bomb is rejected after at most one extra 128 KiB block
    /// rather than after it has exhausted memory. Without it the decoder is
    /// still bounded in working memory, but the total number of bytes it will
    /// hand out is whatever the stream contains.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.stream = std::mem::take(&mut self.stream).with_max_output(limit);
        self
    }

    /// Refuse frames declaring a `Window_Size` larger than `bytes`.
    ///
    /// Unrestricted by default, matching this type's historical behaviour
    /// (reference frames compressed with `zstd --long` declare 16-128 MiB
    /// windows). The window ring still only ever grows to one block plus the
    /// number of bytes actually produced.
    #[must_use]
    pub fn with_max_window(mut self, bytes: usize) -> Self {
        self.stream = std::mem::take(&mut self.stream).with_max_window(bytes);
        self
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(decompressed_bytes_so_far, None)` is called
    /// after every decode step that produces output, and `on_finish()` once the
    /// stream ends.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each pump of the decoder. If cancelled, an
    /// I/O error wrapping [`oxiarc_core::error::OxiArcError::Cancelled`] is
    /// returned.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Number of decompressed bytes produced **so far**.
    ///
    /// This is a running total, not the size of the stream: the decoder never
    /// materialises the whole output, so the final size is unknown until the
    /// stream ends.
    pub fn decompressed_size(&self) -> usize {
        usize::try_from(self.stream.total_out()).unwrap_or(usize::MAX)
    }

    /// Returns `true` when the stream has ended and every decoded byte has been
    /// handed to the caller.
    pub fn is_finished(&self) -> bool {
        self.stream_done && self.out_pos >= self.out_len
    }

    /// Compressed bytes read from the inner reader that were **not** part of
    /// the Zstandard stream — trailing garbage after a complete frame, or the
    /// bytes that follow the first frame when the stream stops early.
    ///
    /// Empty until the stream ends, and empty when it ended exactly on a frame
    /// boundary. This adapter reads ahead in 64 KiB staging blocks, so the
    /// answer covers both the bytes the push decoder buffered internally and
    /// the staging remainder behind them, in stream order. It cannot cover
    /// bytes that were never read: whatever is still inside the inner reader
    /// stays there, and `ZstdStreamDecoder::into_inner`-style recovery is not
    /// offered precisely because the boundary is only knowable here.
    pub fn unused_input(&self) -> &[u8] {
        &self.unused
    }

    /// Record the compressed bytes that were read but never used.
    ///
    /// Called exactly once, on the transition to `StreamEnd`. The push
    /// decoder's own residue comes first: `ZstdStream::decode` takes bytes out
    /// of the staging buffer in order, so anything it held back precedes what
    /// is still sitting in `in_buf`.
    fn capture_unused(&mut self) {
        let mut unused = std::mem::take(&mut self.unused);
        unused.clear();
        unused.extend_from_slice(self.stream.unused_input());
        unused.extend_from_slice(&self.in_buf[self.in_pos..self.in_len]);
        self.unused = unused;
    }

    /// Refill `in_buf` from the inner reader, retrying `Interrupted`.
    fn refill(&mut self) -> io::Result<()> {
        loop {
            match self.inner.read(&mut self.in_buf) {
                Ok(0) => {
                    self.src_eof = true;
                    self.in_pos = 0;
                    self.in_len = 0;
                    return Ok(());
                }
                Ok(n) => {
                    self.in_pos = 0;
                    self.in_len = n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // WouldBlock and everything else propagates untouched; the
                // decoder state is intact and the call can simply be retried.
                Err(e) => return Err(e),
            }
        }
    }

    /// Run one decode step, filling `out_buf`.
    fn pump(&mut self) -> io::Result<()> {
        if let Some(token) = self.cancel.as_ref() {
            token.check().map_err(io::Error::other)?;
        }
        if self.in_pos >= self.in_len && !self.src_eof {
            self.refill()?;
        }

        let flush = if self.src_eof {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = self
            .stream
            .decode(
                &self.in_buf[self.in_pos..self.in_len],
                &mut self.out_buf,
                flush,
            )
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        self.in_pos += progress.consumed;
        self.out_pos = 0;
        self.out_len = progress.produced;

        if progress.produced > 0 {
            if let Some(handle) = self.progress.as_ref() {
                handle.on_progress(self.stream.total_out(), None);
            }
        }

        match progress.status {
            ZstdStatus::StreamEnd => {
                if !self.stream_done {
                    self.capture_unused();
                }
                self.stream_done = true;
                if !self.reported_finish {
                    self.reported_finish = true;
                    if let Some(handle) = self.progress.as_ref() {
                        handle.on_finish();
                    }
                }
            }
            ZstdStatus::NeedInput | ZstdStatus::NeedOutput => {
                if progress.consumed == 0 && progress.produced == 0 && self.src_eof {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "zstd decoder made no progress at end of input",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for ZstdStreamDecoder<R> {
    /// Read decompressed data into `buf`.
    ///
    /// Pumps the push decoder only as far as needed to fill `buf`; the
    /// compressed stream is never read ahead beyond one 64 KiB staging buffer.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.out_pos < self.out_len {
                let n = buf.len().min(self.out_len - self.out_pos);
                buf[..n].copy_from_slice(&self.out_buf[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            if self.stream_done {
                return Ok(0);
            }
            self.pump()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ZstdStreamEncoder, compress_with_level};
    use std::io::Write;

    /// A reader that hands out at most `step` bytes per call and injects an
    /// `Interrupted` error before every real read.
    struct Choppy<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
        interrupt_next: bool,
    }

    impl<'a> Choppy<'a> {
        fn new(data: &'a [u8], step: usize) -> Self {
            Self {
                data,
                pos: 0,
                step,
                interrupt_next: true,
            }
        }
    }

    impl Read for Choppy<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.interrupt_next = true;
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.step).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// A reader that returns `WouldBlock` once, then serves the data.
    struct Blocking<'a> {
        data: &'a [u8],
        pos: usize,
        blocked: bool,
    }

    impl Read for Blocking<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn sample(size: usize) -> Vec<u8> {
        let pattern = b"bounded zstd stream decoder sample payload ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let take = (size - data.len()).min(pattern.len());
            data.extend_from_slice(&pattern[..take]);
        }
        data
    }

    #[test]
    fn interrupted_is_retried_and_tiny_reads_work() {
        let data = sample(200_000);
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut decoder = ZstdStreamDecoder::new(Choppy::new(&frame, 7));
        let mut out = Vec::new();
        let mut buf = [0u8; 3];
        loop {
            let n = decoder.read(&mut buf).expect("read");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, data);
        assert!(decoder.is_finished());
        assert_eq!(decoder.decompressed_size(), data.len());
    }

    #[test]
    fn would_block_propagates_and_state_survives() {
        let data = sample(4096);
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut decoder = ZstdStreamDecoder::new(Blocking {
            data: &frame,
            pos: 0,
            blocked: false,
        });
        let mut buf = [0u8; 64];
        let err = decoder.read(&mut buf).expect_err("first read must block");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        // Retrying picks up exactly where it left off.
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read_to_end");
        assert_eq!(out, data);
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_short_read() {
        let data = sample(70_000);
        let frame = compress_with_level(&data, 3).expect("compress");
        let truncated = &frame[..frame.len() - 5];
        let mut decoder = ZstdStreamDecoder::new(truncated);
        let mut out = Vec::new();
        let err = decoder.read_to_end(&mut out).expect_err("must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn empty_input_is_an_empty_stream() {
        let mut decoder = ZstdStreamDecoder::new(&[][..]);
        let mut buf = [0u8; 16];
        assert_eq!(decoder.read(&mut buf).expect("read"), 0);
        assert!(decoder.is_finished());
    }

    #[test]
    fn read_into_empty_buffer_is_a_no_op() {
        let frame = compress_with_level(b"payload", 3).expect("compress");
        let mut decoder = ZstdStreamDecoder::new(&frame[..]);
        assert_eq!(decoder.read(&mut []).expect("read"), 0);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read_to_end");
        assert_eq!(out, b"payload");
    }

    #[test]
    fn max_output_rejects_a_bomb() {
        let bomb = compress_with_level(&vec![0u8; 4 << 20], 3).expect("compress");
        let mut decoder = ZstdStreamDecoder::new(&bomb[..]).with_max_output(64 * 1024);
        let mut out = Vec::new();
        let err = decoder.read_to_end(&mut out).expect_err("must reject");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(out.len() <= 64 * 1024 + crate::MAX_BLOCK_SIZE);
    }

    #[test]
    fn multi_frame_encoder_output_round_trips() {
        let data = sample(300_000);
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 3).with_block_size(16 * 1024);
        encoder.write_all(&data).expect("write");
        let compressed = encoder.finish().expect("finish");
        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read");
        assert_eq!(out, data);
    }

    /// Trailing garbage must be reported in full, not just the four bytes the
    /// push decoder happened to buffer while sniffing the next frame magic.
    ///
    /// Regression: `unused_input` used to forward `ZstdStream::unused_input`
    /// verbatim. `ZstdStream::fill` moves exactly the four magic bytes it needs
    /// into its carry and counts them as consumed, so every garbage byte beyond
    /// the fourth stayed in this adapter's 64 KiB staging buffer and was
    /// silently dropped from the answer.
    #[test]
    fn unused_input_reports_every_trailing_byte() {
        let frame = compress_with_level(b"complete frame payload", 3).expect("compress");
        let garbage = b"NOT A FRAME, NINE PLUS BYTES OF TRAILING DATA";
        let mut stream = frame.clone();
        stream.extend_from_slice(garbage);

        // A source that serves everything in one read: every trailing byte
        // reached the staging buffer, so every trailing byte must be reported.
        // The old implementation answered with the four magic bytes alone.
        let mut decoder = ZstdStreamDecoder::new(&stream[..]);
        assert!(
            decoder.unused_input().is_empty(),
            "nothing is unused before the stream ends"
        );
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read_to_end");
        assert_eq!(out, b"complete frame payload");
        assert_eq!(
            decoder.unused_input(),
            garbage,
            "the whole trailing remainder must be reported"
        );

        // A drip-feeding source can only have the bytes it was given: the
        // answer is then a non-empty prefix of the garbage, never a lie about
        // bytes still sitting unread in the inner reader.
        for step in [7usize, 1] {
            let mut decoder = ZstdStreamDecoder::new(Choppy::new(&stream, step));
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).expect("read_to_end");
            assert_eq!(out, b"complete frame payload");
            let unused = decoder.unused_input();
            assert!(!unused.is_empty(), "step {step}: nothing reported");
            assert!(
                garbage.starts_with(unused),
                "step {step}: {unused:?} is not a prefix of the trailing bytes"
            );
        }

        // A stream that ends exactly on a frame boundary has nothing unused.
        let mut decoder = ZstdStreamDecoder::new(&frame[..]);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read_to_end");
        assert!(decoder.unused_input().is_empty());
    }

    /// A frame naming a non-zero `Dictionary_ID` cannot be decoded without that
    /// dictionary, and this adapter is strict about it (see the handoff: the
    /// derived entry points inherit `ZstdStream`'s strictness deliberately,
    /// matching the reference decoder's `dictionary_wrong`).
    #[test]
    fn dictionary_id_frame_is_refused_without_a_dictionary() {
        let mut frame = vec![0x28u8, 0xB5, 0x2F, 0xFD];
        frame.push(0x01); // Dictionary_ID_flag = 1, no FCS, not single-segment
        frame.push(0x48); // window descriptor
        frame.push(0x2A); // Dictionary_ID = 42
        let header = 1u32 | (4u32 << 3); // last block, Raw, 4 bytes
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(b"abcd");

        let mut decoder = ZstdStreamDecoder::new(&frame[..]);
        let mut out = Vec::new();
        let err = decoder.read_to_end(&mut out).expect_err("must be refused");
        assert!(
            err.to_string().contains("requires dictionary ID"),
            "unnamed refusal: {err}"
        );

        // The same frame decodes once a raw-content dictionary is supplied.
        let mut decoder =
            ZstdStreamDecoder::with_dictionary(&frame[..], b"raw dictionary content".to_vec());
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("read_to_end");
        assert_eq!(out, b"abcd");
    }

    #[test]
    fn declared_window_ceiling_is_enforced_when_requested() {
        // A 300 KB payload needs a window larger than 4 KiB.
        let data = sample(300_000);
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut decoder = ZstdStreamDecoder::new(&frame[..]).with_max_window(4096);
        let mut out = Vec::new();
        assert!(decoder.read_to_end(&mut out).is_err());
    }
}

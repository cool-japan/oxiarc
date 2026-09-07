//! Async I/O for Zstandard, built on the bounded [`ZstdStream`] push decoder.
//!
//! Unlike the read-all-then-decode async shims elsewhere in the workspace, both
//! types here are genuinely bounded: they never materialise the compressed
//! input or the decompressed output, holding one 128 KiB block carry, two
//! staging buffers and the sliding window.
//!
//! Like [`crate::ZstdStreamDecoder`], and for the same `zstd --long`
//! compatibility reason, neither type restricts a frame's *declared*
//! `Window_Size` by default, so the ring can grow to `min(the declared
//! Window_Size, the bytes actually produced)`. On untrusted input set
//! `with_max_output` (both types) or `with_max_window`
//! ([`AsyncZstdDecompressor`]) to make the bound a constant.
//!
//! # Feature flag
//!
//! This module is only available with the `async-io` feature:
//!
//! ```toml
//! [dependencies]
//! oxiarc-zstd = { version = "0.4", features = ["async-io"] }
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use oxiarc_core::async_io::AsyncDecompressor;
//! use oxiarc_zstd::async_zstd::AsyncZstdDecompressor;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let frame = oxiarc_zstd::compress_with_level(b"async zstd", 3)?;
//!     let mut decoder = AsyncZstdDecompressor::new().with_max_output(1 << 20);
//!     let mut input = std::io::Cursor::new(frame);
//!     let mut output = Vec::new();
//!     decoder.decompress_async(&mut input, &mut output).await?;
//!     assert_eq!(output, b"async zstd");
//!     Ok(())
//! }
//! ```

use crate::stream::{UNRESTRICTED_MAX_WINDOW, ZstdStatus, ZstdStream};
use oxiarc_core::async_io::AsyncDecompressor;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::FlushMode;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// Default staging buffer size (64 KiB), on both the input and output sides.
const STAGING: usize = 64 * 1024;

/// Bounded async Zstandard decompressor.
///
/// Implements [`AsyncDecompressor`] by pumping [`ZstdStream`]: compressed bytes
/// are read into a fixed staging buffer, decoded, and written straight to the
/// output sink. Neither side is ever fully materialised.
#[derive(Debug)]
pub struct AsyncZstdDecompressor {
    /// Output budget applied to every decode this decompressor runs.
    max_output: Option<u64>,
    /// Declared-window ceiling.
    max_window: usize,
    /// Optional dictionary content.
    dict: Option<Vec<u8>>,
}

impl Default for AsyncZstdDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncZstdDecompressor {
    /// Create a decompressor with no output cap and an unrestricted declared
    /// window (reference frames made with `zstd --long` declare 16-128 MiB).
    #[must_use]
    pub fn new() -> Self {
        Self {
            max_output: None,
            max_window: UNRESTRICTED_MAX_WINDOW,
            dict: None,
        }
    }

    /// Cap the total number of decompressed bytes produced.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.max_output = Some(limit);
        self
    }

    /// Refuse frames declaring a `Window_Size` larger than `bytes`.
    #[must_use]
    pub fn with_max_window(mut self, bytes: usize) -> Self {
        self.max_window = bytes;
        self
    }

    /// Decompress with a raw-content dictionary.
    #[must_use]
    pub fn with_dictionary(mut self, dict: Vec<u8>) -> Self {
        self.dict = if dict.is_empty() { None } else { Some(dict) };
        self
    }

    /// Build a freshly configured push decoder.
    fn build_stream(&self) -> ZstdStream {
        let mut stream = ZstdStream::new().with_max_window(self.max_window);
        if let Some(limit) = self.max_output {
            stream = stream.with_max_output(limit);
        }
        if let Some(dict) = self.dict.clone() {
            stream = stream.with_dictionary(dict);
        }
        stream
    }

    /// Shared body of the two trait methods.
    async fn run<R, W>(&self, input: &mut R, output: &mut W, buffer_size: usize) -> Result<usize>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let cap = buffer_size.max(1024);
        let mut stream = self.build_stream();
        let mut in_buf = vec![0u8; cap];
        let mut out_buf = vec![0u8; cap];
        let mut in_pos = 0usize;
        let mut in_len = 0usize;
        let mut src_eof = false;
        let mut total = 0usize;

        loop {
            if in_pos >= in_len && !src_eof {
                let n = input.read(&mut in_buf).await?;
                if n == 0 {
                    src_eof = true;
                } else {
                    in_pos = 0;
                    in_len = n;
                }
            }
            let flush = if src_eof {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = stream.decode(&in_buf[in_pos..in_len], &mut out_buf, flush)?;
            in_pos += progress.consumed;
            if progress.produced > 0 {
                output.write_all(&out_buf[..progress.produced]).await?;
                total += progress.produced;
            }
            match progress.status {
                ZstdStatus::StreamEnd => {
                    output.flush().await?;
                    return Ok(total);
                }
                _ => {
                    if progress.consumed == 0 && progress.produced == 0 && src_eof {
                        return Err(OxiArcError::corrupted(
                            total as u64,
                            "zstd decoder made no progress at end of input",
                        ));
                    }
                }
            }
        }
    }
}

impl AsyncDecompressor for AsyncZstdDecompressor {
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        Box::pin(self.run(input, output, STAGING))
    }

    fn decompress_async_with_buffer<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
        buffer_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        Box::pin(self.run(input, output, buffer_size))
    }
}

/// Bounded async `Read` adapter: [`crate::ZstdStreamDecoder`]'s async twin.
///
/// Implements [`tokio::io::AsyncRead`] over a compressed [`AsyncRead`] source.
/// `Poll::Pending` from the inner reader propagates unchanged (the decoder
/// state is untouched, so the wake-up simply resumes), and an inner EOF
/// switches the decoder to [`FlushMode::Finish`] so a truncated stream is an
/// error rather than a short read.
///
/// # Example
///
/// ```rust,no_run
/// use tokio::io::AsyncReadExt;
/// use oxiarc_zstd::async_zstd::AsyncZstdReader;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let frame = oxiarc_zstd::compress_with_level(b"async read", 3)?;
///     let mut reader = AsyncZstdReader::new(std::io::Cursor::new(frame));
///     let mut out = Vec::new();
///     reader.read_to_end(&mut out).await?;
///     assert_eq!(out, b"async read");
///     Ok(())
/// }
/// ```
pub struct AsyncZstdReader<R> {
    /// Compressed source.
    inner: R,
    /// The bounded push decoder.
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
    /// Whether the inner reader has reported EOF.
    src_eof: bool,
    /// Whether the push decoder has reported `StreamEnd`.
    stream_done: bool,
}

impl<R: AsyncRead + Unpin> AsyncZstdReader<R> {
    /// Wrap `inner`, decoding with an unrestricted declared-window ceiling.
    pub fn new(inner: R) -> Self {
        Self::with_stream(
            inner,
            ZstdStream::new().with_max_window(UNRESTRICTED_MAX_WINDOW),
        )
    }

    /// Wrap `inner`, decoding with a raw-content dictionary.
    pub fn with_dictionary(inner: R, dict: Vec<u8>) -> Self {
        Self::with_stream(
            inner,
            ZstdStream::new()
                .with_max_window(UNRESTRICTED_MAX_WINDOW)
                .with_dictionary(dict),
        )
    }

    /// Wrap `inner` with a caller-configured push decoder.
    pub fn with_stream(inner: R, stream: ZstdStream) -> Self {
        Self {
            inner,
            stream,
            in_buf: vec![0u8; STAGING],
            in_len: 0,
            in_pos: 0,
            out_buf: vec![0u8; STAGING],
            out_len: 0,
            out_pos: 0,
            src_eof: false,
            stream_done: false,
        }
    }

    /// Cap the total number of decompressed bytes this reader will produce.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.stream = std::mem::take(&mut self.stream).with_max_output(limit);
        self
    }

    /// Number of decompressed bytes produced so far.
    pub fn total_out(&self) -> u64 {
        self.stream.total_out()
    }

    /// Consume the adapter and return the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for AsyncZstdReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.out_pos < this.out_len {
                let n = buf.remaining().min(this.out_len - this.out_pos);
                if n == 0 {
                    return Poll::Ready(Ok(()));
                }
                buf.put_slice(&this.out_buf[this.out_pos..this.out_pos + n]);
                this.out_pos += n;
                return Poll::Ready(Ok(()));
            }
            if this.stream_done {
                return Poll::Ready(Ok(()));
            }

            if this.in_pos >= this.in_len && !this.src_eof {
                let mut read_buf = ReadBuf::new(&mut this.in_buf);
                match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            this.src_eof = true;
                        } else {
                            this.in_pos = 0;
                            this.in_len = n;
                        }
                    }
                }
            }

            let flush = if this.src_eof {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = match this.stream.decode(
                &this.in_buf[this.in_pos..this.in_len],
                &mut this.out_buf,
                flush,
            ) {
                Ok(p) => p,
                Err(e) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        e.to_string(),
                    )));
                }
            };
            this.in_pos += progress.consumed;
            this.out_pos = 0;
            this.out_len = progress.produced;
            if progress.status == ZstdStatus::StreamEnd {
                this.stream_done = true;
            } else if progress.consumed == 0 && progress.produced == 0 && this.src_eof {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "zstd decoder made no progress at end of input",
                )));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress_with_level;

    /// A source that yields `Poll::Pending` (after scheduling an immediate
    /// wake) before every real read, exercising the resume path.
    struct Stuttering {
        data: Vec<u8>,
        pos: usize,
        pend_next: bool,
        step: usize,
    }

    impl AsyncRead for Stuttering {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if this.pend_next {
                this.pend_next = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.pend_next = true;
            if this.pos >= this.data.len() {
                return Poll::Ready(Ok(()));
            }
            let n = buf
                .remaining()
                .min(this.step)
                .min(this.data.len() - this.pos);
            buf.put_slice(&this.data[this.pos..this.pos + n]);
            this.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    fn sample(size: usize) -> Vec<u8> {
        let pattern = b"async zstd sample payload with structure ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let take = (size - data.len()).min(pattern.len());
            data.extend_from_slice(&pattern[..take]);
        }
        data
    }

    #[tokio::test]
    async fn async_decompressor_round_trips() {
        let data = sample(300_000);
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut decoder = AsyncZstdDecompressor::new().with_max_output(1 << 20);
        let mut input = std::io::Cursor::new(frame);
        let mut output = Vec::new();
        let n = decoder
            .decompress_async(&mut input, &mut output)
            .await
            .expect("decompress");
        assert_eq!(n, data.len());
        assert_eq!(output, data);
    }

    #[tokio::test]
    async fn async_decompressor_honours_the_output_cap() {
        let bomb = compress_with_level(&vec![0u8; 4 << 20], 3).expect("compress");
        let mut decoder = AsyncZstdDecompressor::new().with_max_output(64 * 1024);
        let mut input = std::io::Cursor::new(bomb);
        let mut output = Vec::new();
        assert!(
            decoder
                .decompress_async(&mut input, &mut output)
                .await
                .is_err()
        );
        assert!(output.len() <= 64 * 1024 + crate::MAX_BLOCK_SIZE);
    }

    #[tokio::test]
    async fn async_reader_survives_pending_at_every_boundary() {
        let data = sample(120_000);
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut reader = AsyncZstdReader::new(Stuttering {
            data: frame,
            pos: 0,
            pend_next: true,
            step: 13,
        });
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut out)
            .await
            .expect("read_to_end");
        assert_eq!(out, data);
        assert_eq!(reader.total_out(), data.len() as u64);
    }

    #[tokio::test]
    async fn async_reader_reports_truncation() {
        let data = sample(70_000);
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut reader =
            AsyncZstdReader::new(std::io::Cursor::new(frame[..frame.len() - 6].to_vec()));
        let mut out = Vec::new();
        let err = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut out)
            .await
            .expect_err("truncation must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn async_dictionary_round_trip() {
        let dict = "shared dictionary content for async frames ".repeat(20);
        let payload = b"shared dictionary content";
        let mut encoder = crate::ZstdEncoder::new();
        encoder.set_level(3);
        encoder.set_dictionary(dict.as_bytes());
        let frame = encoder.compress(payload).expect("compress");

        let mut reader =
            AsyncZstdReader::with_dictionary(std::io::Cursor::new(frame), dict.as_bytes().to_vec());
        let mut out = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut out)
            .await
            .expect("read_to_end");
        assert_eq!(out, payload);
    }
}

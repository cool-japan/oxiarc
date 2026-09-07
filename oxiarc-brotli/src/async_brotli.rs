//! Async I/O support for Brotli compression and decompression.
//!
//! This module provides [`BrotliAsyncCompressor`] and [`BrotliAsyncDecompressor`]
//! that implement the [`oxiarc_core::async_io::AsyncCompressor`] and [`oxiarc_core::async_io::AsyncDecompressor`] traits from
//! `oxiarc-core` for the Brotli algorithm.
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-brotli = { version = "0.3.6", features = ["async-io"] }
//! ```
//!
//! # Memory Note
//!
//! [`BrotliAsyncCompressor`] reads the entire input into memory before
//! processing; it is not a bounded-memory streaming implementation.
//!
//! [`BrotliAsyncDecompressor`] **is** bounded: it drives
//! [`crate::BrotliStream`] with a 64 KiB compressed staging buffer and writes
//! each decoded chunk out as it is produced, so peak memory is `O(window)`
//! rather than `O(stream)`.
//!
//! # Example
//!
//! ```rust,no_run
//! use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
//! use oxiarc_brotli::async_brotli::{BrotliAsyncCompressor, BrotliAsyncDecompressor};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let original = b"Hello, async Brotli!";
//!
//!     let mut enc = BrotliAsyncCompressor::new(6);
//!     let mut input = tokio::io::BufReader::new(&original[..]);
//!     let mut compressed = Vec::new();
//!     enc.compress_async(&mut input, &mut compressed).await?;
//!
//!     let mut dec = BrotliAsyncDecompressor::new();
//!     let mut comp_cursor = std::io::Cursor::new(compressed);
//!     let mut output = Vec::new();
//!     dec.decompress_async(&mut comp_cursor, &mut output).await?;
//!
//!     assert_eq!(&output, original);
//!     Ok(())
//! }
//! ```

use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
use oxiarc_core::error::Result;
use oxiarc_core::traits::FlushMode;
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::compress::{BrotliParams, compress_with_params};
use crate::stream::{BrotliStatus, BrotliStream};

/// Default buffer size for async Brotli operations (64 KB).
const BROTLI_ASYNC_BUFFER_SIZE: usize = 64 * 1024;

/// Hard ceiling on the async decoder's compressed staging buffer.
///
/// Only reached when a meta-block header spans the whole buffer;
/// [`BrotliStream`] rejects headers longer than 1 MiB well before this.
const MAX_ASYNC_STAGING: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// BrotliAsyncCompressor
// ---------------------------------------------------------------------------

/// An async Brotli compressor.
///
/// Implements [`AsyncCompressor`] using a read-all → sync-compress → write-all
/// strategy.
///
/// # Memory Note
///
/// NOTE: This implementation reads the entire input into memory before
/// processing. It is not a bounded-memory streaming implementation.
///
/// # Example
///
/// ```rust,no_run
/// use oxiarc_core::async_io::AsyncCompressor;
/// use oxiarc_brotli::async_brotli::BrotliAsyncCompressor;
///
/// #[tokio::main]
/// async fn main() {
///     let mut enc = BrotliAsyncCompressor::new(6);
///     let mut input = tokio::io::BufReader::new(&b"Hello!"[..]);
///     let mut output = Vec::new();
///     enc.compress_async(&mut input, &mut output).await.expect("compress");
/// }
/// ```
pub struct BrotliAsyncCompressor {
    params: BrotliParams,
}

impl BrotliAsyncCompressor {
    /// Create a new async Brotli compressor with the given quality level (0–11).
    pub fn new(quality: u32) -> Self {
        Self {
            params: BrotliParams {
                quality,
                ..BrotliParams::default()
            },
        }
    }

    /// Create a new async Brotli compressor with full parameter control.
    pub fn with_params(params: BrotliParams) -> Self {
        Self { params }
    }
}

impl AsyncCompressor for BrotliAsyncCompressor {
    /// Compress data asynchronously.
    ///
    /// NOTE: This implementation reads the entire input into memory before
    /// processing. It is not a bounded-memory streaming implementation.
    fn compress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.compress_async_with_buffer(input, output, BROTLI_ASYNC_BUFFER_SIZE)
    }

    /// Compress data asynchronously with a custom read-buffer size.
    ///
    /// NOTE: This implementation reads the entire input into memory before
    /// processing. It is not a bounded-memory streaming implementation.
    fn compress_async_with_buffer<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
        buffer_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        let buf_size = buffer_size.max(256);
        let params = self.params.clone();
        Box::pin(async move {
            // 1. Read all input asynchronously.
            let mut read_buf = vec![0u8; buf_size];
            let mut all_input: Vec<u8> = Vec::new();
            loop {
                let n = input.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                all_input.extend_from_slice(&read_buf[..n]);
            }

            // 2. Compress synchronously in one shot.
            let compressed = compress_with_params(&all_input, &params)?;

            // 3. Write all compressed bytes asynchronously in chunks.
            let total_written = compressed.len();
            let mut offset = 0;
            while offset < compressed.len() {
                let end = (offset + buf_size).min(compressed.len());
                output.write_all(&compressed[offset..end]).await?;
                offset = end;
            }
            output.flush().await?;

            Ok(total_written)
        })
    }
}

// ---------------------------------------------------------------------------
// BrotliAsyncDecompressor
// ---------------------------------------------------------------------------

/// An async Brotli decompressor.
///
/// Implements [`AsyncDecompressor`] on top of [`crate::BrotliStream`]: a
/// bounded compressed staging buffer is refilled from the source and each
/// decoded chunk is written to the sink as it is produced. Peak memory is the
/// staging buffer plus the stream's sliding window, not the decompressed size.
///
/// A source that ends mid-stream is an error ([`std::io::ErrorKind::InvalidData`]
/// carrying the decoder's message), never a short write.
///
/// # Example
///
/// ```rust,no_run
/// use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
/// use oxiarc_brotli::async_brotli::{BrotliAsyncCompressor, BrotliAsyncDecompressor};
///
/// #[tokio::main]
/// async fn main() {
///     let mut enc = BrotliAsyncCompressor::new(6);
///     let mut enc_input = tokio::io::BufReader::new(&b"Hello!"[..]);
///     let mut compressed = Vec::new();
///     enc.compress_async(&mut enc_input, &mut compressed).await.expect("compress");
///
///     let mut dec = BrotliAsyncDecompressor::new();
///     let mut input = tokio::io::BufReader::new(&compressed[..]);
///     let mut output = Vec::new();
///     dec.decompress_async(&mut input, &mut output).await.expect("decompress");
/// }
/// ```
pub struct BrotliAsyncDecompressor {
    /// Optional output cap, forwarded to the decoder.
    max_output: Option<u64>,
    /// Optional declared-window ceiling, forwarded to the decoder.
    max_window: Option<usize>,
}

impl BrotliAsyncDecompressor {
    /// Create a new async Brotli decompressor.
    pub fn new() -> Self {
        Self {
            max_output: None,
            max_window: None,
        }
    }

    /// Refuse to produce more than `limit` bytes.
    ///
    /// Enforced per meta-block before it is decoded, so an over-budget stream
    /// fails without its expansion being produced and without the rest of the
    /// source being read.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.max_output = Some(limit);
        self
    }

    /// Refuse a stream whose declared sliding window exceeds `bytes`.
    ///
    /// Checked before the window is allocated. Defaults to
    /// [`crate::DEFAULT_MAX_WINDOW`] (16 MiB).
    #[must_use]
    pub fn with_max_window(mut self, bytes: usize) -> Self {
        self.max_window = Some(bytes);
        self
    }

    /// Build a decoder carrying this adapter's settings.
    fn build_stream(&self) -> BrotliStream {
        let mut stream = BrotliStream::new();
        if let Some(limit) = self.max_output {
            stream = stream.with_max_output(limit);
        }
        if let Some(bytes) = self.max_window {
            stream = stream.with_max_window(bytes);
        }
        stream
    }
}

impl Default for BrotliAsyncDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncDecompressor for BrotliAsyncDecompressor {
    /// Decompress data asynchronously with bounded memory.
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.decompress_async_with_buffer(input, output, BROTLI_ASYNC_BUFFER_SIZE)
    }

    /// Decompress data asynchronously with a custom staging-buffer size.
    ///
    /// `buffer_size` bounds the compressed bytes held at once (minimum 256);
    /// the decoded bytes are written to `output` as they are produced.
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
        let buf_size = buffer_size.max(256);
        let mut stream = self.build_stream();
        Box::pin(async move {
            let mut staging = vec![0u8; buf_size];
            let mut staged = 0usize;
            let mut staged_pos = 0usize;
            let mut input_done = false;
            let mut decoded = vec![0u8; buf_size.max(BROTLI_ASYNC_BUFFER_SIZE)];
            let mut total_written = 0usize;

            loop {
                let flush = if input_done {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };
                let progress = stream
                    .decode(&staging[staged_pos..staged], &mut decoded, flush)
                    .map_err(std::io::Error::from)?;
                staged_pos += progress.consumed;
                if progress.produced > 0 {
                    output.write_all(&decoded[..progress.produced]).await?;
                    total_written += progress.produced;
                }
                if progress.status == BrotliStatus::StreamEnd {
                    break;
                }
                if progress.status == BrotliStatus::NeedInput {
                    if input_done {
                        // `FlushMode::Finish` would have raised the shortfall,
                        // so an idle decoder here cannot make progress.
                        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
                    }
                    // Compact and refill the staging buffer.
                    if staged_pos > 0 {
                        staging.copy_within(staged_pos..staged, 0);
                        staged -= staged_pos;
                        staged_pos = 0;
                    }
                    if staged == staging.len() {
                        // A meta-block header spanning the whole buffer; grow
                        // so progress is possible. `BrotliStream` rejects a
                        // header longer than 1 MiB, so this ceiling never stops
                        // a valid stream.
                        if staging.len() >= MAX_ASYNC_STAGING {
                            return Err(std::io::Error::other(
                                "brotli meta-block header exceeds the staging buffer",
                            )
                            .into());
                        }
                        let grown = (staging.len() * 2).min(MAX_ASYNC_STAGING);
                        staging.resize(grown, 0);
                    }
                    let n = input.read(&mut staging[staged..]).await?;
                    if n == 0 {
                        input_done = true;
                    } else {
                        staged += n;
                    }
                } else if progress.produced == 0 {
                    return Err(std::io::Error::other("brotli decoder made no progress").into());
                }
            }
            stream.finish().map_err(std::io::Error::from)?;
            output.flush().await?;

            Ok(total_written)
        })
    }
}

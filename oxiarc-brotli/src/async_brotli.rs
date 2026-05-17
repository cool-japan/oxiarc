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
//! oxiarc-brotli = { version = "0.3", features = ["async-io"] }
//! ```
//!
//! # Memory Note
//!
//! NOTE: This implementation reads the entire input into memory before
//! processing. It is not a bounded-memory streaming implementation.
//!
//! # Example
//!
//! ```rust,ignore
//! use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
//! use oxiarc_brotli::async_brotli::{BrotliAsyncCompressor, BrotliAsyncDecompressor};
//! use tokio::io::Cursor;
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
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::compress::{BrotliParams, compress_with_params};
use crate::decompress::decompress;

/// Default buffer size for async Brotli operations (64 KB).
const BROTLI_ASYNC_BUFFER_SIZE: usize = 64 * 1024;

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
/// ```rust,ignore
/// use oxiarc_core::async_io::AsyncCompressor;
/// use oxiarc_brotli::async_brotli::BrotliAsyncCompressor;
///
/// let mut enc = BrotliAsyncCompressor::new(6);
/// let mut input = tokio::io::BufReader::new(&b"Hello!"[..]);
/// let mut output = Vec::new();
/// enc.compress_async(&mut input, &mut output).await.unwrap();
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
/// Implements [`AsyncDecompressor`] using a read-all → sync-decompress → write-all
/// strategy.
///
/// # Memory Note
///
/// NOTE: This implementation reads the entire input into memory before
/// processing. It is not a bounded-memory streaming implementation.
///
/// # Example
///
/// ```rust,ignore
/// use oxiarc_core::async_io::AsyncDecompressor;
/// use oxiarc_brotli::async_brotli::BrotliAsyncDecompressor;
///
/// let mut dec = BrotliAsyncDecompressor::new();
/// let mut input = tokio::io::BufReader::new(&compressed[..]);
/// let mut output = Vec::new();
/// dec.decompress_async(&mut input, &mut output).await.unwrap();
/// ```
pub struct BrotliAsyncDecompressor;

impl BrotliAsyncDecompressor {
    /// Create a new async Brotli decompressor.
    pub fn new() -> Self {
        Self
    }
}

impl Default for BrotliAsyncDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncDecompressor for BrotliAsyncDecompressor {
    /// Decompress data asynchronously.
    ///
    /// NOTE: This implementation reads the entire input into memory before
    /// processing. It is not a bounded-memory streaming implementation.
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

    /// Decompress data asynchronously with a custom read-buffer size.
    ///
    /// NOTE: This implementation reads the entire input into memory before
    /// processing. It is not a bounded-memory streaming implementation.
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
        Box::pin(async move {
            // 1. Read entire compressed stream asynchronously.
            let mut read_buf = vec![0u8; buf_size];
            let mut all_compressed: Vec<u8> = Vec::new();
            loop {
                let n = input.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                all_compressed.extend_from_slice(&read_buf[..n]);
            }

            // 2. Decompress synchronously in one shot.
            let decompressed = decompress(&all_compressed)?;

            // 3. Write all decompressed bytes asynchronously.
            let total_written = decompressed.len();
            output.write_all(&decompressed).await?;
            output.flush().await?;

            Ok(total_written)
        })
    }
}

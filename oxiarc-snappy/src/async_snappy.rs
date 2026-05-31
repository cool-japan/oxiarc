//! Async I/O support for Snappy frame compression and decompression.
//!
//! This module provides implementations of the [`AsyncCompressor`] and
//! [`AsyncDecompressor`] traits from `oxiarc-core` for the Snappy framed format.
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-snappy = { version = "0.3.2", features = ["async-io"] }
//! ```
//!
//! # Note
//!
//! NOTE: Reads entire input into memory before processing. Not bounded-memory streaming.
//!
//! # Example
//!
//! ```rust,ignore
//! use oxiarc_snappy::async_snappy::{AsyncSnappyCompressor, AsyncSnappyDecompressor};
//! use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
//!
//! let mut compressor = AsyncSnappyCompressor;
//! // use compressor.compress_async(&mut input, &mut output).await
//! ```

#![cfg(feature = "async-io")]

use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
use oxiarc_core::error::Result;
use std::future::Future;
use std::io::{Cursor, Read, Write};
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::frame::{FrameDecoder, FrameEncoder};

/// Default buffer size for async Snappy operations (64 KiB, matching the frame chunk size).
const SNAPPY_ASYNC_BUFFER_SIZE: usize = 64 * 1024;

/// A stateless async compressor for Snappy framed format.
///
/// Implements [`AsyncCompressor`] using [`FrameEncoder`] internally.
///
/// NOTE: Reads entire input into memory before processing. Not bounded-memory streaming.
pub struct AsyncSnappyCompressor;

/// A stateless async decompressor for Snappy framed format.
///
/// Implements [`AsyncDecompressor`] using [`FrameDecoder`] internally.
///
/// NOTE: Reads entire input into memory before processing. Not bounded-memory streaming.
pub struct AsyncSnappyDecompressor;

impl AsyncCompressor for AsyncSnappyCompressor {
    fn compress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.compress_async_with_buffer(input, output, SNAPPY_ASYNC_BUFFER_SIZE)
    }

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
        Box::pin(async move {
            // NOTE: Reads entire input into memory before processing.
            // Not bounded-memory streaming.
            let mut read_buf = vec![0u8; buf_size];
            let mut all_input: Vec<u8> = Vec::new();

            // Read entire input stream asynchronously
            loop {
                let n = input.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                all_input.extend_from_slice(&read_buf[..n]);
            }

            // Synchronously compress using FrameEncoder
            let mut compressed = Vec::new();
            {
                let mut encoder = FrameEncoder::new(&mut compressed);
                encoder
                    .write_all(&all_input)
                    .map_err(oxiarc_core::error::OxiArcError::Io)?;
                encoder
                    .finish()
                    .map_err(oxiarc_core::error::OxiArcError::Io)?;
            }

            // Write compressed bytes asynchronously in chunks
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

impl AsyncDecompressor for AsyncSnappyDecompressor {
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.decompress_async_with_buffer(input, output, SNAPPY_ASYNC_BUFFER_SIZE)
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
        let buf_size = buffer_size.max(256);
        Box::pin(async move {
            // NOTE: Reads entire input into memory before processing.
            // Not bounded-memory streaming.
            let mut read_buf = vec![0u8; buf_size];
            let mut all_compressed: Vec<u8> = Vec::new();

            // Read entire compressed stream asynchronously
            loop {
                let n = input.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                all_compressed.extend_from_slice(&read_buf[..n]);
            }

            // Synchronously decompress using FrameDecoder
            let mut decompressed = Vec::new();
            {
                let cursor = Cursor::new(&all_compressed);
                let mut decoder = FrameDecoder::new(cursor);
                decoder
                    .read_to_end(&mut decompressed)
                    .map_err(oxiarc_core::error::OxiArcError::Io)?;
            }

            let total_written = decompressed.len();
            output.write_all(&decompressed).await?;
            output.flush().await?;

            Ok(total_written)
        })
    }
}

/// Compress data from an async reader to an async writer using Snappy framed format.
///
/// NOTE: Reads entire input into memory before processing. Not bounded-memory streaming.
///
/// # Arguments
///
/// * `input` - Async reader providing uncompressed data
/// * `output` - Async writer to receive Snappy-framed compressed data
///
/// # Returns
///
/// The total number of bytes written to the output.
///
/// # Errors
///
/// Returns an error if I/O operations fail or compression encounters invalid data.
pub async fn compress_frame_async<R, W>(mut input: R, output: &mut W) -> Result<usize>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // NOTE: Reads entire input into memory before processing.
    let mut all_input = Vec::new();
    input.read_to_end(&mut all_input).await?;

    let mut compressed = Vec::new();
    {
        let mut encoder = FrameEncoder::new(&mut compressed);
        encoder
            .write_all(&all_input)
            .map_err(oxiarc_core::error::OxiArcError::Io)?;
        encoder
            .finish()
            .map_err(oxiarc_core::error::OxiArcError::Io)?;
    }

    let total = compressed.len();
    output.write_all(&compressed).await?;
    output.flush().await?;
    Ok(total)
}

/// Decompress Snappy-framed data from an async reader to an async writer.
///
/// NOTE: Reads entire input into memory before processing. Not bounded-memory streaming.
///
/// # Arguments
///
/// * `input` - Async reader providing Snappy-framed compressed data
/// * `output` - Async writer to receive decompressed data
///
/// # Returns
///
/// The total number of bytes written to the output.
///
/// # Errors
///
/// Returns an error if I/O operations fail or decompression encounters invalid data.
pub async fn decompress_frame_async<R, W>(mut input: R, output: &mut W) -> Result<usize>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // NOTE: Reads entire input into memory before processing.
    let mut all_compressed = Vec::new();
    input.read_to_end(&mut all_compressed).await?;

    let mut decompressed = Vec::new();
    {
        let cursor = Cursor::new(&all_compressed);
        let mut decoder = FrameDecoder::new(cursor);
        decoder
            .read_to_end(&mut decompressed)
            .map_err(oxiarc_core::error::OxiArcError::Io)?;
    }

    let total = decompressed.len();
    output.write_all(&decompressed).await?;
    output.flush().await?;
    Ok(total)
}

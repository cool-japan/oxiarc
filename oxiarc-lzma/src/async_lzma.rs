//! Async I/O support for LZMA2 compression and decompression.
//!
//! This module provides implementations of the [`AsyncCompressor`] and
//! [`AsyncDecompressor`] traits from `oxiarc-core` for the LZMA2 algorithm.
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-lzma = { version = "0.3.2", features = ["async-io"] }
//! ```
//!
//! # Memory Model
//!
//! NOTE: Reads entire input into memory before processing. Not bounded-memory
//! streaming. LZMA2 framing only (XZ container is out of scope).
//!
//! # Example
//!
//! ```rust,ignore
//! use oxiarc_core::async_io::AsyncCompressor;
//! use oxiarc_lzma::{Lzma2Encoder, LzmaLevel};
//!
//! let mut encoder = Lzma2Encoder::new(LzmaLevel::DEFAULT);
//! // use encoder.compress_async(&mut input, &mut output).await
//! ```

use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
use oxiarc_core::error::Result;
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::lzma2::{Lzma2Decoder, Lzma2Encoder};

/// Default buffer size for async LZMA2 operations (64KB).
const LZMA_ASYNC_BUFFER_SIZE: usize = 64 * 1024;

impl AsyncCompressor for Lzma2Encoder {
    fn compress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.compress_async_with_buffer(input, output, LZMA_ASYNC_BUFFER_SIZE)
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
            // Not bounded-memory streaming. LZMA2 framing only.
            //   1. Read all input bytes asynchronously into a single Vec
            //   2. Compress synchronously using LZMA2 in one shot
            //   3. Write all output bytes asynchronously in chunks
            let mut read_buf = vec![0u8; buf_size];
            let mut all_input: Vec<u8> = Vec::new();

            // Read entire input stream
            loop {
                let n = input.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                all_input.extend_from_slice(&read_buf[..n]);
            }

            // Compress all at once using the synchronous LZMA2 path
            let compressed = self.encode(&all_input)?;

            // Write compressed bytes to the async writer in chunks
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

impl AsyncDecompressor for Lzma2Decoder {
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.decompress_async_with_buffer(input, output, LZMA_ASYNC_BUFFER_SIZE)
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
            // Not bounded-memory streaming. LZMA2 framing only.
            let mut read_buf = vec![0u8; buf_size];
            let mut all_compressed: Vec<u8> = Vec::new();

            // Read entire compressed stream
            loop {
                let n = input.read(&mut read_buf).await?;
                if n == 0 {
                    break;
                }
                all_compressed.extend_from_slice(&read_buf[..n]);
            }

            // Decompress all at once using the synchronous LZMA2 path
            let mut cursor = std::io::Cursor::new(&all_compressed);
            let decompressed = self.decode(&mut cursor)?;

            let total_written = decompressed.len();
            output.write_all(&decompressed).await?;
            output.flush().await?;

            Ok(total_written)
        })
    }
}

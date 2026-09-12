//! Async I/O support for DEFLATE compression and decompression.
//!
//! This module provides implementations of the [`AsyncCompressor`] and
//! [`AsyncDecompressor`] traits from `oxiarc-core` for the DEFLATE algorithm.
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-deflate = { version = "0.3.6", features = ["async-io"] }
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use oxiarc_core::async_io::{AsyncCompressor, AsyncCompressorWrapper};
//! use oxiarc_deflate::Deflater;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!     let deflater = Deflater::new(6);
//!     let mut async_compressor = AsyncCompressorWrapper::new(deflater);
//!     let mut input = tokio::io::BufReader::new(&b"Hello, async DEFLATE!"[..]);
//!     let mut output = Vec::new();
//!     async_compressor
//!         .compress_async(&mut input, &mut output)
//!         .await
//!         .expect("async deflate");
//! }
//! ```

use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
use oxiarc_core::error::Result;
use oxiarc_core::traits::FlushMode;
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::deflate::Deflater;
use crate::inflate::Inflater;
use crate::stream::{InflateStatus, InflateStream};

/// Default buffer size for async DEFLATE operations (64KB).
const DEFLATE_ASYNC_BUFFER_SIZE: usize = 64 * 1024;

impl AsyncCompressor for Deflater {
    fn compress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.compress_async_with_buffer(input, output, DEFLATE_ASYNC_BUFFER_SIZE)
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
            // Both Deflater::compress and Inflater::decompress process their entire
            // input slice in one synchronous call. The async wrappers therefore:
            //   1. Read all input bytes asynchronously into a single Vec
            //   2. Compress/decompress synchronously in one shot
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

            // Compress all at once using the direct synchronous path
            let compressed = self.compress_to_vec(&all_input)?;

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

impl AsyncDecompressor for Inflater {
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.decompress_async_with_buffer(input, output, DEFLATE_ASYNC_BUFFER_SIZE)
    }

    /// Decompress a raw DEFLATE stream from an async reader into an async
    /// writer, using at most `2 * buffer_size` bytes of buffering.
    ///
    /// The stream is decoded incrementally: compressed bytes are pulled
    /// `buffer_size` at a time and decoded output is written out as soon as
    /// it is produced, so peak memory no longer scales with the compressed
    /// or the decompressed size. A stream that ends in the middle of a
    /// member is an error, never a short success.
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
            let mut stream = InflateStream::new();
            if let Some(dictionary) = self.trait_dictionary() {
                stream.set_dictionary(dictionary);
            }

            let mut in_buf = vec![0u8; buf_size];
            let mut out_buf = vec![0u8; buf_size];
            let mut in_pos = 0usize;
            let mut in_len = 0usize;
            let mut source_eof = false;
            let mut total_written = 0usize;
            let mut stalls = 0u8;

            loop {
                if in_pos >= in_len && !source_eof {
                    in_len = input.read(&mut in_buf).await?;
                    in_pos = 0;
                    if in_len == 0 {
                        source_eof = true;
                    }
                }
                // Only once the source is exhausted may running dry be
                // treated as truncation.
                let flush = if source_eof {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };
                let progress = stream.inflate(
                    in_buf.get(in_pos..in_len).unwrap_or_default(),
                    &mut out_buf,
                    flush,
                )?;
                in_pos = in_pos.saturating_add(progress.consumed);
                if progress.produced > 0 {
                    if let Some(fresh) = out_buf.get(..progress.produced) {
                        output.write_all(fresh).await?;
                    }
                    total_written = total_written.saturating_add(progress.produced);
                }
                match progress.status {
                    InflateStatus::StreamEnd => break,
                    // The source is exhausted and the decoder wants more.
                    // The call above ran with `FlushMode::Finish`, under
                    // which the core raises `UnexpectedEof` rather than
                    // asking for input, so this is unreachable — and it
                    // stays unreachable structurally rather than by
                    // argument: breaking here would be a short success on a
                    // truncated stream, which this method's contract
                    // forbids.
                    InflateStatus::NeedInput if source_eof => {
                        return Err(oxiarc_core::error::OxiArcError::unexpected_eof(1));
                    }
                    _ => {}
                }
                if progress.consumed == 0 && progress.produced == 0 {
                    stalls = stalls.saturating_add(1);
                    if stalls >= 2 {
                        return Err(oxiarc_core::error::OxiArcError::corrupted(
                            total_written as u64,
                            "decoder made no progress",
                        ));
                    }
                } else {
                    stalls = 0;
                }
            }

            output.flush().await?;
            self.mark_finished();
            Ok(total_written)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
    use std::io::Cursor;

    #[tokio::test]
    async fn test_async_deflate_roundtrip() {
        let original = b"Hello, World! This is a test of async DEFLATE compression.";
        let original_repeated = original.repeat(100);

        // Compress
        let mut deflater = Deflater::new(6);
        let mut input = Cursor::new(original_repeated.clone());
        let mut compressed = Vec::new();

        let compressed_bytes = deflater
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress failed");

        assert!(compressed_bytes > 0);
        assert!(!compressed.is_empty());

        // Decompress
        let mut inflater = Inflater::new();
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_bytes = inflater
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress failed");

        assert!(decompressed_bytes > 0);
        assert_eq!(decompressed, original_repeated);
    }

    #[tokio::test]
    async fn test_async_deflate_empty_input() {
        let mut deflater = Deflater::new(6);
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut compressed = Vec::new();

        let result = deflater.compress_async(&mut input, &mut compressed).await;

        assert!(result.is_ok());
        // Should produce at least a valid empty DEFLATE stream
        assert!(!compressed.is_empty());
    }

    #[tokio::test]
    async fn test_async_deflate_with_custom_buffer() {
        let original: Vec<u8> = (0..1024).map(|i| (i % 256) as u8).collect();

        // Compress with small buffer
        let mut deflater = Deflater::new(6);
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        deflater
            .compress_async_with_buffer(&mut input, &mut compressed, 128)
            .await
            .expect("async compress with small buffer failed");

        // Decompress with small buffer
        let mut inflater = Inflater::new();
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        inflater
            .decompress_async_with_buffer(&mut comp_cursor, &mut decompressed, 128)
            .await
            .expect("async decompress with small buffer failed");

        assert_eq!(decompressed, original);
    }

    #[tokio::test]
    async fn test_async_deflate_large_data() {
        // 1MB of patterned data (highly compressible)
        let original: Vec<u8> = (0..1024 * 1024).map(|i| (i % 64) as u8).collect();

        let mut deflater = Deflater::new(6);
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        deflater
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress large data failed");

        // Compressed should be smaller than original for patterned data
        assert!(compressed.len() < original.len());

        let mut inflater = Inflater::new();
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        inflater
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress large data failed");

        assert_eq!(decompressed, original);
    }
}

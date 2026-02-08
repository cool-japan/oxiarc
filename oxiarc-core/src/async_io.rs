//! Async I/O support for OxiArc compression/decompression.
//!
//! This module provides async versions of the compression and decompression traits,
//! allowing non-blocking I/O operations for use with async runtimes like Tokio.
//!
//! # Example
//!
//! ```rust,ignore
//! use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
//! use tokio::io::{AsyncRead, AsyncWrite};
//!
//! async fn compress_data<C, R, W>(
//!     compressor: &mut C,
//!     input: &mut R,
//!     output: &mut W,
//! ) -> oxiarc_core::Result<usize>
//! where
//!     C: AsyncCompressor,
//!     R: AsyncRead + Unpin,
//!     W: AsyncWrite + Unpin,
//! {
//!     compressor.compress_async(input, output).await
//! }
//! ```
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-core = { version = "0.2.0", features = ["async-io"] }
//! ```

use crate::error::{OxiArcError, Result};
use crate::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Default buffer size for async operations (32KB).
const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

/// A trait for async compression operations.
///
/// This trait provides an asynchronous interface for compressing data from
/// an `AsyncRead` source to an `AsyncWrite` destination.
pub trait AsyncCompressor: Send {
    /// Compress data asynchronously from input to output.
    ///
    /// # Arguments
    ///
    /// * `input` - Async reader providing uncompressed data
    /// * `output` - Async writer to receive compressed data
    ///
    /// # Returns
    ///
    /// The total number of bytes written to the output.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O operations fail or compression encounters invalid data.
    fn compress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a;

    /// Compress data with a custom buffer size.
    ///
    /// # Arguments
    ///
    /// * `input` - Async reader providing uncompressed data
    /// * `output` - Async writer to receive compressed data
    /// * `buffer_size` - Size of the internal buffer
    ///
    /// # Returns
    ///
    /// The total number of bytes written to the output.
    fn compress_async_with_buffer<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
        buffer_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a;
}

/// A trait for async decompression operations.
///
/// This trait provides an asynchronous interface for decompressing data from
/// an `AsyncRead` source to an `AsyncWrite` destination.
pub trait AsyncDecompressor: Send {
    /// Decompress data asynchronously from input to output.
    ///
    /// # Arguments
    ///
    /// * `input` - Async reader providing compressed data
    /// * `output` - Async writer to receive decompressed data
    ///
    /// # Returns
    ///
    /// The total number of bytes written to the output.
    ///
    /// # Errors
    ///
    /// Returns an error if I/O operations fail or decompression encounters invalid data.
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a;

    /// Decompress data with a custom buffer size.
    ///
    /// # Arguments
    ///
    /// * `input` - Async reader providing compressed data
    /// * `output` - Async writer to receive decompressed data
    /// * `buffer_size` - Size of the internal buffer
    ///
    /// # Returns
    ///
    /// The total number of bytes written to the output.
    fn decompress_async_with_buffer<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
        buffer_size: usize,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a;
}

/// An async wrapper around a synchronous compressor.
///
/// This wrapper adapts any type implementing the [`Compressor`] trait to work
/// with async I/O using Tokio's async read/write traits.
///
/// # Example
///
/// ```rust,ignore
/// use oxiarc_core::async_io::AsyncCompressorWrapper;
/// use oxiarc_deflate::DeflateCompressor;
///
/// let compressor = DeflateCompressor::new();
/// let mut async_compressor = AsyncCompressorWrapper::new(compressor);
/// ```
pub struct AsyncCompressorWrapper<C> {
    inner: C,
    input_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
}

impl<C: Compressor + Send> AsyncCompressorWrapper<C> {
    /// Create a new async compressor wrapper with default buffer size.
    pub fn new(compressor: C) -> Self {
        Self::with_buffer_size(compressor, DEFAULT_BUFFER_SIZE)
    }

    /// Create a new async compressor wrapper with a custom buffer size.
    pub fn with_buffer_size(compressor: C, buffer_size: usize) -> Self {
        Self {
            inner: compressor,
            input_buffer: vec![0u8; buffer_size],
            output_buffer: vec![0u8; buffer_size],
        }
    }

    /// Get a reference to the inner compressor.
    pub fn inner(&self) -> &C {
        &self.inner
    }

    /// Get a mutable reference to the inner compressor.
    pub fn inner_mut(&mut self) -> &mut C {
        &mut self.inner
    }

    /// Consume the wrapper and return the inner compressor.
    pub fn into_inner(self) -> C {
        self.inner
    }

    /// Reset the compressor to its initial state.
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

impl<C: Compressor + Send> AsyncCompressor for AsyncCompressorWrapper<C> {
    fn compress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.compress_async_with_buffer(input, output, DEFAULT_BUFFER_SIZE)
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
        Box::pin(async move {
            // Resize buffers if needed
            if self.input_buffer.len() < buffer_size {
                self.input_buffer.resize(buffer_size, 0);
            }
            if self.output_buffer.len() < buffer_size {
                self.output_buffer.resize(buffer_size, 0);
            }

            let mut total_written = 0usize;
            let mut input_data = Vec::new();
            let mut input_pos = 0;
            let mut eof_reached = false;

            loop {
                // Read more input if we need it and haven't reached EOF
                if input_pos >= input_data.len() && !eof_reached {
                    let bytes_read = input.read(&mut self.input_buffer).await?;
                    if bytes_read == 0 {
                        eof_reached = true;
                    } else {
                        input_data.extend_from_slice(&self.input_buffer[..bytes_read]);
                    }
                }

                let flush = if eof_reached && input_pos >= input_data.len() {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };

                let available_input = if input_pos < input_data.len() {
                    &input_data[input_pos..]
                } else {
                    &[]
                };

                let (consumed, produced, status) =
                    self.inner
                        .compress(available_input, &mut self.output_buffer, flush)?;

                input_pos += consumed;

                if produced > 0 {
                    output.write_all(&self.output_buffer[..produced]).await?;
                    total_written += produced;
                }

                match status {
                    CompressStatus::Done => {
                        output.flush().await?;
                        return Ok(total_written);
                    }
                    CompressStatus::NeedsInput if eof_reached && input_pos >= input_data.len() => {
                        // Final flush
                        loop {
                            let (_, produced, status) = self.inner.compress(
                                &[],
                                &mut self.output_buffer,
                                FlushMode::Finish,
                            )?;
                            if produced > 0 {
                                output.write_all(&self.output_buffer[..produced]).await?;
                                total_written += produced;
                            }
                            if status == CompressStatus::Done {
                                output.flush().await?;
                                return Ok(total_written);
                            }
                        }
                    }
                    CompressStatus::NeedsInput | CompressStatus::NeedsOutput => {
                        // Continue processing
                    }
                }
            }
        })
    }
}

/// An async wrapper around a synchronous decompressor.
///
/// This wrapper adapts any type implementing the [`Decompressor`] trait to work
/// with async I/O using Tokio's async read/write traits.
///
/// # Example
///
/// ```rust,ignore
/// use oxiarc_core::async_io::AsyncDecompressorWrapper;
/// use oxiarc_deflate::DeflateDecompressor;
///
/// let decompressor = DeflateDecompressor::new();
/// let mut async_decompressor = AsyncDecompressorWrapper::new(decompressor);
/// ```
pub struct AsyncDecompressorWrapper<D> {
    inner: D,
    input_buffer: Vec<u8>,
    output_buffer: Vec<u8>,
}

impl<D: Decompressor + Send> AsyncDecompressorWrapper<D> {
    /// Create a new async decompressor wrapper with default buffer size.
    pub fn new(decompressor: D) -> Self {
        Self::with_buffer_size(decompressor, DEFAULT_BUFFER_SIZE)
    }

    /// Create a new async decompressor wrapper with a custom buffer size.
    pub fn with_buffer_size(decompressor: D, buffer_size: usize) -> Self {
        Self {
            inner: decompressor,
            input_buffer: vec![0u8; buffer_size],
            output_buffer: vec![0u8; buffer_size],
        }
    }

    /// Get a reference to the inner decompressor.
    pub fn inner(&self) -> &D {
        &self.inner
    }

    /// Get a mutable reference to the inner decompressor.
    pub fn inner_mut(&mut self) -> &mut D {
        &mut self.inner
    }

    /// Consume the wrapper and return the inner decompressor.
    pub fn into_inner(self) -> D {
        self.inner
    }

    /// Reset the decompressor to its initial state.
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

impl<D: Decompressor + Send> AsyncDecompressor for AsyncDecompressorWrapper<D> {
    fn decompress_async<'a, R, W>(
        &'a mut self,
        input: &'a mut R,
        output: &'a mut W,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'a>>
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        self.decompress_async_with_buffer(input, output, DEFAULT_BUFFER_SIZE)
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
        Box::pin(async move {
            // Resize buffers if needed
            if self.input_buffer.len() < buffer_size {
                self.input_buffer.resize(buffer_size, 0);
            }
            if self.output_buffer.len() < buffer_size {
                self.output_buffer.resize(buffer_size, 0);
            }

            let mut total_written = 0usize;
            let mut input_data = Vec::new();
            let mut input_pos = 0;
            let mut eof_reached = false;

            loop {
                // Read more input if we need it and haven't reached EOF
                if input_pos >= input_data.len() && !eof_reached {
                    let bytes_read = input.read(&mut self.input_buffer).await?;
                    if bytes_read == 0 {
                        eof_reached = true;
                    } else {
                        input_data.extend_from_slice(&self.input_buffer[..bytes_read]);
                    }
                }

                let available_input = if input_pos < input_data.len() {
                    &input_data[input_pos..]
                } else {
                    &[]
                };

                let (consumed, produced, status) = self
                    .inner
                    .decompress(available_input, &mut self.output_buffer)?;

                input_pos += consumed;

                if produced > 0 {
                    output.write_all(&self.output_buffer[..produced]).await?;
                    total_written += produced;
                }

                match status {
                    DecompressStatus::Done => {
                        output.flush().await?;
                        return Ok(total_written);
                    }
                    DecompressStatus::NeedsInput
                        if eof_reached && input_pos >= input_data.len() =>
                    {
                        // No more input available
                        if self.inner.is_finished() {
                            output.flush().await?;
                            return Ok(total_written);
                        }
                        return Err(OxiArcError::unexpected_eof(1));
                    }
                    DecompressStatus::NeedsInput
                    | DecompressStatus::NeedsOutput
                    | DecompressStatus::BlockEnd => {
                        // Continue processing
                    }
                }
            }
        })
    }
}

/// A channel-based async compressor for streaming compression.
///
/// This allows feeding data through a channel for streaming compression
/// scenarios where the input arrives asynchronously.
pub struct StreamingAsyncCompressor<C> {
    wrapper: AsyncCompressorWrapper<C>,
}

impl<C: Compressor + Send> StreamingAsyncCompressor<C> {
    /// Create a new streaming async compressor.
    pub fn new(compressor: C) -> Self {
        Self {
            wrapper: AsyncCompressorWrapper::new(compressor),
        }
    }

    /// Create a new streaming async compressor with a custom buffer size.
    pub fn with_buffer_size(compressor: C, buffer_size: usize) -> Self {
        Self {
            wrapper: AsyncCompressorWrapper::with_buffer_size(compressor, buffer_size),
        }
    }

    /// Compress a chunk of data.
    ///
    /// # Arguments
    ///
    /// * `data` - Data to compress
    /// * `is_final` - Whether this is the last chunk
    ///
    /// # Returns
    ///
    /// Compressed data bytes.
    pub fn compress_chunk(&mut self, data: &[u8], is_final: bool) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut pos = 0;
        let mut buffer = vec![0u8; DEFAULT_BUFFER_SIZE];

        loop {
            let flush = if is_final && pos >= data.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };

            let input_slice = if pos < data.len() { &data[pos..] } else { &[] };

            let (consumed, produced, status) =
                self.wrapper
                    .inner
                    .compress(input_slice, &mut buffer, flush)?;

            pos += consumed;
            output.extend_from_slice(&buffer[..produced]);

            match status {
                CompressStatus::Done => return Ok(output),
                CompressStatus::NeedsInput if pos >= data.len() && is_final => loop {
                    let (_, produced, status) =
                        self.wrapper
                            .inner
                            .compress(&[], &mut buffer, FlushMode::Finish)?;
                    output.extend_from_slice(&buffer[..produced]);
                    if status == CompressStatus::Done {
                        return Ok(output);
                    }
                },
                CompressStatus::NeedsInput if pos >= data.len() => return Ok(output),
                _ => continue,
            }
        }
    }

    /// Reset the compressor for reuse.
    pub fn reset(&mut self) {
        self.wrapper.reset();
    }
}

/// A channel-based async decompressor for streaming decompression.
///
/// This allows feeding data through a channel for streaming decompression
/// scenarios where the input arrives asynchronously.
pub struct StreamingAsyncDecompressor<D> {
    wrapper: AsyncDecompressorWrapper<D>,
    pending_input: Vec<u8>,
}

impl<D: Decompressor + Send> StreamingAsyncDecompressor<D> {
    /// Create a new streaming async decompressor.
    pub fn new(decompressor: D) -> Self {
        Self {
            wrapper: AsyncDecompressorWrapper::new(decompressor),
            pending_input: Vec::new(),
        }
    }

    /// Create a new streaming async decompressor with a custom buffer size.
    pub fn with_buffer_size(decompressor: D, buffer_size: usize) -> Self {
        Self {
            wrapper: AsyncDecompressorWrapper::with_buffer_size(decompressor, buffer_size),
            pending_input: Vec::new(),
        }
    }

    /// Decompress a chunk of data.
    ///
    /// # Arguments
    ///
    /// * `data` - Compressed data chunk
    ///
    /// # Returns
    ///
    /// Decompressed data bytes, or empty if more input is needed.
    pub fn decompress_chunk(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        self.pending_input.extend_from_slice(data);

        let mut output = Vec::new();
        let mut buffer = vec![0u8; DEFAULT_BUFFER_SIZE];
        let mut pos = 0;

        loop {
            if pos >= self.pending_input.len() {
                break;
            }

            let (consumed, produced, status) = self
                .wrapper
                .inner
                .decompress(&self.pending_input[pos..], &mut buffer)?;

            pos += consumed;
            output.extend_from_slice(&buffer[..produced]);

            match status {
                DecompressStatus::Done => {
                    // Remove consumed data
                    self.pending_input.drain(..pos);
                    return Ok(output);
                }
                DecompressStatus::NeedsInput => {
                    // Need more input, keep pending data
                    self.pending_input.drain(..pos);
                    return Ok(output);
                }
                DecompressStatus::NeedsOutput | DecompressStatus::BlockEnd => {
                    // Continue processing
                }
            }
        }

        self.pending_input.drain(..pos);
        Ok(output)
    }

    /// Check if the decompressor has finished.
    pub fn is_finished(&self) -> bool {
        self.wrapper.inner.is_finished()
    }

    /// Reset the decompressor for reuse.
    pub fn reset(&mut self) {
        self.wrapper.reset();
        self.pending_input.clear();
    }
}

/// Compress data concurrently from multiple sources.
///
/// This function takes multiple async readers and compresses them concurrently,
/// writing the results to corresponding async writers.
///
/// # Arguments
///
/// * `operations` - A vector of tuples containing (compressor, input, output)
///
/// # Returns
///
/// A vector of results, one for each operation.
pub async fn compress_concurrent<C, R, W>(
    operations: Vec<(AsyncCompressorWrapper<C>, R, W)>,
) -> Vec<Result<usize>>
where
    C: Compressor + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use tokio::task::JoinSet;

    let mut join_set = JoinSet::new();

    for (mut compressor, mut input, mut output) in operations {
        join_set.spawn(async move { compressor.compress_async(&mut input, &mut output).await });
    }

    let mut results = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(inner_result) => results.push(inner_result),
            Err(e) => results.push(Err(OxiArcError::Io(std::io::Error::other(e.to_string())))),
        }
    }

    results
}

/// Decompress data concurrently from multiple sources.
///
/// This function takes multiple async readers and decompresses them concurrently,
/// writing the results to corresponding async writers.
///
/// # Arguments
///
/// * `operations` - A vector of tuples containing (decompressor, input, output)
///
/// # Returns
///
/// A vector of results, one for each operation.
pub async fn decompress_concurrent<D, R, W>(
    operations: Vec<(AsyncDecompressorWrapper<D>, R, W)>,
) -> Vec<Result<usize>>
where
    D: Decompressor + Send + 'static,
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use tokio::task::JoinSet;

    let mut join_set = JoinSet::new();

    for (mut decompressor, mut input, mut output) in operations {
        join_set.spawn(async move { decompressor.decompress_async(&mut input, &mut output).await });
    }

    let mut results = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(inner_result) => results.push(inner_result),
            Err(e) => results.push(Err(OxiArcError::Io(std::io::Error::other(e.to_string())))),
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    /// A simple passthrough compressor for testing.
    /// It stores data with length-prefixed chunks and a simple header/footer.
    /// Format: [0xAA header] [chunk_len: u16] [chunk_data...] ... [0x00 0x00] [0xBB footer]
    #[derive(Default)]
    struct TestCompressor {
        finished: bool,
        header_written: bool,
    }

    impl TestCompressor {
        fn new() -> Self {
            Self::default()
        }
    }

    impl Compressor for TestCompressor {
        fn compress(
            &mut self,
            input: &[u8],
            output: &mut [u8],
            flush: FlushMode,
        ) -> Result<(usize, usize, CompressStatus)> {
            let mut output_pos = 0;

            // Write a simple header
            if !self.header_written && !output.is_empty() {
                output[0] = 0xAA; // Header marker
                output_pos = 1;
                self.header_written = true;
            }

            // We need at least 2 bytes for length + some data
            if output.len().saturating_sub(output_pos) < 3 {
                return Ok((0, output_pos, CompressStatus::NeedsOutput));
            }

            // Calculate how much data we can write (max 65535 per chunk)
            let available_output = output.len().saturating_sub(output_pos + 3); // 2 for length, 1 for potential footer
            let chunk_size = input.len().min(available_output).min(65535);

            if chunk_size > 0 {
                // Write length-prefixed chunk
                let len_bytes = (chunk_size as u16).to_le_bytes();
                output[output_pos] = len_bytes[0];
                output[output_pos + 1] = len_bytes[1];
                output_pos += 2;

                // Copy data (with simple XOR to simulate "compression")
                for i in 0..chunk_size {
                    output[output_pos + i] = input[i].wrapping_add(1);
                }
                output_pos += chunk_size;
            }

            let consumed = chunk_size;

            let status = if flush == FlushMode::Finish && consumed >= input.len() {
                // Write empty chunk marker (length = 0) and footer
                if output_pos + 3 <= output.len() {
                    output[output_pos] = 0x00;
                    output[output_pos + 1] = 0x00;
                    output[output_pos + 2] = 0xBB; // Footer marker
                    output_pos += 3;
                    self.finished = true;
                    CompressStatus::Done
                } else {
                    CompressStatus::NeedsOutput
                }
            } else if consumed < input.len() {
                CompressStatus::NeedsOutput
            } else {
                CompressStatus::NeedsInput
            };

            Ok((consumed, output_pos, status))
        }

        fn reset(&mut self) {
            self.finished = false;
            self.header_written = false;
        }

        fn is_finished(&self) -> bool {
            self.finished
        }
    }

    /// A simple passthrough decompressor for testing.
    /// Format: [0xAA header] [chunk_len: u16] [chunk_data...] ... [0x00 0x00] [0xBB footer]
    #[derive(Default)]
    struct TestDecompressor {
        finished: bool,
        header_read: bool,
        /// First byte of chunk length when split across reads
        pending_len_byte: Option<u8>,
        chunk_bytes_remaining: usize,
        waiting_for_footer: bool,
    }

    impl TestDecompressor {
        fn new() -> Self {
            Self::default()
        }
    }

    impl Decompressor for TestDecompressor {
        fn decompress(
            &mut self,
            input: &[u8],
            output: &mut [u8],
        ) -> Result<(usize, usize, DecompressStatus)> {
            if input.is_empty() {
                return Ok((0, 0, DecompressStatus::NeedsInput));
            }

            let mut input_pos = 0;
            let mut output_pos = 0;

            // Read header
            if !self.header_read {
                if input[0] != 0xAA {
                    return Err(OxiArcError::invalid_header("Missing test header marker"));
                }
                input_pos = 1;
                self.header_read = true;
            }

            // Check if we're waiting for the footer byte from a previous call
            if self.waiting_for_footer {
                if input_pos >= input.len() {
                    return Ok((input_pos, output_pos, DecompressStatus::NeedsInput));
                }
                if input[input_pos] == 0xBB {
                    self.finished = true;
                    self.waiting_for_footer = false;
                    return Ok((input_pos + 1, output_pos, DecompressStatus::Done));
                }
                return Err(OxiArcError::invalid_header("Missing test footer marker"));
            }

            loop {
                // If we're in the middle of reading a chunk, continue
                if self.chunk_bytes_remaining > 0 {
                    let to_read = self
                        .chunk_bytes_remaining
                        .min(input.len() - input_pos)
                        .min(output.len() - output_pos);
                    if to_read == 0 {
                        if input_pos >= input.len() {
                            return Ok((input_pos, output_pos, DecompressStatus::NeedsInput));
                        } else {
                            return Ok((input_pos, output_pos, DecompressStatus::NeedsOutput));
                        }
                    }

                    for i in 0..to_read {
                        output[output_pos + i] = input[input_pos + i].wrapping_sub(1);
                    }
                    input_pos += to_read;
                    output_pos += to_read;
                    self.chunk_bytes_remaining -= to_read;
                    continue;
                }

                // Read next chunk length (may be partial from previous call)
                let chunk_len = if let Some(first_byte) = self.pending_len_byte.take() {
                    // We have the first byte from a previous call, read second byte
                    if input_pos >= input.len() {
                        self.pending_len_byte = Some(first_byte);
                        return Ok((input_pos, output_pos, DecompressStatus::NeedsInput));
                    }
                    let second_byte = input[input_pos];
                    input_pos += 1;
                    u16::from_le_bytes([first_byte, second_byte]) as usize
                } else {
                    // Read both bytes of chunk length
                    if input_pos >= input.len() {
                        return Ok((input_pos, output_pos, DecompressStatus::NeedsInput));
                    }
                    if input_pos + 1 >= input.len() {
                        // Only one byte available, save it for next call
                        self.pending_len_byte = Some(input[input_pos]);
                        return Ok((input_pos + 1, output_pos, DecompressStatus::NeedsInput));
                    }
                    let len = u16::from_le_bytes([input[input_pos], input[input_pos + 1]]) as usize;
                    input_pos += 2;
                    len
                };

                if chunk_len == 0 {
                    // Empty chunk marker - expect footer
                    if input_pos >= input.len() {
                        // Footer byte is not in this buffer, mark state
                        self.waiting_for_footer = true;
                        return Ok((input_pos, output_pos, DecompressStatus::NeedsInput));
                    }
                    if input[input_pos] == 0xBB {
                        self.finished = true;
                        return Ok((input_pos + 1, output_pos, DecompressStatus::Done));
                    }
                    return Err(OxiArcError::invalid_header("Missing test footer marker"));
                }

                self.chunk_bytes_remaining = chunk_len;
            }
        }

        fn reset(&mut self) {
            self.finished = false;
            self.header_read = false;
            self.pending_len_byte = None;
            self.chunk_bytes_remaining = 0;
            self.waiting_for_footer = false;
        }

        fn is_finished(&self) -> bool {
            self.finished
        }
    }

    #[tokio::test]
    async fn test_async_compression_roundtrip() {
        let original_data = b"Hello, World! This is a test of async compression.";

        // Compress
        let mut compressor = AsyncCompressorWrapper::new(TestCompressor::new());
        let mut input = Cursor::new(original_data.to_vec());
        let mut compressed = Vec::new();

        let compressed_bytes = compressor.compress_async(&mut input, &mut compressed).await;

        assert!(compressed_bytes.is_ok());
        assert!(!compressed.is_empty());

        // Decompress
        let mut decompressor = AsyncDecompressorWrapper::new(TestDecompressor::new());
        let mut compressed_input = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_bytes = decompressor
            .decompress_async(&mut compressed_input, &mut decompressed)
            .await;

        assert!(decompressed_bytes.is_ok());
        assert_eq!(decompressed, original_data);
    }

    #[tokio::test]
    async fn test_async_compression_with_custom_buffer() {
        let original_data = b"Testing with custom buffer size.";

        // Compress with small buffer
        let mut compressor = AsyncCompressorWrapper::with_buffer_size(TestCompressor::new(), 8);
        let mut input = Cursor::new(original_data.to_vec());
        let mut compressed = Vec::new();

        let result = compressor
            .compress_async_with_buffer(&mut input, &mut compressed, 8)
            .await;

        assert!(result.is_ok());

        // Decompress with small buffer
        let mut decompressor =
            AsyncDecompressorWrapper::with_buffer_size(TestDecompressor::new(), 8);
        let mut compressed_input = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let result = decompressor
            .decompress_async_with_buffer(&mut compressed_input, &mut decompressed, 8)
            .await;

        assert!(result.is_ok());
        assert_eq!(decompressed, original_data);
    }

    #[tokio::test]
    async fn test_concurrent_compression() {
        let data1 = b"First piece of data to compress.".to_vec();
        let data2 = b"Second piece of data to compress.".to_vec();
        let data3 = b"Third piece of data to compress.".to_vec();

        let operations = vec![
            (
                AsyncCompressorWrapper::new(TestCompressor::new()),
                Cursor::new(data1.clone()),
                Vec::new(),
            ),
            (
                AsyncCompressorWrapper::new(TestCompressor::new()),
                Cursor::new(data2.clone()),
                Vec::new(),
            ),
            (
                AsyncCompressorWrapper::new(TestCompressor::new()),
                Cursor::new(data3.clone()),
                Vec::new(),
            ),
        ];

        let results = compress_concurrent(operations).await;

        assert_eq!(results.len(), 3);
        for result in &results {
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_concurrent_decompression() {
        // First, compress the data
        let data1 = b"First piece of data.".to_vec();
        let data2 = b"Second piece of data.".to_vec();

        let mut compressed1 = Vec::new();
        let mut compressed2 = Vec::new();

        {
            let mut compressor1 = AsyncCompressorWrapper::new(TestCompressor::new());
            let mut compressor2 = AsyncCompressorWrapper::new(TestCompressor::new());

            let _ = compressor1
                .compress_async(&mut Cursor::new(data1.clone()), &mut compressed1)
                .await;
            let _ = compressor2
                .compress_async(&mut Cursor::new(data2.clone()), &mut compressed2)
                .await;
        }

        // Now decompress concurrently
        let operations = vec![
            (
                AsyncDecompressorWrapper::new(TestDecompressor::new()),
                Cursor::new(compressed1),
                Vec::new(),
            ),
            (
                AsyncDecompressorWrapper::new(TestDecompressor::new()),
                Cursor::new(compressed2),
                Vec::new(),
            ),
        ];

        let results = decompress_concurrent(operations).await;

        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_streaming_compressor() {
        let mut compressor = StreamingAsyncCompressor::new(TestCompressor::new());

        let chunk1 = compressor.compress_chunk(b"First ", false);
        assert!(chunk1.is_ok());

        let chunk2 = compressor.compress_chunk(b"chunk.", true);
        assert!(chunk2.is_ok());
    }

    #[tokio::test]
    async fn test_streaming_decompressor() {
        // First compress some data
        let mut compressor = StreamingAsyncCompressor::new(TestCompressor::new());
        let compressed = compressor.compress_chunk(b"Test data", true);
        assert!(compressed.is_ok());
        let compressed_data = compressed.expect("compression failed");

        // Now decompress in streaming fashion
        let mut decompressor = StreamingAsyncDecompressor::new(TestDecompressor::new());
        let decompressed = decompressor.decompress_chunk(&compressed_data);

        assert!(decompressed.is_ok());
        assert_eq!(decompressed.expect("decompression failed"), b"Test data");
        assert!(decompressor.is_finished());
    }

    #[tokio::test]
    async fn test_compressor_reset() {
        let mut compressor = AsyncCompressorWrapper::new(TestCompressor::new());

        // First compression
        let mut input = Cursor::new(b"First".to_vec());
        let mut output = Vec::new();
        let _ = compressor.compress_async(&mut input, &mut output).await;

        // Reset
        compressor.reset();

        // Second compression
        let mut input = Cursor::new(b"Second".to_vec());
        let mut output = Vec::new();
        let result = compressor.compress_async(&mut input, &mut output).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_decompressor_reset() {
        let mut compressor = AsyncCompressorWrapper::new(TestCompressor::new());
        let mut decompressor = AsyncDecompressorWrapper::new(TestDecompressor::new());

        // First roundtrip
        let mut input = Cursor::new(b"First".to_vec());
        let mut compressed = Vec::new();
        let _ = compressor.compress_async(&mut input, &mut compressed).await;

        let mut compressed_input = Cursor::new(compressed.clone());
        let mut decompressed = Vec::new();
        let _ = decompressor
            .decompress_async(&mut compressed_input, &mut decompressed)
            .await;

        // Reset
        compressor.reset();
        decompressor.reset();

        // Second roundtrip
        let mut input = Cursor::new(b"Second".to_vec());
        let mut compressed = Vec::new();
        let _ = compressor.compress_async(&mut input, &mut compressed).await;

        let mut compressed_input = Cursor::new(compressed);
        let mut decompressed = Vec::new();
        let result = decompressor
            .decompress_async(&mut compressed_input, &mut decompressed)
            .await;

        assert!(result.is_ok());
        assert_eq!(decompressed, b"Second");
    }

    #[tokio::test]
    async fn test_async_with_buf_reader() {
        let original_data = b"Testing with BufReader wrapper.";

        // Compress using BufReader
        let mut compressor = AsyncCompressorWrapper::new(TestCompressor::new());
        let input_cursor = Cursor::new(original_data.to_vec());
        let mut input = BufReader::new(input_cursor);
        let mut compressed = Vec::new();

        let result = compressor.compress_async(&mut input, &mut compressed).await;
        assert!(result.is_ok());

        // Decompress
        let mut decompressor = AsyncDecompressorWrapper::new(TestDecompressor::new());
        let compressed_cursor = Cursor::new(compressed);
        let mut compressed_input = BufReader::new(compressed_cursor);
        let mut decompressed = Vec::new();

        let result = decompressor
            .decompress_async(&mut compressed_input, &mut decompressed)
            .await;

        assert!(result.is_ok());
        assert_eq!(decompressed, original_data);
    }

    #[tokio::test]
    async fn test_large_data_compression() {
        // Create a large buffer (1MB)
        let size = 1024 * 1024;
        let original_data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();

        // Compress
        let mut compressor = AsyncCompressorWrapper::new(TestCompressor::new());
        let mut input = Cursor::new(original_data.clone());
        let mut compressed = Vec::new();

        let compressed_bytes = compressor.compress_async(&mut input, &mut compressed).await;

        assert!(compressed_bytes.is_ok());

        // Decompress
        let mut decompressor = AsyncDecompressorWrapper::new(TestDecompressor::new());
        let mut compressed_input = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_bytes = decompressor
            .decompress_async(&mut compressed_input, &mut decompressed)
            .await;

        assert!(decompressed_bytes.is_ok());
        assert_eq!(decompressed, original_data);
    }

    #[tokio::test]
    async fn test_empty_input() {
        let mut compressor = AsyncCompressorWrapper::new(TestCompressor::new());
        let mut input = Cursor::new(Vec::new());
        let mut compressed = Vec::new();

        let result = compressor.compress_async(&mut input, &mut compressed).await;

        assert!(result.is_ok());
        // Should have at least header and footer
        assert!(compressed.len() >= 2);
    }

    #[tokio::test]
    async fn test_inner_access() {
        let compressor = AsyncCompressorWrapper::new(TestCompressor::new());

        // Test inner() access
        assert!(!compressor.inner().is_finished());

        // Test into_inner()
        let inner = compressor.into_inner();
        assert!(!inner.is_finished());
    }
}

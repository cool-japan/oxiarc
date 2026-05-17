//! Streaming compression and decompression for Brotli.
//!
//! Provides `Write`-based streaming compression and `Read`-based
//! streaming decompression, suitable for processing large data
//! that doesn't fit in memory.
//!
//! ## Progress and Cancellation
//!
//! Both `BrotliCompressor` and `BrotliDecompressor` support optional
//! progress reporting and cooperative cancellation via `oxiarc-core` primitives:
//!
//! ```rust,no_run
//! use std::io::Write;
//! use oxiarc_brotli::streaming::BrotliCompressor;
//! use oxiarc_brotli::compress::BrotliParams;
//! use oxiarc_core::{CancellationToken, noop_progress};
//!
//! let token = CancellationToken::new();
//! let mut output = Vec::new();
//! let params = BrotliParams::default();
//! let mut compressor = BrotliCompressor::new(&mut output, params)
//!     .with_progress(noop_progress())
//!     .with_cancel(token.clone());
//! compressor.write_all(b"Hello, Brotli!").unwrap();
//! let _ = compressor.finish();
//! ```

use std::io::{self, Read, Write};

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::compress::BrotliParams;
use crate::decompress::decompress_with_hooks;
use crate::error::BrotliError;
use crate::pool::BrotliPool;

/// Default buffer size for streaming operations (256KB).
const DEFAULT_BUF_SIZE: usize = 256 * 1024;

/// A streaming Brotli compressor that implements `Write`.
///
/// Data written to this compressor is buffered and compressed
/// in blocks. Call `finish()` to flush all remaining data and
/// write the final Brotli stream.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`].
///
/// # Example
///
/// ```rust,no_run
/// use std::io::Write;
/// use oxiarc_brotli::streaming::BrotliCompressor;
/// use oxiarc_brotli::compress::BrotliParams;
///
/// let mut output = Vec::new();
/// let params = BrotliParams::default();
/// let mut compressor = BrotliCompressor::new(&mut output, params);
/// compressor.write_all(b"Hello, Brotli!").unwrap();
/// let output = compressor.finish().unwrap();
/// ```
pub struct BrotliCompressor<W: Write> {
    /// Inner writer that receives compressed data.
    inner: Option<W>,
    /// Compression parameters.
    params: BrotliParams,
    /// Input buffer.
    buffer: Vec<u8>,
    /// Whether any data has been written (used for multi-block streams).
    has_written: bool,
    /// Optional progress sink; receives `on_progress` once per `finish()`.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token; checked at the start of `finish()`.
    cancel: Option<CancellationToken>,
    /// Cumulative compressed bytes emitted so far.
    bytes_out: u64,
    /// Optional buffer pool for per-encode allocations.
    pool: Option<BrotliPool>,
}

impl<W: Write> BrotliCompressor<W> {
    /// Create a new streaming Brotli compressor.
    pub fn new(inner: W, params: BrotliParams) -> Self {
        BrotliCompressor {
            inner: Some(inner),
            params,
            buffer: Vec::with_capacity(DEFAULT_BUF_SIZE),
            has_written: false,
            progress: None,
            cancel: None,
            bytes_out: 0,
            pool: None,
        }
    }

    /// Create a new streaming compressor with a custom buffer size.
    pub fn with_buffer_size(inner: W, params: BrotliParams, buf_size: usize) -> Self {
        let actual_size = buf_size.max(1024); // Minimum 1KB.
        BrotliCompressor {
            inner: Some(inner),
            params,
            buffer: Vec::with_capacity(actual_size),
            has_written: false,
            progress: None,
            cancel: None,
            bytes_out: 0,
            pool: None,
        }
    }

    /// Attach a buffer pool to amortise per-encode allocations.
    ///
    /// The pool is cloned internally (cheap `Arc` clone) so the caller can
    /// reuse the same pool across multiple compressors without lifetime
    /// constraints.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::io::Write;
    /// use oxiarc_brotli::streaming::BrotliCompressor;
    /// use oxiarc_brotli::compress::BrotliParams;
    /// use oxiarc_brotli::pool::BrotliPool;
    ///
    /// let pool = BrotliPool::new();
    /// let mut output = Vec::new();
    /// let params = BrotliParams::default();
    /// let mut compressor = BrotliCompressor::new(&mut output, params)
    ///     .with_pool(&pool);
    /// compressor.write_all(b"Hello, pooled Brotli!").unwrap();
    /// let _ = compressor.finish();
    /// ```
    pub fn with_pool(mut self, pool: &BrotliPool) -> Self {
        self.pool = Some(pool.clone());
        self
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes_out, None)` is called after each
    /// compressed block is written to the inner writer.  Since the
    /// compressor buffers all input until `finish()`, progress fires
    /// once per `finish()` call.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of `finish()`. If it has been
    /// cancelled, `finish()` returns an I/O error with the message
    /// `"operation cancelled"`.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Finish compression and return the inner writer.
    ///
    /// This flushes all buffered data, writes the final Brotli
    /// stream to the inner writer, and returns it.
    pub fn finish(mut self) -> io::Result<W> {
        self.do_finish()?;
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("compressor already finished"))
    }

    /// Internal finish implementation.
    fn do_finish(&mut self) -> io::Result<()> {
        // Compress all buffered data, threading progress/cancel hooks into
        // the per-meta-block loop inside compress_with_hooks(_pooled).
        let all_data = std::mem::take(&mut self.buffer);

        let compressed = crate::compress::compress_with_hooks_pooled(
            &all_data,
            &self.params,
            self.progress.as_ref(),
            self.cancel.as_ref(),
            self.pool.as_ref(),
        )
        .map_err(|e| io::Error::other(e.to_string()))?;

        let compressed_len = compressed.len() as u64;

        if let Some(ref mut writer) = self.inner {
            writer.write_all(&compressed)?;
            writer.flush()?;
        }

        // Update cumulative bytes_out so callers can inspect final total.
        self.bytes_out += compressed_len;

        Ok(())
    }
}

impl<W: Write> Write for BrotliCompressor<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        self.has_written = true;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // For streaming, we don't compress on flush -- only on finish.
        // This allows better compression by having more context.
        Ok(())
    }
}

impl<W: Write> Drop for BrotliCompressor<W> {
    fn drop(&mut self) {
        // Best-effort finish on drop.
        if self.inner.is_some() && self.has_written {
            let _ = self.do_finish();
        }
    }
}

/// A streaming Brotli decompressor that implements `Read`.
///
/// Reads compressed data from the inner reader and produces
/// decompressed output.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`].
///
/// # Example
///
/// ```rust,no_run
/// use std::io::Read;
/// use oxiarc_brotli::streaming::BrotliDecompressor;
///
/// let compressed_data: Vec<u8> = vec![]; // ... compressed data ...
/// let mut decompressor = BrotliDecompressor::new(&compressed_data[..]);
/// let mut output = Vec::new();
/// decompressor.read_to_end(&mut output).unwrap();
/// ```
pub struct BrotliDecompressor<R: Read> {
    /// Inner reader providing compressed data.
    inner: R,
    /// Decompressed output buffer.
    output_buf: Vec<u8>,
    /// Current read position in the output buffer.
    output_pos: usize,
    /// Whether decompression is complete.
    finished: bool,
    /// Optional progress sink; receives `on_progress` after decompression.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token; checked before decompression starts.
    cancel: Option<CancellationToken>,
}

impl<R: Read> BrotliDecompressor<R> {
    /// Create a new streaming Brotli decompressor.
    pub fn new(inner: R) -> Self {
        BrotliDecompressor {
            inner,
            output_buf: Vec::new(),
            output_pos: 0,
            finished: false,
            progress: None,
            cancel: None,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes_in_consumed, Some(bytes_in_consumed))` is
    /// called once after the entire compressed input has been decompressed.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before decompression begins. If it has been
    /// cancelled, reading returns an I/O error with the message
    /// `"operation cancelled"`.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Read all compressed input and decompress it, threading hooks per meta-block.
    fn decompress_all(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }

        // Read all input data.
        let mut compressed = Vec::new();
        self.inner.read_to_end(&mut compressed)?;

        if compressed.is_empty() {
            self.finished = true;
            return Ok(());
        }

        // Decompress with per-meta-block progress and cancellation hooks.
        self.output_buf =
            decompress_with_hooks(&compressed, self.progress.as_ref(), self.cancel.as_ref())
                .map_err(|e| io::Error::other(e.to_string()))?;
        self.output_pos = 0;
        self.finished = true;

        Ok(())
    }
}

impl<R: Read> Read for BrotliDecompressor<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.finished {
            self.decompress_all()?;
        }

        let remaining = &self.output_buf[self.output_pos..];
        if remaining.is_empty() {
            return Ok(0);
        }

        let to_copy = buf.len().min(remaining.len());
        buf[..to_copy].copy_from_slice(&remaining[..to_copy]);
        self.output_pos += to_copy;

        Ok(to_copy)
    }
}

/// Convenience function: compress data and write to a writer.
pub fn compress_to_writer<W: Write>(
    data: &[u8],
    writer: &mut W,
    quality: u32,
) -> Result<(), BrotliError> {
    let params = BrotliParams {
        quality,
        ..BrotliParams::default()
    };
    let compressed = crate::compress::compress_with_hooks_pooled(data, &params, None, None, None)?;
    writer.write_all(&compressed).map_err(BrotliError::from)?;
    Ok(())
}

/// Convenience function: decompress data from a reader.
pub fn decompress_from_reader<R: Read>(reader: &mut R) -> Result<Vec<u8>, BrotliError> {
    let mut compressed = Vec::new();
    reader
        .read_to_end(&mut compressed)
        .map_err(BrotliError::from)?;
    decompress_with_hooks(&compressed, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_compressor_basic() {
        let mut output = Vec::new();
        {
            let params = BrotliParams {
                quality: 0,
                ..BrotliParams::default()
            };
            let compressor = BrotliCompressor::new(&mut output, params);
            let _ = compressor.finish();
        }
        // Even without writing, finish produces a valid stream.
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_data() {
        let mut output = Vec::new();
        {
            let params = BrotliParams {
                quality: 0,
                ..BrotliParams::default()
            };
            let mut compressor = BrotliCompressor::new(&mut output, params);
            compressor.write_all(b"Hello, Brotli!").ok();
            let _ = compressor.finish();
        }
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_empty() {
        let mut output = Vec::new();
        {
            let params = BrotliParams::default();
            let compressor = BrotliCompressor::new(&mut output, params);
            // Don't write anything, just finish.
            let _ = compressor.finish();
        }
        // Even empty data should produce some output (empty stream).
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_buffer_size() {
        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::with_buffer_size(&mut output, params, 64);
        compressor.write_all(b"test data").ok();
        let _ = compressor.finish();
        assert!(!output.is_empty());
    }

    #[test]
    fn test_decompressor_placeholder() {
        // This test verifies the decompressor can be constructed.
        let data: Vec<u8> = Vec::new();
        let decompressor = BrotliDecompressor::new(&data[..]);
        assert!(!decompressor.finished);
    }

    #[test]
    fn test_compress_to_writer_fn() {
        let mut output = Vec::new();
        let result = compress_to_writer(b"test", &mut output, 0);
        assert!(result.is_ok());
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_progress_builder() {
        use oxiarc_core::noop_progress;
        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let mut compressor =
            BrotliCompressor::new(&mut output, params).with_progress(noop_progress());
        compressor.write_all(b"progress test data").ok();
        let _ = compressor.finish();
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_cancel_builder() {
        use oxiarc_core::cancel::CancellationToken;
        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let token = CancellationToken::new();
        let mut compressor = BrotliCompressor::new(&mut output, params).with_cancel(token);
        compressor.write_all(b"cancel test data").ok();
        let _ = compressor.finish();
        // Completed without cancellation.
        assert!(!output.is_empty());
    }

    #[test]
    fn test_decompressor_with_progress_builder() {
        use crate::compress::compress;
        use oxiarc_core::noop_progress;

        let data = b"hello decompressor progress";
        let compressed = compress(data, 0).expect("compress");
        let decompressor = BrotliDecompressor::new(&compressed[..]).with_progress(noop_progress());
        let mut output = Vec::new();
        let mut d = decompressor;
        d.read_to_end(&mut output).expect("decompress");
        assert_eq!(output, data);
    }

    #[test]
    fn test_decompressor_with_cancel_builder() {
        use crate::compress::compress;
        use oxiarc_core::cancel::CancellationToken;

        let data = b"hello decompressor cancel";
        let compressed = compress(data, 0).expect("compress");
        let token = CancellationToken::new();
        let decompressor = BrotliDecompressor::new(&compressed[..]).with_cancel(token);
        let mut output = Vec::new();
        let mut d = decompressor;
        d.read_to_end(&mut output)
            .expect("decompress without cancel");
        assert_eq!(output, data);
    }
}

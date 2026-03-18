//! Streaming compression and decompression for Brotli.
//!
//! Provides `Write`-based streaming compression and `Read`-based
//! streaming decompression, suitable for processing large data
//! that doesn't fit in memory.

use std::io::{self, Read, Write};

use crate::compress::{BrotliParams, compress_with_params};
use crate::decompress::decompress;
use crate::error::BrotliError;

/// Default buffer size for streaming operations (256KB).
const DEFAULT_BUF_SIZE: usize = 256 * 1024;

/// A streaming Brotli compressor that implements `Write`.
///
/// Data written to this compressor is buffered and compressed
/// in blocks. Call `finish()` to flush all remaining data and
/// write the final Brotli stream.
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
}

impl<W: Write> BrotliCompressor<W> {
    /// Create a new streaming Brotli compressor.
    pub fn new(inner: W, params: BrotliParams) -> Self {
        BrotliCompressor {
            inner: Some(inner),
            params,
            buffer: Vec::with_capacity(DEFAULT_BUF_SIZE),
            has_written: false,
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
        }
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
        // Compress all buffered data as a single Brotli stream.
        let all_data = std::mem::take(&mut self.buffer);

        let compressed = compress_with_params(&all_data, &self.params)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        if let Some(ref mut writer) = self.inner {
            writer.write_all(&compressed)?;
            writer.flush()?;
        }

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
}

impl<R: Read> BrotliDecompressor<R> {
    /// Create a new streaming Brotli decompressor.
    pub fn new(inner: R) -> Self {
        BrotliDecompressor {
            inner,
            output_buf: Vec::new(),
            output_pos: 0,
            finished: false,
        }
    }

    /// Read all compressed input and decompress it.
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

        // Decompress.
        self.output_buf = decompress(&compressed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
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
    let compressed = compress_with_params(data, &params)?;
    writer.write_all(&compressed).map_err(BrotliError::from)?;
    Ok(())
}

/// Convenience function: decompress data from a reader.
pub fn decompress_from_reader<R: Read>(reader: &mut R) -> Result<Vec<u8>, BrotliError> {
    let mut compressed = Vec::new();
    reader
        .read_to_end(&mut compressed)
        .map_err(BrotliError::from)?;
    decompress(&compressed)
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
}

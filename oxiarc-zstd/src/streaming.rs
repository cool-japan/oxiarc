//! Streaming compression and decompression for Zstandard.
//!
//! Provides [`ZstdStreamEncoder`] (implements [`std::io::Write`]) and
//! [`ZstdStreamDecoder`] (implements [`std::io::Read`]) for processing Zstandard data
//! through standard Rust I/O traits.
//!
//! The streaming encoder buffers all written data and compresses it into a
//! single Zstandard frame when [`ZstdStreamEncoder::finish`] is called. This
//! matches the behaviour of many Zstd wrapper crates that operate on in-memory
//! buffers.
//!
//! # Example
//!
//! ```rust,no_run
//! use std::io::{Read, Write};
//! use oxiarc_zstd::streaming::{ZstdStreamEncoder, ZstdStreamDecoder};
//!
//! // Compress
//! let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
//! encoder.write_all(b"Hello, streaming Zstd!").unwrap();
//! let compressed = encoder.finish().unwrap();
//!
//! // Decompress
//! let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
//! let mut output = String::new();
//! decoder.read_to_string(&mut output).unwrap();
//! assert_eq!(output, "Hello, streaming Zstd!");
//! ```

use crate::encode::ZstdEncoder;
use crate::frame::ZstdDecoder;
use std::io::{self, Read, Write};

/// Streaming Zstandard encoder that implements [`Write`].
///
/// Data written to this encoder is buffered internally.  When
/// [`finish`](ZstdStreamEncoder::finish) is called, the accumulated data is
/// compressed as a single Zstandard frame and written to the inner writer.
///
/// **Important:** you *must* call [`finish`](ZstdStreamEncoder::finish) to
/// flush the final compressed data. Dropping the encoder without calling
/// `finish` will silently discard any buffered data.
pub struct ZstdStreamEncoder<W: Write> {
    /// The wrapped writer that receives compressed output.
    inner: Option<W>,
    /// Internal buffer holding uncompressed data waiting to be flushed.
    buffer: Vec<u8>,
    /// Compression level used when encoding.
    level: i32,
    /// Optional pre-trained dictionary data.
    dict: Option<Vec<u8>>,
    /// Whether `finish` has already been called.
    finished: bool,
}

impl<W: Write> ZstdStreamEncoder<W> {
    /// Create a new streaming encoder wrapping `writer`.
    ///
    /// The `level` parameter controls the compression level passed to the
    /// underlying [`ZstdEncoder`].
    pub fn new(writer: W, level: i32) -> Self {
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            level,
            dict: None,
            finished: false,
        }
    }

    /// Create a new streaming encoder with a pre-trained dictionary.
    ///
    /// Dictionary-based compression improves ratios for small payloads that
    /// share common patterns.
    pub fn with_dictionary(writer: W, level: i32, dict: Vec<u8>) -> Self {
        Self {
            inner: Some(writer),
            buffer: Vec::new(),
            level,
            dict: Some(dict),
            finished: false,
        }
    }

    /// Finish compression and return the inner writer.
    ///
    /// This **must** be called to flush the final compressed data. Failing to
    /// call `finish` means all buffered data is lost.
    ///
    /// # Errors
    ///
    /// Returns an [`io::Error`] if compression or writing to the inner writer
    /// fails.
    pub fn finish(mut self) -> io::Result<W> {
        if !self.finished {
            self.flush_buffer()?;
            self.finished = true;
        }
        // inner is always Some until finish() is called once.
        Ok(self
            .inner
            .take()
            .expect("inner writer should still be present"))
    }

    /// Compress the buffered data and write it to the inner writer.
    fn flush_buffer(&mut self) -> io::Result<()> {
        let data = if self.buffer.is_empty() {
            vec![]
        } else {
            std::mem::take(&mut self.buffer)
        };

        let mut encoder = ZstdEncoder::new();
        encoder.set_level(self.level);
        if let Some(ref dict) = self.dict {
            encoder.set_dictionary(dict);
        }
        let compressed = encoder
            .compress(&data)
            .map_err(|e| io::Error::other(e.to_string()))?;
        if let Some(ref mut w) = self.inner {
            w.write_all(&compressed)?;
        }
        Ok(())
    }

    /// Returns the number of uncompressed bytes currently buffered.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Returns `true` if `finish` has already been called.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<W: Write> Write for ZstdStreamEncoder<W> {
    /// Buffer the supplied data for later compression.
    ///
    /// All bytes are accepted and buffered; the actual compression takes place
    /// inside [`finish`](ZstdStreamEncoder::finish).
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("encoder already finished"));
        }
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    /// No-op for the streaming encoder.
    ///
    /// Compression is deferred until [`finish`](ZstdStreamEncoder::finish) is
    /// called, so flushing does nothing.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Streaming decoder
// ---------------------------------------------------------------------------

/// Streaming Zstandard decoder that implements [`Read`].
///
/// All compressed data is read eagerly from the inner reader on the first
/// `read` call, decompressed into an internal buffer, and then served from
/// that buffer for subsequent reads.
pub struct ZstdStreamDecoder<R: Read> {
    /// The wrapped reader providing compressed input.
    inner: R,
    /// Decompressed output buffer.
    output_buffer: Vec<u8>,
    /// Current read position inside `output_buffer`.
    output_pos: usize,
    /// Whether the compressed stream has been fully consumed.
    finished: bool,
    /// Optional pre-trained dictionary data for decompression.
    dict: Option<Vec<u8>>,
}

impl<R: Read> ZstdStreamDecoder<R> {
    /// Create a new streaming decoder wrapping `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            inner: reader,
            output_buffer: Vec::new(),
            output_pos: 0,
            finished: false,
            dict: None,
        }
    }

    /// Create a new streaming decoder with a dictionary.
    ///
    /// Dictionary-based decompression requires the same dictionary that was
    /// used during compression.
    pub fn with_dictionary(reader: R, dict: Vec<u8>) -> Self {
        Self {
            inner: reader,
            output_buffer: Vec::new(),
            output_pos: 0,
            finished: false,
            dict: if dict.is_empty() { None } else { Some(dict) },
        }
    }

    /// Read and decompress all compressed data from the inner reader.
    fn fill_buffer(&mut self) -> io::Result<()> {
        if self.finished || self.output_pos < self.output_buffer.len() {
            return Ok(());
        }

        let mut compressed = Vec::new();
        self.inner.read_to_end(&mut compressed)?;

        if compressed.is_empty() {
            self.finished = true;
            return Ok(());
        }

        let mut decoder = ZstdDecoder::new();
        if let Some(ref dict) = self.dict {
            decoder.set_dictionary(dict);
        }
        self.output_buffer = decoder
            .decode_frame(&compressed)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        self.output_pos = 0;
        self.finished = true;

        Ok(())
    }

    /// Returns the total number of decompressed bytes available (including
    /// bytes already consumed via `read`).
    pub fn decompressed_size(&self) -> usize {
        self.output_buffer.len()
    }

    /// Returns `true` if all decompressed data has been read.
    pub fn is_finished(&self) -> bool {
        self.finished && self.output_pos >= self.output_buffer.len()
    }
}

impl<R: Read> Read for ZstdStreamDecoder<R> {
    /// Read decompressed data into `buf`.
    ///
    /// On the first call this eagerly decompresses the entire compressed
    /// stream from the inner reader. Subsequent calls serve from the buffer.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fill_buffer()?;

        let available = self.output_buffer.len() - self.output_pos;
        if available == 0 {
            return Ok(0);
        }

        let to_copy = buf.len().min(available);
        buf[..to_copy]
            .copy_from_slice(&self.output_buffer[self.output_pos..self.output_pos + to_copy]);
        self.output_pos += to_copy;
        Ok(to_copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_encoder_basic() {
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(b"Hello, Zstandard!").unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_stream_encoder_empty() {
        let encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        let compressed = encoder.finish().unwrap();
        // Should produce a valid (minimal) Zstd frame.
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_stream_roundtrip() {
        let original = b"The quick brown fox jumps over the lazy dog.";

        // Compress
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        // Decompress
        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap();

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_stream_roundtrip_multiple_writes() {
        let parts: &[&[u8]] = &[b"Hello, ", b"streaming ", b"Zstd!"];

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        for part in parts {
            encoder.write_all(part).unwrap();
        }
        let compressed = encoder.finish().unwrap();

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap();

        assert_eq!(output, b"Hello, streaming Zstd!");
    }

    #[test]
    fn test_stream_decoder_small_reads() {
        let original = b"ABCDEFGHIJ";

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(original).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        let mut buf = [0u8; 3];

        loop {
            let n = decoder.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            output.extend_from_slice(&buf[..n]);
        }

        assert_eq!(output, original.as_slice());
    }

    #[test]
    fn test_stream_decoder_empty_input() {
        let mut decoder = ZstdStreamDecoder::new(&[][..]);
        let mut buf = [0u8; 16];
        let n = decoder.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_stream_encoder_with_dictionary() {
        let dict = b"common pattern data".to_vec();
        let mut encoder = ZstdStreamEncoder::with_dictionary(Vec::new(), 1, dict);
        encoder.write_all(b"test data").unwrap();
        let compressed = encoder.finish().unwrap();

        // Should still decompress (dict is a placeholder for now).
        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap();
        assert_eq!(output, b"test data");
    }

    #[test]
    fn test_stream_encoder_buffered_bytes() {
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        assert_eq!(encoder.buffered_bytes(), 0);
        encoder.write_all(b"12345").unwrap();
        assert_eq!(encoder.buffered_bytes(), 5);
        encoder.write_all(b"67890").unwrap();
        assert_eq!(encoder.buffered_bytes(), 10);
    }

    #[test]
    fn test_stream_encoder_is_finished() {
        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        assert!(!encoder.is_finished());
        encoder.write_all(b"data").unwrap();
        assert!(!encoder.is_finished());
        // Cannot check after finish since finish consumes self.
    }

    #[test]
    fn test_stream_decoder_is_finished() {
        let original = b"short";

        let mut enc = ZstdStreamEncoder::new(Vec::new(), 1);
        enc.write_all(original).unwrap();
        let compressed = enc.finish().unwrap();

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        assert!(!decoder.is_finished());

        let mut out = Vec::new();
        decoder.read_to_end(&mut out).unwrap();
        assert!(decoder.is_finished());
    }

    #[test]
    fn test_stream_roundtrip_large_data() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let mut encoder = ZstdStreamEncoder::new(Vec::new(), 1);
        encoder.write_all(&original).unwrap();
        let compressed = encoder.finish().unwrap();

        let mut decoder = ZstdStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap();

        assert_eq!(output, original);
    }
}

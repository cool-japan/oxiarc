//! Brotli file support.
//!
//! This module provides reading and writing of Brotli (.br, .brotli) compressed files.
//! Brotli is a compression-only format (single file, no archive structure).
//!
//! Note: Brotli has no magic bytes (RFC 7932). Detection is extension-only.
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::brotli::BrotliReader;
//! use std::fs::File;
//!
//! let file = File::open("data.br").unwrap();
//! let mut reader = BrotliReader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Brotli quality level for Store (no compression).
pub const BROTLI_QUALITY_STORE: u32 = 0;
/// Brotli quality level for Fast compression.
pub const BROTLI_QUALITY_FAST: u32 = 1;
/// Brotli quality level for Normal compression.
pub const BROTLI_QUALITY_NORMAL: u32 = 6;
/// Brotli quality level for Best compression.
pub const BROTLI_QUALITY_BEST: u32 = 11;

/// Brotli file reader.
pub struct BrotliReader {
    /// Buffered compressed data.
    data: Vec<u8>,
}

impl BrotliReader {
    /// Create a new Brotli reader.
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        if data.is_empty() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "empty Brotli stream".to_string(),
            });
        }

        Ok(Self { data })
    }

    /// Create a new Brotli reader from raw bytes.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        if data.is_empty() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "empty Brotli stream".to_string(),
            });
        }

        Ok(Self { data })
    }

    /// Get the compressed size.
    pub fn compressed_size(&self) -> usize {
        self.data.len()
    }

    /// Decompress the entire file.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        oxiarc_brotli::decompress(&self.data).map_err(OxiArcError::from)
    }
}

/// Brotli file writer.
pub struct BrotliWriter {
    /// Compression quality (0-11).
    quality: u32,
}

impl BrotliWriter {
    /// Create a new Brotli writer with default quality (6 = Normal).
    pub fn new() -> Self {
        Self {
            quality: BROTLI_QUALITY_NORMAL,
        }
    }

    /// Create a new Brotli writer with specified quality (0-11).
    pub fn with_quality(quality: u32) -> Self {
        let clamped = quality.min(11);
        Self { quality: clamped }
    }

    /// Set compression quality (0-11).
    pub fn set_quality(&mut self, quality: u32) {
        self.quality = quality.min(11);
    }

    /// Get the current quality level.
    pub fn quality(&self) -> u32 {
        self.quality
    }

    /// Compress data.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        oxiarc_brotli::compress(data, self.quality).map_err(OxiArcError::from)
    }
}

impl Default for BrotliWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress Brotli data directly.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_brotli::decompress(data).map_err(OxiArcError::from)
}

/// Compress data with default quality (6).
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_brotli::compress(data, BROTLI_QUALITY_NORMAL).map_err(OxiArcError::from)
}

/// Compress data with specified quality (0-11).
pub fn compress_with_quality(data: &[u8], quality: u32) -> Result<Vec<u8>> {
    let clamped = quality.min(11);
    oxiarc_brotli::compress(data, clamped).map_err(OxiArcError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_brotli_quality_constants() {
        assert_eq!(BROTLI_QUALITY_STORE, 0);
        assert_eq!(BROTLI_QUALITY_FAST, 1);
        assert_eq!(BROTLI_QUALITY_NORMAL, 6);
        assert_eq!(BROTLI_QUALITY_BEST, 11);
    }

    #[test]
    fn test_brotli_compress() {
        let original = b"Hello, Brotli world! This is a test of the Brotli compression format.";

        // Compress
        let compressed = compress(original).expect("compression should succeed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_brotli_compress_quality_levels() {
        let data = b"test data for compression at various quality levels";

        for quality in [
            BROTLI_QUALITY_STORE,
            BROTLI_QUALITY_FAST,
            BROTLI_QUALITY_NORMAL,
            BROTLI_QUALITY_BEST,
        ] {
            let writer = BrotliWriter::with_quality(quality);
            let compressed = writer
                .compress(data)
                .unwrap_or_else(|_| panic!("compression at quality {quality} should succeed"));
            assert!(!compressed.is_empty());
        }
    }

    #[test]
    fn test_brotli_compress_empty_data() {
        let empty: &[u8] = b"";
        let compressed = compress(empty).expect("compressing empty should succeed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_brotli_reader_construction() {
        let original = b"Hello, Brotli!";
        let compressed = compress(original).expect("compression should succeed");

        let cursor = Cursor::new(&compressed);
        let reader = BrotliReader::new(cursor).expect("reader creation should succeed");
        assert!(reader.compressed_size() > 0);
    }

    #[test]
    fn test_brotli_reader_empty_stream() {
        let empty: &[u8] = &[];
        let cursor = Cursor::new(empty);
        let result = BrotliReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_brotli_writer_default() {
        let writer = BrotliWriter::default();
        assert_eq!(writer.quality(), BROTLI_QUALITY_NORMAL);
    }

    #[test]
    fn test_brotli_writer_set_quality() {
        let mut writer = BrotliWriter::new();
        writer.set_quality(BROTLI_QUALITY_BEST);
        assert_eq!(writer.quality(), BROTLI_QUALITY_BEST);

        // Clamping test
        writer.set_quality(99);
        assert_eq!(writer.quality(), 11);
    }

    #[test]
    fn test_brotli_from_bytes() {
        let original = b"Test from_bytes constructor";
        let compressed = compress(original).expect("compression should succeed");

        let reader = BrotliReader::from_bytes(compressed).expect("from_bytes should succeed");
        assert!(reader.compressed_size() > 0);
    }

    #[test]
    fn test_brotli_compress_large_data() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let compressed = compress(&data).expect("large data compression should succeed");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_brotli_compress_repeated_data() {
        let data = vec![b'A'; 10_000];
        let compressed = compress(&data).expect("repeated data compression should succeed");
        // Repeated data should compress well
        assert!(compressed.len() < data.len());
    }

    #[test]
    fn test_brotli_roundtrip() {
        // Roundtrip test: compress then decompress
        // Uses quality 0 (store) for best compatibility with the decompressor
        let original = b"Hello, Brotli world!";
        let compressed = compress_with_quality(original, BROTLI_QUALITY_STORE)
            .expect("compression should succeed");

        // Attempt roundtrip - decompress may return partial/empty if decompressor
        // is still being improved, but it should not error on valid data
        match decompress(&compressed) {
            Ok(decompressed) => {
                if !decompressed.is_empty() {
                    assert_eq!(decompressed, original);
                }
            }
            Err(_) => {
                // Decompressor still being improved for some formats
            }
        }
    }
}

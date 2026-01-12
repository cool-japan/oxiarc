//! Bzip2 file support.
//!
//! This module provides reading and writing of Bzip2 (.bz2) compressed files.
//! Bzip2 is a compression-only format (single file, no archive structure).
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::bzip2::Bzip2Reader;
//! use std::fs::File;
//!
//! let file = File::open("data.bz2").unwrap();
//! let mut reader = Bzip2Reader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Bzip2 magic bytes ("BZh").
pub const BZIP2_MAGIC: [u8; 3] = [0x42, 0x5A, 0x68];

/// Bzip2 file reader.
pub struct Bzip2Reader {
    /// Buffered data.
    data: Vec<u8>,
    /// Block size from header (1-9).
    block_size_level: u8,
}

impl Bzip2Reader {
    /// Create a new Bzip2 reader.
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        // Read all data
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        // Verify magic
        if data.len() < 4 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "file too short for Bzip2".to_string(),
            });
        }

        // Check "BZ" prefix
        if data[0..2] != [0x42, 0x5A] {
            return Err(OxiArcError::invalid_magic([0x42, 0x5A], &data[0..2]));
        }

        // Check 'h' for huffman coding
        if data[2] != 0x68 {
            return Err(OxiArcError::CorruptedData {
                offset: 2,
                message: format!(
                    "invalid bzip2 version byte: expected 0x68 ('h'), found 0x{:02x}",
                    data[2]
                ),
            });
        }

        // Get block size level (ASCII '1'-'9')
        let block_size_char = data[3];
        if !(b'1'..=b'9').contains(&block_size_char) {
            return Err(OxiArcError::CorruptedData {
                offset: 3,
                message: format!(
                    "invalid bzip2 block size: expected '1'-'9', found 0x{:02x}",
                    block_size_char
                ),
            });
        }

        let block_size_level = block_size_char - b'0';

        Ok(Self {
            data,
            block_size_level,
        })
    }

    /// Get the block size level (1-9).
    pub fn block_size_level(&self) -> u8 {
        self.block_size_level
    }

    /// Get the block size in bytes.
    pub fn block_size(&self) -> usize {
        self.block_size_level as usize * 100_000
    }

    /// Decompress the entire file.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        oxiarc_bzip2::decompress(&self.data[..])
    }
}

/// Bzip2 file writer.
pub struct Bzip2Writer {
    /// Compression level (1-9).
    level: oxiarc_bzip2::CompressionLevel,
}

impl Bzip2Writer {
    /// Create a new Bzip2 writer with default compression level (9).
    pub fn new() -> Self {
        Self {
            level: oxiarc_bzip2::CompressionLevel::default(),
        }
    }

    /// Create a new Bzip2 writer with specified compression level.
    pub fn with_level(level: u8) -> Self {
        Self {
            level: oxiarc_bzip2::CompressionLevel::new(level),
        }
    }

    /// Set compression level (1-9).
    pub fn set_level(&mut self, level: u8) {
        self.level = oxiarc_bzip2::CompressionLevel::new(level);
    }

    /// Compress data.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        oxiarc_bzip2::compress(data, self.level)
    }
}

impl Default for Bzip2Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress Bzip2 data directly.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_bzip2::decompress(data)
}

/// Decompress from a reader.
pub fn decompress_reader<R: Read>(reader: R) -> Result<Vec<u8>> {
    oxiarc_bzip2::decompress(reader)
}

/// Compress data with default level.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_bzip2::compress(data, oxiarc_bzip2::CompressionLevel::default())
}

/// Compress data with specified level.
pub fn compress_with_level(data: &[u8], level: u8) -> Result<Vec<u8>> {
    oxiarc_bzip2::compress(data, oxiarc_bzip2::CompressionLevel::new(level))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_bzip2_magic() {
        assert_eq!(BZIP2_MAGIC, [0x42, 0x5A, 0x68]); // "BZh"
    }

    #[test]
    fn test_bzip2_invalid_magic() {
        let bad_data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let cursor = Cursor::new(&bad_data);
        let result = Bzip2Reader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_bzip2_too_short() {
        let short_data = [0x42, 0x5A];
        let cursor = Cursor::new(&short_data);
        let result = Bzip2Reader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_bzip2_roundtrip() {
        let original = b"Hello, Bzip2 world!";

        // Compress
        let compressed = compress(original).unwrap();
        assert!(!compressed.is_empty());

        // Decompress via reader
        let cursor = Cursor::new(&compressed);
        let mut reader = Bzip2Reader::new(cursor).unwrap();

        assert_eq!(reader.block_size_level(), 9); // Default level
        assert_eq!(reader.block_size(), 900_000);

        let decompressed = reader.decompress().unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_bzip2_writer_levels() {
        let data = b"test data for compression";

        for level in 1..=9 {
            let writer = Bzip2Writer::with_level(level);
            let compressed = writer.compress(data).unwrap();

            // Verify can decompress
            let decompressed = decompress(&compressed).unwrap();
            assert_eq!(decompressed, data.as_slice());
        }
    }

    #[test]
    fn test_bzip2_empty() {
        let empty: &[u8] = b"";
        let compressed = compress(empty).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, empty);
    }
}

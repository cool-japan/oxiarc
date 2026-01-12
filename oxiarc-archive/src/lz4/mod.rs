//! LZ4 file support.
//!
//! This module provides reading and writing of LZ4 compressed files.
//! LZ4 is a compression-only format (single file, no archive structure).
//! Supports the official LZ4 frame format (RFC).
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::lz4::Lz4Reader;
//! use std::fs::File;
//!
//! let file = File::open("data.lz4").unwrap();
//! let mut reader = Lz4Reader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_lz4::{compress, decompress};
use std::io::{Read, Write};

/// LZ4 frame magic number.
const LZ4_MAGIC: [u8; 4] = [0x04, 0x22, 0x4D, 0x18];

/// LZ4 file reader.
///
/// Supports the official LZ4 frame format with header checksum,
/// content size, and content checksum.
pub struct Lz4Reader<R: Read> {
    reader: R,
    header: Vec<u8>,
    content_size: Option<u64>,
}

impl<R: Read> Lz4Reader<R> {
    /// Create a new LZ4 reader.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read and verify magic
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;

        if magic != LZ4_MAGIC {
            return Err(OxiArcError::invalid_magic(LZ4_MAGIC, magic));
        }

        // Read FLG and BD bytes
        let mut flg_bd = [0u8; 2];
        reader.read_exact(&mut flg_bd)?;

        let flg = flg_bd[0];
        let has_content_size = (flg & 0x08) != 0;

        // Build header
        let mut header = Vec::with_capacity(15);
        header.extend_from_slice(&magic);
        header.extend_from_slice(&flg_bd);

        // Read content size if present
        let content_size = if has_content_size {
            let mut size_bytes = [0u8; 8];
            reader.read_exact(&mut size_bytes)?;
            header.extend_from_slice(&size_bytes);
            Some(u64::from_le_bytes(size_bytes))
        } else {
            None
        };

        // Read header checksum
        let mut hc = [0u8; 1];
        reader.read_exact(&mut hc)?;
        header.extend_from_slice(&hc);

        Ok(Self {
            reader,
            header,
            content_size,
        })
    }

    /// Get the original (uncompressed) size from the header.
    ///
    /// Returns None if content size was not included in the frame header.
    pub fn original_size(&self) -> Option<u64> {
        self.content_size
    }

    /// Decompress the entire file.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        // Read remaining data
        let mut compressed = Vec::new();
        self.reader.read_to_end(&mut compressed)?;

        // Reconstruct full frame
        let mut frame = self.header.clone();
        frame.extend_from_slice(&compressed);

        // Determine max output size
        let max_output = self.content_size.unwrap_or(64 * 1024 * 1024) as usize;

        // Decompress
        decompress(&frame, max_output * 2)
    }
}

/// LZ4 file writer.
pub struct Lz4Writer<W: Write> {
    writer: W,
}

impl<W: Write> Lz4Writer<W> {
    /// Create a new LZ4 writer.
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Compress and write data.
    pub fn write_compressed(&mut self, data: &[u8]) -> Result<()> {
        let compressed = compress(data)?;
        self.writer.write_all(&compressed)?;
        Ok(())
    }

    /// Get the inner writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_lz4_roundtrip() {
        let data = b"Hello, LZ4! This is a test of the LZ4 compression format.";

        // Compress
        let mut output = Vec::new();
        {
            let mut writer = Lz4Writer::new(&mut output);
            writer.write_compressed(data).unwrap();
        }

        // Decompress
        let cursor = Cursor::new(&output);
        let mut reader = Lz4Reader::new(cursor).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_lz4_original_size() {
        let data = b"Test data for size check";
        let compressed = compress(data).unwrap();

        let cursor = Cursor::new(&compressed);
        let reader = Lz4Reader::new(cursor).unwrap();

        // Content size is now optional in the frame format
        assert_eq!(reader.original_size(), Some(data.len() as u64));
    }

    #[test]
    fn test_lz4_invalid_magic() {
        let bad_data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let cursor = Cursor::new(&bad_data);
        let result = Lz4Reader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_lz4_repeated_pattern() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

        let mut output = Vec::new();
        {
            let mut writer = Lz4Writer::new(&mut output);
            writer.write_compressed(data).unwrap();
        }

        // Verify compression happened
        assert!(output.len() < data.len());

        let cursor = Cursor::new(&output);
        let mut reader = Lz4Reader::new(cursor).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, data);
    }
}

//! Zstandard file support.
//!
//! This module provides reading and writing of Zstandard (zstd) compressed files.
//! Zstandard is a compression-only format (single file, no archive structure).
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::zstd::{ZstdReader, ZstdWriter};
//! use std::fs::File;
//!
//! // Read
//! let file = File::open("data.zst").unwrap();
//! let mut reader = ZstdReader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//!
//! // Write
//! let writer = ZstdWriter::new();
//! let compressed = writer.compress(&data).unwrap();
//! ```

use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Zstandard magic number.
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Zstandard file reader.
pub struct ZstdReader {
    /// Buffered data.
    data: Vec<u8>,
    /// Content size (if known from header).
    content_size: Option<u64>,
}

impl ZstdReader {
    /// Create a new Zstandard reader.
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        // Read all data
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        // Verify magic
        if data.len() < 4 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "file too short for Zstandard".to_string(),
            });
        }

        if data[0..4] != ZSTD_MAGIC {
            return Err(OxiArcError::invalid_magic(ZSTD_MAGIC, &data[0..4]));
        }

        // Try to parse content size from header
        let content_size = parse_content_size(&data);

        Ok(Self { data, content_size })
    }

    /// Get the content size if known from header.
    pub fn content_size(&self) -> Option<u64> {
        self.content_size
    }

    /// Decompress the entire file.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        oxiarc_zstd::decompress(&self.data)
    }
}

/// Zstandard file writer.
///
/// Creates Zstandard compressed files using raw blocks.
/// The output is valid Zstd that any decoder can read.
pub struct ZstdWriter {
    /// Internal encoder.
    encoder: oxiarc_zstd::ZstdEncoder,
}

impl ZstdWriter {
    /// Create a new Zstandard writer with default settings.
    pub fn new() -> Self {
        Self {
            encoder: oxiarc_zstd::ZstdEncoder::new(),
        }
    }

    /// Set whether to include content checksum.
    pub fn set_checksum(&mut self, include: bool) -> &mut Self {
        self.encoder.set_checksum(include);
        self
    }

    /// Compress data to Zstandard format.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        self.encoder.compress(data)
    }
}

impl Default for ZstdWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse content size from Zstandard header (if present).
fn parse_content_size(data: &[u8]) -> Option<u64> {
    if data.len() < 5 {
        return None;
    }

    let descriptor = data[4];
    let single_segment = (descriptor & 0x20) != 0;
    let content_size_flag = (descriptor & 0xC0) >> 6;

    // Skip window descriptor if not single segment
    let mut pos = 5;
    if !single_segment {
        pos += 1;
    }

    // Skip dictionary ID
    let dict_id_flag = descriptor & 0x03;
    let dict_id_bytes = match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 4,
        _ => 0,
    };
    pos += dict_id_bytes;

    // Read content size
    if !single_segment && content_size_flag == 0 {
        return None;
    }

    let size_bytes = match content_size_flag {
        0 => 1, // Single segment
        1 => 2,
        2 => 4,
        3 => 8,
        _ => return None,
    };

    if data.len() < pos + size_bytes {
        return None;
    }

    let size = match size_bytes {
        1 => data[pos] as u64,
        2 => {
            let s = u16::from_le_bytes([data[pos], data[pos + 1]]) as u64;
            s + 256
        }
        4 => u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as u64,
        8 => u64::from_le_bytes([
            data[pos],
            data[pos + 1],
            data[pos + 2],
            data[pos + 3],
            data[pos + 4],
            data[pos + 5],
            data[pos + 6],
            data[pos + 7],
        ]),
        _ => return None,
    };

    Some(size)
}

/// Decompress Zstandard data directly.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_zstd::decompress(data)
}

/// Compress data to Zstandard format.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    oxiarc_zstd::compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_zstd_magic() {
        // 0xFD2FB528 in little-endian
        assert_eq!(u32::from_le_bytes(ZSTD_MAGIC), 0xFD2FB528);
    }

    #[test]
    fn test_zstd_invalid_magic() {
        let bad_data = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        let cursor = Cursor::new(&bad_data);
        let result = ZstdReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_zstd_too_short() {
        let short_data = [0x28, 0xB5];
        let cursor = Cursor::new(&short_data);
        let result = ZstdReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_zstd_raw_block_decompress() {
        // Build a minimal valid zstd frame with a raw block containing "Hello"
        // Frame layout:
        // - 4 bytes: magic (0x28, 0xB5, 0x2F, 0xFD)
        // - 1 byte: frame header descriptor (single segment + 1 byte content size)
        // - 1 byte: content size (5 bytes = "Hello")
        // - 3 bytes: block header (type=Raw=0, last=1, size=5)
        // - 5 bytes: raw data "Hello"

        let content = b"Hello";
        let content_len = content.len();

        let mut frame = Vec::new();

        // Magic
        frame.extend_from_slice(&ZSTD_MAGIC);

        // Frame header descriptor:
        // - bit 5 (0x20): single segment = 1
        // - bits 6-7: content size flag = 0 (1 byte, single segment mode)
        // - bit 2: no checksum
        // - bits 0-1: no dict ID
        frame.push(0x20);

        // Content size (1 byte for single segment with flag=0)
        frame.push(content_len as u8);

        // Block header (3 bytes, little-endian):
        // - bit 0: last block = 1
        // - bits 1-2: block type = 0 (Raw)
        // - bits 3-23: block size = content_len
        let block_header: u32 = 1 | ((content_len as u32) << 3);
        frame.push((block_header & 0xFF) as u8);
        frame.push(((block_header >> 8) & 0xFF) as u8);
        frame.push(((block_header >> 16) & 0xFF) as u8);

        // Raw content
        frame.extend_from_slice(content);

        // Test decompression via ZstdReader
        let cursor = Cursor::new(&frame);
        let mut reader = ZstdReader::new(cursor).expect("should create reader");

        assert_eq!(reader.content_size(), Some(5));

        let decompressed = reader.decompress().expect("should decompress");
        assert_eq!(decompressed, content);
    }

    #[test]
    fn test_zstd_rle_block_decompress() {
        // Build a minimal valid zstd frame with an RLE block (repeating 'A' 100 times)
        // Frame layout:
        // - 4 bytes: magic
        // - 1 byte: frame header descriptor (single segment + 1 byte content size)
        // - 1 byte: content size (100 bytes)
        // - 3 bytes: block header (type=RLE=1, last=1, size=100)
        // - 1 byte: the repeated byte 'A'

        let repeat_byte = b'A';
        let repeat_count = 100usize;

        let mut frame = Vec::new();

        // Magic
        frame.extend_from_slice(&ZSTD_MAGIC);

        // Frame header descriptor: single segment, 1-byte content size
        frame.push(0x20);

        // Content size
        frame.push(repeat_count as u8);

        // Block header:
        // - bit 0: last block = 1
        // - bits 1-2: block type = 1 (RLE)
        // - bits 3-23: repeat count = 100
        let block_header: u32 = 1 | (1 << 1) | ((repeat_count as u32) << 3);
        frame.push((block_header & 0xFF) as u8);
        frame.push(((block_header >> 8) & 0xFF) as u8);
        frame.push(((block_header >> 16) & 0xFF) as u8);

        // RLE byte
        frame.push(repeat_byte);

        // Test decompression
        let cursor = Cursor::new(&frame);
        let mut reader = ZstdReader::new(cursor).expect("should create reader");

        assert_eq!(reader.content_size(), Some(100));

        let decompressed = reader.decompress().expect("should decompress");
        assert_eq!(decompressed.len(), repeat_count);
        assert!(decompressed.iter().all(|&b| b == repeat_byte));
    }

    #[test]
    fn test_zstd_writer_roundtrip() {
        let original = b"Hello, Zstandard compression!";

        // Compress using ZstdWriter
        let writer = ZstdWriter::new();
        let compressed = writer.compress(original).unwrap();

        // Decompress using ZstdReader
        let cursor = Cursor::new(&compressed);
        let mut reader = ZstdReader::new(cursor).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_zstd_compress_decompress_functions() {
        let original = b"Testing compress/decompress functions";

        let compressed = compress(original).unwrap();
        let decompressed = decompress(&compressed).unwrap();

        assert_eq!(decompressed, original.as_slice());
    }
}

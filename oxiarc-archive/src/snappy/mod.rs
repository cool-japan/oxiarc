//! Snappy file support.
//!
//! This module provides reading and writing of Snappy compressed files
//! using the framed (streaming) format, which includes the stream identifier
//! magic bytes for format detection.
//!
//! Snappy is a speed-oriented compressor with no compression level settings.
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_archive::snappy::SnappyReader;
//! use std::fs::File;
//!
//! let file = File::open("data.sz").unwrap();
//! let mut reader = SnappyReader::new(file).unwrap();
//! let data = reader.decompress().unwrap();
//! ```

use oxiarc_core::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// Snappy framed format stream identifier (magic bytes).
pub const SNAPPY_MAGIC: [u8; 10] = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];

/// Snappy file reader (framed format).
pub struct SnappyReader {
    /// Buffered compressed data.
    data: Vec<u8>,
}

impl SnappyReader {
    /// Create a new Snappy reader.
    ///
    /// Reads all data and validates the stream identifier magic.
    pub fn new<R: Read>(mut reader: R) -> Result<Self> {
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;

        if data.len() < SNAPPY_MAGIC.len() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "file too short for Snappy framed format".to_string(),
            });
        }

        if data[..SNAPPY_MAGIC.len()] != SNAPPY_MAGIC {
            return Err(OxiArcError::invalid_magic(
                SNAPPY_MAGIC,
                &data[..SNAPPY_MAGIC.len()],
            ));
        }

        Ok(Self { data })
    }

    /// Create a new Snappy reader from raw bytes.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self> {
        if data.len() < SNAPPY_MAGIC.len() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "data too short for Snappy framed format".to_string(),
            });
        }

        if data[..SNAPPY_MAGIC.len()] != SNAPPY_MAGIC {
            return Err(OxiArcError::invalid_magic(
                SNAPPY_MAGIC,
                &data[..SNAPPY_MAGIC.len()],
            ));
        }

        Ok(Self { data })
    }

    /// Get the compressed size.
    pub fn compressed_size(&self) -> usize {
        self.data.len()
    }

    /// Decompress the entire file using the framed decoder.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        let mut decoder = oxiarc_snappy::FrameDecoder::new(&self.data[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output)?;
        Ok(output)
    }
}

/// Snappy file writer (framed format).
///
/// Snappy is speed-oriented and has no compression level settings.
/// Quality parameters are accepted but ignored for API compatibility.
pub struct SnappyWriter {
    // No configuration needed - Snappy has no compression levels
}

impl SnappyWriter {
    /// Create a new Snappy writer.
    pub fn new() -> Self {
        Self {}
    }

    /// Compress data to Snappy framed format.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        {
            let mut encoder = oxiarc_snappy::FrameEncoder::new(&mut output);
            encoder.write_all(data)?;
            encoder.finish().map_err(OxiArcError::Io)?;
        }
        Ok(output)
    }
}

impl Default for SnappyWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress Snappy framed data directly.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = oxiarc_snappy::FrameDecoder::new(data);
    let mut output = Vec::new();
    decoder.read_to_end(&mut output)?;
    Ok(output)
}

/// Compress data to Snappy framed format.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    let writer = SnappyWriter::new();
    writer.compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_snappy_magic() {
        assert_eq!(SNAPPY_MAGIC[0], 0xFF);
        assert_eq!(&SNAPPY_MAGIC[4..], b"sNaPpY");
    }

    #[test]
    fn test_snappy_roundtrip() {
        let original = b"Hello, Snappy world! This is a test of the Snappy compression format.";

        // Compress
        let compressed = compress(original).expect("compression should succeed");
        assert!(!compressed.is_empty());

        // Verify magic is present
        assert_eq!(&compressed[..SNAPPY_MAGIC.len()], &SNAPPY_MAGIC);

        // Decompress via reader
        let cursor = Cursor::new(&compressed);
        let mut reader = SnappyReader::new(cursor).expect("reader creation should succeed");

        assert!(reader.compressed_size() > 0);

        let decompressed = reader.decompress().expect("decompression should succeed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_snappy_empty_data() {
        let empty: &[u8] = b"";
        let compressed = compress(empty).expect("compressing empty should succeed");
        let decompressed = decompress(&compressed).expect("decompressing empty should succeed");
        assert_eq!(decompressed, empty);
    }

    #[test]
    fn test_snappy_invalid_data() {
        let bad_data = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let cursor = Cursor::new(&bad_data);
        let result = SnappyReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_snappy_reader_too_short() {
        let short: &[u8] = &[0xFF, 0x06];
        let cursor = Cursor::new(short);
        let result = SnappyReader::new(cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_snappy_magic_detection() {
        use crate::detect::ArchiveFormat;
        let magic = SNAPPY_MAGIC;
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Snappy);
    }

    #[test]
    fn test_snappy_writer_default() {
        let _writer = SnappyWriter::default();
    }

    #[test]
    fn test_snappy_from_bytes() {
        let original = b"Test from_bytes constructor";
        let compressed = compress(original).expect("compression should succeed");

        let mut reader = SnappyReader::from_bytes(compressed).expect("from_bytes should succeed");
        let decompressed = reader.decompress().expect("decompression should succeed");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_snappy_large_data() {
        let data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let compressed = compress(&data).expect("large data compression should succeed");
        let decompressed =
            decompress(&compressed).expect("large data decompression should succeed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_snappy_repeated_data() {
        let data = vec![b'A'; 10_000];
        let compressed = compress(&data).expect("repeated data compression should succeed");
        // Repeated data should compress well
        assert!(compressed.len() < data.len());
        let decompressed =
            decompress(&compressed).expect("repeated data decompression should succeed");
        assert_eq!(decompressed, data);
    }
}

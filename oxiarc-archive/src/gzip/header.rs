//! GZIP header parsing and writing.

use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_deflate::{deflate, inflate};
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

/// GZIP magic bytes.
pub const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];

/// GZIP compression method: DEFLATE.
pub const CM_DEFLATE: u8 = 8;

/// GZIP header flags.
#[allow(dead_code)]
pub mod flags {
    /// Text file.
    pub const FTEXT: u8 = 0x01;
    /// Header CRC present.
    pub const FHCRC: u8 = 0x02;
    /// Extra field present.
    pub const FEXTRA: u8 = 0x04;
    /// Original filename present.
    pub const FNAME: u8 = 0x08;
    /// Comment present.
    pub const FCOMMENT: u8 = 0x10;
}

/// GZIP file header.
#[derive(Debug, Clone)]
pub struct GzipHeader {
    /// Compression method (should be 8 for DEFLATE).
    pub method: u8,
    /// Flags.
    pub flags: u8,
    /// Modification time (Unix timestamp).
    pub mtime: u32,
    /// Extra flags.
    pub xfl: u8,
    /// Operating system.
    pub os: u8,
    /// Original filename (if FNAME flag set).
    pub filename: Option<String>,
    /// Comment (if FCOMMENT flag set).
    pub comment: Option<String>,
    /// Header CRC16 (if FHCRC flag set).
    pub header_crc: Option<u16>,
}

impl Default for GzipHeader {
    fn default() -> Self {
        Self {
            method: CM_DEFLATE,
            flags: 0,
            mtime: 0,
            xfl: 0,
            os: 255, // Unknown OS
            filename: None,
            comment: None,
            header_crc: None,
        }
    }
}

impl GzipHeader {
    /// Create a new GZIP header with default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a header with filename.
    pub fn with_filename(filename: &str) -> Self {
        Self {
            flags: flags::FNAME,
            filename: Some(filename.to_string()),
            ..Self::default()
        }
    }

    /// Set the modification time to now.
    pub fn with_mtime_now(mut self) -> Self {
        self.mtime = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        self
    }

    /// Write the header to a writer.
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        // Magic
        writer.write_all(&GZIP_MAGIC)?;

        // Method
        writer.write_all(&[self.method])?;

        // Flags
        writer.write_all(&[self.flags])?;

        // Modification time
        writer.write_all(&self.mtime.to_le_bytes())?;

        // XFL and OS
        writer.write_all(&[self.xfl, self.os])?;

        // Filename
        if self.flags & flags::FNAME != 0 {
            if let Some(ref filename) = self.filename {
                writer.write_all(filename.as_bytes())?;
                writer.write_all(&[0])?; // Null terminator
            }
        }

        // Comment
        if self.flags & flags::FCOMMENT != 0 {
            if let Some(ref comment) = self.comment {
                writer.write_all(comment.as_bytes())?;
                writer.write_all(&[0])?; // Null terminator
            }
        }

        Ok(())
    }

    /// Read a GZIP header from a reader.
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 10];
        reader.read_exact(&mut buf)?;

        // Check magic
        if buf[0..2] != GZIP_MAGIC {
            return Err(OxiArcError::invalid_magic(
                GZIP_MAGIC.to_vec(),
                buf[0..2].to_vec(),
            ));
        }

        let method = buf[2];
        if method != CM_DEFLATE {
            return Err(OxiArcError::unsupported_method(format!(
                "GZIP method {}",
                method
            )));
        }

        let flags = buf[3];
        let mtime = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let xfl = buf[8];
        let os = buf[9];

        // Read optional fields
        let mut filename = None;
        let mut comment = None;
        let mut header_crc = None;

        // Extra field
        if flags & flags::FEXTRA != 0 {
            let mut xlen_buf = [0u8; 2];
            reader.read_exact(&mut xlen_buf)?;
            let xlen = u16::from_le_bytes(xlen_buf) as usize;
            let mut extra = vec![0u8; xlen];
            reader.read_exact(&mut extra)?;
        }

        // Filename
        if flags & flags::FNAME != 0 {
            filename = Some(Self::read_null_terminated(reader)?);
        }

        // Comment
        if flags & flags::FCOMMENT != 0 {
            comment = Some(Self::read_null_terminated(reader)?);
        }

        // Header CRC
        if flags & flags::FHCRC != 0 {
            let mut crc_buf = [0u8; 2];
            reader.read_exact(&mut crc_buf)?;
            header_crc = Some(u16::from_le_bytes(crc_buf));
        }

        Ok(Self {
            method,
            flags,
            mtime,
            xfl,
            os,
            filename,
            comment,
            header_crc,
        })
    }

    /// Read a null-terminated string.
    fn read_null_terminated<R: Read>(reader: &mut R) -> Result<String> {
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1];

        loop {
            reader.read_exact(&mut buf)?;
            if buf[0] == 0 {
                break;
            }
            bytes.push(buf[0]);
        }

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// GZIP reader that decompresses data.
pub struct GzipReader<R: Read> {
    /// Underlying reader.
    reader: R,
    /// Parsed header.
    header: GzipHeader,
}

impl<R: Read> GzipReader<R> {
    /// Create a new GZIP reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let header = GzipHeader::read(&mut reader)?;
        Ok(Self { reader, header })
    }

    /// Get the header.
    pub fn header(&self) -> &GzipHeader {
        &self.header
    }

    /// Decompress the data.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        // Read all remaining data (compressed + trailer)
        let mut compressed = Vec::new();
        self.reader.read_to_end(&mut compressed)?;

        if compressed.len() < 8 {
            return Err(OxiArcError::unexpected_eof(8));
        }

        // Trailer is last 8 bytes
        let trailer = &compressed[compressed.len() - 8..];
        let expected_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let expected_size =
            u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]) as usize;

        // Decompress (without trailer)
        let deflate_data = &compressed[..compressed.len() - 8];
        let decompressed = inflate(deflate_data)?;

        // Verify CRC
        let actual_crc = Crc32::compute(&decompressed);
        if actual_crc != expected_crc {
            return Err(OxiArcError::crc_mismatch(expected_crc, actual_crc));
        }

        // Verify size
        if decompressed.len() != expected_size {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "Size mismatch: expected {}, got {}",
                    expected_size,
                    decompressed.len()
                ),
            ));
        }

        Ok(decompressed)
    }
}

/// GZIP writer that compresses data.
pub struct GzipWriter {
    /// Header to use.
    header: GzipHeader,
    /// Compression level (0-9).
    level: u8,
}

impl GzipWriter {
    /// Create a new GZIP writer with default settings.
    pub fn new() -> Self {
        Self {
            header: GzipHeader::new(),
            level: 6,
        }
    }

    /// Create a writer with a specific header.
    pub fn with_header(header: GzipHeader) -> Self {
        Self { header, level: 6 }
    }

    /// Set compression level (0-9).
    pub fn level(mut self, level: u8) -> Self {
        self.level = level.min(9);
        // Set XFL based on level
        self.header.xfl = match self.level {
            0..=1 => 4, // Fastest
            9 => 2,     // Maximum compression
            _ => 0,     // Default
        };
        self
    }

    /// Compress data and write to a writer.
    pub fn compress<W: Write>(&self, data: &[u8], writer: &mut W) -> Result<()> {
        // Write header
        self.header.write(writer)?;

        // Compress with DEFLATE
        let compressed = deflate(data, self.level)?;
        writer.write_all(&compressed)?;

        // Write trailer (CRC32 + ISIZE)
        let crc = Crc32::compute(data);
        writer.write_all(&crc.to_le_bytes())?;

        let isize = (data.len() as u32).to_le_bytes();
        writer.write_all(&isize)?;

        Ok(())
    }

    /// Compress data and return as Vec.
    pub fn compress_to_vec(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.compress(data, &mut output)?;
        Ok(output)
    }
}

impl Default for GzipWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Compress data to GZIP format.
pub fn compress(data: &[u8], level: u8) -> Result<Vec<u8>> {
    GzipWriter::new().level(level).compress_to_vec(data)
}

/// Compress data to GZIP format with filename.
pub fn compress_with_filename(data: &[u8], filename: &str, level: u8) -> Result<Vec<u8>> {
    let header = GzipHeader::with_filename(filename).with_mtime_now();
    GzipWriter::with_header(header)
        .level(level)
        .compress_to_vec(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_gzip_magic() {
        assert_eq!(GZIP_MAGIC, [0x1F, 0x8B]);
    }

    #[test]
    fn test_gzip_header_default() {
        let header = GzipHeader::new();
        assert_eq!(header.method, CM_DEFLATE);
        assert_eq!(header.flags, 0);
    }

    #[test]
    fn test_gzip_header_with_filename() {
        let header = GzipHeader::with_filename("test.txt");
        assert_eq!(header.flags & flags::FNAME, flags::FNAME);
        assert_eq!(header.filename, Some("test.txt".to_string()));
    }

    #[test]
    fn test_gzip_roundtrip() {
        let original = b"Hello, GZIP World! This is a test of compression.";

        // Compress
        let compressed = compress(original, 6).unwrap();

        // Decompress
        let mut reader = GzipReader::new(Cursor::new(compressed)).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_roundtrip_with_filename() {
        let original = b"Test data with filename";

        // Compress with filename
        let compressed = compress_with_filename(original, "data.txt", 6).unwrap();

        // Decompress and check filename
        let mut reader = GzipReader::new(Cursor::new(compressed)).unwrap();
        assert_eq!(reader.header().filename, Some("data.txt".to_string()));

        let decompressed = reader.decompress().unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_empty() {
        let original: &[u8] = b"";
        let compressed = compress(original, 6).unwrap();

        let mut reader = GzipReader::new(Cursor::new(compressed)).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_gzip_repeated() {
        let original = vec![b'A'; 10000];
        let compressed = compress(&original, 9).unwrap();

        // Should compress well
        assert!(compressed.len() < original.len() / 10);

        let mut reader = GzipReader::new(Cursor::new(compressed)).unwrap();
        let decompressed = reader.decompress().unwrap();

        assert_eq!(decompressed, original);
    }
}

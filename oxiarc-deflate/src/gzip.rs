//! Gzip wrapper module for DEFLATE compression.
//!
//! Implements the gzip file format as specified in RFC 1952.
//! A gzip stream consists of:
//! - A 10-byte fixed header
//! - DEFLATE-compressed data
//! - A CRC-32 checksum and original input size (ISIZE) trailer
//!
//! # Example
//!
//! ```rust
//! use oxiarc_deflate::gzip::{gzip_compress, gzip_decompress};
//!
//! let original = b"Hello, gzip world!";
//! let compressed = gzip_compress(original, 6).unwrap();
//! let decompressed = gzip_decompress(&compressed).unwrap();
//! assert_eq!(&decompressed, original);
//! ```

use crate::deflate::Deflater;
use crate::inflate::inflate;
use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};

/// Gzip magic bytes.
const GZIP_ID1: u8 = 0x1f;
const GZIP_ID2: u8 = 0x8b;

/// Compression method: deflate.
const GZIP_CM_DEFLATE: u8 = 8;

/// Gzip header flags byte (no extra fields).
const GZIP_FLG_NONE: u8 = 0;

/// OS byte: unknown (255).
const GZIP_OS_UNKNOWN: u8 = 255;

/// Minimum gzip stream size: 10-byte header + 2-byte empty deflate + 8-byte trailer.
const GZIP_MIN_SIZE: usize = 18;

/// Gzip encoder that wraps DEFLATE with the gzip framing.
pub struct GzipEncoder {
    deflater: Deflater,
}

impl GzipEncoder {
    /// Create a new gzip encoder at the given compression level (0–9).
    pub fn new(level: u8) -> Self {
        Self {
            deflater: Deflater::new(level),
        }
    }

    /// Compress `data` into a complete gzip stream returned as a `Vec<u8>`.
    pub fn compress(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        // --- 10-byte gzip header ---
        // ID1, ID2, CM=8(deflate), FLG=0, MTIME=0(4 bytes LE), XFL=0, OS=255(unknown)
        let mut output = vec![
            GZIP_ID1,
            GZIP_ID2,
            GZIP_CM_DEFLATE,
            GZIP_FLG_NONE,
            0u8,
            0u8,
            0u8,
            0u8, // MTIME = 0 (4 bytes, little-endian)
            0u8, // XFL = 0
            GZIP_OS_UNKNOWN,
        ];

        // --- DEFLATE compressed data ---
        self.deflater.deflate(data, &mut output, true)?;

        // --- Trailer: CRC32 (4 bytes LE) + ISIZE (4 bytes LE) ---
        let crc = Crc32::compute(data);
        output.extend_from_slice(&crc.to_le_bytes());

        // ISIZE is the input size modulo 2^32
        let isize_val = (data.len() as u64 & 0xFFFF_FFFF) as u32;
        output.extend_from_slice(&isize_val.to_le_bytes());

        Ok(output)
    }
}

/// Gzip decoder that strips the gzip framing and decompresses with DEFLATE.
pub struct GzipDecoder;

impl GzipDecoder {
    /// Create a new gzip decoder.
    pub fn new() -> Self {
        Self
    }

    /// Decompress a gzip stream from `data`, returning the original bytes.
    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < GZIP_MIN_SIZE {
            return Err(OxiArcError::InvalidHeader {
                message: "gzip stream too short".to_owned(),
            });
        }

        // Validate magic bytes and CM.
        if data[0] != GZIP_ID1 || data[1] != GZIP_ID2 {
            return Err(OxiArcError::InvalidMagic {
                expected: vec![GZIP_ID1, GZIP_ID2],
                found: vec![data[0], data[1]],
            });
        }
        if data[2] != GZIP_CM_DEFLATE {
            return Err(OxiArcError::UnsupportedMethod {
                method: format!("gzip CM={}", data[2]),
            });
        }

        let flg = data[3];

        // Parse past the fixed 10-byte header.
        let mut pos: usize = 10;

        // FLG bit 2: FEXTRA — skip extra field.
        if flg & 0x04 != 0 {
            if pos + 2 > data.len() {
                return Err(OxiArcError::InvalidHeader {
                    message: "gzip FEXTRA truncated".to_owned(),
                });
            }
            let xlen = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + xlen > data.len() {
                return Err(OxiArcError::InvalidHeader {
                    message: "gzip FEXTRA data truncated".to_owned(),
                });
            }
            pos += xlen;
        }

        // FLG bit 3: FNAME — skip null-terminated original file name.
        if flg & 0x08 != 0 {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            // Skip the null terminator.
            if pos < data.len() {
                pos += 1;
            }
        }

        // FLG bit 4: FCOMMENT — skip null-terminated comment.
        if flg & 0x10 != 0 {
            while pos < data.len() && data[pos] != 0 {
                pos += 1;
            }
            if pos < data.len() {
                pos += 1;
            }
        }

        // FLG bit 1: FHCRC — skip 2-byte header CRC16.
        if flg & 0x02 != 0 {
            if pos + 2 > data.len() {
                return Err(OxiArcError::InvalidHeader {
                    message: "gzip FHCRC truncated".to_owned(),
                });
            }
            pos += 2;
        }

        // The trailer occupies the last 8 bytes.
        if pos + 8 > data.len() {
            return Err(OxiArcError::InvalidHeader {
                message: "gzip stream missing trailer".to_owned(),
            });
        }

        let deflate_end = data.len() - 8;
        let compressed_data = &data[pos..deflate_end];

        // Inflate the DEFLATE payload.
        let decompressed = inflate(compressed_data)?;

        // Read and verify the trailer.
        let trailer = &data[deflate_end..];
        let stored_crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let stored_isize = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);

        let actual_crc = Crc32::compute(&decompressed);
        if actual_crc != stored_crc {
            return Err(OxiArcError::CrcMismatch {
                expected: stored_crc,
                computed: actual_crc,
            });
        }

        let actual_isize = (decompressed.len() as u64 & 0xFFFF_FFFF) as u32;
        if actual_isize != stored_isize {
            return Err(OxiArcError::InvalidHeader {
                message: format!(
                    "gzip ISIZE mismatch: expected {}, got {}",
                    stored_isize, actual_isize
                ),
            });
        }

        Ok(decompressed)
    }
}

impl Default for GzipDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Compress `data` with gzip at the given level (0–9).
///
/// # Example
///
/// ```rust
/// use oxiarc_deflate::gzip::gzip_compress;
///
/// let compressed = gzip_compress(b"hello world", 6).unwrap();
/// assert!(compressed.starts_with(&[0x1f, 0x8b]));
/// ```
pub fn gzip_compress(data: &[u8], level: u8) -> Result<Vec<u8>> {
    GzipEncoder::new(level).compress(data)
}

/// Decompress a gzip stream, returning the original bytes.
///
/// # Example
///
/// ```rust
/// use oxiarc_deflate::gzip::{gzip_compress, gzip_decompress};
///
/// let original = b"hello world";
/// let compressed = gzip_compress(original, 6).unwrap();
/// let decompressed = gzip_decompress(&compressed).unwrap();
/// assert_eq!(&decompressed, original);
/// ```
pub fn gzip_decompress(data: &[u8]) -> Result<Vec<u8>> {
    GzipDecoder::new().decompress(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gzip_roundtrip() {
        let original = b"Hello, gzip! Hello, gzip! Hello, gzip!";
        let compressed = gzip_compress(original, 6).expect("compress failed");
        let decompressed = gzip_decompress(&compressed).expect("decompress failed");
        assert_eq!(&decompressed, original);
    }

    #[test]
    fn test_gzip_roundtrip_empty() {
        let original: &[u8] = b"";
        let compressed = gzip_compress(original, 6).expect("compress failed");
        let decompressed = gzip_decompress(&compressed).expect("decompress failed");
        assert_eq!(&decompressed, original);
    }

    #[test]
    fn test_gzip_roundtrip_all_levels() {
        let original = b"AAAAAAAAAAAAAAAAAABBBBBBBBBBBBBBBBCCCCCCCCCCCCCCCC";
        for level in 0u8..=9 {
            let compressed = gzip_compress(original, level).expect("compress failed");
            // Header magic
            assert_eq!(compressed[0], 0x1f, "bad ID1 at level {}", level);
            assert_eq!(compressed[1], 0x8b, "bad ID2 at level {}", level);
            let decompressed = gzip_decompress(&compressed).expect("decompress failed");
            assert_eq!(
                &decompressed, original,
                "roundtrip failed at level {}",
                level
            );
        }
    }

    #[test]
    fn test_gzip_bad_magic() {
        let bad_data = b"\x00\x00\x08\x00\x00\x00\x00\x00\x00\xff\x00\x00\x00\x00\x00\x00\x00\x00";
        assert!(gzip_decompress(bad_data).is_err());
    }

    #[test]
    fn test_gzip_too_short() {
        assert!(gzip_decompress(b"\x1f\x8b").is_err());
    }

    #[test]
    fn test_gzip_header_bytes() {
        let compressed = gzip_compress(b"test", 1).expect("compress failed");
        assert_eq!(compressed[0], GZIP_ID1);
        assert_eq!(compressed[1], GZIP_ID2);
        assert_eq!(compressed[2], GZIP_CM_DEFLATE);
        assert_eq!(compressed[3], GZIP_FLG_NONE);
        // MTIME = 0
        assert_eq!(&compressed[4..8], &[0u8; 4]);
        // OS = 255
        assert_eq!(compressed[9], GZIP_OS_UNKNOWN);
    }
}

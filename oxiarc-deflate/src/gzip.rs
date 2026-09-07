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
//! let compressed = gzip_compress(original, 6).expect("gzip_compress");
//! let decompressed = gzip_decompress(&compressed).expect("gzip_decompress");
//! assert_eq!(&decompressed, original);
//! ```

use crate::deflate::Deflater;
use crate::inflate::MAX_OUTPUT_CAPACITY_HINT;
use crate::reader::STAGING_BUFFER;
use crate::stream::InflateStatus;
use crate::wrapper::{InflateWrapper, TrailingPolicy, WrappedInflate};
use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::FlushMode;

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
    ///
    /// Handles concatenated multi-member streams per RFC 1952 §2.2: after
    /// each member's trailer, another member may follow and all members'
    /// decompressed contents are concatenated. This is the format produced
    /// by `gzip -c a b`, `pigz`, and [`crate::parallel::compress_gzip_parallel`].
    ///
    /// Trailing zero padding after the final member is tolerated (as in the
    /// `gzip` CLI and Python's `gzip` module, e.g. for tape-block padding);
    /// any other trailing garbage is an error.
    ///
    /// Since 0.4.2 the `FHCRC` header checksum (`FLG` bit 1), previously
    /// skipped, is verified when present.
    ///
    /// # Errors
    ///
    /// [`OxiArcError::InvalidHeader`] for a stream shorter than a minimal
    /// member or a malformed header, [`OxiArcError::InvalidMagic`] for
    /// non-zero trailing garbage, [`OxiArcError::CrcMismatch`] for a bad
    /// CRC-32 or `FHCRC`, and [`OxiArcError::UnexpectedEof`] for a member
    /// (including a later one) that ends early.
    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < GZIP_MIN_SIZE {
            return Err(OxiArcError::InvalidHeader {
                message: "gzip stream too short".to_owned(),
            });
        }

        // Pre-size from the trailing ISIZE field. The value is
        // attacker-controlled, so it is only ever a capacity hint: it is
        // clamped, and the buffer still grows normally if the stream
        // decodes to more. For a multi-member stream this is the *last*
        // member's ISIZE, still a far better start than nothing.
        let size_hint = data
            .len()
            .checked_sub(4)
            .and_then(|t| data.get(t..))
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map_or(0, |b| u32::from_le_bytes(b) as usize);
        let mut output = Vec::with_capacity(size_hint.min(MAX_OUTPUT_CAPACITY_HINT));

        let mut core = WrappedInflate::new(InflateWrapper::Gzip)
            .multi_member(true)
            .trailing_policy(TrailingPolicy::AllowZeros);
        let mut scratch = vec![0u8; STAGING_BUFFER];
        let mut pos = 0usize;
        loop {
            // The whole stream is in hand, so `Finish`: a member that ends
            // early is an error rather than a request for more input.
            let progress = core.inflate(
                data.get(pos..).unwrap_or_default(),
                &mut scratch,
                FlushMode::Finish,
            )?;
            pos = pos.saturating_add(progress.consumed);
            if let Some(fresh) = scratch.get(..progress.produced) {
                output.extend_from_slice(fresh);
            }
            if progress.status == InflateStatus::StreamEnd {
                break;
            }
            if progress.consumed == 0 && progress.produced == 0 {
                return Err(OxiArcError::corrupted(
                    output.len() as u64,
                    "gzip decoder made no progress",
                ));
            }
        }

        Ok(output)
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
/// let compressed = gzip_compress(b"hello world", 6).expect("gzip_compress");
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
/// let compressed = gzip_compress(original, 6).expect("gzip_compress");
/// let decompressed = gzip_decompress(&compressed).expect("gzip_decompress");
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

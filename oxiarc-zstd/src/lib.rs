//! # OxiArc Zstandard
//!
//! Pure Rust implementation of the Zstandard (zstd) compression format (RFC 8878).
//!
//! Zstandard is a modern, fast compression algorithm providing excellent compression
//! ratios. This implementation provides decompression and basic compression support.
//!
//! ## Features
//!
//! - Complete Zstandard frame parsing
//! - FSE (Finite State Entropy) decoding
//! - Huffman decoding for literals
//! - Raw block compression (valid Zstd output)
//! - XXH64 checksum verification
//!
//! ## Example
//!
//! ```rust,no_run
//! use oxiarc_zstd::{compress, decompress};
//!
//! let data = b"Hello, Zstandard!";
//! let compressed = compress(data).unwrap();
//! let decompressed = decompress(&compressed).unwrap();
//! assert_eq!(decompressed, data);
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

mod encode;
mod frame;
mod fse;
mod huffman;
mod literals;
mod sequences;
mod xxhash;

pub use encode::{CompressionStrategy, ZstdEncoder, compress, compress_no_checksum};
pub use frame::{ZstdDecoder, decompress};

use oxiarc_core::error::{OxiArcError, Result};

/// Zstandard magic number (0xFD2FB528 little-endian).
pub const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Skippable frame magic number range start (0x184D2A50).
pub const SKIPPABLE_MAGIC_LOW: u32 = 0x184D2A50;

/// Skippable frame magic number range end (0x184D2A5F).
pub const SKIPPABLE_MAGIC_HIGH: u32 = 0x184D2A5F;

/// Maximum window size (8 MB default, 2 GB max per spec).
pub const MAX_WINDOW_SIZE: usize = 8 * 1024 * 1024;

/// Maximum block size (128 KB).
pub const MAX_BLOCK_SIZE: usize = 128 * 1024;

/// Block types in Zstandard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockType {
    /// Raw uncompressed block.
    Raw,
    /// RLE block (single byte repeated).
    Rle,
    /// Compressed block with literals and sequences.
    Compressed,
    /// Reserved (invalid).
    Reserved,
}

impl BlockType {
    /// Create block type from 2-bit value.
    pub fn from_bits(bits: u8) -> Result<Self> {
        match bits & 0x03 {
            0 => Ok(BlockType::Raw),
            1 => Ok(BlockType::Rle),
            2 => Ok(BlockType::Compressed),
            3 => Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "reserved block type".to_string(),
            }),
            _ => unreachable!(),
        }
    }
}

/// Literals block type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiteralsBlockType {
    /// Raw literals (uncompressed).
    Raw,
    /// RLE literals (single byte).
    Rle,
    /// Compressed with Huffman, tree included.
    Compressed,
    /// Compressed with Huffman, uses previous tree.
    Treeless,
}

impl LiteralsBlockType {
    /// Create from 2-bit value.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => LiteralsBlockType::Raw,
            1 => LiteralsBlockType::Rle,
            2 => LiteralsBlockType::Compressed,
            3 => LiteralsBlockType::Treeless,
            _ => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_block_type_from_bits() {
        assert_eq!(BlockType::from_bits(0).unwrap(), BlockType::Raw);
        assert_eq!(BlockType::from_bits(1).unwrap(), BlockType::Rle);
        assert_eq!(BlockType::from_bits(2).unwrap(), BlockType::Compressed);
        assert!(BlockType::from_bits(3).is_err());
    }

    #[test]
    fn test_literals_block_type() {
        assert_eq!(LiteralsBlockType::from_bits(0), LiteralsBlockType::Raw);
        assert_eq!(LiteralsBlockType::from_bits(1), LiteralsBlockType::Rle);
        assert_eq!(
            LiteralsBlockType::from_bits(2),
            LiteralsBlockType::Compressed
        );
        assert_eq!(LiteralsBlockType::from_bits(3), LiteralsBlockType::Treeless);
    }

    #[test]
    fn test_zstd_magic() {
        // 0xFD2FB528 in little-endian
        assert_eq!(u32::from_le_bytes(ZSTD_MAGIC), 0xFD2FB528);
    }
}

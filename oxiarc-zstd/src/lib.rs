//! # OxiArc Zstandard
//!
//! Pure Rust implementation of the Zstandard (zstd) compression format (RFC 8878).
//!
//! Zstandard is a modern, fast compression algorithm providing excellent compression
//! ratios. This implementation provides full compression and decompression support.
//!
//! ## Features
//!
//! - Full LZ77 + Huffman + FSE compression (levels 1-22)
//! - Complete Zstandard frame parsing and decompression
//! - FSE (Finite State Entropy) encoding and decoding
//! - Huffman encoding and decoding for literals
//! - Dictionary-based compression for small data
//! - Streaming Write/Read API
//! - XXH64 checksum verification
//! - Optional parallel compression
//!
//! ## Example
//!
//! ```rust,no_run
//! use oxiarc_zstd::{compress_with_level, decompress, encode_all, decode_all};
//!
//! // Buffer-based compression with level
//! let data = b"Hello, Zstandard!";
//! let compressed = compress_with_level(data, 3).unwrap();
//! let decompressed = decompress(&compressed).unwrap();
//! assert_eq!(decompressed, data);
//!
//! // Convenience functions (zstd crate compatible pattern)
//! let compressed = encode_all(data, 3).unwrap();
//! let decompressed = decode_all(&compressed).unwrap();
//! assert_eq!(decompressed, data);
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

mod bitwriter;
mod compressed_block;
/// Dictionary support for improved compression of small data.
pub mod dict;
mod encode;
mod frame;
mod fse;
#[allow(dead_code)]
mod fse_encoder;
mod huffman;
#[allow(dead_code)]
mod huffman_encoder;
mod literals;
mod lz77;
mod sequences;
/// Streaming compression and decompression.
pub mod streaming;
mod xxhash;

// Primary compression API
pub use encode::{
    CompressionStrategy, ZstdEncoder, compress, compress_no_checksum, compress_with_level,
    decode_all, encode_all,
};

// Decompression API
pub use frame::{
    ZstdDecoder, decompress, decompress_frame, decompress_multi_frame, decompress_with_dict,
    write_skippable_frame,
};

// Streaming API
pub use streaming::{ZstdStreamDecoder, ZstdStreamEncoder};

/// Alias for [`ZstdStreamEncoder`] — a streaming writer that emits incremental
/// Zstandard frames.
pub type ZstdWriter<W> = ZstdStreamEncoder<W>;

// Dictionary API
pub use dict::{ZstdDict, train_dictionary};

// Advanced: LZ77 types (for users who want fine-grained control)
pub use lz77::{LevelConfig, Lz77Sequence, MatchFinder};

// Advanced: Bitstream writers (for custom encoding)
pub use bitwriter::{BackwardBitWriter, ForwardBitWriter};

#[cfg(feature = "parallel")]
pub use encode::compress_parallel;

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
    fn test_multi_frame_decompress() {
        let frame1 = compress_with_level(b"Hello ", 3).unwrap();
        let frame2 = compress_with_level(b"World!", 3).unwrap();
        let mut combined = frame1;
        combined.extend_from_slice(&frame2);
        let result = decompress_multi_frame(&combined).unwrap();
        assert_eq!(result, b"Hello World!");
    }

    #[test]
    fn test_skippable_frame_ignored() {
        let skip = write_skippable_frame(b"metadata", 0);
        let frame = compress_with_level(b"data", 3).unwrap();
        let mut combined = skip;
        combined.extend_from_slice(&frame);
        let result = decompress_multi_frame(&combined).unwrap();
        assert_eq!(result, b"data");
    }

    #[test]
    fn test_incremental_streaming_writer() {
        use std::io::Write;
        let mut buf = Vec::new();
        let mut writer = ZstdWriter::new(&mut buf, 3);
        // Write in small chunks.
        for chunk in b"Hello World! ".chunks(3) {
            writer.write_all(chunk).unwrap();
        }
        writer.finish().unwrap();
        let decompressed = decompress_multi_frame(&buf).unwrap();
        assert_eq!(decompressed, b"Hello World! ");
    }

    #[test]
    fn test_decompress_frame_returns_consumed() {
        let frame1 = compress_with_level(b"abc", 1).unwrap();
        let frame2 = compress_with_level(b"xyz", 1).unwrap();
        let mut combined = frame1.clone();
        combined.extend_from_slice(&frame2);
        let (data, consumed) = decompress_frame(&combined).unwrap();
        assert_eq!(data, b"abc");
        assert_eq!(consumed, frame1.len());
    }

    #[test]
    fn test_skippable_frame_magic_nibble() {
        for nibble in 0u8..=15 {
            let frame = write_skippable_frame(b"test", nibble);
            let magic = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
            assert!((SKIPPABLE_MAGIC_LOW..=SKIPPABLE_MAGIC_HIGH).contains(&magic));
        }
    }

    #[test]
    fn test_multi_frame_empty_input() {
        let result = decompress_multi_frame(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_multi_frame_skippable_only() {
        let skip = write_skippable_frame(b"some metadata", 3);
        let result = decompress_multi_frame(&skip).unwrap();
        assert!(result.is_empty());
    }

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
        assert_eq!(u32::from_le_bytes(ZSTD_MAGIC), 0xFD2FB528);
    }
}

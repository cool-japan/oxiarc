//! # OxiArc Zstandard
//!
//! Pure Rust implementation of the Zstandard (zstd) compression format (RFC 8878).
//!
//! Zstandard is a modern, fast compression algorithm providing excellent compression
//! ratios. This implementation provides full compression and decompression support.
//!
//! ## Features
//!
//! - LZ77 match-finding with entropy-coded sequences (levels 1-22)
//! - Complete Zstandard frame parsing and decompression, validated
//!   byte-for-byte against frames produced by the reference `zstd` CLI
//!   (all levels, `--ultra -22`, `--long`, `--no-check`,
//!   `--no-content-size`, raw-content dictionaries, multi-frame streams)
//! - FSE (Finite State Entropy) sequence coding using the RFC 8878
//!   predefined tables (and RLE tables for constant symbol categories);
//!   the decoder additionally handles custom `FSE_Compressed` tables
//! - Huffman literals in both directions: 1- and 4-stream decoding, and
//!   Huffman-compressed literal sections on the encode path (self-verified,
//!   with Raw/RLE fallback)
//! - Encoder output is accepted by the reference `zstd` CLI; the live
//!   differential gate lives behind the `zstd-oracle` cargo feature
//! - Raw-content dictionary compression for small data (interoperable with
//!   `zstd -D` in both directions)
//! - Streaming Write/Read API
//! - XXH64 checksum verification
//! - Optional parallel compression
//!
//! Ratio note: sequences always use the predefined/RLE FSE tables (custom
//! block-optimal tables are not emitted yet), so compression ratio on some
//! inputs trails the reference encoder even though every frame is fully
//! interoperable.
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_zstd::{compress_with_level, decompress, encode_all, decode_all};
//!
//! // Buffer-based compression with level
//! let data = b"Hello, Zstandard!";
//! let compressed = compress_with_level(data, 3).expect("compression failed");
//! let decompressed = decompress(&compressed).expect("decompression failed");
//! assert_eq!(decompressed, data);
//!
//! // Convenience functions (zstd crate compatible pattern)
//! let compressed = encode_all(data, 3).expect("encode_all failed");
//! let decompressed = decode_all(&compressed).expect("decode_all failed");
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
// Custom (non-predefined) FSE table *compression* for sequences is not
// wired into the encoder yet: sequences use the RFC 8878 predefined/RLE
// tables, which reference decoders accept. This module stays dormant until
// FSE_Compressed sequence modes are emitted.
#[allow(dead_code)]
mod fse_encoder;
mod huffman;
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
    ZstdDecoder, decompress, decompress_frame, decompress_multi_frame,
    decompress_multi_frame_with_dict, decompress_with_dict, write_skippable_frame,
};

// Streaming API
pub use streaming::{ZstdStreamDecoder, ZstdStreamEncoder};

/// Alias for [`ZstdStreamEncoder`] — a streaming writer that emits incremental
/// Zstandard frames.
pub type ZstdWriter<W> = ZstdStreamEncoder<W>;

// Dictionary API
pub use dict::{ZstdDict, train_dictionary};

// API freeze (0.3.x): the LZ77 building blocks
// (`lz77::{LevelConfig, Lz77Sequence, MatchFinder}`) and the low-level bitstream
// writers (`bitwriter::{ForwardBitWriter, BackwardBitWriter}`) are NOT part of the
// stable public API. They are crate-internal implementation details whose shape
// may change without notice, so they are hidden from the documentation and are
// not covered by the crate's SemVer guarantees. They are re-exported as
// `#[doc(hidden)]` (rather than removed outright) only because some of their
// helper methods currently have no in-crate call sites; making them fully private
// would require crate-internal `#[allow(dead_code)]` annotations that live outside
// this module.
#[doc(hidden)]
pub use bitwriter::{BackwardBitWriter, ForwardBitWriter};
#[doc(hidden)]
pub use lz77::{LevelConfig, Lz77Sequence, MatchFinder};

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
#[non_exhaustive]
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
#[non_exhaustive]
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
        let frame1 = compress_with_level(b"Hello ", 3).expect("compression failed");
        let frame2 = compress_with_level(b"World!", 3).expect("compression failed");
        let mut combined = frame1;
        combined.extend_from_slice(&frame2);
        let result = decompress_multi_frame(&combined).expect("decompression failed");
        assert_eq!(result, b"Hello World!");
    }

    #[test]
    fn test_skippable_frame_ignored() {
        let skip = write_skippable_frame(b"metadata", 0);
        let frame = compress_with_level(b"data", 3).expect("compression failed");
        let mut combined = skip;
        combined.extend_from_slice(&frame);
        let result = decompress_multi_frame(&combined).expect("decompression failed");
        assert_eq!(result, b"data");
    }

    #[test]
    fn test_incremental_streaming_writer() {
        use std::io::Write;
        let mut buf = Vec::new();
        let mut writer = ZstdWriter::new(&mut buf, 3);
        // Write in small chunks.
        for chunk in b"Hello World! ".chunks(3) {
            writer.write_all(chunk).expect("write_all failed");
        }
        writer.finish().expect("finish failed");
        let decompressed = decompress_multi_frame(&buf).expect("decompression failed");
        assert_eq!(decompressed, b"Hello World! ");
    }

    #[test]
    fn test_decompress_frame_returns_consumed() {
        let frame1 = compress_with_level(b"abc", 1).expect("compression failed");
        let frame2 = compress_with_level(b"xyz", 1).expect("compression failed");
        let mut combined = frame1.clone();
        combined.extend_from_slice(&frame2);
        let (data, consumed) = decompress_frame(&combined).expect("decompression failed");
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
        let result = decompress_multi_frame(&[]).expect("decompression failed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_multi_frame_skippable_only() {
        let skip = write_skippable_frame(b"some metadata", 3);
        let result = decompress_multi_frame(&skip).expect("decompression failed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_block_type_from_bits() {
        assert_eq!(
            BlockType::from_bits(0).expect("valid block type"),
            BlockType::Raw
        );
        assert_eq!(
            BlockType::from_bits(1).expect("valid block type"),
            BlockType::Rle
        );
        assert_eq!(
            BlockType::from_bits(2).expect("valid block type"),
            BlockType::Compressed
        );
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

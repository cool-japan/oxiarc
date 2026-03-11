//! # OxiArc LZMA
//!
//! LZMA (Lempel-Ziv-Markov chain Algorithm) compression and decompression.
//!
//! LZMA is a lossless data compression algorithm that provides excellent
//! compression ratios. It's used in:
//! - 7-Zip archives (.7z)
//! - XZ compressed files (.xz)
//! - LZMA-compressed files (.lzma)
//! - Some ZIP archives (method 14)
//!
//! ## Features
//!
//! - **Pure Rust** implementation
//! - **Decompression** of LZMA streams
//! - **Compression** with configurable levels
//! - Range coder for entropy coding
//! - Probability-based context modeling
//!
//! ## Usage
//!
//! ### Decompression
//!
//! ```ignore
//! use oxiarc_lzma::decompress;
//!
//! let compressed = include_bytes!("data.lzma");
//! let decompressed = decompress(&compressed[..])?;
//! ```
//!
//! ### Compression
//!
//! ```ignore
//! use oxiarc_lzma::{compress, LzmaLevel};
//!
//! let data = b"Hello, World!";
//! let compressed = compress(data, LzmaLevel::DEFAULT)?;
//! ```
//!
//! ### LZMA2 Chunked Encoding (XZ compatible)
//!
//! ```ignore
//! use oxiarc_lzma::{encode_lzma2_chunked, decode_lzma2_chunked, LzmaLevel};
//!
//! let data = b"Hello, LZMA2 chunked world!";
//! let encoded = encode_lzma2_chunked(data, LzmaLevel::DEFAULT)?;
//! let decoded = decode_lzma2_chunked(&encoded, 1 << 20)?;
//! ```
//!
//! For custom chunk sizes, use `Lzma2ChunkedEncoder`:
//!
//! ```ignore
//! use oxiarc_lzma::{Lzma2ChunkedEncoder, Lzma2Config, LzmaLevel};
//!
//! let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(64 * 1024);
//! let mut encoder = Lzma2ChunkedEncoder::with_config(config);
//! let encoded = encoder.encode(data)?;
//! ```
//!
//! ## LZMA Format
//!
//! An LZMA stream consists of:
//! 1. Properties byte (lc, lp, pb encoded)
//! 2. Dictionary size (4 bytes, little-endian)
//! 3. Uncompressed size (8 bytes, little-endian, 0xFFFFFFFFFFFFFFFF = unknown)
//! 4. Compressed data
//!
//! The algorithm uses:
//! - LZ77-style dictionary compression with sliding window
//! - Range coding for entropy encoding
//! - Context-dependent probability models

#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod decoder;
pub mod encoder;
pub mod lzma2;
pub mod lzma2_chunk;
pub mod model;
pub mod optimal;
pub mod range_coder;

// Re-exports
pub use decoder::{LzmaDecoder, decompress, decompress_raw};
pub use encoder::{LzmaEncoder, compress, compress_raw};
pub use lzma2::{
    Lzma2Decoder, Lzma2Encoder, decode_lzma2, dict_size_from_props, encode_lzma2,
    props_from_dict_size,
};
pub use lzma2_chunk::{
    ChunkType, DEFAULT_CHUNK_SIZE, LZMA_CHUNK_MAX_COMPRESSED, LZMA_CHUNK_MAX_UNCOMPRESSED,
    Lzma2ChunkedEncoder, Lzma2Config, UNCOMPRESSED_CHUNK_MAX, control, decode_lzma2_chunked,
    encode_lzma2_chunked, encode_lzma2_with_config,
};
pub use model::{LzmaModel, LzmaProperties, State};
pub use range_coder::{RangeDecoder, RangeEncoder};

use oxiarc_core::error::Result;

/// LZMA compression level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzmaLevel(u8);

impl LzmaLevel {
    /// Fastest compression (level 0).
    pub const FAST: Self = Self(0);
    /// Default compression (level 6).
    pub const DEFAULT: Self = Self(6);
    /// Best compression (level 9).
    pub const BEST: Self = Self(9);

    /// Create a new compression level.
    pub fn new(level: u8) -> Self {
        Self(level.min(9))
    }

    /// Get the level value.
    pub fn level(&self) -> u8 {
        self.0
    }

    /// Get the dictionary size for this level.
    pub fn dict_size(&self) -> u32 {
        match self.0 {
            0 => 1 << 16, // 64 KB
            1 => 1 << 18, // 256 KB
            2 => 1 << 19, // 512 KB
            3 => 1 << 20, // 1 MB
            4 => 1 << 21, // 2 MB
            5 => 1 << 22, // 4 MB
            6 => 1 << 23, // 8 MB
            7 => 1 << 24, // 16 MB
            8 => 1 << 25, // 32 MB
            _ => 1 << 26, // 64 MB
        }
    }
}

impl Default for LzmaLevel {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Decompress LZMA data to a Vec.
///
/// This is a convenience wrapper around [`decompress`] that reads from a slice.
pub fn decompress_bytes(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Cursor;
    decompress(Cursor::new(data))
}

/// Compress data to a Vec using default settings.
///
/// This is a convenience wrapper around [`compress`] with default level.
pub fn compress_bytes(data: &[u8]) -> Result<Vec<u8>> {
    compress(data, LzmaLevel::DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_level() {
        assert_eq!(LzmaLevel::FAST.level(), 0);
        assert_eq!(LzmaLevel::DEFAULT.level(), 6);
        assert_eq!(LzmaLevel::BEST.level(), 9);
    }

    #[test]
    fn test_level_clamp() {
        assert_eq!(LzmaLevel::new(100).level(), 9);
    }

    #[test]
    fn test_dict_size() {
        assert_eq!(LzmaLevel::FAST.dict_size(), 1 << 16);
        assert_eq!(LzmaLevel::DEFAULT.dict_size(), 1 << 23);
        assert_eq!(LzmaLevel::BEST.dict_size(), 1 << 26);
    }

    #[test]
    fn test_properties_roundtrip() {
        let props = LzmaProperties::new(3, 0, 2);
        let byte = props.to_byte();
        let decoded = LzmaProperties::from_byte(byte).unwrap();

        assert_eq!(decoded.lc, 3);
        assert_eq!(decoded.lp, 0);
        assert_eq!(decoded.pb, 2);
    }

    #[test]
    fn test_compress_decompress_single_byte() {
        let original = b"A";
        let compressed = compress(original, LzmaLevel::DEFAULT).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compress_decompress_few_bytes() {
        let original = b"ABC";
        let compressed = compress(original, LzmaLevel::DEFAULT).unwrap();
        eprintln!(
            "Compressed {} bytes to {} bytes",
            original.len(),
            compressed.len()
        );
        eprintln!(
            "Compressed data: {:?}",
            &compressed[..compressed.len().min(30)]
        );
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compress_decompress_hello() {
        let original = b"Hello";
        let compressed = compress(original, LzmaLevel::DEFAULT).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compress_decompress_roundtrip() {
        let original = b"Hello, LZMA World! This is a test of compression and decompression.";
        let compressed = compress(original, LzmaLevel::DEFAULT).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compress_decompress_empty() {
        let original: &[u8] = b"";
        let compressed = compress(original, LzmaLevel::DEFAULT).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compress_decompress_repeated() {
        let original = vec![b'A'; 1000];
        let compressed = compress(&original, LzmaLevel::DEFAULT).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_compression_levels() {
        // Test various compression levels
        let data = b"Hello World! This is a test of LZMA compression with various levels.";

        for level in 0..=9 {
            let compressed = compress(data, LzmaLevel::new(level)).unwrap();
            let decompressed = decompress_bytes(&compressed).unwrap();
            assert_eq!(
                &decompressed[..],
                &data[..],
                "Level {} roundtrip failed",
                level
            );
        }
    }

    #[test]
    fn test_optimal_vs_greedy_parsing() {
        // Create test data with repetitive patterns that benefit from optimal parsing
        let mut data = Vec::new();
        for _ in 0..10 {
            data.extend_from_slice(b"The quick brown fox jumps over the lazy dog. ");
        }

        // Test greedy (level 6) vs optimal (level 9)
        let compressed_greedy = compress(&data, LzmaLevel::new(6)).unwrap();
        let compressed_optimal = compress(&data, LzmaLevel::new(9)).unwrap();

        // Both should decompress correctly
        let decompressed_greedy = decompress_bytes(&compressed_greedy).unwrap();
        let decompressed_optimal = decompress_bytes(&compressed_optimal).unwrap();

        assert_eq!(decompressed_greedy, data);
        assert_eq!(decompressed_optimal, data);

        eprintln!("Greedy size: {}", compressed_greedy.len());
        eprintln!("Optimal size: {}", compressed_optimal.len());
    }

    /// DP parser should produce smaller or equal compressed output vs greedy
    /// on highly repetitive data.
    #[test]
    fn test_dp_optimal_compression_ratio() {
        // 1200 bytes of highly repetitive data: repeating 6-byte pattern.
        let pattern = b"abcabc";
        let mut data = Vec::with_capacity(1200);
        while data.len() < 1200 {
            data.extend_from_slice(pattern);
        }
        data.truncate(1200);

        let compressed_greedy = compress(&data, LzmaLevel::new(6)).unwrap();
        let compressed_optimal = compress(&data, LzmaLevel::new(8)).unwrap();

        // Both must round-trip correctly
        let decompressed_greedy = decompress_bytes(&compressed_greedy).unwrap();
        let decompressed_optimal = decompress_bytes(&compressed_optimal).unwrap();
        assert_eq!(decompressed_greedy, data, "greedy roundtrip failed");
        assert_eq!(decompressed_optimal, data, "optimal roundtrip failed");

        // The DP optimal parser (level 8) must not produce larger output than greedy (level 6)
        assert!(
            compressed_optimal.len() <= compressed_greedy.len(),
            "DP optimal ({} bytes) should be <= greedy ({} bytes)",
            compressed_optimal.len(),
            compressed_greedy.len()
        );
    }

    /// Compress with full DP (level 8), decompress, verify identical bytes.
    #[test]
    fn test_dp_roundtrip_various_data() {
        // Test several different data shapes
        let test_cases: &[&[u8]] = &[
            // Completely repetitive
            &[0xAAu8; 2000],
            // Increasing bytes
            &{
                let v: Vec<u8> = (0..=255u8).cycle().take(500).collect();
                v
            }[..],
            // Mixed text-like data
            b"Hello, World! This is a test of the DP optimal parser at level 8. \
              The parser should find optimal matches across the 4096-byte window.",
            // Short data
            b"tiny",
        ];

        for (i, data) in test_cases.iter().enumerate() {
            let compressed = compress(data, LzmaLevel::new(8)).unwrap();
            let decompressed = decompress_bytes(&compressed).unwrap();
            assert_eq!(
                decompressed.as_slice(),
                *data,
                "DP roundtrip failed for test case {}",
                i
            );
        }
    }

    #[test]
    fn test_level_9_compression() {
        // Test level 9 (optimal parsing) specifically
        let original = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(20);
        let compressed = compress(&original, LzmaLevel::BEST).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_level_8_compression() {
        // Test level 8 (optimal parsing with different parameters)
        let original = b"Testing level 8 compression with optimal parsing enabled.".repeat(10);
        let compressed = compress(&original, LzmaLevel::new(8)).unwrap();
        let decompressed = decompress_bytes(&compressed).unwrap();
        assert_eq!(decompressed, original);
    }

    /// Test large data > 4095 bytes (multiple DP blocks) with optimal parsing.
    #[test]
    fn test_complex_data_large_optimal() {
        // Data that spans multiple DP blocks (each block is 4095 bytes)
        // cycling bytes: exercises every hash/match path
        let data: Vec<u8> = (0u8..=255).cycle().take(10000).collect();
        for level in [7u8, 8, 9] {
            let compressed = compress(&data, LzmaLevel::new(level)).unwrap();
            let decompressed = decompress_bytes(&compressed).unwrap();
            assert_eq!(
                decompressed, data,
                "Level {} roundtrip failed for cycling 10k data",
                level
            );
        }
    }

    /// LCG pseudo-random data stress test (exercises all code paths).
    #[test]
    fn test_complex_data_pseudorandom() {
        let mut data = Vec::with_capacity(10000);
        let mut x: u32 = 12345;
        for _ in 0..10000 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.push((x >> 24) as u8);
        }

        for level in [0u8, 3, 6, 7, 8, 9] {
            let compressed = compress(&data, LzmaLevel::new(level)).unwrap();
            let decompressed = decompress_bytes(&compressed).unwrap();
            assert_eq!(
                decompressed, data,
                "Level {} roundtrip failed for pseudorandom 10k data",
                level
            );
        }
    }

    /// Test data with varied byte values and local repetition (binary-like).
    #[test]
    fn test_complex_data_binary_patterns() {
        // Mix of runs, cycling, and unique bytes
        let mut data = Vec::new();
        // Runs of same byte
        for b in 0u8..=255 {
            data.extend(std::iter::repeat_n(b, 8));
        }
        // Cycling 256-byte ramp
        data.extend((0u8..=255).cycle().take(2048));
        // "Text-like" repeated phrase
        data.extend(
            b"The quick brown fox jumps over the lazy dog. "
                .iter()
                .cycle()
                .take(1000),
        );
        // High-entropy 4-byte repeating pattern
        let pat = [0xDE, 0xAD, 0xBE, 0xEF];
        data.extend(pat.iter().cycle().take(800));

        for level in [0u8, 6, 8, 9] {
            let compressed = compress(&data, LzmaLevel::new(level)).unwrap();
            let decompressed = decompress_bytes(&compressed).unwrap();
            assert_eq!(
                decompressed,
                data,
                "Level {} roundtrip failed for binary-pattern data (len={})",
                level,
                data.len()
            );
        }
    }

    /// Test data exactly at the DP block boundary (4095 bytes) and just over it.
    #[test]
    fn test_complex_data_block_boundary() {
        // Exactly 4095 bytes (one full DP block)
        let data_4095: Vec<u8> = (0u8..=254).cycle().take(4095).collect();
        // 4096 bytes: forces a block transition mid-stream
        let data_4096: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        // 8191 bytes: two full blocks exactly
        let data_8191: Vec<u8> = (0u8..=254).cycle().take(8191).collect();

        for (label, data) in [
            ("4095", data_4095.as_slice()),
            ("4096", data_4096.as_slice()),
            ("8191", data_8191.as_slice()),
        ] {
            for level in [7u8, 8, 9] {
                let compressed = compress(data, LzmaLevel::new(level)).unwrap();
                let decompressed = decompress_bytes(&compressed).unwrap();
                assert_eq!(
                    decompressed, data,
                    "Level {} roundtrip failed for {}-byte boundary data",
                    level, label
                );
            }
        }
    }

    /// Test data with rep-distance stress: long matches at rep slots 0-3.
    #[test]
    fn test_complex_data_rep_distance_stress() {
        // Pattern designed to exercise rep[0], rep[1], rep[2], rep[3] transitions
        // Interleaved identical segments separated by different bytes
        let seg_a = b"AAAAAAAAAAAAAAAA"; // 16 A's
        let seg_b = b"BBBBBBBBBBBBBBBB"; // 16 B's
        let sep = b"XYZ";
        let mut data = Vec::new();
        for _ in 0..50 {
            data.extend_from_slice(seg_a);
            data.extend_from_slice(sep);
            data.extend_from_slice(seg_b);
            data.extend_from_slice(sep);
            data.extend_from_slice(seg_a); // rep match should fire here
            data.extend_from_slice(seg_b); // rep match for rep[1]
        }
        for level in [6u8, 7, 8, 9] {
            let compressed = compress(&data, LzmaLevel::new(level)).unwrap();
            let decompressed = decompress_bytes(&compressed).unwrap();
            assert_eq!(
                decompressed, data,
                "Level {} roundtrip failed for rep-distance stress test",
                level
            );
        }
    }

    /// Stress test: all compression levels with all "complex" pattern types.
    #[test]
    fn test_complex_data_all_levels_all_patterns() {
        let patterns: &[(&str, Vec<u8>)] = &[
            ("all_zeros_100", vec![0u8; 100]),
            ("all_same_1000", vec![0x41u8; 1000]),
            ("cycling_256_1000", (0..=255u8).cycle().take(1000).collect()),
            (
                "text_repeat_100",
                b"The quick brown fox jumps over the lazy dog"
                    .iter()
                    .cycle()
                    .take(430)
                    .copied()
                    .collect(),
            ),
            ("binary_random_500", {
                let mut v = Vec::with_capacity(500);
                let mut x: u32 = 99991;
                for _ in 0..500 {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    v.push((x >> 24) as u8);
                }
                v
            }),
        ];

        for level in 0u8..=9 {
            for (name, data) in patterns {
                let compressed = compress(data, LzmaLevel::new(level)).unwrap();
                let decompressed = decompress_bytes(&compressed).unwrap();
                assert_eq!(
                    decompressed.as_slice(),
                    data.as_slice(),
                    "Level {} roundtrip failed for pattern '{}'",
                    level,
                    name
                );
            }
        }
    }
}

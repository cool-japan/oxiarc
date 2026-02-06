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

        // Note: Optimal parsing should generally produce smaller or equal output
        // but due to our simplified implementation, we just verify correctness
        eprintln!("Greedy size: {}", compressed_greedy.len());
        eprintln!("Optimal size: {}", compressed_optimal.len());
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
}

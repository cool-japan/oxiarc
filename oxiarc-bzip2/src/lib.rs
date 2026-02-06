//! BZip2 compression/decompression for OxiArc.
//!
//! This crate provides pure Rust implementation of BZip2 compression.
//!
//! BZip2 uses a pipeline of transformations:
//! 1. Run-Length Encoding (RLE) - Initial encoding of runs
//! 2. Burrows-Wheeler Transform (BWT) - Block sorting for better compression
//! 3. Move-to-Front Transform (MTF) - Locality transformation
//! 4. Zero-Run Length Encoding - Special encoding for zeros
//! 5. Huffman Coding - Final entropy coding

#![warn(missing_docs)]
#![warn(clippy::all)]

/// Burrows-Wheeler Transform implementation.
pub mod bwt;
mod decode;
mod encode;
mod huffman;
mod mtf;
mod rle;

pub use decode::{BzDecoder, decompress};
pub use encode::{BzEncoder, compress};

#[cfg(feature = "parallel")]
pub use encode::compress_parallel;

/// BZip2 magic bytes ("BZ").
pub const BZIP2_MAGIC: [u8; 2] = [0x42, 0x5A];

/// Block header magic bytes (0x314159265359).
pub const BLOCK_MAGIC: [u8; 6] = [0x31, 0x41, 0x59, 0x26, 0x53, 0x59];

/// End of stream magic bytes (0x177245385090).
pub const EOS_MAGIC: [u8; 6] = [0x17, 0x72, 0x45, 0x38, 0x50, 0x90];

/// Maximum block size (900k).
pub const MAX_BLOCK_SIZE: usize = 900_000;

/// Compression level (1-9, where 9 = 900k block size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionLevel(u8);

impl CompressionLevel {
    /// Create a new compression level (clamped to 1-9).
    pub fn new(level: u8) -> Self {
        Self(level.clamp(1, 9))
    }

    /// Get the block size for this level.
    pub fn block_size(&self) -> usize {
        self.0 as usize * 100_000
    }

    /// Get the level value.
    pub fn level(&self) -> u8 {
        self.0
    }
}

impl Default for CompressionLevel {
    fn default() -> Self {
        Self(9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_level() {
        let level = CompressionLevel::new(5);
        assert_eq!(level.level(), 5);
        assert_eq!(level.block_size(), 500_000);
    }

    #[test]
    fn test_compression_level_clamp() {
        assert_eq!(CompressionLevel::new(0).level(), 1);
        assert_eq!(CompressionLevel::new(10).level(), 9);
    }

    #[test]
    fn test_default_level() {
        let level = CompressionLevel::default();
        assert_eq!(level.level(), 9);
        assert_eq!(level.block_size(), 900_000);
    }

    #[test]
    fn test_roundtrip_hello() {
        // Start with a simpler test case - single character
        let original = b"a";
        let compressed = compress(original, CompressionLevel::new(1)).unwrap();
        eprintln!(
            "Compressed {} bytes to {} bytes",
            original.len(),
            compressed.len()
        );
        eprintln!("Compressed data: {:02x?}", &compressed);
        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, original.as_slice());
    }

    #[test]
    fn test_roundtrip_repeated() {
        let original = b"aaaaaaaaaabbbbbbbbbbcccccccccc";
        let compressed = compress(original, CompressionLevel::new(1)).unwrap();
        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, original.as_slice());
    }

    #[test]
    fn test_roundtrip_empty() {
        let original = b"";
        let compressed = compress(original, CompressionLevel::new(1)).unwrap();
        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, original.as_slice());
    }
}

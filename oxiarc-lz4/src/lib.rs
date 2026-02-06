//! Pure Rust LZ4 compression implementation.
//!
//! LZ4 is a lossless compression algorithm focusing on compression and
//! decompression speed. It provides very fast decompression while offering
//! reasonable compression ratios.
//!
//! # Features
//!
//! - Block compression/decompression (raw LZ4 blocks)
//! - Official LZ4 frame format with XXHash32 checksums
//! - Frame descriptor options (block size, checksums, content size)
//! - Compatible with lz4 reference implementation
//!
//! # Example
//!
//! ```
//! use oxiarc_lz4::{compress, decompress};
//!
//! let data = b"Hello, World! Hello, World!";
//! let compressed = compress(data).unwrap();
//! let decompressed = decompress(&compressed, data.len() * 2).unwrap();
//! assert_eq!(decompressed, data);
//! ```

mod block;
mod frame;
pub mod hc;
pub mod xxhash;

pub use block::{compress_block, decompress_block};
pub use frame::{
    BlockMaxSize, FrameDescriptor, LZ4_FRAME_MAGIC, Lz4Compressor, Lz4Decompressor, compress,
    compress_with_options, decompress,
};
pub use hc::{HcEncoder, HcLevel, compress_hc, compress_hc_level};

#[cfg(feature = "parallel")]
pub use frame::{compress_parallel, compress_with_options_parallel};

use oxiarc_core::error::Result;

/// LZ4 compression level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lz4Level {
    /// Fast compression (default).
    #[default]
    Fast,
    /// High compression (slower but better ratio).
    High,
}

/// Compress data using LZ4 block format.
pub fn compress_bytes(data: &[u8]) -> Result<Vec<u8>> {
    compress_block(data)
}

/// Decompress LZ4 block data.
pub fn decompress_bytes(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    decompress_block(data, max_output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_empty() {
        let data: &[u8] = b"";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed, 0).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_hello() {
        let data = b"Hello, World!";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let compressed = compress(data).unwrap();
        // Repeated data should compress well
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_pattern() {
        let data = b"abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz";
        let compressed = compress(data).unwrap();
        let decompressed = decompress(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let compressed = compress_block(data).unwrap();
        let decompressed = decompress_block(&compressed, data.len()).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_level() {
        assert_eq!(Lz4Level::default(), Lz4Level::Fast);
    }
}

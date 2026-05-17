//! Pure Rust Snappy compression implementation.
//!
//! This crate provides both the raw Snappy block format and the Snappy
//! framed (streaming) format with CRC32C checksums.
//!
//! # Block Format
//!
//! The block format provides simple compress/decompress functions for
//! in-memory data. This is the core Snappy algorithm.
//!
//! ```
//! use oxiarc_snappy::{compress, decompress};
//!
//! let data = b"Hello, World! Hello, World!";
//! let compressed = compress(data);
//! let decompressed = decompress(&compressed).unwrap();
//! assert_eq!(decompressed, data);
//! ```
//!
//! # Framed Format (Streaming)
//!
//! The framed format provides streaming compression/decompression using
//! `std::io::Write` and `std::io::Read` traits. Data is split into
//! chunks of up to 64 KiB, each with a CRC32C checksum.
//!
//! ```
//! use oxiarc_snappy::{FrameEncoder, FrameDecoder};
//! use std::io::{Write, Read};
//!
//! // Compress
//! let mut compressed = Vec::new();
//! {
//!     let mut encoder = FrameEncoder::new(&mut compressed);
//!     encoder.write_all(b"Hello, streaming Snappy!").unwrap();
//!     encoder.finish().unwrap();
//! }
//!
//! // Decompress
//! let mut decoder = FrameDecoder::new(&compressed[..]);
//! let mut output = Vec::new();
//! decoder.read_to_end(&mut output).unwrap();
//! assert_eq!(output, b"Hello, streaming Snappy!");
//! ```

#[cfg(feature = "async-io")]
pub mod async_snappy;
pub mod compress;
pub mod crc32c;
pub mod decompress;
pub mod error;
pub mod frame;
#[cfg(feature = "parallel")]
pub mod frame_parallel;
pub mod pool;

// Re-export the main public API

#[cfg(feature = "async-io")]
pub use async_snappy::{
    AsyncSnappyCompressor, AsyncSnappyDecompressor, compress_frame_async, decompress_frame_async,
};
pub use compress::compress;
pub use compress::compress_block_with_dict;
pub use compress::max_compress_len;
pub use decompress::decompress;
pub use decompress::decompress_block_with_dict;
pub use decompress::get_decompress_len as decompress_len;
pub use error::SnappyError;
pub use frame::FrameDecoder;
pub use frame::FrameEncoder;
pub use frame::compress_frame_pooled;
pub use frame::compress_frame_with_dict;
pub use frame::decompress_frame_with_dict;
#[cfg(feature = "parallel")]
pub use frame_parallel::compress_parallel;
pub use pool::PoolStats;
pub use pool::SnappyPool;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn test_block_roundtrip_empty() {
        let data: &[u8] = b"";
        let compressed = compress(data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip_hello() {
        let data = b"Hello, World!";
        let compressed = compress(data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip_repeated() {
        let data = vec![b'A'; 10_000];
        let compressed = compress(&data);
        // Repeated data should compress significantly
        assert!(
            compressed.len() < data.len() / 2,
            "compressed {} vs original {}",
            compressed.len(),
            data.len()
        );
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip_pattern() {
        let data = b"abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz";
        let compressed = compress(data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_block_roundtrip_binary() {
        let data: Vec<u8> = (0..=255).cycle().take(4096).collect();
        let compressed = compress(&data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip_single_byte() {
        let data = [0x42];
        let compressed = compress(&data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip_two_bytes() {
        let data = [0x42, 0x43];
        let compressed = compress(&data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_roundtrip_three_bytes() {
        let data = [0x42, 0x43, 0x44];
        let compressed = compress(&data);
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_decompress_len() {
        let data = vec![0u8; 12345];
        let compressed = compress(&data);
        let len = decompress_len(&compressed).expect("should decode length");
        assert_eq!(len, 12345);
    }

    #[test]
    fn test_max_compress_len_bounds() {
        for size in [0, 1, 100, 1000, 65536, 1_000_000] {
            let max_len = max_compress_len(size);
            let data = vec![0xFFu8; size];
            let compressed = compress(&data);
            assert!(
                compressed.len() <= max_len,
                "compressed {} > max {} for input size {}",
                compressed.len(),
                max_len,
                size
            );
        }
    }

    #[test]
    fn test_decompress_invalid_data() {
        // Random garbage should fail
        let garbage = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let result = decompress(&garbage);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_roundtrip_various_sizes() {
        for size in [0, 1, 10, 100, 1000, 65535, 65536, 65537, 100_000] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();

            let mut compressed = Vec::new();
            {
                let mut encoder = FrameEncoder::new(&mut compressed);
                encoder
                    .write_all(&data)
                    .unwrap_or_else(|e| panic!("write failed for size {size}: {e}"));
                encoder
                    .finish()
                    .unwrap_or_else(|e| panic!("finish failed for size {size}: {e}"));
            }

            let mut decoder = FrameDecoder::new(&compressed[..]);
            let mut output = Vec::new();
            decoder
                .read_to_end(&mut output)
                .unwrap_or_else(|e| panic!("read failed for size {size}: {e}"));

            assert_eq!(output, data, "roundtrip mismatch for size {size}");
        }
    }

    #[test]
    fn test_frame_multi_write_roundtrip() {
        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder
                .write_all(b"Part 1. ")
                .expect("write should succeed");
            encoder
                .write_all(b"Part 2. ")
                .expect("write should succeed");
            encoder.write_all(b"Part 3.").expect("write should succeed");
            encoder.finish().expect("finish should succeed");
        }

        let mut decoder = FrameDecoder::new(&compressed[..]);
        let mut output = String::new();
        decoder
            .read_to_string(&mut output)
            .expect("read should succeed");

        assert_eq!(output, "Part 1. Part 2. Part 3.");
    }

    #[test]
    fn test_block_roundtrip_all_same_byte() {
        // Run-length-like data
        for byte_val in [0x00, 0x55, 0xAA, 0xFF] {
            let data = vec![byte_val; 50_000];
            let compressed = compress(&data);
            let decompressed = decompress(&compressed).expect("should decompress");
            assert_eq!(decompressed, data);
        }
    }

    #[test]
    fn test_block_roundtrip_lorem_ipsum() {
        let data = b"Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
            Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. \
            Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris \
            nisi ut aliquip ex ea commodo consequat. Duis aute irure dolor in \
            reprehenderit in voluptate velit esse cillum dolore eu fugiat nulla \
            pariatur. Excepteur sint occaecat cupidatat non proident, sunt in \
            culpa qui officia deserunt mollit anim id est laborum.";
        let compressed = compress(data);
        // Text should compress somewhat
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data.as_slice());
    }
}

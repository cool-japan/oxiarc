//! # OxiArc Brotli
//!
//! Pure Rust implementation of the Brotli compression format (RFC 7932).
//!
//! Brotli is a general-purpose lossless compression algorithm that uses a
//! combination of LZ77, Huffman coding, and a static dictionary to achieve
//! excellent compression ratios, especially for web content.
//!
//! ## Features
//!
//! - LZ77 compression with backward references
//! - Context-dependent Huffman coding
//! - Static dictionary support (RFC 7932 Appendix A)
//! - Insert-and-copy length encoding
//! - Distance codes with short-distance ring buffer cache
//! - Multiple quality levels (0-11)
//! - Streaming Write/Read API
//!
//! ## Example
//!
//! ```rust,no_run
//! use oxiarc_brotli::{compress, decompress};
//!
//! let data = b"Hello, Brotli!";
//! let compressed = compress(data, 6).unwrap();
//! let decompressed = decompress(&compressed).unwrap();
//! assert_eq!(decompressed, data);
//! ```
//!
//! ## Streaming Example
//!
//! ```rust,no_run
//! use std::io::{Read, Write};
//! use oxiarc_brotli::streaming::{BrotliCompressor, BrotliDecompressor};
//! use oxiarc_brotli::compress::BrotliParams;
//!
//! // Compress
//! let mut compressed = Vec::new();
//! let params = BrotliParams::default();
//! let mut compressor = BrotliCompressor::new(&mut compressed, params);
//! compressor.write_all(b"Hello, streaming Brotli!").unwrap();
//! let compressed_output = compressor.finish().unwrap();
//!
//! // Decompress
//! let mut decompressor = BrotliDecompressor::new(&compressed[..]);
//! let mut output = Vec::new();
//! decompressor.read_to_end(&mut output).unwrap();
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod bit_reader;
pub mod bit_writer;
/// Brotli compression.
pub mod compress;
/// Context modeling for prefix code selection.
pub mod context;
/// Brotli decompression.
pub mod decompress;
/// Static dictionary (RFC 7932 Appendix A).
pub mod dictionary;
/// Error types for Brotli operations.
pub mod error;
/// Huffman (prefix) coding.
pub mod huffman;
/// LZ77 matching engine.
pub mod lz77;
/// Streaming compression and decompression.
pub mod streaming;

/// Parallel compression and decompression (requires `parallel` feature).
#[cfg(feature = "parallel")]
pub mod parallel;

/// Thread-safe buffer pool for amortising per-encode allocations.
pub mod pool;

/// Async I/O support via Tokio (requires `async-io` feature).
#[cfg(feature = "async-io")]
pub mod async_brotli;

// Re-export primary API.
pub use compress::{BrotliParams, compress, compress_with_params};
pub use decompress::decompress;
pub use error::{BrotliError, BrotliResult};
pub use pool::{BrotliPool, PoolStats};
pub use streaming::{BrotliCompressor, BrotliDecompressor};

#[cfg(feature = "parallel")]
pub use parallel::{compress_parallel, compress_parallel_with_params, decompress_frame_parallel};

#[cfg(feature = "async-io")]
pub use async_brotli::{BrotliAsyncCompressor, BrotliAsyncDecompressor};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_empty() {
        let result = compress(b"", 6);
        assert!(result.is_ok());
        let compressed = result.expect("should compress empty");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_compress_quality_0() {
        let data = b"Hello, world! This is a test of Brotli compression at quality 0.";
        let result = compress(data, 0);
        assert!(result.is_ok());
    }

    #[test]
    fn test_compress_various_qualities() {
        let data = b"The quick brown fox jumps over the lazy dog.";
        for quality in 0..=11 {
            let result = compress(data, quality);
            assert!(
                result.is_ok(),
                "compression failed at quality {quality}: {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn test_compress_repeated_data() {
        let data = "abcdef".repeat(100);
        let result = compress(data.as_bytes(), 6);
        assert!(result.is_ok());
        let compressed = result.expect("should compress");
        // Repeated data should compress well.
        assert!(compressed.len() < data.len());
    }

    #[test]
    fn test_compress_large_data() {
        let data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let result = compress(&data, 4);
        assert!(result.is_ok());
    }

    #[test]
    fn test_params_default() {
        let params = BrotliParams::default();
        assert_eq!(params.quality, 6);
        assert_eq!(params.lgwin, 22);
        assert_eq!(params.lgblock, 0);
    }

    #[test]
    fn test_params_validation() {
        let mut params = BrotliParams::default();
        assert!(params.validate().is_ok());

        params.quality = 12;
        assert!(params.validate().is_err());
    }

    #[test]
    fn test_streaming_compressor() {
        use std::io::Write;

        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::new(&mut output, params);
        compressor
            .write_all(b"Hello, streaming!")
            .expect("should write");
        let _ = compressor.finish().expect("should finish");
        assert!(!output.is_empty());
    }

    #[test]
    fn test_error_display() {
        let err = BrotliError::InvalidParameter("test".to_string());
        let msg = format!("{err}");
        assert!(msg.contains("test"));

        let err = BrotliError::UnexpectedEof;
        let msg = format!("{err}");
        assert!(msg.contains("unexpected"));
    }

    #[test]
    fn test_compress_decompress_roundtrip_simple() {
        let data = b"Hello, world! This is a test of Brotli compression.".repeat(10);
        let compressed = compress(&data, 1).expect("should compress");
        let decompressed = decompress(&compressed).expect("should decompress");
        assert_eq!(decompressed, data.as_slice(), "round-trip mismatch");
    }

    #[test]
    fn test_compress_decompress_binary_pattern() {
        for size in [100, 1000, 10000] {
            let data: Vec<u8> = (0..size).map(|i| ((i * 137) % 256) as u8).collect();
            let compressed = compress(&data, 6).unwrap_or_else(|e| {
                panic!("should compress binary size={size}: {e}");
            });
            let decompressed = decompress(&compressed).unwrap_or_else(|e| {
                panic!("should decompress binary size={size}: {e}");
            });
            assert_eq!(decompressed, data, "binary {size} round-trip mismatch");
        }
    }

    #[test]
    fn test_compress_decompress_uniform() {
        for size in [1, 10, 50, 100, 1000] {
            let data = vec![42u8; size];
            let compressed = compress(&data, 1).unwrap_or_else(|e| {
                panic!("should compress size={size}: {e}");
            });
            let decompressed = decompress(&compressed).unwrap_or_else(|e| {
                panic!("should decompress size={size}: {e}");
            });
            assert_eq!(decompressed, data, "uniform {size} round-trip mismatch");
        }
    }

    #[test]
    fn test_brotli_params_window_size() {
        let params = BrotliParams {
            lgwin: 16,
            ..BrotliParams::default()
        };
        assert_eq!(params.window_size(), 65536);

        let params = BrotliParams {
            lgwin: 22,
            ..BrotliParams::default()
        };
        assert_eq!(params.window_size(), 4194304);
    }
}

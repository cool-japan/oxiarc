//! # OxiArc Brotli
//!
//! Pure Rust implementation of the Brotli compression format (RFC 7932).
//!
//! Brotli is a general-purpose lossless compression algorithm that uses a
//! combination of LZ77, Huffman coding, and a static dictionary to achieve
//! excellent compression ratios, especially for web content.
//!
//! ## Interoperability
//!
//! The **decoder** implements the full RFC 7932 format — simple and complex
//! prefix codes, block-type switching, literal/distance context maps and
//! the exact Section 7.1 context tables, metadata meta-blocks, the complete
//! distance code space (short codes, `NPOSTFIX`/`NDIRECT`), and the
//! embedded 122,784-byte Appendix A static dictionary with all 121 word
//! transforms. It is validated by differential tests to decode reference
//! `brotli` CLI output byte-identically across qualities 0-11 and window
//! sizes 10-24 (see the `brotli-oracle` cargo feature).
//!
//! The **encoder** emits RFC-conformant streams accepted by the reference
//! `brotli -d`. It uses one prefix code per category per meta-block (no
//! multi-block-type splitting, context modeling, or dictionary-reference
//! emission yet), so compression ratios trail the reference encoder —
//! close on typical text at q5-9, further behind at q10-11 on structured
//! data. Incompressible input falls back to stored (uncompressed)
//! meta-blocks, bounding worst-case expansion to a few bytes per 16 MiB.
//!
//! ## Strictness
//!
//! The decoder rejects (never silently mis-decodes): truncated streams,
//! trailing garbage after the last meta-block, non-zero padding bits,
//! incomplete or over-subscribed prefix codes, invalid distances, and
//! meta-block length overruns. Note that Brotli itself carries no checksum,
//! so corruption that yields a different *valid* stream is undetectable by
//! design.
//!
//! Brotli declares no total uncompressed size, so untrusted input should be
//! decoded with [`decompress_with_limit`] (or
//! [`BrotliStream::with_max_output`] / [`BrotliDecompressor::with_max_output`]):
//! the budget is enforced per meta-block *while* decoding, so a decompression
//! bomb is rejected before its expansion is ever produced. The unbounded entry
//! points fall back to a built-in 256 MB guard.
//!
//! ## Decoding a stream that arrives in pieces
//!
//! [`decompress`] needs the whole compressed stream in one slice and produces
//! the whole output at once. For an HTTP body, a pipe, or anything else that
//! arrives in chunks, use [`BrotliStream`]: a push decoder that makes progress
//! from whatever input and output space it is given, with peak memory
//! proportional to the stream's declared sliding window rather than to the
//! stream. Feeding it one byte at a time into a one-byte output slice produces
//! exactly the bytes one call with everything would. See the [`stream`] module
//! for the memory model and the strictness guarantees.
//!
//! ```rust
//! use oxiarc_brotli::{compress, BrotliStatus, BrotliStream};
//! use oxiarc_core::traits::FlushMode;
//!
//! let compressed = compress(b"arrives in pieces", 5).expect("compress");
//! let mut stream = BrotliStream::new().with_max_output(1 << 20);
//! let mut decoded = Vec::new();
//! let mut out = [0u8; 4];
//! let mut fed = 0;
//! loop {
//!     let end = (fed + 3).min(compressed.len());
//!     let flush = if end == compressed.len() { FlushMode::Finish } else { FlushMode::None };
//!     let progress = stream.decode(&compressed[fed..end], &mut out, flush).expect("decode");
//!     fed += progress.consumed;
//!     decoded.extend_from_slice(&out[..progress.produced]);
//!     if progress.status == BrotliStatus::StreamEnd { break; }
//! }
//! stream.finish().expect("complete stream");
//! assert_eq!(decoded, b"arrives in pieces");
//! ```
//!
//! ## Features
//!
//! - LZ77 compression with backward references
//! - RFC 7932 prefix coding with two-level `O(1)` decode tables
//! - Static dictionary (RFC 7932 Appendix A, byte-exact) with all 121
//!   transforms, including UTF-8-aware ferment casing
//! - Insert-and-copy command alphabet with implicit distance-code-0 reuse
//! - Distance ring buffer semantics per Section 4
//! - Multiple quality levels (0-11); quality 0 = stored meta-blocks
//! - Window sizes `lgwin` 10-24 (window = `(1 << lgwin) - 16` bytes)
//! - Bounded incremental decoding ([`BrotliStream`]) with a real sliding
//!   window, exact per-meta-block output caps and a declared-window ceiling
//! - Incremental `Write` compressor and `Read`/`AsyncRead` decompressor
//!   adapters (see [`streaming`] and [`async_brotli`](crate::async_brotli))
//!
//! ## Example
//!
//! ```rust,no_run
//! use oxiarc_brotli::{compress, decompress};
//!
//! let data = b"Hello, Brotli!";
//! let compressed = compress(data, 6).expect("compress");
//! let decompressed = decompress(&compressed).expect("decompress");
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
//! compressor.write_all(b"Hello, streaming Brotli!").expect("write");
//! let compressed_output = compressor.finish().expect("finish");
//!
//! // Decompress
//! let mut decompressor = BrotliDecompressor::new(&compressed[..]);
//! let mut output = Vec::new();
//! decompressor.read_to_end(&mut output).expect("read");
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod bit_reader;
pub mod bit_writer;
/// Literal block splitting and context-map serialization for the encoder.
mod block_split;
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
/// Bounded, truly incremental Brotli decoding.
pub mod stream;
/// Streaming compression and decompression.
pub mod streaming;
/// Shared RFC 7932 constant tables (lengths, commands, block counts).
pub mod tables;

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
pub use decompress::{
    MetaBlockShape, decompress, decompress_reporting_shapes, decompress_with_limit,
};
pub use error::{BrotliError, BrotliResult};
pub use pool::{BrotliPool, PoolStats};
pub use stream::{BrotliProgress, BrotliStatus, BrotliStream, DEFAULT_MAX_WINDOW};
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
        // RFC 7932 Section 9.1: window size = (1 << WBITS) - 16.
        let params = BrotliParams {
            lgwin: 16,
            ..BrotliParams::default()
        };
        assert_eq!(params.window_size(), 65520);

        let params = BrotliParams {
            lgwin: 22,
            ..BrotliParams::default()
        };
        assert_eq!(params.window_size(), 4194288);
    }
}

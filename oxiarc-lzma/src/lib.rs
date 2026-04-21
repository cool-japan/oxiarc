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

    /// Test that progress callbacks fire during encode and decode, values are monotone,
    /// and the final reported byte count is close to the input size.
    #[test]
    fn test_lzma_progress_callbacks() {
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::{Arc, Mutex};

        /// A progress sink that records every `(processed, total)` call in order.
        struct RecordingSink {
            calls: Mutex<Vec<(u64, Option<u64>)>>,
        }

        impl RecordingSink {
            fn new() -> Arc<Self> {
                Arc::new(Self {
                    calls: Mutex::new(Vec::new()),
                })
            }

            fn calls(&self) -> Vec<(u64, Option<u64>)> {
                self.calls.lock().expect("mutex poisoned").clone()
            }
        }

        impl ProgressSink for RecordingSink {
            fn on_progress(&self, processed: u64, total: Option<u64>) {
                self.calls
                    .lock()
                    .expect("mutex poisoned")
                    .push((processed, total));
            }
        }

        // 100 KB of pseudo-random data (reproducible)
        let data: Vec<u8> = {
            let mut buf = Vec::with_capacity(100_000);
            let mut x: u32 = 0xDEAD_BEEF;
            for _ in 0..100_000 {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.push((x >> 24) as u8);
            }
            buf
        };

        // --- Compress with recording sink ---
        let enc_sink = RecordingSink::new();
        let enc_handle: ProgressHandle = enc_sink.clone();

        let dict_size = LzmaLevel::new(3).dict_size();
        let compressed = {
            use crate::encoder::LzmaEncoder;
            let mut output = Vec::new();
            let props = LzmaEncoder::new(LzmaLevel::new(3), dict_size).properties();

            // Write LZMA header (for decompress_bytes compatibility)
            output.push(props.to_byte());
            output.extend_from_slice(&dict_size.to_le_bytes());
            output.extend_from_slice(&(data.len() as u64).to_le_bytes());

            let enc = LzmaEncoder::new(LzmaLevel::new(3), dict_size).with_progress(enc_handle);
            let raw = enc.compress(&data).expect("encode failed");
            output.extend_from_slice(&raw);
            output
        };

        let enc_calls = enc_sink.calls();
        assert!(
            !enc_calls.is_empty(),
            "encoder must emit at least one progress call"
        );

        // Verify monotone non-decreasing processed values
        for window in enc_calls.windows(2) {
            assert!(
                window[1].0 >= window[0].0,
                "encoder progress must be non-decreasing: {} -> {}",
                window[0].0,
                window[1].0
            );
        }

        // Final call should be close to input size (within 1 byte)
        let enc_final = enc_calls.last().expect("must have at least one call").0;
        assert!(
            enc_final >= data.len() as u64 - 1,
            "final encoder progress ({}) should be close to input size ({})",
            enc_final,
            data.len()
        );

        // All encoder calls should carry Some(total) == data.len()
        for (_, total) in &enc_calls {
            assert_eq!(
                *total,
                Some(data.len() as u64),
                "encoder total must be input size"
            );
        }

        // --- Decompress with recording sink ---
        let dec_sink = RecordingSink::new();
        let dec_handle: ProgressHandle = dec_sink.clone();

        let decompressed = {
            use crate::decoder::LzmaDecoder;
            use std::io::Cursor;
            let decoder = LzmaDecoder::from_header(Cursor::new(&compressed))
                .expect("from_header failed")
                .with_progress(dec_handle);
            decoder.decompress().expect("decompress failed")
        };

        assert_eq!(decompressed, data, "round-trip must be lossless");

        let dec_calls = dec_sink.calls();
        assert!(
            !dec_calls.is_empty(),
            "decoder must emit at least one progress call"
        );

        // Verify monotone non-decreasing values
        for window in dec_calls.windows(2) {
            assert!(
                window[1].0 >= window[0].0,
                "decoder progress must be non-decreasing: {} -> {}",
                window[0].0,
                window[1].0
            );
        }

        // Final decoded bytes should match decompressed length
        let dec_final = dec_calls.last().expect("must have at least one call").0;
        assert_eq!(
            dec_final,
            data.len() as u64,
            "final decoder progress must equal decompressed size"
        );

        // Verify that a noop token doesn't break anything
        let _: Vec<u8> = {
            use crate::encoder::LzmaEncoder;
            let token = CancellationToken::new();
            let mut out = Vec::new();
            let props = LzmaEncoder::new(LzmaLevel::new(3), dict_size).properties();
            out.push(props.to_byte());
            out.extend_from_slice(&dict_size.to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            let raw = LzmaEncoder::new(LzmaLevel::new(3), dict_size)
                .with_cancel(token)
                .compress(&data)
                .expect("encode with noop cancel failed");
            out.extend_from_slice(&raw);
            out
        };
    }

    /// Test that a pre-cancelled token causes `Err(OxiArcError::Cancelled)` immediately.
    /// Also tests mid-decode cancellation using a progress-triggered cancel.
    #[test]
    fn test_lzma_cancellation() {
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::error::OxiArcError;
        use oxiarc_core::progress::ProgressSink;
        use std::io::Cursor;
        use std::sync::Arc;

        // Build a valid compressed buffer (small, fast)
        let original = b"Hello, cancellation test!".repeat(40);
        let compressed = compress(&original, LzmaLevel::new(3)).expect("compress failed");

        // --- Pre-cancel: token already cancelled before decode starts ---
        {
            use crate::decoder::LzmaDecoder;
            let token = CancellationToken::new();
            token.cancel(); // cancel BEFORE decode

            let decoder = LzmaDecoder::from_header(Cursor::new(&compressed))
                .expect("from_header failed")
                .with_cancel(token);

            let result = decoder.decompress();
            assert!(
                matches!(result, Err(OxiArcError::Cancelled)),
                "pre-cancelled token must produce Cancelled error, got: {:?}",
                result
            );
        }

        // --- Mid-decode cancel via progress sink trigger ---
        // We use a CountingSink that cancels the token after the first progress callback.
        {
            use crate::decoder::LzmaDecoder;
            use oxiarc_core::progress::ProgressHandle;
            use std::sync::atomic::{AtomicBool, Ordering};

            // Build a larger input to ensure multiple checkpoints
            let large_original: Vec<u8> = (0u8..=255).cycle().take(1_000_000).collect();
            let large_compressed =
                compress(&large_original, LzmaLevel::new(3)).expect("large compress failed");

            let token = CancellationToken::new();
            let token_for_sink = token.clone();
            let triggered = Arc::new(AtomicBool::new(false));
            let triggered_clone = triggered.clone();

            struct TriggerCancel {
                token: CancellationToken,
                triggered: Arc<AtomicBool>,
            }

            impl ProgressSink for TriggerCancel {
                fn on_progress(&self, _processed: u64, _total: Option<u64>) {
                    // Cancel on the very first callback so the next check fires quickly
                    if !self.triggered.load(Ordering::Relaxed) {
                        self.triggered.store(true, Ordering::Relaxed);
                        self.token.cancel();
                    }
                }
            }

            let sink: ProgressHandle = Arc::new(TriggerCancel {
                token: token_for_sink,
                triggered: triggered_clone,
            });

            let decoder = LzmaDecoder::from_header(Cursor::new(&large_compressed))
                .expect("from_header failed")
                .with_progress(sink)
                .with_cancel(token);

            let result = decoder.decompress();
            assert!(
                matches!(result, Err(OxiArcError::Cancelled)),
                "mid-decode cancel via progress sink must produce Cancelled error, got: {:?}",
                result
            );
        }
    }
}

//! Brotli interop tests: quality level coverage and round-trip correctness.
//!
//! These tests verify that our pure-Rust Brotli implementation produces
//! bitstreams that our own decompressor can recover losslessly across the
//! full quality range (0–11) and for a representative variety of input
//! shapes (empty, single byte, text, binary, large, highly compressible).
//!
//! Because we do not ship a C reference binary in the test environment we
//! test *interoperability* by the only means available without external
//! dependencies: compress with OxiArc → decompress with OxiArc and assert
//! byte-for-byte equality.  The quality-ordering assertions (q=11 ≤ q=0 for
//! repetitive data) give confidence that the quality dispatcher is wired
//! correctly end-to-end.

use oxiarc_brotli::{BrotliParams, compress, compress_with_params, decompress};

// ─── Empty / trivial inputs ───────────────────────────────────────────────────

/// Empty input must round-trip correctly.
/// The compressed form must be non-empty (it carries a Brotli stream header).
#[test]
fn test_brotli_interop_empty_roundtrip() {
    let compressed = compress(&[], 6).expect("compress empty should not fail");
    assert!(
        !compressed.is_empty(),
        "Brotli compressed form of empty input must be non-empty (has stream header)"
    );
    let decompressed = decompress(&compressed).expect("decompress empty should not fail");
    assert!(
        decompressed.is_empty(),
        "decompressed empty must yield empty vec"
    );
}

/// A single ASCII byte `'A'` (0x41) must survive a round-trip.
#[test]
fn test_brotli_interop_single_byte_roundtrip() {
    let input = &[0x41u8]; // 'A'
    let compressed = compress(input, 6).expect("compress single byte should not fail");
    let decompressed = decompress(&compressed).expect("decompress single byte should not fail");
    assert_eq!(
        decompressed.as_slice(),
        input.as_slice(),
        "single byte 'A' must round-trip"
    );
}

/// A single byte of value 0x00 (null byte) must round-trip.
#[test]
fn test_brotli_interop_single_null_byte_roundtrip() {
    let input = &[0x00u8];
    let compressed = compress(input, 6).expect("compress null byte should not fail");
    let decompressed = decompress(&compressed).expect("decompress null byte should not fail");
    assert_eq!(decompressed.as_slice(), input.as_slice());
}

/// A single byte of value 0xFF must round-trip.
#[test]
fn test_brotli_interop_single_ff_byte_roundtrip() {
    let input = &[0xFFu8];
    let compressed = compress(input, 6).expect("compress 0xFF byte should not fail");
    let decompressed = decompress(&compressed).expect("decompress 0xFF byte should not fail");
    assert_eq!(decompressed.as_slice(), input.as_slice());
}

// ─── Full quality-level sweep ─────────────────────────────────────────────────

/// Every quality level from 0 to 11 must produce output that decompresses
/// back to the original bytes.  This exercises the entire quality dispatcher.
#[test]
fn test_brotli_interop_all_quality_levels_produce_decompressable_output() {
    let input: &[u8] = b"The quick brown fox jumps over the lazy dog. \
          This is a test string for Brotli compression at all quality levels.";
    for quality in 0u32..=11 {
        let compressed = compress(input, quality)
            .unwrap_or_else(|e| panic!("compress q={quality} should not fail: {e}"));
        let decompressed = decompress(&compressed)
            .unwrap_or_else(|e| panic!("decompress q={quality} should not fail: {e}"));
        assert_eq!(
            decompressed.as_slice(),
            input,
            "quality={quality} round-trip produced incorrect output"
        );
    }
}

// ─── Quality 0 (fastest / store-only path) ────────────────────────────────────

/// Quality 0 is the fastest path (uncompressed meta-blocks in our impl).
/// Output must decompress correctly regardless of compression ratio.
#[test]
fn test_brotli_interop_quality_0_fastest() {
    let input = vec![0xAAu8; 1024];
    let compressed_q0 = compress(&input, 0).expect("compress q=0 should not fail");
    let compressed_q6 = compress(&input, 6).expect("compress q=6 should not fail");

    let decompressed_q0 = decompress(&compressed_q0).expect("decompress q=0 should not fail");
    let decompressed_q6 = decompress(&compressed_q6).expect("decompress q=6 should not fail");

    assert_eq!(decompressed_q0, input, "q=0 decompressed must match input");
    assert_eq!(decompressed_q6, input, "q=6 decompressed must match input");
}

/// Quality 0 on text-like content must round-trip.
#[test]
fn test_brotli_interop_quality_0_text_roundtrip() {
    let input = b"Hello, Brotli at quality zero! ".repeat(32);
    let compressed = compress(&input, 0).expect("compress q=0 text should not fail");
    let decompressed = decompress(&compressed).expect("decompress q=0 text should not fail");
    assert_eq!(
        decompressed.as_slice(),
        input.as_slice(),
        "q=0 text round-trip failed"
    );
}

// ─── Quality 11 (best ratio) ──────────────────────────────────────────────────

/// Quality 11 must achieve at least as good a ratio as quality 0 on highly
/// compressible (repetitive) data.
#[test]
fn test_brotli_interop_quality_11_best_ratio() {
    let input = vec![b'A'; 4096]; // maximally compressible
    let compressed_q0 = compress(&input, 0).expect("compress q=0 should not fail");
    let compressed_q11 = compress(&input, 11).expect("compress q=11 should not fail");

    let decompressed = decompress(&compressed_q11).expect("decompress q=11 should not fail");
    assert_eq!(decompressed, input, "q=11 decompressed must match original");

    assert!(
        compressed_q11.len() <= compressed_q0.len(),
        "q=11 ({}) should produce output no larger than q=0 ({}) for repetitive data",
        compressed_q11.len(),
        compressed_q0.len()
    );
}

/// Quality 11 on a repeated pattern of 8 distinct bytes must round-trip.
#[test]
fn test_brotli_interop_quality_11_repeated_pattern_roundtrip() {
    let pattern: &[u8] = b"ABCDEFGH";
    let input: Vec<u8> = pattern.iter().cycle().take(2048).cloned().collect();
    let compressed = compress(&input, 11).expect("compress q=11 pattern should not fail");
    let decompressed = decompress(&compressed).expect("decompress q=11 pattern should not fail");
    assert_eq!(
        decompressed, input,
        "q=11 repeated pattern round-trip failed"
    );
}

// ─── Large inputs ─────────────────────────────────────────────────────────────

/// 256 KiB of text-like content (ASCII cycling through A–Z) must round-trip.
/// This exercises multi-meta-block compression paths.
#[test]
fn test_brotli_interop_large_input_roundtrip() {
    let input: Vec<u8> = (0u32..262144).map(|i| b'A' + (i % 26) as u8).collect();
    let compressed = compress(&input, 6).expect("compress 256KiB should not fail");
    let decompressed = decompress(&compressed).expect("decompress 256KiB should not fail");
    assert_eq!(decompressed, input, "256 KiB text-like round-trip failed");
}

/// 64 KiB of uniform bytes must round-trip at quality 4.
#[test]
fn test_brotli_interop_64kib_uniform_roundtrip() {
    let input = vec![0x55u8; 65536];
    let compressed = compress(&input, 4).expect("compress 64KiB uniform should not fail");
    let decompressed = decompress(&compressed).expect("decompress 64KiB uniform should not fail");
    assert_eq!(decompressed, input, "64 KiB uniform round-trip failed");
}

// ─── Binary data ─────────────────────────────────────────────────────────────

/// Binary data (full 0x00–0xFF cycling byte sequence) must compress and
/// decompress losslessly.  Binary data is not text and does not benefit from
/// the Brotli static dictionary, so this exercises the raw LZ77+Huffman path.
#[test]
fn test_brotli_interop_binary_data_roundtrip() {
    let input: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
    let compressed = compress(&input, 6).expect("compress binary data should not fail");
    let decompressed = decompress(&compressed).expect("decompress binary data should not fail");
    assert_eq!(decompressed, input, "binary data round-trip failed");
}

/// Pseudo-random binary data (deterministic) must round-trip at quality 1.
///
/// Uses a multiplicative-hash scramble — deterministic and non-repeating —
/// at quality 1 which exercises the LZ77+Huffman code path while staying
/// well within the decompressor's validated distance range.
#[test]
fn test_brotli_interop_pseudo_random_binary_roundtrip() {
    // Linear-congruential scramble — deterministic, non-repeating.
    let input: Vec<u8> = (0u32..4096)
        .map(|i| ((i.wrapping_mul(137)) % 256) as u8)
        .collect();
    let compressed = compress(&input, 1).expect("compress pseudo-random should not fail");
    let decompressed = decompress(&compressed).expect("decompress pseudo-random should not fail");
    assert_eq!(
        decompressed, input,
        "pseudo-random binary round-trip failed"
    );
}

// ─── `compress_with_params` ───────────────────────────────────────────────────

/// `compress_with_params` with explicit params must produce decompressable output.
#[test]
fn test_brotli_interop_with_params_roundtrip() {
    let params = BrotliParams {
        quality: 9,
        lgwin: 20,
        lgblock: 0,
    };
    let input = b"test data for compress_with_params round-trip verification";
    let compressed =
        compress_with_params(input, &params).expect("compress_with_params should not fail");
    let decompressed =
        decompress(&compressed).expect("decompress compress_with_params output should not fail");
    assert_eq!(
        decompressed.as_slice(),
        input.as_slice(),
        "compress_with_params round-trip failed"
    );
}

/// Default `BrotliParams` (quality 6, lgwin 22) must round-trip.
#[test]
fn test_brotli_interop_default_params_roundtrip() {
    let params = BrotliParams::default();
    let input = b"Default BrotliParams should produce correct output for this input string.";
    let compressed =
        compress_with_params(input, &params).expect("compress default params should not fail");
    let decompressed =
        decompress(&compressed).expect("decompress default params output should not fail");
    assert_eq!(
        decompressed.as_slice(),
        input.as_slice(),
        "default params round-trip failed"
    );
}

/// Minimum window size (lgwin=16, 64 KiB window) must still produce
/// decompressable output.
#[test]
fn test_brotli_interop_min_window_size_roundtrip() {
    let params = BrotliParams {
        quality: 4,
        lgwin: 16,
        lgblock: 0,
    };
    let input: Vec<u8> = (0u8..=127).cycle().take(512).collect();
    let compressed =
        compress_with_params(&input, &params).expect("compress lgwin=16 should not fail");
    let decompressed = decompress(&compressed).expect("decompress lgwin=16 output should not fail");
    assert_eq!(decompressed, input, "lgwin=16 round-trip failed");
}

// ─── Compressed size ordering ─────────────────────────────────────────────────

/// For highly compressible data (all-zeros), compressed size must never exceed
/// the original size at quality 6 or higher.
#[test]
fn test_brotli_interop_compression_is_beneficial_for_uniform_data() {
    let input = vec![0u8; 4096];
    let compressed = compress(&input, 6).expect("compress all-zeros should not fail");
    let decompressed = decompress(&compressed).expect("decompress all-zeros should not fail");
    assert_eq!(decompressed, input);
    assert!(
        compressed.len() < input.len(),
        "q=6 should compress 4096 all-zeros to less than {} bytes, got {}",
        input.len(),
        compressed.len()
    );
}

/// Invalid quality level (>11) must be rejected before any compression work.
#[test]
fn test_brotli_interop_invalid_quality_rejected() {
    let result = compress(b"hello", 12);
    assert!(
        result.is_err(),
        "quality 12 must be rejected as invalid (range is 0-11)"
    );
}

/// Params with out-of-range lgwin must be rejected.
#[test]
fn test_brotli_interop_invalid_lgwin_rejected() {
    let params = BrotliParams {
        quality: 6,
        lgwin: 25, // out of range: valid is 16-24
        lgblock: 0,
    };
    let result = compress_with_params(b"hello", &params);
    assert!(
        result.is_err(),
        "lgwin=25 must be rejected as invalid (range is 16-24)"
    );
}

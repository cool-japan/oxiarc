//! Integration tests for LZ4 block-layer prefix dictionary support.
//!
//! These tests exercise `compress_block_with_dict` / `decompress_block_dict`
//! and the builder types `Lz4DictBlockEncoder` / `Lz4DictBlockDecoder`.

use oxiarc_lz4::block::{compress_block_with_dict, decompress_block_dict};
use oxiarc_lz4::dict::{Lz4DictBlockDecoder, Lz4DictBlockEncoder};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a deterministic pseudo-random byte vector from a simple LCG.
///
/// This is intentionally not a cryptographic RNG — we need reproducible data
/// for property tests without pulling in external crates.
fn pseudo_random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 33) & 0xFF) as u8
        })
        .collect()
}

/// Build a deterministic byte vector with a repetitive pattern to ensure
/// the compressor has matches to exploit.
fn pattern_bytes(len: usize) -> Vec<u8> {
    let pattern = b"The quick brown fox jumps over the lazy dog. 0123456789 ";
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let remaining = len - out.len();
        out.extend_from_slice(&pattern[..remaining.min(pattern.len())]);
    }
    out
}

// ---------------------------------------------------------------------------
// Test 1: basic roundtrip with a 4 KiB dictionary
// ---------------------------------------------------------------------------

#[test]
fn test_roundtrip_4kib_dict() {
    let dict = pattern_bytes(4 * 1024);
    let input = pattern_bytes(32 * 1024);

    let compressed =
        compress_block_with_dict(&input, &dict, 1).expect("compress_block_with_dict failed");
    let decompressed = decompress_block_dict(&compressed, &dict, input.len() * 2)
        .expect("decompress_block_dict failed");

    assert_eq!(decompressed, input, "roundtrip mismatch with 4 KiB dict");
}

// ---------------------------------------------------------------------------
// Test 2: HC mode roundtrip at levels 1 and 9
// ---------------------------------------------------------------------------

#[test]
fn test_hc_roundtrip_level1() {
    use oxiarc_lz4::dict::{Lz4Dict, compress_with_dict_hc, decompress_with_dict};
    use oxiarc_lz4::hc::HcLevel;

    let dict_bytes = pattern_bytes(4 * 1024);
    let dict = Lz4Dict::new(&dict_bytes);
    let input = pattern_bytes(16 * 1024);

    let level = HcLevel::new(1).expect("level 1 is valid");
    let compressed =
        compress_with_dict_hc(&input, &dict, level).expect("hc dict compress level 1 failed");
    let decompressed = decompress_with_dict(&compressed, input.len() * 2, &dict)
        .expect("hc dict decompress level 1 failed");

    assert_eq!(decompressed, input, "HC level 1 roundtrip failed");
}

#[test]
fn test_hc_roundtrip_level9() {
    use oxiarc_lz4::dict::{Lz4Dict, compress_with_dict_hc, decompress_with_dict};
    use oxiarc_lz4::hc::HcLevel;

    let dict_bytes = pattern_bytes(4 * 1024);
    let dict = Lz4Dict::new(&dict_bytes);
    let input = pattern_bytes(16 * 1024);

    let level = HcLevel::new(9).expect("level 9 is valid");
    let compressed =
        compress_with_dict_hc(&input, &dict, level).expect("hc dict compress level 9 failed");
    let decompressed = decompress_with_dict(&compressed, input.len() * 2, &dict)
        .expect("hc dict decompress level 9 failed");

    assert_eq!(decompressed, input, "HC level 9 roundtrip failed");
}

// ---------------------------------------------------------------------------
// Test 3: encode with dict, decode WITHOUT dict → must differ or error
// ---------------------------------------------------------------------------

#[test]
fn test_decode_without_dict_fails_or_differs() {
    use oxiarc_lz4::decompress_block;

    let dict = pattern_bytes(4 * 1024);
    let input = pattern_bytes(16 * 1024);

    let compressed =
        compress_block_with_dict(&input, &dict, 1).expect("compress_block_with_dict failed");

    // Decoding without the dictionary should either error (offset beyond
    // output bounds) or, on the rare occasion the stream happens to be valid
    // plain-LZ4, produce different output.
    let result = decompress_block(&compressed, input.len() * 2);
    match result {
        Err(_) => { /* expected: back-ref points into dict region → error */ }
        Ok(ref decoded) => {
            // If it didn't error, the output must not match the original.
            assert_ne!(
                decoded.as_slice(),
                input.as_slice(),
                "decoded without dict should not match original"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Test 4: dict exactly 64 KiB
// ---------------------------------------------------------------------------

#[test]
fn test_dict_exactly_64kib() {
    let dict = pseudo_random_bytes(42, 64 * 1024);
    let input = pseudo_random_bytes(99, 4 * 1024);

    let compressed =
        compress_block_with_dict(&input, &dict, 1).expect("compress (64 KiB dict) failed");
    let decompressed = decompress_block_dict(&compressed, &dict, input.len() * 2)
        .expect("decompress (64 KiB dict) failed");

    assert_eq!(
        decompressed, input,
        "roundtrip mismatch with exact 64 KiB dict"
    );
}

// ---------------------------------------------------------------------------
// Test 5: dict longer than 64 KiB — must truncate to last 64 KiB
// ---------------------------------------------------------------------------

#[test]
fn test_dict_longer_than_64kib_truncates() {
    // 96 KiB dict; the encoder/decoder must both use the last 64 KiB.
    let full_dict = pseudo_random_bytes(7, 96 * 1024);
    let tail64 = &full_dict[full_dict.len() - 64 * 1024..];

    let input = pseudo_random_bytes(13, 4 * 1024);

    // Compress with full dict (should internally use last 64 KiB).
    let compressed =
        compress_block_with_dict(&input, &full_dict, 1).expect("compress (96 KiB dict) failed");

    // Decompress with only the last 64 KiB — must succeed if truncation is consistent.
    let decompressed = decompress_block_dict(&compressed, tail64, input.len() * 2)
        .expect("decompress (last 64 KiB of dict) failed");

    assert_eq!(decompressed, input, "dict truncation roundtrip failed");
}

// ---------------------------------------------------------------------------
// Test 6: empty dict — must behave identically to no-dict compression
// ---------------------------------------------------------------------------

#[test]
fn test_empty_dict_equivalent_to_no_dict() {
    use oxiarc_lz4::compress_block;

    let data = pattern_bytes(8 * 1024);
    let empty: &[u8] = b"";

    let with_empty_dict =
        compress_block_with_dict(&data, empty, 1).expect("compress with empty dict failed");
    let plain = compress_block(&data).expect("plain compress failed");

    // Both should decompress to the original data.
    let decomp_dict = decompress_block_dict(&with_empty_dict, empty, data.len() * 2)
        .expect("decompress with empty dict failed");
    let decomp_plain =
        oxiarc_lz4::decompress_block(&plain, data.len() * 2).expect("plain decompress failed");

    assert_eq!(decomp_dict, data, "empty-dict compress/decompress failed");
    assert_eq!(decomp_plain, data, "plain compress/decompress failed");
}

// ---------------------------------------------------------------------------
// Test 7: property — deterministic pseudo-random input + dict roundtrips
// ---------------------------------------------------------------------------

#[test]
fn test_property_random_roundtrip() {
    // Try several seed pairs to exercise varied dict/input combinations.
    let cases: &[(u64, u64, usize, usize)] = &[
        (1, 2, 1024, 4096),
        (3, 4, 8192, 8192),
        (5, 6, 4096, 16384),
        (7, 8, 200, 500),         // small data
        (9, 10, 16 * 1024, 1024), // large dict, small input
    ];

    for &(dict_seed, input_seed, dict_len, input_len) in cases {
        let dict = pseudo_random_bytes(dict_seed, dict_len);
        let input = pseudo_random_bytes(input_seed, input_len);

        let compressed = compress_block_with_dict(&input, &dict, 1).unwrap_or_else(|e| {
            panic!(
                "compress failed (dict_seed={}, input_seed={}): {}",
                dict_seed, input_seed, e
            )
        });
        let decompressed = decompress_block_dict(&compressed, &dict, input.len() * 2)
            .unwrap_or_else(|e| {
                panic!(
                    "decompress failed (dict_seed={}, input_seed={}): {}",
                    dict_seed, input_seed, e
                )
            });

        assert_eq!(
            decompressed, input,
            "roundtrip mismatch (dict_seed={}, input_seed={})",
            dict_seed, input_seed
        );
    }
}

// ---------------------------------------------------------------------------
// Test 8: Lz4DictBlockEncoder / Lz4DictBlockDecoder builder API
// ---------------------------------------------------------------------------

#[test]
fn test_builder_api_roundtrip() {
    let dict_bytes = b"shared prefix that appears in the test data repeatedly";
    let encoder = Lz4DictBlockEncoder::new(dict_bytes);
    let decoder = Lz4DictBlockDecoder::new(dict_bytes);

    let data = b"shared prefix that appears in the test data repeatedly and then some more";
    let compressed = encoder.compress(data).expect("encoder.compress failed");
    let decompressed = decoder
        .decompress(&compressed, data.len() * 2)
        .expect("decoder.decompress failed");

    assert_eq!(decompressed, data, "builder API roundtrip failed");
}

#[test]
fn test_builder_api_with_accel() {
    let dict_bytes = pattern_bytes(2 * 1024);
    let input = pattern_bytes(8 * 1024);

    // Test several acceleration values.
    for accel in [1, 5, 10, 50] {
        let encoder = Lz4DictBlockEncoder::with_accel(&dict_bytes, accel);
        let decoder = Lz4DictBlockDecoder::new(&dict_bytes);

        let compressed = encoder
            .compress(&input)
            .unwrap_or_else(|e| panic!("compress accel={} failed: {}", accel, e));
        let decompressed = decoder
            .decompress(&compressed, input.len() * 2)
            .unwrap_or_else(|e| panic!("decompress accel={} failed: {}", accel, e));

        assert_eq!(
            decompressed, input,
            "builder roundtrip failed at accel={}",
            accel
        );
    }
}

#[test]
fn test_builder_empty_input() {
    let dict_bytes = b"some dict data";
    let encoder = Lz4DictBlockEncoder::new(dict_bytes);
    let decoder = Lz4DictBlockDecoder::new(dict_bytes);

    let data: &[u8] = b"";
    let compressed = encoder.compress(data).expect("compress empty failed");
    let decompressed = decoder
        .decompress(&compressed, 100)
        .expect("decompress empty failed");
    assert_eq!(decompressed, data);
}

// ---------------------------------------------------------------------------
// Test 9: builder dict() accessor
// ---------------------------------------------------------------------------

#[test]
fn test_builder_dict_accessor() {
    let dict_bytes = b"accessor test data";
    let encoder = Lz4DictBlockEncoder::new(dict_bytes);
    let decoder = Lz4DictBlockDecoder::new(dict_bytes);

    assert_eq!(encoder.dict().len(), dict_bytes.len());
    assert_eq!(decoder.dict().len(), dict_bytes.len());
    assert_eq!(encoder.dict().id(), decoder.dict().id());
}

//! Regression tests for high-entropy / incompressible round-trip correctness.
//!
//! These guard two bugs that together broke every quality level on
//! incompressible input while leaving compressible input working:
//!
//! 1. **Incomplete length-limited Huffman codes.** For near-uniform,
//!    all-symbols-present literal distributions, the old `compute_code_lengths`
//!    heuristic produced a code-length table whose Kraft sum was strictly below
//!    `2^15` (an *incomplete* prefix code). The decoder then hit bit patterns
//!    that decoded to no symbol and failed with
//!    "invalid Huffman code: no matching code found". Fixed by replacing the
//!    heuristic with the package-merge algorithm, which always yields a complete
//!    (and length-optimal) code.
//!
//! 2. **Insert lengths above 319 silently truncated.** A single incompressible
//!    meta-block is emitted as one insert-and-copy command whose insert length
//!    equals the whole block. The encoder only had insert categories 0–15 (max
//!    319) and wrote the excess in 7 bits, wrapping around — so the decoder
//!    stopped the literal run early and desynchronised. Fixed by extending the
//!    insert-length code table (and the matching decoder) to large inserts via
//!    the extended insert-and-copy symbols (128–703).
//!
//! Every case below asserts an exact byte-for-byte round-trip across the full
//! quality range 1..=11.

use oxiarc_brotli::{compress, decompress};

/// Deterministic LCG byte generator (non-repeating, full-entropy-ish).
fn lcg_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 33) as u8);
    }
    out
}

/// Round-trip `data` at every quality 1..=11 and assert exact equality.
fn assert_roundtrip_all_qualities(data: &[u8], label: &str) {
    for quality in 1u32..=11 {
        let compressed = compress(data, quality)
            .unwrap_or_else(|e| panic!("{label}: compress q={quality} failed: {e}"));
        let decompressed = decompress(&compressed)
            .unwrap_or_else(|e| panic!("{label}: decompress q={quality} failed: {e}"));
        assert_eq!(
            decompressed,
            data,
            "{label}: round-trip mismatch at quality {quality} \
             (in={} bytes, out={} bytes, comp={} bytes)",
            data.len(),
            decompressed.len(),
            compressed.len()
        );
    }
}

/// (a) Pseudo-random 4 KiB block — the original "no matching code found" case.
#[test]
fn random_4k_roundtrip_all_qualities() {
    let data = lcg_bytes(4096, 0x1234_5678);
    assert_roundtrip_all_qualities(&data, "random-4k");
}

/// (a') Several independent random blocks of varied sizes and seeds.
#[test]
fn random_varied_sizes_roundtrip() {
    for (size, seed) in [
        (1usize, 1u64),
        (2, 2),
        (15, 3),
        (255, 4),
        (256, 5),
        (257, 6),
        (319, 7), // exactly the old short insert-length boundary
        (320, 8), // first length needing an extended insert category
        (1000, 9),
        (4096, 10),
        (5000, 11),
    ] {
        let data = lcg_bytes(size, seed);
        assert_roundtrip_all_qualities(&data, &format!("random-{size}"));
    }
}

/// (b) Incompressible counter pattern (each byte distinct within a 256-window).
#[test]
fn incompressible_counter_roundtrip_all_qualities() {
    let data: Vec<u8> = (0u32..4096)
        .map(|i| i.wrapping_mul(2654435761).rotate_left(13) as u8)
        .collect();
    assert_roundtrip_all_qualities(&data, "counter-4k");
}

/// (c) Blocks containing all 256 distinct byte values.
#[test]
fn all_distinct_bytes_roundtrip_all_qualities() {
    // Exactly one of each byte (perfectly uniform distribution).
    let one_each: Vec<u8> = (0u16..256).map(|b| b as u8).collect();
    assert_roundtrip_all_qualities(&one_each, "all-distinct-256");

    // All 256 values repeated to fill a larger block (still uniform frequency).
    let repeated: Vec<u8> = (0u32..4096).map(|i| (i % 256) as u8).collect();
    assert_roundtrip_all_qualities(&repeated, "all-distinct-4k");

    // A shuffled permutation block (uniform but non-sequential ordering).
    let mut shuffled = Vec::with_capacity(2048);
    for i in 0u32..2048 {
        shuffled.push((i.wrapping_mul(167) % 256) as u8);
    }
    assert_roundtrip_all_qualities(&shuffled, "all-distinct-shuffled");
}

/// (d) All-same-byte blocks (maximally compressible) for a range of values/sizes.
#[test]
fn all_same_byte_roundtrip_all_qualities() {
    for &byte in &[0u8, 0x55, 0xAA, 0xFF, b'A'] {
        for &size in &[1usize, 2, 100, 1000, 4096] {
            let data = vec![byte; size];
            assert_roundtrip_all_qualities(&data, &format!("same-{byte:#04x}-{size}"));
        }
    }
}

/// (e) Empty input round-trips at every quality.
#[test]
fn empty_roundtrip_all_qualities() {
    let data: &[u8] = &[];
    for quality in 1u32..=11 {
        let compressed =
            compress(data, quality).unwrap_or_else(|e| panic!("empty compress q={quality}: {e}"));
        let decompressed =
            decompress(&compressed).unwrap_or_else(|e| panic!("empty decompress q={quality}: {e}"));
        assert!(
            decompressed.is_empty(),
            "empty round-trip produced {} bytes at quality {quality}",
            decompressed.len()
        );
    }
}

/// Large (~64 KiB) pseudo-random buffer must round-trip at every quality.
///
/// This crosses the per-quality meta-block size thresholds, exercising both the
/// single-large-meta-block path and (for forced small blocks) multi-block paths
/// with incompressible content.
#[test]
fn random_64k_roundtrip_all_qualities() {
    let data = lcg_bytes(64 * 1024, 0xDEAD_BEEF);
    assert_roundtrip_all_qualities(&data, "random-64k");
}

/// Mixed compressible + incompressible regions in one buffer.
///
/// The first half is highly repetitive (LZ77 copies dominate) and the second
/// half is pseudo-random (literals dominate), forcing both code paths within a
/// single meta-block and a non-trivial literal Huffman distribution.
#[test]
fn mixed_compressible_incompressible_roundtrip() {
    let mut data = Vec::with_capacity(8192);
    data.extend(std::iter::repeat_n(b"oxiarc-brotli ".iter().copied(), 256).flatten());
    data.extend(lcg_bytes(4096, 0xABCD_1234));
    assert_roundtrip_all_qualities(&data, "mixed");
}

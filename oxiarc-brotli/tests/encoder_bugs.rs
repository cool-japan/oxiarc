//! Regression tests for encoder correctness bugs.
//!
//! Bug 1: Multi-block encoder broken for large inputs at quality 4+.
//! Bug 2: Quality-1 encoder produces incorrect output for repeated-pattern data.
//!
//! These tests were initially marked `#[ignore]` to confirm the bugs,
//! then the `#[ignore]` annotations are removed once the fixes are applied.

use oxiarc_brotli::{BrotliParams, compress, compress_with_params, decompress};

// ─── Bug 1: Multi-block encoder correctness ───────────────────────────────────

/// Quality 4 with 300 KiB input (> 256 KiB block boundary) must round-trip.
///
/// This exercises the multi-meta-block code path at the boundary where
/// quality 4 switches block size (256 KiB). Any ISLAST/bit-alignment bug
/// will surface here.
#[test]
fn test_multiblock_roundtrip_quality_4() {
    let input: Vec<u8> = (0u32..300_000).map(|i| (i % 251) as u8).collect(); // 300 KiB
    let params = BrotliParams {
        quality: 4,
        lgwin: 22,
        lgblock: 0,
    };
    let compressed = compress_with_params(&input, &params).expect("compress q=4 300KiB");
    let decompressed = decompress(&compressed).expect("decompress q=4 300KiB");
    assert_eq!(
        decompressed, input,
        "q=4 multi-block 300KiB round-trip failed"
    );
}

/// Extended quality + size sweep for multi-block correctness.
///
/// Covers boundary conditions at each quality level's block-size threshold.
#[test]
fn test_multiblock_roundtrip_quality_sweep() {
    // quality -> block_size (auto): q<=4 -> 256KiB, q<=8 -> 1MiB, q9-11 -> 4MiB.
    // Test each quality level with an input slightly larger than its block boundary.
    let quality_size_pairs: &[(u32, usize)] = &[
        (4, 256 * 1024 + 1), // just over q4 block boundary
        (5, 256 * 1024 + 1), // q5 block is 1MiB but test at 257KiB still multi-block if q4 was used
        (6, 512 * 1024),     // 512 KiB, two blocks at q<=4 level
        (9, 256 * 1024 + 1), // q9 block is 4MiB, but test smaller multi-block
    ];

    for &(quality, size) in quality_size_pairs {
        // Use lgblock=18 (256KiB) to force multi-block at every quality.
        let params = BrotliParams {
            quality,
            lgwin: 22,
            lgblock: 18, // force 256KiB blocks regardless of quality
        };
        let input: Vec<u8> = (0u32..size as u32).map(|i| (i % 251) as u8).collect();
        let compressed = compress_with_params(&input, &params)
            .unwrap_or_else(|e| panic!("compress q={quality} size={size} failed: {e}"));
        let decompressed = decompress(&compressed)
            .unwrap_or_else(|e| panic!("decompress q={quality} size={size} failed: {e}"));
        assert_eq!(
            decompressed, input,
            "multi-block round-trip failed: quality={quality} size={size}"
        );
    }
}

/// Explicit large-input test at q=5 with > 1 MiB input.
#[test]
fn test_multiblock_roundtrip_quality_5_large() {
    let size = 1024 * 1024 + 4096; // slightly over 1MiB
    let params = BrotliParams {
        quality: 5,
        lgwin: 22,
        lgblock: 18, // force 256KiB blocks
    };
    let input: Vec<u8> = (0u32..size as u32).map(|i| (i % 251) as u8).collect();
    let compressed =
        compress_with_params(&input, &params).expect("compress q=5 1MiB+ should not fail");
    let decompressed = decompress(&compressed).expect("decompress q=5 1MiB+ should not fail");
    assert_eq!(
        decompressed, input,
        "q=5 multi-block 1MiB+ round-trip failed"
    );
}

// ─── Bug 2: Quality-1 repeated-pattern correctness ────────────────────────────

/// Quality 1 on 50000 bytes of repeated "hello world " must round-trip.
///
/// The fast LZ77 path at quality 1 finds 256-byte matches for repetitive data.
/// A copy-length of 1 (from splitting 256 mod 17 == 1) is not encodable;
/// the decoder always reads at least copy-length 2 for category 0.
#[test]
fn test_quality1_repeated_pattern() {
    let input: Vec<u8> = b"hello world "
        .iter()
        .cycle()
        .take(50_000)
        .cloned()
        .collect();
    let compressed = compress(&input, 1).expect("compress q=1 repeated pattern");
    let decompressed = decompress(&compressed).expect("decompress q=1 repeated pattern");
    assert_eq!(
        decompressed, input,
        "q=1 repeated-pattern round-trip failed"
    );
}

/// Quality-1 sweep over multiple patterns and sizes.
#[test]
fn test_quality1_various_patterns() {
    let patterns: &[&[u8]] = &[b"abcde", b"\x00\x01\x02", b"hello world "];
    let repeats = [100usize, 1000, 10_000];

    for &pattern in patterns {
        for &repeat in &repeats {
            let input: Vec<u8> = pattern.iter().cycle().take(repeat).cloned().collect();
            let compressed = compress(&input, 1).unwrap_or_else(|e| {
                panic!(
                    "compress q=1 pattern={:?} repeat={repeat} failed: {e}",
                    pattern
                )
            });
            let decompressed = decompress(&compressed).unwrap_or_else(|e| {
                panic!(
                    "decompress q=1 pattern={:?} repeat={repeat} failed: {e}",
                    pattern
                )
            });
            assert_eq!(
                decompressed, input,
                "q=1 pattern={:?} repeat={repeat} round-trip failed",
                pattern
            );
        }
    }
}

/// Quality-0 and quality-2 must round-trip repeated patterns (regression guard).
#[test]
fn test_quality0_quality2_repeated_pattern_roundtrip() {
    let input: Vec<u8> = b"abcde".iter().cycle().take(10_000).cloned().collect();
    for quality in [0u32, 2] {
        let compressed = compress(&input, quality)
            .unwrap_or_else(|e| panic!("compress q={quality} failed: {e}"));
        let decompressed = decompress(&compressed)
            .unwrap_or_else(|e| panic!("decompress q={quality} failed: {e}"));
        assert_eq!(
            decompressed, input,
            "quality={quality} repeated-pattern round-trip failed"
        );
    }
}

/// Quality-1 on 256-byte matches specifically (256 = 15 * 17 + 1, i.e. the
/// length that triggered the tail-of-1 split bug).
#[test]
fn test_quality1_max_match_length_boundary() {
    // Create data where LZ77 finds 256-byte matches.
    let mut input = Vec::with_capacity(512);
    input.extend_from_slice(b"hello world, this is a test pattern repeated many times for brotli testing purposes. padding to 128 bytes total so it fits into a single lz77 match chunk of size 256 bytes indeed!!");
    assert!(input.len() >= 4, "base block must be ≥ min_match_len=4");
    // Repeat enough to get a 256-byte match.
    while input.len() < 512 {
        let chunk: Vec<u8> = input.clone();
        input.extend_from_slice(&chunk[..chunk.len().min(256)]);
    }
    let input = &input[..512];

    let compressed = compress(input, 1).expect("compress q=1 max-match boundary");
    let decompressed = decompress(&compressed).expect("decompress q=1 max-match boundary");
    assert_eq!(
        decompressed, input,
        "q=1 max-match-length boundary round-trip failed"
    );
}

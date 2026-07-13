//! Decoder robustness against truncated and corrupted streams.
//!
//! Brotli carries no checksum, so a bit flip can legitimately decode to a
//! *different valid* stream — that is inherent to the format and cannot be
//! detected. What the decoder MUST guarantee:
//!
//! - it never panics, hangs, or allocates unboundedly on malformed input,
//! - every proper prefix (truncation) of a valid stream returns `Err` —
//!   a truncated stream can never silently succeed, because phantom
//!   zero-padded bits are never consumed as code bits.

use oxiarc_brotli::{compress, decompress};

fn sample_inputs() -> Vec<Vec<u8>> {
    let mut state = 0x00D1_CE5Eu64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u8
    };
    vec![
        b"Hello, Brotli! Hello, Brotli! Hello, Brotli!".to_vec(),
        vec![0u8; 4096],
        b"The quick brown fox jumps over the lazy dog. ".repeat(100),
        (0..2048).map(|_| next()).collect(),
    ]
}

/// Every proper prefix of a valid stream must fail to decode.
#[test]
fn test_all_truncations_rejected() {
    for (i, data) in sample_inputs().into_iter().enumerate() {
        for quality in [1u32, 6] {
            let stream = compress(&data, quality).expect("compress");
            for cut in 0..stream.len() {
                let result = decompress(&stream[..cut]);
                assert!(
                    result.is_err(),
                    "input {i} q{quality}: truncation to {cut}/{} bytes returned Ok",
                    stream.len()
                );
            }
        }
    }
}

/// Single-bit flips anywhere in the stream must never panic and must
/// complete quickly. (An `Ok` with different bytes is possible by design —
/// brotli has no integrity check — but resource use must stay bounded.)
#[test]
fn test_bit_flips_never_panic() {
    for data in sample_inputs() {
        let stream = compress(&data, 6).expect("compress");
        for byte_idx in 0..stream.len() {
            for bit in [0u8, 3, 7] {
                let mut corrupted = stream.clone();
                corrupted[byte_idx] ^= 1 << bit;
                // Must return (Ok or Err) without panicking; output size is
                // bounded by the decoder's internal output-size guard.
                let _ = decompress(&corrupted);
            }
        }
    }
}

/// Random garbage must be rejected or decoded within bounds — never panic.
#[test]
fn test_random_garbage_never_panics() {
    let mut state = 0x0BAD_5EED_u64;
    let mut next = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as u8
    };
    for len in [1usize, 2, 3, 7, 16, 64, 256, 1024] {
        for _ in 0..50 {
            let garbage: Vec<u8> = (0..len).map(|_| next()).collect();
            let _ = decompress(&garbage);
        }
    }
}

/// The former CPU-DoS shape: streams whose literal code lengths are mostly
/// long (11-15 bits) must decode in O(1) per symbol via the two-level
/// table. A corrupted ~100 KB input previously took ~1.9 s; with the
/// two-level table the whole loop below is near-instant.
#[test]
fn test_long_code_decode_is_fast() {
    // High-entropy input forces near-uniform literal codes; decoding must
    // complete far below the old pathological threshold.
    let mut state = 0x5EED_CAFEu64;
    let data: Vec<u8> = (0..100_000)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect();
    let stream = compress(&data, 6).expect("compress");
    let start = std::time::Instant::now();
    let decoded = decompress(&stream).expect("decompress");
    let elapsed = start.elapsed();
    assert_eq!(decoded, data);
    assert!(
        elapsed.as_millis() < 500,
        "decode took {elapsed:?}; two-level table regression?"
    );
}

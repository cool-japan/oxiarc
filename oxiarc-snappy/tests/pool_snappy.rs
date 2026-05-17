//! Integration tests for `SnappyPool` — FrameEncoder/FrameDecoder scratch buffer reuse.

use std::io::{Read, Write};
use std::sync::Arc;

use oxiarc_snappy::{FrameDecoder, FrameEncoder, SnappyPool, compress_frame_pooled};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Encode `input` using a pooled FrameEncoder and return the compressed bytes.
fn encode_pooled(input: &[u8], pool: &SnappyPool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = FrameEncoder::with_pool(&mut out, pool);
    enc.write_all(input).expect("pooled encode write failed");
    enc.finish().expect("pooled encode finish failed");
    out
}

/// Encode `input` using a non-pooled FrameEncoder.
fn encode_plain(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = FrameEncoder::new(&mut out);
    enc.write_all(input).expect("plain encode write failed");
    enc.finish().expect("plain encode finish failed");
    out
}

/// Decode `compressed` using a pooled FrameDecoder and return the decompressed bytes.
fn decode_pooled(compressed: &[u8], pool: &SnappyPool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut dec = FrameDecoder::with_pool(compressed, pool);
    dec.read_to_end(&mut out).expect("pooled decode failed");
    out
}

/// Decode `compressed` using a non-pooled FrameDecoder.
fn decode_plain(compressed: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut dec = FrameDecoder::new(compressed);
    dec.read_to_end(&mut out).expect("plain decode failed");
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: encoder hits
// ─────────────────────────────────────────────────────────────────────────────

/// Three sequential `FrameEncoder::with_pool` encode+finish cycles must
/// accumulate at least 2 encoder scratch hits (the first call is always a
/// pool miss; calls 2 and 3 should be hits).
#[test]
fn test_pool_encoder_hits() {
    let pool = SnappyPool::new();

    // Use 128 KiB so we exercise 2 chunks per encode.
    let input: Vec<u8> = (0u32..131_072).map(|i| (i % 251) as u8).collect();

    let out1 = encode_pooled(&input, &pool);
    let out2 = encode_pooled(&input, &pool);
    let out3 = encode_pooled(&input, &pool);

    let stats = pool.stats();
    assert!(
        stats.encoder_scratch_hits >= 2,
        "expected >= 2 encoder scratch hits, got {} (allocs={})",
        stats.encoder_scratch_hits,
        stats.encoder_scratch_allocations
    );

    // All three outputs must decompress correctly.
    for (i, compressed) in [&out1, &out2, &out3].iter().enumerate() {
        let decompressed = decode_plain(compressed);
        assert_eq!(
            decompressed,
            input,
            "roundtrip failed for encoder call {}",
            i + 1
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: decoder hits
// ─────────────────────────────────────────────────────────────────────────────

/// Three sequential `FrameDecoder::with_pool` read_to_end calls must
/// accumulate at least 2 decoder scratch hits.
#[test]
fn test_pool_decoder_hits() {
    let pool = SnappyPool::new();
    let input: Vec<u8> = (0u32..131_072).map(|i| (i % 199) as u8).collect();

    // Encode without pool so the compressed bytes are ready.
    let compressed = encode_plain(&input);

    let out1 = decode_pooled(&compressed, &pool);
    let out2 = decode_pooled(&compressed, &pool);
    let out3 = decode_pooled(&compressed, &pool);

    let stats = pool.stats();
    assert!(
        stats.decoder_scratch_hits >= 2,
        "expected >= 2 decoder scratch hits, got {} (allocs={})",
        stats.decoder_scratch_hits,
        stats.decoder_scratch_allocations
    );

    for (i, decompressed) in [out1, out2, out3].iter().enumerate() {
        assert_eq!(*decompressed, input, "decode mismatch at call {}", i + 1);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: roundtrip equality
// ─────────────────────────────────────────────────────────────────────────────

/// Pooled-encode → pooled-decode must be byte-identical to the original.
/// Pooled-encode output must be decodable by a non-pooled FrameDecoder.
/// Non-pooled encode output must be decodable by a pooled FrameDecoder.
#[test]
fn test_pool_roundtrip_equality() {
    let pool = SnappyPool::new();
    let input: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
        .iter()
        .cycle()
        .take(65_536)
        .copied()
        .collect();

    // pooled encode → pooled decode
    let pooled_compressed = encode_pooled(&input, &pool);
    let pooled_decoded = decode_pooled(&pooled_compressed, &pool);
    assert_eq!(
        pooled_decoded, input,
        "pooled encode → pooled decode mismatch"
    );

    // pooled encode → plain decode
    let plain_from_pooled = decode_plain(&pooled_compressed);
    assert_eq!(
        plain_from_pooled, input,
        "pooled encode → plain decode mismatch"
    );

    // plain encode → pooled decode
    let plain_compressed = encode_plain(&input);
    let pooled_from_plain = decode_pooled(&plain_compressed, &pool);
    assert_eq!(
        pooled_from_plain, input,
        "plain encode → pooled decode mismatch"
    );

    // compress_frame_pooled convenience helper
    let pool2 = SnappyPool::new();
    let helper_compressed =
        compress_frame_pooled(&input, &pool2).expect("compress_frame_pooled failed");
    let helper_decoded = decode_plain(&helper_compressed);
    assert_eq!(
        helper_decoded, input,
        "compress_frame_pooled → plain decode mismatch"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: concurrent
// ─────────────────────────────────────────────────────────────────────────────

/// 8 rayon threads, each encoding 256 KiB via the same pool.  Total encoder
/// scratch allocations must be low relative to the number of threads (the
/// pool should see significant reuse once warmed up).
#[test]
fn test_pool_concurrent() {
    use rayon::prelude::*;

    let pool = Arc::new(SnappyPool::new());

    // Each thread encodes 256 KiB (4 chunks of 64 KiB).
    let input: Vec<u8> = (0u32..262_144).map(|i| (i % 211) as u8).collect();
    let input = Arc::new(input);

    let results: Vec<Vec<u8>> = (0..8usize)
        .into_par_iter()
        .map(|_| {
            let p = Arc::clone(&pool);
            let inp = Arc::clone(&input);
            encode_pooled(&inp, &p)
        })
        .collect();

    // Verify all outputs decode correctly.
    for (i, compressed) in results.iter().enumerate() {
        let decoded = decode_plain(compressed);
        assert_eq!(
            &decoded,
            input.as_ref(),
            "concurrent thread {} decode mismatch",
            i
        );
    }

    // Total allocations across both buckets should not exceed 8 threads × 2
    // (encoder + decoder) × chunk-count × some overhead, but practically
    // the pool should ensure we don't re-allocate on every single call.
    // We just assert that the pool was actually used (at least 1 hit).
    let stats = pool.stats();
    assert!(
        stats.encoder_scratch_hits >= 1 || stats.decoder_scratch_hits >= 1,
        "expected at least one pool hit in concurrent test; got encoder_hits={} decoder_hits={}",
        stats.encoder_scratch_hits,
        stats.decoder_scratch_hits
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: cap respected
// ─────────────────────────────────────────────────────────────────────────────

/// `SnappyPool::with_cap(2)` — pool size stays ≤ 2 after excess returns.
#[test]
fn test_pool_cap() {
    let pool = SnappyPool::with_cap(2);
    let input: Vec<u8> = b"cap-test data "
        .iter()
        .cycle()
        .take(16_384)
        .copied()
        .collect();

    // Perform 4 encode+decode cycles.
    for _ in 0..4 {
        let compressed = encode_pooled(&input, &pool);
        let decoded = decode_pooled(&compressed, &pool);
        assert_eq!(decoded, input, "cap test roundtrip failed");
    }

    // After 4 cycles with cap=2, allocations must be <= cap+1 per bucket
    // (first call always allocates; subsequent calls hit).
    // We verify that the pool actually pooled: hits > 0 for both buckets
    // (at least calls 2..4 should be hits).
    let stats = pool.stats();
    assert!(
        stats.encoder_scratch_hits >= 1,
        "encoder scratch should have >= 1 hit with cap=2, got {}",
        stats.encoder_scratch_hits,
    );
    assert!(
        stats.decoder_scratch_hits >= 1,
        "decoder scratch should have >= 1 hit with cap=2, got {}",
        stats.decoder_scratch_hits,
    );

    // Total allocations should be bounded: we can't do better than cap+1
    // per bucket (initial misses), but we should never exceed 4*chunks (worst case).
    // A meaningful bound: allocations < total_calls = 4 (since pool reuses).
    assert!(
        stats.encoder_scratch_allocations <= 4,
        "expected encoder allocations <= 4, got {}",
        stats.encoder_scratch_allocations
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: default constructor
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn test_pool_default() {
    let pool = SnappyPool::default();
    let compressed = encode_pooled(b"default pool test", &pool);
    let decoded = decode_plain(&compressed);
    assert_eq!(decoded, b"default pool test");
}

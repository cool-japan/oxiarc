//! Criterion benchmarks for `oxiarc-brotli`.
//!
//! The decode groups exist to answer one question: **what does bounded,
//! resumable decoding cost?** Each payload is decoded three ways over the same
//! bytes — the one-shot [`decompress`] (the baseline), the push decoder in one
//! call, and the push decoder fed in realistic chunks — so the ratio is read
//! straight off the report. The target is at least 85 % of one-shot
//! throughput at 64 KiB chunks.
//!
//! `bench_decode_tiny_chunks` is deliberately punishing (1-byte input, 1-byte
//! output) and is expected to be slow; it is here as a correctness-shaped
//! performance floor, not a target.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use oxiarc_brotli::{
    BrotliParams, BrotliStatus, BrotliStream, compress, compress_with_params, decompress,
};
use oxiarc_core::traits::FlushMode;

/// Deterministic pseudo-random bytes.
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Benchmark payloads: compressible text, a match-heavy body, and
/// semi-random data that forces uncompressed meta-blocks.
fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    let mut semi = Vec::new();
    for (i, chunk) in pseudo_random(1 << 20, 7).chunks(16).enumerate() {
        semi.extend_from_slice(format!("line {i}: ").as_bytes());
        for b in chunk {
            semi.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        semi.push(b'\n');
    }
    vec![
        (
            "text_1m",
            b"The quick brown fox jumps over the lazy dog. ".repeat(24_000),
        ),
        ("matchy_1m", vec![0x5Au8; 1 << 20]),
        ("semi_random_3m", semi),
    ]
}

/// Drive the push decoder to completion with fixed chunk sizes.
fn push_decode(compressed: &[u8], in_chunk: usize, out: &mut [u8]) -> usize {
    let mut stream = BrotliStream::new();
    let mut produced = 0usize;
    let mut pos = 0usize;
    loop {
        let end = (pos + in_chunk).min(compressed.len());
        let flush = if end == compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&compressed[pos..end], out, flush)
            .expect("bench decode");
        pos += progress.consumed;
        produced += progress.produced;
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    produced
}

fn bench_compress_small(c: &mut Criterion) {
    let data = b"Hello, Brotli! This is a benchmark for compression.";
    c.bench_function("brotli_compress_small_q0", |b| {
        b.iter(|| compress(black_box(data), 0))
    });
}

fn bench_compress_repeated(c: &mut Criterion) {
    let data = "abcdefgh".repeat(1000);
    c.bench_function("brotli_compress_repeated_q6", |b| {
        b.iter(|| compress(black_box(data.as_bytes()), 6))
    });
}

/// The headline comparison: one-shot versus bounded incremental decoding of
/// the same stream.
fn bench_decode_oneshot_vs_incremental(c: &mut Criterion) {
    for (name, data) in payloads() {
        let compressed = compress(&data, 5).expect("compress");
        let mut group = c.benchmark_group(format!("brotli_decode_{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        group.bench_function("oneshot", |b| {
            b.iter(|| decompress(black_box(&compressed)).expect("decode"))
        });

        let mut out = vec![0u8; 256 * 1024];
        group.bench_function("incremental_whole", |b| {
            b.iter(|| push_decode(black_box(&compressed), compressed.len(), &mut out))
        });
        group.bench_function("incremental_64k_chunks", |b| {
            b.iter(|| push_decode(black_box(&compressed), 64 * 1024, &mut out))
        });
        group.bench_function("incremental_1k_chunks", |b| {
            b.iter(|| push_decode(black_box(&compressed), 1024, &mut out))
        });
        group.finish();
    }
}

/// One byte in, one byte out — the worst case the resumable machinery can be
/// asked to handle. Small payload; expected to be slow.
fn bench_decode_tiny_chunks(c: &mut Criterion) {
    let data = b"The quick brown fox jumps over the lazy dog. ".repeat(200);
    let compressed = compress(&data, 5).expect("compress");
    let mut group = c.benchmark_group("brotli_decode_tiny_chunks");
    group.throughput(Throughput::Bytes(data.len() as u64));
    let mut out = [0u8; 1];
    group.bench_function("incremental_1x1", |b| {
        b.iter(|| push_decode(black_box(&compressed), 1, &mut out))
    });
    group.finish();
}

/// The bounded-memory trade, measured directly: the same payload decoded with a
/// cache-resident 1 KiB window and with a 4 MiB one.
///
/// The one-shot decoder is unaffected — it uses its output `Vec` as the window
/// either way — so the spread between these two groups *is* the cost of holding
/// a real ring, which is what buys `O(window)` instead of `O(body)` memory.
fn bench_decode_window_sizes(c: &mut Criterion) {
    let data = pseudo_random(1 << 20, 3);
    for lgwin in [10u32, 22] {
        let params = BrotliParams {
            quality: 5,
            lgwin,
            lgblock: 0,
        };
        let compressed = compress_with_params(&data, &params).expect("compress");
        let mut group = c.benchmark_group(format!("brotli_decode_window_lgwin{lgwin}"));
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_function("oneshot", |b| {
            b.iter(|| decompress(black_box(&compressed)).expect("decode"))
        });
        let mut out = vec![0u8; 256 * 1024];
        group.bench_function("incremental_64k_chunks", |b| {
            b.iter(|| push_decode(black_box(&compressed), 64 * 1024, &mut out))
        });
        group.finish();
    }
}

criterion_group!(
    benches,
    bench_compress_small,
    bench_compress_repeated,
    bench_decode_oneshot_vs_incremental,
    bench_decode_tiny_chunks,
    bench_decode_window_sizes
);
criterion_main!(benches);

//! Benchmark: [`XzDecoder`] reuse vs. a fresh decoder per call.
//!
//! Measures the concrete case the reusable decoder context targets: TIFF
//! `Compression = 34925` (`oxiarc_lzma::xz`'s module docs name this
//! directly) writes one complete `.xz` stream per strip or tile, and a
//! large image can carry thousands of them, all sharing one LZMA2
//! dictionary size. This benchmark decodes `STRIP_COUNT` copies of one
//! `STRIP_SIZE`-byte compressed stream two ways:
//!
//! - `reuse_one_decoder` — one [`XzDecoder`], called once per strip.
//! - `fresh_decoder_per_call` — [`oxiarc_lzma::xz::decompress_into`], the
//!   thin one-shot wrapper that allocates a fresh `XzDecoder` (and so a
//!   fresh LZMA2 dictionary buffer) on every call.
//!
//! Both loops decode byte-identical output; only the allocation pattern
//! differs (see `oxiarc-lzma/src/xz/decoder.rs` tests for the byte-identity
//! proof — this file only measures speed).

use criterion::{Criterion, criterion_group, criterion_main};
use oxiarc_lzma::xz::{self, XzDecoder};
use std::hint::black_box;

/// Number of independent `.xz` streams decoded per benchmark iteration —
/// the "thousands of strips in one image" shape from the design brief.
const STRIP_COUNT: usize = 1000;

/// Size of each strip's *uncompressed* payload — a plausible single-row or
/// small-tile TIFF strip.
const STRIP_SIZE: usize = 64 * 1024;

/// Deterministic xorshift PRNG, matching the crate's own test/bench
/// convention (no external `rand` dependency needed for a reproducible,
/// effectively-incompressible fixture).
fn xorshift_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// One compressed `.xz` stream shaped like one TIFF strip: `STRIP_SIZE`
/// bytes of pseudo-random pixel-like data, and its plaintext for sizing the
/// output buffer.
fn strip_stream() -> (Vec<u8>, Vec<u8>) {
    let payload = xorshift_bytes(0x5713_57A1_0000_0001, STRIP_SIZE);
    let stream = xz::compress(&payload, 6).expect("compress a strip-shaped payload");
    (payload, stream)
}

fn bench_xz_decoder_reuse(c: &mut Criterion) {
    let (payload, stream) = strip_stream();

    let mut group = c.benchmark_group("xz_decoder_reuse");
    // Each measured iteration already decodes STRIP_COUNT streams, so a
    // smaller sample count keeps the benchmark's wall time reasonable
    // while still giving Criterion enough samples for a mean + stddev.
    group.sample_size(10);

    group.bench_function("reuse_one_decoder", |b| {
        b.iter(|| {
            let mut decoder = XzDecoder::new();
            let mut out = vec![0u8; payload.len()];
            for _ in 0..STRIP_COUNT {
                let written = decoder
                    .decompress_into(black_box(&stream), &mut out)
                    .expect("reused decode");
                black_box(written);
            }
            black_box(out)
        });
    });

    group.bench_function("fresh_decoder_per_call", |b| {
        b.iter(|| {
            let mut out = vec![0u8; payload.len()];
            for _ in 0..STRIP_COUNT {
                // What a caller gets by NOT holding an `XzDecoder` across
                // calls: the free function builds a fresh one every time
                // (see `oxiarc-lzma/src/xz/header.rs`'s `decompress_into`).
                let written = xz::decompress_into(black_box(&stream), &mut out)
                    .expect("fresh-decoder decode");
                black_box(written);
            }
            black_box(out)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_xz_decoder_reuse);
criterion_main!(benches);

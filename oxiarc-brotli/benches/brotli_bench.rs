use criterion::{Criterion, black_box, criterion_group, criterion_main};
use oxiarc_brotli::compress;

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

criterion_group!(benches, bench_compress_small, bench_compress_repeated);
criterion_main!(benches);

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_zstd::{compress, decompress};
use std::hint::black_box;

#[cfg(feature = "parallel")]
use oxiarc_zstd::compress_parallel;

fn bench_zstd_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("zstd_compression");

    for size in [1_000, 10_000, 100_000, 1_000_000, 10_000_000] {
        let data = vec![0xAAu8; size];
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("serial", size), &data, |b, data| {
            b.iter(|| {
                let compressed = compress(data).expect("compression failed");
                black_box(compressed);
            });
        });

        #[cfg(feature = "parallel")]
        group.bench_with_input(BenchmarkId::new("parallel", size), &data, |b, data| {
            b.iter(|| {
                let compressed = compress_parallel(data).expect("compression failed");
                black_box(compressed);
            });
        });
    }

    group.finish();
}

fn bench_zstd_decompression(c: &mut Criterion) {
    let mut group = c.benchmark_group("zstd_decompression");

    for size in [1_000, 10_000, 100_000, 1_000_000] {
        let data = vec![0xAAu8; size];
        let compressed = compress(&data).expect("compression failed");

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("decompress", size),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress(compressed).expect("decompression failed");
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

fn bench_zstd_rle_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("zstd_rle");

    // RLE-friendly data (all same byte)
    let data = vec![0xFFu8; 5_000_000];
    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("serial_rle", |b| {
        b.iter(|| {
            let compressed = compress(&data).expect("compression failed");
            black_box(compressed);
        });
    });

    #[cfg(feature = "parallel")]
    group.bench_function("parallel_rle", |b| {
        b.iter(|| {
            let compressed = compress_parallel(&data).expect("compression failed");
            black_box(compressed);
        });
    });

    group.finish();
}

fn bench_zstd_mixed_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("zstd_mixed");

    // Mixed data (not RLE-able)
    let mut data = Vec::new();
    for i in 0..2_000_000 {
        data.push((i % 256) as u8);
    }

    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("serial_mixed", |b| {
        b.iter(|| {
            let compressed = compress(&data).expect("compression failed");
            black_box(compressed);
        });
    });

    #[cfg(feature = "parallel")]
    group.bench_function("parallel_mixed", |b| {
        b.iter(|| {
            let compressed = compress_parallel(&data).expect("compression failed");
            black_box(compressed);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_zstd_compression,
    bench_zstd_decompression,
    bench_zstd_rle_data,
    bench_zstd_mixed_data
);
criterion_main!(benches);

//! Comprehensive performance benchmarks for oxiarc-bzip2
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed at different levels (1-9)
//! - BWT (Burrows-Wheeler Transform) performance
//! - Performance across various data patterns
//! - Throughput measurements (MB/s)
//! - Compression ratios for different scenarios
//! - Block size impact on performance

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_bzip2::{CompressionLevel, bwt, compress, decompress};
use std::hint::black_box;

#[cfg(feature = "parallel")]
use oxiarc_bzip2::compress_parallel;

/// Type alias for pattern generator functions
type PatternGenerator = fn(usize) -> Vec<u8>;

/// Generate test data patterns for benchmarking
mod test_data {
    /// Uniform data - all bytes are the same (best for RLE)
    pub fn uniform(size: usize) -> Vec<u8> {
        vec![0xAA; size]
    }

    /// Random data - no patterns (worst compression)
    pub fn random(size: usize) -> Vec<u8> {
        // Simple PRNG for reproducible random data
        let mut data = Vec::with_capacity(size);
        let mut seed: u64 = 0x123456789ABCDEF0;
        for _ in 0..size {
            // Linear congruential generator
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            data.push((seed >> 32) as u8);
        }
        data
    }

    /// Repetitive pattern - good for BWT
    pub fn repetitive(size: usize) -> Vec<u8> {
        let pattern = b"TOBEORNOTTOBEORTOBEORNOT";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(pattern.len());
            data.extend_from_slice(&pattern[..chunk_size]);
        }
        data
    }

    /// Text-like data - realistic scenario
    pub fn text_like(size: usize) -> Vec<u8> {
        let text = b"The quick brown fox jumps over the lazy dog. \
                     Pack my box with five dozen liquor jugs. \
                     How vexingly quick daft zebras jump! \
                     Lorem ipsum dolor sit amet, consectetur adipiscing elit. ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(text.len());
            data.extend_from_slice(&text[..chunk_size]);
        }
        data
    }

    /// Binary executable-like data - mixed patterns
    pub fn binary_like(size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        let mut seed: u64 = 0x123456789ABCDEF0;

        // Simulate sections of an executable
        let section_size = size / 4;

        // Code section - more repetitive patterns
        for _ in 0..section_size {
            data.push((seed % 256) as u8);
            if seed % 10 < 3 {
                seed = seed.wrapping_add(1);
            }
        }

        // Data section - moderate patterns
        for _ in 0..section_size {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            data.push((seed >> 32) as u8);
        }

        // Zero section - highly compressible
        data.extend(std::iter::repeat_n(0, section_size));

        // Random section - less compressible
        for _ in 0..(size - data.len()) {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            data.push((seed >> 32) as u8);
        }

        data
    }

    /// Highly compressible data - long repeated sequences
    pub fn compressible(size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        let patterns = [
            &b"aaaaaaaaaa"[..],
            &b"bbbbbbbbbb"[..],
            &b"cccccccccc"[..],
            &b"0000000000"[..],
        ];

        let mut pattern_idx = 0;
        while data.len() < size {
            let pattern = patterns[pattern_idx % patterns.len()];
            let remaining = size - data.len();
            let chunk_size = remaining.min(pattern.len());
            data.extend_from_slice(&pattern[..chunk_size]);
            pattern_idx += 1;
        }

        data
    }
}

/// Standard data sizes for benchmarking
/// Note: BWT has quadratic worst-case for highly repetitive data, so we use moderate sizes
mod data_sizes {
    pub const TINY: usize = 1024; // 1 KB
    pub const SMALL: usize = 10 * 1024; // 10 KB
    pub const MEDIUM: usize = 64 * 1024; // 64 KB
    pub const LARGE: usize = 256 * 1024; // 256 KB (within BZip2 block size limits)
}

/// Benchmark compression levels (1-9)
fn bench_compression_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_levels");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for level in [1, 3, 5, 7, 9] {
        let comp_level = CompressionLevel::new(level);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), comp_level).unwrap();
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark BWT (Burrows-Wheeler Transform) performance
fn bench_bwt_transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("bwt_transform");

    let sizes = [
        ("1KB", data_sizes::TINY),
        ("10KB", data_sizes::SMALL),
        ("64KB", data_sizes::MEDIUM),
    ];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let (transformed, orig_ptr) = bwt::transform(black_box(data));
                black_box((transformed, orig_ptr));
            });
        });
    }

    group.finish();
}

/// Benchmark BWT inverse transform performance
fn bench_bwt_inverse(c: &mut Criterion) {
    let mut group = c.benchmark_group("bwt_inverse");

    let sizes = [
        ("1KB", data_sizes::TINY),
        ("10KB", data_sizes::SMALL),
        ("64KB", data_sizes::MEDIUM),
    ];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);
        let (transformed, orig_ptr) = bwt::transform(&data);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_name),
            &(transformed, orig_ptr),
            |b, (transformed, orig_ptr)| {
                b.iter(|| {
                    let reconstructed = bwt::inverse_transform(black_box(transformed), *orig_ptr);
                    black_box(reconstructed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark compression speed for different data types
fn bench_compression_data_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_data_types");

    let patterns: [(&str, PatternGenerator); 6] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;
    let level = CompressionLevel::default();

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), level).unwrap();
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark compression speed for different input sizes
fn bench_compression_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_sizes");

    let sizes = [
        ("1KB", data_sizes::TINY),
        ("10KB", data_sizes::SMALL),
        ("64KB", data_sizes::MEDIUM),
        ("256KB", data_sizes::LARGE),
    ];

    let level = CompressionLevel::default();

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data), level).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

/// Benchmark decompression speed
fn bench_decompression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_speed");

    let patterns: [(&str, PatternGenerator); 6] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;
    let level = CompressionLevel::default();

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = compress(&original, level).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress(&black_box(compressed)[..]).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark decompression speed for different sizes
fn bench_decompression_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_sizes");

    let sizes = [
        ("1KB", data_sizes::TINY),
        ("10KB", data_sizes::SMALL),
        ("64KB", data_sizes::MEDIUM),
        ("256KB", data_sizes::LARGE),
    ];

    let level = CompressionLevel::default();

    for (size_name, size) in sizes {
        let original = test_data::text_like(size);
        let compressed = compress(&original, level).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress(&black_box(compressed)[..]).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark compression ratios
fn bench_compression_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_ratio");
    group.sample_size(10);

    let patterns: [(&str, PatternGenerator); 6] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        // Test multiple compression levels
        for level in [1, 5, 9] {
            let comp_level = CompressionLevel::new(level);
            let id = format!("{}/level_{}", pattern_name, level);

            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), comp_level).unwrap();
                    let ratio = data.len() as f64 / compressed.len() as f64;
                    black_box((compressed, ratio));
                });
            });
        }
    }

    group.finish();
}

/// Benchmark roundtrip (compress + decompress)
fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("roundtrip");

    let patterns: [(&str, PatternGenerator); 6] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;
    let level = CompressionLevel::default();

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), level).unwrap();
                    let decompressed = decompress(&compressed[..]).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark block size impact
fn bench_block_size_impact(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_size_impact");

    let data = test_data::text_like(data_sizes::LARGE);

    for level in [1, 3, 5, 7, 9] {
        let comp_level = CompressionLevel::new(level);
        let block_size = comp_level.block_size();

        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}k", block_size / 1000)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), comp_level).unwrap();
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark memory allocation patterns
fn bench_memory_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_allocation");

    let sizes = [
        ("1KB", data_sizes::TINY),
        ("10KB", data_sizes::SMALL),
        ("64KB", data_sizes::MEDIUM),
        ("256KB", data_sizes::LARGE),
    ];

    let level = CompressionLevel::default();

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression + decompression
                let compressed = compress(black_box(data), level).unwrap();
                let decompressed = decompress(&compressed[..]).unwrap();
                black_box((compressed, decompressed));
            });
        });
    }

    group.finish();
}

/// Benchmark parallel vs serial compression
#[cfg(feature = "parallel")]
fn bench_parallel_vs_serial(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel_vs_serial");

    let sizes = [
        ("100KB", 100 * 1024),
        ("1MB", 1024 * 1024),
        ("5MB", 5 * 1024 * 1024),
    ];

    let level = CompressionLevel::new(5);

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));

        // Serial compression
        group.bench_with_input(BenchmarkId::new("serial", size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data), level).unwrap();
                black_box(compressed);
            });
        });

        // Parallel compression
        group.bench_with_input(BenchmarkId::new("parallel", size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress_parallel(black_box(data), level).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

#[cfg(feature = "parallel")]
criterion_group!(
    benches,
    bench_compression_levels,
    bench_bwt_transform,
    bench_bwt_inverse,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_block_size_impact,
    bench_memory_allocation,
    bench_parallel_vs_serial,
);

#[cfg(not(feature = "parallel"))]
criterion_group!(
    benches,
    bench_compression_levels,
    bench_bwt_transform,
    bench_bwt_inverse,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_block_size_impact,
    bench_memory_allocation,
);

criterion_main!(benches);

//! Comprehensive performance benchmarks for oxiarc-lzma
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed at different levels (0-9)
//! - Performance across various data patterns
//! - Throughput measurements (MB/s)
//! - Compression ratios for different scenarios
//! - Memory efficiency and allocation patterns

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_lzma::{LzmaLevel, compress, decompress_bytes};
use std::hint::black_box;

/// Type alias for pattern generator functions
type PatternGenerator = fn(usize) -> Vec<u8>;

/// Generate test data patterns for benchmarking
mod test_data {
    /// Uniform data - all bytes are the same (best compression)
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

    /// Repetitive pattern - common in text files
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
mod data_sizes {
    pub const TINY: usize = 1024; // 1 KB
    pub const SMALL: usize = 10 * 1024; // 10 KB
    pub const MEDIUM: usize = 100 * 1024; // 100 KB
    pub const LARGE: usize = 1024 * 1024; // 1 MB
}

/// Benchmark compression speed across different levels
fn bench_compression_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_levels");

    let levels = [
        ("level_0_fast", LzmaLevel::new(0)),
        ("level_1", LzmaLevel::new(1)),
        ("level_3", LzmaLevel::new(3)),
        ("level_6_default", LzmaLevel::DEFAULT),
        ("level_9_best", LzmaLevel::BEST),
    ];

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for (name, level) in levels {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data), level).unwrap();
                black_box(compressed);
            });
        });
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
    let level = LzmaLevel::DEFAULT;

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
        ("100KB", data_sizes::MEDIUM),
        ("1MB", data_sizes::LARGE),
    ];

    let level = LzmaLevel::DEFAULT;

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
    let level = LzmaLevel::DEFAULT;

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = compress(&original, level).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress_bytes(black_box(compressed)).unwrap();
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
        ("100KB", data_sizes::MEDIUM),
        ("1MB", data_sizes::LARGE),
    ];

    let level = LzmaLevel::DEFAULT;

    for (size_name, size) in sizes {
        let original = test_data::text_like(size);
        let compressed = compress(&original, level).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress_bytes(black_box(compressed)).unwrap();
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
        for level in [0, 3, 6, 9] {
            let level_obj = LzmaLevel::new(level);
            let id = format!("{}/level_{}", pattern_name, level);

            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), level_obj).unwrap();
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
    let level = LzmaLevel::DEFAULT;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), level).unwrap();
                    let decompressed = decompress_bytes(&compressed).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark level vs size tradeoff
fn bench_level_size_tradeoff(c: &mut Criterion) {
    let mut group = c.benchmark_group("level_size_tradeoff");
    group.sample_size(10);

    let data = test_data::text_like(data_sizes::MEDIUM);

    for level in 0..=9 {
        let level_obj = LzmaLevel::new(level);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data), level_obj).unwrap();
                    let ratio = data.len() as f64 / compressed.len() as f64;
                    black_box((compressed, ratio));
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
        ("100KB", data_sizes::MEDIUM),
        ("1MB", data_sizes::LARGE),
    ];

    let level = LzmaLevel::DEFAULT;

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression + decompression
                let compressed = compress(black_box(data), level).unwrap();
                let decompressed = decompress_bytes(&compressed).unwrap();
                black_box((compressed, decompressed));
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_levels,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_level_size_tradeoff,
    bench_memory_allocation,
);
criterion_main!(benches);

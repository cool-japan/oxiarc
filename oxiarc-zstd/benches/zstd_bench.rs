//! Comprehensive performance benchmarks for oxiarc-zstd
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed with different strategies
//! - Performance across various data patterns
//! - Throughput measurements (MB/s)
//! - Compression ratios for different scenarios
//! - Impact of checksums and content size headers

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_zstd::{CompressionStrategy, ZstdEncoder, compress, compress_no_checksum, decompress};
use std::hint::black_box;

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

    /// JSON-like data - realistic structured text
    pub fn json_like(size: usize) -> Vec<u8> {
        let json = br#"{"name":"John Doe","age":30,"email":"john@example.com","active":true,"tags":["rust","compression","benchmark"]}"#;
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(json.len());
            data.extend_from_slice(&json[..chunk_size]);
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

/// Benchmark compression strategies
fn bench_compression_strategies(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_strategies");

    let strategies = [
        ("raw", CompressionStrategy::Raw),
        ("rle_only", CompressionStrategy::RleOnly),
    ];

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for (name, strategy) in strategies {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &data, |b, data| {
            b.iter(|| {
                let mut encoder = ZstdEncoder::new();
                encoder.set_strategy(strategy);
                let compressed = encoder.compress(black_box(data)).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

/// Benchmark compression with and without checksums
fn bench_checksum_impact(c: &mut Criterion) {
    let mut group = c.benchmark_group("checksum_impact");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("with_checksum"),
        &data,
        |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data)).unwrap();
                black_box(compressed);
            });
        },
    );

    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("no_checksum"),
        &data,
        |b, data| {
            b.iter(|| {
                let compressed = compress_no_checksum(black_box(data)).unwrap();
                black_box(compressed);
            });
        },
    );

    group.finish();
}

/// Benchmark compression speed for different data types
fn bench_compression_data_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_data_types");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("json", test_data::json_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data)).unwrap();
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

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data)).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

/// Benchmark decompression speed
fn bench_decompression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_speed");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("json", test_data::json_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = compress(&original).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress(black_box(compressed)).unwrap();
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

    for (size_name, size) in sizes {
        let original = test_data::text_like(size);
        let compressed = compress(&original).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress(black_box(compressed)).unwrap();
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

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("json", test_data::json_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data)).unwrap();
                    let ratio = data.len() as f64 / compressed.len() as f64;
                    black_box((compressed, ratio));
                });
            },
        );
    }

    group.finish();
}

/// Benchmark roundtrip (compress + decompress)
fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("roundtrip");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("json", test_data::json_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress(black_box(data)).unwrap();
                    let decompressed = decompress(&compressed).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark strategy vs data pattern
fn bench_strategy_vs_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("strategy_vs_pattern");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("json", test_data::json_like as PatternGenerator),
    ];

    let strategies = [
        ("raw", CompressionStrategy::Raw),
        ("rle", CompressionStrategy::RleOnly),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        for (strategy_name, strategy) in strategies {
            let id = format!("{}/{}", pattern_name, strategy_name);

            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let mut encoder = ZstdEncoder::new();
                    encoder.set_strategy(strategy);
                    let compressed = encoder.compress(black_box(data)).unwrap();
                    black_box(compressed);
                });
            });
        }
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

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression + decompression
                let compressed = compress(black_box(data)).unwrap();
                let decompressed = decompress(&compressed).unwrap();
                black_box((compressed, decompressed));
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_strategies,
    bench_checksum_impact,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_strategy_vs_pattern,
    bench_memory_allocation,
);
criterion_main!(benches);

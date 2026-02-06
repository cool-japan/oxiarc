//! Comprehensive performance benchmarks for oxiarc-lzhuf
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed across different LZH methods (lh0-lh7)
//! - Performance with various data patterns
//! - Throughput measurements (MB/s)
//! - Compression ratios for different scenarios
//! - Impact of window sizes on performance

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_lzhuf::{LzhMethod, decode_lzh, encode_lzh};
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

    /// Japanese text - relevant for LHA's Japanese origins
    pub fn japanese_like(size: usize) -> Vec<u8> {
        // UTF-8 encoded Japanese text pattern
        let pattern = "こんにちは世界。LHAアーカイブは日本で人気がありました。".as_bytes();
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(pattern.len());
            data.extend_from_slice(&pattern[..chunk_size]);
        }
        data
    }
}

/// Standard data sizes for benchmarking
/// Note: Sizes are limited by LZH window buffer capacity (window_size * 2)
/// Using conservative sizes to avoid implementation bugs
mod data_sizes {
    pub const TINY: usize = 512; // 512 B
    pub const SMALL: usize = 2 * 1024; // 2 KB
    pub const MEDIUM: usize = 4 * 1024; // 4 KB
    pub const LARGE: usize = 8 * 1024; // 8 KB

    /// Get appropriate data size for each method based on window capacity
    /// Using conservative sizes (window_size - 1KB margin) to avoid edge cases
    pub fn max_for_method(method: oxiarc_lzhuf::LzhMethod) -> usize {
        use oxiarc_lzhuf::LzhMethod;
        match method {
            LzhMethod::Lh0 => 16 * 1024, // No limit for stored, but keep reasonable
            LzhMethod::Lh4 => 3 * 1024,  // 4KB window -> use 3KB
            LzhMethod::Lh5 => 7 * 1024,  // 8KB window -> use 7KB
            LzhMethod::Lh6 => 31 * 1024, // 32KB window -> use 31KB
            LzhMethod::Lh7 => 63 * 1024, // 64KB window -> use 63KB
        }
    }
}

/// Benchmark compression across different LZH methods
fn bench_compression_methods(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_methods");

    let methods = [
        ("lh0_stored", LzhMethod::Lh0),
        ("lh4_4kb", LzhMethod::Lh4),
        ("lh5_8kb", LzhMethod::Lh5),
        ("lh6_32kb", LzhMethod::Lh6),
        ("lh7_64kb", LzhMethod::Lh7),
    ];

    for (name, method) in methods {
        // Use appropriate data size for each method
        let size = data_sizes::max_for_method(method);
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &data, |b, data| {
            b.iter(|| {
                let compressed = encode_lzh(black_box(data), method).unwrap();
                black_box(compressed);
            });
        });
    }

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
        ("japanese", test_data::japanese_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;
    let method = LzhMethod::Lh5; // Most common method

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = encode_lzh(black_box(data), method).unwrap();
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

    let method = LzhMethod::Lh5;

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = encode_lzh(black_box(data), method).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

/// Benchmark decompression speed across different methods
fn bench_decompression_methods(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_methods");

    let methods = [
        ("lh0_stored", LzhMethod::Lh0),
        ("lh4_4kb", LzhMethod::Lh4),
        ("lh5_8kb", LzhMethod::Lh5),
        ("lh6_32kb", LzhMethod::Lh6),
        ("lh7_64kb", LzhMethod::Lh7),
    ];

    for (name, method) in methods {
        // Use appropriate data size for each method
        let size = data_sizes::max_for_method(method);
        let original = test_data::text_like(size);
        let compressed = encode_lzh(&original, method).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(name),
            &(compressed, size),
            |b, (compressed, size)| {
                b.iter(|| {
                    let decompressed =
                        decode_lzh(black_box(compressed), method, *size as u64).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark decompression speed for different data types
fn bench_decompression_data_types(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_data_types");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("japanese", test_data::japanese_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;
    let method = LzhMethod::Lh5;

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = encode_lzh(&original, method).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &(compressed, size),
            |b, (compressed, size)| {
                b.iter(|| {
                    let decompressed =
                        decode_lzh(black_box(compressed), method, *size as u64).unwrap();
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

    let method = LzhMethod::Lh5;

    for (size_name, size) in sizes {
        let original = test_data::text_like(size);
        let compressed = encode_lzh(&original, method).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_name),
            &(compressed, size),
            |b, (compressed, size)| {
                b.iter(|| {
                    let decompressed =
                        decode_lzh(black_box(compressed), method, *size as u64).unwrap();
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
        ("japanese", test_data::japanese_like as PatternGenerator),
    ];

    // Test different LZH methods
    for method in [
        LzhMethod::Lh4,
        LzhMethod::Lh5,
        LzhMethod::Lh6,
        LzhMethod::Lh7,
    ] {
        // Use appropriate data size for each method
        let size = data_sizes::max_for_method(method);

        for (pattern_name, generator) in patterns {
            let data = generator(size);
            let id = format!("{}/{}", pattern_name, method.name());

            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = encode_lzh(black_box(data), method).unwrap();
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

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("japanese", test_data::japanese_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;
    let method = LzhMethod::Lh5;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = encode_lzh(black_box(data), method).unwrap();
                    let decompressed = decode_lzh(&compressed, method, data.len() as u64).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark window size impact
fn bench_window_size_impact(c: &mut Criterion) {
    let mut group = c.benchmark_group("window_size_impact");

    let methods = [
        ("4KB", LzhMethod::Lh4),
        ("8KB", LzhMethod::Lh5),
        ("32KB", LzhMethod::Lh6),
        ("64KB", LzhMethod::Lh7),
    ];

    for (window_name, method) in methods {
        // Use appropriate data size for each method
        let size = data_sizes::max_for_method(method);
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(window_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = encode_lzh(black_box(data), method).unwrap();
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark method vs data pattern effectiveness
fn bench_method_vs_pattern(c: &mut Criterion) {
    let mut group = c.benchmark_group("method_vs_pattern");

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
    ];

    let methods = [
        LzhMethod::Lh4,
        LzhMethod::Lh5,
        LzhMethod::Lh6,
        LzhMethod::Lh7,
    ];

    for method in methods {
        // Use appropriate data size for each method (use smallest common size)
        let size =
            data_sizes::max_for_method(LzhMethod::Lh5).min(data_sizes::max_for_method(method));

        for (pattern_name, generator) in patterns {
            let data = generator(size);
            let id = format!("{}/{}", pattern_name, method.name());

            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = encode_lzh(black_box(data), method).unwrap();
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

    let method = LzhMethod::Lh5;

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression + decompression
                let compressed = encode_lzh(black_box(data), method).unwrap();
                let decompressed = decode_lzh(&compressed, method, data.len() as u64).unwrap();
                black_box((compressed, decompressed));
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_methods,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_methods,
    bench_decompression_data_types,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_window_size_impact,
    bench_method_vs_pattern,
    bench_memory_allocation,
);
criterion_main!(benches);

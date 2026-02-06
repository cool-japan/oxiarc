//! Comprehensive performance benchmarks for oxiarc-lz4
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed (LZ4 Fast vs LZ4-HC)
//! - Performance across various data patterns
//! - Throughput measurements (MB/s)
//! - Compression ratios for different scenarios
//! - Block vs Frame format performance
//! - Impact of HC compression levels (0-12)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_lz4::{
    BlockMaxSize, FrameDescriptor, HcLevel, compress, compress_block, compress_hc,
    compress_hc_level, compress_with_options, decompress, decompress_block,
};
use std::hint::black_box;

#[cfg(feature = "parallel")]
use oxiarc_lz4::compress_parallel;

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

    /// JSON-like data - realistic structured text
    pub fn json_like(size: usize) -> Vec<u8> {
        let json = br#"{"name":"John Doe","age":30,"email":"john@example.com","active":true,"tags":["rust","compression","lz4"]}"#;
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

/// Benchmark LZ4 Fast vs LZ4-HC compression
fn bench_compression_modes(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_modes");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    // LZ4 Fast (frame format)
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("lz4_fast_frame"),
        &data,
        |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data)).unwrap();
                black_box(compressed);
            });
        },
    );

    // LZ4 Fast (block format)
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("lz4_fast_block"),
        &data,
        |b, data| {
            b.iter(|| {
                let compressed = compress_block(black_box(data)).unwrap();
                black_box(compressed);
            });
        },
    );

    // LZ4-HC default level
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("lz4_hc_default"),
        &data,
        |b, data| {
            b.iter(|| {
                let compressed = compress_hc(black_box(data)).unwrap();
                black_box(compressed);
            });
        },
    );

    group.finish();
}

/// Benchmark HC compression levels (0-12)
fn bench_hc_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("hc_compression_levels");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for level in [1, 3, 6, 9, 12] {
        let hc_level = HcLevel::new(level).expect("valid HC level");
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress_hc_level(black_box(data), hc_level).unwrap();
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark block size impact on frame compression
fn bench_block_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_block_sizes");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for block_size in [
        BlockMaxSize::Size64KB,
        BlockMaxSize::Size256KB,
        BlockMaxSize::Size1MB,
        BlockMaxSize::Size4MB,
    ] {
        let desc = FrameDescriptor {
            block_max_size: block_size,
            ..Default::default()
        };

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{:?}", block_size)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress_with_options(black_box(data), desc).unwrap();
                    black_box(compressed);
                });
            },
        );
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

/// Benchmark decompression speed (frame format)
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
                    let decompressed = decompress(black_box(compressed), size * 2).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark decompression speed for block format
fn bench_block_decompression(c: &mut Criterion) {
    let mut group = c.benchmark_group("block_decompression");

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
        let compressed = compress_block(&original).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = decompress_block(black_box(compressed), size * 2).unwrap();
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
                    let decompressed = decompress(black_box(compressed), size * 2).unwrap();
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

        // Test Fast vs HC
        for mode in ["fast", "hc"] {
            let id = format!("{}/{}", pattern_name, mode);

            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = if mode == "fast" {
                        compress(black_box(data)).unwrap()
                    } else {
                        compress_hc(black_box(data)).unwrap()
                    };
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
                    let decompressed = decompress(&compressed, data.len() * 2).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark HC level vs compression ratio tradeoff
fn bench_hc_level_tradeoff(c: &mut Criterion) {
    let mut group = c.benchmark_group("hc_level_tradeoff");
    group.sample_size(10);

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for level in 1..=12 {
        let hc_level = HcLevel::new(level).expect("valid HC level");

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress_hc_level(black_box(data), hc_level).unwrap();
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

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression + decompression
                let compressed = compress(black_box(data)).unwrap();
                let decompressed = decompress(&compressed, data.len() * 2).unwrap();
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

    let sizes = [("1MB", 1024 * 1024), ("10MB", 10 * 1024 * 1024)];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));

        // Serial compression
        group.bench_with_input(BenchmarkId::new("serial", size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress(black_box(data)).unwrap();
                black_box(compressed);
            });
        });

        // Parallel compression
        group.bench_with_input(BenchmarkId::new("parallel", size_name), &data, |b, data| {
            b.iter(|| {
                let compressed = compress_parallel(black_box(data)).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

#[cfg(feature = "parallel")]
criterion_group!(
    benches,
    bench_compression_modes,
    bench_hc_levels,
    bench_block_sizes,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_block_decompression,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_hc_level_tradeoff,
    bench_memory_allocation,
    bench_parallel_vs_serial,
);

#[cfg(not(feature = "parallel"))]
criterion_group!(
    benches,
    bench_compression_modes,
    bench_hc_levels,
    bench_block_sizes,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_block_decompression,
    bench_decompression_sizes,
    bench_compression_ratio,
    bench_roundtrip,
    bench_hc_level_tradeoff,
    bench_memory_allocation,
);

criterion_main!(benches);

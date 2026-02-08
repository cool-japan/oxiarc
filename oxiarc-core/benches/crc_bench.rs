//! Comprehensive performance benchmarks for CRC implementations
//!
//! This benchmark suite evaluates:
//! - CRC-16, CRC-32, and CRC-64 performance
//! - Throughput measurements (MB/s) across different data sizes
//! - Performance of slicing-by-8 optimization for large data
//! - Comparison across different data patterns
//! - Incremental vs single-shot CRC calculation
//! - SIMD vs software implementation comparison (when simd feature enabled)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_core::crc::{Crc16, Crc32, Crc64};
use std::hint::black_box;

/// Type alias for pattern generator functions
type PatternGenerator = fn(usize) -> Vec<u8>;

/// Generate test data patterns for benchmarking
mod test_data {
    /// Uniform data - all bytes are the same
    pub fn uniform(size: usize) -> Vec<u8> {
        vec![0xAA; size]
    }

    /// Random data - varied byte values
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

    /// Zero data - all zeros
    pub fn zeros(size: usize) -> Vec<u8> {
        vec![0; size]
    }

    /// Sequential data - counting bytes
    pub fn sequential(size: usize) -> Vec<u8> {
        (0..size).map(|i| i as u8).collect()
    }

    /// Text-like data
    pub fn text_like(size: usize) -> Vec<u8> {
        let text = b"The quick brown fox jumps over the lazy dog. ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(text.len());
            data.extend_from_slice(&text[..chunk_size]);
        }
        data
    }
}

/// Standard data sizes for benchmarking
mod data_sizes {
    pub const TINY: usize = 16; // 16 B (threshold for slicing-by-8)
    pub const SMALL: usize = 256; // 256 B
    pub const MEDIUM: usize = 4 * 1024; // 4 KB
    pub const LARGE: usize = 64 * 1024; // 64 KB
    pub const XLARGE: usize = 1024 * 1024; // 1 MB
}

/// Benchmark CRC-16 across different data sizes
fn bench_crc16_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc16_sizes");

    let sizes = [
        ("16B", data_sizes::TINY),
        ("256B", data_sizes::SMALL),
        ("4KB", data_sizes::MEDIUM),
        ("64KB", data_sizes::LARGE),
        ("1MB", data_sizes::XLARGE),
    ];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc16::compute(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

/// Benchmark CRC-32 across different data sizes
fn bench_crc32_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32_sizes");

    let sizes = [
        ("16B", data_sizes::TINY),
        ("256B", data_sizes::SMALL),
        ("4KB", data_sizes::MEDIUM),
        ("64KB", data_sizes::LARGE),
        ("1MB", data_sizes::XLARGE),
    ];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

/// Benchmark CRC-64 across different data sizes
fn bench_crc64_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc64_sizes");

    let sizes = [
        ("16B", data_sizes::TINY),
        ("256B", data_sizes::SMALL),
        ("4KB", data_sizes::MEDIUM),
        ("64KB", data_sizes::LARGE),
        ("1MB", data_sizes::XLARGE),
    ];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc64::compute(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

/// Benchmark CRC-32 with different data patterns
fn bench_crc32_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32_patterns");

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("zeros", test_data::zeros as PatternGenerator),
        ("sequential", test_data::sequential as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
    ];

    let size = data_sizes::LARGE;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let checksum = Crc32::compute(black_box(data));
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark CRC-64 with different data patterns
fn bench_crc64_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc64_patterns");

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("zeros", test_data::zeros as PatternGenerator),
        ("sequential", test_data::sequential as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
    ];

    let size = data_sizes::LARGE;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let checksum = Crc64::compute(black_box(data));
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark incremental CRC-32 calculation
fn bench_crc32_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc32_incremental");

    let size = data_sizes::LARGE;
    let data = test_data::text_like(size);

    // Benchmark single-shot
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("single_shot"),
        &data,
        |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute(black_box(data));
                black_box(checksum);
            });
        },
    );

    // Benchmark incremental with various chunk sizes
    for chunk_size in [256, 1024, 4096, 16384] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("chunks_{}", chunk_size)),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut crc = Crc32::new();
                    for chunk in data.chunks(chunk_size) {
                        crc.update(black_box(chunk));
                    }
                    let checksum = crc.finalize();
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark incremental CRC-64 calculation
fn bench_crc64_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc64_incremental");

    let size = data_sizes::LARGE;
    let data = test_data::text_like(size);

    // Benchmark single-shot
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(
        BenchmarkId::from_parameter("single_shot"),
        &data,
        |b, data| {
            b.iter(|| {
                let checksum = Crc64::compute(black_box(data));
                black_box(checksum);
            });
        },
    );

    // Benchmark incremental with various chunk sizes
    for chunk_size in [256, 1024, 4096, 16384] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("chunks_{}", chunk_size)),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut crc = Crc64::new();
                    for chunk in data.chunks(chunk_size) {
                        crc.update(black_box(chunk));
                    }
                    let checksum = crc.finalize();
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark comparison of CRC-16, CRC-32, and CRC-64
fn bench_crc_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("crc_comparison");

    let size = data_sizes::LARGE;
    let data = test_data::text_like(size);

    // CRC-16
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(BenchmarkId::from_parameter("crc16"), &data, |b, data| {
        b.iter(|| {
            let checksum = Crc16::compute(black_box(data));
            black_box(checksum);
        });
    });

    // CRC-32
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(BenchmarkId::from_parameter("crc32"), &data, |b, data| {
        b.iter(|| {
            let checksum = Crc32::compute(black_box(data));
            black_box(checksum);
        });
    });

    // CRC-64
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(BenchmarkId::from_parameter("crc64"), &data, |b, data| {
        b.iter(|| {
            let checksum = Crc64::compute(black_box(data));
            black_box(checksum);
        });
    });

    group.finish();
}

/// Benchmark slicing-by-8 optimization threshold
fn bench_slicing_threshold(c: &mut Criterion) {
    let mut group = c.benchmark_group("slicing_threshold");

    // Test sizes around the 16-byte threshold
    for size in [8, 12, 16, 20, 32, 64, 128] {
        let data = test_data::text_like(size);

        // CRC-32
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("crc32", size), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute(black_box(data));
                black_box(checksum);
            });
        });

        // CRC-64
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("crc64", size), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc64::compute(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

/// Benchmark throughput scaling with data size
fn bench_throughput_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput_scaling");

    // Test exponentially increasing sizes for CRC-32
    for size in [
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ] {
        let data = test_data::text_like(size);

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}B", size)),
            &data,
            |b, data| {
                b.iter(|| {
                    let checksum = Crc32::compute(black_box(data));
                    black_box(checksum);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark SIMD vs Software CRC-32 implementation
///
/// This benchmark compares the default (possibly SIMD-accelerated) implementation
/// against the software-only implementation across various data sizes.
fn bench_simd_vs_software(c: &mut Criterion) {
    let mut group = c.benchmark_group("simd_vs_software");

    // Print which implementation is being used
    println!("CRC-32 implementation: {}", Crc32::implementation_name());
    println!("SIMD available: {}", Crc32::is_simd_available());

    let sizes = [
        ("64B", 64),
        ("256B", 256),
        ("1KB", 1024),
        ("4KB", 4 * 1024),
        ("16KB", 16 * 1024),
        ("64KB", 64 * 1024),
        ("256KB", 256 * 1024),
        ("1MB", 1024 * 1024),
    ];

    for (size_name, size) in sizes {
        let data = test_data::random(size);

        // Benchmark default implementation (SIMD if available)
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("default", size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute(black_box(data));
                black_box(checksum);
            });
        });

        // Benchmark software-only implementation
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("software", size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute_software(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

/// Benchmark SIMD threshold behavior
///
/// Tests performance around the SIMD threshold (64 bytes) to understand
/// the crossover point where SIMD becomes beneficial.
fn bench_simd_threshold(c: &mut Criterion) {
    let mut group = c.benchmark_group("simd_threshold");

    // Test sizes around and beyond the 64-byte SIMD threshold
    for size in [32, 48, 64, 80, 96, 128, 192, 256, 512] {
        let data = test_data::random(size);

        // Default (may use SIMD)
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("default", size), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute(black_box(data));
                black_box(checksum);
            });
        });

        // Software only
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("software", size), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute_software(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

/// Benchmark large data throughput (for measuring peak SIMD performance)
fn bench_large_data_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_data_throughput");

    // Large data sizes to measure peak throughput
    let sizes = [
        ("1MB", 1024 * 1024),
        ("4MB", 4 * 1024 * 1024),
        ("16MB", 16 * 1024 * 1024),
    ];

    for (size_name, size) in sizes {
        let data = test_data::random(size);

        // Default implementation
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("default", size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute(black_box(data));
                black_box(checksum);
            });
        });

        // Software implementation
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("software", size_name), &data, |b, data| {
            b.iter(|| {
                let checksum = Crc32::compute_software(black_box(data));
                black_box(checksum);
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_crc16_sizes,
    bench_crc32_sizes,
    bench_crc64_sizes,
    bench_crc32_patterns,
    bench_crc64_patterns,
    bench_crc32_incremental,
    bench_crc64_incremental,
    bench_crc_comparison,
    bench_slicing_threshold,
    bench_throughput_scaling,
    bench_simd_vs_software,
    bench_simd_threshold,
    bench_large_data_throughput,
);
criterion_main!(benches);

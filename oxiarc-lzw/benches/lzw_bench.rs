//! Comprehensive performance benchmarks for oxiarc-lzw
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed (throughput)
//! - Compression ratios for various data patterns
//! - Comparison with weezl implementation
//! - Performance across different data sizes (tile sizes)

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_lzw::{compress_tiff, decompress_tiff};
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

    /// Repetitive pattern - common in image tiles
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
                     How vexingly quick daft zebras jump! ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(text.len());
            data.extend_from_slice(&text[..chunk_size]);
        }
        data
    }

    /// Natural image-like data - simulates grayscale tile with gradients
    pub fn image_like(size: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(size);
        let side = (size as f64).sqrt() as usize;

        for y in 0..side {
            for x in 0..side {
                // Create a gradient pattern
                let value = ((x * 255 / side) + (y * 255 / side)) / 2;
                data.push(value.min(255) as u8);
            }
        }

        // Fill remaining if not perfect square
        while data.len() < size {
            data.push(128);
        }

        data
    }
}

/// Standard tile sizes for GeoTIFF benchmarking
mod tile_sizes {
    /// Small tile: 256x256 pixels = 64KB
    pub const SMALL: usize = 256 * 256;

    /// Medium tile: 512x512 pixels = 256KB
    pub const MEDIUM: usize = 512 * 512;

    /// Large tile: 1024x1024 pixels = 1MB
    pub const LARGE: usize = 1024 * 1024;
}

/// Benchmark compression speed for different data sizes and patterns
fn bench_compression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_speed");

    let sizes = [
        ("small_64KB", tile_sizes::SMALL),
        ("medium_256KB", tile_sizes::MEDIUM),
        ("large_1MB", tile_sizes::LARGE),
    ];

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("image", test_data::image_like as PatternGenerator),
    ];

    for (size_name, size) in sizes {
        for (pattern_name, generator) in patterns {
            let data = generator(size);
            let id = format!("{}/{}", size_name, pattern_name);

            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = compress_tiff(black_box(data)).unwrap();
                    black_box(compressed);
                });
            });
        }
    }

    group.finish();
}

/// Benchmark decompression speed
fn bench_decompression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_speed");

    let sizes = [
        ("small_64KB", tile_sizes::SMALL),
        ("medium_256KB", tile_sizes::MEDIUM),
        ("large_1MB", tile_sizes::LARGE),
    ];

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("image", test_data::image_like as PatternGenerator),
    ];

    for (size_name, size) in sizes {
        for (pattern_name, generator) in patterns {
            let original = generator(size);
            let compressed = compress_tiff(&original).unwrap();
            let id = format!("{}/{}", size_name, pattern_name);

            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(
                BenchmarkId::from_parameter(&id),
                &(compressed, size),
                |b, (compressed, size)| {
                    b.iter(|| {
                        let decompressed = decompress_tiff(black_box(compressed), *size).unwrap();
                        black_box(decompressed);
                    });
                },
            );
        }
    }

    group.finish();
}

/// Benchmark compression ratios
fn bench_compression_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_ratio");
    group.sample_size(10); // Fewer samples for ratio measurements

    let sizes = [
        ("small_64KB", tile_sizes::SMALL),
        ("medium_256KB", tile_sizes::MEDIUM),
        ("large_1MB", tile_sizes::LARGE),
    ];

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("image", test_data::image_like as PatternGenerator),
    ];

    for (size_name, size) in sizes {
        for (pattern_name, generator) in patterns {
            let data = generator(size);
            let id = format!("{}/{}", size_name, pattern_name);

            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = compress_tiff(black_box(data)).unwrap();
                    let ratio = data.len() as f64 / compressed.len() as f64;
                    black_box(ratio);
                });
            });
        }
    }

    group.finish();
}

/// Benchmark roundtrip (compress + decompress)
fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("roundtrip");

    let sizes = [
        ("small_64KB", tile_sizes::SMALL),
        ("medium_256KB", tile_sizes::MEDIUM),
        ("large_1MB", tile_sizes::LARGE),
    ];

    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("image", test_data::image_like as PatternGenerator),
    ];

    for (size_name, size) in sizes {
        for (pattern_name, generator) in patterns {
            let data = generator(size);
            let id = format!("{}/{}", size_name, pattern_name);

            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = compress_tiff(black_box(data)).unwrap();
                    let decompressed = decompress_tiff(&compressed, data.len()).unwrap();
                    black_box(decompressed);
                });
            });
        }
    }

    group.finish();
}

/// Compare with weezl implementation
fn bench_compare_weezl(c: &mut Criterion) {
    let mut group = c.benchmark_group("compare_weezl");

    // Use a moderate size for fair comparison
    let size = tile_sizes::MEDIUM;
    let patterns: [(&str, PatternGenerator); 5] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("image", test_data::image_like as PatternGenerator),
    ];

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        // Benchmark oxiarc-lzw compression
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("oxiarc_compress", pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = compress_tiff(black_box(data)).unwrap();
                    black_box(compressed);
                });
            },
        );

        // Benchmark weezl compression
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("weezl_compress", pattern_name),
            &data,
            |b, data| {
                b.iter(|| {
                    use weezl::BitOrder;
                    use weezl::encode::Encoder as WeezlEncoder;

                    let mut encoder = WeezlEncoder::new(BitOrder::Msb, 8);
                    let result = encoder.encode(black_box(data)).ok();
                    black_box(result);
                });
            },
        );

        // Benchmark oxiarc-lzw decompression
        let compressed_oxiarc = compress_tiff(&data).unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("oxiarc_decompress", pattern_name),
            &(compressed_oxiarc, size),
            |b, (compressed, size)| {
                b.iter(|| {
                    let decompressed = decompress_tiff(black_box(compressed), *size).unwrap();
                    black_box(decompressed);
                });
            },
        );

        // Benchmark weezl decompression
        use weezl::BitOrder;
        use weezl::encode::Encoder as WeezlEncoder;
        let mut encoder = WeezlEncoder::new(BitOrder::Msb, 8);
        let compressed_weezl = encoder.encode(&data).ok().unwrap_or_default();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("weezl_decompress", pattern_name),
            &compressed_weezl,
            |b, compressed| {
                b.iter(|| {
                    use weezl::decode::Decoder as WeezlDecoder;

                    let mut decoder = WeezlDecoder::new(BitOrder::Msb, 8);
                    let result = decoder.decode(black_box(compressed)).ok();
                    black_box(result);
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
        ("small_64KB", tile_sizes::SMALL),
        ("medium_256KB", tile_sizes::MEDIUM),
        ("large_1MB", tile_sizes::LARGE),
    ];

    for (size_name, size) in sizes {
        let data = test_data::image_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression
                let compressed = compress_tiff(black_box(data)).unwrap();
                // This tests allocation + decompression
                let decompressed = decompress_tiff(&compressed, data.len()).unwrap();
                black_box((compressed, decompressed));
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_speed,
    bench_decompression_speed,
    bench_compression_ratio,
    bench_roundtrip,
    bench_compare_weezl,
    bench_memory_allocation,
);
criterion_main!(benches);

//! Comprehensive performance benchmarks for oxiarc-deflate
//!
//! This benchmark suite evaluates:
//! - Compression/decompression speed at different levels (0-9)
//! - LZ77 tokenization performance
//! - Huffman encoding performance
//! - DEFLATE vs ZLIB format performance
//! - Performance across various data patterns
//! - Throughput measurements (MB/s)
//! - Compression ratios for different scenarios
//! - The resumable push decoder (`InflateStream` / `WrappedInflate`) at
//!   several feed granularities, next to the one-shot `inflate()` it shares
//!   a core with

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{
    GzipStreamDecoder, InflateReader, InflateStatus, InflateStream, InflateWrapper, WrappedInflate,
    ZlibStreamDecoder, deflate, gzip_compress, inflate, inflate_into, lz77::Lz77Encoder,
    zlib_compress, zlib_decompress,
};
use std::hint::black_box;
use std::io::Read;

/// Type alias for pattern generator functions
type PatternGenerator = fn(usize) -> Vec<u8>;

/// Generate test data patterns for benchmarking
mod test_data {
    /// Uniform data - all bytes are the same
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

    /// Repetitive pattern - good for LZ77
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

    /// Highly compressible data
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

    /// HTML-like data - realistic web content
    pub fn html_like(size: usize) -> Vec<u8> {
        let html = b"<html><head><title>Example</title></head><body><p>The quick brown fox jumps over the lazy dog.</p></body></html>";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk_size = remaining.min(html.len());
            data.extend_from_slice(&html[..chunk_size]);
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

/// Benchmark compression levels (0-9)
fn bench_compression_levels(c: &mut Criterion) {
    let mut group = c.benchmark_group("compression_levels");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for level in [0, 1, 3, 6, 9] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = deflate(black_box(data), level).unwrap();
                    black_box(compressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark LZ77 encoding at different levels
fn bench_lz77_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("lz77_encoding");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for level in [1, 5, 9] {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut encoder = Lz77Encoder::with_level(level);
                    let tokens = encoder.compress(black_box(data));
                    black_box(tokens);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark DEFLATE vs ZLIB format
fn bench_deflate_vs_zlib(c: &mut Criterion) {
    let mut group = c.benchmark_group("deflate_vs_zlib");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    // DEFLATE format
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(BenchmarkId::from_parameter("deflate"), &data, |b, data| {
        b.iter(|| {
            let compressed = deflate(black_box(data), 6).unwrap();
            black_box(compressed);
        });
    });

    // ZLIB format (with Adler32 checksum)
    group.throughput(Throughput::Bytes(size as u64));
    group.bench_with_input(BenchmarkId::from_parameter("zlib"), &data, |b, data| {
        b.iter(|| {
            let compressed = zlib_compress(black_box(data), 6).unwrap();
            black_box(compressed);
        });
    });

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
        ("html", test_data::html_like as PatternGenerator),
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
                    let compressed = deflate(black_box(data), 6).unwrap();
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
                let compressed = deflate(black_box(data), 6).unwrap();
                black_box(compressed);
            });
        });
    }

    group.finish();
}

/// Benchmark decompression speed (inflate)
fn bench_decompression_speed(c: &mut Criterion) {
    let mut group = c.benchmark_group("decompression_speed");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("html", test_data::html_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = deflate(&original, 6).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = inflate(black_box(compressed)).unwrap();
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
        let compressed = deflate(&original, 6).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = inflate(black_box(compressed)).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark the resumable push decoder at several feed granularities.
///
/// `whole_input` is the gate that matters: it drives the same `InflateStream`
/// core the one-shot `inflate()` uses, over the same corpus, so the two
/// numbers are directly comparable and a regression in the push path shows
/// up as a gap against `decompression_speed/*`.
fn bench_inflate_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("inflate_stream");

    let size = data_sizes::MEDIUM;
    let original = test_data::text_like(size);
    let compressed = deflate(&original, 6).unwrap();

    group.throughput(Throughput::Bytes(size as u64));

    // Whole input, growable output: the direct counterpart of `inflate()`.
    group.bench_with_input(
        BenchmarkId::from_parameter("whole_input"),
        &compressed,
        |b, compressed| {
            b.iter(|| {
                let mut stream = InflateStream::new();
                let out = stream.inflate_to_vec(black_box(compressed)).unwrap();
                black_box(out);
            });
        },
    );

    // Bounded output at three feed granularities, which is how the HTTP,
    // PNG and TIFF layers actually drive the decoder.
    for (label, chunk, buffer) in [
        ("chunk_65536", 65_536usize, 65_536usize),
        ("chunk_4096", 4_096, 65_536),
        ("chunk_1", 1, 65_536),
    ] {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let mut stream = InflateStream::new();
                    let mut scratch = vec![0u8; buffer];
                    let mut out = Vec::with_capacity(size);
                    let mut fed = 0usize;
                    loop {
                        let end = (fed + chunk).min(compressed.len());
                        let flush = if end >= compressed.len() {
                            FlushMode::Finish
                        } else {
                            FlushMode::None
                        };
                        let p = stream
                            .inflate(&compressed[fed..end], &mut scratch, flush)
                            .unwrap();
                        fed += p.consumed;
                        out.extend_from_slice(&scratch[..p.produced]);
                        if p.status == InflateStatus::StreamEnd {
                            break;
                        }
                    }
                    black_box(out);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark gzip/zlib framing through the push decoder, so the wrapper's
/// per-member bookkeeping is measured separately from the DEFLATE core.
fn bench_wrapped_inflate(c: &mut Criterion) {
    let mut group = c.benchmark_group("wrapped_inflate");

    let size = data_sizes::MEDIUM;
    let original = test_data::text_like(size);
    group.throughput(Throughput::Bytes(size as u64));

    for (label, framing, bytes) in [
        (
            "gzip",
            InflateWrapper::Gzip,
            gzip_compress(&original, 6).unwrap(),
        ),
        (
            "zlib",
            InflateWrapper::Zlib,
            zlib_compress(&original, 6).unwrap(),
        ),
        (
            "auto_zlib",
            InflateWrapper::Auto,
            zlib_compress(&original, 6).unwrap(),
        ),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(label), &bytes, |b, bytes| {
            b.iter(|| {
                let mut decoder = WrappedInflate::new(framing);
                let mut scratch = vec![0u8; 65_536];
                let mut out = Vec::with_capacity(size);
                let mut fed = 0usize;
                loop {
                    let p = decoder
                        .inflate(&bytes[fed..], &mut scratch, FlushMode::Finish)
                        .unwrap();
                    fed += p.consumed;
                    out.extend_from_slice(&scratch[..p.produced]);
                    if p.status == InflateStatus::StreamEnd {
                        break;
                    }
                }
                black_box(out);
            });
        });
    }

    group.finish();
}

/// Benchmark `inflate_into`, the fixed-destination decode path, which had no
/// coverage before.
fn bench_inflate_into(c: &mut Criterion) {
    let mut group = c.benchmark_group("inflate_into");

    let patterns: [(&str, PatternGenerator); 3] = [
        ("text", test_data::text_like as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
    ];
    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = deflate(&original, 6).unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                let mut dst = vec![0u8; size];
                b.iter(|| {
                    let written = inflate_into(black_box(compressed), &mut dst).unwrap();
                    black_box(written);
                });
            },
        );
    }

    group.finish();
}

/// Streaming decode through the `Read` adapters.
///
/// The `read_*` parameters are the size of the *caller's* buffer, which is
/// what the mandatory 64 KiB staging buffer exists to decouple from the
/// decode cost: `read_3` used to mean one <=32 KiB window update per three
/// bytes, and must now cost the same per byte as `read_65536`.
fn bench_gzip_stream_decoder(c: &mut Criterion) {
    let mut group = c.benchmark_group("gzip_stream_decoder");

    for (label, size) in [
        ("64KiB", 64 * 1024usize),
        ("1MiB", 1024 * 1024),
        ("16MiB", 16 * 1024 * 1024),
    ] {
        let original = test_data::text_like(size);
        let compressed = gzip_compress(&original, 6).unwrap();
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &compressed,
            |b, compressed| {
                let mut sink = vec![0u8; size];
                b.iter(|| {
                    let mut decoder = GzipStreamDecoder::new(black_box(&compressed[..]));
                    let mut written = 0usize;
                    loop {
                        let n = decoder.read(&mut sink[written..]).unwrap();
                        if n == 0 {
                            break;
                        }
                        written += n;
                    }
                    black_box(written);
                });
            },
        );
    }

    // Caller-buffer granularity at a fixed payload size.
    let original = test_data::text_like(data_sizes::MEDIUM);
    let compressed = gzip_compress(&original, 6).unwrap();
    for read_size in [3usize, 4096, 65_536] {
        group.throughput(Throughput::Bytes(data_sizes::MEDIUM as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("read_{read_size}")),
            &compressed,
            |b, compressed| {
                let mut buf = vec![0u8; read_size];
                b.iter(|| {
                    let mut decoder = GzipStreamDecoder::new(black_box(&compressed[..]));
                    let mut total = 0usize;
                    loop {
                        let n = decoder.read(&mut buf).unwrap();
                        if n == 0 {
                            break;
                        }
                        total += n;
                    }
                    black_box(total);
                });
            },
        );
    }

    group.finish();
}

/// The generic `Read` adapter across framings, plus the zlib decoder, at a
/// 64 KiB caller buffer.
fn bench_inflate_reader(c: &mut Criterion) {
    let mut group = c.benchmark_group("inflate_reader");
    let size = data_sizes::MEDIUM;
    let original = test_data::text_like(size);
    group.throughput(Throughput::Bytes(size as u64));

    let framings: [(&str, InflateWrapper, Vec<u8>); 4] = [
        (
            "gzip",
            InflateWrapper::Gzip,
            gzip_compress(&original, 6).unwrap(),
        ),
        (
            "zlib",
            InflateWrapper::Zlib,
            zlib_compress(&original, 6).unwrap(),
        ),
        ("raw", InflateWrapper::Raw, deflate(&original, 6).unwrap()),
        (
            "auto",
            InflateWrapper::Auto,
            gzip_compress(&original, 6).unwrap(),
        ),
    ];

    for (label, wrapper, compressed) in framings {
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &compressed,
            |b, compressed| {
                let mut buf = vec![0u8; 65_536];
                b.iter(|| {
                    let mut reader = InflateReader::new(black_box(&compressed[..]), wrapper);
                    let mut total = 0usize;
                    loop {
                        let n = reader.read(&mut buf).unwrap();
                        if n == 0 {
                            break;
                        }
                        total += n;
                    }
                    black_box(total);
                });
            },
        );
    }

    let compressed = zlib_compress(&original, 6).unwrap();
    group.bench_with_input(
        BenchmarkId::from_parameter("zlib_stream_decoder"),
        &compressed,
        |b, compressed| {
            let mut buf = vec![0u8; 65_536];
            b.iter(|| {
                let mut decoder = ZlibStreamDecoder::new(black_box(&compressed[..]));
                let mut total = 0usize;
                loop {
                    let n = decoder.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                black_box(total);
            });
        },
    );

    group.finish();
}

/// Benchmark ZLIB decompression
fn bench_zlib_decompression(c: &mut Criterion) {
    let mut group = c.benchmark_group("zlib_decompression");

    let patterns: [(&str, PatternGenerator); 7] = [
        ("uniform", test_data::uniform as PatternGenerator),
        ("random", test_data::random as PatternGenerator),
        ("repetitive", test_data::repetitive as PatternGenerator),
        ("text", test_data::text_like as PatternGenerator),
        ("binary", test_data::binary_like as PatternGenerator),
        ("compressible", test_data::compressible as PatternGenerator),
        ("html", test_data::html_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let original = generator(size);
        let compressed = zlib_compress(&original, 6).unwrap();

        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(pattern_name),
            &compressed,
            |b, compressed| {
                b.iter(|| {
                    let decompressed = zlib_decompress(black_box(compressed)).unwrap();
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
        ("html", test_data::html_like as PatternGenerator),
    ];

    let size = data_sizes::MEDIUM;

    for (pattern_name, generator) in patterns {
        let data = generator(size);

        // Test multiple compression levels
        for level in [1, 6, 9] {
            let id = format!("{}/level_{}", pattern_name, level);

            group.bench_with_input(BenchmarkId::from_parameter(&id), &data, |b, data| {
                b.iter(|| {
                    let compressed = deflate(black_box(data), level).unwrap();
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
        ("html", test_data::html_like as PatternGenerator),
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
                    let compressed = deflate(black_box(data), 6).unwrap();
                    let decompressed = inflate(&compressed).unwrap();
                    black_box(decompressed);
                });
            },
        );
    }

    group.finish();
}

/// Benchmark level vs speed tradeoff
fn bench_level_speed_tradeoff(c: &mut Criterion) {
    let mut group = c.benchmark_group("level_speed_tradeoff");

    let size = data_sizes::MEDIUM;
    let data = test_data::text_like(size);

    for level in 0..=9 {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("level_{}", level)),
            &data,
            |b, data| {
                b.iter(|| {
                    let compressed = deflate(black_box(data), level).unwrap();
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
        ("100KB", data_sizes::MEDIUM),
        ("1MB", data_sizes::LARGE),
    ];

    for (size_name, size) in sizes {
        let data = test_data::text_like(size);

        group.bench_with_input(BenchmarkId::from_parameter(size_name), &data, |b, data| {
            b.iter(|| {
                // This tests allocation + compression + decompression
                let compressed = deflate(black_box(data), 6).unwrap();
                let decompressed = inflate(&compressed).unwrap();
                black_box((compressed, decompressed));
            });
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_compression_levels,
    bench_lz77_encoding,
    bench_deflate_vs_zlib,
    bench_compression_data_types,
    bench_compression_sizes,
    bench_decompression_speed,
    bench_decompression_sizes,
    bench_inflate_stream,
    bench_wrapped_inflate,
    bench_inflate_into,
    bench_gzip_stream_decoder,
    bench_inflate_reader,
    bench_zlib_decompression,
    bench_compression_ratio,
    bench_roundtrip,
    bench_level_speed_tradeoff,
    bench_memory_allocation,
);
criterion_main!(benches);

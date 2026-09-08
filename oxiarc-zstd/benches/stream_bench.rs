//! Throughput benchmarks for the bounded [`ZstdStream`] push decoder.
//!
//! The gate the streaming-truth audit sets (§11) is: incremental decode must
//! reach at least 85 % of the one-shot decoder's throughput on 1 MiB+ payloads.
//! `oneshot_baseline` and `incremental_chunks/65536` are the two numbers to
//! compare; the 4 KiB and 1 KiB rows and the `starved_128kib` group show how the
//! cost degrades as the caller starves the decoder.
//!
//! **Read the ratio, not the absolute numbers.** Criterion measures wall time,
//! so on a busy machine every row moves together by a factor of several. The
//! figures published in `README.md` were taken as an interleaved A/B (one-shot
//! and incremental alternating, best of 40 rounds each), which is stable under
//! load; run this bench on an idle machine to reproduce them directly.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p oxiarc-zstd --bench stream_bench
//! ```

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{
    ZstdStatus, ZstdStream, compress_with_level, decompress, decompress_into, decompress_with_limit,
};
use std::hint::black_box;

/// Deterministic pseudo-random bytes.
fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

/// A structured 1 MiB corpus: compressible, multi-block, realistic.
fn structured(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut i = 0u32;
    while data.len() < size {
        data.extend_from_slice(
            format!(
                "record {i:08} name=widget-{} qty={} price={}.99\n",
                i % 97,
                i % 13,
                i % 997
            )
            .as_bytes(),
        );
        i += 1;
    }
    data.truncate(size);
    data
}

/// Drive a warm stream over `frame` in `chunk`-byte pieces.
fn drive(stream: &mut ZstdStream, frame: &[u8], chunk: usize, scratch: &mut [u8]) -> usize {
    stream.reset();
    let mut pos = 0usize;
    let mut produced = 0usize;
    loop {
        let end = pos.saturating_add(chunk).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let p = stream
            .decode(&frame[pos..end], scratch, flush)
            .expect("decode must succeed");
        pos += p.consumed;
        produced += p.produced;
        if p.status == ZstdStatus::StreamEnd {
            return produced;
        }
    }
}

/// One-shot vs incremental over the same 1 MiB frame.
fn bench_decode_throughput(c: &mut Criterion) {
    let corpora: [(&str, Vec<u8>); 2] = [
        ("structured_1mib", structured(1 << 20)),
        ("random_1mib", pseudo_random(1 << 20, 0xBEEF)),
    ];

    for (name, data) in corpora {
        let frame = compress_with_level(&data, 3).expect("compress");
        let mut group = c.benchmark_group(format!("zstd_decode/{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        group.bench_function("oneshot_baseline", |b| {
            b.iter(|| black_box(decompress(black_box(&frame)).expect("decompress")));
        });

        group.bench_function("oneshot_into", |b| {
            let mut dst = vec![0u8; data.len()];
            b.iter(|| {
                black_box(decompress_into(black_box(&frame), &mut dst).expect("decompress_into"))
            });
        });

        group.bench_function("oneshot_with_limit", |b| {
            b.iter(|| {
                black_box(decompress_with_limit(black_box(&frame), data.len()).expect("with_limit"))
            });
        });

        // Incremental rows: one warm stream reused, so the numbers measure
        // decoding rather than allocation.
        for chunk in [64 * 1024usize, 4096, 1024] {
            let mut stream = ZstdStream::new().with_max_window(usize::MAX);
            let mut scratch = vec![0u8; 64 * 1024];
            // Warm-up so the window and carry are already at their high-water
            // mark before the first timed iteration.
            drive(&mut stream, &frame, chunk, &mut scratch);
            group.bench_with_input(
                BenchmarkId::new("incremental_chunks", chunk),
                &chunk,
                |b, &chunk| {
                    b.iter(|| black_box(drive(&mut stream, &frame, chunk, &mut scratch)));
                },
            );
        }

        group.finish();
    }
}

/// RGB8 image rows: the TIFF-strip shape, whose bytes are almost all
/// Huffman-coded literals.
fn rgb8_rows(width: usize, rows: usize) -> Vec<u8> {
    let mut x = 0x1234_5678u32;
    let mut out = Vec::with_capacity(width * rows * 3);
    for y in 0..rows {
        for i in 0..width {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let noise = (x & 0x0F) as usize;
            out.push((((i * 255) / width + noise) & 0xFF) as u8);
            out.push((((y * 255) / rows.max(1) + noise / 2) & 0xFF) as u8);
            out.push((((i + y) * 128 / (width + rows) + noise / 4) & 0xFF) as u8);
        }
    }
    out
}

/// The shape matrix the decode-throughput work is measured against.
///
/// Each row is one payload at one compression level, decoded three ways. The
/// shapes stress different parts of the decoder: the TIFF-strip rows are
/// literal-bound (the interleaved Huffman loop), `repetitive` is bound by the
/// overlapping-match copy, and `incompressible` is a `Raw` block memcpy.
///
/// For a comparison against the reference decoder rather than against our own
/// previous self, use `examples/decode_throughput.rs`, which measures the same
/// shapes against `zstd -b -d` with interleaved rounds.
fn bench_shape_matrix(c: &mut Criterion) {
    let strip = rgb8_rows(4096, 24);
    let shapes: [(&str, &[u8], &[i32]); 3] = [
        ("tiff_strip_rgb8", &strip, &[1, 3, 9, 19]),
        ("incompressible", &[], &[3]),
        ("repetitive", &[], &[3]),
    ];

    for (name, fixed, levels) in shapes {
        let data: Vec<u8> = match name {
            "incompressible" => pseudo_random(1 << 20, 0x5EED),
            "repetitive" => b"oxiarc-zstd/"
                .iter()
                .copied()
                .cycle()
                .take(1 << 20)
                .collect(),
            _ => fixed.to_vec(),
        };
        let mut group = c.benchmark_group(format!("zstd_shape/{name}"));
        group.throughput(Throughput::Bytes(data.len() as u64));

        for &level in levels {
            let frame = compress_with_level(&data, level).expect("compress");
            group.bench_with_input(BenchmarkId::new("oneshot_into", level), &level, |b, _| {
                let mut dst = vec![0u8; data.len()];
                b.iter(|| {
                    black_box(
                        decompress_into(black_box(&frame), &mut dst).expect("decompress_into"),
                    )
                });
            });
            group.bench_with_input(BenchmarkId::new("legacy_oneshot", level), &level, |b, _| {
                b.iter(|| black_box(decompress(black_box(&frame)).expect("decompress")));
            });
            let mut stream = ZstdStream::new().with_max_window(usize::MAX);
            let mut scratch = vec![0u8; 64 * 1024];
            drive(&mut stream, &frame, 64 * 1024, &mut scratch);
            group.bench_with_input(BenchmarkId::new("stream_64kib", level), &level, |b, _| {
                b.iter(|| black_box(drive(&mut stream, &frame, 64 * 1024, &mut scratch)));
            });
        }

        group.finish();
    }
}

/// How far throughput falls when the caller starves the decoder on both sides.
fn bench_starved_schedules(c: &mut Criterion) {
    let data = structured(128 * 1024);
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut group = c.benchmark_group("zstd_decode/starved_128kib");
    group.throughput(Throughput::Bytes(data.len() as u64));

    for (label, in_chunk, out_chunk) in [
        ("in64k_out64k", 64 * 1024usize, 64 * 1024usize),
        ("in1k_out1k", 1024, 1024),
        ("in1_out64k", 1, 64 * 1024),
        ("in64k_out1", 64 * 1024, 1),
    ] {
        let mut stream = ZstdStream::new().with_max_window(usize::MAX);
        let mut scratch = vec![0u8; out_chunk];
        drive(&mut stream, &frame, in_chunk, &mut scratch);
        group.bench_function(label, |b| {
            b.iter(|| black_box(drive(&mut stream, &frame, in_chunk, &mut scratch)));
        });
    }
    group.finish();
}

/// `ZstdStreamDecoder`'s `Read` shell versus the raw push decoder.
fn bench_read_adapter(c: &mut Criterion) {
    use std::io::Read;

    let data = structured(1 << 20);
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut group = c.benchmark_group("zstd_decode/read_adapter");
    group.throughput(Throughput::Bytes(data.len() as u64));

    group.bench_function("read_to_end", |b| {
        b.iter(|| {
            let mut decoder = oxiarc_zstd::ZstdStreamDecoder::new(black_box(&frame[..]));
            let mut out = Vec::with_capacity(1 << 20);
            decoder.read_to_end(&mut out).expect("read_to_end");
            black_box(out.len())
        });
    });

    group.bench_function("read_4kib_buffer", |b| {
        b.iter(|| {
            let mut decoder = oxiarc_zstd::ZstdStreamDecoder::new(black_box(&frame[..]));
            let mut buf = [0u8; 4096];
            let mut total = 0usize;
            loop {
                let n = decoder.read(&mut buf).expect("read");
                if n == 0 {
                    break;
                }
                total += n;
            }
            black_box(total)
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_decode_throughput,
    bench_shape_matrix,
    bench_starved_schedules,
    bench_read_adapter
);
criterion_main!(benches);

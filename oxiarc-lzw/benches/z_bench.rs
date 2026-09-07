//! Benchmarks for the UNIX `compress(1)` / `.Z` container.
//!
//! Three axes matter for this codec:
//!
//! * **`max_bits`** — the code-width ceiling decides how large the code
//!   table grows and therefore both ratio and cache behaviour; 9 and 16 are
//!   the extremes the format allows.
//! * **entry point** — `decompress` (growable `Vec`), `decompress_into`
//!   (caller-owned slice, no output allocation) and [`ZReader`] (16 KiB
//!   chunks, bounded working set) decode the same bytes through three
//!   different sinks.
//! * **payload shape** — LZW on text is dictionary-bound, on incompressible
//!   noise it is code-emission-bound.
//!
//! Ratio is reported as a throughput-free group so `cargo bench` prints the
//! compressed sizes next to the timings.

use std::hint::black_box;
use std::io::{Read, Write};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_lzw::z::{ZReader, ZWriter, compress, decompress, decompress_into};

/// Deterministic xorshift byte stream (incompressible filler).
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// English-shaped text with a small vocabulary.
fn text(n: usize) -> Vec<u8> {
    const WORDS: [&[u8]; 8] = [
        b"the ", b"quick ", b"brown ", b"fox ", b"jumps ", b"over ", b"lazy ", b"dog ",
    ];
    let mut out = Vec::with_capacity(n + 8);
    let mut i: u32 = 0;
    while out.len() < n {
        out.extend_from_slice(WORDS[(i.wrapping_mul(2_654_435_761) >> 20) as usize % WORDS.len()]);
        i = i.wrapping_add(1);
    }
    out.truncate(n);
    out
}

/// Structured records: highly repetitive, the shape `.Z` was designed for.
fn records(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 64);
    let mut i: u32 = 0;
    while out.len() < n {
        out.extend_from_slice(format!("id={i:08} name=row{:04} flag=0\n", i % 997).as_bytes());
        i = i.wrapping_add(1);
    }
    out.truncate(n);
    out
}

const SIZE: usize = 1 << 20;

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("text", text(SIZE)),
        ("records", records(SIZE)),
        ("noise", noise(SIZE, 0x9E37_79B9)),
        ("zeros", vec![0u8; SIZE]),
    ]
}

fn bench_compress(c: &mut Criterion) {
    let mut group = c.benchmark_group("z_compress");
    for (name, payload) in corpus() {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        for max_bits in [12u8, 16] {
            group.bench_with_input(
                BenchmarkId::new(format!("b{max_bits}"), name),
                &payload,
                |b, payload| {
                    b.iter(|| black_box(compress(black_box(payload), max_bits)));
                },
            );
        }
    }
    group.finish();
}

fn bench_decompress(c: &mut Criterion) {
    let mut group = c.benchmark_group("z_decompress");
    for (name, payload) in corpus() {
        let stream = compress(&payload, 16).expect("compress");
        group.throughput(Throughput::Bytes(payload.len() as u64));

        group.bench_with_input(BenchmarkId::new("vec", name), &stream, |b, stream| {
            b.iter(|| black_box(decompress(black_box(stream))));
        });

        let mut out = vec![0u8; payload.len()];
        group.bench_with_input(BenchmarkId::new("into", name), &stream, |b, stream| {
            b.iter(|| black_box(decompress_into(black_box(stream), black_box(&mut out))));
        });

        group.bench_with_input(BenchmarkId::new("reader", name), &stream, |b, stream| {
            b.iter(|| {
                let mut sink = Vec::with_capacity(SIZE);
                let mut reader = ZReader::new(black_box(&stream[..]));
                let read = reader.read_to_end(&mut sink);
                black_box((read, sink))
            });
        });
    }
    group.finish();
}

fn bench_writer(c: &mut Criterion) {
    let mut group = c.benchmark_group("z_writer");
    for (name, payload) in corpus() {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("64k_writes", name),
            &payload,
            |b, payload| {
                b.iter(|| {
                    let mut writer =
                        ZWriter::new(Vec::with_capacity(SIZE / 2), 16).expect("writer");
                    for chunk in payload.chunks(64 * 1024) {
                        writer.write_all(chunk).expect("write");
                    }
                    black_box(writer.finish())
                });
            },
        );
    }
    group.finish();
}

fn bench_ratio(c: &mut Criterion) {
    // Not a timing: printed once so the ratios travel with the benchmark
    // results instead of living only in a report.
    for (name, payload) in corpus() {
        for max_bits in [9u8, 12, 16] {
            let stream = compress(&payload, max_bits).expect("compress");
            let ratio = stream.len() as f64 / payload.len() as f64;
            println!(
                "ratio {name} b{max_bits}: {} bytes ({ratio:.4})",
                stream.len()
            );
        }
    }
    let mut group = c.benchmark_group("z_ratio_probe");
    group.sample_size(10);
    let payload = records(SIZE);
    group.bench_function("records_b16", |b| {
        b.iter(|| black_box(compress(black_box(&payload), 16)));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_compress,
    bench_decompress,
    bench_writer,
    bench_ratio
);
criterion_main!(benches);

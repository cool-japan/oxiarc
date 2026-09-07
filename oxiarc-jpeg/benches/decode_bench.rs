//! Decode throughput benchmarks.
//!
//! Run with `cargo bench -p oxiarc-jpeg`. When `cjpeg`/`djpeg` are on `PATH`
//! the fixtures are generated from a synthetic photographic source at several
//! sizes and subsampling ratios; otherwise the benchmark falls back to the
//! crate's embedded one-pixel sample so the harness still runs.

use std::hint::black_box;
use std::process::Command;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_jpeg::{DecodeOptions, Decoder, Upsampling};

/// Build a synthetic PPM with edges, gradients and flat regions.
fn source_ppm(width: usize, height: usize) -> Vec<u8> {
    let mut data = format!("P6\n{width} {height}\n255\n").into_bytes();
    for y in 0..height {
        for x in 0..width {
            let r = ((x * 7 + y * 3) % 256) as u8;
            let g = if (x / 16 + y / 16) % 2 == 0 { 220 } else { 30 };
            let b = ((x * x + y * y) % 251) as u8;
            data.extend_from_slice(&[r, g, b]);
        }
    }
    data
}

/// Encode a fixture with `cjpeg`, or return `None` when it is unavailable.
fn cjpeg(args: &[&str], source: &[u8]) -> Option<Vec<u8>> {
    let dir = std::env::temp_dir();
    let stamp = format!("oxiarc_jpeg_bench_{}", std::process::id());
    let input = dir.join(format!("{stamp}.ppm"));
    let output = dir.join(format!("{stamp}.jpg"));
    std::fs::write(&input, source).ok()?;
    let status = Command::new("cjpeg")
        .args(args)
        .arg("-outfile")
        .arg(&output)
        .arg(&input)
        .status()
        .ok()?;
    let bytes = if status.success() {
        std::fs::read(&output).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    bytes
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");
    let cases: [(&str, &[&str], usize, usize); 4] = [
        (
            "baseline_420_q75_512",
            &["-quality", "75", "-sample", "2x2"],
            512,
            512,
        ),
        (
            "baseline_444_q95_512",
            &["-quality", "95", "-sample", "1x1"],
            512,
            512,
        ),
        (
            "progressive_q75_512",
            &["-quality", "75", "-progressive"],
            512,
            512,
        ),
        (
            "grayscale_q75_512",
            &["-quality", "75", "-grayscale"],
            512,
            512,
        ),
    ];

    for (name, args, width, height) in cases {
        let source = source_ppm(width, height);
        let Some(jpeg) = cjpeg(args, &source) else {
            eprintln!("skipping {name}: cjpeg unavailable");
            continue;
        };
        group.throughput(Throughput::Elements((width * height) as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut decoder = Decoder::new(jpeg.as_slice());
                black_box(decoder.decode().expect("decode"))
            });
        });
    }

    // Always-available fallback so the harness runs on a hermetic machine.
    let sample: &[u8] = &oxiarc_jpeg::sample::GRAY_1X1;
    group.bench_function("embedded_gray_1x1", |b| {
        b.iter(|| {
            let mut decoder = Decoder::new(sample);
            black_box(decoder.decode().expect("decode"))
        });
    });
    group.finish();
}

fn bench_upsampling(c: &mut Criterion) {
    let source = source_ppm(512, 512);
    let Some(jpeg) = cjpeg(&["-quality", "75", "-sample", "2x2"], &source) else {
        eprintln!("skipping upsampling benchmarks: cjpeg unavailable");
        return;
    };
    let mut group = c.benchmark_group("upsampling");
    group.throughput(Throughput::Elements(512 * 512));
    for (name, mode) in [("fancy", Upsampling::Fancy), ("box", Upsampling::Box)] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let options = DecodeOptions {
                    upsampling: mode,
                    ..DecodeOptions::default()
                };
                let mut decoder = Decoder::with_options(jpeg.as_slice(), options);
                black_box(decoder.decode().expect("decode"))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_decode, bench_upsampling);
criterion_main!(benches);

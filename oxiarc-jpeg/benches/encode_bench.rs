//! Encode throughput benchmarks.
//!
//! Run with `cargo bench -p oxiarc-jpeg --bench encode_bench`. Each group
//! measures megapixels per second on a synthetic photographic source; when
//! `cjpeg` is on `PATH` a `reference/…` group times the same work through it,
//! so the ratio can be read straight off one run on one machine.
//!
//! `cjpeg`'s figure includes reading a PPM off disk and writing a JPEG back,
//! which ours does not, so the reference is a lower bound on libjpeg's speed
//! rather than an exact one. It is still the only same-machine number that
//! matters for the wave target.

use std::hint::black_box;
use std::process::Command;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_jpeg::{
    EncodeOptions, EncodeProcess, EntropyCoding, InputColor, RestartInterval, Subsampling,
    encode_to_vec_with_options, encode_u16_to_vec_with_options,
};

/// A synthetic photographic source: gradients, edges and flat regions.
fn source(width: usize, height: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(width * height * 3);
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

fn source_ppm(width: usize, height: usize) -> Vec<u8> {
    let mut data = format!("P6\n{width} {height}\n255\n").into_bytes();
    data.extend_from_slice(&source(width, height));
    data
}

/// `true` when `cjpeg` runs.
fn cjpeg_available() -> bool {
    Command::new("cjpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success() || !out.stderr.is_empty())
        .unwrap_or(false)
}

/// Run `cjpeg` once over a file already on disk.
fn run_cjpeg(args: &[&str], input: &std::path::Path, output: &std::path::Path) {
    let _ = Command::new("cjpeg")
        .args(args)
        .arg("-outfile")
        .arg(output)
        .arg(input)
        .status();
}

fn bench_encode(c: &mut Criterion) {
    let cases: [(&str, usize, usize, EncodeOptions, Vec<&str>); 10] = [
        (
            "baseline_420_q75_512",
            512,
            512,
            EncodeOptions {
                quality: 75,
                ..Default::default()
            },
            vec!["-quality", "75", "-sample", "2x2,1x1,1x1"],
        ),
        (
            "baseline_444_q95_512",
            512,
            512,
            EncodeOptions {
                quality: 95,
                subsampling: Subsampling::S444,
                ..Default::default()
            },
            vec!["-quality", "95", "-sample", "1x1,1x1,1x1"],
        ),
        (
            "baseline_420_q75_1920x1080",
            1920,
            1080,
            EncodeOptions {
                quality: 75,
                ..Default::default()
            },
            vec!["-quality", "75", "-sample", "2x2,1x1,1x1"],
        ),
        (
            "optimized_420_q75_512",
            512,
            512,
            EncodeOptions {
                quality: 75,
                optimize_huffman: true,
                ..Default::default()
            },
            vec!["-quality", "75", "-optimize", "-sample", "2x2,1x1,1x1"],
        ),
        (
            "progressive_420_q75_512",
            512,
            512,
            EncodeOptions {
                quality: 75,
                process: EncodeProcess::Progressive,
                ..Default::default()
            },
            vec!["-quality", "75", "-progressive", "-sample", "2x2,1x1,1x1"],
        ),
        (
            "lossless_psv1_512",
            512,
            512,
            EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 1,
                    point_transform: 0,
                },
                ..Default::default()
            },
            vec!["-lossless", "1"],
        ),
        (
            "arithmetic_420_q75_512",
            512,
            512,
            EncodeOptions {
                quality: 75,
                entropy: EntropyCoding::Arithmetic,
                ..Default::default()
            },
            vec!["-quality", "75", "-arithmetic", "-sample", "2x2,1x1,1x1"],
        ),
        (
            "arithmetic_progressive_q75_512",
            512,
            512,
            EncodeOptions {
                quality: 75,
                process: EncodeProcess::Progressive,
                entropy: EntropyCoding::Arithmetic,
                ..Default::default()
            },
            vec![
                "-quality",
                "75",
                "-progressive",
                "-arithmetic",
                "-sample",
                "2x2,1x1,1x1",
            ],
        ),
        (
            "restart_420_q75_1024",
            1024,
            1024,
            EncodeOptions {
                quality: 75,
                restart_interval: RestartInterval::McuRows(1),
                ..Default::default()
            },
            vec!["-quality", "75", "-restart", "1", "-sample", "2x2,1x1,1x1"],
        ),
        // The QM coder is the slowest stage of an arithmetic encode, so this
        // is where the `rayon` feature has the most to gain.
        (
            "arithmetic_restart_420_q75_1024",
            1024,
            1024,
            EncodeOptions {
                quality: 75,
                entropy: EntropyCoding::Arithmetic,
                restart_interval: RestartInterval::McuRows(1),
                ..Default::default()
            },
            vec![
                "-quality",
                "75",
                "-arithmetic",
                "-restart",
                "1",
                "-sample",
                "2x2,1x1,1x1",
            ],
        ),
    ];

    let mut group = c.benchmark_group("encode");
    let reference = cjpeg_available();
    let dir = std::env::temp_dir();
    for (name, width, height, options, args) in cases {
        let pixels = source(width, height);
        let pixel_count = (width * height) as u64;
        group.throughput(Throughput::Elements(pixel_count));
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = encode_to_vec_with_options(
                    black_box(&pixels),
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    black_box(&options),
                )
                .expect("encode");
                black_box(out.len())
            });
        });

        if reference {
            let input = dir.join(format!("oxiarc_encbench_{}_{name}.ppm", std::process::id()));
            let output = dir.join(format!("oxiarc_encbench_{}_{name}.jpg", std::process::id()));
            if std::fs::write(&input, source_ppm(width, height)).is_ok() {
                group.bench_function(format!("reference/{name}"), |b| {
                    b.iter(|| run_cjpeg(&args, &input, &output));
                });
                let _ = std::fs::remove_file(&input);
                let _ = std::fs::remove_file(&output);
            }
        }
    }
    group.finish();

    let mut group = c.benchmark_group("encode_twelve_bit");
    let width = 512usize;
    let height = 512usize;
    let samples: Vec<u16> = (0..width * height)
        .map(|i| ((i * 37) % 4096) as u16)
        .collect();
    group.throughput(Throughput::Elements((width * height) as u64));
    group.bench_function("grayscale_q90_512", |b| {
        let options = EncodeOptions {
            quality: 90,
            precision: 12,
            ..Default::default()
        };
        b.iter(|| {
            let out = encode_u16_to_vec_with_options(
                black_box(&samples),
                width as u16,
                height as u16,
                InputColor::Luma,
                &options,
            )
            .expect("encode");
            black_box(out.len())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_encode);
criterion_main!(benches);

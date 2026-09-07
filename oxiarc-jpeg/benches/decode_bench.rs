//! Decode throughput benchmarks.
//!
//! Run with `cargo bench -p oxiarc-jpeg`. When `cjpeg`/`djpeg` are on `PATH`
//! the fixtures are generated from a synthetic photographic source at several
//! sizes and subsampling ratios; otherwise the benchmark falls back to the
//! crate's embedded one-pixel sample so the harness still runs.

use std::hint::black_box;
use std::process::Command;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_jpeg::{DecodeOptions, Decoder, Scale, Upsampling};

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

/// `true` when `djpeg` runs.
fn djpeg_available() -> bool {
    Command::new("djpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success() || !out.stderr.is_empty())
        .unwrap_or(false)
}

/// Run `djpeg` once over a file already on disk.
fn run_djpeg(args: &[&str], input: &std::path::Path, output: &std::path::Path) {
    let _ = Command::new("djpeg")
        .args(args)
        .arg("-outfile")
        .arg(output)
        .arg(input)
        .status();
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

    // Arithmetic coding, whose decode cost is dominated by the QM coder
    // rather than by the IDCT: one binary decision per magnitude bit, with a
    // data-dependent renormalisation loop.
    #[cfg(feature = "arithmetic")]
    {
        let arithmetic_cases: [(&str, &[&str], usize, usize); 3] = [
            (
                "arithmetic_420_q75_512",
                &["-quality", "75", "-sample", "2x2", "-arithmetic"],
                512,
                512,
            ),
            (
                "arithmetic_444_q95_512",
                &["-quality", "95", "-sample", "1x1", "-arithmetic"],
                512,
                512,
            ),
            (
                "arithmetic_progressive_q75_512",
                &["-quality", "75", "-progressive", "-arithmetic"],
                512,
                512,
            ),
        ];
        for (name, args, width, height) in arithmetic_cases {
            let source = source_ppm(width, height);
            let Some(jpeg) = cjpeg(args, &source) else {
                eprintln!("skipping {name}: cjpeg cannot write arithmetic streams");
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
    }

    // Restart intervals, which the `rayon` feature decodes in parallel and
    // which are otherwise a small overhead over the plain baseline case.
    {
        let source = source_ppm(1024, 1024);
        if let Some(jpeg) = cjpeg(
            &["-quality", "75", "-sample", "2x2", "-restart", "1"],
            &source,
        ) {
            group.throughput(Throughput::Elements(1024 * 1024));
            group.bench_function("baseline_420_restart_1024", |b| {
                b.iter(|| {
                    let mut decoder = Decoder::new(jpeg.as_slice());
                    black_box(decoder.decode().expect("decode"))
                });
            });
        }
    }

    // The same shape with the QM coder. Entropy coding is a far larger share
    // of an arithmetic decode than of a Huffman one, so this is the case the
    // `rayon` feature has the most to gain on.
    #[cfg(feature = "arithmetic")]
    {
        let source = source_ppm(1024, 1024);
        if let Some(jpeg) = cjpeg(
            &[
                "-quality",
                "75",
                "-sample",
                "2x2",
                "-arithmetic",
                "-restart",
                "1",
            ],
            &source,
        ) {
            group.throughput(Throughput::Elements(1024 * 1024));
            group.bench_function("arithmetic_420_restart_1024", |b| {
                b.iter(|| {
                    let mut decoder = Decoder::new(jpeg.as_slice());
                    black_box(decoder.decode().expect("decode"))
                });
            });
        }
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

/// [`DecodeOptions::scale`] at every `M` in `{1, 2, 4, 8}` — the four values
/// libjpeg itself reconstructs with a dedicated fixed-point kernel this crate
/// ports exactly (`idct/scaled.rs`'s module doc) — over one 4:2:0 and one
/// 4:4:4 source. When `djpeg` is on `PATH` a `reference/…` group times
/// `djpeg -dct int -scale M/8` over the same encoded file, so the ratio can
/// be read straight off one run, the same convention `encode_bench.rs`'s
/// `reference/…` group uses for `cjpeg`.
///
/// Throughput is reported against the **source's** pixel count (512x512, the
/// same denominator at every `M`) rather than the shrinking output size, so
/// the Melem/s column is directly comparable across scales on one row of the
/// criterion report instead of being inflated at small `M` by a smaller
/// denominator.
fn bench_scaled_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("scaled_decode");
    let reference = djpeg_available();
    let dir = std::env::temp_dir();

    let fixtures: [(&str, &[&str]); 2] = [
        ("420_q75", &["-quality", "75", "-sample", "2x2"]),
        ("444_q95", &["-quality", "95", "-sample", "1x1"]),
    ];

    for (fixture_name, args) in fixtures {
        let source = source_ppm(512, 512);
        let Some(jpeg) = cjpeg(args, &source) else {
            eprintln!("skipping scaled_decode/{fixture_name}: cjpeg unavailable");
            continue;
        };

        let input_path = dir.join(format!(
            "oxiarc_jpeg_scalebench_{}_{fixture_name}.jpg",
            std::process::id()
        ));
        let reference_ready = reference && std::fs::write(&input_path, &jpeg).is_ok();

        for numerator in [1u8, 2, 4, 8] {
            let scale = Scale::new(numerator).expect("1..=16");
            let name = format!("{fixture_name}_m{numerator}_8");
            group.throughput(Throughput::Elements(512 * 512));
            group.bench_function(&name, |b| {
                b.iter(|| {
                    let options = DecodeOptions {
                        scale,
                        ..DecodeOptions::default()
                    };
                    let mut decoder = Decoder::with_options(jpeg.as_slice(), options);
                    black_box(decoder.decode().expect("decode"))
                });
            });

            if reference_ready {
                let scale_arg = format!("{numerator}/8");
                let output_path = dir.join(format!(
                    "oxiarc_jpeg_scalebench_{}_{fixture_name}_m{numerator}.ppm",
                    std::process::id()
                ));
                group.bench_function(format!("reference/{name}"), |b| {
                    b.iter(|| {
                        run_djpeg(
                            &["-dct", "int", "-scale", &scale_arg, "-pnm"],
                            &input_path,
                            &output_path,
                        );
                    });
                });
                let _ = std::fs::remove_file(&output_path);
            }
        }
        let _ = std::fs::remove_file(&input_path);
    }
    group.finish();
}

criterion_group!(benches, bench_decode, bench_upsampling, bench_scaled_decode);
criterion_main!(benches);

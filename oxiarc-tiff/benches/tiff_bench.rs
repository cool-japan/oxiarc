//! Decode and encode benchmarks for the codecs this build ships.
//!
//! Run with `cargo bench -p oxiarc-tiff`. The groups are named so a regression
//! gate can compare them across revisions; the design target for this wave is
//! that no group regresses by more than 2 %.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_tiff::{
    ColorType, Compression, Decoder, Encoder, ImageSpec, Layout, Predictor, SampleFormat,
};
use std::hint::black_box;
use std::io::Cursor;

/// A deterministic image with both flat runs and noisy regions, so PackBits
/// exercises its repeat and literal branches in realistic proportions.
fn make_image(width: u32, height: u32, samples: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height) as usize * samples);
    for y in 0..height {
        for x in 0..width {
            for s in 0..samples {
                let flat = (x / 16 + y / 16) % 3 == 0;
                let value = if flat {
                    0xA0u8
                } else {
                    ((x.wrapping_mul(31) ^ y.wrapping_mul(17)) as usize + s * 7) as u8
                };
                data.push(value);
            }
        }
    }
    data
}

fn encode(spec: &ImageSpec, data: &[u8]) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(spec, data).expect("write");
    encoder.finish().expect("finish");
    buffer.into_inner()
}

fn bench_decode(c: &mut Criterion) {
    let (width, height) = (1024u32, 1024u32);
    let gray = make_image(width, height, 1);
    let rgb = make_image(width, height, 3);

    let cases: [(&str, ImageSpec, &[u8]); 5] = [
        (
            "gray8_strips_none",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ),
        (
            "gray8_strips_packbits",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::PackBits)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ),
        (
            "gray8_tiles_none",
            ImageSpec::new(width, height, ColorType::Gray(8)).with_layout(Layout::Tiles {
                width: 256,
                length: 256,
            }),
            &gray,
        ),
        (
            "rgb8_strips_none",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &rgb,
        ),
        (
            "rgb8_strips_packbits",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::PackBits)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &rgb,
        ),
    ];

    let mut group = c.benchmark_group("tiff_decode");
    for (name, spec, data) in cases {
        let file = encode(&spec, data);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &file, |b, file| {
            b.iter(|| {
                let mut decoder = Decoder::new(Cursor::new(file.clone())).expect("decoder");
                black_box(decoder.read_image().expect("decode"))
            });
        });
    }
    group.finish();
}

fn bench_predictor(c: &mut Criterion) {
    let (width, height) = (512u32, 512u32);
    let mut gray16 = Vec::with_capacity((width * height) as usize * 2);
    let mut float32 = Vec::with_capacity((width * height) as usize * 4);
    for i in 0..(width * height) as usize {
        gray16.extend_from_slice(&((i as u16).wrapping_mul(37)).to_ne_bytes());
        float32.extend_from_slice(&(i as f32 * 0.001).to_ne_bytes());
    }

    let mut group = c.benchmark_group("tiff_predictor");
    for (name, spec, data) in [
        (
            "gray16_horizontal",
            ImageSpec::new(width, height, ColorType::Gray(16))
                .with_predictor(Predictor::Horizontal)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray16,
        ),
        (
            "float32_floating_point",
            ImageSpec::new(width, height, ColorType::Gray(32))
                .with_sample_format(SampleFormat::IeeeFp)
                .with_predictor(Predictor::FloatingPoint)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &float32,
        ),
    ] {
        let file = encode(&spec, data);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &file, |b, file| {
            b.iter(|| {
                let mut decoder = Decoder::new(Cursor::new(file.clone())).expect("decoder");
                black_box(decoder.read_image().expect("decode"))
            });
        });
    }
    group.finish();
}

fn bench_region(c: &mut Criterion) {
    // The COG access pattern: a small window out of a large tiled image.
    let (width, height) = (2048u32, 2048u32);
    let data = make_image(width, height, 1);
    let spec = ImageSpec::new(width, height, ColorType::Gray(8)).with_layout(Layout::Tiles {
        width: 256,
        length: 256,
    });
    let file = encode(&spec, &data);

    let mut group = c.benchmark_group("tiff_read_region");
    group.throughput(Throughput::Bytes(512 * 512));
    group.bench_function("512x512_window_of_2048x2048_tiled", |b| {
        b.iter(|| {
            let mut decoder = Decoder::new(Cursor::new(file.clone())).expect("decoder");
            black_box(decoder.read_region(768, 768, 512, 512).expect("region"))
        });
    });
    group.finish();
}

fn bench_encode(c: &mut Criterion) {
    let (width, height) = (1024u32, 1024u32);
    let gray = make_image(width, height, 1);
    let rgb = make_image(width, height, 3);

    let mut group = c.benchmark_group("tiff_encode");
    for (name, spec, data) in [
        (
            "gray8_none",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ),
        (
            "gray8_packbits",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::PackBits)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ),
        (
            "rgb8_tiles_packbits",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::PackBits)
                .with_layout(Layout::Tiles {
                    width: 256,
                    length: 256,
                }),
            &rgb,
        ),
    ] {
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &spec, |b, spec| {
            b.iter(|| black_box(encode(spec, data).len()));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_decode,
    bench_predictor,
    bench_region,
    bench_encode
);
criterion_main!(benches);

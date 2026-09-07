//! Decode benchmarks, by colour type and by unfilter kernel.
//!
//! Fixtures are generated in-process so the benchmark is hermetic: no files,
//! no network, no reference crate. Run with
//! `cargo bench -p oxiarc-png -- decode`.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use oxiarc_png::chunk::{self, SIGNATURE, write_chunk};
use oxiarc_png::filter::{RowFilter, apply_filter, unfilter};
use oxiarc_png::{
    ApngDecoder, ApngEncoder, BitDepth, BlendOp, BytesPerPixel, ColorType, DisposeOp, Encoder,
    Filter, FrameControl, Transformations,
};

/// A deterministic pseudo-random byte source.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    fn next_u8(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }
}

/// Photograph-like data: smooth gradients plus a little noise, which is what
/// the filters are designed for. Pure noise would make every filter useless
/// and the benchmark meaningless.
fn plausible_image(width: usize, height: usize, channels: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x5EED);
    let mut out = vec![0u8; width * height * channels];
    for y in 0..height {
        for x in 0..width {
            for c in 0..channels {
                let base = ((x * 3 + y * 5 + c * 40) % 256) as u8;
                out[(y * width + x) * channels + c] = base.wrapping_add(rng.next_u8() >> 4);
            }
        }
    }
    out
}

fn build_png(
    width: u32,
    height: u32,
    color_type: ColorType,
    bit_depth: BitDepth,
    interlace: bool,
    samples: &[u8],
    palette: Option<&[u8]>,
) -> Vec<u8> {
    let bpp = BytesPerPixel::from_color_and_depth(color_type, bit_depth);
    let stride = (width as usize * color_type.samples() * usize::from(bit_depth as u8)).div_ceil(8);
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    let mut previous: Vec<u8> = Vec::new();
    for y in 0..height as usize {
        let row = &samples[y * stride..(y + 1) * stride];
        raw.push(RowFilter::Paeth.into_u8());
        raw.extend_from_slice(&apply_filter(RowFilter::Paeth, bpp, &previous, row));
        previous = row.to_vec();
    }
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = bit_depth as u8;
    ihdr[9] = color_type as u8;
    ihdr[12] = u8::from(interlace);
    let mut out = SIGNATURE.to_vec();
    write_chunk(&mut out, chunk::IHDR, &ihdr).expect("ihdr");
    if let Some(palette) = palette {
        write_chunk(&mut out, chunk::PLTE, palette).expect("plte");
    }
    let idat = oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib");
    write_chunk(&mut out, chunk::IDAT, &idat).expect("idat");
    write_chunk(&mut out, chunk::IEND, &[]).expect("iend");
    out
}

fn decode_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");
    let cases: &[(&str, ColorType, BitDepth, u32)] = &[
        ("rgba8_512", ColorType::Rgba, BitDepth::Eight, 512),
        ("rgb8_512", ColorType::Rgb, BitDepth::Eight, 512),
        ("gray8_512", ColorType::Grayscale, BitDepth::Eight, 512),
        ("gray16_256", ColorType::Grayscale, BitDepth::Sixteen, 256),
        ("rgb16_256", ColorType::Rgb, BitDepth::Sixteen, 256),
        ("palette8_512", ColorType::Indexed, BitDepth::Eight, 512),
    ];
    let palette: Vec<u8> = (0..256u16)
        .flat_map(|i| [i as u8, (i * 3) as u8, (i * 7) as u8])
        .collect();
    for &(name, color_type, bit_depth, size) in cases {
        let channels = color_type.samples() * usize::from(bit_depth as u8) / 8;
        let samples = plausible_image(size as usize, size as usize, channels.max(1));
        let png = build_png(
            size,
            size,
            color_type,
            bit_depth,
            false,
            &samples,
            (color_type == ColorType::Indexed).then_some(&palette[..]),
        );
        group.throughput(criterion::Throughput::Bytes(samples.len() as u64));
        group.bench_function(name, |b| {
            b.iter(|| oxiarc_png::decode(black_box(&png)).expect("decode"))
        });
    }
    group.finish();
}

fn interlaced_and_transform_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_variants");
    let samples = plausible_image(256, 256, 1);
    let png = build_png(
        256,
        256,
        ColorType::Grayscale,
        BitDepth::Eight,
        false,
        &samples,
        None,
    );
    group.bench_function("gray8_256_to_rgba8", |b| {
        b.iter(|| {
            oxiarc_png::decode_with(
                black_box(&png),
                Default::default(),
                Transformations::EXPAND | Transformations::ALPHA,
            )
            .expect("decode")
        })
    });
    group.bench_function("gray8_256_identity", |b| {
        b.iter(|| oxiarc_png::decode(black_box(&png)).expect("decode"))
    });
    group.finish();
}

fn unfilter_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("unfilter");
    let row: Vec<u8> = plausible_image(4096, 1, 1);
    let previous: Vec<u8> = plausible_image(4096, 1, 1);
    for bpp in [
        BytesPerPixel::One,
        BytesPerPixel::Two,
        BytesPerPixel::Three,
        BytesPerPixel::Four,
        BytesPerPixel::Six,
        BytesPerPixel::Eight,
    ] {
        group.throughput(criterion::Throughput::Bytes(row.len() as u64));
        group.bench_function(format!("paeth_bpp{}", bpp.into_usize()), |b| {
            b.iter_batched(
                || row.clone(),
                |mut current| {
                    unfilter(
                        RowFilter::Paeth,
                        bpp,
                        black_box(&previous),
                        black_box(&mut current),
                    );
                    current
                },
                criterion::BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

/// Encode by filter strategy, on a size big enough that a strategy's
/// per-row cost (as opposed to fixed per-call overhead) dominates.
///
/// No comparison against the `png` crate lives in this repository —
/// `deny.toml` bans it from ever becoming a dependency here, per policy.
/// Any ratio against `png`'s own encoder has to be measured in an external
/// scratch project instead; see the crate's `TODO.md`/handoff notes for
/// whether that measurement has been taken and, if so, its recorded
/// numbers, rather than duplicating a moving target in a doc comment.
fn encode_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");
    let samples = plausible_image(1024, 1024, 4);
    let strategies: &[(&str, Filter)] = &[
        ("none", Filter::NoFilter),
        ("up", Filter::Up),
        ("adaptive", Filter::Adaptive),
        ("minentropy", Filter::MinEntropy),
    ];
    for &(name, filter) in strategies {
        group.throughput(criterion::Throughput::Bytes(samples.len() as u64));
        group.bench_function(format!("rgba8_1024_{name}"), |b| {
            b.iter(|| {
                let mut out = Vec::new();
                let mut enc = Encoder::new(&mut out, 1024, 1024);
                enc.set_color(ColorType::Rgba);
                enc.set_depth(BitDepth::Eight);
                enc.set_filter(filter);
                let mut w = enc.write_header().expect("header");
                w.write_image_data(black_box(&samples)).expect("data");
                w.finish().expect("finish");
                out
            })
        });
    }
    group.finish();
}

/// Interlaced vs. non-interlaced encoding at the same size, so the Adam7
/// pass-extraction overhead (`extract_pass_row`, run once per pass instead
/// of once per image) is visible as a ratio rather than an absolute
/// number alone.
fn encode_interlace_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode_variants");
    let samples = plausible_image(512, 512, 4);
    group.throughput(criterion::Throughput::Bytes(samples.len() as u64));
    for (name, interlaced) in [
        ("encode_interlaced_512", true),
        ("encode_noninterlaced_512", false),
    ] {
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut out = Vec::new();
                let mut enc = Encoder::new(&mut out, 512, 512);
                enc.set_color(ColorType::Rgba);
                enc.set_depth(BitDepth::Eight);
                enc.set_interlaced(interlaced);
                let mut w = enc.write_header().expect("header");
                w.write_image_data(black_box(&samples)).expect("data");
                w.finish().expect("finish");
                out
            })
        });
    }
    group.finish();
}

/// Compositing a 30-frame APNG end to end (`ApngDecoder::next_composed`
/// for every frame), the shape a scrubber or a `save_all`-equivalent
/// consumer would drive.
fn apng_compose_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("apng");
    let (width, height) = (64u32, 64u32);
    let frame_pixels = plausible_image(width as usize, height as usize, 4);
    let frame_count = 30u32;
    let mut buf = Vec::new();
    {
        let mut enc = ApngEncoder::new(&mut buf, width, height, frame_count, 1).expect("new");
        for i in 0..frame_count {
            let ctl = FrameControl {
                width,
                height,
                dispose_op: if i % 3 == 0 {
                    DisposeOp::Background
                } else {
                    DisposeOp::None
                },
                blend_op: if i % 2 == 0 {
                    BlendOp::Source
                } else {
                    BlendOp::Over
                },
                delay_num: 1,
                delay_den: 30,
                ..FrameControl::default()
            };
            enc.write_frame(&ctl, &frame_pixels).expect("frame");
        }
        enc.finish().expect("finish");
    }
    group.throughput(criterion::Throughput::Elements(u64::from(frame_count)));
    group.bench_function("apng_compose_30frames", |b| {
        b.iter(|| {
            let mut dec = ApngDecoder::new(black_box(&buf[..])).expect("open");
            let mut composed = 0u32;
            while dec.next_composed().expect("compose").is_some() {
                composed += 1;
            }
            composed
        })
    });
    group.finish();
}

criterion_group!(
    benches,
    decode_benches,
    interlaced_and_transform_benches,
    unfilter_benches,
    encode_benches,
    encode_interlace_benches,
    apng_compose_benches
);
criterion_main!(benches);

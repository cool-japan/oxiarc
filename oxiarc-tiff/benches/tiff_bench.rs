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

/// A whole-image decode benchmark for every codec this build ships.
fn bench_codecs(c: &mut Criterion) {
    let (width, height) = (1024u32, 1024u32);
    let gray = make_image(width, height, 1);
    let rgb = make_image(width, height, 3);
    let bilevel: Vec<u8> = (0..(width * height) as usize)
        .map(|i| u8::from((i / 7 + i / (width as usize * 3)) % 5 == 0))
        .collect();

    let mut cases: Vec<(&str, ImageSpec, &[u8])> = vec![
        (
            "gray8_lzw",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Lzw)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ),
        (
            "gray8_deflate",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Deflate { level: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ),
        (
            "rgb8_deflate_predictor",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::Deflate { level: 6 })
                .with_predictor(Predictor::Horizontal)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &rgb,
        ),
        (
            "bilevel_g4",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup4)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &bilevel,
        ),
        (
            "bilevel_g3_2d",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup3 {
                    two_dimensional: true,
                    byte_align_eol: false,
                })
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &bilevel,
        ),
    ];
    if cfg!(feature = "zstd") {
        cases.push((
            "gray8_zstd",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Zstd { level: 3 })
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ));
    }
    if cfg!(feature = "lzma") {
        cases.push((
            "gray8_lzma",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Lzma { preset: 1 })
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ));
    }
    if cfg!(feature = "jpeg") {
        cases.push((
            "gray8_jpeg",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Jpeg {
                    quality: 80,
                    shared_tables: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &gray,
        ));
        cases.push((
            "ycbcr_jpeg_420",
            ImageSpec::new(width, height, ColorType::YCbCr(8))
                .with_compression(Compression::Jpeg {
                    quality: 80,
                    shared_tables: true,
                })
                .with_ycbcr_subsampling(2, 2)
                .with_layout(Layout::Strips { rows_per_strip: 64 }),
            &rgb,
        ));
    }

    let mut group = c.benchmark_group("tiff_codec_decode");
    for (name, spec, data) in &cases {
        let bytes = encode(spec, data);
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &bytes, |b, bytes| {
            b.iter(|| {
                let mut decoder = Decoder::new(Cursor::new(bytes.clone())).expect("decoder");
                black_box(decoder.read_image().expect("decode"));
            });
        });
    }
    group.finish();

    let mut group = c.benchmark_group("tiff_codec_encode");
    for (name, spec, data) in &cases {
        group.throughput(Throughput::Bytes(data.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), data, |b, data| {
            b.iter(|| black_box(encode(spec, data).len()));
        });
    }
    group.finish();
}

/// A dictionary-of-`Vec` LZW decoder, the shape this crate's LZW path had
/// before `oxiarc_lzw::decompress_tiff_into` landed.
///
/// It is here so the "at least 3x the old path" gate can be *measured* rather
/// than asserted: the old code no longer exists to benchmark, but its data
/// structure — one `Vec<u8>` per dictionary entry, cloned on every code — is
/// what made it slow, and that is reproduced exactly.
fn legacy_lzw_decode(src: &[u8]) -> Vec<u8> {
    const CLEAR: u16 = 256;
    const EOI: u16 = 257;
    let mut dictionary: Vec<Vec<u8>> = Vec::with_capacity(4096);
    let reset = |dictionary: &mut Vec<Vec<u8>>| {
        dictionary.clear();
        for value in 0..256u16 {
            dictionary.push(vec![value as u8]);
        }
        dictionary.push(Vec::new());
        dictionary.push(Vec::new());
    };
    reset(&mut dictionary);
    let mut out: Vec<u8> = Vec::new();
    let mut width = 9u32;
    let mut bit = 0usize;
    let mut previous: Option<Vec<u8>> = None;
    let total_bits = src.len() * 8;
    while bit + width as usize <= total_bits {
        let mut code = 0u16;
        for offset in 0..width as usize {
            let index = bit + offset;
            let byte = src[index / 8];
            code = (code << 1) | u16::from((byte >> (7 - index % 8)) & 1);
        }
        bit += width as usize;
        if code == EOI {
            break;
        }
        if code == CLEAR {
            reset(&mut dictionary);
            width = 9;
            previous = None;
            continue;
        }
        let entry = match dictionary.get(usize::from(code)) {
            Some(entry) if !entry.is_empty() => entry.clone(),
            _ => match &previous {
                Some(prefix) => {
                    let mut entry = prefix.clone();
                    entry.push(prefix[0]);
                    entry
                }
                None => break,
            },
        };
        out.extend_from_slice(&entry);
        if let Some(prefix) = &previous {
            let mut new_entry = prefix.clone();
            new_entry.push(entry[0]);
            dictionary.push(new_entry);
        }
        previous = Some(entry);
        width = match dictionary.len() {
            n if n + 1 >= 2048 => 12,
            n if n + 1 >= 1024 => 11,
            n if n + 1 >= 512 => 10,
            _ => 9,
        };
    }
    out
}

/// The gate from the design review: an LZW strip must decode at least three
/// times faster than the dictionary-of-`Vec` path it replaced.
fn bench_lzw_strip(c: &mut Criterion) {
    let (width, rows) = (1024u32, 64u32);
    let strip_pixels = make_image(width, rows, 1);
    let spec = ImageSpec::new(width, rows, ColorType::Gray(8))
        .with_compression(Compression::Lzw)
        .with_layout(Layout::Strips {
            rows_per_strip: rows,
        });
    let file = encode(&spec, &strip_pixels);
    let strip = {
        let mut decoder = Decoder::new(Cursor::new(file)).expect("decoder");
        decoder.read_strip_raw(0).expect("raw strip")
    };
    // Both paths must produce the same bytes, or the comparison is worthless.
    let mut fast = vec![0u8; strip_pixels.len()];
    let written = oxiarc_tiff::compression::decode_into(
        &strip,
        &mut fast,
        &oxiarc_tiff::compression::CodecContext::new(
            oxiarc_tiff::CompressionMethod::Lzw,
            width as usize,
            rows as usize,
            &[8],
            1,
            oxiarc_tiff::Endian::Little,
        ),
    )
    .expect("decode");
    assert_eq!(written, strip_pixels.len());
    assert_eq!(fast, strip_pixels);
    assert_eq!(legacy_lzw_decode(&strip), strip_pixels);

    let mut group = c.benchmark_group("tiff_lzw_strip");
    group.throughput(Throughput::Bytes(strip_pixels.len() as u64));
    group.bench_function("decompress_tiff_into", |b| {
        let cx = oxiarc_tiff::compression::CodecContext::new(
            oxiarc_tiff::CompressionMethod::Lzw,
            width as usize,
            rows as usize,
            &[8],
            1,
            oxiarc_tiff::Endian::Little,
        );
        let mut out = vec![0u8; strip_pixels.len()];
        b.iter(|| {
            black_box(
                oxiarc_tiff::compression::decode_into(&strip, &mut out, &cx).expect("decode"),
            );
        });
    });
    group.bench_function("legacy_vec_dictionary", |b| {
        b.iter(|| black_box(legacy_lzw_decode(&strip).len()));
    });
    group.finish();
}

/// Serial versus `rayon`-parallel decode and encode, on an image large
/// enough (4096x4096, tiled) that per-chunk work dominates fixed overhead.
///
/// **The decode half of this pair is the feature's weakest case, not its
/// best.** The fixture is PackBits, whose decode is `memcpy`-bound, so the
/// serial fetch pass and the thread-pool dispatch are most of what the
/// parallel arm adds: an interleaved A/B puts parallel PackBits decode
/// somewhere around 0.9x-1.1x of serial depending on how compressible the
/// pixels are, and uncompressed at ~0.66x, while LZW tiles come out 2.6x-3.6x
/// *faster* and Deflate and the fax codecs at 2.0x-3.0x. Quote the README's
/// by-codec table, not a single number from this group. The encode half has no
/// such caveat -- every codec parallelises there.
///
/// tiff-design.md P7 asks for this pair specifically. The other numeric gate
/// -- critique.md section 7's "whole-image decode within 1.25x of `tiffcp`" --
/// is *not* measured here, because it needs an external binary rather than a
/// Rust benchmark; the crate README's "Benchmarks" section carries the
/// measured table. Read it before quoting the gate as met: the uncompressed,
/// PackBits and LZMA paths clear it, but LZW, Deflate, ZSTD and JPEG land at
/// roughly 1.4x-6x of `tiffcp`'s wall clock even though `tiffcp` is charged
/// with an encode and a file write we do not do. That shortfall is in the
/// shared codec crates, not in this one: the same image decodes in a few
/// milliseconds with no codec at all, and a codec-only strip measurement puts
/// the throughput squarely inside `oxiarc-lzw` / `oxiarc-zstd`.
#[cfg(feature = "rayon")]
fn bench_rayon(c: &mut Criterion) {
    let (width, height) = (4096u32, 4096u32);
    let gray = make_image(width, height, 1);
    let spec = ImageSpec::new(width, height, ColorType::Gray(8))
        .with_compression(Compression::PackBits)
        .with_layout(Layout::Tiles {
            width: 256,
            length: 256,
        });
    let bytes = encode(&spec, &gray);

    let mut group = c.benchmark_group("tiff_rayon_decode");
    group.throughput(Throughput::Bytes(gray.len() as u64));
    group.bench_function("serial", |b| {
        b.iter(|| {
            let mut decoder = Decoder::new(Cursor::new(bytes.clone())).expect("decoder");
            black_box(decoder.read_image().expect("decode").len())
        });
    });
    group.bench_function("parallel", |b| {
        b.iter(|| {
            let mut decoder = Decoder::new(Cursor::new(bytes.clone())).expect("decoder");
            black_box(decoder.read_image_parallel().expect("decode").len())
        });
    });
    group.finish();

    let mut group = c.benchmark_group("tiff_rayon_encode");
    group.throughput(Throughput::Bytes(gray.len() as u64));
    group.bench_function("serial", |b| {
        b.iter(|| {
            let mut buffer = Cursor::new(Vec::new());
            let mut encoder = Encoder::new(&mut buffer).expect("encoder");
            encoder.write_image(&spec, &gray).expect("write");
            encoder.finish().expect("finish");
            black_box(buffer.into_inner().len())
        });
    });
    group.bench_function("parallel", |b| {
        b.iter(|| {
            let mut buffer = Cursor::new(Vec::new());
            let mut encoder = Encoder::new(&mut buffer).expect("encoder");
            encoder
                .write_image_parallel(&spec, &gray)
                .expect("write parallel");
            encoder.finish().expect("finish");
            black_box(buffer.into_inner().len())
        });
    });
    group.finish();
}

#[cfg(feature = "rayon")]
criterion_group!(
    benches,
    bench_decode,
    bench_predictor,
    bench_region,
    bench_encode,
    bench_codecs,
    bench_lzw_strip,
    bench_rayon
);
#[cfg(not(feature = "rayon"))]
criterion_group!(
    benches,
    bench_decode,
    bench_predictor,
    bench_region,
    bench_encode,
    bench_codecs,
    bench_lzw_strip
);
criterion_main!(benches);

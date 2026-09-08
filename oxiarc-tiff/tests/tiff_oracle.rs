//! Differential oracle tests against libtiff (`tiffcp`, `tiffinfo`), Pillow and
//! `tifffile`.
//!
//! Rationale: an oxiarc-encode -> oxiarc-decode round-trip can pass while the
//! codec is a private dialect that no real TIFF tool accepts. These tests
//! validate **both** directions against independent references:
//!
//! 1. **decode** — fixtures written by `tifffile` (and re-coded by `tiffcp`)
//!    must decode to byte-identical sample arrays here;
//! 2. **encode** — files written here must be read by `tiffinfo` with no
//!    warnings, re-coded by `tiffcp -c none`, and opened by `tifffile` with
//!    byte-identical pixels.
//!
//! Gated behind the `tiff-oracle` feature. Every test self-skips (prints a
//! note, does not fail) when the reference tool is unavailable, exactly like
//! `oxiarc-snappy`'s `snappy-oracle`.
#![cfg(feature = "tiff-oracle")]

mod oracle_support;

use oracle_support::{
    Fixture, codec_is_available, colour_for, decode_file, every_codec_is_available, find_python,
    find_tool, read_manifest, run_driver, scratch_dir, write_driver,
};
use oxiarc_tiff::tags::PlanarConfiguration;
use oxiarc_tiff::{
    ColorType, Compression, Decoder, Encoder, Endian, ImageSpec, Layout, VariantChoice,
};
use std::fs;
use std::io::Cursor;
use std::process::Command;

#[test]
fn reference_written_fixtures_decode_byte_identically() {
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let dir = scratch_dir("decode");
    let driver = write_driver(&dir);
    run_driver(&python, &driver, "gen", &dir);

    let fixtures = read_manifest(&dir);
    assert!(!fixtures.is_empty(), "the driver produced no fixtures");
    // Guard against a silently shrinking driver: these are the rows of the
    // edge-case register that only an external writer can produce.
    for required in [
        "gray8_tiles",
        "rgb8_planar",
        "gray8_bigtiff",
        "gray8_bigendian",
        "float16",
        "float64",
        "gray64",
        "cmyk8",
        "bilevel_miniswhite",
        "gray8_miniswhite",
        "palette8",
    ] {
        assert!(
            fixtures.iter().any(|f| f.name == required),
            "the driver no longer emits the `{required}` fixture"
        );
    }
    let mut skipped = 0usize;
    let mut checked = 0usize;
    for fixture in &fixtures {
        let path = dir.join(format!("{}.tif", fixture.name));
        // The driver writes fixtures for every codec libtiff has; a build
        // without one of them skips its fixtures rather than failing on
        // `FeatureNotCompiled` while reading them.
        if !codec_is_available(&path) {
            skipped += 1;
            continue;
        }
        let expected = fs::read(dir.join(format!("{}.raw", fixture.name))).expect("raw");
        let got = decode_file(&path);
        assert_eq!(
            got.len(),
            expected.len(),
            "{}: decoded {} bytes, expected {}",
            fixture.name,
            got.len(),
            expected.len()
        );
        assert_eq!(got, expected, "{}: pixel mismatch", fixture.name);
        checked += 1;
    }
    assert!(checked > 0, "every fixture was skipped");
    if every_codec_is_available() {
        assert_eq!(skipped, 0, "an all-codecs build must skip no fixture");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tiffcp_recoded_fixtures_decode_byte_identically() {
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    let dir = scratch_dir("tiffcp");
    let driver = write_driver(&dir);
    run_driver(&python, &driver, "gen", &dir);

    // Every variant libtiff can produce for the codecs this build supports.
    let variants: [(&str, &[&str]); 28] = [
        ("none", &["-c", "none"]),
        ("packbits", &["-c", "packbits"]),
        ("r1", &["-c", "none", "-r", "1"]),
        ("r8", &["-c", "none", "-r", "8"]),
        ("onestrip", &["-c", "none", "-r", "1000000"]),
        ("tiles16", &["-c", "none", "-t", "-w", "16", "-l", "16"]),
        (
            "tiles_packbits",
            &["-c", "packbits", "-t", "-w", "32", "-l", "16"],
        ),
        ("bigendian", &["-c", "none", "-B"]),
        ("bigtiff", &["-c", "packbits", "-8"]),
        ("separate", &["-c", "none", "-p", "separate"]),
        // FillOrder 2. libtiff reverses the bits of every byte of the *raw*
        // strip at every bit depth, so these catch a decoder that only
        // reverses sub-byte data, or reverses after decompression.
        ("fill2_none", &["-c", "none", "-f", "lsb2msb"]),
        ("fill2_packbits", &["-c", "packbits", "-f", "lsb2msb"]),
        (
            "fill2_tiles",
            &[
                "-c", "packbits", "-f", "lsb2msb", "-t", "-w", "16", "-l", "16",
            ],
        ),
        ("fill1_none", &["-c", "none", "-f", "msb2lsb"]),
        // The lossless codecs, with and without the predictors libtiff only
        // installs for them. `zip:3` is the floating-point predictor, which
        // no other tool on this machine can write.
        ("lzw", &["-c", "lzw"]),
        ("lzw_pred2", &["-c", "lzw:2"]),
        ("zip", &["-c", "zip"]),
        ("zip_pred2", &["-c", "zip:2"]),
        ("zip_pred3", &["-c", "zip:3"]),
        ("lzma", &["-c", "lzma"]),
        ("zstd", &["-c", "zstd"]),
        ("zstd_pred2", &["-c", "zstd:2"]),
        // The codecs crossed with the container and layout options, so a
        // codec that only works in the simplest shape cannot pass: a
        // bit-reversed *compressed* stream, a codec in tiles, in separate
        // planes, and in a BigTIFF.
        ("fill2_lzw", &["-c", "lzw", "-f", "lsb2msb"]),
        ("fill2_zip", &["-c", "zip", "-f", "lsb2msb"]),
        ("lzw_bigtiff", &["-c", "lzw", "-8"]),
        ("zip_tiles", &["-c", "zip", "-t", "-w", "16", "-l", "16"]),
        ("zip_separate", &["-c", "zip", "-p", "separate"]),
        (
            "lzma_tiles_pred2",
            &["-c", "lzma:2", "-t", "-w", "16", "-l", "16"],
        ),
    ];

    let fixtures = read_manifest(&dir);
    let mut checked = 0usize;
    // Sample widths for which a `-f lsb2msb` re-coding was confirmed to carry
    // FillOrder 2. Without this the `fill2_*` rows could pass vacuously: if
    // libtiff ever stopped writing tag 266 for a depth, the re-coded file
    // would simply have no fill order to get wrong.
    let mut fill_order_depths: Vec<String> = Vec::new();
    let mut labels_seen: Vec<&str> = Vec::new();
    // Rows `tiffcp` produced but this build cannot decode, so the row-coverage
    // assertions below can tell "libtiff never wrote it" (a real hole) from
    // "this build does not compile that codec" (a feature choice). In an
    // all-codecs build this stays empty and every assertion is unchanged.
    let mut labels_uncompiled: Vec<&str> = Vec::new();
    let mut codecs_seen: Vec<oxiarc_tiff::CompressionMethod> = Vec::new();
    let mut predictors_seen: Vec<oxiarc_tiff::Predictor> = Vec::new();
    for fixture in &fixtures {
        let source = dir.join(format!("{}.tif", fixture.name));
        let expected = fs::read(dir.join(format!("{}.raw", fixture.name))).expect("raw");
        for (label, args) in variants {
            let target = dir.join(format!("{}_{label}.tif", fixture.name));
            let status = Command::new(&tiffcp)
                .args(args)
                .arg(&source)
                .arg(&target)
                .output()
                .expect("spawn tiffcp");
            if !status.status.success() {
                // libtiff refuses some combinations (for example separate
                // planes for a single-channel image); that is not our failure.
                continue;
            }
            if !codec_is_available(&target) {
                // libtiff wrote a codec this build does not compile.
                if !labels_uncompiled.contains(&label) {
                    labels_uncompiled.push(label);
                }
                continue;
            }
            if label.starts_with("fill2") {
                let bytes = fs::read(&target).expect("read recoded");
                let mut probe = Decoder::new(Cursor::new(bytes)).expect("decoder");
                let tag = probe
                    .find_tag(oxiarc_tiff::Tag::FillOrder)
                    .expect("fill order tag")
                    .and_then(|value| value.first_u64());
                assert_eq!(
                    tag,
                    Some(2),
                    "`tiffcp {}` on {} did not write FillOrder 2, so this row \
                     proves nothing; drop it or pick another fixture",
                    args.join(" "),
                    fixture.name
                );
                if !fill_order_depths.contains(&fixture.dtype) {
                    fill_order_depths.push(fixture.dtype.clone());
                }
            }
            let got = decode_file(&target);
            assert_eq!(
                got,
                expected,
                "{} through `tiffcp {}`",
                fixture.name,
                args.join(" ")
            );
            let bytes = fs::read(&target).expect("read recoded");
            let mut probe = Decoder::new(Cursor::new(bytes)).expect("decoder");
            let method = probe.info().expect("info").compression;
            if !codecs_seen.contains(&method) {
                codecs_seen.push(method);
            }
            let predictor = probe.info().expect("info").predictor;
            if !predictors_seen.contains(&predictor) {
                predictors_seen.push(predictor);
            }
            if !labels_seen.contains(&label) {
                labels_seen.push(label);
            }
            checked += 1;
        }
    }
    assert!(checked > 500, "only {checked} tiffcp variants were checked");
    if every_codec_is_available() {
        assert!(
            labels_uncompiled.is_empty(),
            "an all-codecs build must skip no variant, skipped {labels_uncompiled:?}"
        );
    }
    // Every row must have produced at least one file: a `tiffcp` invocation
    // that always failed would shrink the matrix without failing anything.
    for (label, args) in variants {
        assert!(
            labels_seen.contains(&label) || labels_uncompiled.contains(&label),
            "`tiffcp {}` never produced a file, so that row proves nothing",
            args.join(" ")
        );
    }
    // The matrix is only worth its runtime if libtiff really produced every
    // codec: a row whose `tiffcp` invocation silently failed would otherwise
    // shrink the coverage without failing anything.
    for method in [
        oxiarc_tiff::CompressionMethod::None,
        oxiarc_tiff::CompressionMethod::PackBits,
        oxiarc_tiff::CompressionMethod::Lzw,
        oxiarc_tiff::CompressionMethod::AdobeDeflate8,
        oxiarc_tiff::CompressionMethod::Lzma,
        oxiarc_tiff::CompressionMethod::Zstd,
    ] {
        assert!(
            codecs_seen.contains(&method) || !method.is_available(),
            "no `tiffcp` variant produced {method}; seen: {codecs_seen:?}"
        );
    }
    for predictor in [
        oxiarc_tiff::Predictor::Horizontal,
        oxiarc_tiff::Predictor::FloatingPoint,
    ] {
        assert!(
            predictors_seen.contains(&predictor),
            "no `tiffcp` variant produced predictor {predictor}; seen: {predictors_seen:?}"
        );
    }
    // The every-bit-depth claim in the crate docs is only as good as the
    // widths this loop actually exercised with FillOrder 2.
    for width in ["uint8", "uint16", "uint32", "uint64", "float32", "float64"] {
        assert!(
            fill_order_depths.iter().any(|d| d == width),
            "no FillOrder-2 fixture covered {width}; confirmed: {fill_order_depths:?}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn libtiff_and_tifffile_read_what_we_write() {
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let dir = scratch_dir("encode");
    let driver = write_driver(&dir);
    run_driver(&python, &driver, "gen", &dir);
    let fixtures = read_manifest(&dir);

    for fixture in &fixtures {
        let raw = fs::read(dir.join(format!("{}.raw", fixture.name))).expect("raw");
        let (bits, format) = colour_for(&fixture.dtype, fixture.samples_per_pixel);
        let colour = ColorType::Multiband {
            bit_depth: bits as u8,
            num_samples: fixture.samples_per_pixel,
        };
        let spec = ImageSpec::new(fixture.width, fixture.height, colour)
            .with_sample_format(format)
            .with_photometric(if fixture.samples_per_pixel >= 3 {
                oxiarc_tiff::PhotometricInterpretation::Rgb
            } else {
                oxiarc_tiff::PhotometricInterpretation::BlackIsZero
            })
            .with_layout(Layout::Strips { rows_per_strip: 4 });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(&spec, &raw).expect("write");
        encoder.finish().expect("finish");
        fs::write(
            dir.join(format!("{}_ours.tif", fixture.name)),
            buffer.into_inner(),
        )
        .expect("write file");
    }

    let result = run_driver(&python, &driver, "check", &dir);
    assert_eq!(result, "OK", "tifffile rejected our output: {result}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tiffinfo_reads_what_we_write_without_warnings() {
    let Some(tiffinfo) = find_tool("tiffinfo") else {
        eprintln!("skipping: libtiff's tiffinfo is not on PATH");
        return;
    };
    let dir = scratch_dir("tiffinfo");

    let mut cases: Vec<(&str, ImageSpec, Vec<u8>)> = vec![
        (
            "gray8",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| i as u8).collect(),
        ),
        (
            "gray8_packbits",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_compression(Compression::PackBits)
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| (i / 4) as u8).collect(),
        ),
        (
            "rgb8_tiles",
            ImageSpec::new(32, 32, ColorType::Rgb(8)).with_layout(Layout::Tiles {
                width: 16,
                length: 16,
            }),
            (0..32 * 32 * 3u32).map(|i| i as u8).collect(),
        ),
        (
            "rgb8_planar",
            ImageSpec::new(16, 16, ColorType::Rgb(8))
                .with_planar(PlanarConfiguration::Planar)
                .with_layout(Layout::Strips { rows_per_strip: 8 }),
            (0..16 * 16 * 3u32).map(|i| i as u8).collect(),
        ),
        (
            "gray16",
            ImageSpec::new(16, 16, ColorType::Gray(16))
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).flat_map(|i| (i as u16).to_ne_bytes()).collect(),
        ),
        (
            "gray8_bigtiff",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| i as u8).collect(),
        ),
        // One page per codec, so `tiffinfo` has to parse every compression
        // value, option tag and `JPEGTables` blob this crate writes.
        (
            "gray8_lzw",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_compression(Compression::Lzw)
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| (i * 3) as u8).collect(),
        ),
        (
            "gray16_deflate_predictor",
            ImageSpec::new(16, 16, ColorType::Gray(16))
                .with_compression(Compression::Deflate { level: 6 })
                .with_predictor(oxiarc_tiff::Predictor::Horizontal)
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32)
                .flat_map(|i| (i as u16 * 7).to_ne_bytes())
                .collect(),
        ),
        (
            "bilevel_g4",
            ImageSpec::new(16, 16, ColorType::Gray(1))
                .with_photometric(oxiarc_tiff::PhotometricInterpretation::WhiteIsZero)
                .with_compression(Compression::CcittGroup4)
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| u8::from(i % 3 == 0)).collect(),
        ),
        (
            "bilevel_g3_2d_fill",
            ImageSpec::new(16, 16, ColorType::Gray(1))
                .with_photometric(oxiarc_tiff::PhotometricInterpretation::WhiteIsZero)
                .with_compression(Compression::CcittGroup3 {
                    two_dimensional: true,
                    byte_align_eol: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| u8::from(i % 5 < 2)).collect(),
        ),
    ];
    if cfg!(feature = "lzma") {
        cases.push((
            "gray8_lzma",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_compression(Compression::Lzma { preset: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| (i / 3) as u8).collect(),
        ));
    }
    if cfg!(feature = "zstd") {
        cases.push((
            "gray8_zstd",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_compression(Compression::Zstd { level: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 4 }),
            (0..256u32).map(|i| (i / 5) as u8).collect(),
        ));
    }
    if cfg!(feature = "jpeg") {
        cases.push((
            "gray8_jpeg",
            ImageSpec::new(16, 16, ColorType::Gray(8))
                .with_compression(Compression::Jpeg {
                    quality: 85,
                    shared_tables: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 8 }),
            (0..256u32).map(|i| (i * 2) as u8).collect(),
        ));
        cases.push((
            "ycbcr_jpeg_subsampled",
            ImageSpec::new(32, 32, ColorType::YCbCr(8))
                .with_compression(Compression::Jpeg {
                    quality: 85,
                    shared_tables: true,
                })
                .with_ycbcr_subsampling(2, 2)
                .with_layout(Layout::Strips { rows_per_strip: 16 }),
            (0..32 * 32 * 3u32).map(|i| (i / 3 % 256) as u8).collect(),
        ));
    }

    for (name, spec, data) in cases {
        for (label, endian, variant) in [
            ("le", Endian::Little, VariantChoice::Classic),
            ("be", Endian::Big, VariantChoice::Classic),
            ("big", Endian::Little, VariantChoice::Big),
        ] {
            let mut buffer = Cursor::new(Vec::new());
            let mut encoder = Encoder::new(&mut buffer)
                .expect("encoder")
                .with_endian(endian)
                .with_variant(variant);
            encoder.write_image(&spec, &data).expect("write");
            encoder.finish().expect("finish");
            let path = dir.join(format!("{name}_{label}.tif"));
            fs::write(&path, buffer.into_inner()).expect("write file");

            let output = Command::new(&tiffinfo)
                .arg("-D")
                .arg(&path)
                .output()
                .expect("spawn tiffinfo");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "tiffinfo rejected {name}_{label}: {stderr}"
            );
            assert!(
                !stderr.to_lowercase().contains("warning"),
                "tiffinfo warned on {name}_{label}: {stderr}"
            );
            assert!(
                !stderr.to_lowercase().contains("error"),
                "tiffinfo errored on {name}_{label}: {stderr}"
            );

            // And libtiff can re-code it, which proves the strips are readable.
            if let Some(tiffcp) = find_tool("tiffcp") {
                let round = dir.join(format!("{name}_{label}_round.tif"));
                let status = Command::new(&tiffcp)
                    .args(["-c", "none"])
                    .arg(&path)
                    .arg(&round)
                    .output()
                    .expect("spawn tiffcp");
                assert!(
                    status.status.success(),
                    "tiffcp could not re-code {name}_{label}: {}",
                    String::from_utf8_lossy(&status.stderr)
                );
                let back = decode_file(&round);
                if name.contains("ycbcr") {
                    // libtiff converts a YCbCr page to RGB while re-coding, so
                    // the bytes are a different colour space, not different
                    // pixels. All this row proves is that libtiff read our
                    // subsampled JPEG strips and produced a full-size image;
                    // `jpeg_recodings_decode_close_to_the_reference` and
                    // `libtiff_accepts_our_subsampled_ycbcr` check the values.
                    assert_eq!(back.len(), data.len(), "{name}_{label} length");
                } else if name.contains("jpeg") {
                    // JPEG is lossy: what this proves is that libtiff decoded
                    // our strips at all, and that the pixels it got are close
                    // to the ones we wrote.
                    assert_eq!(back.len(), data.len(), "{name}_{label} length");
                    let worst = back
                        .iter()
                        .zip(data.iter())
                        .map(|(a, b)| a.abs_diff(*b))
                        .max()
                        .unwrap_or(0);
                    assert!(worst <= 24, "{name}_{label}: worst |diff| {worst}");
                } else {
                    assert_eq!(back, data, "{name}_{label} round trip through tiffcp");
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pillow_reads_what_we_write() {
    let ok = Command::new("python3")
        .args(["-c", "import PIL.Image, numpy"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping: python3 with Pillow is not available");
        return;
    }
    let dir = scratch_dir("pillow");
    let data: Vec<u8> = (0..32 * 32 * 3u32).map(|i| (i % 251) as u8).collect();
    let spec =
        ImageSpec::new(32, 32, ColorType::Rgb(8)).with_layout(Layout::Strips { rows_per_strip: 8 });
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(&spec, &data).expect("write");
    encoder.finish().expect("finish");
    let path = dir.join("rgb8.tif");
    fs::write(&path, buffer.into_inner()).expect("write file");
    fs::write(dir.join("rgb8.raw"), &data).expect("write raw");

    let script = r#"
import sys
import numpy as np
from PIL import Image
img = Image.open(sys.argv[1])
got = np.asarray(img.convert("RGB"), dtype=np.uint8).reshape(-1)
expected = np.frombuffer(open(sys.argv[2], "rb").read(), dtype=np.uint8)
print("OK" if np.array_equal(got, expected) else "FAIL")
"#;
    let script_path = dir.join("check_pillow.py");
    fs::write(&script_path, script).expect("write script");
    let output = Command::new("python3")
        .arg(&script_path)
        .arg(&path)
        .arg(dir.join("rgb8.raw"))
        .output()
        .expect("spawn python");
    assert!(
        output.status.success(),
        "Pillow failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "OK",
        "Pillow read different pixels"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Documents a real libtiff behaviour this crate must not be surprised by.
///
/// libtiff installs the predictor hooks only from the codecs that call
/// `TIFFPredictorInit` (LZW, Deflate, ZSTD, LZMA, PixarLog, LERC). With
/// `Compression = None` or `PackBits` it ignores tag 317 completely, so a file
/// this crate writes with a predictor and an unpredicted codec is *not*
/// interoperable even though both directions agree here.
#[test]
fn libtiff_ignores_the_predictor_for_uncompressed_data() {
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    let dir = scratch_dir("predictor_note");
    let data: Vec<u8> = (0..256u32).flat_map(|i| (i as u16).to_ne_bytes()).collect();
    let spec = ImageSpec::new(16, 16, ColorType::Gray(16))
        .with_predictor(oxiarc_tiff::Predictor::Horizontal)
        .with_layout(Layout::Strips { rows_per_strip: 4 });
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(&spec, &data).expect("write");
    encoder.finish().expect("finish");
    let ours = dir.join("predictor.tif");
    let bytes = buffer.into_inner();
    fs::write(&ours, &bytes).expect("write file");

    // This crate reads its own file back exactly.
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    assert_eq!(
        decoder.read_image().expect("read").to_native_bytes(),
        data,
        "our own round trip must be exact"
    );

    // libtiff, however, hands back the raw deltas.
    let round = dir.join("predictor_round.tif");
    let status = Command::new(&tiffcp)
        .args(["-c", "none"])
        .arg(&ours)
        .arg(&round)
        .output()
        .expect("spawn tiffcp");
    assert!(status.status.success());
    let libtiff_view = decode_file(&round);
    assert_ne!(
        libtiff_view, data,
        "if libtiff ever starts predicting uncompressed data, drop this test \
         and the interop caveat in the crate docs"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Multi-page: `tifffile` writes three heterogeneous pages, we walk them, then
/// we write three pages and `tifffile` walks ours.
#[test]
fn multi_page_files_agree_with_tifffile() {
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let dir = scratch_dir("multipage");
    let driver = write_driver(&dir);
    run_driver(&python, &driver, "genmulti", &dir);

    let text = fs::read_to_string(dir.join("multipage.tsv")).expect("multipage manifest");
    let pages: Vec<Fixture> = text
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            Fixture {
                name: parts[0].to_string(),
                width: parts[1].parse().expect("width"),
                height: parts[2].parse().expect("height"),
                samples_per_pixel: parts[3].parse().expect("spp"),
                dtype: parts[4].to_string(),
            }
        })
        .collect();
    assert_eq!(pages.len(), 3, "the driver must write three pages");

    // Read direction: every page decodes byte-identically, in any order.
    let bytes = fs::read(dir.join("multipage.tif")).expect("read multipage");
    let mut decoder = Decoder::new(Cursor::new(bytes.clone())).expect("decoder");
    assert_eq!(decoder.image_count().expect("count"), pages.len());
    for order in [[0usize, 1, 2], [2, 0, 1]] {
        for index in order {
            let page = &pages[index];
            decoder.seek_to_image(index).expect("seek");
            assert_eq!(
                decoder.dimensions().expect("dims"),
                (page.width, page.height),
                "page {index} dimensions"
            );
            let expected =
                fs::read(dir.join(format!("multipage_{}.raw", page.name))).expect("page raw");
            let got = decoder.read_image().expect("decode page").to_native_bytes();
            assert_eq!(got, expected, "page {index} pixels");
        }
    }

    // Write direction: the same three pages, read back by `tifffile`.
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    for page in &pages {
        let raw = fs::read(dir.join(format!("multipage_{}.raw", page.name))).expect("page raw");
        let (bits, format) = colour_for(&page.dtype, page.samples_per_pixel);
        let spec = ImageSpec::new(
            page.width,
            page.height,
            ColorType::Multiband {
                bit_depth: bits as u8,
                num_samples: page.samples_per_pixel,
            },
        )
        .with_sample_format(format)
        .with_photometric(if page.samples_per_pixel >= 3 {
            oxiarc_tiff::PhotometricInterpretation::Rgb
        } else {
            oxiarc_tiff::PhotometricInterpretation::BlackIsZero
        })
        .with_layout(Layout::Strips { rows_per_strip: 2 });
        encoder.write_image(&spec, &raw).expect("write page");
    }
    encoder.finish().expect("finish");
    fs::write(dir.join("multipage_ours.tif"), buffer.into_inner()).expect("write file");

    let result = run_driver(&python, &driver, "checkmulti", &dir);
    assert_eq!(
        result, "OK",
        "tifffile rejected our multi-page file: {result}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Sub-byte depths and palette images this crate writes must survive libtiff.
///
/// `tifffile` cannot *write* 2- or 4-bit samples without `imagecodecs`, so the
/// external check for these depths has to run in the encode direction: libtiff
/// re-codes our file, and we decode libtiff's output back to the same pixels.
#[test]
fn sub_byte_and_palette_images_survive_libtiff() {
    let Some(tiffinfo) = find_tool("tiffinfo") else {
        eprintln!("skipping: libtiff's tiffinfo is not on PATH");
        return;
    };
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    let dir = scratch_dir("subbyte");

    let mut cases: Vec<(String, ImageSpec, Vec<u8>)> = Vec::new();
    for bits in [1u8, 2, 4] {
        let max = (1u32 << bits) - 1;
        let (width, height) = (17u32, 5u32);
        let data: Vec<u8> = (0..width * height).map(|i| (i % (max + 1)) as u8).collect();
        for (label, photometric) in [
            (
                "minisblack",
                oxiarc_tiff::PhotometricInterpretation::BlackIsZero,
            ),
            (
                "miniswhite",
                oxiarc_tiff::PhotometricInterpretation::WhiteIsZero,
            ),
        ] {
            cases.push((
                format!("gray{bits}_{label}"),
                ImageSpec::new(width, height, ColorType::Gray(bits))
                    .with_photometric(photometric)
                    .with_layout(Layout::Strips { rows_per_strip: 2 }),
                data.clone(),
            ));
        }
    }
    // A 4-bit palette image; the colormap is full-range so libtiff does not
    // guess that it is an 8-bit map.
    let entries = 16usize;
    let mut map = vec![0u16; entries * 3];
    for i in 0..entries {
        let step = (i as u32) * 0x1111;
        map[i] = step as u16;
        map[entries + i] = (0xFFFF - step) as u16;
        map[2 * entries + i] = ((step / 2) as u16) | 0x0101;
    }
    let indices: Vec<u8> = (0..6u32 * 4).map(|i| (i % 16) as u8).collect();
    cases.push((
        "palette4".to_string(),
        ImageSpec::new(6, 4, ColorType::Palette(4))
            .with_color_map(map)
            .with_layout(Layout::Strips { rows_per_strip: 2 }),
        indices,
    ));

    for (name, spec, data) in &cases {
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(spec, data).expect("write");
        encoder.finish().expect("finish");
        let path = dir.join(format!("{name}.tif"));
        fs::write(&path, buffer.into_inner()).expect("write file");

        let output = Command::new(&tiffinfo)
            .arg(&path)
            .output()
            .expect("spawn tiffinfo");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "tiffinfo rejected {name}: {stderr}"
        );
        assert!(
            !stderr.to_lowercase().contains("warning") && !stderr.to_lowercase().contains("error"),
            "tiffinfo complained about {name}: {stderr}"
        );
        // libtiff omits `Bits/Sample` when it equals the TIFF default of 1.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let bits = spec.bits_per_sample.first().copied().unwrap_or(0);
        if bits != 1 {
            assert!(
                stdout.contains(&format!("Bits/Sample: {bits}")),
                "tiffinfo did not report the bit depth of {name}:\n{stdout}"
            );
        }

        let round = dir.join(format!("{name}_round.tif"));
        let status = Command::new(&tiffcp)
            .args(["-c", "none"])
            .arg(&path)
            .arg(&round)
            .output()
            .expect("spawn tiffcp");
        assert!(
            status.status.success(),
            "tiffcp could not re-code {name}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert_eq!(decode_file(&round), *data, "{name} through tiffcp");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// `FillOrder = 2` must round trip through libtiff at every bit depth.
///
/// libtiff reverses the bits of every byte of the **raw** (still compressed)
/// strip, not only of sub-byte data and not after decompression; a decoder
/// that gets either detail wrong passes its own round trip and fails here.
#[test]
fn fill_order_two_round_trips_through_libtiff() {
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    let dir = scratch_dir("fillorder");

    let gray8: Vec<u8> = (0..24u32 * 6).map(|i| (i % 253) as u8).collect();
    let gray16: Vec<u8> = (0..24u32 * 6)
        .flat_map(|i| ((i * 977) as u16).to_ne_bytes())
        .collect();
    let bilevel: Vec<u8> = (0..24u32 * 6).map(|i| (i % 3 == 0) as u8).collect();

    let cases: [(&str, ColorType, &Vec<u8>); 3] = [
        ("gray8", ColorType::Gray(8), &gray8),
        ("gray16", ColorType::Gray(16), &gray16),
        ("bilevel", ColorType::Gray(1), &bilevel),
    ];

    for (name, colour, data) in cases {
        for compression in [Compression::None, Compression::PackBits] {
            let spec = ImageSpec::new(24, 6, colour)
                .with_fill_order(oxiarc_tiff::FillOrder::Lsb2Msb)
                .with_compression(compression)
                .with_layout(Layout::Strips { rows_per_strip: 2 });
            let mut buffer = Cursor::new(Vec::new());
            let mut encoder = Encoder::new(&mut buffer).expect("encoder");
            encoder.write_image(&spec, data).expect("write");
            encoder.finish().expect("finish");
            let label = format!("{name}_{compression:?}");
            let path = dir.join(format!("{label}.tif"));
            fs::write(&path, buffer.into_inner()).expect("write file");

            // libtiff re-codes our FillOrder-2 file back to FillOrder 1 …
            let round = dir.join(format!("{label}_msb.tif"));
            let status = Command::new(&tiffcp)
                .args(["-c", "none", "-f", "msb2lsb"])
                .arg(&path)
                .arg(&round)
                .output()
                .expect("spawn tiffcp");
            assert!(
                status.status.success(),
                "tiffcp could not re-code {label}: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert_eq!(
                decode_file(&round),
                **data,
                "{label}: libtiff read our FillOrder-2 chunk differently"
            );

            // … and back to FillOrder 2, which we must read again.
            let again = dir.join(format!("{label}_lsb.tif"));
            let status = Command::new(&tiffcp)
                .args(["-c", "packbits", "-f", "lsb2msb"])
                .arg(&round)
                .arg(&again)
                .output()
                .expect("spawn tiffcp");
            assert!(status.status.success(), "tiffcp lsb2msb failed for {label}");
            assert_eq!(decode_file(&again), **data, "{label}: libtiff -> us");
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// Uncompressed subsampled YCbCr: nothing on PATH can *write* one, so the
/// external check is that libtiff accepts and re-codes ours.
#[test]
fn libtiff_accepts_our_subsampled_ycbcr() {
    let Some(tiffinfo) = find_tool("tiffinfo") else {
        eprintln!("skipping: libtiff's tiffinfo is not on PATH");
        return;
    };
    let dir = scratch_dir("ycbcr");
    let (width, height) = (16u32, 16u32);
    let data: Vec<u8> = (0..width * height * 3).map(|i| (i % 251) as u8).collect();

    for (h, v) in [(1u16, 1u16), (2, 1), (2, 2)] {
        let spec = ImageSpec::new(width, height, ColorType::YCbCr(8))
            .with_ycbcr_subsampling(h, v)
            .with_layout(Layout::Strips { rows_per_strip: 8 });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(&spec, &data).expect("write");
        encoder.finish().expect("finish");
        let path = dir.join(format!("ycbcr_{h}x{v}.tif"));
        let bytes = buffer.into_inner();
        fs::write(&path, &bytes).expect("write file");

        let output = Command::new(&tiffinfo)
            .arg(&path)
            .output()
            .expect("spawn tiffinfo");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "tiffinfo rejected ycbcr {h}x{v}: {stderr}"
        );
        assert!(
            !stderr.to_lowercase().contains("warning") && !stderr.to_lowercase().contains("error"),
            "tiffinfo complained about ycbcr {h}x{v}: {stderr}"
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("YCbCr Subsampling: ") && stdout.contains("YCbCr"),
            "tiffinfo did not report the subsampling of ycbcr {h}x{v}:\n{stdout}"
        );

        // And our own read of the same file agrees with the luma plane.
        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        let decoded = decoder.read_image().expect("decode").to_native_bytes();
        for pixel in 0..(width * height) as usize {
            assert_eq!(
                decoded[pixel * 3],
                data[pixel * 3],
                "luma {pixel} at {h}x{v}"
            );
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

/// **Regression (TIFF-verify V7), validated externally.** The `compat`
/// encoder's colour-type markers must write a `SampleFormat` (339) that an
/// **independent** reader agrees with.
///
/// The bug this pins was a *label* bug: the compat encoder wrote every marker
/// as `SampleFormat = Uint`, so a `Gray32Float` page carried IEEE-754 bytes
/// under an unsigned-integer tag. Our own encoder and our own decoder agreed
/// with each other before the fix as much as after it, which is exactly why
/// the round-trip tests in `tests/compat_api.rs` cannot be the whole
/// evidence: only a reader this crate did not write can say the label is
/// right. `tifffile` reports the dtype it inferred from the tag, so a
/// mislabelled page shows up as `uint32` where `float32` belongs.
#[cfg(feature = "compat")]
#[test]
fn tifffile_agrees_with_the_compat_encoders_sample_format() {
    use oxiarc_tiff::compat::encoder::{
        TiffEncoder,
        colortype::{Gray8, Gray16, Gray32Float, Gray64Float, GrayI8, GrayI16, GrayI32},
    };
    use std::process::Command;

    let Some(python) = find_python() else {
        eprintln!("skipping: python3 is not available");
        return;
    };
    let has_tifffile = Command::new(&python)
        .args(["-c", "import tifffile"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_tifffile {
        eprintln!("skipping: python3 tifffile is not available");
        return;
    }

    let dir = scratch_dir("compat_sample_format");
    let mut expected: Vec<(String, &str)> = Vec::new();

    macro_rules! emit {
        ($marker:ty, $data:expr, $dtype:literal) => {{
            let name = format!("{}.tif", stringify!($marker).to_lowercase());
            let mut buffer = Cursor::new(Vec::new());
            TiffEncoder::new(&mut buffer)
                .expect("compat encoder")
                .write_image::<$marker>(4, 2, $data)
                .expect("write");
            fs::write(dir.join(&name), buffer.into_inner()).expect("write fixture");
            expected.push((name, $dtype));
        }};
    }

    emit!(Gray8, &[1u8, 2, 3, 4, 5, 6, 7, 8], "uint8");
    emit!(Gray16, &[1u16, 2, 3, 4, 5, 6, 7, 8], "uint16");
    emit!(GrayI8, &[-1i8, 2, -3, 4, -5, 6, -7, 8], "int8");
    emit!(GrayI16, &[-1i16, 2, -3, 4, -5, 6, -7, 8], "int16");
    emit!(GrayI32, &[-1i32, 2, -3, 4, -5, 6, -7, 8], "int32");
    emit!(
        Gray32Float,
        &[0.5f32, -1.25, 2.0, 3.5, -4.0, 5.25, 6.0, 7.75],
        "float32"
    );
    emit!(
        Gray64Float,
        &[0.5f64, -1.25, 2.0, 3.5, -4.0, 5.25, 6.0, 7.75],
        "float64"
    );

    let names: Vec<&str> = expected.iter().map(|(n, _)| n.as_str()).collect();
    let script = format!(
        r#"
import tifffile, json
out = {{}}
for name in {names:?}:
    with tifffile.TiffFile({dir:?} + "/" + name) as tif:
        page = tif.pages[0]
        out[name] = [str(page.dtype), int(page.sampleformat)]
print(json.dumps(out))
"#,
        names = names,
        dir = dir.display().to_string(),
    );
    let output = Command::new(&python)
        .arg("-c")
        .arg(&script)
        .output()
        .expect("spawn python");
    assert!(
        output.status.success(),
        "tifffile could not read the compat-written pages: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = String::from_utf8_lossy(&output.stdout).into_owned();

    // `tifffile` reports the dtype it derived from `SampleFormat` +
    // `BitsPerSample`. A mislabelled float page prints `uint32` here.
    for (name, dtype) in &expected {
        let needle = format!("\"{name}\": [\"{dtype}\"");
        assert!(
            report.contains(&needle),
            "tifffile read {name} as something other than {dtype}\n{report}"
        );
    }
    // Non-vacuity: every fixture really appeared in the report.
    assert_eq!(
        report.matches("\": [\"").count(),
        expected.len(),
        "tifffile did not report every fixture:\n{report}"
    );

    let _ = fs::remove_dir_all(&dir);
}

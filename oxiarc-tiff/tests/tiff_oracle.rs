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

use oxiarc_tiff::tags::{PlanarConfiguration, SampleFormat};
use oxiarc_tiff::{
    ColorType, Compression, Decoder, Encoder, Endian, ImageSpec, Layout, VariantChoice,
};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Python driver: writes the fixture matrix and re-reads our output.
const PY_DRIVER: &str = r#"
import os
import sys

import numpy as np
import tifffile


def cases():
    """(name, array handed to imwrite, expected interleaved array, imwrite kwargs).

    `tifffile` wants a (samples, height, width) array for planarconfig
    "separate", while the reference pixel order this crate produces is always
    interleaved, so the two arrays differ for that one case.
    """
    rng = np.random.default_rng(20260907)
    out = []
    gray8 = (rng.integers(0, 256, size=(23, 31), dtype=np.uint8))
    out.append(("gray8_strips", gray8, gray8, dict(photometric="minisblack", rowsperstrip=5)))
    out.append(("gray8_onestrip", gray8, gray8,
                dict(photometric="minisblack", rowsperstrip=1 << 20)))
    gray8_tiled = rng.integers(0, 256, size=(64, 64), dtype=np.uint8)
    out.append(("gray8_tiles", gray8_tiled, gray8_tiled,
                dict(photometric="minisblack", tile=(16, 16))))
    out.append(("gray8_bigtiff", gray8, gray8, dict(photometric="minisblack", bigtiff=True)))
    out.append(("gray8_bigendian", gray8, gray8, dict(photometric="minisblack", byteorder=">")))

    rgb8 = rng.integers(0, 256, size=(17, 29, 3), dtype=np.uint8)
    out.append(("rgb8_strips", rgb8, rgb8, dict(photometric="rgb", rowsperstrip=4)))
    out.append(("rgb8_planar", np.ascontiguousarray(rgb8.transpose(2, 0, 1)), rgb8,
                dict(photometric="rgb", planarconfig="separate", rowsperstrip=4)))
    out.append(("rgb8_bigendian", rgb8, rgb8, dict(photometric="rgb", byteorder=">")))
    rgb8_tiled = rng.integers(0, 256, size=(48, 48, 3), dtype=np.uint8)
    out.append(("rgb8_tiles", rgb8_tiled, rgb8_tiled,
                dict(photometric="rgb", tile=(16, 16))))

    rgba8 = rng.integers(0, 256, size=(9, 11, 4), dtype=np.uint8)
    out.append(("rgba8", rgba8, rgba8, dict(photometric="rgb", extrasamples="unassalpha")))

    gray16 = rng.integers(0, 65536, size=(13, 19), dtype=np.uint16)
    out.append(("gray16", gray16, gray16, dict(photometric="minisblack", rowsperstrip=3)))
    out.append(("gray16_bigendian", gray16, gray16,
                dict(photometric="minisblack", byteorder=">")))

    gray32 = rng.integers(0, 1 << 31, size=(7, 5), dtype=np.uint32)
    out.append(("gray32", gray32, gray32, dict(photometric="minisblack")))

    int16 = rng.integers(-32768, 32767, size=(11, 7), dtype=np.int16)
    out.append(("int16", int16, int16, dict(photometric="minisblack")))

    f32 = rng.standard_normal(size=(12, 10)).astype(np.float32)
    out.append(("float32", f32, f32, dict(photometric="minisblack", rowsperstrip=4)))
    out.append(("float32_bigendian", f32, f32,
                dict(photometric="minisblack", byteorder=">")))

    f64 = rng.standard_normal(size=(6, 6)).astype(np.float64)
    out.append(("float64", f64, f64, dict(photometric="minisblack")))

    f16 = rng.standard_normal(size=(6, 8)).astype(np.float16)
    out.append(("float16", f16, f16, dict(photometric="minisblack")))

    cmyk = rng.integers(0, 256, size=(8, 8, 4), dtype=np.uint8)
    out.append(("cmyk8", cmyk, cmyk, dict(photometric="separated")))
    return out


def write_fixtures(dirpath):
    manifest = []
    for name, arr, expected, kwargs in cases():
        path = os.path.join(dirpath, name + ".tif")
        tifffile.imwrite(path, arr, compression=None, **kwargs)
        with open(os.path.join(dirpath, name + ".raw"), "wb") as fh:
            fh.write(np.ascontiguousarray(expected).tobytes())
        height = expected.shape[0]
        width = expected.shape[1]
        spp = expected.shape[2] if expected.ndim == 3 else 1
        manifest.append(
            "\t".join([name, str(width), str(height), str(spp), str(expected.dtype)])
        )
    with open(os.path.join(dirpath, "manifest.tsv"), "w") as fh:
        fh.write("\n".join(manifest) + "\n")


def check_ours(dirpath):
    failures = []
    for line in open(os.path.join(dirpath, "manifest.tsv")):
        name, width, height, spp, dtype = line.rstrip("\n").split("\t")
        path = os.path.join(dirpath, name + "_ours.tif")
        if not os.path.exists(path):
            continue
        expected = np.frombuffer(
            open(os.path.join(dirpath, name + ".raw"), "rb").read(), dtype=np.dtype(dtype)
        )
        got = tifffile.imread(path)
        got = np.asarray(got, dtype=np.dtype(dtype)).reshape(-1)
        if got.shape != expected.shape:
            failures.append(f"{name}: shape {got.shape} != {expected.shape}")
        elif not np.array_equal(got, expected):
            failures.append(f"{name}: pixel mismatch")
    if failures:
        print("FAIL " + "; ".join(failures))
    else:
        print("OK")


if __name__ == "__main__":
    mode = sys.argv[1]
    target = sys.argv[2]
    if mode == "gen":
        write_fixtures(target)
    elif mode == "check":
        check_ours(target)
    else:
        raise SystemExit("unknown mode " + mode)
"#;

/// `python3` with numpy and tifffile importable, or `None`.
fn find_python() -> Option<PathBuf> {
    let ok = Command::new("python3")
        .args(["-c", "import numpy, tifffile"])
        .output()
        .ok()?
        .status
        .success();
    ok.then(|| PathBuf::from("python3"))
}

/// A libtiff tool on `PATH`, or `None`.
fn find_tool(name: &str) -> Option<PathBuf> {
    let output = Command::new("which").arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// A unique scratch directory under [`std::env::temp_dir`].
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_tiff_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn write_driver(dir: &Path) -> PathBuf {
    let path = dir.join("driver.py");
    fs::write(&path, PY_DRIVER).expect("write driver");
    path
}

fn run_driver(python: &Path, driver: &Path, mode: &str, dir: &Path) -> String {
    let output = Command::new(python)
        .arg(driver)
        .arg(mode)
        .arg(dir)
        .output()
        .expect("spawn python driver");
    assert!(
        output.status.success(),
        "driver {mode} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// One row of the generated fixture manifest.
struct Fixture {
    name: String,
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    dtype: String,
}

fn read_manifest(dir: &Path) -> Vec<Fixture> {
    let text = fs::read_to_string(dir.join("manifest.tsv")).expect("manifest");
    text.lines()
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
        .collect()
}

fn colour_for(dtype: &str, spp: u16) -> (u16, SampleFormat) {
    match dtype {
        "uint8" | "int8" => (
            8,
            if dtype == "int8" {
                SampleFormat::Int
            } else {
                SampleFormat::Uint
            },
        ),
        "uint16" => (16, SampleFormat::Uint),
        "int16" => (16, SampleFormat::Int),
        "uint32" => (32, SampleFormat::Uint),
        "int32" => (32, SampleFormat::Int),
        "float16" => (16, SampleFormat::IeeeFp),
        "float32" => (32, SampleFormat::IeeeFp),
        "float64" => (64, SampleFormat::IeeeFp),
        other => panic!("unhandled dtype {other} for {spp} samples"),
    }
}

fn decode_file(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).expect("read fixture");
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    decoder.read_image().expect("decode").to_native_bytes()
}

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
    for fixture in &fixtures {
        let expected = fs::read(dir.join(format!("{}.raw", fixture.name))).expect("raw");
        let got = decode_file(&dir.join(format!("{}.tif", fixture.name)));
        assert_eq!(
            got.len(),
            expected.len(),
            "{}: decoded {} bytes, expected {}",
            fixture.name,
            got.len(),
            expected.len()
        );
        assert_eq!(got, expected, "{}: pixel mismatch", fixture.name);
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
    let variants: [(&str, &[&str]); 10] = [
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
    ];

    let fixtures = read_manifest(&dir);
    let mut checked = 0usize;
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
            let got = decode_file(&target);
            assert_eq!(
                got,
                expected,
                "{} through `tiffcp {}`",
                fixture.name,
                args.join(" ")
            );
            checked += 1;
        }
    }
    assert!(checked > 20, "only {checked} tiffcp variants were checked");
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

    let cases: [(&str, ImageSpec, Vec<u8>); 6] = [
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
    ];

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
                assert_eq!(back, data, "{name}_{label} round trip through tiffcp");
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

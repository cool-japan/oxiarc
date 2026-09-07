//! Shared machinery for the differential oracle suites.
//!
//! The Python driver, the tool probes and the scratch-directory helpers are
//! used by both `tiff_oracle.rs` (containers, geometry, colour) and
//! `tiff_oracle_codecs.rs` (one suite per codec), so they live here rather
//! than being copied.

#![allow(dead_code)]

use oxiarc_tiff::Decoder;
use oxiarc_tiff::tags::SampleFormat;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Python driver: writes the fixture matrix and re-reads our output.
pub const PY_DRIVER: &str = r#"
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

    # 64-bit integers: the widest baseline sample this crate unpacks.
    u64 = rng.integers(0, 1 << 61, size=(5, 4), dtype=np.uint64)
    out.append(("gray64", u64, u64, dict(photometric="minisblack")))

    # Bilevel. `tifffile` writes a bool array as BitsPerSample 1, and this
    # crate unpacks 1-bit samples into one 0/1 byte each, which is exactly
    # what `.astype(np.uint8)` of the same array is.
    bilevel = rng.integers(0, 2, size=(9, 13), dtype=np.uint8).astype(bool)
    out.append(("bilevel_miniswhite", bilevel, bilevel.astype(np.uint8),
                dict(photometric="miniswhite")))
    out.append(("bilevel_minisblack", bilevel, bilevel.astype(np.uint8),
                dict(photometric="minisblack")))

    # Codecs an independent writer can produce without `imagecodecs`:
    # zlib (Compression 8), lzma (34925) and zstd (50000), plus the
    # horizontal predictor, which no uncompressed writer emits.
    out.append(("gray8_zlib", gray8, gray8,
                dict(photometric="minisblack", compression="zlib", rowsperstrip=5)))
    out.append(("rgb8_zlib_tiles", rgb8_tiled, rgb8_tiled,
                dict(photometric="rgb", compression="zlib", tile=(16, 16))))
    out.append(("gray16_lzma", gray16, gray16,
                dict(photometric="minisblack", compression="lzma", rowsperstrip=3)))
    out.append(("rgb8_zstd", rgb8, rgb8,
                dict(photometric="rgb", compression="zstd", rowsperstrip=4)))
    out.append(("gray16_zlib_predictor", gray16, gray16,
                dict(photometric="minisblack", compression="zlib", predictor=True,
                     rowsperstrip=3)))
    out.append(("gray8_lzma_bigendian", gray8, gray8,
                dict(photometric="minisblack", compression="lzma", byteorder=">")))

    # MinIsWhite at 8 bits: the raw read path must NOT invert.
    out.append(("gray8_miniswhite", gray8, gray8, dict(photometric="miniswhite")))

    # Palette. The raw read path returns the indices, never the expanded RGB,
    # and so does `tifffile.imread`.
    indices = rng.integers(0, 256, size=(7, 11), dtype=np.uint8)
    colormap = rng.integers(0, 65536, size=(3, 256), dtype=np.uint16)
    out.append(("palette8", indices, indices,
                dict(photometric="palette", colormap=colormap)))
    return out


def write_multipage(dirpath):
    """A three-page file whose pages differ in size and channel count."""
    rng = np.random.default_rng(20260908)
    pages = [
        rng.integers(0, 256, size=(5, 6), dtype=np.uint8),
        rng.integers(0, 256, size=(7, 4, 3), dtype=np.uint8),
        rng.integers(0, 65536, size=(3, 9), dtype=np.uint16),
    ]
    with tifffile.TiffWriter(os.path.join(dirpath, "multipage.tif")) as writer:
        for page in pages:
            writer.write(
                page,
                photometric="rgb" if page.ndim == 3 else "minisblack",
                compression=None,
                contiguous=False,
            )
    lines = []
    for i, page in enumerate(pages):
        with open(os.path.join(dirpath, f"multipage_{i}.raw"), "wb") as fh:
            fh.write(np.ascontiguousarray(page).tobytes())
        spp = page.shape[2] if page.ndim == 3 else 1
        lines.append("\t".join(
            [str(i), str(page.shape[1]), str(page.shape[0]), str(spp), str(page.dtype)]
        ))
    with open(os.path.join(dirpath, "multipage.tsv"), "w") as fh:
        fh.write("\n".join(lines) + "\n")


def check_multipage(dirpath):
    path = os.path.join(dirpath, "multipage_ours.tif")
    failures = []
    with tifffile.TiffFile(path) as handle:
        pages = handle.pages
        expected_rows = [
            line.rstrip("\n").split("\t")
            for line in open(os.path.join(dirpath, "multipage.tsv"))
            if line.strip()
        ]
        if len(pages) != len(expected_rows):
            failures.append(f"page count {len(pages)} != {len(expected_rows)}")
        else:
            for row, page in zip(expected_rows, pages):
                index, width, height, spp, dtype = row
                expected = np.frombuffer(
                    open(os.path.join(dirpath, f"multipage_{index}.raw"), "rb").read(),
                    dtype=np.dtype(dtype),
                )
                got = np.asarray(page.asarray(), dtype=np.dtype(dtype)).reshape(-1)
                if not np.array_equal(got, expected):
                    failures.append(f"page {index} mismatch")
    print("FAIL " + "; ".join(failures) if failures else "OK")


def write_fixtures(dirpath):
    manifest = []
    for name, arr, expected, kwargs in cases():
        path = os.path.join(dirpath, name + ".tif")
        compression = kwargs.pop("compression", None)
        tifffile.imwrite(path, arr, compression=compression, **kwargs)
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
    elif mode == "genmulti":
        write_multipage(target)
    elif mode == "checkmulti":
        check_multipage(target)
    else:
        raise SystemExit("unknown mode " + mode)
"#;

/// `python3` with numpy and tifffile importable, or `None`.
pub fn find_python() -> Option<PathBuf> {
    let ok = Command::new("python3")
        .args(["-c", "import numpy, tifffile"])
        .output()
        .ok()?
        .status
        .success();
    ok.then(|| PathBuf::from("python3"))
}

/// A libtiff tool on `PATH`, or `None`.
pub fn find_tool(name: &str) -> Option<PathBuf> {
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
pub fn scratch_dir(label: &str) -> PathBuf {
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

pub fn write_driver(dir: &Path) -> PathBuf {
    let path = dir.join("driver.py");
    fs::write(&path, PY_DRIVER).expect("write driver");
    path
}

pub fn run_driver(python: &Path, driver: &Path, mode: &str, dir: &Path) -> String {
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
pub struct Fixture {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub samples_per_pixel: u16,
    pub dtype: String,
}

pub fn read_manifest(dir: &Path) -> Vec<Fixture> {
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

pub fn colour_for(dtype: &str, spp: u16) -> (u16, SampleFormat) {
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
        "uint64" => (64, SampleFormat::Uint),
        "int64" => (64, SampleFormat::Int),
        "float16" => (16, SampleFormat::IeeeFp),
        "float32" => (32, SampleFormat::IeeeFp),
        "float64" => (64, SampleFormat::IeeeFp),
        other => panic!("unhandled dtype {other} for {spp} samples"),
    }
}

pub fn decode_file(path: &Path) -> Vec<u8> {
    let bytes = fs::read(path).expect("read fixture");
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    decoder.read_image().expect("decode").to_native_bytes()
}

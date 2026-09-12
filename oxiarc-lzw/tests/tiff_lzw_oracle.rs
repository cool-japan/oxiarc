//! TIFF-LZW differential oracle tests against Pillow (libtiff-backed) and,
//! when present, libtiff's `tiffcp` CLI.
//!
//! Rationale: oxiarc-encode -> oxiarc-decode round-trips can pass while the
//! codec is a private dialect that fails 100% against real TIFF tooling
//! (exactly what happened before ClearCode support — see LZW-01). These
//! tests validate BOTH directions against an independent reference:
//!
//! 1. **decode direction**: Pillow-produced TIFF-LZW strips must decode
//!    byte-identically with [`oxiarc_lzw::decompress_tiff`].
//! 2. **encode direction**: minimal TIFF files wrapping
//!    [`oxiarc_lzw::compress_tiff`] output must be opened by Pillow with
//!    byte-identical pixel data (and be accepted by `tiffcp` when available).
//!
//! Gated behind the `tiff-oracle` feature. Each test self-skips (prints a
//! note, does not fail) if `python3` with Pillow is not available.
#![cfg(feature = "tiff-oracle")]

use oxiarc_lzw::{compress_tiff, decompress_tiff};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Python driver shared by both directions.
///
/// - `encode_ref <dir>`: for every manifest row, read `<name>.raw`, save it
///   as a single-strip TIFF-LZW via Pillow, extract the raw strip bytes to
///   `<name>.lzw`.
/// - `check_oxiarc <dir>`: for every manifest row, open `<name>_oxiarc.tif`
///   with Pillow and byte-compare the decoded pixels against `<name>.raw`.
const PY_DRIVER: &str = r#"
import os
import sys

from PIL import Image, TiffImagePlugin

# Force single-strip TIFFs so the strip bytes form one LZW stream.
TiffImagePlugin.STRIP_SIZE = 1 << 26


def read_manifest(dirpath):
    rows = []
    with open(os.path.join(dirpath, "manifest.tsv"), "r", encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            name, length, width, height = line.split("\t")
            rows.append((name, int(length), int(width), int(height)))
    return rows


def encode_ref(dirpath):
    for name, length, width, height in read_manifest(dirpath):
        with open(os.path.join(dirpath, name + ".raw"), "rb") as f:
            raw = f.read()
        assert len(raw) == length == width * height, name
        img = Image.frombytes("L", (width, height), raw)
        tif_path = os.path.join(dirpath, name + "_ref.tif")
        img.save(tif_path, format="TIFF", compression="tiff_lzw")
        # `Image.open` is lazy and keeps the file handle open; on Windows the
        # `os.remove` below fails with WinError 32 unless it is closed first,
        # so always read the tags inside a context manager.
        with Image.open(tif_path) as ref:
            offsets = ref.tag_v2[273]
            counts = ref.tag_v2[279]
        if len(offsets) != 1:
            print(f"FAIL {name} expected 1 strip, got {len(offsets)}")
            sys.exit(1)
        with open(tif_path, "rb") as f:
            f.seek(offsets[0])
            strip = f.read(counts[0])
        with open(os.path.join(dirpath, name + ".lzw"), "wb") as f:
            f.write(strip)
        os.remove(tif_path)
        print(f"OK {name}")


def check_oxiarc(dirpath):
    failures = 0
    for name, length, width, height in read_manifest(dirpath):
        with open(os.path.join(dirpath, name + ".raw"), "rb") as f:
            raw = f.read()
        tif_path = os.path.join(dirpath, name + "_oxiarc.tif")
        try:
            with Image.open(tif_path) as img:
                decoded = img.tobytes()
        except Exception as exc:  # noqa: BLE001 - report any Pillow rejection
            print(f"FAIL {name} pillow-error {exc}")
            failures += 1
            continue
        if decoded != raw:
            bad = next(
                (i for i, (a, b) in enumerate(zip(decoded, raw)) if a != b),
                min(len(decoded), len(raw)),
            )
            print(
                f"FAIL {name} pixel-mismatch len {len(decoded)} vs {len(raw)},"
                f" first diff at {bad}"
            )
            failures += 1
            continue
        print(f"OK {name}")
    if failures:
        sys.exit(1)


if __name__ == "__main__":
    mode, dirpath = sys.argv[1], sys.argv[2]
    if mode == "encode_ref":
        encode_ref(dirpath)
    elif mode == "check_oxiarc":
        check_oxiarc(dirpath)
    else:
        sys.exit(f"unknown mode {mode!r}")
"#;

/// Deterministic pseudo-random bytes (same LCG as `tests/bench_ratios.rs`).
fn lcg_bytes(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
    for _ in 0..len {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed >> 32) as u8);
    }
    data
}

/// The differential corpus. Sizes deliberately sweep the 9->10 bit-width
/// transition and EOI phantom-entry corners (1..=40, 240..=280, 500..=530),
/// the 10->11 and 11->12 transitions, and inputs large/entropic enough to
/// force table-fill ClearCode resets at entry 4094 (16 KiB+ of LCG noise).
fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut cases = Vec::new();
    for n in (1..=40).chain(240..=280).chain(500..=530) {
        cases.push((format!("lcg_{n:07}"), lcg_bytes(n)));
    }
    for n in [1024, 4096, 8192, 16384, 65536, 131072, 262144] {
        cases.push((format!("lcg_{n:07}"), lcg_bytes(n)));
    }
    cases.push(("zeros_64k".to_string(), vec![0u8; 65536]));
    cases.push(("same_byte_100k".to_string(), vec![b'X'; 100_000]));
    cases.push((
        "text_1m".to_string(),
        b"The quick brown fox jumps over the lazy dog. ".repeat(23_303)[..1_048_576].to_vec(),
    ));
    cases.push(("alt_ab_50k".to_string(), b"AB".repeat(25_000)));
    cases.push((
        "gradient_256k".to_string(),
        (0..512u32)
            .flat_map(|y| (0..512u32).map(move |x| ((x + y) % 256) as u8))
            .collect(),
    ));
    cases.push(("allbytes_256".to_string(), (0..=255).collect()));
    cases
}

/// Pick image dimensions whose product is exactly `n` (single 8-bit band).
fn dimensions(n: usize) -> (usize, usize) {
    if n > 4096 {
        for w in [4096, 2048, 1024, 512, 256, 128] {
            if n % w == 0 {
                return (w, n / w);
            }
        }
    }
    (n, 1)
}

/// Build a minimal single-strip little-endian classic TIFF wrapping `strip`
/// as LZW-compressed (Compression=5) 8-bit grayscale data.
fn minimal_tiff(strip: &[u8], width: usize, height: usize) -> Vec<u8> {
    let strip_offset: u32 = 8;
    let mut strip_padded = strip.to_vec();
    if strip_padded.len() % 2 == 1 {
        strip_padded.push(0); // keep the IFD on a word boundary
    }
    let ifd_offset = strip_offset + strip_padded.len() as u32;

    let mut out = Vec::new();
    out.extend_from_slice(b"II"); // little-endian
    out.extend_from_slice(&42u16.to_le_bytes()); // TIFF magic
    out.extend_from_slice(&ifd_offset.to_le_bytes());
    out.extend_from_slice(&strip_padded);

    // (tag, type, count, value) with type 3 = SHORT, 4 = LONG.
    let entries: [(u16, u16, u32, u32); 9] = [
        (256, 4, 1, width as u32),       // ImageWidth
        (257, 4, 1, height as u32),      // ImageLength
        (258, 3, 1, 8),                  // BitsPerSample
        (259, 3, 1, 5),                  // Compression = LZW
        (262, 3, 1, 1),                  // Photometric = BlackIsZero
        (273, 4, 1, strip_offset),       // StripOffsets
        (277, 3, 1, 1),                  // SamplesPerPixel
        (278, 4, 1, height as u32),      // RowsPerStrip
        (279, 4, 1, strip.len() as u32), // StripByteCounts
    ];
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        // SHORT values live in the low 2 bytes of the 4-byte value field.
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    out
}

/// `python3` with Pillow importable, or `None` (callers self-skip).
fn find_pillow_python() -> Option<PathBuf> {
    let ok = Command::new("python3")
        .args(["-c", "import PIL.Image"])
        .output()
        .ok()?
        .status
        .success();
    ok.then(|| PathBuf::from("python3"))
}

/// `tiffcp` on PATH, or `None`.
fn find_tiffcp() -> Option<PathBuf> {
    // Probe the bare name first and use it as-is when it spawns:
    // `which` does not exist on Windows outside a POSIX shell (the
    // oracle would silently self-skip there), and inside one — MSYS /
    // Git Bash — it prints a POSIX path such as `/mingw64/bin/...`
    // that `CreateProcess` cannot open (the oracle would then panic
    // on spawn instead of running). Letting the OS resolve the name
    // avoids both. Only spawnability is checked, not the exit status.
    if Command::new("tiffcp").arg("--version").output().is_ok() {
        return Some(PathBuf::from("tiffcp"));
    }
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(locator).arg("tiffcp").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // `where` can report several matches, one per line; take the first.
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// Unique scratch dir under [`std::env::temp_dir`].
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_tiff_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Write `manifest.tsv` + `<name>.raw` for every corpus case.
fn write_corpus(dir: &Path, cases: &[(String, Vec<u8>)]) {
    let mut manifest = String::new();
    for (name, raw) in cases {
        let (width, height) = dimensions(raw.len());
        manifest.push_str(&format!("{name}\t{}\t{width}\t{height}\n", raw.len()));
        std::fs::write(dir.join(format!("{name}.raw")), raw).expect("write raw");
    }
    std::fs::write(dir.join("manifest.tsv"), manifest).expect("write manifest");
}

fn run_driver(python: &Path, driver: &Path, mode: &str, dir: &Path) -> String {
    let output = Command::new(python)
        .arg(driver)
        .arg(mode)
        .arg(dir)
        .output()
        .expect("spawn python driver");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "python driver `{mode}` failed (exit {:?}):\nstdout:\n{stdout}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

/// Direction 1: Pillow/libtiff-compressed strips -> oxiarc decode.
#[test]
fn oracle_reference_strips_decode_byte_identical() {
    let Some(python) = find_pillow_python() else {
        eprintln!("[tiff-oracle] python3+Pillow not found; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("decode");
    let driver = dir.join("driver.py");
    std::fs::write(&driver, PY_DRIVER).expect("write driver");

    let cases = corpus();
    write_corpus(&dir, &cases);
    run_driver(&python, &driver, "encode_ref", &dir);

    let mut identical_compressed = 0usize;
    for (name, raw) in &cases {
        let strip = std::fs::read(dir.join(format!("{name}.lzw")))
            .unwrap_or_else(|e| panic!("[{name}] missing reference strip: {e}"));
        let decoded = decompress_tiff(&strip, raw.len())
            .unwrap_or_else(|e| panic!("[{name}] oxiarc failed to decode Pillow strip: {e}"));
        assert_eq!(
            &decoded, raw,
            "[{name}] Pillow strip must decode byte-identically"
        );
        if compress_tiff(raw).ok().as_deref() == Some(strip.as_slice()) {
            identical_compressed += 1;
        }
    }
    eprintln!(
        "[tiff-oracle] decode direction: {}/{} Pillow strips decoded byte-identical \
         ({identical_compressed} also byte-identical to oxiarc's own encoding)",
        cases.len(),
        cases.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Direction 2: oxiarc-compressed TIFFs -> Pillow/libtiff decode.
#[test]
fn oracle_oxiarc_tiffs_accepted_by_pillow() {
    let Some(python) = find_pillow_python() else {
        eprintln!("[tiff-oracle] python3+Pillow not found; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("encode");
    let driver = dir.join("driver.py");
    std::fs::write(&driver, PY_DRIVER).expect("write driver");

    let cases = corpus();
    write_corpus(&dir, &cases);
    for (name, raw) in &cases {
        let strip = compress_tiff(raw).unwrap_or_else(|e| panic!("[{name}] encode failed: {e}"));
        let (width, height) = dimensions(raw.len());
        let tiff = minimal_tiff(&strip, width, height);
        std::fs::write(dir.join(format!("{name}_oxiarc.tif")), tiff).expect("write tiff");
    }

    let stdout = run_driver(&python, &driver, "check_oxiarc", &dir);
    let ok = stdout.lines().filter(|l| l.starts_with("OK ")).count();
    assert_eq!(
        ok,
        cases.len(),
        "Pillow must accept and correctly decode every oxiarc TIFF:\n{stdout}"
    );
    eprintln!(
        "[tiff-oracle] encode direction: {ok}/{} oxiarc TIFF-LZW files decoded correctly by Pillow",
        cases.len()
    );

    // Secondary oracle: libtiff's own CLI must also accept our LZW streams.
    if let Some(tiffcp) = find_tiffcp() {
        let mut accepted = 0usize;
        for (name, _) in &cases {
            let src = dir.join(format!("{name}_oxiarc.tif"));
            let dst = dir.join(format!("{name}_tiffcp.tif"));
            let output = Command::new(&tiffcp)
                .arg("-c")
                .arg("none")
                .arg(&src)
                .arg(&dst)
                .output()
                .expect("spawn tiffcp");
            assert!(
                output.status.success(),
                "[{name}] tiffcp rejected oxiarc TIFF-LZW: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            accepted += 1;
        }
        eprintln!(
            "[tiff-oracle] encode direction: {accepted}/{} oxiarc TIFF-LZW files accepted by tiffcp",
            cases.len()
        );
    } else {
        eprintln!("[tiff-oracle] tiffcp not found; skipped the libtiff CLI cross-check");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

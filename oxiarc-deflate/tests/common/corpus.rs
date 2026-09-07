//! Shared corpus generation for the DEFLATE encoder ratio/throughput gates.
//!
//! Included by `tests/zlib_ratio_oracle.rs` and `examples/zlib_ab.rs` via
//! `#[path = ...] mod corpus;`, so it is deliberately **not** part of the
//! crate's public API and lives in a `tests/` subdirectory (cargo only turns
//! top-level `tests/*.rs` files into test binaries).
//!
//! Every corpus is either generated deterministically in Rust or produced by
//! `python3` (+ Pillow for the PNG-filtered rows). Python-backed corpora
//! self-skip when the tool is missing; the Rust-generated ones always exist,
//! so the gates never vacuously pass on a bare machine.
#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

/// One named corpus buffer.
pub struct Sample {
    /// Human-readable name printed in tables and assertion messages.
    pub name: &'static str,
    /// The bytes to compress.
    pub data: Vec<u8>,
}

/// Deterministic xorshift64* PRNG (no external dependency, reproducible).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    pub fn below(&mut self, n: u32) -> u32 {
        if n == 0 { 0 } else { self.next_u32() % n }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(n);
        out
    }
}

/// The workspace root (`CARGO_MANIFEST_DIR/..`).
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Concatenated Rust source from this workspace, capped at `cap` bytes.
///
/// Real source code: the "text" member of a Canterbury/Silesia-style set.
/// Always available (the repository is what we are building).
pub fn rust_sources(cap: usize) -> Vec<u8> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rs(&workspace_root().join("oxiarc-core"), &mut files, 400);
    collect_rs(&workspace_root().join("oxiarc-deflate"), &mut files, 400);
    files.sort();
    let mut out = Vec::with_capacity(cap.min(1 << 22));
    for f in files {
        if out.len() >= cap {
            break;
        }
        if let Ok(bytes) = std::fs::read(&f) {
            out.extend_from_slice(&bytes);
        }
    }
    out.truncate(cap);
    out
}

fn collect_rs(dir: &std::path::Path, out: &mut Vec<PathBuf>, budget: usize) {
    if out.len() >= budget {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut sorted: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    sorted.sort();
    for path in sorted {
        if out.len() >= budget {
            return;
        }
        if path.is_dir() {
            collect_rs(&path, out, budget);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Synthetic HTML with realistic tag/attribute repetition.
pub fn html_like(target: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x48544d4c);
    let words = [
        "compression",
        "deflate",
        "archive",
        "stream",
        "encoder",
        "window",
        "huffman",
        "symbol",
        "distance",
        "literal",
        "match",
        "block",
        "table",
        "buffer",
        "length",
        "offset",
        "header",
        "record",
        "index",
        "chunk",
    ];
    let mut out = Vec::with_capacity(target + 1024);
    out.extend_from_slice(b"<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>Corpus</title>\n</head>\n<body>\n");
    let mut section = 0usize;
    while out.len() < target {
        section += 1;
        out.extend_from_slice(
            format!("<div class=\"section section-{section}\" id=\"s{section}\">\n").as_bytes(),
        );
        out.extend_from_slice(format!("  <h2>Section {section}</h2>\n").as_bytes());
        for _ in 0..6 {
            out.extend_from_slice(b"  <p class=\"body-text\">");
            for _ in 0..(12 + rng.below(24)) {
                let w = words[rng.below(words.len() as u32) as usize];
                out.push(b' ');
                out.extend_from_slice(w.as_bytes());
            }
            out.extend_from_slice(b".</p>\n");
        }
        out.extend_from_slice(b"  <table>\n");
        for row in 0..8 {
            out.extend_from_slice(
                format!(
                    "    <tr><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                    row,
                    rng.below(100000),
                    words[rng.below(words.len() as u32) as usize]
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(b"  </table>\n</div>\n");
    }
    out.extend_from_slice(b"</body>\n</html>\n");
    out.truncate(target);
    out
}

/// Structured binary records: a mix of counters, floats and padding.
///
/// This is the "binary"/`x-ray`-style member: highly structured, poorly served
/// by literal-only coding, and a good exercise of long-distance matches.
pub fn binary_records(target: usize) -> Vec<u8> {
    let mut rng = Rng::new(0xB1_1A_2C_0D_E5_u64);
    let mut out = Vec::with_capacity(target + 64);
    let mut counter: u32 = 0;
    while out.len() < target {
        counter = counter.wrapping_add(1);
        out.extend_from_slice(&counter.to_le_bytes());
        out.extend_from_slice(&(counter as u64 * 1000).to_le_bytes());
        let f = (counter as f64) * 0.015_625;
        out.extend_from_slice(&f.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
        out.push((rng.below(4) * 17) as u8);
        out.extend_from_slice(b"REC\0");
    }
    out.truncate(target);
    out
}

/// A long stretch of one byte value plus occasional breaks: exercises the
/// run-length path and `nice_length` early exit.
pub fn runs(target: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x52554e53);
    let mut out = Vec::with_capacity(target);
    while out.len() < target {
        let b = (rng.below(6) * 40) as u8;
        let n = (100 + rng.below(5000)) as usize;
        out.extend(std::iter::repeat_n(b, n.min(target - out.len())));
        if out.len() < target {
            out.push(rng.next_u32() as u8);
        }
    }
    out.truncate(target);
    out
}

/// Pseudo-random bytes: incompressible, must fall back to stored blocks.
pub fn random(target: usize) -> Vec<u8> {
    Rng::new(0x5A5A_1234_5678).bytes(target)
}

/// Log-file-like text: highly repetitive prefixes, varying tails.
pub fn log_lines(target: usize) -> Vec<u8> {
    let mut rng = Rng::new(0x10_6C_1E_5A_u64);
    let levels = ["INFO", "WARN", "ERROR", "DEBUG", "TRACE"];
    let modules = [
        "oxiarc::deflate",
        "oxiarc::inflate",
        "oxiarc::zip",
        "oxiarc::tar",
        "oxiarc::lzma",
    ];
    let mut out = Vec::with_capacity(target + 256);
    let mut secs = 1_600_000_000u64;
    while out.len() < target {
        secs += 1 + u64::from(rng.below(3));
        out.extend_from_slice(
            format!(
                "2020-09-13T{:02}:{:02}:{:02}.{:03}Z {:>5} [{}] request id={} bytes={} status={}\n",
                (secs / 3600) % 24,
                (secs / 60) % 60,
                secs % 60,
                rng.below(1000),
                levels[rng.below(levels.len() as u32) as usize],
                modules[rng.below(modules.len() as u32) as usize],
                rng.below(1_000_000),
                rng.below(65536),
                200 + rng.below(5) * 100,
            )
            .as_bytes(),
        );
    }
    out.truncate(target);
    out
}

/// Whether `python3` with `zlib` is usable as a reference.
pub fn python3_available() -> bool {
    Command::new("python3")
        .args(["-c", "import zlib"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Whether `python3` has Pillow + numpy (for the PNG-filtered corpora).
pub fn pillow_available() -> bool {
    Command::new("python3")
        .args(["-c", "import PIL, numpy"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Unique temp path for fixture generation.
pub fn temp_path(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxiarc_deflate_corpus_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ))
}

/// Reference: `zlib.compress(data, level)` run by CPython.
///
/// Returns `None` when `python3` is unavailable.
pub fn python_zlib_compress(data: &[u8], level: u8) -> Option<Vec<u8>> {
    if !python3_available() {
        return None;
    }
    let inp = temp_path("in");
    let outp = temp_path("out");
    std::fs::write(&inp, data).ok()?;
    let script = "import sys, zlib\n\
         data = open(sys.argv[1],'rb').read()\n\
         open(sys.argv[2],'wb').write(zlib.compress(data, int(sys.argv[3])))\n";
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&inp)
        .arg(&outp)
        .arg(level.to_string())
        .output()
        .ok()?;
    let result = if status.status.success() {
        std::fs::read(&outp).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    result
}

/// Reference: raw DEFLATE (`wbits=-15`) produced by CPython's zlib.
pub fn python_raw_deflate(data: &[u8], level: u8) -> Option<Vec<u8>> {
    if !python3_available() {
        return None;
    }
    let inp = temp_path("rin");
    let outp = temp_path("rout");
    std::fs::write(&inp, data).ok()?;
    let script = "import sys, zlib\n\
         data = open(sys.argv[1],'rb').read()\n\
         c = zlib.compressobj(int(sys.argv[3]), zlib.DEFLATED, -15)\n\
         open(sys.argv[2],'wb').write(c.compress(data) + c.flush())\n";
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&inp)
        .arg(&outp)
        .arg(level.to_string())
        .output()
        .ok()?;
    let result = if status.status.success() {
        std::fs::read(&outp).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    result
}

/// Reference: raw DEFLATE produced by CPython's zlib under an explicit
/// strategy (`Z_FILTERED` 1, `Z_HUFFMAN_ONLY` 2, `Z_RLE` 3, `Z_FIXED` 4).
pub fn python_raw_deflate_strategy(data: &[u8], level: u8, strategy: u8) -> Option<Vec<u8>> {
    if !python3_available() {
        return None;
    }
    let inp = temp_path("sin");
    let outp = temp_path("sout");
    std::fs::write(&inp, data).ok()?;
    let script = "import sys, zlib\n\
         data = open(sys.argv[1],'rb').read()\n\
         c = zlib.compressobj(int(sys.argv[3]), zlib.DEFLATED, -15, 8, int(sys.argv[4]))\n\
         open(sys.argv[2],'wb').write(c.compress(data) + c.flush())\n";
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&inp)
        .arg(&outp)
        .arg(level.to_string())
        .arg(strategy.to_string())
        .output()
        .ok()?;
    let result = if status.status.success() {
        std::fs::read(&outp).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&outp);
    result
}

/// Corpora that discriminate the *strategy* code paths, which the ratio
/// corpora do not exercise: near-random bytes with periodic anchors (where the
/// dynamic code beats a stored block but the static one does not), bytes drawn
/// only from the 9-bit half of the fixed literal alphabet (same shape, larger
/// gap), a long run-length shape for `Z_RLE`, and predictor-like deltas for
/// `Z_FILTERED`.
pub fn strategy_samples() -> Vec<Sample> {
    let mut rng = Rng::new(0x5150_1234_abcd_ef01);
    let mut anchored = Vec::new();
    for k in 0..200u32 {
        anchored.extend_from_slice(&rng.bytes(300));
        anchored.extend_from_slice(format!("ANCHOR{}", k % 7).as_bytes());
    }

    let mut nine_bit = Vec::with_capacity(40_000);
    while nine_bit.len() < 40_000 {
        nine_bit.push(144u8.wrapping_add((rng.next_u32() % 112) as u8));
    }

    let mut runs_then_text = Vec::new();
    for k in 0..400u32 {
        runs_then_text.extend(std::iter::repeat_n(b'a' + (k % 26) as u8, 97));
        runs_then_text.extend_from_slice(b"the quick brown fox ");
    }

    let mut deltas = Vec::with_capacity(64 * 1024);
    let mut acc = 0u8;
    while deltas.len() < 64 * 1024 {
        acc = acc.wrapping_add((rng.next_u32() % 5) as u8).wrapping_sub(2);
        deltas.push(acc);
    }

    vec![
        Sample {
            name: "anchored-random",
            data: anchored,
        },
        Sample {
            name: "nine-bit-alphabet",
            data: nine_bit,
        },
        Sample {
            name: "runs-then-text",
            data: runs_then_text,
        },
        Sample {
            name: "predictor-deltas",
            data: deltas,
        },
    ]
}

/// PNG-filtered scanline bytes for the four PNG2-style fixtures.
///
/// Runs Pillow to render each image, then applies the PNG adaptive filter
/// (minimum-sum-of-absolute-differences) exactly like a PNG encoder would, so
/// the bytes handed to DEFLATE are the real thing. Returns an empty vector
/// when Pillow is unavailable.
pub fn png_filtered_rows() -> Vec<Sample> {
    if !pillow_available() {
        return Vec::new();
    }
    let out_dir = temp_path("png");
    let _ = std::fs::create_dir_all(&out_dir);
    let script = r#"
import sys, os, math, random
import numpy as np
from PIL import Image, ImageDraw

out = sys.argv[1]
random.seed(20260907)
rng = np.random.default_rng(20260907)

def filt(arr, bpp):
    # arr: HxW*C uint8 rows already flattened per row
    h, w = arr.shape
    prev = np.zeros(w, dtype=np.int16)
    out_rows = []
    for y in range(h):
        cur = arr[y].astype(np.int16)
        cands = []
        # 0 None
        cands.append((0, cur.copy()))
        # 1 Sub
        a = np.zeros(w, dtype=np.int16); a[bpp:] = cur[:-bpp]
        cands.append((1, (cur - a) & 0xFF))
        # 2 Up
        cands.append((2, (cur - prev) & 0xFF))
        # 3 Average
        cands.append((3, (cur - ((a + prev) // 2)) & 0xFF))
        # 4 Paeth
        c = np.zeros(w, dtype=np.int16); c[bpp:] = prev[:-bpp]
        p = a + prev - c
        pa = np.abs(p - a); pb = np.abs(p - prev); pc = np.abs(p - c)
        pred = np.where((pa <= pb) & (pa <= pc), a, np.where(pb <= pc, prev, c))
        cands.append((4, (cur - pred) & 0xFF))
        best = None; best_score = None
        for ft, row in cands:
            r = row.astype(np.uint8).astype(np.int16)
            score = int(np.sum(np.where(r < 128, r, 256 - r)))
            if best_score is None or score < best_score:
                best_score = score; best = (ft, row)
        ft, row = best
        out_rows.append(bytes([ft]) + row.astype(np.uint8).tobytes())
        prev = cur
    return b"".join(out_rows)

def save(name, arr, bpp):
    data = filt(arr, bpp)
    open(os.path.join(out, name), "wb").write(data)

# 1. gradient + noise, RGB8 256x192 (the PNG2 fixture)
W, H = 256, 192
img = np.zeros((H, W, 3), dtype=np.uint8)
for y in range(H):
    for x in range(W):
        img[y, x] = ((x * 255) // W, (y * 255) // H, ((x + y) * 255) // (W + H))
noise = rng.integers(-12, 13, size=(H, W, 3))
img = np.clip(img.astype(np.int16) + noise, 0, 255).astype(np.uint8)
save("gradient_noise_rgb8.bin", img.reshape(H, W * 3), 3)

# 2. photo-like: smooth blobs + film grain, RGB8 320x240
W, H = 320, 240
yy, xx = np.mgrid[0:H, 0:W]
r = (128 + 100 * np.sin(xx / 37.0) * np.cos(yy / 51.0))
g = (128 + 90 * np.sin((xx + yy) / 29.0))
b = (128 + 80 * np.cos(xx / 61.0 + yy / 23.0))
photo = np.stack([r, g, b], axis=2)
photo = np.clip(photo + rng.normal(0, 6, photo.shape), 0, 255).astype(np.uint8)
save("photo_like_rgb8.bin", photo.reshape(H, W * 3), 3)

# 3. flat colour with a few shapes, RGBA8 256x256
im = Image.new("RGBA", (256, 256), (32, 96, 160, 255))
d = ImageDraw.Draw(im)
d.rectangle([40, 40, 200, 120], fill=(220, 220, 60, 255))
d.ellipse([80, 130, 230, 240], fill=(10, 10, 10, 255))
flat = np.array(im, dtype=np.uint8)
save("flat_rgba8.bin", flat.reshape(256, 256 * 4), 4)

# 4. text-like render: high-contrast glyph shapes, grayscale 512x256
im = Image.new("L", (512, 256), 255)
d = ImageDraw.Draw(im)
words = ["deflate", "huffman", "window", "match", "literal", "distance", "block"]
for i in range(24):
    d.text((6 + (i % 4) * 120, 8 + (i // 4) * 40), words[i % len(words)] * 2, fill=0)
txt = np.array(im, dtype=np.uint8)
save("text_like_gray8.bin", txt, 1)
"#;
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&out_dir)
        .output();
    let ok = status.map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        return Vec::new();
    }
    let names: [(&'static str, &'static str); 4] = [
        ("png-gradient+noise-rgb8", "gradient_noise_rgb8.bin"),
        ("png-photo-like-rgb8", "photo_like_rgb8.bin"),
        ("png-flat-rgba8", "flat_rgba8.bin"),
        ("png-text-like-gray8", "text_like_gray8.bin"),
    ];
    let mut out = Vec::new();
    for (name, file) in names {
        if let Ok(bytes) = std::fs::read(out_dir.join(file)) {
            out.push(Sample { name, data: bytes });
        }
    }
    let _ = std::fs::remove_dir_all(&out_dir);
    out
}

/// The always-available (Rust-generated) part of the corpus.
pub fn base_samples() -> Vec<Sample> {
    vec![
        Sample {
            name: "rust-source",
            data: rust_sources(512 * 1024),
        },
        Sample {
            name: "html",
            data: html_like(256 * 1024),
        },
        Sample {
            name: "log-lines",
            data: log_lines(256 * 1024),
        },
        Sample {
            name: "binary-records",
            data: binary_records(256 * 1024),
        },
        Sample {
            name: "runs",
            data: runs(128 * 1024),
        },
        Sample {
            name: "random",
            data: random(64 * 1024),
        },
    ]
}

/// The full corpus: Rust-generated plus (when Pillow is present) the four
/// PNG-filtered scanline fixtures.
pub fn all_samples() -> Vec<Sample> {
    let mut v = base_samples();
    v.extend(png_filtered_rows());
    v
}

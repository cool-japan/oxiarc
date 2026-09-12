//! GIF LZW differential tests against Pillow — feature `gif-oracle`.
//!
//! [`oxiarc_lzw::gif_decompress`] was rewritten in 0.4.2 to share the TIFF
//! strip decoder's loop (it used to clone a `Vec<u8>` per emitted code), so
//! it needs what the TIFF path already had: a check against a real,
//! independent implementation in **both** directions.
//!
//! * decode — Pillow writes a GIF, this suite pulls the raw LZW sub-blocks
//!   out of the container and decodes them here; the bytes must equal the
//!   pixels Pillow itself reads back;
//! * encode — [`oxiarc_lzw::gif_compress`] output is wrapped in a minimal
//!   GIF89a container and Pillow must read the original image back.
//!
//! Pillow's GIF plugin is its own LZW implementation (not libtiff's), and
//! it is what the rest of the world decodes GIFs with, so agreement with it
//! is the meaningful check. Every image covers a different code-table
//! shape: flat regions (long runs), gradients, text-like repetition, noise
//! (one code per pixel, and a table that fills and resets) and the
//! degenerate 1x1 case.
//!
//! `gif_decompress` returns the image data **in stored order**, which for
//! an interlaced GIF is not row order — the four-pass interlace is a
//! container-level concern, exactly as the predictor is for TIFF. Pillow
//! writes interlaced files by default, so this suite covers both: it reads
//! the interlace flag out of the image descriptor and de-interlaces before
//! comparing, and half the fixtures are written non-interlaced so the
//! straight path is exercised too. (An interlaced fixture that were
//! compared without de-interlacing would fail with the rows in the order
//! 0, 8, 16, 4, 12, 2, 6, ... — which is how this was found.)
//!
//! Self-skips (prints a note, does not fail) when `python3` or Pillow is
//! missing.
#![cfg(feature = "gif-oracle")]

use oxiarc_lzw::{gif_compress, gif_decompress};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Writes one 8-bit palette GIF per shape with Pillow, plus the pixels
/// Pillow reads back from it, so the test can compare both directions.
const PY_WRITE: &str = r#"
import io
import sys

import numpy as np
from PIL import Image

out = sys.argv[1]


def shapes(side):
    yield "flat", np.full((side, side), 7, dtype=np.uint8)
    y, x = np.mgrid[0:side, 0:side]
    yield "gradient", ((x + y) % 256).astype(np.uint8)
    plates = np.zeros((side, side), dtype=np.uint8)
    plates[: side // 2, : side // 2] = 200
    plates[side // 2 :, side // 2 :] = 31
    yield "plates", plates
    rng = np.random.default_rng(20260908)
    yield "noise", rng.integers(0, 256, size=(side, side), dtype=np.uint8)
    words = np.frombuffer(b"the quick brown fox jumps over the lazy dog ", dtype=np.uint8)
    tiled = np.resize(words, side * side).reshape(side, side)
    yield "textlike", tiled.astype(np.uint8)


palette = bytes(range(256)) * 3
names = []
for side in (1, 17, 64, 200):
    interlace = side % 2 == 1
    for name, array in shapes(side):
        image = Image.fromarray(array, mode="P")
        image.putpalette(palette)
        buffer = io.BytesIO()
        image.save(buffer, format="GIF", interlace=interlace)
        data = buffer.getvalue()
        label = f"{name}_{side}"
        with open(f"{out}/{label}.gif", "wb") as handle:
            handle.write(data)
        # What Pillow itself decodes that file to.
        back = Image.open(io.BytesIO(data))
        back.load()
        with open(f"{out}/{label}.raw", "wb") as handle:
            handle.write(np.array(back, dtype=np.uint8).tobytes())
        names.append(f"{label}\t{side}")
print("\n".join(names))
"#;

/// Reads a GIF this crate wrote and prints its pixels, so the encode
/// direction is checked by Pillow rather than by another oxiarc decode.
const PY_READ: &str = r#"
import sys

import numpy as np
from PIL import Image

image = Image.open(sys.argv[1])
image.load()
sys.stdout.buffer.write(np.array(image, dtype=np.uint8).tobytes())
"#;

fn python_available() -> bool {
    Command::new("python3")
        .args(["-c", "import numpy, PIL"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_lzw_gif_oracle_{tag}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the oracle temp directory");
    dir
}

/// The first image of a GIF87a/89a file: its LZW minimum code size, its
/// concatenated image data sub-blocks, and whether it is interlaced.
fn gif_image_data(data: &[u8]) -> Option<(u8, Vec<u8>, bool)> {
    if data.get(..3)? != b"GIF" {
        return None;
    }
    let flags = *data.get(10)?;
    let mut at = 13usize;
    if flags & 0x80 != 0 {
        at += 3 << ((flags & 0x07) + 1);
    }
    loop {
        match *data.get(at)? {
            // Extension block: skip its label and its sub-blocks.
            0x21 => {
                at += 2;
                loop {
                    let len = usize::from(*data.get(at)?);
                    at += 1 + len;
                    if len == 0 {
                        break;
                    }
                }
            }
            // Image descriptor.
            0x2C => {
                let local = *data.get(at + 9)?;
                at += 10;
                if local & 0x80 != 0 {
                    at += 3 << ((local & 0x07) + 1);
                }
                let minimum_code_size = *data.get(at)?;
                at += 1;
                let mut blocks = Vec::new();
                loop {
                    let len = usize::from(*data.get(at)?);
                    at += 1;
                    if len == 0 {
                        break;
                    }
                    blocks.extend_from_slice(data.get(at..at + len)?);
                    at += len;
                }
                return Some((minimum_code_size, blocks, local & 0x40 != 0));
            }
            _ => return None,
        }
    }
}

/// A minimal GIF89a around one image, so Pillow can decode what
/// `gif_compress` produced. `minimum_code_size` (2..=8) also sizes the
/// global colour table, which is `1 << minimum_code_size` grey entries.
fn wrap_gif(width: u16, height: u16, minimum_code_size: u8, stream: &[u8]) -> Vec<u8> {
    let entries = 1usize << minimum_code_size;
    let mut out = Vec::with_capacity(stream.len() + entries * 3 + 64);
    out.extend_from_slice(b"GIF89a");
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    // Global colour table present, 8 bits per channel, `1 << (n + 1)`
    // entries where n is the low three bits.
    out.push(0x80 | 0x70 | (minimum_code_size - 1));
    out.push(0); // background colour index
    out.push(0); // pixel aspect ratio
    let step = 255 / (entries.max(2) - 1);
    for index in 0..entries {
        let grey = (index * step) as u8;
        out.extend_from_slice(&[grey, grey, grey]);
    }
    out.push(0x2C); // image descriptor
    out.extend_from_slice(&0u16.to_le_bytes()); // left
    out.extend_from_slice(&0u16.to_le_bytes()); // top
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push(0); // no local colour table, not interlaced
    out.push(minimum_code_size);
    for chunk in stream.chunks(255) {
        out.push(chunk.len() as u8);
        out.extend_from_slice(chunk);
    }
    out.push(0); // block terminator
    out.push(0x3B); // trailer
    out
}

/// Undo GIF's four-pass row interlace: stored rows come in the order
/// 0, 8, 16, ... then 4, 12, ... then 2, 6, ... then 1, 3, 5, ...
fn deinterlace(pixels: &[u8], width: usize, height: usize) -> Vec<u8> {
    let mut out = vec![0u8; pixels.len()];
    let mut stored = 0usize;
    for (start, step) in [(0usize, 8usize), (4, 8), (2, 4), (1, 2)] {
        let mut row = start;
        while row < height {
            let from = stored * width;
            let to = row * width;
            if let (Some(source), Some(target)) =
                (pixels.get(from..from + width), out.get_mut(to..to + width))
            {
                target.copy_from_slice(source);
            }
            stored += 1;
            row += step;
        }
    }
    out
}

fn run_python(script: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("python3")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn write_fixtures(dir: &Path) -> Vec<(String, usize)> {
    let listing = run_python(PY_WRITE, &[&dir.display().to_string()])
        .expect("Pillow could not write the GIF fixtures");
    String::from_utf8_lossy(&listing)
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let name = parts.next()?.to_string();
            let side: usize = parts.next()?.trim().parse().ok()?;
            Some((name, side))
        })
        .collect()
}

#[test]
fn oracle_pillow_gifs_decode_byte_identically() {
    if !python_available() {
        println!("python3 with numpy + Pillow is unavailable: skipping");
        return;
    }
    let dir = temp_dir("decode");
    let fixtures = write_fixtures(&dir);
    assert!(!fixtures.is_empty(), "no fixtures were produced");

    let mut checked = 0usize;
    let mut widths = Vec::new();
    let mut interlaced_seen = false;
    let mut straight_seen = false;
    for (name, side) in &fixtures {
        let gif = std::fs::read(dir.join(format!("{name}.gif"))).expect("read the GIF");
        let expected = std::fs::read(dir.join(format!("{name}.raw"))).expect("read the pixels");
        assert_eq!(
            expected.len(),
            side * side,
            "{name}: unexpected pixel count"
        );
        let (minimum_code_size, stream, interlaced) =
            gif_image_data(&gif).unwrap_or_else(|| panic!("{name}: cannot find the image data"));
        widths.push(minimum_code_size);
        interlaced_seen |= interlaced;
        straight_seen |= !interlaced;
        let stored = gif_decompress(&stream, minimum_code_size)
            .unwrap_or_else(|error| panic!("{name}: gif_decompress failed: {error}"));
        assert_eq!(stored.len(), side * side, "{name}: wrong number of pixels");
        let decoded = if interlaced {
            deinterlace(&stored, *side, *side)
        } else {
            stored
        };
        assert_eq!(
            decoded,
            expected,
            "{name}: our decode differs from Pillow's ({} vs {} bytes)",
            decoded.len(),
            expected.len()
        );
        checked += 1;
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("{checked} Pillow GIFs decoded byte-identically (minimum code sizes {widths:?})");
    assert!(
        checked >= 20,
        "expected the full fixture matrix, got {checked}"
    );
    assert!(interlaced_seen, "no interlaced fixture was produced");
    assert!(straight_seen, "no non-interlaced fixture was produced");
}

/// Pillow always writes `minimum_code_size == 8`, so the narrow initial
/// widths (2..=7 — the ones that need a code table whose `min_bits` is
/// below the 9 the public `LzwConfig` policy allows) can only be checked
/// against a reference in the encode direction. This does that: one GIF per
/// width, read back by Pillow.
#[test]
fn oracle_pillow_reads_back_every_minimum_code_size() {
    if !python_available() {
        println!("python3 with numpy + Pillow is unavailable: skipping");
        return;
    }
    let dir = temp_dir("widths");
    let side = 48usize;
    let mut checked = 0usize;
    for minimum_code_size in 2u8..=8 {
        let colours = 1u16 << minimum_code_size;
        // A payload that uses the whole palette, has flat runs and a
        // pseudo-random tail, so the table fills at the narrow widths too.
        let mut pixels = Vec::with_capacity(side * side);
        let mut seed = 0x1234_5678u32;
        for index in 0..side * side {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let value = if index % 97 < 40 {
                (index / 97) as u16 % colours
            } else {
                (seed >> 16) as u16 % colours
            };
            pixels.push(value as u8);
        }
        let stream = gif_compress(&pixels, minimum_code_size)
            .unwrap_or_else(|error| panic!("mcs {minimum_code_size}: {error}"));
        assert_eq!(
            gif_decompress(&stream, minimum_code_size)
                .unwrap_or_else(|error| panic!("mcs {minimum_code_size}: {error}")),
            pixels,
            "mcs {minimum_code_size}: our own round trip differs"
        );
        let path = dir.join(format!("mcs{minimum_code_size}.gif"));
        let side_u16 = u16::try_from(side).expect("side fits in u16");
        std::fs::write(
            &path,
            wrap_gif(side_u16, side_u16, minimum_code_size, &stream),
        )
        .expect("write our GIF");
        let read_back = run_python(PY_READ, &[&path.display().to_string()])
            .unwrap_or_else(|| panic!("mcs {minimum_code_size}: Pillow could not read our GIF"));
        assert_eq!(
            read_back, pixels,
            "mcs {minimum_code_size}: Pillow read back different pixels"
        );
        checked += 1;
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("{checked} minimum code sizes (2..=8) read back byte-identically by Pillow");
    assert_eq!(checked, 7);
}

#[test]
fn oracle_pillow_reads_back_what_gif_compress_writes() {
    if !python_available() {
        println!("python3 with numpy + Pillow is unavailable: skipping");
        return;
    }
    let dir = temp_dir("encode");
    let fixtures = write_fixtures(&dir);
    assert!(!fixtures.is_empty(), "no fixtures were produced");

    let mut checked = 0usize;
    for (name, side) in &fixtures {
        let pixels = std::fs::read(dir.join(format!("{name}.raw"))).expect("read the pixels");
        let stream = gif_compress(&pixels, 8).unwrap_or_else(|e| panic!("{name}: {e}"));
        // Self-consistency first, so a failure below is Pillow's reading and
        // not our own round trip.
        assert_eq!(
            gif_decompress(&stream, 8).unwrap_or_else(|e| panic!("{name}: {e}")),
            pixels,
            "{name}: our own round trip differs"
        );
        let side_u16 = u16::try_from(*side).expect("fixture side fits in u16");
        let path = dir.join(format!("{name}_oxiarc.gif"));
        std::fs::write(&path, wrap_gif(side_u16, side_u16, 8, &stream)).expect("write our GIF");
        let read_back = run_python(PY_READ, &[&path.display().to_string()])
            .unwrap_or_else(|| panic!("{name}: Pillow could not read our GIF"));
        assert_eq!(
            read_back, pixels,
            "{name}: Pillow read back different pixels from our GIF"
        );
        checked += 1;
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!("{checked} oxiarc-written GIFs read back byte-identically by Pillow");
    assert!(
        checked >= 20,
        "expected the full fixture matrix, got {checked}"
    );
}

//! libtiff `tiffcp -c lzw` fixture strips decoded through
//! [`oxiarc_lzw::decompress_tiff_into`] — feature `tiff-oracle`.
//!
//! Unlike `tiff_lzw_oracle.rs` (which drives Pillow and always produces a
//! *single* strip), this suite exercises the exact shape the `oxiarc-tiff`
//! reader will meet: a real libtiff file with **many strips**, each of which
//! is an independent LZW stream that must be decoded straight into the
//! caller's row buffer. Strips are extracted with a dependency-free
//! `struct`-based TIFF/IFD parser in `python3` (tifffile is not needed and
//! cannot read LZW without `imagecodecs` on this machine).
//!
//! Covered:
//!
//! * `tiffcp -c lzw` at several `-r` (rows-per-strip) settings, so strip
//!   boundaries fall on and off code boundaries;
//! * `tiffcp -c lzw:2` (horizontal predictor), whose strips decode to
//!   differenced rows — the predictor is undone here to prove the *codec*
//!   output is byte-exact;
//! * every strip decoded into an exactly-sized buffer with no intermediate
//!   allocation, cross-checked against `decompress_tiff`.
//!
//! Self-skips (prints a note, does not fail) when `tiffcp` or `python3` is
//! not available.
#![cfg(feature = "tiff-oracle")]

use oxiarc_lzw::{decompress_tiff, decompress_tiff_into};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Dependency-free strip extractor: prints one `offset<TAB>count` line per
/// strip, preceded by `width<TAB>height<TAB>rows_per_strip<TAB>predictor`.
const PY_EXTRACT: &str = r#"
import struct
import sys

TYPE_SIZES = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8}


def read_ifd(data):
    endian = data[:2]
    if endian == b"II":
        prefix = "<"
    elif endian == b"MM":
        prefix = ">"
    else:
        sys.exit("not a TIFF")
    magic, ifd_off = struct.unpack(prefix + "HI", data[2:8])
    if magic != 42:
        sys.exit("not a classic TIFF")
    (count,) = struct.unpack(prefix + "H", data[ifd_off : ifd_off + 2])
    tags = {}
    for i in range(count):
        base = ifd_off + 2 + i * 12
        tag, typ, n = struct.unpack(prefix + "HHI", data[base : base + 8])
        size = TYPE_SIZES.get(typ, 0) * n
        if size <= 4:
            payload = data[base + 8 : base + 8 + size]
        else:
            (off,) = struct.unpack(prefix + "I", data[base + 8 : base + 12])
            payload = data[off : off + size]
        fmt = {1: "B", 3: "H", 4: "I"}.get(typ)
        if fmt is None:
            tags[tag] = []
            continue
        tags[tag] = list(struct.unpack(prefix + fmt * n, payload))
    return tags


def main(path):
    with open(path, "rb") as f:
        data = f.read()
    tags = read_ifd(data)
    width = tags[256][0]
    height = tags[257][0]
    rows = tags.get(278, [height])[0]
    predictor = tags.get(317, [1])[0]
    compression = tags[259][0]
    if compression != 5:
        sys.exit(f"expected Compression=5 (LZW), got {compression}")
    if tags.get(258, [8])[0] != 8 or tags.get(277, [1])[0] != 1:
        sys.exit("expected 8-bit single-band data")
    print(f"{width}\t{height}\t{rows}\t{predictor}")
    for off, cnt in zip(tags[273], tags[279]):
        print(f"{off}\t{cnt}")


if __name__ == "__main__":
    main(sys.argv[1])
"#;

/// Deterministic image-like payload (gradients plus noise), which makes
/// libtiff build a deep code table per strip.
fn image_bytes(width: usize, height: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(width * height);
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    for y in 0..height {
        for x in 0..width {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let noise = ((seed >> 59) as usize) & 0x07;
            data.push((((x + y * 3) / 2 + noise) % 256) as u8);
        }
    }
    data
}

/// Minimal uncompressed little-endian classic TIFF (8-bit grayscale, one
/// strip) used as the input to `tiffcp`.
fn uncompressed_tiff(raw: &[u8], width: usize, height: usize) -> Vec<u8> {
    let strip_offset: u32 = 8;
    let mut body = raw.to_vec();
    if body.len() % 2 == 1 {
        body.push(0);
    }
    let ifd_offset = strip_offset + body.len() as u32;

    let mut out = Vec::new();
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_offset.to_le_bytes());
    out.extend_from_slice(&body);

    let entries: [(u16, u16, u32, u32); 9] = [
        (256, 4, 1, width as u32),     // ImageWidth
        (257, 4, 1, height as u32),    // ImageLength
        (258, 3, 1, 8),                // BitsPerSample
        (259, 3, 1, 1),                // Compression = none
        (262, 3, 1, 1),                // Photometric = BlackIsZero
        (273, 4, 1, strip_offset),     // StripOffsets
        (277, 3, 1, 1),                // SamplesPerPixel
        (278, 4, 1, height as u32),    // RowsPerStrip
        (279, 4, 1, raw.len() as u32), // StripByteCounts
    ];
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, count, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

fn tool_available(tool: &str, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_lzw_tiffcp_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Strip table extracted from a libtiff-written file.
struct StripTable {
    width: usize,
    height: usize,
    rows_per_strip: usize,
    predictor: u16,
    strips: Vec<(usize, usize)>,
}

fn extract_strips(python: &str, script: &Path, tiff: &Path) -> StripTable {
    let output = Command::new(python)
        .arg(script)
        .arg(tiff)
        .output()
        .expect("spawn python extractor");
    assert!(
        output.status.success(),
        "python extractor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut lines = stdout.lines();
    let header: Vec<usize> = lines
        .next()
        .expect("header line")
        .split('\t')
        .map(|f| f.parse().expect("numeric header field"))
        .collect();
    let strips = lines
        .map(|line| {
            let mut fields = line.split('\t');
            let offset = fields.next().expect("offset").parse().expect("offset int");
            let count = fields.next().expect("count").parse().expect("count int");
            (offset, count)
        })
        .collect();
    StripTable {
        width: header[0],
        height: header[1],
        rows_per_strip: header[2],
        predictor: header[3] as u16,
        strips,
    }
}

/// Undo the TIFF horizontal predictor (tag 317 = 2) for 8-bit samples.
fn undo_predictor(rows: &mut [u8], width: usize) {
    for row in rows.chunks_mut(width) {
        for i in 1..row.len() {
            row[i] = row[i].wrapping_add(row[i - 1]);
        }
    }
}

#[test]
fn tiffcp_lzw_strips_decode_into_byte_identical() {
    if !tool_available("tiffcp", &["-h"]) && !tool_available("which", &["tiffcp"]) {
        eprintln!("[tiff-oracle] tiffcp not available; skipping (self-skip, not a failure)");
        return;
    }
    if !tool_available("python3", &["-c", "import struct"]) {
        eprintln!("[tiff-oracle] python3 not available; skipping (self-skip, not a failure)");
        return;
    }

    let dir = scratch_dir("lzw");
    let script = dir.join("extract.py");
    std::fs::write(&script, PY_EXTRACT).expect("write extractor");

    let (width, height) = (301usize, 97usize);
    let raw = image_bytes(width, height);
    let src = dir.join("src.tif");
    std::fs::write(&src, uncompressed_tiff(&raw, width, height)).expect("write source tiff");

    let mut checked_strips = 0usize;
    let mut checked_files = 0usize;
    for codec in ["lzw", "lzw:2"] {
        for rows in ["1", "3", "16", "97"] {
            let dst = dir.join(format!("out_{}_{rows}.tif", codec.replace(':', "_")));
            let output = Command::new("tiffcp")
                .args(["-c", codec, "-r", rows])
                .arg(&src)
                .arg(&dst)
                .output()
                .expect("spawn tiffcp");
            if !output.status.success() {
                eprintln!(
                    "[tiff-oracle] tiffcp -c {codec} -r {rows} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }

            let table = extract_strips("python3", &script, &dst);
            assert_eq!(table.width, width);
            assert_eq!(table.height, height);
            let file = std::fs::read(&dst).expect("read libtiff output");

            for (index, &(offset, count)) in table.strips.iter().enumerate() {
                let first_row = index * table.rows_per_strip;
                let rows_here = table.rows_per_strip.min(height - first_row);
                let expected_len = rows_here * width;
                let strip = &file[offset..offset + count];

                // The zero-allocation path: decode straight into an
                // exactly-sized row buffer.
                let mut decoded = vec![0u8; expected_len];
                let written = decompress_tiff_into(strip, &mut decoded).unwrap_or_else(|e| {
                    panic!("[{codec} r={rows}] strip {index} failed to decode: {e}")
                });
                assert_eq!(
                    written, expected_len,
                    "[{codec} r={rows}] strip {index} produced {written} of {expected_len} bytes"
                );

                // Cross-check against the growable-Vec entry point.
                let via_vec = decompress_tiff(strip, expected_len).expect("vec decode");
                assert_eq!(
                    via_vec, decoded,
                    "[{codec} r={rows}] strip {index} paths disagree"
                );

                if table.predictor == 2 {
                    undo_predictor(&mut decoded, width);
                }
                let expected = &raw[first_row * width..first_row * width + expected_len];
                assert_eq!(
                    decoded, expected,
                    "[{codec} r={rows}] strip {index} decoded to the wrong pixels"
                );
                checked_strips += 1;
            }
            checked_files += 1;
        }
    }

    assert!(
        checked_strips > 100,
        "expected a substantial number of libtiff strips, got {checked_strips}"
    );
    eprintln!(
        "[tiff-oracle] tiffcp: {checked_strips} LZW strips from {checked_files} libtiff files \
         decoded byte-identically through decompress_tiff_into"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

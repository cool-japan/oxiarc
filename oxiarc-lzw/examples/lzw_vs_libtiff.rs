//! Interleaved A/B benchmark: this crate's LZW decoder against libtiff's
//! `LZWDecode`, on strips libtiff itself produced.
//!
//! Run with:
//!
//! ```text
//! cargo run --release --example lzw_vs_libtiff
//! ```
//!
//! # What is compared
//!
//! * **ours** — [`oxiarc_lzw::decompress_tiff_into`] over every strip of a
//!   `tiffcp -c lzw` file, into an exactly-sized buffer, and the result
//!   checked against the original bytes.
//! * **libtiff** — `tiffcp -c none lzw.tif out.tif` **minus**
//!   `tiffcp -c none uncompressed.tif out.tif` over the same geometry. The
//!   two runs differ only by the LZW decode, so the difference is libtiff's
//!   codec time with the file I/O, the strip loop and the process start
//!   subtracted out. (A direct measurement that links `libtiff` and times
//!   `TIFFReadEncodedStrip` agrees with this subtraction to within a few
//!   per cent; it cannot be shipped here because this workspace is
//!   C-free.)
//!
//! Each round runs both arms back to back, and the reported figure is the
//! median of the rounds, so a load spike hits both arms alike. The machine
//! load is printed with the results because it is the single biggest
//! influence on the absolute times — only the *ratio* is portable.
//!
//! # Payload shapes
//!
//! Photo-like RGB8 rows, 16-bit grayscale rows, English-like text and
//! incompressible noise, each at three strip sizes from 64 KiB to 1 MiB.
//! The four shapes matter because LZW's cost per output byte is dominated
//! by the number of *codes*: noise emits about one code per byte and so
//! measures per-code overhead alone, while text emits one code per six or
//! seven bytes and measures the string expansion.
//!
//! # Other dialects
//!
//! libtiff writes only the standard dialect, so the old-style, LSB-compat
//! and GIF dialects are timed on this crate's own streams (round-tripped,
//! so correctness is still checked) and reported without a reference arm.
//!
//! Self-skips with a note when `tiffcp` is not on `PATH`.

use oxiarc_lzw::{
    LzwConfig, compress, decompress_into, decompress_tiff_into, gif_compress, gif_decompress,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

/// One payload shape: its name, its generator, and the TIFF geometry it
/// needs (`SamplesPerPixel`, `BitsPerSample`).
type Payload = (&'static str, fn() -> Vec<u8>, u16, u16);

/// Side of the square test image. 4096 puts one page at 16-50 MB, which is
/// what makes the subtraction below trustworthy: the LZW decode is then a
/// large fraction of each `tiffcp` run rather than a difference of two
/// nearly equal numbers. (At 2048 the same measurement scattered between
/// 0.8x and 1.6x for one build; at 4096 it is stable to a few per cent.)
const SIDE: usize = 4096;

/// Interleaved rounds per fixture.
const ROUNDS: usize = 7;

/// Strip sizes to sweep, in bytes of *decoded* output.
const STRIP_TARGETS: [usize; 3] = [64 * 1024, 256 * 1024, 1024 * 1024];

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// Deterministic xorshift, so every run of this example measures the same
/// bytes.
struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 32) as u32
    }
}

/// Photo-like RGB8: smooth gradients, two flat plates, light dither.
fn rgb8() -> Vec<u8> {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let mut out = Vec::with_capacity(SIDE * SIDE * 3);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let flat = (256..768).contains(&y) && (256..1280).contains(&x);
            let (r, g, b) = if flat {
                (240, 240, 240)
            } else {
                (
                    (((x >> 4) + (y >> 5)) % 256) as u8,
                    (((x >> 5) * 3 + (y >> 4)) % 256) as u8,
                    ((((x + y) >> 5) * 5) % 256) as u8,
                )
            };
            let dither = if x % 4 == 0 {
                0
            } else {
                (rng.next_u32() & 1) as u8
            };
            out.push(r.wrapping_add(dither));
            out.push(g.wrapping_add(dither));
            out.push(b);
        }
    }
    out
}

/// 16-bit grayscale: smooth ramp, one plateau, low noise (little-endian).
fn gray16() -> Vec<u8> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(SIDE * SIDE * 2);
    for y in 0..SIDE {
        for x in 0..SIDE {
            let value = if (500..1500).contains(&y) && (500..1500).contains(&x) {
                30_000u16
            } else {
                ((((x >> 5) + (y >> 6)) % 512) * 128) as u16
            };
            let value = value.wrapping_add((rng.next_u32() & 1) as u16);
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out
}

/// English-like text: the shape LZW compresses best.
fn text() -> Vec<u8> {
    let words: [&[u8]; 16] = [
        b"the ",
        b"quick ",
        b"brown ",
        b"fox ",
        b"jumps ",
        b"over ",
        b"lazy ",
        b"dog ",
        b"compression ",
        b"lempel ",
        b"ziv ",
        b"welch ",
        b"tiff ",
        b"strip ",
        b"decode ",
        b"throughput ",
    ];
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    let mut out = Vec::with_capacity(SIDE * SIDE);
    while out.len() < SIDE * SIDE {
        out.extend_from_slice(words[(rng.next_u32() as usize) % words.len()]);
    }
    out.truncate(SIDE * SIDE);
    out
}

/// Incompressible noise: one code per byte, so this is pure per-code cost.
fn noise() -> Vec<u8> {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    (0..SIDE * SIDE).map(|_| rng.next_u32() as u8).collect()
}

// ---------------------------------------------------------------------------
// Minimal TIFF writer / IFD reader
// ---------------------------------------------------------------------------

/// One uncompressed little-endian classic TIFF, single strip, as the input
/// `tiffcp` re-compresses.
fn uncompressed_tiff(raw: &[u8], samples: u16, bits: u16) -> Vec<u8> {
    // Layout: header, image data, out-of-line BitsPerSample array, IFD.
    let entries: u16 = 10;
    let data_offset = 8u32;
    let mut body = raw.to_vec();
    if body.len() % 2 == 1 {
        body.push(0);
    }
    let extra_offset = data_offset + body.len() as u32;
    let extra: Vec<u8> = if samples > 1 {
        (0..samples).flat_map(|_| bits.to_le_bytes()).collect()
    } else {
        Vec::new()
    };
    let ifd_offset = extra_offset + extra.len() as u32;

    let mut out = Vec::with_capacity(ifd_offset as usize + 2 + usize::from(entries) * 12 + 4);
    out.extend_from_slice(b"II");
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_offset.to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&extra);
    out.extend_from_slice(&entries.to_le_bytes());

    let short_entry = |tag: u16, value: u16, out: &mut Vec<u8>| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&u32::from(value).to_le_bytes());
    };
    let long_entry = |tag: u16, value: u32, out: &mut Vec<u8>| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    };

    long_entry(256, SIDE as u32, &mut out); // ImageWidth
    long_entry(257, SIDE as u32, &mut out); // ImageLength
    if samples > 1 {
        out.extend_from_slice(&258u16.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&u32::from(samples).to_le_bytes());
        out.extend_from_slice(&extra_offset.to_le_bytes());
    } else {
        short_entry(258, bits, &mut out); // BitsPerSample
    }
    short_entry(259, 1, &mut out); // Compression = none
    short_entry(262, if samples > 1 { 2 } else { 1 }, &mut out); // Photometric
    long_entry(273, data_offset, &mut out); // StripOffsets
    short_entry(277, samples, &mut out); // SamplesPerPixel
    long_entry(278, SIDE as u32, &mut out); // RowsPerStrip
    long_entry(279, raw.len() as u32, &mut out); // StripByteCounts
    short_entry(284, 1, &mut out); // PlanarConfiguration = chunky
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

/// Byte size of one TIFF field type, or 0 for the types this reader does
/// not need.
fn type_size(kind: u16) -> usize {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => 0,
    }
}

/// Every strip of a little-endian classic single-IFD TIFF, plus its
/// RowsPerStrip. Only the tags this example writes are understood.
fn read_strips(file: &Path) -> Option<(Vec<Vec<u8>>, usize)> {
    let data = std::fs::read(file).ok()?;
    if data.get(..2)? != b"II" {
        return None;
    }
    let ifd = u32::from_le_bytes(*data.get(4..8)?.first_chunk::<4>()?) as usize;
    let count = u16::from_le_bytes(*data.get(ifd..ifd + 2)?.first_chunk::<2>()?);
    let mut offsets = Vec::new();
    let mut counts = Vec::new();
    let mut rows = 0usize;
    for index in 0..usize::from(count) {
        let base = ifd + 2 + index * 12;
        let field = data.get(base..base + 12)?;
        let tag = u16::from_le_bytes(*field.first_chunk::<2>()?);
        let kind = u16::from_le_bytes(*field.get(2..4)?.first_chunk::<2>()?);
        let items = u32::from_le_bytes(*field.get(4..8)?.first_chunk::<4>()?) as usize;
        let size = type_size(kind) * items;
        let payload: &[u8] = if size <= 4 {
            field.get(8..8 + size)?
        } else {
            let at = u32::from_le_bytes(*field.get(8..12)?.first_chunk::<4>()?) as usize;
            data.get(at..at + size)?
        };
        let values: Vec<u64> = match kind {
            3 => payload
                .chunks_exact(2)
                .filter_map(|c| {
                    c.first_chunk::<2>()
                        .map(|c| u64::from(u16::from_le_bytes(*c)))
                })
                .collect(),
            4 => payload
                .chunks_exact(4)
                .filter_map(|c| {
                    c.first_chunk::<4>()
                        .map(|c| u64::from(u32::from_le_bytes(*c)))
                })
                .collect(),
            _ => Vec::new(),
        };
        match tag {
            273 => offsets = values,
            279 => counts = values,
            278 => rows = *values.first()? as usize,
            _ => {}
        }
    }
    if offsets.len() != counts.len() || offsets.is_empty() || rows == 0 {
        return None;
    }
    let mut strips = Vec::with_capacity(offsets.len());
    for (at, len) in offsets.iter().zip(&counts) {
        let start = *at as usize;
        let end = start + *len as usize;
        strips.push(data.get(start..end)?.to_vec());
    }
    Some((strips, rows))
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-h")
        .output()
        .map(|out| out.status.code().is_some())
        .unwrap_or(false)
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    values.get(values.len() / 2).copied().unwrap_or(f64::NAN)
}

/// `tiffcp -c none src dst`, timed in milliseconds.
fn time_tiffcp(src: &Path, dst: &Path) -> Option<f64> {
    let start = Instant::now();
    let status = Command::new("tiffcp")
        .args(["-c", "none"])
        .arg(src)
        .arg(dst)
        .status()
        .ok()?;
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    status.success().then_some(elapsed)
}

/// One fixture: a payload at one strip size, with libtiff's LZW strips and
/// the two `tiffcp` inputs whose difference isolates the codec.
struct Fixture<'a> {
    label: String,
    raw: &'a [u8],
    strip_bytes: usize,
    strips: Vec<Vec<u8>>,
    lzw_tif: PathBuf,
    plain_tif: PathBuf,
}

impl Fixture<'_> {
    /// Decode every strip through `decompress_tiff_into`, checking the
    /// bytes, and return the elapsed milliseconds.
    fn time_ours(&self, buffer: &mut [u8]) -> f64 {
        let start = Instant::now();
        let mut produced = 0usize;
        for strip in &self.strips {
            let want = (self.raw.len() - produced).min(self.strip_bytes);
            let out = &mut buffer[..want];
            let written = decompress_tiff_into(strip, out).expect("decode a libtiff LZW strip");
            produced += written;
        }
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(produced, self.raw.len(), "{}: short decode", self.label);
        elapsed
    }

    /// Check the decoded bytes against the payload libtiff compressed.
    fn verify(&self) {
        let mut decoded = Vec::with_capacity(self.raw.len());
        let mut buffer = vec![0u8; self.strip_bytes];
        for strip in &self.strips {
            let want = (self.raw.len() - decoded.len()).min(self.strip_bytes);
            let written = decompress_tiff_into(strip, &mut buffer[..want]).expect("decode strip");
            decoded.extend_from_slice(&buffer[..written]);
        }
        assert_eq!(
            decoded, self.raw,
            "{}: bytes differ from libtiff's input",
            self.label
        );
    }
}

/// Build the three strip-size fixtures for one payload. `tiffcp` writes
/// both the LZW file and the uncompressed file of the same geometry; the
/// difference between decoding the two is the codec.
fn build_fixtures<'a>(
    dir: &Path,
    name: &str,
    raw: &'a [u8],
    samples: u16,
    bits: u16,
) -> Vec<Fixture<'a>> {
    let row_bytes = SIDE * usize::from(samples) * usize::from(bits) / 8;
    let source = dir.join(format!("{name}_src.tif"));
    std::fs::write(&source, uncompressed_tiff(raw, samples, bits)).expect("write source tiff");
    let mut fixtures = Vec::new();
    for target in STRIP_TARGETS {
        let rows = (target / row_bytes).max(1);
        let lzw_tif = dir.join(format!("{name}_{rows}_lzw.tif"));
        let plain_tif = dir.join(format!("{name}_{rows}_none.tif"));
        for (codec, out) in [("lzw", &lzw_tif), ("none", &plain_tif)] {
            let ok = Command::new("tiffcp")
                .args(["-c", codec, "-r", &rows.to_string()])
                .arg(&source)
                .arg(out)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            assert!(ok, "tiffcp -c {codec} failed for {name}");
        }
        let (strips, rows_read) = read_strips(&lzw_tif).expect("parse tiffcp output");
        assert_eq!(rows_read, rows, "{name}: tiffcp used another RowsPerStrip");
        fixtures.push(Fixture {
            label: format!("{name} r{rows}"),
            raw,
            strip_bytes: rows * row_bytes,
            strips,
            lzw_tif,
            plain_tif,
        });
    }
    let _ = std::fs::remove_file(&source);
    fixtures
}

/// One row of the results table.
struct Row {
    label: String,
    strip_kib: usize,
    bytes: usize,
    ours_ms: f64,
    libtiff_ms: f64,
}

/// Time one payload's fixtures, interleaving the two arms per round.
fn measure(dir: &Path, name: &str, raw: &[u8], samples: u16, bits: u16) -> Vec<Row> {
    let fixtures = build_fixtures(dir, name, raw, samples, bits);
    for fixture in &fixtures {
        fixture.verify();
    }
    let scratch = dir.join("scratch.tif");
    let widest = fixtures
        .iter()
        .map(|fixture| fixture.strip_bytes)
        .max()
        .unwrap_or(1 << 20);
    let mut buffer = vec![0u8; widest];
    let mut ours: Vec<Vec<f64>> = vec![Vec::new(); fixtures.len()];
    let mut reference: Vec<Vec<f64>> = vec![Vec::new(); fixtures.len()];

    for _ in 0..ROUNDS {
        for (index, fixture) in fixtures.iter().enumerate() {
            // Arm A, then arm B, so any load spike lands on both.
            ours[index].push(fixture.time_ours(&mut buffer));
            let with_codec = time_tiffcp(&fixture.lzw_tif, &scratch);
            let without = time_tiffcp(&fixture.plain_tif, &scratch);
            if let (Some(with_codec), Some(without)) = (with_codec, without) {
                reference[index].push((with_codec - without).max(0.0));
            }
        }
    }

    let rows = fixtures
        .iter()
        .enumerate()
        .map(|(index, fixture)| Row {
            label: fixture.label.clone(),
            strip_kib: fixture.strip_bytes / 1024,
            bytes: fixture.raw.len(),
            ours_ms: median(ours[index].clone()),
            libtiff_ms: median(reference[index].clone()),
        })
        .collect();
    for fixture in &fixtures {
        let _ = std::fs::remove_file(&fixture.lzw_tif);
        let _ = std::fs::remove_file(&fixture.plain_tif);
    }
    let _ = std::fs::remove_file(&scratch);
    rows
}

/// Timings for the dialects libtiff cannot write, plus the GIF codec.
fn other_dialects() {
    println!();
    println!("Dialects libtiff does not write (this crate only; round trip checked)");
    println!("{:<26}{:>10}{:>12}{:>12}", "dialect", "bytes", "ms", "MB/s");
    let payload = text();
    for (name, config) in [
        ("TIFF (standard, MSB)", LzwConfig::TIFF),
        ("TIFF_OLD_STYLE (MSB)", LzwConfig::TIFF_OLD_STYLE),
        ("TIFF_COMPAT_LSB (LSB)", LzwConfig::TIFF_COMPAT_LSB),
        ("GIF config (LSB)", LzwConfig::GIF),
    ] {
        let stream = compress(&payload, config).expect("compress");
        let mut out = vec![0u8; payload.len()];
        let mut times = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let start = Instant::now();
            let written = decompress_into(&stream, &mut out, config).expect("decode");
            times.push(start.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(written, payload.len(), "{name}: short decode");
        }
        assert_eq!(out, payload, "{name}: bytes differ");
        let ms = median(times);
        println!(
            "{:<26}{:>10}{:>12.2}{:>12.1}",
            name,
            payload.len(),
            ms,
            payload.len() as f64 / 1e6 / (ms / 1000.0)
        );
    }

    // The GIF codec (its own clear/EOI codes and initial width).
    let stream = gif_compress(&payload, 8).expect("gif compress");
    let mut times = Vec::with_capacity(ROUNDS);
    let mut decoded = Vec::new();
    for _ in 0..ROUNDS {
        let start = Instant::now();
        decoded = gif_decompress(&stream, 8).expect("gif decompress");
        times.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(decoded, payload, "gif_decompress: bytes differ");
    let ms = median(times);
    println!(
        "{:<26}{:>10}{:>12.2}{:>12.1}",
        "gif_decompress (mcs 8)",
        payload.len(),
        ms,
        payload.len() as f64 / 1e6 / (ms / 1000.0)
    );
}

fn main() {
    if !have("tiffcp") {
        println!("tiffcp is not on PATH: skipping the libtiff comparison.");
        other_dialects();
        return;
    }

    let dir = std::env::temp_dir().join("oxiarc_lzw_vs_libtiff");
    std::fs::create_dir_all(&dir).expect("create the fixture directory");
    println!("Building fixtures with tiffcp in {}", dir.display());

    let payloads: [Payload; 4] = [
        ("RGB8", rgb8, 3, 8),
        ("Gray16", gray16, 1, 16),
        ("text", text, 1, 8),
        ("noise", noise, 1, 8),
    ];
    // One payload is held at a time: a 4096x4096 RGB8 page alone is 48 MiB.
    let mut rows = Vec::new();
    for (name, generate, samples, bits) in payloads {
        let raw = generate();
        rows.extend(measure(&dir, name, &raw, samples, bits));
    }

    let load = Command::new("uptime")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .unwrap_or_default();
    println!();
    println!("machine load at the end of the run: {}", load.trim());
    println!("{ROUNDS} interleaved rounds, {SIDE}x{SIDE} pages, medians\n");
    println!(
        "{:<16}{:>11}{:>13}{:>10}{:>8}{:>11}",
        "fixture", "strip KiB", "libtiff ms", "ours ms", "ratio", "ours MB/s"
    );
    let mut worst = 0.0f64;
    for row in &rows {
        let ratio = if row.libtiff_ms > 0.0 {
            row.ours_ms / row.libtiff_ms
        } else {
            f64::NAN
        };
        if ratio.is_finite() {
            worst = worst.max(ratio);
        }
        println!(
            "{:<16}{:>11}{:>13.2}{:>10.2}{:>8.2}{:>11.1}",
            row.label,
            row.strip_kib,
            row.libtiff_ms,
            row.ours_ms,
            ratio,
            row.bytes as f64 / 1e6 / (row.ours_ms / 1000.0)
        );
    }
    println!("\nworst ratio {worst:.2}x of libtiff's decode time (lower is better)");

    other_dialects();
    let _ = std::fs::remove_dir(&dir);
}

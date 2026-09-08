//! Criterion-free A/B decode-throughput measurement against the `zstd` CLI.
//!
//! Run it in release mode (a debug build measures the optimiser, not the
//! decoder):
//!
//! ```text
//! cargo run --release -p oxiarc-zstd --example decode_throughput
//! cargo run --release -p oxiarc-zstd --example decode_throughput -- text     # name filter
//! OXIARC_ZSTD_PERF_ROUNDS=15 cargo run --release -p oxiarc-zstd --example decode_throughput
//! OXIARC_ZSTD_PERF_NO_REF=1 cargo run --release -p oxiarc-zstd --example decode_throughput
//! ```
//!
//! `OXIARC_ZSTD_PERF_NO_REF` drops the reference arm (which costs a second per
//! round), for profiling and for quick before/after comparisons of one shape.
//!
//! # What is measured
//!
//! Four arms over the *same* frame bytes, one round of each in turn
//! (interleaved, so a change in machine load moves every arm together) and the
//! **median** of the rounds reported:
//!
//! | arm | entry point |
//! |---|---|
//! | `into` | [`oxiarc_zstd::decompress_into`] — one-shot into a caller slice, fresh decoder per call |
//! | `frame` | [`oxiarc_zstd::ZstdDecoder::decode_frame`] — legacy one-shot, decoder reused |
//! | `stream` | [`oxiarc_zstd::ZstdStream`] — push decoder, 64 KiB output chunks, stream reused |
//! | `zstd` | `zstd -b -d` — libzstd single-thread, in-memory, repeated by the CLI itself |
//!
//! Every arm's output is compared byte-for-byte against the fixture before any
//! timing is reported, so a fast wrong answer cannot be mistaken for a win.
//!
//! The `zstd` arm self-skips (a note, not a failure) when the CLI is absent.
//! The machine's load average is printed with the table because the ratios,
//! not the absolute numbers, are the deliverable.
//!
//! # Fixtures
//!
//! Generated programmatically and cached under
//! `std::env::temp_dir()/oxiarc-zstd-perf/`, so repeated A/B runs reuse them
//! (building the 50 MB level-19 frame takes minutes). Shapes:
//!
//! * `tiff288k` / `tiff1m` — RGB8 image rows, the TIFF-strip shape: a smooth
//!   gradient plus per-row noise, 4096 px wide, at levels 1, 3, 9 and 19.
//! * `text50m` — 50 MB of generated prose with paragraph-scale repeats several
//!   megabytes apart, at levels 3 and 19 (long distances, large window).
//! * `random8m` — incompressible (the frame is almost all `Raw` blocks).
//! * `repeat8m` — highly repetitive (short-offset overlapping matches).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdDecoder, ZstdStatus, ZstdStream, decompress_into};

/// Output chunk the `stream` arm drains into (the contract's 64 KiB).
const OUT_CHUNK: usize = 64 * 1024;

/// Rounds per arm; the median is reported. Override with
/// `OXIARC_ZSTD_PERF_ROUNDS`.
const DEFAULT_ROUNDS: usize = 9;

/// One measurement shape: a raw payload and the frame the CLI made of it.
struct Fixture {
    /// Shape name (the filter matches on this).
    name: String,
    /// Compression level the frame was produced at.
    level: i32,
    /// The original bytes; every arm must reproduce them exactly.
    raw: Vec<u8>,
    /// The `.zst` frame.
    frame: Vec<u8>,
}

fn main() {
    let filter = std::env::args().nth(1);
    let rounds = std::env::var("OXIARC_ZSTD_PERF_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_ROUNDS)
        .max(1);

    if !have_zstd() {
        println!("note: `zstd` is not on PATH — fixtures and the reference arm are unavailable.");
        println!("      install zstd 1.5+ and re-run; nothing is measured without it.");
        return;
    }

    let dir = std::env::temp_dir().join("oxiarc-zstd-perf");
    std::fs::create_dir_all(&dir).expect("create the fixture directory");

    println!("load average at start: {}", load_average());
    println!("fixture cache: {}", dir.display());
    println!();

    let plan: Vec<(&str, i32)> = vec![
        ("tiff288k", 1),
        ("tiff288k", 3),
        ("tiff288k", 9),
        ("tiff288k", 19),
        ("tiff1m", 1),
        ("tiff1m", 3),
        ("tiff1m", 9),
        ("tiff1m", 19),
        ("text50m", 3),
        ("text50m", 19),
        ("random8m", 3),
        ("repeat8m", 3),
    ];

    let mut rows: Vec<Row> = Vec::new();
    for (name, level) in plan {
        if let Some(f) = filter.as_deref() {
            if !name.contains(f) {
                continue;
            }
        }
        let fixture = load_fixture(&dir, name, level);
        rows.push(measure(&fixture, rounds, &dir));
    }

    for (label, pick) in [("median", 0usize), ("best", 1usize)] {
        println!();
        println!("{label} of {rounds} interleaved rounds, MB/s of *output* (1 MB = 1e6 bytes)");
        println!(
            "{:<10} {:>3} {:>10} {:>7} {:>9} {:>9} {:>9} {:>9} {:>7} {:>7} {:>7}",
            "shape",
            "lvl",
            "output",
            "ratio",
            "into",
            "frame",
            "stream",
            "zstd",
            "into/z",
            "frm/z",
            "str/z"
        );
        for r in &rows {
            let v = if pick == 0 { r.median } else { r.best };
            println!(
                "{:<10} {:>3} {:>10} {:>7.2} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7} {:>7} {:>7}",
                r.name,
                r.level,
                r.output,
                r.ratio,
                v[0],
                v[1],
                v[2],
                v[3],
                fmt_ratio(v[0], v[3]),
                fmt_ratio(v[1], v[3]),
                fmt_ratio(v[2], v[3]),
            );
        }
    }
    println!();
    println!("load average at end:   {}", load_average());
}

/// One row of the report.
struct Row {
    name: String,
    level: i32,
    output: usize,
    ratio: f64,
    /// Median and best-of MB/s per arm, in the order `into, frame, stream, zstd`.
    median: [f64; 4],
    best: [f64; 4],
}

/// Format `ours / reference` as a ratio, or `n/a` when the reference is absent.
fn fmt_ratio(ours: f64, reference: f64) -> String {
    if reference <= 0.0 {
        "n/a".to_string()
    } else {
        format!("{:.2}x", ours / reference)
    }
}

/// Measure every arm on one fixture, interleaving the rounds.
///
/// All four arms — including the `zstd` CLI benchmark — run once per round, in
/// the same order, so a change in machine load moves them together. Both the
/// median and the best of the rounds are reported: on a contended box the
/// median measures the contention and the best measures the decoder, and a
/// conclusion that only holds for one of them is not a conclusion.
fn measure(fixture: &Fixture, rounds: usize, dir: &Path) -> Row {
    let mut into_mbs: Vec<f64> = Vec::with_capacity(rounds);
    let mut frame_mbs: Vec<f64> = Vec::with_capacity(rounds);
    let mut stream_mbs: Vec<f64> = Vec::with_capacity(rounds);
    let mut zstd_mbs: Vec<f64> = Vec::with_capacity(rounds);

    let out = fixture.raw.len();
    let path = frame_path(dir, &fixture.name, fixture.level);

    let mut dst = vec![0u8; out];
    let mut decoder = ZstdDecoder::new();
    let mut stream = ZstdStream::new()
        .with_multi_frame(false)
        .with_max_window(usize::MAX);
    let mut chunk = vec![0u8; OUT_CHUNK];
    let mut collected: Vec<u8> = Vec::with_capacity(out);

    // Correctness first: a fast wrong answer is not a measurement.
    let n = decompress_into(&fixture.frame, &mut dst).expect("decompress_into");
    assert_eq!(&dst[..n], &fixture.raw[..], "decompress_into mismatch");
    let owned = decoder.decode_frame(&fixture.frame).expect("decode_frame");
    assert_eq!(owned, fixture.raw, "decode_frame mismatch");
    run_stream(&mut stream, &fixture.frame, &mut chunk, &mut collected);
    assert_eq!(collected, fixture.raw, "ZstdStream mismatch");

    for _ in 0..rounds {
        let t = Instant::now();
        let n = decompress_into(&fixture.frame, &mut dst).expect("decompress_into");
        into_mbs.push(mbs(out, t.elapsed()));
        assert_eq!(n, out);

        let t = Instant::now();
        let owned = decoder.decode_frame(&fixture.frame).expect("decode_frame");
        frame_mbs.push(mbs(out, t.elapsed()));
        assert_eq!(owned.len(), out);

        let t = Instant::now();
        run_stream(&mut stream, &fixture.frame, &mut chunk, &mut collected);
        stream_mbs.push(mbs(out, t.elapsed()));
        assert_eq!(collected.len(), out);

        // Skipping the reference arm makes a profiling run (or a quick A/B of
        // one shape) spend its time in the decoder instead of waiting a second
        // per round on the CLI benchmark.
        if std::env::var_os("OXIARC_ZSTD_PERF_NO_REF").is_none() {
            zstd_mbs.push(zstd_decode_mbs(&path));
        } else {
            zstd_mbs.push(0.0);
        }
    }

    Row {
        name: fixture.name.clone(),
        level: fixture.level,
        output: out,
        ratio: out as f64 / fixture.frame.len() as f64,
        median: [
            median(&mut into_mbs),
            median(&mut frame_mbs),
            median(&mut stream_mbs),
            median(&mut zstd_mbs),
        ],
        best: [
            best(&into_mbs),
            best(&frame_mbs),
            best(&stream_mbs),
            best(&zstd_mbs),
        ],
    }
}

/// Drive [`ZstdStream`] to the end of the frame, 64 KiB of output at a time.
fn run_stream(stream: &mut ZstdStream, frame: &[u8], chunk: &mut [u8], out: &mut Vec<u8>) {
    stream.reset();
    out.clear();
    let mut pos = 0usize;
    loop {
        let progress = stream
            .decode(&frame[pos..], chunk, FlushMode::Finish)
            .expect("ZstdStream::decode");
        pos += progress.consumed;
        out.extend_from_slice(&chunk[..progress.produced]);
        match progress.status {
            ZstdStatus::StreamEnd => return,
            ZstdStatus::NeedOutput => continue,
            ZstdStatus::NeedInput => {
                assert!(
                    progress.consumed != 0 || progress.produced != 0,
                    "ZstdStream made no progress"
                );
            }
            other => panic!("unexpected ZstdStatus {other:?}"),
        }
    }
}

/// Output MB/s (decimal megabytes) for `bytes` produced in `elapsed`.
fn mbs(bytes: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return f64::INFINITY;
    }
    bytes as f64 / secs / 1.0e6
}

/// Median of the throughput samples (sorts in place).
fn median(samples: &mut [f64]) -> f64 {
    samples.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    samples[samples.len() / 2]
}

/// The fastest of the throughput samples: the round least disturbed by other
/// load on the machine.
fn best(samples: &[f64]) -> f64 {
    samples.iter().copied().fold(0.0f64, f64::max)
}

/// `true` when the reference CLI is callable.
fn have_zstd() -> bool {
    Command::new("zstd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// The machine's load average, or a note when it cannot be read.
fn load_average() -> String {
    Command::new("uptime")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.split_once("load average")
                .or_else(|| s.split_once("load averages"))
                .map(|(_, tail)| tail.trim_start_matches([':', 's', ' ']).trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// libzstd's own decode throughput on `path`, via `zstd -b -d` (in-memory,
/// repeated by the CLI for at least a second). Returns 0.0 if it cannot be
/// parsed.
fn zstd_decode_mbs(path: &Path) -> f64 {
    let out = Command::new("zstd")
        .args(["-b", "-d", "-i1"])
        .arg(path)
        .output();
    let Ok(out) = out else { return 0.0 };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    // The CLI overwrites one progress line with `\r`; the final segment holds
    // `... , <compress> MB/s, <decompress> MB/s <level>#`.
    let mut last = 0.0f64;
    for segment in text.split(['\r', '\n']) {
        let fields: Vec<&str> = segment.split(',').collect();
        for field in fields {
            let field = field.trim();
            if let Some(num) = field.strip_suffix("MB/s") {
                if let Ok(v) = num.trim().parse::<f64>() {
                    last = v;
                }
            } else if let Some(rest) = field.split_once("MB/s") {
                if let Ok(v) = rest.0.trim().parse::<f64>() {
                    last = v;
                }
            }
        }
    }
    last
}

/// Path of the cached frame for one shape and level.
fn frame_path(dir: &Path, name: &str, level: i32) -> PathBuf {
    dir.join(format!("{name}.l{level}.zst"))
}

/// Load (or build and cache) one fixture.
fn load_fixture(dir: &Path, name: &str, level: i32) -> Fixture {
    let raw_path = dir.join(format!("{name}.raw"));
    let raw = if raw_path.is_file() {
        std::fs::read(&raw_path).expect("read the cached payload")
    } else {
        let raw = build_payload(name);
        std::fs::write(&raw_path, &raw).expect("cache the payload");
        raw
    };

    let frame_path = frame_path(dir, name, level);
    if !frame_path.is_file() {
        eprintln!("building {name} level {level} ({} bytes)…", raw.len());
        let mut cmd = Command::new("zstd");
        cmd.args(["-q", "-f", &format!("-{level}")]);
        // Long-distance matching only where the shape calls for it: a 50 MB
        // corpus with far repeats is exactly what `--long` exists for, and it
        // makes the frame declare a large window. A TIFF strip is written by
        // libtiff/GDAL with a plain `ZSTD_compress`, so leaving the default
        // window is what keeps that shape representative.
        if name == "text50m" {
            cmd.arg("--long=27");
        }
        let status = cmd
            .arg(&raw_path)
            .arg("-o")
            .arg(&frame_path)
            .status()
            .expect("run the zstd CLI");
        assert!(status.success(), "zstd -{level} failed for {name}");
    }
    let frame = std::fs::read(&frame_path).expect("read the cached frame");

    Fixture {
        name: name.to_string(),
        level,
        raw,
        frame,
    }
}

/// Build one shape's payload.
fn build_payload(name: &str) -> Vec<u8> {
    match name {
        "tiff288k" => rgb8_rows(4096, 24),
        "tiff1m" => rgb8_rows(4096, 88),
        "text50m" => text_corpus(50_000_000),
        "random8m" => random_bytes(8 << 20),
        "repeat8m" => repetitive(8 << 20),
        other => panic!("unknown fixture {other}"),
    }
}

/// A deterministic xorshift64* step.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    *state = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// RGB8 image rows: a smooth two-axis gradient plus bounded per-pixel noise.
///
/// This is the TIFF-strip shape — the rows correlate strongly with their
/// predecessor, so the encoder emits long matches at a stride of `width * 3`,
/// which is what a photographic strip looks like to a zstd decoder.
fn rgb8_rows(width: usize, rows: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9ABC_DEF1u64;
    let mut out = Vec::with_capacity(width * rows * 3);
    for y in 0..rows {
        for x in 0..width {
            let noise = (next_rand(&mut state) & 0x0F) as usize;
            let r = ((x * 255) / width + noise) & 0xFF;
            let g = ((y * 255) / rows.max(1) + noise / 2) & 0xFF;
            let b = ((x + y) * 128 / (width + rows) + noise / 4) & 0xFF;
            out.push(r as u8);
            out.push(g as u8);
            out.push(b as u8);
        }
    }
    out
}

/// Generated prose with paragraph-scale repeats several megabytes apart.
///
/// The long-range repeats are the point: they make a level-19 frame use far
/// offsets and a large window, which a locally-repetitive corpus would not.
fn text_corpus(target: usize) -> Vec<u8> {
    const WORDS: [&str; 32] = [
        "the",
        "quick",
        "brown",
        "fox",
        "jumps",
        "over",
        "lazy",
        "dog",
        "compression",
        "window",
        "sequence",
        "literal",
        "offset",
        "entropy",
        "huffman",
        "table",
        "block",
        "frame",
        "stream",
        "decoder",
        "encoder",
        "buffer",
        "history",
        "match",
        "length",
        "checksum",
        "dictionary",
        "reference",
        "throughput",
        "measurement",
        "pathology",
        "median",
    ];
    let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
    let mut out: Vec<u8> = Vec::with_capacity(target + 4096);
    let mut next_repeat = 2_000_000usize;
    while out.len() < target {
        if out.len() >= next_repeat && out.len() > 1_500_000 {
            // Re-emit 128 KiB from ~1.5 MB back: a far-offset match.
            let from = out.len() - 1_500_000;
            let len = 128 * 1024;
            out.extend_from_within(from..from + len);
            next_repeat = out.len() + 2_000_000;
            continue;
        }
        let sentence_words = 8 + (next_rand(&mut state) % 12) as usize;
        for i in 0..sentence_words {
            let w = WORDS[(next_rand(&mut state) % WORDS.len() as u64) as usize];
            if i > 0 {
                out.push(b' ');
            }
            out.extend_from_slice(w.as_bytes());
        }
        out.extend_from_slice(b".\n");
    }
    out.truncate(target);
    out
}

/// Incompressible bytes (the encoder falls back to `Raw` blocks).
fn random_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x0BAD_C0DE_1234_5678u64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        out.extend_from_slice(&next_rand(&mut state).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Highly repetitive: short cycles, so the decoder's overlapping-match copy
/// dominates.
fn repetitive(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let pattern = b"oxiarc-zstd/";
    while out.len() + pattern.len() <= len {
        out.extend_from_slice(pattern);
    }
    out.resize(len, b'-');
    out
}

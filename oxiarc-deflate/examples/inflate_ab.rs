//! Criterion-free A/B table: `oxiarc-deflate`'s four decode entry points
//! versus CPython's `zlib.decompress`.
//!
//! Every arm decodes **the same bytes** (a zlib stream produced by python's
//! `zlib.compress`, plus its raw DEFLATE payload for the raw entry point),
//! and every arm's output is checked against the original buffer before it
//! is timed, so a fast-but-wrong decoder cannot win a row.
//!
//! Arms:
//!
//! | column | entry point | shape |
//! |---|---|---|
//! | `inflate` | [`oxiarc_deflate::inflate`] | one-shot raw DEFLATE into a `Vec` |
//! | `stream` | [`oxiarc_deflate::InflateStream`] | push, 64 KiB output slices |
//! | `wrapped` | [`oxiarc_deflate::WrappedInflate`] | push, zlib framing + Adler-32 |
//! | `reader` | [`oxiarc_deflate::InflateReader`] | `Read` adapter over a `Cursor` |
//! | `python` | `zlib.decompress` | timed *inside* python with `perf_counter` |
//!
//! Measurements are **interleaved** (one round of every arm, then the next
//! round) and reported as medians, because the machine this runs on is
//! shared: absolute MB/s drifts with load, ratios do not. The load average
//! at the start and end of the run is printed with the tables.
//!
//! Two ratio columns follow the arms: `worst` is the worst arm over python
//! using each side's **median**, `wbest` the same using each side's **best
//! round**. On a quiet machine they agree; under contention the medians
//! collapse towards whatever else is running and `wbest` — the round that
//! got a whole performance core — is the figure to read.
//!
//! Run with:
//!
//! ```text
//! cargo run --release --example inflate_ab
//! ```
//!
//! Optional arguments restrict the matrix, e.g. `-- rgb8` (shape substring),
//! `-- rgb8 1MiB` (shape + size), or `-- all 64KiB 6` (size + level).

#[path = "../tests/common/corpus.rs"]
mod corpus;

use std::io::{Cursor, Read};
use std::process::Command;
use std::time::Instant;

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{
    InflateReader, InflateStatus, InflateStream, InflateWrapper, WrappedInflate, inflate,
    inflate_into, zlib_compress,
};

/// One (shape, size) corpus buffer.
struct Shape {
    name: &'static str,
    size_label: &'static str,
    data: Vec<u8>,
    /// When set, the reference stream is produced with a `Z_FULL_FLUSH`
    /// every this many input bytes (the many-block-headers shape).
    flush_every: Option<usize>,
}

/// Output staging size for the push arms, matching `InflateReader`'s own.
const OUT_CHUNK: usize = 64 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("iter") {
        iterate(args.get(1).cloned());
        return;
    }
    let shape_filter = args.first().cloned();
    let size_filter = args.get(1).cloned();
    let level_filter = args.get(2).and_then(|s| s.parse::<u8>().ok());

    let have_python = corpus::python3_available();
    if !have_python {
        println!("note: python3 with zlib not found - the reference column is omitted\n");
    }

    println!("load average at start: {}", load_average());
    println!(
        "arms: inflate (one-shot raw) | stream (push 64 KiB) | wrapped (zlib push) | \
         reader (Read adapter) | python (zlib.decompress)\n"
    );

    let sizes: [(&str, usize); 3] = [
        ("64KiB", 64 * 1024),
        ("1MiB", 1024 * 1024),
        ("16MiB", 16 * 1024 * 1024),
    ];

    let mut worst = f64::MAX;
    let mut worst_where = String::new();
    let mut rows = 0usize;

    for (size_label, size) in sizes {
        if let Some(f) = size_filter.as_deref() {
            if f != "all" && !size_label.contains(f) {
                continue;
            }
        }
        for shape in shapes(size, size_label) {
            if let Some(f) = shape_filter.as_deref() {
                if f != "all" && !shape.name.contains(f) {
                    continue;
                }
            }
            println!(
                "=== {} @ {} ({} bytes) ===",
                shape.name,
                shape.size_label,
                shape.data.len()
            );
            println!(
                "{:>3} {:>10} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7} {:>7}",
                "lvl",
                "compressed",
                "inflate",
                "stream",
                "wrapped",
                "reader",
                "into",
                "python",
                "worst",
                "wbest"
            );
            for level in [1u8, 6, 9] {
                if level_filter.is_some_and(|l| l != level) {
                    continue;
                }
                let zlib_stream = compress(&shape.data, level, have_python, shape.flush_every);
                let raw = raw_payload(&zlib_stream);
                // More rounds than a quiet machine would need: this one is
                // shared, and the maximum of a run is a far better estimate
                // of uncontended throughput than its median when another
                // build is competing for the same cores.
                let rounds = if shape.data.len() > (4 << 20) { 5 } else { 9 };

                let mut ours = [
                    Vec::<f64>::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ];
                let mut theirs: Vec<f64> = Vec::new();
                let mut scratch = vec![0u8; shape.data.len()];
                for _ in 0..rounds {
                    ours[0].push(time_mbs(&shape.data, || arm_inflate(&raw)));
                    let want = shape.data.len();
                    ours[1].push(time_mbs(&shape.data, || arm_stream(&raw, want)));
                    ours[2].push(time_mbs(&shape.data, || arm_wrapped(&zlib_stream, want)));
                    ours[3].push(time_mbs(&shape.data, || arm_reader(&raw, want)));
                    ours[4].push(time_into(&shape.data, &raw, &mut scratch));
                    if have_python {
                        if let Some(mbs) =
                            corpus::python_zlib_decompress_throughput(&zlib_stream, 1)
                        {
                            theirs.push(mbs);
                        }
                    }
                }
                let med: Vec<f64> = ours.iter().map(|v| median(v)).collect();
                let best: Vec<f64> = ours.iter().map(|v| best_of(v)).collect();
                let py = median(&theirs);
                let py_best = best_of(&theirs);
                let worst_median = ratio_of_worst(&med, py);
                let worst_best = ratio_of_worst(&best, py_best);
                let (worst_arm, worst_best_text) = if py > 0.0 {
                    (format!("{worst_median:.2}x"), format!("{worst_best:.2}x"))
                } else {
                    ("-".to_string(), "-".to_string())
                };
                if py > 0.0 && worst_best < worst {
                    worst = worst_best;
                    worst_where = format!("{} @ {} level {}", shape.name, shape.size_label, level);
                }
                rows += 1;
                println!(
                    "{:>3} {:>10} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7} {:>7}",
                    level,
                    zlib_stream.len(),
                    med[0],
                    med[1],
                    med[2],
                    med[3],
                    med[4],
                    py,
                    worst_arm,
                    worst_best_text
                );
            }
            println!();
        }
    }

    println!("load average at end:   {}", load_average());
    if worst < f64::MAX {
        println!(
            "rows measured: {rows}; worst arm/python ratio over the whole matrix \
             (best-of-round, the load-robust figure): {worst:.2}x at {worst_where} \
             (gate: >= 0.60x, target 0.80x)"
        );
    }
}

/// Ours-only sweep used while optimising: no python, best-of-9 (robust to
/// a loaded machine), the two arms that isolate the symbol loop.
fn iterate(filter: Option<String>) {
    println!("load average: {}", load_average());
    println!(
        "{:<20} {:>3} {:>10} {:>10}",
        "shape", "lvl", "inflate", "into"
    );
    for shape in shapes(1024 * 1024, "1MiB") {
        if let Some(f) = filter.as_deref() {
            if !shape.name.contains(f) {
                continue;
            }
        }
        for level in [1u8, 6, 9] {
            let zlib_stream = compress(&shape.data, level, true, shape.flush_every);
            let raw = raw_payload(&zlib_stream);
            let mut scratch = vec![0u8; shape.data.len()];
            let mut a = 0.0f64;
            let mut b = 0.0f64;
            for _ in 0..9 {
                a = a.max(time_mbs(&shape.data, || arm_inflate(&raw)));
                b = b.max(time_into(&shape.data, &raw, &mut scratch));
            }
            println!("{:<20} {:>3} {:>10.1} {:>10.1}", shape.name, level, a, b);
        }
    }
}

/// The five data shapes the decode gate covers.
fn shapes(size: usize, size_label: &'static str) -> Vec<Shape> {
    vec![
        Shape {
            name: "png-filtered-rows",
            size_label,
            data: corpus::png_filtered_image_rows(size),
            flush_every: None,
        },
        Shape {
            name: "rgb8-image-rows",
            size_label,
            data: corpus::rgb8_image_rows(size),
            flush_every: None,
        },
        Shape {
            name: "text",
            size_label,
            // Deterministic: `rust_sources` reads the workspace, which
            // changes while the decoder is being optimised, so the "text"
            // row would not be comparable across runs.
            data: corpus::html_like(size),
            flush_every: None,
        },
        Shape {
            name: "incompressible",
            size_label,
            data: corpus::random(size),
            flush_every: None,
        },
        Shape {
            name: "repetitive-json",
            size_label,
            data: corpus::json_records(size),
            flush_every: None,
        },
        Shape {
            name: "many-blocks-text",
            size_label,
            data: corpus::html_like(size),
            flush_every: Some(8 * 1024),
        },
    ]
}

/// A complete zlib stream: python's when it is available (so the bytes are
/// the reference encoder's), ours otherwise.
fn compress(data: &[u8], level: u8, have_python: bool, flush_every: Option<usize>) -> Vec<u8> {
    if have_python {
        let bytes = match flush_every {
            Some(chunk) => corpus::python_zlib_compress_full_flush(data, level, chunk),
            None => corpus::python_zlib_compress(data, level),
        };
        if let Some(bytes) = bytes {
            return bytes;
        }
    }
    zlib_compress(data, level).expect("zlib_compress")
}

/// The raw DEFLATE payload of a zlib stream (2-byte header, 4-byte Adler-32).
fn raw_payload(zlib_stream: &[u8]) -> Vec<u8> {
    let end = zlib_stream.len().saturating_sub(4);
    zlib_stream.get(2..end).unwrap_or_default().to_vec()
}

fn arm_inflate(raw: &[u8]) -> Vec<u8> {
    inflate(raw).expect("inflate")
}

fn arm_stream(raw: &[u8], expected: usize) -> Vec<u8> {
    let mut stream = InflateStream::new();
    // Pre-sized: the staging `Vec`'s growth is the harness's cost, not the
    // decoder's, and python's `zlib.decompress` does not pay it either.
    let mut out = Vec::with_capacity(expected);
    let mut chunk = vec![0u8; OUT_CHUNK];
    let mut pos = 0usize;
    let mut guard = 0u32;
    loop {
        let progress = stream
            .inflate(&raw[pos..], &mut chunk, FlushMode::Finish)
            .expect("InflateStream::inflate");
        pos += progress.consumed;
        out.extend_from_slice(&chunk[..progress.produced]);
        guard += 1;
        assert!(guard < 1_000_000, "runaway InflateStream drive loop");
        match progress.status {
            InflateStatus::StreamEnd => break,
            _ if progress.consumed == 0 && progress.produced == 0 => panic!("no progress"),
            _ => {}
        }
    }
    out
}

fn arm_wrapped(zlib_stream: &[u8], expected: usize) -> Vec<u8> {
    let mut wrapped = WrappedInflate::new(InflateWrapper::Zlib);
    let mut out = Vec::with_capacity(expected);
    let mut chunk = vec![0u8; OUT_CHUNK];
    let mut pos = 0usize;
    let mut guard = 0u32;
    loop {
        let progress = wrapped
            .inflate(&zlib_stream[pos..], &mut chunk, FlushMode::Finish)
            .expect("WrappedInflate::inflate");
        pos += progress.consumed;
        out.extend_from_slice(&chunk[..progress.produced]);
        guard += 1;
        assert!(guard < 1_000_000, "runaway WrappedInflate drive loop");
        match progress.status {
            InflateStatus::StreamEnd => break,
            _ if progress.consumed == 0 && progress.produced == 0 => panic!("no progress"),
            _ => {}
        }
    }
    out
}

fn arm_reader(raw: &[u8], expected: usize) -> Vec<u8> {
    let mut reader = InflateReader::new(Cursor::new(raw), InflateWrapper::Raw);
    let mut out = Vec::with_capacity(expected);
    reader.read_to_end(&mut out).expect("InflateReader");
    out
}

/// `inflate_into` into an exactly-sized buffer: no staging copy, no `Vec`
/// growth, so this column isolates the symbol loop itself.
fn time_into(expect: &[u8], raw: &[u8], scratch: &mut [u8]) -> f64 {
    let t0 = Instant::now();
    let produced = inflate_into(raw, scratch).expect("inflate_into");
    let dt = t0.elapsed().as_secs_f64();
    assert_eq!(produced, expect.len(), "inflate_into length mismatch");
    assert!(
        scratch.get(..produced) == Some(expect),
        "inflate_into bytes differ from the original"
    );
    if dt <= 0.0 {
        return 0.0;
    }
    (expect.len() as f64 / (1024.0 * 1024.0)) / dt
}

/// Run `f`, check its output against `expect`, and return MB/s.
fn time_mbs<F: FnOnce() -> Vec<u8>>(expect: &[u8], f: F) -> f64 {
    let t0 = Instant::now();
    let out = f();
    let dt = t0.elapsed().as_secs_f64();
    assert_eq!(out.len(), expect.len(), "decoded length mismatch");
    assert!(out == expect, "decoded bytes differ from the original");
    std::hint::black_box(&out);
    if dt <= 0.0 {
        return 0.0;
    }
    (expect.len() as f64 / (1024.0 * 1024.0)) / dt
}

/// Worst arm-over-reference ratio of a row.
fn ratio_of_worst(arms: &[f64], reference: f64) -> f64 {
    if reference <= 0.0 {
        return f64::MAX;
    }
    let mut lo = f64::MAX;
    for value in arms {
        lo = lo.min(value / reference);
    }
    lo
}

/// The maximum of a sample: on a shared machine this estimates uncontended
/// throughput far better than the median, because the best round is the one
/// that got a whole performance core.
fn best_of(values: &[f64]) -> f64 {
    values.iter().copied().fold(0.0f64, f64::max)
}

fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// Best-effort 1-minute load average, for the header line.
fn load_average() -> String {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        return text
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
    }
    Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.trim()
                .trim_matches(|c| c == '{' || c == '}')
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

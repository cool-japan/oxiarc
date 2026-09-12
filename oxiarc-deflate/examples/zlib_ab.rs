//! Criterion-free A/B table: `oxiarc-deflate` versus CPython's `zlib`.
//!
//! Prints two tables:
//!
//! 1. **Ratio** — compressed size at every level 1..=9 for each corpus, next to
//!    `zlib.compress(data, level)`, with the percentage difference. The gate is
//!    one-sided: `oxiarc <= python * 1.03`.
//! 2. **Throughput** — MB/s per level for both encoders, measured with an
//!    interleaved A/B (python timing is taken *inside* python with
//!    `time.perf_counter()`, so process spawn cost never enters the number),
//!    plus a per-call-size sweep that shows the cost is proportional to the
//!    input for 1 KiB / 4 KiB / 32 KiB / 1 MiB `Deflater::deflate` calls.
//!
//! Run with:
//!
//! ```text
//! cargo run --release --example zlib_ab
//! ```
//!
//! Add an argument to restrict the corpus, e.g. `cargo run --release
//! --example zlib_ab -- html`.

#[path = "../tests/common/corpus.rs"]
mod corpus;

use oxiarc_deflate::{Deflater, zlib_compress};
use std::io::Write;
use std::process::Command;
use std::time::Instant;

fn main() {
    let filter = std::env::args().nth(1);
    let samples: Vec<corpus::Sample> = corpus::all_samples()
        .into_iter()
        .filter(|s| filter.as_deref().is_none_or(|f| s.name.contains(f)))
        .collect();

    let have_python = corpus::python3_available();
    if !have_python {
        println!("note: python3 with zlib not found - reference columns omitted\n");
    }

    println!("=== Compressed size: oxiarc_deflate::zlib_compress vs python zlib.compress ===");
    println!(
        "{:<26} {:>9} {:>3} {:>10} {:>10} {:>8}",
        "corpus", "raw", "lvl", "oxiarc", "python", "delta"
    );
    let mut worst = f64::MIN;
    let mut worst_where = String::new();
    for s in &samples {
        for level in 1u8..=9 {
            let ours = zlib_compress(&s.data, level).expect("zlib_compress");
            let theirs = corpus::python_zlib_compress(&s.data, level);
            let (ref_len, delta) = match &theirs {
                Some(t) => {
                    let d = (ours.len() as f64 - t.len() as f64) / t.len() as f64 * 100.0;
                    if d > worst {
                        worst = d;
                        worst_where = format!("{} level {}", s.name, level);
                    }
                    (t.len().to_string(), format!("{d:+.2}%"))
                }
                None => ("-".to_string(), "-".to_string()),
            };
            println!(
                "{:<26} {:>9} {:>3} {:>10} {:>10} {:>8}",
                s.name,
                s.data.len(),
                level,
                ours.len(),
                ref_len,
                delta
            );
        }
    }
    if have_python {
        println!("\nworst (largest) delta: {worst:+.2}% at {worst_where}");
    }

    println!("\n=== Optimal parser (opt-in) vs the default ladder ===");
    println!(
        "{:<26} {:>9} {:>9} {:>8} {:>9} {:>9} {:>8}",
        "corpus", "L6", "opt(6)", "delta", "L9", "opt(9)", "delta"
    );
    let mut worst_opt = f64::MIN;
    let mut worst_opt_where = String::new();
    for s in &samples {
        // The DP is much slower than the ladder; a 96 KiB slice of every
        // corpus keeps this table runnable in seconds while still covering
        // each data shape.
        let data = &s.data[..s.data.len().min(96 * 1024)];
        let mut row = [0usize; 4];
        for (slot, (level, optimal)) in
            row.iter_mut()
                .zip([(6u8, false), (6, true), (9, false), (9, true)])
        {
            let mut d = if optimal {
                Deflater::with_optimal_parsing(level)
            } else {
                Deflater::new(level)
            };
            *slot = d.compress_to_vec(data).expect("deflate").len();
        }
        let d6 = (row[1] as f64 - row[0] as f64) / row[0] as f64 * 100.0;
        let d9 = (row[3] as f64 - row[2] as f64) / row[2] as f64 * 100.0;
        if d9 > worst_opt {
            worst_opt = d9;
            worst_opt_where = s.name.to_string();
        }
        println!(
            "{:<26} {:>9} {:>9} {:>7.2}% {:>9} {:>9} {:>7.2}%",
            s.name, row[0], row[1], d6, row[2], row[3], d9
        );
    }
    println!("worst optimal-vs-level-9 delta: {worst_opt:+.2}% at {worst_opt_where}");

    println!("\n=== Throughput (MB/s, interleaved A/B, best of 3) ===");
    println!(
        "{:<26} {:>3} {:>12} {:>12} {:>8}",
        "corpus", "lvl", "oxiarc", "python", "ratio"
    );
    for s in &samples {
        if s.data.len() < 64 * 1024 {
            continue;
        }
        for level in [1u8, 6, 9] {
            let mut ours_best = 0.0f64;
            let mut theirs_best = 0.0f64;
            for _ in 0..3 {
                let t0 = Instant::now();
                let out = zlib_compress(&s.data, level).expect("zlib_compress");
                let dt = t0.elapsed().as_secs_f64();
                std::hint::black_box(&out);
                let mbs = (s.data.len() as f64 / (1024.0 * 1024.0)) / dt;
                ours_best = ours_best.max(mbs);
                if let Some(t) = python_throughput(&s.data, level) {
                    theirs_best = theirs_best.max(t);
                }
            }
            let ratio = if theirs_best > 0.0 {
                format!("{:.2}x", ours_best / theirs_best)
            } else {
                "-".to_string()
            };
            println!(
                "{:<26} {:>3} {:>12.1} {:>12.1} {:>8}",
                s.name, level, ours_best, theirs_best, ratio
            );
        }
    }

    println!("\n=== Per-call cost sweep (level 6, no cliff expected) ===");
    let big = corpus::rust_sources(4 * 1024 * 1024);
    let big = if big.len() >= 1 << 21 {
        big
    } else {
        let mut v = big.clone();
        while v.len() < (1 << 21) {
            let more = v.clone();
            v.extend_from_slice(&more);
        }
        v.truncate(1 << 21);
        v
    };
    println!(
        "{:>10} {:>8} {:>12} {:>14} {:>10}",
        "call size", "calls", "ms", "MB/s", "out bytes"
    );
    for chunk in [1024usize, 4096, 32768, 1024 * 1024] {
        let t0 = Instant::now();
        let mut d = Deflater::new(6);
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        let mut calls = 0;
        while i < big.len() {
            let end = (i + chunk).min(big.len());
            let last = end == big.len();
            d.deflate(&big[i..end], &mut out, last).expect("deflate");
            calls += 1;
            i = end;
        }
        out.flush().ok();
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "{:>10} {:>8} {:>12.1} {:>14.1} {:>10}",
            chunk,
            calls,
            dt * 1000.0,
            (big.len() as f64 / (1024.0 * 1024.0)) / dt,
            out.len()
        );
    }
}

/// Time `zlib.compress` inside python and return MB/s.
fn python_throughput(data: &[u8], level: u8) -> Option<f64> {
    let path = corpus::temp_path("tp");
    std::fs::write(&path, data).ok()?;
    let script = "import sys, zlib, time\n\
        data = open(sys.argv[1],'rb').read()\n\
        lvl = int(sys.argv[2])\n\
        best = 0.0\n\
        for _ in range(3):\n\
        \tt0 = time.perf_counter()\n\
        \tzlib.compress(data, lvl)\n\
        \tdt = time.perf_counter() - t0\n\
        \tbest = max(best, (len(data)/1048576.0)/dt)\n\
        print('%.6f' % best)\n";
    let out = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&path)
        .arg(level.to_string())
        .output()
        .ok()?;
    let _ = std::fs::remove_file(&path);
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
}

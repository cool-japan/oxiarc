//! Differential oracle tests against the reference `brotli` CLI, in BOTH
//! directions:
//!
//! 1. **Decode direction** (the primary real-world requirement):
//!    reference-`brotli`-compressed streams across the quality (0..=11) and
//!    window (10..=24) grid must decode byte-identically with OxiArc. Any
//!    `Ok` with wrong bytes (silent corruption) is an immediate failure.
//! 2. **Encode direction**: OxiArc-compressed streams must be accepted by
//!    `brotli -d` and decode back to the original bytes exactly.
//!
//! Gated behind the `brotli-oracle` feature. Each test self-skips (prints a
//! note, does not fail) if `brotli` is not found on PATH, following the
//! `lha-oracle` pattern used elsewhere in the workspace.
#![cfg(feature = "brotli-oracle")]

use std::path::{Path, PathBuf};
use std::process::Command;

use oxiarc_brotli::{BrotliParams, compress_with_params, decompress};

/// Locate the `brotli` binary via `which`. Returns `None` if not found.
fn find_brotli() -> Option<PathBuf> {
    let output = Command::new("which").arg("brotli").output().ok()?;
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

/// Unique scratch directory under `std::env::temp_dir()`.
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_brotli_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Deterministic pseudo-random bytes (splitmix-style).
fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

/// Dictionary-rich synthetic English text (words common in the RFC 7932
/// static dictionary, so reference q>=5 output contains dictionary
/// references and word transforms).
fn english_text(len: usize) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "time",
        "down",
        "life",
        "left",
        "back",
        "code",
        "data",
        "show",
        "only",
        "site",
        "city",
        "open",
        "just",
        "like",
        "free",
        "work",
        "text",
        "year",
        "over",
        "body",
        "love",
        "form",
        "book",
        "play",
        "live",
        "line",
        "help",
        "home",
        "side",
        "more",
        "word",
        "long",
        "them",
        "view",
        "find",
        "page",
        "days",
        "full",
        "head",
        "term",
        "each",
        "area",
        "from",
        "true",
        "mark",
        "able",
        "upon",
        "high",
        "date",
        "land",
        "news",
        "even",
        "next",
        "case",
        "both",
        "post",
        "used",
        "made",
        "hand",
        "here",
        "what",
        "name",
        "the",
        "people",
        "should",
        "public",
        "information",
        "development",
        "world",
    ];
    let mut out = Vec::with_capacity(len + 16);
    let mut i = 0usize;
    while out.len() < len {
        out.extend_from_slice(WORDS[i % WORDS.len()].as_bytes());
        match i % 11 {
            10 => out.extend_from_slice(b". The "),
            4 => out.extend_from_slice(b", "),
            _ => out.push(b' '),
        }
        i += 1;
    }
    out.truncate(len);
    out
}

/// The shared corpus: diverse shapes and sizes crossing block boundaries.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![0x41]),
        ("one_zero", vec![0x00]),
        ("zeros_64", vec![0u8; 64]),
        ("zeros_100k", vec![0u8; 100_000]),
        ("rep_abc", b"abc".repeat(3000)),
        (
            "rep_sentence",
            b"The quick brown fox jumps over the lazy dog. ".repeat(700),
        ),
        ("text_10k", english_text(10_000)),
        ("text_300k", english_text(300_000)),
        (
            "utf8",
            "こんにちは世界。Компрессия данных très bien. "
                .repeat(400)
                .into_bytes(),
        ),
        ("random_1k", random_bytes(1024, 42)),
        ("random_64k", random_bytes(65536, 43)),
        ("sz_65535", random_bytes(65535, 44)),
        ("sz_65537", random_bytes(65537, 45)),
        (
            "inc_u32",
            (0u32..40_000).flat_map(|i| i.to_le_bytes()).collect(),
        ),
        ("bytes_0_255", (0u8..=255).cycle().take(4096).collect()),
        ("text_1m5", english_text(1_500_000)),
    ]
}

/// Reference-compress `data` with the CLI at (quality, lgwin).
fn reference_compress(brotli: &Path, dir: &Path, data: &[u8], q: u32, w: u32) -> Vec<u8> {
    let input = dir.join("in.bin");
    let output = dir.join("in.bin.br");
    std::fs::write(&input, data).expect("write input");
    let _ = std::fs::remove_file(&output);
    let status = Command::new(brotli)
        .arg("-f")
        .arg("-k")
        .arg("-q")
        .arg(q.to_string())
        .arg("-w")
        .arg(w.to_string())
        .arg(&input)
        .status()
        .expect("spawn brotli");
    assert!(status.success(), "reference brotli -q {q} -w {w} failed");
    std::fs::read(&output).expect("read reference output")
}

/// Reference-decompress with the CLI; returns `None` if rejected.
fn reference_decompress(brotli: &Path, dir: &Path, compressed: &[u8]) -> Option<Vec<u8>> {
    let input = dir.join("oxi.br");
    let output = dir.join("oxi");
    std::fs::write(&input, compressed).expect("write compressed");
    let _ = std::fs::remove_file(&output);
    let status = Command::new(brotli)
        .arg("-d")
        .arg("-f")
        .arg("-k")
        .arg(&input)
        .status()
        .expect("spawn brotli -d");
    if !status.success() {
        return None;
    }
    Some(std::fs::read(&output).expect("read decompressed output"))
}

/// Decode direction: every reference stream must decode byte-identically.
/// Zero tolerance for errors AND for silent mismatches.
#[test]
fn test_oracle_reference_encode_oxiarc_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("dec");

    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in corpus() {
        // Full quality sweep at the default window; window sweep at two
        // representative qualities. Big inputs use a reduced grid to keep
        // the test fast.
        let big = data.len() > 200_000;
        let qualities: &[u32] = if big {
            &[1, 5, 9, 11]
        } else {
            &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
        };
        for &q in qualities {
            let windows: &[u32] = if big || !(q == 5 || q == 11) {
                &[22]
            } else {
                &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24]
            };
            for &w in windows {
                let compressed = reference_compress(&brotli, &dir, &data, q, w);
                total += 1;
                match decompress(&compressed) {
                    Ok(ref decoded) if *decoded == data => {}
                    Ok(decoded) => failures.push(format!(
                        "SILENT MISMATCH {name} q{q} w{w}: {} != {} bytes",
                        decoded.len(),
                        data.len()
                    )),
                    Err(e) => failures.push(format!("ERROR {name} q{q} w{w}: {e}")),
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "decode direction: {}/{total} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("[brotli-oracle] decode direction: {total}/{total} reference streams byte-identical");
}

/// Encode direction: every OxiArc stream must be accepted by `brotli -d`
/// and decode back to the original bytes.
#[test]
fn test_oracle_oxiarc_encode_reference_decode() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("enc");

    let mut total = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for (name, data) in corpus() {
        let big = data.len() > 200_000;
        let qualities: &[u32] = if big {
            &[0, 5, 11]
        } else {
            &[0, 1, 2, 5, 6, 9, 11]
        };
        for &q in qualities {
            let windows: &[u32] = if big { &[22] } else { &[10, 16, 22, 24] };
            for &w in windows {
                let params = BrotliParams {
                    quality: q,
                    lgwin: w,
                    lgblock: 0,
                };
                let compressed = compress_with_params(&data, &params)
                    .unwrap_or_else(|e| panic!("compress {name} q{q} w{w}: {e}"));
                total += 1;
                match reference_decompress(&brotli, &dir, &compressed) {
                    Some(ref decoded) if *decoded == data => {}
                    Some(decoded) => failures.push(format!(
                        "MISMATCH {name} q{q} w{w}: reference decoded {} != {} bytes",
                        decoded.len(),
                        data.len()
                    )),
                    None => failures.push(format!("REJECTED {name} q{q} w{w} by brotli -d")),
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        failures.is_empty(),
        "encode direction: {}/{total} failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!(
        "[brotli-oracle] encode direction: {total}/{total} OxiArc streams accepted by brotli -d"
    );
}

/// Multi-meta-block boundary: force small lgblock so multiple compressed
/// meta-blocks are emitted, and verify the reference decoder agrees.
#[test]
fn test_oracle_multiblock_streams() {
    let Some(brotli) = find_brotli() else {
        eprintln!("[brotli-oracle] `brotli` not on PATH; skipping (not a failure)");
        return;
    };
    let dir = scratch_dir("multi");

    let data = english_text(300_000);
    for q in [1u32, 5, 9] {
        let params = BrotliParams {
            quality: q,
            lgwin: 22,
            lgblock: 16, // 64 KiB meta-blocks -> 5 compressed blocks
        };
        let compressed = compress_with_params(&data, &params).expect("compress");
        let decoded = reference_decompress(&brotli, &dir, &compressed)
            .unwrap_or_else(|| panic!("multi-block q{q} rejected by brotli -d"));
        assert_eq!(decoded, data, "multi-block q{q} content mismatch");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

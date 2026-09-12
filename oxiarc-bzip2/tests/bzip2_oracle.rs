//! Differential interop tests against the reference `bzip2` CLI (libbz2).
//!
//! Both directions are exercised, because self round-trips alone cannot
//! detect a private dialect:
//! 1. reference-compress -> oxiarc-decode must be byte-identical, and
//! 2. oxiarc-compress -> reference-decode must be byte-identical,
//!
//! over a battery of inputs (empty, tiny, runs, random, text, structured
//! binary, block-boundary sizes) and both compression levels 1 and 9.
//! Multi-stream concatenation and the legacy randomised-block fixture are
//! also cross-checked.
//!
//! Gated behind the `bzip2-oracle` feature. Each test self-skips (prints a
//! note, does not fail) if `bzip2` is not found on PATH, following the
//! `lha-oracle` pattern in `oxiarc-lzhuf`.
#![cfg(feature = "bzip2-oracle")]

use oxiarc_bzip2::{CompressionLevel, compress, decompress};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Legacy randomised-block fixture, shared with the always-on suite.
const RANDOMISED_REP_L1: &[u8] = include_bytes!("data/randomised_rep_l1.bz2");

/// Locate the `bzip2` binary via `which`. Returns `None` if not found (or
/// if `which` itself is unavailable), in which case callers must self-skip.
fn find_bzip2() -> Option<PathBuf> {
    // Probe the bare name first and use it as-is when it spawns:
    // `which` does not exist on Windows outside a POSIX shell (the
    // oracle would silently self-skip there), and inside one — MSYS /
    // Git Bash — it prints a POSIX path such as `/mingw64/bin/...`
    // that `CreateProcess` cannot open (the oracle would then panic
    // on spawn instead of running). Letting the OS resolve the name
    // avoids both. Only spawnability is checked, not the exit status.
    if Command::new("bzip2").arg("--version").output().is_ok() {
        return Some(PathBuf::from("bzip2"));
    }
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(locator).arg("bzip2").output().ok()?;
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

/// Unique scratch directory per test invocation.
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_bzip2_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Run `bzip2 <flag> -c` over `input` via temp files, returning stdout.
fn run_bzip2(bzip2: &Path, dir: &Path, label: &str, flag: &str, input: &[u8]) -> Vec<u8> {
    let in_path = dir.join(format!("{label}.in"));
    std::fs::write(&in_path, input).expect("write oracle input");
    let in_file = std::fs::File::open(&in_path).expect("open oracle input");
    let output = Command::new(bzip2)
        .arg(flag)
        .arg("-c")
        .stdin(in_file)
        .output()
        .expect("spawn bzip2");
    assert!(
        output.status.success(),
        "[{label}] `bzip2 {flag}` failed (exit {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Deterministic xorshift32 byte stream (matches `interop_bzip2.rs`).
fn xorshift_data(size: usize) -> Vec<u8> {
    let mut state: u32 = 0x9E37_79B9;
    let mut out = Vec::with_capacity(size + 4);
    while out.len() < size {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(size);
    out
}

/// Deterministic text stream (matches `interop_bzip2.rs`).
fn text_data(size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size + 80);
    let mut i = 0usize;
    while out.len() < size {
        out.extend_from_slice(
            format!("line {i:07}: the quick brown fox jumps over the lazy dog 0123456789\n")
                .as_bytes(),
        );
        i += 1;
    }
    out.truncate(size);
    out
}

/// Structured binary: incrementing little-endian u32s (the audit's +50%
/// multi-table ratio case).
fn incrementing_u32(count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(count * 4);
    for i in 0..count as u32 {
        out.extend_from_slice(&i.to_le_bytes());
    }
    out
}

/// The differential input battery: name + payload.
fn battery() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![0x42]),
        ("all_zeros_256k", vec![0u8; 256 * 1024]),
        ("repetitive", b"abcabcabc".repeat(20_000)),
        ("random_64k", xorshift_data(64 * 1024)),
        ("text_100k_plus_1", text_data(100_001)),
        ("structured_u32_400k", incrementing_u32(100_000)),
        ("text_1m2_multiblock", text_data(1_200_000)),
    ]
}

#[test]
fn oracle_reference_compress_oxiarc_decode() {
    let Some(bzip2) = find_bzip2() else {
        eprintln!("[bzip2-oracle] `bzip2` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("ref_to_oxiarc");
    let mut cases = 0usize;
    for (name, data) in battery() {
        for level in [1u8, 9u8] {
            let flag = format!("-{level}");
            let compressed = run_bzip2(&bzip2, &dir, name, &flag, &data);
            let decoded = decompress(&compressed[..])
                .unwrap_or_else(|e| panic!("[{name} {flag}] oxiarc decode failed: {e}"));
            assert_eq!(decoded, data, "[{name} {flag}] decode mismatch");
            cases += 1;
        }
    }
    eprintln!("[bzip2-oracle] reference->oxiarc: {cases}/{cases} byte-identical");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_oxiarc_compress_reference_decode() {
    let Some(bzip2) = find_bzip2() else {
        eprintln!("[bzip2-oracle] `bzip2` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("oxiarc_to_ref");
    let mut cases = 0usize;
    for (name, data) in battery() {
        for level in [1u8, 9u8] {
            let compressed = compress(&data, CompressionLevel::new(level))
                .unwrap_or_else(|e| panic!("[{name} -{level}] oxiarc compress failed: {e}"));
            let decoded = run_bzip2(&bzip2, &dir, name, "-d", &compressed);
            assert_eq!(decoded, data, "[{name} -{level}] `bzip2 -d` mismatch");
            cases += 1;
        }
    }
    eprintln!("[bzip2-oracle] oxiarc->reference: {cases}/{cases} accepted, byte-identical");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_multistream_reference_streams_oxiarc_decodes_all() {
    let Some(bzip2) = find_bzip2() else {
        eprintln!("[bzip2-oracle] `bzip2` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("multistream_ref");
    let a = text_data(30_000);
    let b = xorshift_data(20_000);
    let mut cat = run_bzip2(&bzip2, &dir, "ms_a", "-9", &a);
    cat.extend_from_slice(&run_bzip2(&bzip2, &dir, "ms_b", "-1", &b));

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    let decoded = decompress(&cat[..]).expect("oxiarc decode of concatenated CLI streams");
    assert_eq!(
        decoded, expected,
        "multi-stream decode must cover ALL streams"
    );
    eprintln!(
        "[bzip2-oracle] cat(a.bz2, b.bz2): {} + {} bytes decoded in full",
        a.len(),
        b.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_multistream_oxiarc_streams_reference_decodes_all() {
    let Some(bzip2) = find_bzip2() else {
        eprintln!("[bzip2-oracle] `bzip2` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("multistream_oxiarc");
    let a = b"oxiarc multi-stream member A\n".repeat(500);
    let b = b"member B at a different level\n".repeat(300);
    let mut cat = compress(&a, CompressionLevel::new(9)).expect("compress member A");
    cat.extend_from_slice(&compress(&b, CompressionLevel::new(1)).expect("compress member B"));

    let mut expected = a.clone();
    expected.extend_from_slice(&b);
    let decoded = run_bzip2(&bzip2, &dir, "ms_oxiarc", "-d", &cat);
    assert_eq!(
        decoded, expected,
        "`bzip2 -d` must accept oxiarc concatenation"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_randomised_fixture_cross_check() {
    let Some(bzip2) = find_bzip2() else {
        eprintln!("[bzip2-oracle] `bzip2` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("randomised");
    let expected = b"repetitive repetitive repetitive!\n".repeat(64);
    let reference = run_bzip2(&bzip2, &dir, "rand_fixture", "-d", RANDOMISED_REP_L1);
    assert_eq!(
        reference, expected,
        "`bzip2 -d` disagrees on randomised fixture"
    );
    let ours = decompress(RANDOMISED_REP_L1).expect("oxiarc decode of randomised fixture");
    assert_eq!(
        ours, reference,
        "oxiarc and reference disagree on randomised fixture"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_ratio_report_structured_data() {
    let Some(bzip2) = find_bzip2() else {
        eprintln!("[bzip2-oracle] `bzip2` not on PATH; skipping (self-skip, not a failure)");
        return;
    };
    let dir = scratch_dir("ratio");
    let data = incrementing_u32(100_000); // 400 KB structured binary
    let ours = compress(&data, CompressionLevel::new(9)).expect("oxiarc compress");
    let reference = run_bzip2(&bzip2, &dir, "ratio", "-9", &data);
    eprintln!(
        "[bzip2-oracle] structured 400KB: oxiarc {} bytes vs bzip2 -9 {} bytes ({:.1}% of reference)",
        ours.len(),
        reference.len(),
        100.0 * ours.len() as f64 / reference.len() as f64
    );
    // Multi-table clustering must keep us in the reference's ballpark; the
    // pre-fix single-table encoder was ~150% of reference on this input.
    assert!(
        ours.len() <= reference.len() * 115 / 100,
        "oxiarc output ({}) drifted above 115% of reference ({})",
        ours.len(),
        reference.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

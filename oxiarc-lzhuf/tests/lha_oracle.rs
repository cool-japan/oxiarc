//! Encode-direction oracle tests: validate OxiArc-produced LZH archives
//! against the real `lha` CLI (Lhasa), which is decompress-only (it can
//! list/test/extract but has no create/add subcommand — see
//! `tests/data/README.md` for the full provenance/rationale). This makes it
//! a one-way oracle: we cannot byte-compare OxiArc's compressed stream
//! against a reference (two conformant encoders may legitimately produce
//! different, equally-valid compressed bytes for the same input), but we
//! *can* verify that a real, independent LHA implementation successfully
//! reads what OxiArc writes — the correct proof for the encode direction.
//!
//! Gated behind the `lha-oracle` feature (which implies `parallel`, whose
//! [`lzh_compress_parallel`] builds the minimal single-entry test archive —
//! `oxiarc-lzhuf` already has full LHA level-1 header-writing capability via
//! that path, so no separate archive writer is needed here). Each test
//! self-skips (prints a note, does not fail) if `lha` is not found on PATH.
#![cfg(feature = "lha-oracle")]

use oxiarc_lzhuf::{LzhEntryInput, LzhMethod, lzh_compress_parallel};
use std::path::PathBuf;
use std::process::Command;

/// Locate the `lha` binary via `which`. Returns `None` if not found (or if
/// `which` itself is unavailable), in which case callers must self-skip.
fn find_lha() -> Option<PathBuf> {
    // Probe the bare name first and use it as-is when it spawns:
    // `which` does not exist on Windows outside a POSIX shell (the
    // oracle would silently self-skip there), and inside one — MSYS /
    // Git Bash — it prints a POSIX path such as `/mingw64/bin/...`
    // that `CreateProcess` cannot open (the oracle would then panic
    // on spawn instead of running). Letting the OS resolve the name
    // avoids both. Only spawnability is checked, not the exit status.
    if Command::new("lha").arg("--version").output().is_ok() {
        return Some(PathBuf::from("lha"));
    }
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(locator).arg("lha").output().ok()?;
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

/// Build a single-entry archive from `payload`, then verify a real `lha`
/// binary can `t`(est) and `x`(tract) it, with extracted bytes matching
/// `payload` exactly. Self-skips (prints a note, returns without failing) if
/// `lha` is not on PATH.
fn oracle_roundtrip(label: &str, payload: &[u8]) {
    if find_lha().is_none() {
        eprintln!(
            "[lha-oracle] `lha` not found on PATH; skipping '{label}' (self-skip, not a failure)"
        );
        return;
    }

    let entry_name = "payload.bin";
    let entries = [LzhEntryInput {
        name: entry_name,
        data: payload,
    }];
    let archive = lzh_compress_parallel(&entries, LzhMethod::Lh5)
        .unwrap_or_else(|e| panic!("[{label}] archive build failed: {e}"));

    // Unique scratch directory per test (label is unique per payload; PID and
    // thread id further guard against collisions across test runners that
    // share a process, e.g. `cargo test`'s thread-per-test model).
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_lha_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");

    let archive_path = dir.join("test.lzh");
    std::fs::write(&archive_path, &archive).expect("write archive");

    // `lha t`: CRC-test the archive we just wrote.
    let test_output = Command::new("lha")
        .arg("t")
        .arg(&archive_path)
        .output()
        .expect("spawn `lha t`");
    assert!(
        test_output.status.success(),
        "[{label}] `lha t` reported failure (exit {:?}): stdout={:?} stderr={:?}",
        test_output.status.code(),
        String::from_utf8_lossy(&test_output.stdout),
        String::from_utf8_lossy(&test_output.stderr)
    );

    // `lha x`: extract to a scratch subdirectory and diff against the input.
    let extract_dir = dir.join("extracted");
    std::fs::create_dir_all(&extract_dir).expect("create extract dir");
    let extract_output = Command::new("lha")
        .arg(format!("xw={}", extract_dir.display()))
        .arg(&archive_path)
        .output()
        .expect("spawn `lha x`");
    assert!(
        extract_output.status.success(),
        "[{label}] `lha x` reported failure (exit {:?}): stdout={:?} stderr={:?}",
        extract_output.status.code(),
        String::from_utf8_lossy(&extract_output.stdout),
        String::from_utf8_lossy(&extract_output.stderr)
    );

    let extracted_path = extract_dir.join(entry_name);
    let extracted = std::fs::read(&extracted_path)
        .unwrap_or_else(|e| panic!("[{label}] reading extracted file failed: {e}"));
    assert_eq!(
        extracted, payload,
        "[{label}] extracted bytes must match the original input exactly"
    );

    eprintln!(
        "[lha-oracle] '{label}': OK ({} bytes -> {} bytes compressed, lha t + lha x both succeeded)",
        payload.len(),
        archive.len()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_empty_payload() {
    oracle_roundtrip("empty", &[]);
}

#[test]
fn oracle_one_byte_payload() {
    oracle_roundtrip("one_byte", &[0x42]);
}

#[test]
fn oracle_repetitive_english_text() {
    let payload: Vec<u8> = b"The quick brown fox jumps over the lazy dog. "
        .iter()
        .cycle()
        .take(460)
        .copied()
        .collect();
    oracle_roundtrip("repetitive_english_text", &payload);
}

#[test]
fn oracle_large_corpus_plaintext_recompressed() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/lha213_lh5_long.expected"
    );
    let payload = std::fs::read(path).expect("read lha213_lh5_long.expected fixture");
    oracle_roundtrip("large_corpus_plaintext_recompressed", &payload);
}

#[test]
fn oracle_long_single_byte_run() {
    let payload = vec![0x58u8; 10_000];
    oracle_roundtrip("long_single_byte_run", &payload);
}

#[test]
fn oracle_all_256_byte_values() {
    let payload: Vec<u8> = (0u16..=255).map(|v| v as u8).collect();
    oracle_roundtrip("all_256_byte_values", &payload);
}

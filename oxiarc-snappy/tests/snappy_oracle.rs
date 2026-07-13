//! Differential oracle tests for the Snappy framing format, validating that
//! SNAPPY-01's new per-chunk (64 KiB) and total-output caps do NOT regress
//! interop with genuine reference-produced streams — only out-of-spec or
//! malicious chunks are rejected (see `frame::tests::test_snappy01_*` in
//! `src/frame.rs` for the malicious-input side of that guarantee).
//!
//! The oracle is `cramjam` (a Rust-backed Python binding that implements the
//! same Snappy block + framing + CRC32C spec as Google's canonical C++
//! `snappy`), invoked out-of-process via `python3`. Both directions are
//! checked:
//!   1. reference-compress (`cramjam.snappy.compress`) -> oxiarc-decode
//!      (`FrameDecoder`) must be byte-identical.
//!   2. oxiarc-compress (`FrameEncoder`) -> reference-decode
//!      (`cramjam.snappy.decompress`) must be byte-identical.
//!
//! Gated behind the `snappy-oracle` feature. Each test self-skips (prints a
//! note, does not fail) if `python3` or its `cramjam` module is unavailable.
#![cfg(feature = "snappy-oracle")]

use oxiarc_snappy::{FrameDecoder, FrameEncoder};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;

/// True if `python3 -c "import cramjam"` succeeds.
fn cramjam_available() -> bool {
    Command::new("python3")
        .args(["-c", "import cramjam"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Build a unique scratch file path under the process/thread-specific temp
/// directory (never a hardcoded absolute path).
fn temp_path(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "oxiarc_snappy_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    p
}

/// Run `cramjam.snappy.compress(data)` via `python3`, returning the framed
/// bytes. `None` if the python invocation fails for any reason.
fn cramjam_compress(data: &[u8]) -> Option<Vec<u8>> {
    let in_path = temp_path("compress_in");
    let out_path = temp_path("compress_out");
    std::fs::write(&in_path, data).ok()?;

    let script = r#"
import sys, cramjam
with open(sys.argv[1], 'rb') as f:
    data = f.read()
out = bytes(cramjam.snappy.compress(data))
with open(sys.argv[2], 'wb') as f:
    f.write(out)
"#;
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&in_path)
        .arg(&out_path)
        .status()
        .ok()?;

    let result = if status.success() {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
    result
}

/// Run `cramjam.snappy.decompress(data)` via `python3`, returning the
/// decompressed bytes. `None` if the python invocation fails for any reason
/// (including the reference decoder rejecting the input).
fn cramjam_decompress(data: &[u8]) -> Option<Vec<u8>> {
    let in_path = temp_path("decompress_in");
    let out_path = temp_path("decompress_out");
    std::fs::write(&in_path, data).ok()?;

    let script = r#"
import sys, cramjam
with open(sys.argv[1], 'rb') as f:
    data = f.read()
out = bytes(cramjam.snappy.decompress(data))
with open(sys.argv[2], 'wb') as f:
    f.write(out)
"#;
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&in_path)
        .arg(&out_path)
        .status()
        .ok()?;

    let result = if status.success() {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
    result
}

/// Representative sizes: empty, tiny, and both sides of the framing
/// format's 64 KiB per-chunk boundary (single-chunk, multi-chunk, and a
/// size that lands mid-chunk on the final partial chunk).
const SIZES: &[usize] = &[0, 1, 13, 100, 4096, 65535, 65536, 65537, 131_072, 200_000];

/// Deterministic, non-trivially-compressible byte pattern of a given size
/// (mixes a cycling ramp with occasional repeats so both the literal and
/// copy code paths are exercised).
fn pattern(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| if i % 37 == 0 { 0x42 } else { (i % 251) as u8 })
        .collect()
}

/// Direction 1: `cramjam.snappy.compress` output must decode byte-identical
/// via oxiarc's `FrameDecoder`.
#[test]
fn test_oracle_reference_compress_oxiarc_decode() {
    if !cramjam_available() {
        eprintln!(
            "[snappy-oracle] python3 `cramjam` module not available; skipping \
             test_oracle_reference_compress_oxiarc_decode (self-skip, not a failure)"
        );
        return;
    }

    let mut checked = 0usize;
    for &size in SIZES {
        let data = pattern(size);
        let Some(framed) = cramjam_compress(&data) else {
            eprintln!(
                "[snappy-oracle] cramjam_compress failed for size {size}; skipping this size"
            );
            continue;
        };

        let mut decoder = FrameDecoder::new(&framed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap_or_else(|e| {
            panic!("oxiarc FrameDecoder rejected a cramjam-produced frame (size={size}): {e}")
        });
        assert_eq!(
            output, data,
            "size={size}: oxiarc decode of cramjam-compressed frame mismatched"
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no sizes were successfully checked against the cramjam oracle"
    );
    eprintln!(
        "[snappy-oracle] reference-compress -> oxiarc-decode: {checked}/{} sizes byte-identical",
        SIZES.len()
    );
}

/// Direction 2: oxiarc's `FrameEncoder` output must decode byte-identical
/// via `cramjam.snappy.decompress`.
#[test]
fn test_oracle_oxiarc_compress_reference_decode() {
    if !cramjam_available() {
        eprintln!(
            "[snappy-oracle] python3 `cramjam` module not available; skipping \
             test_oracle_oxiarc_compress_reference_decode (self-skip, not a failure)"
        );
        return;
    }

    let mut checked = 0usize;
    for &size in SIZES {
        let data = pattern(size);

        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::new(&mut compressed);
            encoder
                .write_all(&data)
                .unwrap_or_else(|e| panic!("oxiarc encode failed for size {size}: {e}"));
            encoder
                .finish()
                .unwrap_or_else(|e| panic!("oxiarc finish failed for size {size}: {e}"));
        }

        let Some(decoded) = cramjam_decompress(&compressed) else {
            panic!(
                "cramjam (reference) rejected an oxiarc-produced frame for size {size} — \
                 interop regression"
            );
        };
        assert_eq!(
            decoded, data,
            "size={size}: cramjam decode of oxiarc-compressed frame mismatched"
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no sizes were successfully checked against the cramjam oracle"
    );
    eprintln!(
        "[snappy-oracle] oxiarc-compress -> reference-decode: {checked}/{} sizes byte-identical",
        SIZES.len()
    );
}

/// Sanity check specifically for SNAPPY-01: a *reference-produced* frame
/// whose chunks sit exactly at the 64 KiB boundary must still decode fully
/// under oxiarc's new per-chunk cap (proving the cap targets out-of-spec
/// chunks only, not legitimate reference output).
#[test]
fn test_oracle_boundary_chunk_sizes_unaffected_by_snappy01_caps() {
    if !cramjam_available() {
        eprintln!(
            "[snappy-oracle] python3 `cramjam` module not available; skipping \
             test_oracle_boundary_chunk_sizes_unaffected_by_snappy01_caps (self-skip)"
        );
        return;
    }

    // Exactly 3 full 64 KiB chunks of incompressible-ish data, produced by
    // the reference implementation.
    let data = pattern(3 * 65536);
    let Some(framed) = cramjam_compress(&data) else {
        eprintln!("[snappy-oracle] cramjam_compress failed; skipping");
        return;
    };

    let mut decoder = FrameDecoder::new(&framed[..]);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .expect("a reference-produced 3x64KiB-chunk frame must decode after SNAPPY-01 hardening");
    assert_eq!(output, data);
}

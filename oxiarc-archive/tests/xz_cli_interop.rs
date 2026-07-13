//! Live XZ differential tests against XZ Utils (`xz`) — feature `xz-oracle`.
//!
//! * `xz -6` output whose block spans multiple LZMA2 chunks (the shape
//!   that previously failed at the archive level before the persistent
//!   `Lzma2Decoder` position fix) must decode byte-exactly.
//! * `xz --block-size` output with several *blocks* (multi-record index)
//!   must decode byte-exactly.
//! * oxiarc-written `.xz` must pass `xz -t` and round-trip via `xz -dc`.
//!
//! Every test self-skips with a printed note when `xz` is not on PATH,
//! mirroring the `lha-oracle` pattern, and uses a unique temp directory
//! per test so the default parallel runner cannot race.

#![cfg(feature = "xz-oracle")]

use oxiarc_archive::xz;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_xz_oracle_{}_{}_{}",
        test_name,
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).expect("create unique temp dir");
    dir
}

fn xz_available() -> bool {
    let ok = Command::new("xz")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !ok {
        eprintln!("skipping xz-oracle test: `xz` not available on PATH");
    }
    ok
}

fn run_in(dir: &Path, tool: &str, args: &[&str]) -> std::process::Output {
    Command::new(tool)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{tool}`: {e}"))
}

/// Moderately-compressible deterministic payload: alternating 64-byte
/// xorshift64 (incompressible) and fixed-text (highly compressible)
/// blocks, so `xz -6` emits genuinely compressed LZMA chunks and, for
/// payloads whose compressed size exceeds 64 KiB, more than one chunk.
fn mixed_payload(total_len: usize) -> Vec<u8> {
    let pattern: Vec<u8> = b"OxiArc xz oracle: moderately compressible payload block. "
        .iter()
        .cycle()
        .take(64)
        .copied()
        .collect();
    let mut state = 0x1357_9BDF_2468_ACE0u64 | 1;
    let mut prng = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state.to_le_bytes()
    };
    let mut data = Vec::with_capacity(total_len + 64);
    let mut block = 0usize;
    while data.len() < total_len {
        if block % 2 == 0 {
            for _ in 0..8 {
                let bytes = prng();
                data.extend_from_slice(&bytes);
            }
        } else {
            data.extend_from_slice(&pattern);
        }
        block += 1;
    }
    data.truncate(total_len);
    data
}

#[test]
fn real_xz6_multichunk_block_decodes_byte_exact() {
    if !xz_available() {
        return;
    }
    let dir = unique_temp_dir("multichunk");
    // 256 KiB at ~0.5 ratio compresses to ~130 KiB > 64 KiB, guaranteeing
    // several LZMA2 chunks inside a single block.
    let original = mixed_payload(256 * 1024);
    fs::write(dir.join("input.bin"), &original).expect("write input");

    let out = run_in(&dir, "xz", &["-6", "-k", "input.bin"]);
    assert!(out.status.success(), "xz -6 failed: {out:?}");

    let compressed = fs::read(dir.join("input.bin.xz")).expect("read .xz");
    let decoded = xz::decompress(&mut std::io::Cursor::new(&compressed))
        .expect("decode real xz -6 output at the archive level");
    assert_eq!(decoded, original, "multi-chunk decode mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn real_xz_multiblock_stream_decodes_byte_exact() {
    if !xz_available() {
        return;
    }
    let dir = unique_temp_dir("multiblock");
    let original = mixed_payload(200 * 1024);
    fs::write(dir.join("input.bin"), &original).expect("write input");

    // Force multiple blocks (and a multi-record index + footer
    // Backward-Size cross-check on the read side).
    let out = run_in(&dir, "xz", &["-6", "-k", "--block-size=65536", "input.bin"]);
    assert!(out.status.success(), "xz --block-size failed: {out:?}");

    let compressed = fs::read(dir.join("input.bin.xz")).expect("read .xz");
    let decoded = xz::decompress(&mut std::io::Cursor::new(&compressed))
        .expect("decode real multi-block xz output");
    assert_eq!(decoded, original, "multi-block decode mismatch");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oxiarc_written_xz_accepted_and_round_tripped_by_xz_cli() {
    if !xz_available() {
        return;
    }
    let dir = unique_temp_dir("oxi_to_xz");
    let original = mixed_payload(96 * 1024);

    let compressed = xz::compress(&original, 6).expect("oxiarc xz compress");
    fs::write(dir.join("oxi.xz"), &compressed).expect("write oxi.xz");

    // Integrity test by the reference implementation.
    let out = run_in(&dir, "xz", &["-t", "oxi.xz"]);
    assert!(
        out.status.success(),
        "xz -t rejected oxiarc output: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Byte-exact round-trip through the reference decompressor.
    let out = run_in(&dir, "xz", &["-dc", "oxi.xz"]);
    assert!(out.status.success(), "xz -dc failed: {out:?}");
    assert_eq!(out.stdout, original, "xz -dc content mismatch");
    let _ = fs::remove_dir_all(&dir);
}

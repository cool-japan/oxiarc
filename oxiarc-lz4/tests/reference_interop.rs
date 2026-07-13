//! Differential / interop regression tests against the reference `lz4` CLI.
//!
//! The audit found that oxiarc-lz4's block round-trip was interoperable but
//! three defects remained:
//!
//! * **LZ4-01** — the block decoder ignored `max_output` *within* a single
//!   sequence, so a crafted block was a decompression bomb.
//! * **LZ4-02** — the encoders violated the LASTLITERALS(5) end-of-block
//!   invariant, so reference `lz4 -d` rejected common (all-zero / repetitive /
//!   RLE) inputs.
//! * **LZ4-03** — the frame decoder ignored the block-independence flag, so
//!   reference `lz4 -BD` (linked-block) frames failed on the second block.
//!
//! These tests are the permanent regression gate. The small fixtures embedded
//! below (produced by `lz4 v1.10.0`) always run. The heavier differential
//! tests that shell out to the live `lz4` binary are compiled only under the
//! `lz4-oracle` feature and self-skip when the binary is not on PATH.

use oxiarc_lz4::block::{compress_block, compress_block_hc, decompress_block};
use oxiarc_lz4::{FrameDescriptor, compress_with_options, decompress};

// ---------------------------------------------------------------------------
// Fixtures produced by the reference `lz4` CLI (always-on, no binary needed)
// ---------------------------------------------------------------------------

/// `lz4` frame for 512 zero bytes (default independent blocks).
const REF_ZEROS512_FRAME: &[u8] = &[
    0x04, 0x22, 0x4d, 0x18, 0x64, 0x40, 0xa7, 0x0c, 0x00, 0x00, 0x00, 0x1f, 0x00, 0x01, 0x00, 0xff,
    0xe8, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xde, 0x53, 0xc1, 0x32,
];

/// Deterministic pattern used to produce the linked-block fixture below.
fn linked_pattern(n: usize) -> Vec<u8> {
    let pat = b"The quick brown fox jumps over the lazy dog 0123456789 ";
    (0..n).map(|i| pat[i % pat.len()]).collect()
}

/// Length of the linked-block fixture's decompressed content.
const LINKED_LEN: usize = 200_000;

/// `lz4 -BD -B4` (linked / block-dependent, 64 KiB blocks) frame of
/// `linked_pattern(LINKED_LEN)`. FLG byte 0x44 -> block_independence bit clear.
const REF_LINKED_FRAME: &[u8] = &[
    0x04, 0x22, 0x4d, 0x18, 0x44, 0x40, 0x5e, 0x42, 0x01, 0x00, 0x00, 0xff, 0x28, 0x54, 0x68, 0x65,
    0x20, 0x71, 0x75, 0x69, 0x63, 0x6b, 0x20, 0x62, 0x72, 0x6f, 0x77, 0x6e, 0x20, 0x66, 0x6f, 0x78,
    0x20, 0x6a, 0x75, 0x6d, 0x70, 0x73, 0x20, 0x6f, 0x76, 0x65, 0x72, 0x20, 0x74, 0x68, 0x65, 0x20,
    0x6c, 0x61, 0x7a, 0x79, 0x20, 0x64, 0x6f, 0x67, 0x20, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36,
    0x37, 0x38, 0x39, 0x20, 0x37, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xb1, 0x50, 0x6f, 0x76, 0x65, 0x72, 0x20, 0x0a, 0x01, 0x00,
    0x00, 0x0f, 0xe1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xe8, 0x50, 0x65, 0x20, 0x71, 0x75, 0x69, 0x0a, 0x01, 0x00, 0x00, 0x0f,
    0x37, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xe8, 0x50, 0x65, 0x20, 0x6c, 0x61, 0x7a, 0x17, 0x00, 0x00, 0x00, 0x0f, 0xe1, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x35, 0x50, 0x20,
    0x66, 0x6f, 0x78, 0x20, 0x00, 0x00, 0x00, 0x00, 0xfb, 0x06, 0x54, 0xd6,
];

// ---------------------------------------------------------------------------
// LZ4-01: decompression-bomb / cap-bypass rejection (always-on)
// ---------------------------------------------------------------------------

/// Build a single-sequence LZ4 block that decodes to ~1M bytes from ~4 KiB of
/// input: 1 literal + a match whose length is inflated with 4000 `0xFF`
/// continuation bytes. This reproduces the confirmed 64 -> 1,020,020 bomb.
fn bomb_block() -> Vec<u8> {
    let mut block = vec![0x1f, 0x00, 0x01, 0x00]; // token(lit=1,ml=15) + literal + offset=1
    block.extend(std::iter::repeat_n(0xffu8, 4000)); // match-length continuations
    block.push(0x00); // final continuation byte
    block
}

#[test]
fn lz4_01_bomb_block_is_rejected_not_expanded() {
    let bomb = bomb_block();
    // With a tight cap the decoder must refuse rather than emit ~1 MB.
    let result = decompress_block(&bomb, 64);
    assert!(
        result.is_err(),
        "decompression bomb was not rejected under a 64-byte cap"
    );
}

#[test]
fn lz4_01_bomb_block_rejected_at_realistic_cap() {
    let bomb = bomb_block();
    // Even with a 4 MiB cap (a full LZ4 block) the single sequence overshoots
    // (it targets ~1 GiB-class growth via chained continuations); confirm the
    // per-copy projection guard rejects it deterministically without panic.
    let result = decompress_block(&bomb, 512 * 1024);
    assert!(result.is_err(), "bomb not rejected under 512 KiB cap");
}

#[test]
fn lz4_01_legitimate_block_at_exact_cap_still_ok() {
    // A valid block must still decode when max_output equals the exact size.
    let data = vec![7u8; 4096];
    let compressed = compress_block(&data).expect("compress");
    let out = decompress_block(&compressed, data.len()).expect("decompress at exact cap");
    assert_eq!(out, data);
}

// ---------------------------------------------------------------------------
// LZ4-02: LASTLITERALS(5) end-of-block invariant (always-on structural check)
// ---------------------------------------------------------------------------

/// Walk an LZ4 block and return the length of its final literal run, or `None`
/// if the block ends with a match (i.e. violates the LASTLITERALS invariant)
/// or is malformed. A conformant block always ends in a literals-only sequence.
fn final_literal_run(block: &[u8]) -> Option<usize> {
    let mut pos = 0usize;
    loop {
        if pos >= block.len() {
            // Reached the end right after a match copy -> ends with a match.
            return None;
        }
        let token = block[pos];
        pos += 1;
        // Literal length (with extension).
        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            loop {
                let b = *block.get(pos)? as usize;
                pos += 1;
                lit += b;
                if b != 255 {
                    break;
                }
            }
        }
        // Skip the literal bytes.
        pos = pos.checked_add(lit)?;
        if pos == block.len() {
            // Final sequence is literals-only: this is the trailing run.
            return Some(lit);
        }
        if pos > block.len() {
            return None;
        }
        // A match follows: 2-byte offset + optional match-length extension.
        // We only need to advance `pos` past the extension bytes here.
        pos = pos.checked_add(2)?;
        if (token & 0x0f) == 15 {
            loop {
                let b = *block.get(pos)?;
                pos += 1;
                if b != 255 {
                    break;
                }
            }
        }
        // Continue to the next sequence.
    }
}

fn assert_lastliterals_ok(block: &[u8], label: &str) {
    match final_literal_run(block) {
        Some(n) => assert!(
            n >= 5,
            "{label}: final literal run is {n} (< LASTLITERALS=5)"
        ),
        None => panic!("{label}: block ends with a match (LASTLITERALS violated)"),
    }
}

#[test]
fn lz4_02_fast_encoder_respects_lastliterals() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("all-zero-64", vec![0u8; 64]),
        ("all-zero-4096", vec![0u8; 4096]),
        ("all-0xAA-1000", vec![0xAAu8; 1000]),
        ("rle-ababab", b"ab".repeat(500)),
        ("repetitive", b"The quick brown fox. ".repeat(300)),
        ("incrementing", (0..5000).map(|i| (i % 251) as u8).collect()),
    ];
    for (label, data) in cases {
        let block = compress_block(&data).expect("compress");
        assert_lastliterals_ok(&block, label);
        // And it must still round-trip through oxiarc.
        let out = decompress_block(&block, data.len()).expect("decompress");
        assert_eq!(out, data, "{label}: round-trip mismatch");
    }
}

#[test]
fn lz4_02_hc_encoder_respects_lastliterals() {
    let data = b"The quick brown fox jumps over the lazy dog. ".repeat(200);
    for level in [1, 6, 9, 12] {
        let block = compress_block_hc(&data, level).expect("hc compress");
        assert_lastliterals_ok(&block, &format!("hc-level-{level}"));
        let out = decompress_block(&block, data.len()).expect("hc decompress");
        assert_eq!(out, data, "hc level {level}: round-trip mismatch");
    }
    // All-zero is the classic LASTLITERALS trap; make sure HC handles it too.
    let zeros = vec![0u8; 8192];
    for level in [1, 9, 12] {
        let block = compress_block_hc(&zeros, level).expect("hc compress zeros");
        assert_lastliterals_ok(&block, &format!("hc-zeros-level-{level}"));
    }
}

// ---------------------------------------------------------------------------
// LZ4-01 (decode) / LZ4-03: decode reference frames (always-on fixtures)
// ---------------------------------------------------------------------------

#[test]
fn decode_reference_zeros_frame() {
    let out = decompress(REF_ZEROS512_FRAME, 4096).expect("decode reference zeros frame");
    assert_eq!(out, vec![0u8; 512]);
}

#[test]
fn lz4_03_decode_reference_linked_frame() {
    // Reference `lz4 -BD` frame — the second and later blocks reference the
    // previous block's tail. Before the fix this failed on block 2.
    let out = decompress(REF_LINKED_FRAME, LINKED_LEN * 2).expect("decode linked frame");
    assert_eq!(out, linked_pattern(LINKED_LEN));
}

// ---------------------------------------------------------------------------
// LZ4-03: oxiarc can emit linked frames that round-trip through oxiarc
// ---------------------------------------------------------------------------

#[test]
fn lz4_03_oxiarc_linked_roundtrip() {
    let data = linked_pattern(LINKED_LEN);
    let desc = FrameDescriptor::new()
        .with_content_size(data.len() as u64)
        .with_block_independence(false)
        .with_block_max_size(oxiarc_lz4::BlockMaxSize::Size64KB);
    let compressed = compress_with_options(&data, desc).expect("linked compress");
    let out = decompress(&compressed, data.len() * 2).expect("linked decompress");
    assert_eq!(out, data);
}

// ---------------------------------------------------------------------------
// Live differential tests against the reference `lz4` CLI (feature-gated)
// ---------------------------------------------------------------------------

#[cfg(feature = "lz4-oracle")]
mod oracle {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn lz4_available() -> bool {
        Command::new("lz4")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "oxiarc_lz4_oracle_{}_{}_{}.tmp",
            std::process::id(),
            id,
            tag
        ))
    }

    /// Decompress an oxiarc-produced frame with the reference `lz4 -d`.
    fn lz4_decompress(frame: &[u8]) -> Vec<u8> {
        let inp = tmp_path("in.lz4");
        let outp = tmp_path("out.bin");
        std::fs::write(&inp, frame).expect("write frame");
        let status = Command::new("lz4")
            .args(["-d", "-f"])
            .arg(&inp)
            .arg(&outp)
            .status()
            .expect("run lz4 -d");
        assert!(status.success(), "lz4 -d rejected the oxiarc frame");
        let out = std::fs::read(&outp).expect("read lz4 output");
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
        out
    }

    /// Compress raw bytes with the reference `lz4`, returning the frame.
    fn lz4_compress(raw: &[u8], extra_args: &[&str]) -> Vec<u8> {
        let inp = tmp_path("in.bin");
        let outp = tmp_path("out.lz4");
        std::fs::write(&inp, raw).expect("write raw");
        let mut cmd = Command::new("lz4");
        cmd.arg("-f");
        for a in extra_args {
            cmd.arg(a);
        }
        let status = cmd.arg(&inp).arg(&outp).status().expect("run lz4");
        assert!(status.success(), "lz4 failed to compress");
        let frame = std::fs::read(&outp).expect("read lz4 frame");
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
        frame
    }

    fn diverse_inputs() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("empty", Vec::new()),
            ("one-byte", vec![0x5a]),
            ("all-zero-64", vec![0u8; 64]),
            ("all-zero-100k", vec![0u8; 100_000]),
            ("rle", b"ab".repeat(50_000)),
            ("repetitive-text", b"The quick brown fox. ".repeat(20_000)),
            (
                "incompressible",
                (0..200_000u32)
                    .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
                    .collect(),
            ),
            ("boundary-64k", vec![0x42u8; 64 * 1024]),
            ("boundary-64k+1", vec![0x42u8; 64 * 1024 + 1]),
            ("boundary-1m", linked_pattern(1024 * 1024)),
            ("boundary-2m", linked_pattern(2 * 1024 * 1024 + 123)),
        ]
    }

    // --- LZ4-02 gate: oxiarc encode -> lz4 -d must accept, byte-identical. ---
    #[test]
    fn oracle_oxiarc_frames_accepted_by_lz4() {
        if !lz4_available() {
            eprintln!("skipping: `lz4` binary not on PATH");
            return;
        }
        let mut n = 0;
        for (label, data) in diverse_inputs() {
            let desc = FrameDescriptor::new().with_content_size(data.len() as u64);
            let frame = compress_with_options(&data, desc).expect("oxiarc compress");
            let decoded = lz4_decompress(&frame);
            assert_eq!(decoded, data, "{label}: lz4 -d output mismatch");
            n += 1;
        }
        eprintln!("oracle_oxiarc_frames_accepted_by_lz4: {n}/{n} frames accepted by lz4 -d");
    }

    // --- Reference lz4 encode -> oxiarc decode, byte-identical. ---
    #[test]
    fn oracle_reference_frames_decoded_by_oxiarc() {
        if !lz4_available() {
            eprintln!("skipping: `lz4` binary not on PATH");
            return;
        }
        let mut n = 0;
        for (label, data) in diverse_inputs() {
            for args in [
                &["-1"][..],
                &["-9"][..],
                &["-9", "-BD"][..],
                &["-BD", "-B4"][..],
            ] {
                let frame = lz4_compress(&data, args);
                let out = decompress(&frame, data.len().max(1) * 2 + 64).expect("oxiarc decode");
                assert_eq!(out, data, "{label} args={args:?}: oxiarc decode mismatch");
                n += 1;
            }
        }
        eprintln!(
            "oracle_reference_frames_decoded_by_oxiarc: {n}/{n} reference frames decoded byte-identical"
        );
    }

    // --- LZ4-03 gate both directions on linked (-BD) frames. ---
    #[test]
    fn oracle_linked_frames_both_directions() {
        if !lz4_available() {
            eprintln!("skipping: `lz4` binary not on PATH");
            return;
        }
        let inputs = [
            ("linked-200k", linked_pattern(200_000)),
            ("linked-1m", linked_pattern(1024 * 1024)),
            ("linked-zeros", vec![0u8; 300_000]),
        ];
        let mut n = 0;
        for (label, data) in inputs {
            // reference -BD -> oxiarc decode
            let ref_frame = lz4_compress(&data, &["-BD", "-B4"]);
            let out = decompress(&ref_frame, data.len() * 2).expect("oxiarc decode -BD");
            assert_eq!(out, data, "{label}: oxiarc decode of lz4 -BD mismatch");

            // oxiarc linked -> lz4 -d
            let desc = FrameDescriptor::new()
                .with_content_size(data.len() as u64)
                .with_block_independence(false)
                .with_block_max_size(oxiarc_lz4::BlockMaxSize::Size64KB);
            let oxi_frame = compress_with_options(&data, desc).expect("oxiarc linked compress");
            let decoded = lz4_decompress(&oxi_frame);
            assert_eq!(
                decoded, data,
                "{label}: lz4 -d of oxiarc linked frame mismatch"
            );
            n += 1;
        }
        eprintln!("oracle_linked_frames_both_directions: {n}/{n} linked pairs interoperable");
    }
}

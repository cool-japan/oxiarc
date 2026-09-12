//! The `.xz` block **check** field, end to end.
//!
//! The XZ container stores an integrity check of every block's *original*
//! (post-filter, uncompressed) data: CRC-32, CRC-64/ECMA-182, SHA-256, or
//! nothing at all (xz spec §2.1.1.2, §3.4). `XzWriter` computes it and
//! `XzReader` verifies it.
//!
//! Every other test in this crate that mentions a check type is either an
//! oxiarc→oxiarc round trip (self-consistent: a wrong digest would be both
//! written and verified wrongly, and the round trip would still pass) or a
//! decode of a stream the real `xz` CLI produced (which pins the digest
//! *functions*, but only while the reader's comparison is switched on).
//! Nothing asserted that a **corrupted check field is rejected**, so the
//! comparison itself could be deleted outright without a single test
//! noticing — verified by mutation: replacing each `if computed != expected`
//! with `if false && computed != expected` left the whole suite green.
//!
//! This file closes that hole from both ends:
//!
//! * the writer emits exactly the reference digest of the block's data, and
//! * the reader rejects a stream whose check field has been altered,
//!
//! for every check type the format defines, in single-block and multi-block
//! streams — plus, behind the `xz-oracle` feature, `xz -t` accepting what
//! this writer emits for *each* check type (the pre-existing oracle only
//! ever fed the CLI the default `CheckType::Crc32`).

use oxiarc_core::crc::{Crc32, Crc64};
use oxiarc_core::error::OxiArcError;
use oxiarc_core::sha256::Sha256;
use oxiarc_lzma::LzmaLevel;
use oxiarc_lzma::xz::{self, CheckType, XzWriter};

/// A payload with no short repeats, so the check bytes computed over it do
/// not collide with anything else in the stream (asserted by
/// [`locate_unique`], not assumed).
fn payload_bytes(len: usize) -> Vec<u8> {
    let mut state = 0x1234_5678_9ABC_DEF0u64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// The reference check bytes for `data` under `check`, exactly as the xz
/// spec defines them (little-endian for the CRCs, big-endian digest for
/// SHA-256).
fn reference_check_bytes(check: CheckType, data: &[u8]) -> Vec<u8> {
    match check {
        CheckType::None => Vec::new(),
        CheckType::Crc32 => Crc32::compute(data).to_le_bytes().to_vec(),
        CheckType::Crc64 => Crc64::compute(data).to_le_bytes().to_vec(),
        CheckType::Sha256 => Sha256::compute(data).to_vec(),
        // `CheckType` is `#[non_exhaustive]`; a check type this file does
        // not know how to compute must fail loudly rather than be silently
        // treated as "no check".
        other => panic!("unhandled check type {other:?}: add it to this test"),
    }
}

/// Find `needle` in `haystack`, asserting it occurs exactly once so a test
/// can never corrupt the wrong bytes and quietly prove nothing.
fn locate_unique(haystack: &[u8], needle: &[u8], label: &str) -> usize {
    assert!(!needle.is_empty(), "{label}: empty needle");
    let hits: Vec<usize> = haystack
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        hits.len(),
        1,
        "{label}: expected the check bytes to appear exactly once in the \
         stream, found {} occurrences at {hits:?}",
        hits.len()
    );
    hits[0]
}

fn stream_of(payload: &[u8], check: CheckType, block_size: u64) -> Vec<u8> {
    XzWriter::new(LzmaLevel::new(6))
        .with_check_type(check)
        .with_block_size(block_size)
        .compress(payload)
        .unwrap_or_else(|err| panic!("compress with {check:?}: {err}"))
}

// ---------------------------------------------------------------------------
// Writer: the emitted check bytes are the reference digest of the block data
// ---------------------------------------------------------------------------

#[test]
fn the_writer_emits_the_reference_check_of_every_block() {
    let payload = payload_bytes(9_000);

    for check in [CheckType::Crc32, CheckType::Crc64, CheckType::Sha256] {
        // Single block: the check covers the whole payload.
        let single = stream_of(&payload, check, u64::MAX);
        let expected = reference_check_bytes(check, &payload);
        locate_unique(&single, &expected, &format!("{check:?} single-block"));

        // Multi-block: each block carries the check of *its own* slice, so
        // every per-block digest must be present, and the digest of the
        // whole payload must not be (that is the classic off-by-one this
        // pins down: computing the check over the accumulated stream rather
        // than the block).
        let block_size = 2_048usize;
        let multi = stream_of(&payload, check, block_size as u64);
        for (index, chunk) in payload.chunks(block_size).enumerate() {
            let per_block = reference_check_bytes(check, chunk);
            locate_unique(&multi, &per_block, &format!("{check:?} block {index}"));
        }
        if payload.len() > block_size {
            assert!(
                !multi
                    .windows(expected.len())
                    .any(|window| window == expected.as_slice()),
                "{check:?}: a multi-block stream must not carry the check of \
                 the whole payload"
            );
        }
    }
}

#[test]
fn a_check_type_of_none_stores_no_check_bytes() {
    let payload = payload_bytes(4_000);
    let none = stream_of(&payload, CheckType::None, u64::MAX);
    let crc32 = stream_of(&payload, CheckType::Crc32, u64::MAX);
    // The CRC-32 stream is exactly 4 bytes longer: the check field is the
    // only difference between the two.
    assert_eq!(
        none.len() + 4,
        crc32.len(),
        "CheckType::None must not reserve space for a check"
    );
    assert_eq!(
        xz::decompress(&mut std::io::Cursor::new(&none)).expect("decode"),
        payload
    );
}

// ---------------------------------------------------------------------------
// Reader: a corrupted check field is rejected
// ---------------------------------------------------------------------------

/// Flipping any bit of the stored check must be detected, for every check
/// type and through every public entry point.
///
/// This is the test whose absence let the whole `verify_check` comparison
/// be disabled without a failure; keep it (or an equivalent) whenever that
/// code is touched.
#[test]
fn a_corrupted_check_field_is_rejected_for_every_check_type() {
    let payload = payload_bytes(6_000);

    for check in [CheckType::Crc32, CheckType::Crc64, CheckType::Sha256] {
        let stream = stream_of(&payload, check, u64::MAX);
        let expected = reference_check_bytes(check, &payload);
        let offset = locate_unique(&stream, &expected, &format!("{check:?}"));

        for byte in 0..expected.len() {
            for mask in [0x01u8, 0x80] {
                let mut corrupted = stream.clone();
                corrupted[offset + byte] ^= mask;

                let err =
                    xz::decompress(&mut std::io::Cursor::new(&corrupted)).expect_err(&format!(
                        "{check:?}: flipping check byte {byte} (mask {mask:#04x}) \
                         must be detected by xz::decompress"
                    ));
                match check {
                    CheckType::Crc32 => assert!(
                        matches!(err, OxiArcError::CrcMismatch { .. }),
                        "{check:?}: expected CrcMismatch, got {err:?}"
                    ),
                    CheckType::Crc64 => assert!(
                        err.to_string().contains("CRC-64 mismatch"),
                        "{check:?}: expected a CRC-64 mismatch, got {err}"
                    ),
                    CheckType::Sha256 => assert!(
                        err.to_string().contains("SHA-256 mismatch"),
                        "{check:?}: expected a SHA-256 mismatch, got {err}"
                    ),
                    CheckType::None => unreachable!("None has no check field"),
                    other => panic!("unhandled check type {other:?}"),
                }

                // The bounded entry points must agree: a stream that fails
                // its integrity check is never silently truncated into the
                // caller's buffer.
                let mut dst = vec![0u8; payload.len()];
                assert!(
                    xz::decompress_into(&corrupted, &mut dst).is_err(),
                    "{check:?}: decompress_into accepted a corrupted check"
                );
                assert!(
                    xz::decompress_with_limit(&corrupted, payload.len()).is_err(),
                    "{check:?}: decompress_with_limit accepted a corrupted check"
                );

                // ...and so must a reused decoder context: the whole point
                // of `XzDecoder` is that reuse cannot change which streams
                // are accepted.
                let mut reused = xz::XzDecoder::new();
                let mut warm = vec![0u8; payload.len()];
                reused
                    .decompress_into(&stream, &mut warm)
                    .expect("prime the reuse cache with the intact stream");
                assert!(
                    reused.decompress_into(&corrupted, &mut dst).is_err(),
                    "{check:?}: a primed XzDecoder accepted a corrupted check"
                );
            }
        }
    }
}

/// The same property inside a multi-block stream, where the corrupted check
/// belongs to a block in the *middle*: the failure must surface rather than
/// being masked by the surrounding blocks verifying correctly.
#[test]
fn a_corrupted_check_field_in_a_middle_block_is_rejected() {
    let payload = payload_bytes(9_000);
    let block_size = 2_048usize;

    for check in [CheckType::Crc32, CheckType::Crc64, CheckType::Sha256] {
        let stream = stream_of(&payload, check, block_size as u64);
        let blocks: Vec<&[u8]> = payload.chunks(block_size).collect();
        assert!(blocks.len() >= 3, "need a genuine middle block");
        let middle = blocks[blocks.len() / 2];
        let expected = reference_check_bytes(check, middle);
        let offset = locate_unique(&stream, &expected, &format!("{check:?} middle block"));

        let mut corrupted = stream.clone();
        corrupted[offset] ^= 0x40;
        let err = xz::decompress(&mut std::io::Cursor::new(&corrupted)).expect_err(&format!(
            "{check:?}: a corrupted middle-block check must fail"
        ));
        assert!(
            !err.to_string().is_empty(),
            "{check:?}: error must carry a message"
        );
        let mut dst = vec![0u8; payload.len()];
        assert!(xz::decompress_into(&corrupted, &mut dst).is_err());
    }
}

/// A truncated check field (the block ends before the check is complete)
/// is an error, never a silently-accepted block.
#[test]
fn a_truncated_check_field_is_an_error() {
    let payload = payload_bytes(2_000);
    for check in [CheckType::Crc32, CheckType::Crc64, CheckType::Sha256] {
        let stream = stream_of(&payload, check, u64::MAX);
        let width = reference_check_bytes(check, &payload).len();
        let offset = locate_unique(
            &stream,
            &reference_check_bytes(check, &payload),
            &format!("{check:?}"),
        );
        for cut in offset..offset + width {
            let mut dst = vec![0u8; payload.len()];
            assert!(
                xz::decompress(&mut std::io::Cursor::new(&stream[..cut])).is_err(),
                "{check:?}: truncating inside the check field at {cut} was accepted"
            );
            assert!(xz::decompress_into(&stream[..cut], &mut dst).is_err());
        }
    }
}

// ---------------------------------------------------------------------------
// External ground truth: `xz -t` accepts what this writer emits, for every
// check type (not only the default `CheckType::Crc32`).
// ---------------------------------------------------------------------------

#[cfg(feature = "xz-oracle")]
mod oracle {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxiarc_xz_check_{label}_{}_{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn tool_available(tool: &str, args: &[&str]) -> bool {
        Command::new(tool)
            .args(args)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    /// Every check type this writer can emit must satisfy the reference
    /// implementation, in single-block *and* multi-block streams.
    ///
    /// The pre-existing multi-block oracle only ever handed `xz` the
    /// default `CheckType::Crc32`, so the CRC-64 and SHA-256 digests this
    /// crate writes had no external ground truth at all — they were only
    /// ever checked against this crate's own reader.
    #[test]
    fn every_check_type_the_writer_emits_is_accepted_by_the_xz_cli() {
        if !tool_available("xz", &["--version"]) {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let dir = unique_temp_dir("checks");
        let payload = payload_bytes(120_000);

        let mut verified = 0usize;
        for check in [
            CheckType::None,
            CheckType::Crc32,
            CheckType::Crc64,
            CheckType::Sha256,
        ] {
            for block_size in [u64::MAX, 16_384] {
                let stream = stream_of(&payload, check, block_size);
                let path = dir.join(format!("{check:?}_{block_size}.xz"));
                std::fs::write(&path, &stream).expect("write stream");

                let tested = Command::new("xz")
                    .arg("-t")
                    .arg(&path)
                    .output()
                    .expect("spawn xz -t");
                assert!(
                    tested.status.success(),
                    "[{check:?} block_size {block_size}] xz -t rejected our stream: {}",
                    String::from_utf8_lossy(&tested.stderr)
                );

                let decoded = Command::new("xz")
                    .args(["-dc"])
                    .arg(&path)
                    .output()
                    .expect("spawn xz -dc");
                assert!(
                    decoded.status.success(),
                    "[{check:?} block_size {block_size}] xz -dc failed: {}",
                    String::from_utf8_lossy(&decoded.stderr)
                );
                assert_eq!(
                    decoded.stdout, payload,
                    "[{check:?} block_size {block_size}] xz decoded our stream to \
                     different bytes"
                );

                // `xz --robot -lvv` reports the check name it read out of
                // the stream header, so a writer that declared one check
                // and computed another would be caught here too.
                let listed = Command::new("xz")
                    .args(["--robot", "-lvv"])
                    .arg(&path)
                    .output()
                    .expect("spawn xz --robot -lvv");
                assert!(
                    listed.status.success(),
                    "[{check:?}] xz --robot -lvv failed"
                );
                let text = String::from_utf8_lossy(&listed.stdout);
                let expected_name = match check {
                    CheckType::None => "None",
                    CheckType::Crc32 => "CRC32",
                    CheckType::Crc64 => "CRC64",
                    CheckType::Sha256 => "SHA-256",
                    other => panic!("unhandled check type {other:?}"),
                };
                assert!(
                    text.contains(expected_name),
                    "[{check:?}] xz did not report check {expected_name}; got:\n{text}"
                );
                verified += 1;
            }
        }
        eprintln!(
            "[xz-oracle] {verified} oxiarc-written streams accepted by `xz -t` across \
             all four check types"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reference tool must *reject* a stream whose check we corrupt —
    /// proof that the field is load-bearing in real `xz` too, so the
    /// reader-side rejection tests above are testing the same thing the
    /// format actually requires.
    #[test]
    fn the_xz_cli_also_rejects_a_corrupted_check_field() {
        if !tool_available("xz", &["--version"]) {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let dir = unique_temp_dir("corrupt");
        let payload = payload_bytes(6_000);

        for check in [CheckType::Crc32, CheckType::Crc64, CheckType::Sha256] {
            let stream = stream_of(&payload, check, u64::MAX);
            let offset = locate_unique(
                &stream,
                &reference_check_bytes(check, &payload),
                &format!("{check:?}"),
            );
            let mut corrupted = stream.clone();
            corrupted[offset] ^= 0x01;
            let path = dir.join(format!("{check:?}_corrupt.xz"));
            std::fs::write(&path, &corrupted).expect("write corrupted stream");

            let tested = Command::new("xz")
                .arg("-t")
                .arg(&path)
                .output()
                .expect("spawn xz -t");
            assert!(
                !tested.status.success(),
                "[{check:?}] xz -t accepted a stream with a corrupted check field, \
                 so this test is not testing what it claims"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

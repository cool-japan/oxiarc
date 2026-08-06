//! Differential oracle for the legacy LZH methods against the real `lha` CLI.
//!
//! `lha` here is Lhasa (decompress-only). Its decoder table
//! (`lib/lha_decoder.c`) registers `-lzs-`, `-lz5-`, `-lz4-` and `-pm0-` — the
//! last two through the null/passthrough decoder — so all four can be checked
//! end to end: this crate encodes a payload, wraps it in a `.lzh` container
//! through the real public `LzhWriter` API, and requires an independent LHA
//! implementation to CRC-test and extract it back to the original bytes. The
//! same archive is then read back through `LzhReader`, so a pass means the
//! encoder, the decoder and the reference all agree on one byte stream.
//!
//! Lhasa registers **no** `-lh2-` or `-lh3-` decoder (there is no
//! `lh2_decoder.c`/`lh3_decoder.c` in its source tree at all), so those two
//! methods cannot be gated this way; they are covered by `tests/lzh_legacy.rs`
//! and by the reference vectors in `oxiarc-lzhuf`.
//!
//! Gated behind the `lha-oracle` feature; every test self-skips (prints a note,
//! does not fail) when `lha` is not on PATH.
#![cfg(feature = "lha-oracle")]

use oxiarc_archive::lzh::{LzhReader, LzhWriter};
use oxiarc_core::Crc16;
use oxiarc_lzhuf::{LzhMethod, decode_lzh, encode_lzh};
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

/// Locate the `lha` binary via `which`; `None` means "self-skip".
fn find_lha() -> Option<PathBuf> {
    let output = Command::new("which").arg("lha").output().ok()?;
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

/// Create a unique per-test scratch directory under the system temp dir.
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_lzh_legacy_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Build a single-entry `.lzh` archive carrying `payload` compressed with
/// `method`, written through the public writer API.
fn build_archive(name: &str, method: LzhMethod, payload: &[u8], header_level: u8) -> Vec<u8> {
    let compressed = if method.is_stored() {
        payload.to_vec()
    } else {
        encode_lzh(payload, method).expect("encode legacy method")
    };
    let mut buf = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut buf).with_header_level(header_level);
        writer
            .add_file_raw(
                name,
                method,
                Crc16::compute(payload),
                payload.len() as u64,
                &compressed,
                0,
                None,
            )
            .expect("add_file_raw");
        writer.finish().expect("finish");
    }
    buf
}

/// The full three-way check for one method and payload.
fn oracle_roundtrip(label: &str, method: LzhMethod, payload: &[u8], header_level: u8) {
    if find_lha().is_none() {
        eprintln!(
            "[lha-oracle] `lha` not found on PATH; skipping '{label}' (self-skip, not a failure)"
        );
        return;
    }

    let entry_name = "payload.bin";
    let archive = build_archive(entry_name, method, payload, header_level);

    let dir = scratch_dir(label);
    let archive_path = dir.join("legacy.lzh");
    std::fs::write(&archive_path, &archive).expect("write archive");

    // 1. The reference implementation must CRC-test the archive clean.
    let test_output = Command::new("lha")
        .arg("t")
        .arg(&archive_path)
        .output()
        .expect("spawn `lha t`");
    assert!(
        test_output.status.success(),
        "[{label}] `lha t` failed (exit {:?}): stdout={:?} stderr={:?}",
        test_output.status.code(),
        String::from_utf8_lossy(&test_output.stdout),
        String::from_utf8_lossy(&test_output.stderr)
    );

    // 2. ... and extract it back to exactly the original bytes.
    let extract_dir = dir.join("extracted");
    std::fs::create_dir_all(&extract_dir).expect("create extract dir");
    let extract_output = Command::new("lha")
        .arg(format!("xfw={}", extract_dir.display()))
        .arg(&archive_path)
        .output()
        .expect("spawn `lha x`");
    assert!(
        extract_output.status.success(),
        "[{label}] `lha x` failed (exit {:?}): stdout={:?} stderr={:?}",
        extract_output.status.code(),
        String::from_utf8_lossy(&extract_output.stdout),
        String::from_utf8_lossy(&extract_output.stderr)
    );
    let extracted = std::fs::read(extract_dir.join(entry_name)).expect("read extracted file");
    assert_eq!(
        extracted, payload,
        "[{label}] `lha x` output must match the original input byte for byte"
    );

    // 3. This crate's own decoder must reproduce the same bytes from the same
    //    compressed stream the reference just accepted.
    let compressed = if method.is_stored() {
        payload.to_vec()
    } else {
        encode_lzh(payload, method).expect("encode")
    };
    let decoded = decode_lzh(&compressed, method, payload.len() as u64).expect("decode");
    assert_eq!(decoded, payload, "[{label}] codec-level round-trip");

    // 4. ... and so must the archive reader over the very archive bytes `lha`
    //    just read.
    let mut reader = LzhReader::new(Cursor::new(archive.clone())).expect("open archive");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1, "[{label}] one entry expected");
    let mut via_reader = Vec::new();
    reader
        .extract(&entries[0], &mut via_reader)
        .expect("LzhReader::extract");
    assert_eq!(via_reader, payload, "[{label}] archive-level round-trip");

    eprintln!(
        "[lha-oracle] '{label}' ({}): OK — {} bytes -> {} compressed; lha t + lha x agree",
        method.name(),
        payload.len(),
        compressed.len()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A mixed payload: ASCII runs, structured records and pseudo-random bytes.
fn mixed_payload(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut state = 0x1234_5678u32;
    while data.len() < len {
        data.extend_from_slice(b"OxiArc legacy LZH oracle payload line ==============\n");
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        data.extend_from_slice(&state.to_le_bytes());
        data.extend(std::iter::repeat_n(b' ', (state % 17) as usize));
    }
    data.truncate(len);
    data
}

#[test]
fn lzs_matches_the_reference_decoder() {
    oracle_roundtrip("lzs_text", LzhMethod::Lzs, &mixed_payload(9_000), 2);
}

#[test]
fn lzs_matches_the_reference_decoder_on_runs() {
    let data: Vec<u8> = std::iter::repeat_n(b'=', 20_000).collect();
    oracle_roundtrip("lzs_runs", LzhMethod::Lzs, &data, 2);
}

#[test]
fn lzs_matches_the_reference_decoder_on_binary() {
    let data: Vec<u8> = (0..12_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    oracle_roundtrip("lzs_binary", LzhMethod::Lzs, &data, 2);
}

#[test]
fn lz5_matches_the_reference_decoder() {
    oracle_roundtrip("lz5_text", LzhMethod::Lz5, &mixed_payload(40_000), 2);
}

#[test]
fn lz5_matches_the_reference_decoder_on_runs() {
    let data: Vec<u8> = std::iter::repeat_n(b'-', 30_000).collect();
    oracle_roundtrip("lz5_runs", LzhMethod::Lz5, &data, 2);
}

#[test]
fn lz5_matches_the_reference_decoder_on_binary() {
    let data: Vec<u8> = (0..25_000u32).map(|i| (i ^ (i >> 5)) as u8).collect();
    oracle_roundtrip("lz5_binary", LzhMethod::Lz5, &data, 2);
}

#[test]
fn lz4_stored_matches_the_reference_decoder() {
    oracle_roundtrip("lz4_stored", LzhMethod::Lz4, &mixed_payload(5_000), 2);
}

#[test]
fn pm0_stored_matches_the_reference_decoder() {
    oracle_roundtrip("pm0_stored", LzhMethod::Pm0, &mixed_payload(5_000), 2);
}

#[test]
fn legacy_methods_survive_level_one_headers() {
    oracle_roundtrip("lz5_level1", LzhMethod::Lz5, &mixed_payload(7_000), 1);
    oracle_roundtrip("lzs_level1", LzhMethod::Lzs, &mixed_payload(7_000), 1);
}

#[test]
fn small_and_empty_payloads_match_the_reference_decoder() {
    oracle_roundtrip("lz5_tiny", LzhMethod::Lz5, b"a", 2);
    oracle_roundtrip("lzs_tiny", LzhMethod::Lzs, b"a", 2);
    oracle_roundtrip("lz5_short", LzhMethod::Lz5, b"hello hello hello hello", 2);
    oracle_roundtrip("lzs_short", LzhMethod::Lzs, b"hello hello hello hello", 2);
}

//! Archive-layer coverage for the legacy LZH methods (`-lh2-`, `-lh3-`,
//! `-lzs-`, `-lz4-`, `-lz5-`, `-pm0-`).
//!
//! Hermetic — no external tool. The differential half lives in
//! `tests/lzh_legacy_oracle.rs` (real `lha`, for the four methods Lhasa can
//! decode) and in `oxiarc-lzhuf/tests/lzh_legacy_vectors.rs` (frozen
//! reference-verified vectors, for `-lh2-`/`-lh3-`, which no available tool
//! decodes). What this file proves is that the container layer routes each
//! method to the right codec, reports it honestly in the entry listing, and
//! survives a full write -> read cycle with CRC-16 verification.

use oxiarc_archive::lzh::{LzhReader, LzhWriter};
use oxiarc_core::Crc16;
use oxiarc_core::entry::CompressionMethod;
use oxiarc_lzhuf::{LzhMethod, encode_lzh};
use std::io::Cursor;

/// Every legacy method this crate can both write and read.
const LEGACY_METHODS: &[LzhMethod] = &[
    LzhMethod::Lh2,
    LzhMethod::Lh3,
    LzhMethod::Lzs,
    LzhMethod::Lz4,
    LzhMethod::Lz5,
    LzhMethod::Pm0,
];

/// Build a single-entry archive carrying `payload` under `method`.
fn build(name: &str, method: LzhMethod, payload: &[u8], header_level: u8) -> Vec<u8> {
    let compressed = if method.is_stored() {
        payload.to_vec()
    } else {
        encode_lzh(payload, method).expect("encode")
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

/// Read the single entry back out of `archive`.
fn read_back(archive: &[u8]) -> (String, CompressionMethod, Vec<u8>) {
    let mut reader = LzhReader::new(Cursor::new(archive.to_vec())).expect("open");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1, "one entry expected");
    let mut out = Vec::new();
    reader.extract(&entries[0], &mut out).expect("extract");
    (entries[0].name.clone(), entries[0].method, out)
}

fn sample_payload(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut state = 0x0BAD_F00Du32;
    while data.len() < len {
        data.extend_from_slice(b"legacy LZH archive-layer payload ----------------\n");
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        data.extend_from_slice(&state.to_le_bytes());
        data.extend(std::iter::repeat_n(b'.', (state % 23) as usize));
    }
    data.truncate(len);
    data
}

#[test]
fn every_legacy_method_round_trips_through_the_archive_layer() {
    let payload = sample_payload(50_000);
    for &method in LEGACY_METHODS {
        for level in [1u8, 2] {
            let archive = build("legacy.bin", method, &payload, level);
            let (name, reported, extracted) = read_back(&archive);
            assert_eq!(name, "legacy.bin", "{method} level {level}");
            assert_eq!(
                reported.name(),
                method.name(),
                "{method} level {level}: the listing must report the real method, \
                 not \"Unknown\""
            );
            assert_eq!(
                extracted, payload,
                "{method} level {level}: extracted bytes must match"
            );
        }
    }
}

#[test]
fn every_legacy_method_round_trips_an_empty_and_a_one_byte_entry() {
    for &method in LEGACY_METHODS {
        for payload in [b"".as_slice(), b"x".as_slice()] {
            let archive = build("tiny.bin", method, payload, 2);
            let (_, _, extracted) = read_back(&archive);
            assert_eq!(
                extracted,
                payload,
                "{method} on a {}-byte entry",
                payload.len()
            );
        }
    }
}

#[test]
fn a_corrupted_legacy_payload_is_rejected_by_crc() {
    let payload = sample_payload(4_000);
    for &method in LEGACY_METHODS {
        let mut archive = build("legacy.bin", method, &payload, 2);
        // Flip a bit deep inside the compressed payload (the last quarter of
        // the file is always data, never header).
        let target = archive.len() * 3 / 4;
        archive[target] ^= 0x40;
        let mut reader = LzhReader::new(Cursor::new(archive)).expect("open");
        let entries = reader.entries().to_vec();
        let mut out = Vec::new();
        let result = reader.extract(&entries[0], &mut out);
        assert!(
            result.is_err() || out != payload,
            "{method}: a corrupted payload must not silently extract as valid"
        );
    }
}

#[test]
fn a_still_unsupported_method_is_listed_and_refused_not_fatal() {
    // `-pm2-` remains unimplemented: the entry must still be listed (so the
    // rest of the archive stays usable) and extraction must fail cleanly. The
    // payload bytes are arbitrary here — nothing can decode them, which is the
    // point — so they are written verbatim rather than through a codec.
    let payload = sample_payload(256);
    let method = LzhMethod::Unknown(*b"-pm2-");
    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive);
        writer
            .add_file_raw(
                "future.bin",
                method,
                Crc16::compute(&payload),
                payload.len() as u64,
                &payload,
                0,
                None,
            )
            .expect("add_file_raw");
        writer.finish().expect("finish");
    }
    let mut reader = LzhReader::new(Cursor::new(archive)).expect("open");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "future.bin");
    let mut out = Vec::new();
    assert!(
        reader.extract(&entries[0], &mut out).is_err(),
        "an unimplemented method must return an error, never wrong bytes"
    );
}

#[test]
fn multi_entry_archives_mix_legacy_and_modern_methods() {
    let payload = sample_payload(20_000);
    let mut buf = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut buf);
        for (index, &method) in LEGACY_METHODS
            .iter()
            .chain([LzhMethod::Lh5, LzhMethod::Lh0].iter())
            .enumerate()
        {
            let compressed = if method.is_stored() {
                payload.to_vec()
            } else {
                encode_lzh(&payload, method).expect("encode")
            };
            writer
                .add_file_raw(
                    &format!("entry{index}.bin"),
                    method,
                    Crc16::compute(&payload),
                    payload.len() as u64,
                    &compressed,
                    0,
                    None,
                )
                .expect("add_file_raw");
        }
        writer.finish().expect("finish");
    }

    let mut reader = LzhReader::new(Cursor::new(buf)).expect("open");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), LEGACY_METHODS.len() + 2);
    for entry in &entries {
        let mut out = Vec::new();
        reader.extract(entry, &mut out).expect("extract");
        assert_eq!(out, payload, "entry {} must round-trip", entry.name);
    }
}

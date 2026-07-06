//! Archive-level regression tests for the lh5 window-corruption bug.
//!
//! The lh5 encoder used to corrupt payloads beyond its 8 KB window:
//! CRC-16 mismatch on extraction for incompressible data >= 16 KB, and
//! silent corruption for compressible data at 64 KB+. These tests exercise
//! the full LzhWriter -> LzhReader path (extract() verifies the stored
//! CRC-16 internally, reproducing the original failure mode exactly).

use oxiarc_archive::lzh::{LzhCompressionLevel, LzhReader, LzhWriter};
use std::io::Cursor;

/// Deterministic xorshift32 pseudo-random bytes (effectively incompressible).
fn xorshift_data(len: usize, mut state: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 4);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn compressible_data(len: usize) -> Vec<u8> {
    b"lzh archive roundtrip pattern 0123456789 "
        .iter()
        .cycle()
        .take(len)
        .copied()
        .collect()
}

/// Write `data` as a single lh5 entry, read it back, extract it.
/// `LzhReader::extract` verifies the header CRC-16 against the
/// decompressed bytes, so a CRC mismatch fails here.
fn archive_roundtrip(data: &[u8], label: &str) {
    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive);
        writer.set_compression(LzhCompressionLevel::Lh5);
        writer.add_file("payload.bin", data).expect("add_file");
        writer.finish().expect("finish");
    }

    let mut reader = LzhReader::new(Cursor::new(&archive)).expect("open archive");
    let entries = reader.entries();
    assert_eq!(entries.len(), 1, "{label}: entry count");
    assert_eq!(entries[0].size, data.len() as u64, "{label}: size");

    let extracted = reader
        .extract_to_vec(&entries[0])
        .unwrap_or_else(|e| panic!("{label}: extraction failed (CRC-16 mismatch?): {e}"));
    assert_eq!(extracted, data, "{label}: silent corruption detected");
}

#[test]
fn lh5_archive_roundtrip_incompressible_sizes() {
    for &size in &[8 * 1024usize, 16 * 1024, 64 * 1024, 100 * 1024] {
        let data = xorshift_data(size, 0xA5A5_5A5A);
        archive_roundtrip(&data, &format!("incompressible {size}B"));
    }
}

#[test]
fn lh5_archive_roundtrip_compressible_sizes() {
    for &size in &[8 * 1024usize, 16 * 1024, 64 * 1024, 100 * 1024] {
        let data = compressible_data(size);
        archive_roundtrip(&data, &format!("compressible {size}B"));
    }
}

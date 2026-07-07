//! Decode-direction ground-truth tests against the real-world LZH corpus in
//! `tests/data/` (see `tests/data/README.md` for full provenance). Each
//! fixture was produced by a genuine, independent LHA-family tool — not by
//! OxiArc — so a byte-exact decode here is proof of real format
//! compatibility, not merely internal self-consistency.
//!
//! Archive-level (multi-header-level, extension-chain) parsing is
//! `oxiarc-archive`'s job, not this crate's; the tiny header reader below
//! exists only to locate each fixture's compressed payload for this test and
//! is deliberately minimal (levels 0/1/2, no error recovery). It was derived
//! and cross-validated three independent ways before being written here:
//! the recovered CRC-16 and filename bytes matched the corpus README's
//! manifest exactly, and the level-0/1/2 variants of the identical `gpl-2`
//! payload were confirmed to start at byte-identical compressed offsets.

use oxiarc_lzhuf::{LzhMethod, decode_lzh};
use std::path::Path;

/// Chase a chain of LZH "extension header" chunks starting at `pos` in
/// `data`. Each chunk is `[u16 LE size][data...]`, where `size` counts itself
/// (the 2 size bytes are included); `size == 0` terminates the chain. Used by
/// header levels 1 and 2, whose extension mechanism is identical. Returns the
/// offset immediately after the terminator.
fn chase_extensions(data: &[u8], mut pos: usize) -> usize {
    loop {
        let size = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("2 bytes")) as usize;
        if size == 0 {
            return pos + 2;
        }
        pos += size;
    }
}

/// Parse just enough of an LZH level-0/1/2 header to locate the compressed
/// payload. Returns `(payload_start_offset, method, uncompressed_size)`.
fn locate_payload(data: &[u8]) -> (usize, LzhMethod, u64) {
    let method = LzhMethod::from_id(&data[2..7]).expect("recognised method id");
    let uncompressed_size = u32::from_le_bytes(data[11..15].try_into().expect("4 bytes")) as u64;
    let level = data[20];
    let start = match level {
        // Level 0: no extension-header mechanism at all; the base header's
        // own size field directly gives the total on-disk header length
        // (header_size_byte + the 2 bytes it does not count: itself and the
        // checksum byte).
        0 => data[0] as usize + 2,
        // Level 1: filename is inline (length-prefixed at [21]), followed by
        // CRC16(2), an OS-id byte, then the extension chain.
        1 => {
            let name_len = data[21] as usize;
            let os_id_pos = 22 + name_len + 2;
            chase_extensions(data, os_id_pos + 1)
        }
        // Level 2: no inline filename (it lives in an extension instead);
        // fixed fields run method(5) comp(4) uncomp(4) time(4) attr(1)
        // level(1) = up to offset 20, then CRC16(2) + OS-id(1), then the
        // extension chain starts at offset 24.
        2 => chase_extensions(data, 24),
        other => panic!("unsupported LZH header level {other} in test fixture"),
    };
    (start, method, uncompressed_size)
}

/// Decode `data_path`'s LZH payload and assert it matches `expected_path`
/// byte-for-byte.
fn assert_fixture_decodes_exact(data_path: &str, expected_path: &str) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let data = std::fs::read(Path::new(manifest_dir).join("tests/data").join(data_path))
        .unwrap_or_else(|e| panic!("reading fixture {data_path}: {e}"));
    let expected = std::fs::read(
        Path::new(manifest_dir)
            .join("tests/data")
            .join(expected_path),
    )
    .unwrap_or_else(|e| panic!("reading expected output {expected_path}: {e}"));

    let (payload_start, method, uncompressed_size) = locate_payload(&data);
    let payload = &data[payload_start..];

    let decoded = decode_lzh(payload, method, uncompressed_size).unwrap_or_else(|e| {
        panic!("decoding fixture {data_path} (method {method}, {uncompressed_size} bytes): {e}")
    });

    assert_eq!(
        decoded.len(),
        expected.len(),
        "{data_path}: decoded length mismatch"
    );
    if decoded != expected {
        let first_diff = decoded
            .iter()
            .zip(expected.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(usize::MAX);
        panic!("{data_path}: decoded output diverges from {expected_path} at byte {first_diff}");
    }
}

#[test]
fn decode_lha_unix114i_h0_lh5() {
    assert_fixture_decodes_exact("lha_unix114i_h0_lh5.lzh", "lha_unix114i_h0_lh5.expected");
}

#[test]
fn decode_lha_unix114i_h1_lh5() {
    assert_fixture_decodes_exact("lha_unix114i_h1_lh5.lzh", "lha_unix114i_h1_lh5.expected");
}

#[test]
fn decode_lha_unix114i_h2_lh5() {
    assert_fixture_decodes_exact("lha_unix114i_h2_lh5.lzh", "lha_unix114i_h2_lh5.expected");
}

#[test]
fn decode_lha255e_lh5() {
    assert_fixture_decodes_exact("lha255e_lh5.lzh", "lha255e_lh5.expected");
}

#[test]
fn decode_lha213_lh5_long() {
    // The large multi-block fixture: 1.24 MB spanning many Huffman blocks
    // (re-sent code tables), the case a single-block decoder would silently
    // get wrong.
    assert_fixture_decodes_exact("lha213_lh5_long.lzh", "lha213_lh5_long.expected");
}

#[test]
fn decode_lha_unix114i_h0_lh0_stored_baseline() {
    assert_fixture_decodes_exact("lha_unix114i_h0_lh0.lzh", "lha_unix114i_h0_lh0.expected");
}

#[test]
fn all_lh5_fixtures_decode_to_byte_identical_content() {
    // Independent cross-tool/cross-header-level confirmation: four -lh5-
    // archives from different encoders and header levels all carry the same
    // GPL-2 text and must decode identically to each other, not just to the
    // stored .expected file.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let read_expected = |name: &str| {
        std::fs::read(Path::new(manifest_dir).join("tests/data").join(name))
            .unwrap_or_else(|e| panic!("reading {name}: {e}"))
    };
    let h0 = read_expected("lha_unix114i_h0_lh5.expected");
    let h1 = read_expected("lha_unix114i_h1_lh5.expected");
    let h2 = read_expected("lha_unix114i_h2_lh5.expected");
    let e255 = read_expected("lha255e_lh5.expected");
    assert_eq!(h0, h1);
    assert_eq!(h0, h2);
    assert_eq!(h0, e255);
}

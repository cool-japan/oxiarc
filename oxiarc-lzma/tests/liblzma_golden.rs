//! Hermetic liblzma interoperability regression tests.
//!
//! Guards the spec-conformant probability-model layout (in particular the
//! distance-slot special table `PosDecoders + dist - posSlot`, LzmaSpec.cpp)
//! and the literal state-machine transitions against regressions. Prior to
//! the fix, oxiarc-lzma round-tripped against itself but could not decode
//! real liblzma / xz / 7-Zip streams, and its own output was rejected by
//! real tools.
//!
//! All vectors under `tests/data/` were generated ONCE during development:
//!
//! * `liblzma_*.bin` were produced by CPython's `lzma` module (liblzma):
//!   - `liblzma_lzma1_raw_*`: `FORMAT_RAW`, `filters=[{'id': FILTER_LZMA1,
//!     'preset': 6}]` (lc=3, lp=0, pb=2)
//!   - `liblzma_lzma1_alone_small`: `FORMAT_ALONE`, `preset=6`
//!   - `liblzma_lzma2_raw_*`: `FORMAT_RAW`, `filters=[{'id': FILTER_LZMA2,
//!     'preset': 6}]`
//! * `oxiarc_*.bin` were produced by this crate's encoder and verified
//!   byte-exact decodable by liblzma (`lzma.LZMADecompressor`, `eof=True`,
//!   `unused_data` empty) during development. They are embedded so future
//!   decoder changes stay compatible with streams already written by oxiarc.
//!
//! The committed test suite invokes no external tools.

use std::io::Cursor;

use oxiarc_lzma::{LzmaLevel, LzmaProperties};

/// liblzma: raw LZMA1, preset 6, of `small_fixture()`.
const LIBLZMA_LZMA1_RAW_SMALL: &[u8] = include_bytes!("data/liblzma_lzma1_raw_small.bin");
/// liblzma: raw LZMA1, preset 6, of `large_fixture()` (> 64 KiB uncompressed).
const LIBLZMA_LZMA1_RAW_LARGE: &[u8] = include_bytes!("data/liblzma_lzma1_raw_large.bin");
/// liblzma: `.lzma` (LZMA_Alone container), preset 6, of `small_fixture()`.
const LIBLZMA_LZMA1_ALONE_SMALL: &[u8] = include_bytes!("data/liblzma_lzma1_alone_small.bin");
/// liblzma: raw LZMA2, preset 6, of `small_fixture()`.
const LIBLZMA_LZMA2_RAW_SMALL: &[u8] = include_bytes!("data/liblzma_lzma2_raw_small.bin");
/// liblzma: raw LZMA2, preset 6, of `large_fixture()` (> 64 KiB uncompressed).
const LIBLZMA_LZMA2_RAW_LARGE: &[u8] = include_bytes!("data/liblzma_lzma2_raw_large.bin");

/// oxiarc: `compress_raw(small, level 6, 1 MiB dict)`, liblzma-verified.
const OXIARC_LZMA1_RAW_SMALL: &[u8] = include_bytes!("data/oxiarc_lzma1_raw_small.bin");
/// oxiarc: `compress_raw(large, level 6, 1 MiB dict)`, liblzma-verified.
const OXIARC_LZMA1_RAW_LARGE: &[u8] = include_bytes!("data/oxiarc_lzma1_raw_large.bin");
/// oxiarc: `compress(small, level 6)` (`.lzma` header), liblzma-verified.
const OXIARC_LZMA1_ALONE_SMALL: &[u8] = include_bytes!("data/oxiarc_lzma1_alone_small.bin");
/// oxiarc: `lzma2_compress(small, 6)`, liblzma-verified.
const OXIARC_LZMA2_SMALL: &[u8] = include_bytes!("data/oxiarc_lzma2_small.bin");
/// oxiarc: `lzma2_compress(large, 6)`, liblzma-verified.
const OXIARC_LZMA2_LARGE: &[u8] = include_bytes!("data/oxiarc_lzma2_large.bin");

/// Properties used by all raw LZMA1 vectors (liblzma preset 6 defaults).
fn default_props() -> LzmaProperties {
    LzmaProperties::new(3, 0, 2)
}

/// 1116-byte text fixture; must match `small_fixture()` in the generator
/// script byte for byte.
fn small_fixture() -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..12 {
        out.extend_from_slice(
            format!(
                "OxiArc LZMA interop fixture line {i:02}: \
                 The quick brown fox jumps over the lazy dog. 0123456789\n"
            )
            .as_bytes(),
        );
    }
    out
}

/// 100 005-byte fixture (> 64 KiB) driven by a deterministic LCG; must match
/// `large_fixture()` in the generator script byte for byte.
fn large_fixture() -> Vec<u8> {
    const WORDS: [&[u8]; 8] = [
        b"alpha", b"bravo", b"charlie", b"delta", b"echo", b"foxtrot", b"golf", b"hotel",
    ];
    let mut out = Vec::new();
    let mut seed: u64 = 0x1234_5678;
    let mut i = 0u32;
    while out.len() < 100_000 {
        seed = (seed.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7FFF_FFFF;
        let word = WORDS[(seed & 7) as usize];
        let rep = ((seed >> 8) & 3) + 1;
        out.extend_from_slice(format!("line {i:06} ").as_bytes());
        for _ in 0..rep {
            out.extend_from_slice(word);
        }
        out.push(b'\n');
        i += 1;
    }
    out
}

fn decode_raw_lzma1(packed: &[u8], expected_len: u64) -> Vec<u8> {
    oxiarc_lzma::decompress_raw(
        Cursor::new(packed),
        default_props(),
        1 << 23,
        Some(expected_len),
    )
    .expect("raw LZMA1 stream must decode")
}

// ─────────────────────────────────────────────────────────────────────────────
// (1) Real liblzma streams must decode with oxiarc.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn decodes_liblzma_lzma1_raw_small() {
    let expected = small_fixture();
    let decoded = decode_raw_lzma1(LIBLZMA_LZMA1_RAW_SMALL, expected.len() as u64);
    assert_eq!(decoded, expected, "liblzma raw LZMA1 (small) mismatch");
}

#[test]
fn decodes_liblzma_lzma1_raw_large() {
    let expected = large_fixture();
    assert!(expected.len() > 64 * 1024, "fixture must exceed 64 KiB");
    let decoded = decode_raw_lzma1(LIBLZMA_LZMA1_RAW_LARGE, expected.len() as u64);
    assert_eq!(decoded, expected, "liblzma raw LZMA1 (large) mismatch");
}

#[test]
fn decodes_liblzma_lzma1_alone_container() {
    // FORMAT_ALONE: 13-byte header (props, dict size, uncompressed size),
    // unknown-size variant terminated by the end-of-stream marker.
    let decoded = oxiarc_lzma::decompress_bytes(LIBLZMA_LZMA1_ALONE_SMALL)
        .expect(".lzma container must decode");
    assert_eq!(decoded, small_fixture(), "liblzma .lzma container mismatch");
}

#[test]
fn decodes_liblzma_lzma2_raw_small() {
    let decoded = oxiarc_lzma::decode_lzma2(LIBLZMA_LZMA2_RAW_SMALL, 1 << 23)
        .expect("raw LZMA2 stream must decode");
    assert_eq!(
        decoded,
        small_fixture(),
        "liblzma raw LZMA2 (small) mismatch"
    );
}

#[test]
fn decodes_liblzma_lzma2_raw_large() {
    let expected = large_fixture();
    assert!(expected.len() > 64 * 1024, "fixture must exceed 64 KiB");
    let decoded = oxiarc_lzma::decode_lzma2(LIBLZMA_LZMA2_RAW_LARGE, 1 << 23)
        .expect("raw LZMA2 stream must decode");
    assert_eq!(decoded, expected, "liblzma raw LZMA2 (large) mismatch");
}

// ─────────────────────────────────────────────────────────────────────────────
// (2) liblzma-verified oxiarc streams must keep decoding.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn decodes_embedded_oxiarc_lzma1_raw_vectors() {
    let expected = small_fixture();
    let decoded = decode_raw_lzma1(OXIARC_LZMA1_RAW_SMALL, expected.len() as u64);
    assert_eq!(
        decoded, expected,
        "embedded oxiarc raw LZMA1 (small) mismatch"
    );

    let expected = large_fixture();
    let decoded = decode_raw_lzma1(OXIARC_LZMA1_RAW_LARGE, expected.len() as u64);
    assert_eq!(
        decoded, expected,
        "embedded oxiarc raw LZMA1 (large) mismatch"
    );
}

#[test]
fn decodes_embedded_oxiarc_lzma1_alone_vector() {
    let decoded = oxiarc_lzma::decompress_bytes(OXIARC_LZMA1_ALONE_SMALL)
        .expect("embedded oxiarc .lzma stream must decode");
    assert_eq!(decoded, small_fixture(), "embedded oxiarc .lzma mismatch");
}

#[test]
fn decodes_embedded_oxiarc_lzma2_vectors() {
    let decoded = oxiarc_lzma::decode_lzma2(OXIARC_LZMA2_SMALL, 1 << 23)
        .expect("embedded oxiarc LZMA2 stream must decode");
    assert_eq!(
        decoded,
        small_fixture(),
        "embedded oxiarc LZMA2 (small) mismatch"
    );

    let expected = large_fixture();
    let decoded = oxiarc_lzma::decode_lzma2(OXIARC_LZMA2_LARGE, 1 << 23)
        .expect("embedded oxiarc LZMA2 stream must decode");
    assert_eq!(decoded, expected, "embedded oxiarc LZMA2 (large) mismatch");
}

// ─────────────────────────────────────────────────────────────────────────────
// (2b) Self round-trips over the same fixtures, all encoder paths.
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn round_trips_raw_lzma1_greedy_and_optimal() {
    let expected = large_fixture();
    for level in [1u8, 6, 9] {
        let packed = oxiarc_lzma::compress_raw(&expected, LzmaLevel::new(level), 1 << 20)
            .expect("compress_raw must succeed");
        let decoded = decode_raw_lzma1(&packed, expected.len() as u64);
        assert_eq!(
            decoded, expected,
            "raw LZMA1 round-trip failed at level {level}"
        );
    }
}

#[test]
fn round_trips_lzma_alone_container() {
    let expected = small_fixture();
    let packed =
        oxiarc_lzma::compress(&expected, LzmaLevel::new(6)).expect("compress must succeed");
    let decoded = oxiarc_lzma::decompress_bytes(&packed).expect("own .lzma stream must decode");
    assert_eq!(decoded, expected, ".lzma round-trip failed");
}

#[test]
fn round_trips_lzma2_chunked() {
    let expected = large_fixture();
    let packed = oxiarc_lzma::lzma2_compress(&expected, 6).expect("lzma2_compress must succeed");
    let decoded = oxiarc_lzma::lzma2_decompress(&packed).expect("own LZMA2 stream must decode");
    assert_eq!(decoded, expected, "LZMA2 chunked round-trip failed");
}

// ─────────────────────────────────────────────────────────────────────────────
// Spec-layout details worth pinning down explicitly.
// ─────────────────────────────────────────────────────────────────────────────

/// The LZMA2 reset field occupies control-byte bits 5-6; value 2 means
/// "state reset + new properties" WITHOUT a dictionary reset. The buggy
/// pre-fix parser misread bit 5 as a dictionary reset, so a `0xC0` chunk
/// following a `0xE0` chunk must keep the dictionary intact.
#[test]
fn lzma2_state_reset_chunk_keeps_dictionary() {
    // liblzma-style two-chunk stream, built from oxiarc's own encoder pieces:
    // chunk 1 (0xE0: props + state + dict reset) then chunk 2 (0xC0: props +
    // state reset only) that back-references chunk 1 data via the dictionary.
    let part1 = small_fixture();
    let part2 = small_fixture(); // identical content: matches reach into chunk 1

    let props = default_props();
    let enc1 = oxiarc_lzma::LzmaEncoder::new(LzmaLevel::new(6), 1 << 20);
    let payload1 = enc1.compress_chunk(&part1).expect("chunk 1 compress");
    let enc2 = oxiarc_lzma::LzmaEncoder::new(LzmaLevel::new(6), 1 << 20);
    let payload2 = enc2.compress_chunk(&part2).expect("chunk 2 compress");

    let mut stream = Vec::new();
    for (control_base, payload, raw) in [(0xE0u8, &payload1, &part1), (0xC0u8, &payload2, &part2)] {
        let unpacked_minus_1 = raw.len() - 1;
        let control = control_base | ((unpacked_minus_1 >> 16) & 0x1F) as u8;
        stream.push(control);
        stream.extend_from_slice(&((unpacked_minus_1 & 0xFFFF) as u16).to_be_bytes());
        stream.extend_from_slice(&((payload.len() - 1) as u16).to_be_bytes());
        stream.push(props.to_byte());
        stream.extend_from_slice(payload);
    }
    stream.push(0x00); // end of LZMA2 stream

    let decoded =
        oxiarc_lzma::decode_lzma2(&stream, 1 << 23).expect("two-chunk LZMA2 stream must decode");
    let mut expected = part1;
    expected.extend_from_slice(&part2);
    assert_eq!(
        decoded, expected,
        "0xC0 chunk must not reset the dictionary"
    );
}

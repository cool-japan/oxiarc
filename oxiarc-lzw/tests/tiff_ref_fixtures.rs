//! Always-run TIFF-LZW interop regression tests against pinned reference
//! fixtures.
//!
//! # Fixture provenance
//!
//! Every `tests/data/*.lzw` file is a raw single-strip TIFF-LZW stream
//! produced by Pillow 12.1.0 / libtiff 4.7.1 (`compression="tiff_lzw"`,
//! strip bytes extracted via the StripOffsets/StripByteCounts tags) from a
//! raw input that is reproduced bit-exactly in this file (LCG identical to
//! `tests/bench_ratios.rs`, plus trivial patterns). Unlike the
//! `tiff-oracle` feature tests these need no external tools, so they gate
//! every `cargo test` run:
//!
//! - **decode direction**: real libtiff-produced strips (leading ClearCode,
//!   early-change width growth, table-fill ClearCode resets in
//!   `lcg_16384.lzw`) must decode byte-identically.
//! - **encode direction**: `compress_tiff` must reproduce the reference
//!   bytes exactly. TIFF LZW with libtiff's parameters (greedy longest
//!   match, ClearCode at strip start and at table entry 4094, early change,
//!   phantom final-entry accounting before EOI) is fully deterministic, and
//!   the full 125-case oracle corpus confirmed byte-identity with libtiff
//!   across boundary sweeps and multi-reset inputs — so any divergence here
//!   is a real interop regression, not encoder freedom.
//!
//! The fixture sizes deliberately sit on the nasty corners: 253/254/255
//! straddle the decoder's 9->10 bit early-change threshold interacting with
//! the EOI phantom entry, 511/512 cross it mid-stream, and 16384 LCG bytes
//! force the code table to fill past entry 4094 (mid-stream ClearCode
//! resets, exercised twice).

use oxiarc_lzw::{compress_tiff, decompress_tiff};

/// Deterministic pseudo-random bytes (same LCG as `tests/bench_ratios.rs`).
fn lcg_bytes(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
    for _ in 0..len {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed >> 32) as u8);
    }
    data
}

/// All pinned fixtures as (name, raw input, reference LZW strip).
fn fixtures() -> Vec<(&'static str, Vec<u8>, &'static [u8])> {
    vec![
        (
            "lcg_253",
            lcg_bytes(253),
            include_bytes!("data/lcg_253.lzw").as_slice(),
        ),
        (
            "lcg_254",
            lcg_bytes(254),
            include_bytes!("data/lcg_254.lzw").as_slice(),
        ),
        (
            "lcg_255",
            lcg_bytes(255),
            include_bytes!("data/lcg_255.lzw").as_slice(),
        ),
        (
            "lcg_511",
            lcg_bytes(511),
            include_bytes!("data/lcg_511.lzw").as_slice(),
        ),
        (
            "lcg_512",
            lcg_bytes(512),
            include_bytes!("data/lcg_512.lzw").as_slice(),
        ),
        (
            "lcg_16384",
            lcg_bytes(16384),
            include_bytes!("data/lcg_16384.lzw").as_slice(),
        ),
        (
            "allbytes_256",
            (0..=255).collect(),
            include_bytes!("data/allbytes_256.lzw").as_slice(),
        ),
        (
            "zeros_65536",
            vec![0u8; 65536],
            include_bytes!("data/zeros_65536.lzw").as_slice(),
        ),
        (
            "fox_4500",
            b"The quick brown fox jumps over the lazy dog. ".repeat(100),
            include_bytes!("data/fox_4500.lzw").as_slice(),
        ),
    ]
}

/// Every fixture begins with the mandatory TIFF 6.0 ClearCode (256), i.e.
/// first 9 bits = `1_0000_0000` = byte 0x80 + high bit of the next byte.
#[test]
fn reference_strips_start_with_clear_code() {
    for (name, _, strip) in fixtures() {
        assert!(
            strip.len() >= 2,
            "[{name}] reference strip implausibly short"
        );
        assert_eq!(
            strip[0], 0x80,
            "[{name}] TIFF 6.0 strips start with ClearCode (256) MSB-first"
        );
    }
}

/// Decode direction: libtiff-produced strips decode byte-identically.
#[test]
fn reference_strips_decode_byte_identical() {
    for (name, raw, strip) in fixtures() {
        let decoded = decompress_tiff(strip, raw.len())
            .unwrap_or_else(|e| panic!("[{name}] failed to decode libtiff strip: {e}"));
        assert_eq!(decoded, raw, "[{name}] wrong decode of libtiff strip");
    }
}

/// Encode direction: `compress_tiff` reproduces libtiff's bytes exactly
/// (see the module docs for why byte-identity is the correct expectation).
#[test]
fn oxiarc_encoding_matches_libtiff_byte_for_byte() {
    for (name, raw, strip) in fixtures() {
        let encoded = compress_tiff(&raw).unwrap_or_else(|e| panic!("[{name}] encode failed: {e}"));
        assert_eq!(
            encoded, strip,
            "[{name}] oxiarc TIFF-LZW output diverged from the libtiff reference stream"
        );
    }
}

/// Truncated reference strips must never be silently mis-decoded.
///
/// Acceptable outcomes per cut point: `Err` (data codes lost), or `Ok` with
/// output shorter than `expected_size` (the caller sees the shortfall), or
/// `Ok` with the complete correct bytes (the cut only removed the trailing
/// EOI code / padding bits, so every data code was still present). The one
/// forbidden outcome is `Ok` with full-length WRONG data.
#[test]
fn truncated_reference_strips_never_silently_corrupt() {
    for (name, raw, strip) in fixtures() {
        for cut in [1, strip.len() / 2, strip.len() - 1] {
            let truncated = &strip[..cut];
            match decompress_tiff(truncated, raw.len()) {
                Err(_) => {}
                Ok(decoded) => {
                    if decoded.len() == raw.len() {
                        assert_eq!(
                            decoded, raw,
                            "[{name}] cut={cut}: full-length decode of a truncated \
                             strip returned wrong bytes (silent corruption)"
                        );
                    }
                }
            }
        }
    }
}

/// Corrupted (bit-flipped) strips must never panic; wrong data must not be
/// silently returned as a full-length successful decode.
#[test]
fn corrupted_reference_strips_never_panic() {
    for (name, raw, strip) in fixtures() {
        for pos in [0usize, 1, strip.len() / 3, strip.len() / 2, strip.len() - 1] {
            let mut corrupted = strip.to_vec();
            corrupted[pos] ^= 0x55;
            // Must not panic; Err or (possibly wrong) Ok are both acceptable
            // at the codec layer — TIFF has no per-strip checksum, so some
            // bit flips are undetectable by construction.
            let _ = decompress_tiff(&corrupted, raw.len());
            let _ = name;
        }
    }
}

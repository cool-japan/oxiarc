//! Always-on interoperability regression tests.
//!
//! The fixtures under `tests/data/zstd/` are **real** frames produced by the
//! reference `zstd` CLI (v1.5.x). Before the FSE/Huffman backward-bitstream
//! fix, 6 of these 8 frames made the decoder panic (FSE state out of range,
//! Huffman table overflow, 4-stream slice panics) and 2 — `chunk_09` and
//! `chunk_24` — decoded silently to *wrong* bytes. Every frame must decode
//! byte-identically, forever.
//!
//! The full live differential against the `zstd` CLI (wide level/flag matrix,
//! both directions) lives in `tests/zstd_oracle.rs` behind the `zstd-oracle`
//! feature.

use oxiarc_zstd::{ZstdEncoder, compress_with_level, decode_all, decompress, encode_all};

/// The reference-produced fixture frames and their expected outputs.
const FIXTURES: &[(&str, &[u8], &[u8])] = &[
    (
        "chunk_00",
        include_bytes!("data/zstd/chunk_00.zst"),
        include_bytes!("data/zstd/chunk_00.raw"),
    ),
    (
        "chunk_09",
        include_bytes!("data/zstd/chunk_09.zst"),
        include_bytes!("data/zstd/chunk_09.raw"),
    ),
    (
        "chunk_24",
        include_bytes!("data/zstd/chunk_24.zst"),
        include_bytes!("data/zstd/chunk_24.raw"),
    ),
    (
        "chunk_31",
        include_bytes!("data/zstd/chunk_31.zst"),
        include_bytes!("data/zstd/chunk_31.raw"),
    ),
    (
        "chunk_50",
        include_bytes!("data/zstd/chunk_50.zst"),
        include_bytes!("data/zstd/chunk_50.raw"),
    ),
    (
        "chunk_51",
        include_bytes!("data/zstd/chunk_51.zst"),
        include_bytes!("data/zstd/chunk_51.raw"),
    ),
    (
        "chunk_58",
        include_bytes!("data/zstd/chunk_58.zst"),
        include_bytes!("data/zstd/chunk_58.raw"),
    ),
    (
        "chunk_63",
        include_bytes!("data/zstd/chunk_63.zst"),
        include_bytes!("data/zstd/chunk_63.raw"),
    ),
];

/// Every reference-produced fixture frame decodes byte-identically.
#[test]
fn reference_fixtures_decode_byte_identical() {
    for (name, frame, expected) in FIXTURES {
        let decoded =
            decompress(frame).unwrap_or_else(|e| panic!("fixture {name} failed to decode: {e}"));
        assert_eq!(
            decoded.len(),
            expected.len(),
            "fixture {name}: wrong length"
        );
        assert_eq!(&decoded, expected, "fixture {name}: wrong bytes");
    }
}

/// Truncating a valid reference frame at any prefix length must return `Err`
/// (or, for prefixes that happen to end exactly on a frame boundary, the
/// correct bytes) — never panic, never wrong bytes.
#[test]
fn truncated_reference_frames_error_cleanly() {
    let (_, frame, expected) = FIXTURES[0];
    for len in 0..frame.len() {
        match decompress(&frame[..len]) {
            Err(_) => {}
            Ok(out) => assert_eq!(
                &out, expected,
                "truncation at {len} returned Ok with wrong bytes"
            ),
        }
    }
}

/// Flipping any single bit in a reference frame must never panic: malformed
/// input always surfaces as `Err` (or, for these checksum-less fixtures, as
/// a differently-decoded but cleanly-produced output — the format carries no
/// integrity data unless the checksum flag is set).
#[test]
fn bit_flipped_reference_frames_never_panic() {
    let (_, frame, _) = FIXTURES[1];
    // Walk a stride through the frame so the test stays fast while touching
    // header, table-description and bitstream regions. Any panic fails the
    // test; both Ok and Err are acceptable outcomes.
    for byte_idx in (0..frame.len()).step_by(7) {
        for bit in 0..8 {
            let mut corrupted = frame.to_vec();
            corrupted[byte_idx] ^= 1 << bit;
            let _ = decompress(&corrupted);
        }
    }
}

/// With a checksummed frame (our encoder emits XXH64 checksums by default),
/// single-bit corruption of block *data* must never yield `Ok` with wrong
/// content — the checksum catches whatever the structural validation misses.
#[test]
fn bit_flipped_checksummed_frames_never_silently_corrupt() {
    let data: Vec<u8> = (0..20_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let frame = compress_with_level(&data, 3).expect("compress");
    for byte_idx in (0..frame.len()).step_by(5) {
        for bit in 0..8 {
            let mut corrupted = frame.clone();
            corrupted[byte_idx] ^= 1 << bit;
            if let Ok(out) = decompress(&corrupted) {
                // Flips that decode Ok must not have changed the content
                // (e.g. they hit an ignored header bit)... anything else is
                // silent corruption.
                assert_eq!(
                    out, data,
                    "bit flip at byte {byte_idx} bit {bit} silently corrupted output"
                );
            }
        }
    }
}

/// Regression for ZSTD-04: `set_content_size(false)` used to truncate the
/// Frame_Content_Size to one byte inside a Single_Segment frame, silently
/// corrupting every input of 256 bytes or more (255 worked, 256 broke).
#[test]
fn no_content_size_roundtrip_across_boundary() {
    for size in [0usize, 1, 255, 256, 257, 1000, 100_000] {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let mut encoder = ZstdEncoder::new();
        encoder.set_content_size(false);
        let compressed = encoder.compress(&data).expect("compress");
        let decoded = decode_all(&compressed).expect("decode");
        assert_eq!(decoded, data, "no-content-size roundtrip failed at {size}");
    }
}

/// Regression for ZSTD-06: repetitive frames used to mis-decode through the
/// repeat-offset path (CRC-caught errors and, on the fixture corpus, silent
/// corruption). Root cause was the FSE bitstream direction; keep these
/// self-roundtrips as cheap sentinels.
#[test]
fn repetitive_content_roundtrips() {
    let patterns: [&[u8]; 3] = [b"ABCDEFGH", b"ab", b"The quick brown fox. "];
    for pattern in patterns {
        for reps in [13usize, 100, 5000] {
            let data: Vec<u8> = pattern
                .iter()
                .copied()
                .cycle()
                .take(pattern.len() * reps)
                .collect();
            for level in [1, 3, 9, 19] {
                let compressed = compress_with_level(&data, level).expect("compress");
                let decoded = decode_all(&compressed).expect("decode");
                assert_eq!(
                    decoded, data,
                    "repetitive roundtrip failed: pattern {:?} x{reps} level {level}",
                    pattern
                );
            }
        }
    }
}

/// Self-roundtrip across block/window boundaries and content classes.
#[test]
fn boundary_size_roundtrips() {
    for size in [
        0usize, 1, 31, 32, 255, 256, 4095, 4096, 65_535, 65_536, 131_071, 131_072, 131_073,
    ] {
        let data: Vec<u8> = (0..size).map(|i| (i * 31 % 253) as u8).collect();
        let compressed = encode_all(&data, 3).expect("compress");
        let decoded = decode_all(&compressed).expect("decode");
        assert_eq!(decoded, data, "boundary roundtrip failed at {size}");
    }
}

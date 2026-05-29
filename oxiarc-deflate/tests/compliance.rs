//! Compliance tests verifying DEFLATE/zlib/gzip output format correctness
//! against the RFC specifications:
//!   - RFC 1951: DEFLATE compressed data format
//!   - RFC 1950: zlib compressed data format
//!   - RFC 1952: gzip file format
//!
//! These tests validate byte-level format correctness without any external
//! reference decoder — checksums are verified with the same algorithms
//! already shipped in oxiarc-deflate and oxiarc-core.

use oxiarc_core::Crc32;
use oxiarc_deflate::gzip::{gzip_compress, gzip_decompress};
use oxiarc_deflate::zlib::{Adler32, zlib_compress, zlib_decompress};
use oxiarc_deflate::{deflate, inflate};

// ---------------------------------------------------------------------------
// Helper: compute expected zlib FLG byte for a given level (mirrors zlib.rs).
// ---------------------------------------------------------------------------

/// Compute the zlib CMF and FLG header bytes for the given compression level.
///
/// Mirrors the logic in `zlib_compress` so we can compare encoder output
/// against the expected values without re-encoding.
fn expected_zlib_header(level: u8) -> (u8, u8) {
    // CMF is always 0x78: CM=8 (DEFLATE), CINFO=7 (window=32 KiB)
    let cmf: u8 = 0x78;

    // FLEVEL encoding (bits 6-7 of FLG)
    let flevel: u8 = match level {
        0..=2 => 0, // Fastest
        3..=5 => 1, // Fast
        6 => 2,     // Default
        7..=9 => 3, // Maximum
        _ => 2,     // Default fallback
    };

    let fdict: u8 = 0;
    let base = (cmf as u16) * 256 + ((flevel << 6) | (fdict << 5)) as u16;
    let remainder = base % 31;
    let fcheck = if remainder == 0 {
        0u8
    } else {
        (31 - remainder) as u8
    };
    let flg = (flevel << 6) | (fdict << 5) | fcheck;

    (cmf, flg)
}

// ===========================================================================
// Zlib wrapper compliance
// ===========================================================================

/// Level-6 default produces CMF=0x78, FLG=0x9C (the most commonly cited zlib
/// magic pair).
#[test]
fn test_zlib_header_level6_magic_bytes() {
    let input = b"Hello, zlib compliance test!";
    let compressed = zlib_compress(input, 6).expect("zlib_compress failed");

    assert!(
        compressed.len() >= 6,
        "zlib stream too short to contain header+checksum"
    );
    assert_eq!(
        compressed[0], 0x78,
        "CMF must be 0x78 (CM=8, CINFO=7) at level 6"
    );
    assert_eq!(
        compressed[1], 0x9C,
        "FLG must be 0x9C for level 6 (FLEVEL=2=Default, valid FCHECK)"
    );
}

/// For every compression level 0-9 the zlib header must:
///  - have CMF == 0x78 (DEFLATE, 32 KiB window)
///  - satisfy (CMF * 256 + FLG) % 31 == 0  (RFC 1950 §2.2)
///  - match the FLEVEL bits we can predict from the level encoding table
#[test]
fn test_zlib_header_structural_validity_all_levels() {
    let input = b"structural validity test payload";

    for level in 0u8..=9 {
        let compressed = zlib_compress(input, level)
            .unwrap_or_else(|e| panic!("zlib_compress level {} failed: {}", level, e));

        assert!(compressed.len() >= 6, "level {}: stream too short", level);

        let cmf = compressed[0];
        let flg = compressed[1];

        // CM must be 8 (bits 0-3)
        assert_eq!(
            cmf & 0x0F,
            8,
            "level {}: CM in CMF must be 8 (DEFLATE)",
            level
        );

        // RFC 1950 §2.2: (CMF * 256 + FLG) % 31 == 0
        let check = ((cmf as u16) * 256 + flg as u16) % 31;
        assert_eq!(
            check, 0,
            "level {}: (CMF*256+FLG)%31 must be 0, got {} (CMF={:#04x} FLG={:#04x})",
            level, check, cmf, flg
        );

        // FDICT bit (bit 5) must be clear (no preset dictionary)
        assert_eq!(
            flg & 0x20,
            0,
            "level {}: FDICT bit must be 0 (no preset dictionary)",
            level
        );

        // FLEVEL bits (bits 6-7) must match our prediction
        let (expected_cmf, expected_flg) = expected_zlib_header(level);
        assert_eq!(cmf, expected_cmf, "level {}: CMF mismatch", level);
        assert_eq!(
            flg, expected_flg,
            "level {}: FLG mismatch (expected {:#04x}, got {:#04x})",
            level, expected_flg, flg
        );
    }
}

/// The last 4 bytes of a zlib stream must be the big-endian Adler-32 of
/// the *uncompressed* data (RFC 1950 §2.2).
#[test]
fn test_zlib_adler32_trailer() {
    let inputs: &[&[u8]] = &[
        b"",
        b"a",
        b"Hello, Adler-32!",
        &[0u8; 1000],
        &[0xFFu8; 2000],
    ];

    for input in inputs {
        let compressed =
            zlib_compress(input, 6).unwrap_or_else(|e| panic!("zlib_compress failed: {}", e));

        assert!(
            compressed.len() >= 6,
            "stream too short to contain Adler-32 trailer"
        );

        // Last 4 bytes are big-endian Adler-32
        let n = compressed.len();
        let stored_adler = u32::from_be_bytes([
            compressed[n - 4],
            compressed[n - 3],
            compressed[n - 2],
            compressed[n - 1],
        ]);

        let expected_adler = Adler32::checksum(input);
        assert_eq!(
            stored_adler,
            expected_adler,
            "Adler-32 mismatch for input len {}",
            input.len()
        );
    }
}

/// For an empty input the Adler-32 is 1 (initial state a=1, b=0).
#[test]
fn test_zlib_adler32_empty_input_equals_one() {
    let compressed = zlib_compress(b"", 6).expect("zlib_compress failed");
    let n = compressed.len();
    let stored_adler = u32::from_be_bytes([
        compressed[n - 4],
        compressed[n - 3],
        compressed[n - 2],
        compressed[n - 1],
    ]);
    // Adler-32 of empty data = 1 (a=1, b=0 → (0<<16)|1 = 1)
    assert_eq!(stored_adler, 1, "Adler-32 of empty data must be 1");
}

// ===========================================================================
// Gzip wrapper compliance
// ===========================================================================

/// Every gzip stream must start with the magic bytes 0x1F 0x8B and have
/// CM=8 (DEFLATE) at byte 2 (RFC 1952 §2.3.1).
#[test]
fn test_gzip_magic_bytes_and_method() {
    let inputs: &[&[u8]] = &[b"", b"x", b"compliance gzip magic test"];

    for input in inputs {
        let compressed =
            gzip_compress(input, 6).unwrap_or_else(|e| panic!("gzip_compress failed: {}", e));

        assert!(
            compressed.len() >= 10,
            "gzip stream must be at least 10 bytes (header)"
        );
        assert_eq!(compressed[0], 0x1F, "gzip ID1 must be 0x1F");
        assert_eq!(compressed[1], 0x8B, "gzip ID2 must be 0x8B");
        assert_eq!(compressed[2], 8, "gzip CM must be 8 (DEFLATE)");
    }
}

/// The gzip header bytes 4-7 carry MTIME (modification time). Our encoder
/// sets MTIME=0 since we do not record file timestamps.
#[test]
fn test_gzip_mtime_is_zero() {
    let compressed = gzip_compress(b"mtime zero test", 6).expect("gzip_compress failed");
    assert_eq!(
        &compressed[4..8],
        &[0u8; 4],
        "gzip MTIME must be 0x00000000"
    );
}

/// Byte 9 of the gzip header is the OS identifier. Our encoder sets it to 255
/// (unknown), which is the most portable value (RFC 1952 §2.3.1).
#[test]
fn test_gzip_os_byte_is_unknown() {
    let compressed = gzip_compress(b"os unknown", 6).expect("gzip_compress failed");
    assert_eq!(compressed[9], 255, "gzip OS byte must be 255 (unknown)");
}

/// The last 8 bytes of a gzip stream are:
///   bytes n-8 .. n-5: CRC-32 of the uncompressed data, little-endian
///   bytes n-4 .. n-1: ISIZE (uncompressed length mod 2^32), little-endian
/// (RFC 1952 §2.3.1)
#[test]
fn test_gzip_crc32_and_isize_trailer() {
    let inputs: &[&[u8]] = &[
        b"",
        b"z",
        b"The quick brown fox jumps over the lazy dog",
        &[0xA5u8; 512],
    ];

    for input in inputs {
        let compressed =
            gzip_compress(input, 6).unwrap_or_else(|e| panic!("gzip_compress failed: {}", e));

        assert!(
            compressed.len() >= 18,
            "gzip stream too short to contain trailer (min 18 bytes)"
        );

        let n = compressed.len();
        let stored_crc = u32::from_le_bytes([
            compressed[n - 8],
            compressed[n - 7],
            compressed[n - 6],
            compressed[n - 5],
        ]);
        let stored_isize = u32::from_le_bytes([
            compressed[n - 4],
            compressed[n - 3],
            compressed[n - 2],
            compressed[n - 1],
        ]);

        let expected_crc = Crc32::compute(input);
        let expected_isize = (input.len() as u64 & 0xFFFF_FFFF) as u32;

        assert_eq!(
            stored_crc,
            expected_crc,
            "gzip CRC-32 mismatch for input len {}",
            input.len()
        );
        assert_eq!(
            stored_isize,
            expected_isize,
            "gzip ISIZE mismatch for input len {}",
            input.len()
        );
    }
}

/// For empty input: CRC-32 = 0 and ISIZE = 0.
#[test]
fn test_gzip_trailer_empty_input() {
    let compressed = gzip_compress(b"", 6).expect("gzip_compress failed");
    let n = compressed.len();
    let stored_crc = u32::from_le_bytes([
        compressed[n - 8],
        compressed[n - 7],
        compressed[n - 6],
        compressed[n - 5],
    ]);
    let stored_isize = u32::from_le_bytes([
        compressed[n - 4],
        compressed[n - 3],
        compressed[n - 2],
        compressed[n - 1],
    ]);
    assert_eq!(stored_crc, 0, "CRC-32 of empty data must be 0");
    assert_eq!(stored_isize, 0, "ISIZE of empty data must be 0");
}

// ===========================================================================
// Raw DEFLATE block-header compliance
// ===========================================================================

/// The first byte of every raw DEFLATE stream encodes BFINAL (bit 0) and
/// BTYPE (bits 1-2). BTYPE must never be 0b11 (reserved, RFC 1951 §3.2.3).
#[test]
fn test_deflate_raw_btype_never_reserved() {
    let input = b"DEFLATE block type compliance test payload repeated repeated repeated";

    for level in 0u8..=9 {
        let compressed = deflate(input, level)
            .unwrap_or_else(|e| panic!("deflate level {} failed: {}", level, e));

        assert!(!compressed.is_empty(), "level {}: empty output", level);

        let btype = (compressed[0] >> 1) & 0x03;
        assert_ne!(
            btype, 0b11,
            "level {}: BTYPE=0b11 is reserved (RFC 1951 §3.2.3); got byte[0]={:#04x}",
            level, compressed[0]
        );
    }
}

/// For a small input that fits in a single block, BFINAL must be 1 in the
/// first block header (RFC 1951 §3.2.3).
#[test]
fn test_deflate_bfinal_set_on_single_block() {
    // Small input guaranteed to fit in one block at any level.
    let input = b"tiny";

    for level in 1u8..=9 {
        let compressed = deflate(input, level)
            .unwrap_or_else(|e| panic!("deflate level {} failed: {}", level, e));

        assert!(!compressed.is_empty(), "level {}: empty output", level);

        let bfinal = compressed[0] & 0x01;
        assert_eq!(
            bfinal, 1,
            "level {}: BFINAL must be 1 for single-block output (byte[0]={:#04x})",
            level, compressed[0]
        );
    }
}

// ===========================================================================
// Round-trip tests (all wrappers, multiple edge cases)
// ===========================================================================

fn roundtrip_raw(input: &[u8], level: u8) {
    let compressed =
        deflate(input, level).unwrap_or_else(|e| panic!("deflate level {} failed: {}", level, e));
    let decompressed =
        inflate(&compressed).unwrap_or_else(|e| panic!("inflate level {} failed: {}", level, e));
    assert_eq!(
        decompressed, input,
        "raw DEFLATE round-trip mismatch at level {}",
        level
    );
}

fn roundtrip_zlib(input: &[u8], level: u8) {
    let compressed = zlib_compress(input, level)
        .unwrap_or_else(|e| panic!("zlib_compress level {} failed: {}", level, e));
    let decompressed = zlib_decompress(&compressed)
        .unwrap_or_else(|e| panic!("zlib_decompress level {} failed: {}", level, e));
    assert_eq!(
        decompressed, input,
        "zlib round-trip mismatch at level {}",
        level
    );
}

fn roundtrip_gzip(input: &[u8], level: u8) {
    let compressed = gzip_compress(input, level)
        .unwrap_or_else(|e| panic!("gzip_compress level {} failed: {}", level, e));
    let decompressed = gzip_decompress(&compressed)
        .unwrap_or_else(|e| panic!("gzip_decompress level {} failed: {}", level, e));
    assert_eq!(
        decompressed, input,
        "gzip round-trip mismatch at level {}",
        level
    );
}

/// Empty input round-trips through raw DEFLATE, zlib, and gzip.
#[test]
fn test_roundtrip_empty_input() {
    for level in [1u8, 6, 9] {
        roundtrip_raw(b"", level);
        roundtrip_zlib(b"", level);
        roundtrip_gzip(b"", level);
    }
}

/// Single-byte input round-trips through all three wrappers at all levels.
#[test]
fn test_roundtrip_single_byte() {
    for level in 1u8..=9 {
        roundtrip_raw(b"\xA5", level);
        roundtrip_zlib(b"\xA5", level);
        roundtrip_gzip(b"\xA5", level);
    }
}

/// All compression levels 1-9 in zlib mode with a 256 KiB repetitive input.
#[test]
fn test_roundtrip_all_levels_zlib() {
    let pattern = b"All-levels zlib compliance round-trip test. ";
    let mut input = Vec::with_capacity(256 * 1024);
    while input.len() < 256 * 1024 {
        input.extend_from_slice(pattern);
    }
    input.truncate(256 * 1024);

    for level in 1u8..=9 {
        roundtrip_zlib(&input, level);
    }
}

/// Highly repetitive pattern (b"abcde".repeat(10 000)) in gzip mode.
#[test]
fn test_roundtrip_repeated_pattern_gzip() {
    let unit = b"abcde";
    let input: Vec<u8> = unit
        .iter()
        .cycle()
        .take(unit.len() * 10_000)
        .copied()
        .collect();

    roundtrip_gzip(&input, 6);

    // Sanity: highly repetitive data should compress to < 1 % of original
    let compressed = gzip_compress(&input, 6).expect("gzip_compress failed");
    assert!(
        compressed.len() < input.len() / 50,
        "repeated pattern should compress to < 2% of original (got {} -> {})",
        input.len(),
        compressed.len()
    );
}

/// Pseudo-random 64 KiB (LCG) round-trips through all three wrappers.
#[test]
fn test_roundtrip_pseudo_random() {
    // Linear congruential generator — deterministic, no external deps.
    let mut state: u32 = 0xDEAD_BEEF;
    let mut input = Vec::with_capacity(65536);
    for _ in 0..65536 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        input.push((state >> 24) as u8);
    }

    roundtrip_raw(&input, 6);
    roundtrip_zlib(&input, 6);
    roundtrip_gzip(&input, 6);
}

/// Compressing already-compressed data should produce valid output (round-trip
/// must succeed) and the double-compressed size should not exceed ~110% of the
/// original compressed size (incompressible data expands at most slightly).
#[test]
fn test_roundtrip_already_compressed() {
    // Start with pseudo-random data: hard to compress
    let mut state: u32 = 0xCAFE_BABE;
    let mut raw = Vec::with_capacity(32768);
    for _ in 0..32768 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        raw.push((state >> 24) as u8);
    }

    // First pass: compress raw → first_compressed
    let first_compressed = zlib_compress(&raw, 6).expect("first zlib_compress failed");

    // Second pass: compress the already-compressed bytes
    let second_compressed =
        zlib_compress(&first_compressed, 6).expect("second zlib_compress failed");

    // Must be decodable through two layers
    let once_decompressed =
        zlib_decompress(&second_compressed).expect("outer zlib_decompress failed");
    let twice_decompressed =
        zlib_decompress(&once_decompressed).expect("inner zlib_decompress failed");
    assert_eq!(
        twice_decompressed, raw,
        "double-compression round-trip failed"
    );

    // Second compressed should not exceed 110% + 64 bytes of first (incompressible)
    let max_allowed = first_compressed.len() * 11 / 10 + 64;
    assert!(
        second_compressed.len() <= max_allowed,
        "double-compressed ({} bytes) exceeds 110% of single-compressed ({} bytes)",
        second_compressed.len(),
        first_compressed.len()
    );
}

/// 64 KiB round-trip through all wrappers.
#[test]
fn test_roundtrip_64kib() {
    let input: Vec<u8> = (0u8..=255).cycle().take(65536).collect();
    roundtrip_raw(&input, 6);
    roundtrip_zlib(&input, 6);
    roundtrip_gzip(&input, 6);
}

/// 1 MiB round-trip through gzip (exercises multi-block paths).
#[test]
fn test_roundtrip_1mib_gzip() {
    let pattern = b"1MiB gzip round-trip compliance test. ";
    let mut input = Vec::with_capacity(1024 * 1024);
    while input.len() < 1024 * 1024 {
        input.extend_from_slice(pattern);
    }
    input.truncate(1024 * 1024);

    roundtrip_gzip(&input, 6);
}

// ===========================================================================
// Parallel gzip tests
// ===========================================================================

/// GzipStreamDecoder can decompress single-member gzip output from
/// gzip_compress_parallel for sizes that fit within one chunk.
///
/// Sizes tested: 100 KiB (sub-chunk) and 1 MiB (exactly one chunk).
/// Both produce a single GZIP member so the streaming decoder always succeeds.
#[cfg(feature = "parallel")]
#[test]
fn test_parallel_gzip_roundtrip_single_member_via_streaming_decoder() {
    use oxiarc_deflate::GzipStreamDecoder;
    use oxiarc_deflate::gzip_compress_parallel;
    use std::io::Read;

    // chunk size = 1 MiB (DEFAULT_PARALLEL_CHUNK_SIZE)
    let chunk_size = 1024 * 1024;

    // Only single-member cases: <= chunk_size bytes
    let sizes = [
        100 * 1024_usize, // 100 KiB — fits in one chunk
        1024 * 1024,      // exactly one chunk
    ];

    for &sz in &sizes {
        // Deterministic test data
        let input: Vec<u8> = (0u8..=255).cycle().take(sz).collect();

        let compressed = gzip_compress_parallel(&input, 6, chunk_size)
            .unwrap_or_else(|e| panic!("gzip_compress_parallel size={} failed: {}", sz, e));

        // Single-member output: decompressible by both serial and streaming decoder
        let mut decoder = GzipStreamDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .unwrap_or_else(|e| panic!("GzipStreamDecoder size={} failed: {}", sz, e));

        assert_eq!(
            decompressed, input,
            "parallel gzip round-trip mismatch at size {}",
            sz
        );
    }
}

/// GzipStreamDecoder correctly decompresses 3 MiB multi-member output from
/// gzip_compress_parallel.
///
/// Each GZIP member boundary is located by parsing the DEFLATE block structure
/// via `Inflater::inflate_consumed` (bit-level block boundary tracking), then
/// reading the 8-byte GZIP footer that immediately follows.  This replaces the
/// previous unreliable heuristic that scanned for `0x1F 0x8B` magic bytes in
/// the compressed payload — those bytes can appear anywhere in DEFLATE data,
/// causing false-positive splits and decoder failure.
#[cfg(feature = "parallel")]
#[test]
fn test_parallel_gzip_roundtrip_multi_member() {
    use oxiarc_deflate::GzipStreamDecoder;
    use oxiarc_deflate::gzip_compress_parallel;
    use std::io::Read;

    let chunk_size = 1024 * 1024; // 1 MiB chunks → 3 members for 3 MiB input
    let sz = 3 * 1024 * 1024;

    let input: Vec<u8> = (0u8..=255).cycle().take(sz).collect();

    let compressed =
        gzip_compress_parallel(&input, 6, chunk_size).expect("gzip_compress_parallel failed");

    let mut decoder = GzipStreamDecoder::new(&compressed[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .expect("GzipStreamDecoder read_to_end failed");

    assert_eq!(
        decompressed, input,
        "parallel gzip multi-member round-trip failed"
    );
}

/// The single-shot `gzip_decompress` does NOT support multi-member streams.
///
/// For a 3 MiB input with 1 MiB chunks the parallel encoder produces 3 GZIP
/// members; the serial decoder will encounter unexpected data after the first
/// member's trailer and return an error (or silently truncate).
///
/// This test documents that boundary: `GzipStreamDecoder` must be used for
/// multi-member streams; the single-shot `gzip_decompress` is for single members.
#[cfg(feature = "parallel")]
#[test]
fn test_serial_decoder_rejects_multi_member() {
    use oxiarc_deflate::gzip_compress_parallel;

    let chunk_size = 1024 * 1024; // 1 MiB chunks
    let input: Vec<u8> = (0u8..=255).cycle().take(3 * 1024 * 1024).collect();

    let compressed =
        gzip_compress_parallel(&input, 6, chunk_size).expect("gzip_compress_parallel failed");

    // The serial decoder should fail or truncate on a multi-member stream.
    // GzipStreamDecoder must be used for multi-member streams.
    let result = gzip_decompress(&compressed);
    match result {
        Err(_) => { /* expected: serial decoder rejects multi-member input */ }
        Ok(data) if data == input => {
            panic!(
                "Serial GzipDecoder now handles multi-member streams correctly! \
                 Consider adding multi-member support to gzip_decompress."
            );
        }
        Ok(_) => {
            // Truncated / partial result — also counts as documented limitation of the serial path
        }
    }
}

/// gzip_compress_parallel with empty input produces a valid single-member
/// gzip stream decodable by the serial decoder.
#[cfg(feature = "parallel")]
#[test]
fn test_parallel_gzip_empty_input() {
    use oxiarc_deflate::gzip_compress_parallel;

    let compressed =
        gzip_compress_parallel(&[], 6, 1024 * 1024).expect("gzip_compress_parallel failed");

    // Magic bytes must be present
    assert!(compressed.len() >= 18, "empty parallel gzip too short");
    assert_eq!(compressed[0], 0x1F, "empty parallel gzip: bad ID1");
    assert_eq!(compressed[1], 0x8B, "empty parallel gzip: bad ID2");

    // Serial decoder must handle the single-member empty output
    let decompressed =
        gzip_decompress(&compressed).expect("gzip_decompress on empty parallel output failed");
    assert!(
        decompressed.is_empty(),
        "decompressed empty should be empty"
    );
}

/// gzip_compress_parallel with a single byte round-trips via serial decoder
/// (single-member output).
#[cfg(feature = "parallel")]
#[test]
fn test_parallel_gzip_single_byte() {
    use oxiarc_deflate::gzip_compress_parallel;

    let input = b"\xBE";
    let compressed =
        gzip_compress_parallel(input, 6, 1024 * 1024).expect("gzip_compress_parallel failed");

    let decompressed = gzip_decompress(&compressed)
        .expect("gzip_decompress on single-byte parallel output failed");
    assert_eq!(
        decompressed.as_slice(),
        input.as_ref(),
        "single-byte parallel gzip round-trip failed"
    );
}

/// gzip_compress_parallel returns decodable output: decompress through the
/// streaming decoder and compare to original input (100 KiB, sub-chunk).
#[cfg(feature = "parallel")]
#[test]
fn test_parallel_gzip_byte_count_return() {
    use oxiarc_deflate::GzipStreamDecoder;
    use oxiarc_deflate::gzip_compress_parallel;
    use std::io::Read;

    let input: Vec<u8> = (0u8..=255).cycle().take(100 * 1024).collect();
    let compressed =
        gzip_compress_parallel(&input, 6, 1024 * 1024).expect("gzip_compress_parallel failed");

    let mut decoder = GzipStreamDecoder::new(&compressed[..]);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .expect("streaming decoder failed");

    assert_eq!(out.len(), input.len(), "byte count mismatch");
    assert_eq!(out, input, "content mismatch in parallel gzip output");
}

// ===========================================================================
// Strict spec-compliant inflater (regression guard for the dynamic-Huffman
// "invalid code lengths set" bug).
//
// The encoder previously emitted *incomplete* Huffman code-length tables
// (Kraft sum < 1.0) for the dynamic-block (BTYPE=10) path. oxiarc's own
// inflater is lenient and accepted them, but spec-compliant decoders
// (zlib `inflate_table`) reject incomplete literal/length and code-length
// tables with "invalid code lengths set".
//
// The decoder below is deliberately STRICT and self-contained (no external
// crates, no reliance on oxiarc's lenient inflater): when it builds a Huffman
// table it verifies the code is *complete* exactly as zlib does. A
// regression of the old bug therefore fails these tests at the
// `build_huffman` step, even though oxiarc's production inflater would still
// round-trip the broken stream.
// ===========================================================================

/// LSB-first bit reader over a DEFLATE byte stream.
struct SpecBitReader<'a> {
    data: &'a [u8],
    /// Absolute bit position from the start of `data`.
    bit_pos: usize,
}

impl<'a> SpecBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    /// Read `n` bits LSB-first (DEFLATE bit order), returning them as a u32.
    fn read_bits(&mut self, n: u8) -> u32 {
        let mut value = 0u32;
        for i in 0..n {
            let byte_index = self.bit_pos >> 3;
            assert!(
                byte_index < self.data.len(),
                "spec inflater ran past end of stream (bit {})",
                self.bit_pos
            );
            let bit = (self.data[byte_index] >> (self.bit_pos & 7)) & 1;
            value |= (bit as u32) << i;
            self.bit_pos += 1;
        }
        value
    }

    /// Skip to the next byte boundary (used before stored-block LEN/NLEN).
    fn align_to_byte(&mut self) {
        let rem = self.bit_pos & 7;
        if rem != 0 {
            self.bit_pos += 8 - rem;
        }
    }
}

/// A canonical Huffman decoding table built from per-symbol code lengths.
///
/// Construction enforces RFC 1951 / zlib completeness: the code must be
/// neither over-subscribed nor incomplete (with the single documented
/// exception of an all-zero table, which represents "no codes").
struct SpecHuffman {
    /// Map from (length, canonical_code) -> symbol.
    codes: std::collections::HashMap<(u8, u32), u16>,
    max_len: u8,
}

impl SpecHuffman {
    /// Build a strict canonical Huffman decoder from `lengths`.
    ///
    /// `allow_incomplete_single` permits the lone legal incomplete case used by
    /// the *distance* alphabet: exactly one distance code of length 1 (RFC 1951
    /// allows a single-distance code). All other incomplete or over-subscribed
    /// dynamic tables are rejected, mirroring zlib's `inflate_table`.
    ///
    /// `fixed_table` marks the two RFC 1951 §3.2.6 *fixed* tables, which zlib
    /// hardcodes and accepts despite the fixed distance code being incomplete
    /// (30 five-bit codes => Kraft 0.9375; codes 30/31 are reserved). The
    /// dynamic-Huffman regression we are guarding never sets this flag.
    fn build(
        lengths: &[u8],
        alphabet: &str,
        allow_incomplete_single: bool,
        fixed_table: bool,
    ) -> Self {
        let max_len = lengths.iter().copied().max().unwrap_or(0);
        if max_len == 0 {
            return Self {
                codes: std::collections::HashMap::new(),
                max_len: 0,
            };
        }

        // Count codes of each length and check completeness via the "left"
        // (available code space) accounting that zlib uses.
        let mut bl_count = vec![0u32; (max_len as usize) + 1];
        let mut used = 0u32;
        for &l in lengths {
            if l > 0 {
                bl_count[l as usize] += 1;
                used += 1;
            }
        }

        // Single-code special case (one symbol of length 1): incomplete (Kraft
        // 0.5). Allowed only for the distance alphabet.
        if used == 1 && bl_count.get(1).copied().unwrap_or(0) == 1 && !fixed_table {
            assert!(
                allow_incomplete_single,
                "{alphabet} alphabet has a single 1-bit code (incomplete, Kraft=0.5) \
                 which spec decoders reject as 'invalid code lengths set'"
            );
        }

        // zlib's `left` accounting: start with 1 code at length 0 doubling each
        // level, subtracting the codes used at that length. `left` must reach
        // exactly 0 for a complete code; `left < 0` is over-subscribed; `left >
        // 0` after the last length is incomplete.
        let mut left: i64 = 1;
        for (len, &count) in bl_count.iter().enumerate().skip(1) {
            left <<= 1;
            left -= count as i64;
            assert!(
                left >= 0,
                "{alphabet} Huffman table is OVER-SUBSCRIBED at length {len}"
            );
        }
        if left != 0 && !fixed_table {
            // Incomplete code. Allowed only for the documented single-distance
            // case (already asserted above); everything else is the bug we are
            // guarding against.
            let is_single_distance =
                allow_incomplete_single && used == 1 && bl_count.get(1).copied().unwrap_or(0) == 1;
            assert!(
                is_single_distance,
                "{alphabet} Huffman table is INCOMPLETE (left={left}, Kraft sum < 1.0) — \
                 spec decoders reject this as 'invalid code lengths set'"
            );
        }

        // Assign canonical codes (RFC 1951 §3.2.2).
        let mut next_code = vec![0u32; (max_len as usize) + 1];
        let mut code = 0u32;
        for bits in 1..=max_len as usize {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        let mut codes = std::collections::HashMap::new();
        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                let c = next_code[len as usize];
                next_code[len as usize] += 1;
                codes.insert((len, c), sym as u16);
            }
        }

        Self { codes, max_len }
    }

    /// Decode one symbol MSB-first over the canonical code space.
    fn decode(&self, reader: &mut SpecBitReader) -> u16 {
        assert!(self.max_len > 0, "decode on empty Huffman table");
        let mut code = 0u32;
        for len in 1..=self.max_len {
            code = (code << 1) | reader.read_bits(1);
            if let Some(&sym) = self.codes.get(&(len, code)) {
                return sym;
            }
        }
        panic!(
            "spec inflater: no matching Huffman code (max_len={})",
            self.max_len
        );
    }
}

/// Code-length alphabet order (RFC 1951 §3.2.7).
const SPEC_CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Length base/extra-bit tables for the literal/length alphabet (codes 257-285).
const SPEC_LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const SPEC_LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Distance base/extra-bit tables for the distance alphabet (codes 0-29).
const SPEC_DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const SPEC_DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Fixed literal/length code lengths (RFC 1951 §3.2.6).
fn spec_fixed_litlen_lengths() -> Vec<u8> {
    let mut l = vec![0u8; 288];
    for (i, len) in l.iter_mut().enumerate() {
        *len = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    l
}

/// Fully inflate a raw DEFLATE byte stream using the strict decoder above.
///
/// Panics (failing the test) if any Huffman table is over-subscribed or
/// incomplete — i.e. exactly the spec violation that produced the
/// "invalid code lengths set" error in real zlib.
fn spec_inflate(data: &[u8]) -> Vec<u8> {
    let mut reader = SpecBitReader::new(data);
    let mut out: Vec<u8> = Vec::new();

    loop {
        let bfinal = reader.read_bits(1);
        let btype = reader.read_bits(2);

        match btype {
            0 => {
                // Stored block.
                reader.align_to_byte();
                let len = reader.read_bits(16) as usize;
                let nlen = reader.read_bits(16);
                assert_eq!(
                    len as u32 & 0xFFFF,
                    !nlen & 0xFFFF,
                    "stored block LEN/NLEN mismatch"
                );
                for _ in 0..len {
                    out.push(reader.read_bits(8) as u8);
                }
            }
            1 | 2 => {
                let (litlen, dist) = if btype == 1 {
                    let litlen = SpecHuffman::build(
                        &spec_fixed_litlen_lengths(),
                        "fixed-litlen",
                        false,
                        true,
                    );
                    let dist = SpecHuffman::build(&[5u8; 30], "fixed-distance", false, true);
                    (litlen, dist)
                } else {
                    // Dynamic block header.
                    let hlit = reader.read_bits(5) as usize + 257;
                    let hdist = reader.read_bits(5) as usize + 1;
                    let hclen = reader.read_bits(4) as usize + 4;

                    let mut cl_lengths = [0u8; 19];
                    for i in 0..hclen {
                        cl_lengths[SPEC_CL_ORDER[i]] = reader.read_bits(3) as u8;
                    }
                    // The code-length code MUST be complete.
                    let cl_huff = SpecHuffman::build(&cl_lengths, "code-length", false, false);

                    // Decode the combined litlen+dist length sequence.
                    let total = hlit + hdist;
                    let mut all_lengths: Vec<u8> = Vec::with_capacity(total);
                    while all_lengths.len() < total {
                        let sym = cl_huff.decode(&mut reader);
                        match sym {
                            0..=15 => all_lengths.push(sym as u8),
                            16 => {
                                let repeat = reader.read_bits(2) as usize + 3;
                                let prev = *all_lengths
                                    .last()
                                    .expect("repeat-16 with no previous length");
                                all_lengths.resize(all_lengths.len() + repeat, prev);
                            }
                            17 => {
                                let repeat = reader.read_bits(3) as usize + 3;
                                all_lengths.resize(all_lengths.len() + repeat, 0);
                            }
                            18 => {
                                let repeat = reader.read_bits(7) as usize + 11;
                                all_lengths.resize(all_lengths.len() + repeat, 0);
                            }
                            _ => panic!("invalid code-length symbol {sym}"),
                        }
                    }
                    assert_eq!(
                        all_lengths.len(),
                        total,
                        "code-length run overflowed HLIT+HDIST"
                    );

                    let litlen_lengths = &all_lengths[..hlit];
                    let dist_lengths = &all_lengths[hlit..];

                    // litlen MUST be complete; distance may be the single legal
                    // incomplete case (one 1-bit code) or empty.
                    let litlen = SpecHuffman::build(litlen_lengths, "litlen", false, false);
                    let dist = SpecHuffman::build(dist_lengths, "distance", true, false);
                    (litlen, dist)
                };

                // Decode symbols until end-of-block (256).
                loop {
                    let sym = litlen.decode(&mut reader);
                    match sym {
                        0..=255 => out.push(sym as u8),
                        256 => break,
                        257..=285 => {
                            let li = (sym - 257) as usize;
                            let length = SPEC_LEN_BASE[li] as usize
                                + reader.read_bits(SPEC_LEN_EXTRA[li]) as usize;
                            let dsym = dist.decode(&mut reader) as usize;
                            assert!(dsym < 30, "distance symbol {dsym} out of range");
                            let distance = SPEC_DIST_BASE[dsym] as usize
                                + reader.read_bits(SPEC_DIST_EXTRA[dsym]) as usize;
                            assert!(
                                distance <= out.len(),
                                "back-reference distance {distance} exceeds output length {}",
                                out.len()
                            );
                            let start = out.len() - distance;
                            for k in 0..length {
                                let b = out[start + k];
                                out.push(b);
                            }
                        }
                        _ => panic!("invalid litlen symbol {sym}"),
                    }
                }
            }
            _ => panic!("invalid BTYPE 3 (reserved)"),
        }

        if bfinal == 1 {
            break;
        }
    }

    out
}

/// Strip the 2-byte zlib header and 4-byte Adler-32 trailer, returning the
/// raw DEFLATE payload.
fn strip_zlib(compressed: &[u8]) -> &[u8] {
    assert!(compressed.len() >= 6, "zlib stream too short");
    &compressed[2..compressed.len() - 4]
}

/// Build a deterministic 512x512x3 RGB scanline stream (each row prefixed with
/// a 0 filter byte), mirroring a real PNG IDAT input.
fn png_scanline_stream() -> Vec<u8> {
    let w = 512usize;
    let h = 512usize;
    let mut out = Vec::with_capacity(h * (1 + w * 3));
    for y in 0..h {
        out.push(0u8);
        for x in 0..w {
            out.push(((x * 7 + y * 3) & 0xFF) as u8);
            out.push(((x ^ y) & 0xFF) as u8);
            out.push(((x.wrapping_mul(y) >> 3) & 0xFF) as u8);
        }
    }
    out
}

/// Self-test: the strict inflater must REJECT a hand-crafted incomplete
/// code-length table (sanity check that the guard actually fires).
#[test]
#[should_panic(expected = "INCOMPLETE")]
fn test_spec_inflater_rejects_incomplete_codelen() {
    // Construct a minimal dynamic block whose code-length code is incomplete:
    // a single code-length symbol of length 2 (Kraft = 0.25 < 1.0).
    // BFINAL=1, BTYPE=10, HLIT=0(=>257), HDIST=0(=>1), HCLEN=0(=>4),
    // then 4 x 3-bit code-length code lengths. We set only one to a nonzero
    // value of 2 so the code-length alphabet is incomplete.
    let mut bits: Vec<u8> = Vec::new();
    let mut acc = 0u32;
    let mut nbits = 0u8;
    let push = |val: u32, n: u8, bits: &mut Vec<u8>, acc: &mut u32, nbits: &mut u8| {
        *acc |= (val & ((1u32 << n) - 1)) << *nbits;
        *nbits += n;
        while *nbits >= 8 {
            bits.push((*acc & 0xFF) as u8);
            *acc >>= 8;
            *nbits -= 8;
        }
    };
    push(1, 1, &mut bits, &mut acc, &mut nbits); // BFINAL=1
    push(2, 2, &mut bits, &mut acc, &mut nbits); // BTYPE=10 dynamic
    push(0, 5, &mut bits, &mut acc, &mut nbits); // HLIT=0
    push(0, 5, &mut bits, &mut acc, &mut nbits); // HDIST=0
    push(0, 4, &mut bits, &mut acc, &mut nbits); // HCLEN=0 -> 4 codes
    // 4 code-length code lengths (order: 16,17,18,0). Make only symbol "16"
    // have length 2 -> incomplete code-length alphabet.
    push(2, 3, &mut bits, &mut acc, &mut nbits);
    push(0, 3, &mut bits, &mut acc, &mut nbits);
    push(0, 3, &mut bits, &mut acc, &mut nbits);
    push(0, 3, &mut bits, &mut acc, &mut nbits);
    if nbits > 0 {
        bits.push((acc & 0xFF) as u8);
    }
    // This must panic with "INCOMPLETE" inside SpecHuffman::build.
    let _ = spec_inflate(&bits);
}

/// Core regression: every level 1-9 must produce a *spec-compliant* zlib
/// stream for a repetitive buffer that takes the dynamic-Huffman path at
/// level >= 5. Pre-fix, levels 5-9 emitted incomplete code-length tables and
/// this test would panic with "INCOMPLETE"/"invalid code lengths set".
#[test]
fn test_dynamic_huffman_spec_compliant_repetitive_all_levels() {
    let mut data = Vec::new();
    let pat = b"the quick brown fox jumps over the lazy dog. ";
    while data.len() < 200_000 {
        data.extend_from_slice(pat);
    }

    for level in 1u8..=9 {
        let compressed = zlib_compress(&data, level)
            .unwrap_or_else(|e| panic!("zlib_compress level {level} failed: {e:?}"));
        let raw = strip_zlib(&compressed);
        let decoded = spec_inflate(raw);
        assert_eq!(
            decoded, data,
            "strict spec inflater mismatch at level {level}"
        );
    }
}

/// The 512x512 RGB PNG-scanline stream must be spec-compliant at all levels
/// 1-9 (this is the real-world ~750 KB case from the bug report).
#[test]
fn test_dynamic_huffman_spec_compliant_png_scanlines_all_levels() {
    let data = png_scanline_stream();
    for level in 1u8..=9 {
        let compressed = zlib_compress(&data, level)
            .unwrap_or_else(|e| panic!("zlib_compress level {level} failed: {e:?}"));
        let decoded = spec_inflate(strip_zlib(&compressed));
        assert_eq!(decoded, data, "PNG scanline spec mismatch at level {level}");
    }
}

/// An incompressible (pseudo-random) buffer must also be spec-compliant at all
/// levels (this path typically stays on fixed Huffman, but we verify it).
#[test]
fn test_dynamic_huffman_spec_compliant_random_all_levels() {
    // Deterministic LCG, no rand crate needed.
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut data = Vec::with_capacity(200_000);
    for _ in 0..200_000 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        data.push((state >> 33) as u8);
    }
    for level in 1u8..=9 {
        let compressed = zlib_compress(&data, level)
            .unwrap_or_else(|e| panic!("zlib_compress level {level} failed: {e:?}"));
        let decoded = spec_inflate(strip_zlib(&compressed));
        assert_eq!(
            decoded, data,
            "random buffer spec mismatch at level {level}"
        );
    }
}

/// The gzip path shares the same DEFLATE encoder; verify its DEFLATE payload
/// is spec-compliant at level 9 (dynamic block) for a repetitive buffer.
#[test]
fn test_gzip_dynamic_huffman_spec_compliant_level9() {
    let mut data = Vec::new();
    let pat = b"compress me compress me 0123456789 ";
    while data.len() < 150_000 {
        data.extend_from_slice(pat);
    }
    let compressed = gzip_compress(&data, 9).expect("gzip_compress level 9 failed");
    // gzip: 10-byte fixed header, then DEFLATE, then 8-byte trailer (CRC32+ISIZE).
    assert!(compressed.len() > 18, "gzip stream too short");
    let raw = &compressed[10..compressed.len() - 8];
    let decoded = spec_inflate(raw);
    assert_eq!(
        decoded, data,
        "gzip dynamic-Huffman payload not spec-compliant"
    );
}

/// Single-distinct-symbol input ("allsame") used to produce a 1-bit (Kraft
/// 0.5) literal/length table. The encoder must now synthesize a complete code
/// (phantom second symbol) so the litlen table is accepted by strict decoders.
#[test]
fn test_dynamic_huffman_spec_compliant_single_symbol() {
    // Highly repetitive single byte -> tiny alphabet -> dynamic path at L9.
    let data = vec![b'X'; 100_000];
    for level in [5u8, 6, 9] {
        let compressed = zlib_compress(&data, level)
            .unwrap_or_else(|e| panic!("zlib_compress level {level} failed: {e:?}"));
        let decoded = spec_inflate(strip_zlib(&compressed));
        assert_eq!(
            decoded, data,
            "single-symbol spec mismatch at level {level}"
        );
    }
}

/// The graph-based optimal parser (`Deflater::with_optimal_parsing`) shares the
/// fixed `HuffmanBuilder`; verify its output is spec-compliant, including for
/// inputs larger than the 64 KiB LZ77 window (regression for the
/// `find_all_matches` window-overflow panic).
#[test]
fn test_optimal_parser_spec_compliant_large_input() {
    use oxiarc_deflate::Deflater;

    let mut data = Vec::new();
    let pat = b"the quick brown fox jumps over the lazy dog 0123456789 ";
    while data.len() < 150_000 {
        data.extend_from_slice(pat);
    }

    for level in [6u8, 9] {
        let mut d = Deflater::with_optimal_parsing(level);
        let raw = d
            .compress_to_vec(&data)
            .unwrap_or_else(|e| panic!("optimal compress level {level} failed: {e:?}"));
        let decoded = spec_inflate(&raw);
        assert_eq!(
            decoded, data,
            "optimal parser spec mismatch at level {level}"
        );
    }
}

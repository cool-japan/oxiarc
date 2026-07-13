//! Regression tests for the streaming/trait wrappers around the (verified
//! bit-exact) core DEFLATE/zlib/gzip codec.
//!
//! Each test targets one of the confirmed wrapper bugs:
//! - DEFLATE-01: `Inflater::decompress` silently truncated payloads larger
//!   than the caller's output buffer (any payload >32 KiB via
//!   `decompress_all`) and reported `Done`.
//! - DEFLATE-02: repeated `Deflater::deflate(_, false)` byte-padded between
//!   non-final blocks, producing a stream oxiarc's own `inflate` rejected.
//! - DEFLATE-03: `Compressor::compress` silently discarded compressed bytes
//!   that overflowed the caller's output buffer and reported `Done`.
//! - DEFLATE-04: `GzipDecoder` failed on concatenated multi-member gzip
//!   (RFC 1952 §2.2) with a spurious CRC mismatch — including oxiarc's own
//!   `compress_gzip_parallel` output.
//! - DEFLATE-05: `ZlibStreamDecoder`'s concatenation fallback re-ran full
//!   decompression per candidate offset (quadratic, bomb-amplifiable DoS).
//!
//! The gzip/zlib byte fixtures below were produced by CPython's `gzip` /
//! `zlib` modules (deterministic: `mtime=0`), so the decode direction is
//! exercised against genuine reference-encoder output on every test run
//! without any external tool. Broader live differential tests against
//! `python3` and the system `gzip` CLI live in `tests/zlib_oracle.rs`
//! (feature `zlib-oracle`).

use oxiarc_core::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};
use oxiarc_deflate::{
    Deflater, Inflater, ZlibStreamDecoder, deflate, gzip_compress, gzip_decompress, inflate,
    zlib_compress,
};
use std::io::Read;

// ---------------------------------------------------------------------------
// Reference fixtures (produced by CPython 3 `gzip`/`zlib`, mtime=0)
// ---------------------------------------------------------------------------

/// `gzip.compress(b"hello ", 9, mtime=0) + gzip.compress(b"multi-member ", 1,
/// mtime=0) + gzip.compress(b"gzip world!", 6, mtime=0)` — three concatenated
/// members from the reference encoder.
const PY_GZIP_MULTI_MEMBER: [u8; 90] = [
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57,
    0x00, 0x00, 0xf6, 0xf9, 0x81, 0xed, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x04, 0xff, 0xcb, 0x2d, 0xcd, 0x29, 0xc9, 0xd4, 0xcd, 0x4d, 0xcd, 0x4d, 0x4a, 0x2d,
    0x52, 0x00, 0x00, 0xec, 0x66, 0x33, 0xe7, 0x0d, 0x00, 0x00, 0x00, 0x1f, 0x8b, 0x08, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xff, 0x4b, 0xaf, 0xca, 0x2c, 0x50, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x51,
    0x04, 0x00, 0xbe, 0x81, 0x68, 0xa0, 0x0b, 0x00, 0x00, 0x00,
];

/// `gzip.compress(b"hello ", 9, mtime=0)` followed by 7 zero bytes of
/// trailing padding (as produced by tape-block/aligned writers; both the
/// `gzip` CLI and Python tolerate it).
const PY_GZIP_TRAILING_ZEROS: [u8; 33] = [
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57,
    0x00, 0x00, 0xf6, 0xf9, 0x81, 0xed, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00,
];

/// `zlib.compress(b"first zlib member. ", 9) + zlib.compress(b"second zlib
/// member!", 1)` — two concatenated zlib streams from the reference encoder.
const PY_ZLIB_CONCAT: [u8; 54] = [
    0x78, 0xda, 0x4b, 0xcb, 0x2c, 0x2a, 0x2e, 0x51, 0xa8, 0xca, 0xc9, 0x4c, 0x52, 0xc8, 0x4d, 0xcd,
    0x4d, 0x4a, 0x2d, 0xd2, 0x53, 0x00, 0x00, 0x49, 0x17, 0x06, 0xe0, 0x78, 0x01, 0x2b, 0x4e, 0x4d,
    0xce, 0xcf, 0x4b, 0x51, 0xa8, 0xca, 0xc9, 0x4c, 0x52, 0xc8, 0x4d, 0xcd, 0x4d, 0x4a, 0x2d, 0x52,
    0x04, 0x00, 0x48, 0xe1, 0x07, 0x07,
];

/// Deterministic mixed-content test payload sized to cross the 32 KiB
/// `decompress_all` buffer and the 32 KiB LZ77 window multiple times.
fn make_payload(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    while data.len() < len {
        // Alternate compressible text runs and pseudo-random bytes.
        let text = format!("chunk {} of the wrapper regression payload; ", data.len());
        data.extend_from_slice(text.as_bytes());
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push((state >> 32) as u8);
        }
    }
    data.truncate(len);
    data
}

// ---------------------------------------------------------------------------
// DEFLATE-01: Inflater::decompress must stream, never truncate
// ---------------------------------------------------------------------------

/// `decompress_all` uses a fixed 32 KiB scratch buffer; before the fix any
/// payload larger than that was silently truncated to 32,768 bytes.
#[test]
fn test_inflater_decompress_all_larger_than_internal_buffer() {
    for size in [32_769usize, 50_000, 200_000, 1_440_800] {
        let original = make_payload(size);
        let compressed = deflate(&original, 6).expect("deflate failed");

        let mut inflater = Inflater::new();
        let decompressed = inflater
            .decompress_all(&compressed)
            .expect("decompress_all failed");

        assert_eq!(
            decompressed.len(),
            original.len(),
            "decompress_all truncated a {size}-byte payload"
        );
        assert_eq!(decompressed, original, "decompress_all corrupted payload");
        assert!(inflater.is_finished(), "inflater must report finished");
    }
}

/// Drive the `Decompressor` trait manually with a small bounded output
/// buffer: `NeedsOutput` must be reported while decoded bytes remain and
/// `Done` only once fully drained.
#[test]
fn test_inflater_decompress_bounded_output_buffer() {
    let original = make_payload(100_000);
    let compressed = deflate(&original, 6).expect("deflate failed");

    let mut inflater = Inflater::new();
    let mut out = Vec::new();
    let mut buf = [0u8; 1000];
    let mut input_pos = 0usize;
    let mut saw_needs_output = false;

    loop {
        let (consumed, produced, status) = inflater
            .decompress(&compressed[input_pos..], &mut buf)
            .expect("decompress failed");
        input_pos += consumed;
        out.extend_from_slice(&buf[..produced]);
        match status {
            DecompressStatus::Done => break,
            DecompressStatus::NeedsOutput => {
                saw_needs_output = true;
                assert!(
                    !inflater.is_finished(),
                    "must not report finished while bytes remain undelivered"
                );
            }
            other => panic!("unexpected status {other:?}"),
        }
        assert!(out.len() <= original.len(), "produced more than expected");
    }

    assert!(
        saw_needs_output,
        "expected NeedsOutput with a 1000-byte buffer"
    );
    assert_eq!(out, original, "bounded-buffer streaming decode mismatch");
    assert!(inflater.is_finished());
}

// ---------------------------------------------------------------------------
// DEFLATE-02: bit continuity across deflate(_, false) calls
// ---------------------------------------------------------------------------

/// Repeated non-final `deflate` calls followed by a final one must produce a
/// single continuous DEFLATE stream that oxiarc's own `inflate` accepts.
/// Before the fix, byte padding was inserted between the non-final blocks
/// and `inflate` failed with "Unexpected end of file".
#[test]
fn test_deflater_repeated_nonfinal_calls_bit_continuity() {
    for level in [1u8, 5, 6, 9] {
        let chunk1 = make_payload(10_000);
        let chunk2 = make_payload(20_000);
        let chunk3 = b"final chunk".to_vec();

        let mut deflater = Deflater::new(level);
        let mut compressed = Vec::new();
        deflater
            .deflate(&chunk1, &mut compressed, false)
            .expect("deflate chunk1 failed");
        deflater
            .deflate(&chunk2, &mut compressed, false)
            .expect("deflate chunk2 failed");
        deflater
            .deflate(&chunk3, &mut compressed, true)
            .expect("deflate chunk3 failed");

        let mut expected = chunk1;
        expected.extend_from_slice(&chunk2);
        expected.extend_from_slice(&chunk3);

        let decompressed = inflate(&compressed)
            .unwrap_or_else(|e| panic!("level {level}: inflate rejected multi-call stream: {e}"));
        assert_eq!(
            decompressed, expected,
            "level {level}: multi-call stream corrupted"
        );
    }
}

/// The trait-level `compress_all` convenience (which chains FlushMode::None
/// calls and a final Finish) must emit one continuous stream.
#[test]
fn test_compressor_compress_all_roundtrip() {
    for level in [0u8, 1, 6, 9] {
        let original = make_payload(150_000);
        let mut deflater = Deflater::new(level);
        let compressed = deflater
            .compress_all(&original)
            .expect("compress_all failed");

        let decompressed = inflate(&compressed)
            .unwrap_or_else(|e| panic!("level {level}: inflate rejected compress_all output: {e}"));
        assert_eq!(
            decompressed, original,
            "level {level}: compress_all roundtrip mismatch"
        );
    }
}

// ---------------------------------------------------------------------------
// DEFLATE-03: Compressor::compress with a bounded output buffer
// ---------------------------------------------------------------------------

/// Feed input in chunks through a 64-byte output buffer; the compressor must
/// report `NeedsOutput` and drain across calls instead of truncating.
#[test]
fn test_compressor_bounded_output_buffer_roundtrip() {
    let original = make_payload(120_000);
    let mut deflater = Deflater::new(6);
    let mut compressed = Vec::new();
    let mut buf = [0u8; 64];
    let mut input_pos = 0usize;
    let mut saw_needs_output = false;
    const INPUT_CHUNK: usize = 8_192;

    loop {
        let end = (input_pos + INPUT_CHUNK).min(original.len());
        let flush = if input_pos >= original.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let (consumed, produced, status) = deflater
            .compress(&original[input_pos..end], &mut buf, flush)
            .expect("compress failed");
        input_pos += consumed;
        compressed.extend_from_slice(&buf[..produced]);
        match status {
            CompressStatus::Done => break,
            CompressStatus::NeedsOutput => saw_needs_output = true,
            CompressStatus::NeedsInput => {}
            other => panic!("unexpected status {other:?}"),
        }
    }

    assert!(
        saw_needs_output,
        "expected NeedsOutput with a 64-byte buffer"
    );
    let decompressed =
        inflate(&compressed).expect("inflate rejected bounded-buffer compressor output");
    assert_eq!(decompressed, original, "bounded-buffer compress mismatch");
}

/// A pure drain call (`consumed == 0`) must not lose the caller's input:
/// verify the reported consumed counts sum exactly to the input length.
#[test]
fn test_compressor_consumed_accounting() {
    let original = make_payload(40_000);
    let mut deflater = Deflater::new(6);
    let mut buf = [0u8; 128];
    let mut total_consumed = 0usize;
    let mut compressed = Vec::new();

    loop {
        let flush = if total_consumed >= original.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let (consumed, produced, status) = deflater
            .compress(&original[total_consumed..], &mut buf, flush)
            .expect("compress failed");
        total_consumed += consumed;
        assert!(total_consumed <= original.len(), "over-consumed input");
        compressed.extend_from_slice(&buf[..produced]);
        if status == CompressStatus::Done {
            break;
        }
    }

    assert_eq!(
        total_consumed,
        original.len(),
        "consumed-input accounting broken"
    );
    assert_eq!(
        inflate(&compressed).expect("inflate failed"),
        original,
        "consumed-accounting roundtrip mismatch"
    );
}

// ---------------------------------------------------------------------------
// DEFLATE-04: multi-member gzip in GzipDecoder
// ---------------------------------------------------------------------------

/// Reference-produced (CPython gzip) multi-member stream must decode to the
/// concatenation of all members' contents.
#[test]
fn test_gzip_decoder_python_multi_member_fixture() {
    let decompressed = gzip_decompress(&PY_GZIP_MULTI_MEMBER)
        .expect("GzipDecoder must decode reference multi-member gzip");
    assert_eq!(decompressed, b"hello multi-member gzip world!");
}

/// Trailing zero padding after the final member is tolerated (gzip CLI /
/// Python semantics), while non-zero trailing garbage is an error.
#[test]
fn test_gzip_decoder_trailing_bytes_semantics() {
    let decompressed =
        gzip_decompress(&PY_GZIP_TRAILING_ZEROS).expect("trailing zero padding must be tolerated");
    assert_eq!(decompressed, b"hello ");

    let mut garbage = PY_GZIP_TRAILING_ZEROS[..26].to_vec(); // member only
    garbage.extend_from_slice(b"garbage!");
    assert!(
        gzip_decompress(&garbage).is_err(),
        "non-zero trailing garbage must be rejected, not silently ignored"
    );
}

/// oxiarc-encoded members concatenated pairwise must decode via the serial
/// `GzipDecoder` (previously a spurious CrcMismatch).
#[test]
fn test_gzip_decoder_own_concatenated_members() {
    let a = make_payload(70_000);
    let b = make_payload(33_000);

    let mut concatenated = gzip_compress(&a, 9).expect("gzip_compress a failed");
    concatenated.extend_from_slice(&gzip_compress(&b, 1).expect("gzip_compress b failed"));

    let decompressed =
        gzip_decompress(&concatenated).expect("GzipDecoder must decode concatenated own members");
    let mut expected = a;
    expected.extend_from_slice(&b);
    assert_eq!(decompressed, expected, "multi-member roundtrip mismatch");
}

/// `compress_gzip_parallel` emits one member per 512 KiB chunk; its output
/// is documented as decodable by `GzipDecoder` and now must actually be.
#[cfg(feature = "parallel")]
#[test]
fn test_gzip_decoder_decodes_compress_gzip_parallel_output() {
    use oxiarc_deflate::{GzipStreamDecoder, compress_gzip_parallel};

    // > 2 chunks of 512 KiB → at least 3 members.
    let original = make_payload(1_300_000);
    let compressed = compress_gzip_parallel(&original, 6).expect("parallel compress failed");

    let decompressed = gzip_decompress(&compressed)
        .expect("GzipDecoder must decode compress_gzip_parallel output");
    assert_eq!(decompressed, original, "parallel-output roundtrip mismatch");

    // The streaming decoder must agree byte-for-byte.
    let mut stream = GzipStreamDecoder::new(&compressed[..]);
    let mut via_stream = Vec::new();
    stream
        .read_to_end(&mut via_stream)
        .expect("GzipStreamDecoder failed on parallel output");
    assert_eq!(via_stream, original, "stream/serial decoder disagreement");
}

/// A truncated second member must be an error, never a silent partial `Ok`.
#[test]
fn test_gzip_decoder_truncated_second_member_errors() {
    let a = make_payload(5_000);
    let b = make_payload(5_000);
    let mut concatenated = gzip_compress(&a, 6).expect("gzip_compress a failed");
    let second = gzip_compress(&b, 6).expect("gzip_compress b failed");
    concatenated.extend_from_slice(&second[..second.len() - 5]); // drop trailer bytes

    assert!(
        gzip_decompress(&concatenated).is_err(),
        "truncated trailing member must be an error, not silent truncation"
    );
}

// ---------------------------------------------------------------------------
// DEFLATE-05: ZlibStreamDecoder concatenation without quadratic re-decoding
// ---------------------------------------------------------------------------

/// Reference-produced (CPython zlib) concatenated members decode in order.
#[test]
fn test_zlib_stream_decoder_python_concat_fixture() {
    let mut decoder = ZlibStreamDecoder::new(&PY_ZLIB_CONCAT[..]);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .expect("ZlibStreamDecoder must decode reference concatenated members");
    assert_eq!(output, b"first zlib member. second zlib member!");
}

/// oxiarc-encoded concatenated zlib members decode in order (previously
/// handled only via the quadratic prefix-scanning fallback).
#[test]
fn test_zlib_stream_decoder_own_concatenated_members() {
    let a = make_payload(60_000);
    let b = make_payload(45_000);
    let mut concatenated = zlib_compress(&a, 9).expect("zlib_compress a failed");
    concatenated.extend_from_slice(&zlib_compress(&b, 1).expect("zlib_compress b failed"));

    let mut decoder = ZlibStreamDecoder::new(&concatenated[..]);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .expect("ZlibStreamDecoder failed on concatenated members");
    let mut expected = a;
    expected.extend_from_slice(&b);
    assert_eq!(output, expected, "concatenated zlib roundtrip mismatch");
}

/// The adversarial stream from the audit: a highly-compressible member whose
/// Adler-32 is corrupted, followed by kilobytes of bytes that all look like
/// plausible zlib headers. The old fallback re-ran full decompression at
/// every candidate offset (>60 s on ~22 KiB); the fix must fail fast.
#[test]
fn test_zlib_stream_decoder_adversarial_stream_fails_fast() {
    // ~1 MiB of zeros compresses to ~1 KiB and re-expands on every probe.
    let bomb_plain = vec![0u8; 1_000_000];
    let mut stream = zlib_compress(&bomb_plain, 9).expect("zlib_compress failed");
    let last = stream.len() - 1;
    stream[last] ^= 0xFF; // corrupt the Adler-32 trailer
    // Header soup: 0x78 0x9c is a valid zlib header at every even offset,
    // and 0x9c 0x78... keeps the scanner probing at odd ones too.
    for _ in 0..10_000 {
        stream.extend_from_slice(&[0x78, 0x9c]);
    }

    let started = std::time::Instant::now();
    let mut decoder = ZlibStreamDecoder::new(&stream[..]);
    let mut output = Vec::new();
    let result = decoder.read_to_end(&mut output);
    let elapsed = started.elapsed();

    assert!(
        result.is_err(),
        "corrupted Adler-32 must be an error, not silently decoded"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "adversarial stream took {elapsed:?}; decoder must be O(n), not quadratic"
    );
}

/// The opt-in output cap bounds decompression-bomb amplification.
#[test]
fn test_zlib_stream_decoder_max_output_cap() {
    let bomb_plain = vec![0u8; 4_000_000];
    let compressed = zlib_compress(&bomb_plain, 9).expect("zlib_compress failed");

    // Under the cap: decodes fine.
    let mut ok_decoder = ZlibStreamDecoder::new(&compressed[..]).with_max_output(4_000_000);
    let mut output = Vec::new();
    ok_decoder
        .read_to_end(&mut output)
        .expect("cap equal to payload size must succeed");
    assert_eq!(output.len(), 4_000_000);

    // Over the cap: hard error.
    let mut capped = ZlibStreamDecoder::new(&compressed[..]).with_max_output(1_000_000);
    let mut sink = Vec::new();
    assert!(
        capped.read_to_end(&mut sink).is_err(),
        "output beyond the configured cap must be rejected"
    );
}

/// Invalid leading bytes are an error (not a silent empty result), while
/// non-zlib trailing bytes after a complete member stop decoding gracefully.
#[test]
fn test_zlib_stream_decoder_header_error_semantics() {
    let mut bad = ZlibStreamDecoder::new(&b"not zlib at all"[..]);
    let mut sink = Vec::new();
    assert!(
        bad.read_to_end(&mut sink).is_err(),
        "garbage input must be an error, not an empty Ok"
    );

    let mut with_trailer = zlib_compress(b"payload", 6).expect("zlib_compress failed");
    with_trailer.extend_from_slice(b"XYZ"); // not a zlib header
    let mut decoder = ZlibStreamDecoder::new(&with_trailer[..]);
    let mut output = Vec::new();
    decoder
        .read_to_end(&mut output)
        .expect("trailing garbage after a complete member is tolerated");
    assert_eq!(output, b"payload");
}

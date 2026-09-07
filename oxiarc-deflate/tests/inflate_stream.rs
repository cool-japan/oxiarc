//! Resumable-inflate coverage: `InflateStream` and `WrappedInflate`.
//!
//! The core property under test is **split invariance**: for any way of
//! chopping the compressed stream into chunks and any output-buffer size,
//! the push decoder must produce byte-identical output to the one-shot
//! `inflate()`. Everything else here — truncation, corruption, limits,
//! multi-member framing — asserts that the decoder errors cleanly, in a
//! bounded number of calls, rather than panicking or spinning.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{
    Deflater, InflateStatus, InflateStream, InflateWrapper, TrailingPolicy, WrappedInflate,
    deflate, gzip_compress, inflate, zlib_compress, zlib_compress_with_dict,
};

// ---------------------------------------------------------------------------
// Corpus (mirrors tests/inflate_differential.rs so both suites see the same
// block shapes)
// ---------------------------------------------------------------------------

/// Deterministic xorshift PRNG (no external dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// Inputs chosen to force every DEFLATE block type and every interesting
/// back-reference shape through the decoder.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one-byte", vec![0x42]),
        ("two-bytes", vec![0, 255]),
        ("random-64k", (0..65_536).map(|_| rng.byte()).collect()),
        ("zeros-256k", vec![0u8; 262_144]),
        ("repeat-16", b"0123456789abcdef".repeat(20_000).to_vec()),
    ];

    // Maximum back-reference distance (32768) and maximum match length (258).
    let mut max_dist: Vec<u8> = (0..32_768).map(|_| rng.byte()).collect();
    let head = max_dist.clone();
    max_dist.push(0xAB);
    max_dist.extend_from_slice(&head);
    out.push(("max-distance", max_dist));

    let mut text = Vec::new();
    for i in 0..40_000u32 {
        text.extend_from_slice(match i % 7 {
            0 => b"the quick ".as_slice(),
            1 => b"brown fox ".as_slice(),
            2 => b"jumps ".as_slice(),
            3 => b"over ".as_slice(),
            4 => b"the lazy ".as_slice(),
            5 => b"dog. ".as_slice(),
            _ => b"\n".as_slice(),
        });
    }
    out.push(("text", text));

    out.push((
        "two-symbols",
        (0..100_000)
            .map(|i| if i % 3 == 0 { b'a' } else { b'b' })
            .collect(),
    ));
    out.push((
        "sawtooth",
        (0..200_000u32).map(|i| (i % 251) as u8).collect(),
    ));

    out
}

/// A small corpus for the O(n·m) schedules.
fn small_corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one-byte", vec![0x42]),
        (
            "text-4k",
            b"the quick brown fox jumps over the lazy dog. ".repeat(90),
        ),
        ("zeros-8k", vec![0u8; 8192]),
        (
            "sawtooth-4k",
            (0..4096u32).map(|i| (i % 251) as u8).collect(),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Drive helpers
// ---------------------------------------------------------------------------

/// Drive `InflateStream` with fixed input chunks and a fixed output buffer,
/// asserting a call-count bound so a stalled state machine fails the test
/// instead of hanging it.
fn push_decode(
    compressed: &[u8],
    in_chunk: usize,
    out_size: usize,
    expected_out: usize,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut stream = InflateStream::new();
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_size.max(1)];
    let mut fed = 0usize;
    let mut calls = 0usize;
    let bound = call_bound(compressed.len(), in_chunk, expected_out, out_size);
    loop {
        calls += 1;
        assert!(calls < bound, "no progress after {calls} calls");
        let end = (fed + in_chunk).min(compressed.len());
        let slice = compressed.get(fed..end).unwrap_or_default();
        let flush = if end >= compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.inflate(slice, &mut scratch, flush)?;
        fed += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
    }
}

/// A generous but finite bound on the number of `inflate` calls a schedule
/// may need: every call must either absorb input or fill output.
fn call_bound(in_len: usize, in_chunk: usize, out_len: usize, out_size: usize) -> usize {
    4 * (in_len / in_chunk.max(1) + out_len / out_size.max(1) + 8)
}

// ---------------------------------------------------------------------------
// S1-S8: split invariance
// ---------------------------------------------------------------------------

#[test]
fn s1_byte_at_a_time_input() {
    for (name, data) in small_corpus() {
        for level in [0u8, 6, 9] {
            let compressed = deflate(&data, level).expect("deflate");
            let decoded = push_decode(&compressed, 1, 65536, data.len()).expect("decode");
            assert_eq!(decoded, data, "{name} level {level}");
        }
    }
}

#[test]
fn s2_one_byte_output_buffer() {
    for (name, data) in small_corpus() {
        for level in [0u8, 6, 9] {
            let compressed = deflate(&data, level).expect("deflate");
            let decoded =
                push_decode(&compressed, compressed.len().max(1), 1, data.len()).expect("decode");
            assert_eq!(decoded, data, "{name} level {level}");
        }
    }
}

#[test]
fn s3_one_byte_input_and_output() {
    for (name, data) in small_corpus() {
        for level in [0u8, 6, 9] {
            let compressed = deflate(&data, level).expect("deflate");
            let decoded = push_decode(&compressed, 1, 1, data.len()).expect("decode");
            assert_eq!(decoded, data, "{name} level {level}");
        }
    }
}

#[test]
fn s5_prime_sized_chunks_over_the_whole_corpus() {
    for (name, data) in corpus() {
        for level in [0u8, 1, 6, 9] {
            let compressed = deflate(&data, level).expect("deflate");
            for (in_chunk, out_size) in [(7usize, 4093usize), (13, 251), (251, 13), (4093, 7)] {
                let decoded =
                    push_decode(&compressed, in_chunk, out_size, data.len()).expect("decode");
                assert_eq!(decoded, data, "{name} level {level} {in_chunk}/{out_size}");
            }
        }
    }
}

#[test]
fn s6_zero_length_input_calls_are_inert() {
    let data = b"interleaved empty feeds".repeat(20);
    let compressed = deflate(&data, 6).expect("deflate");
    let mut stream = InflateStream::new();
    let mut out = Vec::new();
    let mut scratch = [0u8; 32];
    let mut fed = 0usize;
    loop {
        // An empty feed consumes nothing. It may still *produce*: bytes were
        // counted as consumed when they entered the bit accumulator, so up
        // to seven bytes' worth of symbols (and any match still being
        // copied) can be decoded with no new input at all. That output is
        // real and must be collected, not discarded.
        let idle = stream
            .inflate(&[], &mut scratch, FlushMode::None)
            .expect("idle call");
        assert_eq!(idle.consumed, 0, "an empty feed cannot consume");
        out.extend_from_slice(&scratch[..idle.produced]);

        let end = (fed + 3).min(compressed.len());
        let flush = if end >= compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let p = stream
            .inflate(&compressed[fed..end], &mut scratch, flush)
            .expect("inflate");
        fed += p.consumed;
        out.extend_from_slice(&scratch[..p.produced]);
        if p.status == InflateStatus::StreamEnd {
            break;
        }
    }
    assert_eq!(out, data);
}

#[test]
fn s7_zero_length_output_calls_report_need_output() {
    let data = b"zero sized output".repeat(20);
    let compressed = deflate(&data, 6).expect("deflate");
    let mut stream = InflateStream::new();
    let p = stream
        .inflate(&compressed, &mut [], FlushMode::None)
        .expect("inflate");
    assert_eq!(p.produced, 0);
    assert_eq!(p.status, InflateStatus::NeedOutput);
    // Bytes may have been absorbed into the accumulator; they must not be
    // re-fed, so continue from `consumed`.
    let mut out = vec![0u8; data.len() + 16];
    let p2 = stream
        .inflate(&compressed[p.consumed..], &mut out, FlushMode::Finish)
        .expect("inflate");
    assert_eq!(&out[..p2.produced], &data[..]);
}

#[test]
fn s8_split_at_every_offset_of_a_small_stream() {
    // Splitting at every byte offset lands inside every structural element:
    // between the three header bits and LEN/NLEN, mid-HCLEN, inside a
    // code-length repeat, between a length symbol and its extra bits, and
    // in the middle of a match.
    for (name, data) in small_corpus() {
        for level in [0u8, 6, 9] {
            let compressed = deflate(&data, level).expect("deflate");
            for split in 0..=compressed.len() {
                let mut stream = InflateStream::new();
                let mut out = Vec::new();
                let mut scratch = vec![0u8; 128];
                let mut fed = 0usize;
                let mut calls = 0usize;
                let bound = call_bound(compressed.len(), 1, data.len(), 128);
                loop {
                    calls += 1;
                    assert!(calls < bound, "{name}: no progress at split {split}");
                    let end = if fed < split { split } else { compressed.len() };
                    let flush = if end >= compressed.len() {
                        FlushMode::Finish
                    } else {
                        FlushMode::None
                    };
                    let p = stream
                        .inflate(&compressed[fed..end], &mut scratch, flush)
                        .expect("inflate");
                    fed += p.consumed;
                    out.extend_from_slice(&scratch[..p.produced]);
                    if p.status == InflateStatus::StreamEnd {
                        break;
                    }
                }
                assert_eq!(out, data, "{name} level {level} split {split}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// D1: differential against the one-shot decoder
// ---------------------------------------------------------------------------

#[test]
fn d1_push_path_agrees_with_one_shot_at_every_level() {
    for (name, data) in corpus() {
        for level in 0u8..=9 {
            let compressed = deflate(&data, level).expect("deflate");
            let reference = inflate(&compressed).expect("inflate");
            assert_eq!(reference, data, "{name} level {level}: reference");
            for (in_chunk, out_size) in [(1usize, 64usize), (7, 4096), (4096, 65536)] {
                let decoded =
                    push_decode(&compressed, in_chunk, out_size, data.len()).expect("push");
                assert_eq!(
                    decoded, reference,
                    "{name} level {level} {in_chunk}/{out_size}"
                );
            }
        }
    }
}

#[test]
fn d1_all_three_block_types_agree() {
    // Level 0 emits stored blocks; a tiny high-entropy input at level 9
    // emits fixed-Huffman blocks; a large skewed input emits dynamic ones.
    let stored = vec![0xA5u8; 200_000];
    let fixed = b"abc".to_vec();
    let dynamic = b"the quick brown fox ".repeat(5_000);
    for (name, data, level) in [
        ("stored", stored, 0u8),
        ("fixed", fixed, 9),
        ("dynamic", dynamic, 9),
    ] {
        let compressed = deflate(&data, level).expect("deflate");
        let reference = inflate(&compressed).expect("inflate");
        for chunk in [1usize, 3, 1024] {
            let decoded = push_decode(&compressed, chunk, 97, data.len()).expect("push");
            assert_eq!(decoded, reference, "{name} chunk {chunk}");
        }
    }
}

// ---------------------------------------------------------------------------
// Reference fixtures (produced by CPython 3 `gzip`/`zlib`, mtime=0), shared
// with tests/wrapper_regressions.rs
// ---------------------------------------------------------------------------

/// `gzip.compress(b"hello ", 9, mtime=0) + gzip.compress(b"multi-member ", 1,
/// mtime=0) + gzip.compress(b"gzip world!", 6, mtime=0)`.
const PY_GZIP_MULTI_MEMBER: [u8; 90] = [
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57,
    0x00, 0x00, 0xf6, 0xf9, 0x81, 0xed, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x04, 0xff, 0xcb, 0x2d, 0xcd, 0x29, 0xc9, 0xd4, 0xcd, 0x4d, 0xcd, 0x4d, 0x4a, 0x2d,
    0x52, 0x00, 0x00, 0xec, 0x66, 0x33, 0xe7, 0x0d, 0x00, 0x00, 0x00, 0x1f, 0x8b, 0x08, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xff, 0x4b, 0xaf, 0xca, 0x2c, 0x50, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x51,
    0x04, 0x00, 0xbe, 0x81, 0x68, 0xa0, 0x0b, 0x00, 0x00, 0x00,
];

/// `gzip.compress(b"hello ", 9, mtime=0)` plus 7 zero bytes of padding.
const PY_GZIP_TRAILING_ZEROS: [u8; 33] = [
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57,
    0x00, 0x00, 0xf6, 0xf9, 0x81, 0xed, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00,
];

/// `zlib.compress(b"first zlib member. ", 9) + zlib.compress(b"second zlib
/// member!", 1)`.
const PY_ZLIB_CONCAT: [u8; 54] = [
    0x78, 0xda, 0x4b, 0xcb, 0x2c, 0x2a, 0x2e, 0x51, 0xa8, 0xca, 0xc9, 0x4c, 0x52, 0xc8, 0x4d, 0xcd,
    0x4d, 0x4a, 0x2d, 0xd2, 0x53, 0x00, 0x00, 0x49, 0x17, 0x06, 0xe0, 0x78, 0x01, 0x2b, 0x4e, 0x4d,
    0xce, 0xcf, 0x4b, 0x51, 0xa8, 0xca, 0xc9, 0x4c, 0x52, 0xc8, 0x4d, 0xcd, 0x4d, 0x4a, 0x2d, 0x52,
    0x04, 0x00, 0x48, 0xe1, 0x07, 0x07,
];

// ---------------------------------------------------------------------------
// Wrapper drive helper
// ---------------------------------------------------------------------------

/// Drive a `WrappedInflate` with fixed input chunks and a fixed output
/// buffer, under the same call-count bound as `push_decode`.
fn wrapped_decode(
    decoder: &mut WrappedInflate,
    input: &[u8],
    in_chunk: usize,
    out_size: usize,
    expected_out: usize,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_size.max(1)];
    let mut fed = 0usize;
    let mut calls = 0usize;
    let bound = call_bound(input.len(), in_chunk, expected_out, out_size);
    loop {
        calls += 1;
        assert!(calls < bound, "no progress after {calls} calls");
        let end = (fed + in_chunk).min(input.len());
        let slice = input.get(fed..end).unwrap_or_default();
        let flush = if end >= input.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = decoder.inflate(slice, &mut scratch, flush)?;
        fed += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
    }
}

/// Hand-build one gzip member with the requested optional header fields.
fn build_gzip_member(
    payload: &[u8],
    extra: Option<&[u8]>,
    name: Option<&[u8]>,
    comment: Option<&[u8]>,
    hcrc: bool,
) -> Vec<u8> {
    let mut flg = 0u8;
    if extra.is_some() {
        flg |= 0x04;
    }
    if name.is_some() {
        flg |= 0x08;
    }
    if comment.is_some() {
        flg |= 0x10;
    }
    if hcrc {
        flg |= 0x02;
    }
    let mut header = vec![0x1f, 0x8b, 0x08, flg, 0x11, 0x22, 0x33, 0x44, 0x00, 0x03];
    if let Some(extra) = extra {
        header.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        header.extend_from_slice(extra);
    }
    if let Some(name) = name {
        header.extend_from_slice(name);
        header.push(0);
    }
    if let Some(comment) = comment {
        header.extend_from_slice(comment);
        header.push(0);
    }
    if hcrc {
        let crc = oxiarc_core::Crc32::compute(&header) as u16;
        header.extend_from_slice(&crc.to_le_bytes());
    }
    header.extend_from_slice(&deflate(payload, 6).expect("deflate"));
    header.extend_from_slice(&oxiarc_core::Crc32::compute(payload).to_le_bytes());
    header.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    header
}

// ---------------------------------------------------------------------------
// F1-F12: format coverage
// ---------------------------------------------------------------------------

#[test]
fn f1_multi_member_gzip_byte_at_a_time() {
    for chunk in [1usize, 2, 3, 7, 90] {
        let mut decoder = WrappedInflate::new(InflateWrapper::Gzip).multi_member(true);
        let out = wrapped_decode(&mut decoder, &PY_GZIP_MULTI_MEMBER, chunk, 5, 30)
            .expect("multi-member gzip");
        assert_eq!(out, b"hello multi-member gzip world!", "chunk {chunk}");
        assert_eq!(decoder.members_decoded(), 3, "chunk {chunk}");
    }
}

#[test]
fn f2_every_gzip_header_flag_combination() {
    let payload = b"header flag matrix".repeat(9);
    for mask in 0u8..16 {
        let extra: Option<&[u8]> = if mask & 1 != 0 {
            Some(b"XTaAbCd")
        } else {
            None
        };
        let name: Option<&[u8]> = if mask & 2 != 0 {
            Some(b"file.name")
        } else {
            None
        };
        let comment: Option<&[u8]> = if mask & 4 != 0 {
            Some(b"a comment")
        } else {
            None
        };
        let hcrc = mask & 8 != 0;
        let member = build_gzip_member(&payload, extra, name, comment, hcrc);
        for chunk in [1usize, 5, member.len()] {
            let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
            let out = wrapped_decode(&mut decoder, &member, chunk, 13, payload.len())
                .unwrap_or_else(|e| panic!("mask {mask} chunk {chunk}: {e}"));
            assert_eq!(out, payload, "mask {mask} chunk {chunk}");
            let header = decoder.gzip_header().expect("header");
            assert_eq!(header.extra.as_deref(), extra, "mask {mask}");
            assert_eq!(header.name.as_deref(), name, "mask {mask}");
            assert_eq!(header.comment.as_deref(), comment, "mask {mask}");
            assert_eq!(header.mtime, 0x4433_2211, "mask {mask}");
            assert_eq!(header.os, 3, "mask {mask}");
        }
    }
}

#[test]
fn f3_gzip_fextra_at_the_maximum_xlen() {
    let extra = vec![0x5Au8; 65_535];
    let member = build_gzip_member(b"big extra field", Some(&extra), None, None, true);
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    let out = wrapped_decode(&mut decoder, &member, 2185, 64, 15).expect("decode");
    assert_eq!(out, b"big extra field");
    assert_eq!(
        decoder.gzip_header().expect("header").extra.as_deref(),
        Some(&extra[..])
    );
}

#[test]
fn f4_gzip_header_field_cap() {
    let name = vec![b'n'; 64 * 1024];
    let member = build_gzip_member(b"ok", None, Some(&name), None, false);
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    // Exactly at the cap: accepted.
    assert_eq!(
        wrapped_decode(&mut decoder, &member, 4096, 32, 2).expect("at cap"),
        b"ok"
    );

    let over = vec![b'n'; 64 * 1024 + 1];
    let member = build_gzip_member(b"ok", None, Some(&over), None, false);
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    let err = wrapped_decode(&mut decoder, &member, 4096, 32, 2).expect_err("over cap");
    assert!(err.to_string().contains("64 KiB"), "{err}");
}

#[test]
fn f5_gzip_fhcrc_is_verified_by_default() {
    let payload = b"fhcrc payload";
    let good = build_gzip_member(payload, None, Some(b"x"), None, true);
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    assert_eq!(
        wrapped_decode(&mut decoder, &good, 1, 8, payload.len()).expect("good"),
        payload
    );

    let mut bad = good.clone();
    // The FHCRC bytes sit immediately before the DEFLATE payload: 10 fixed
    // header bytes + "x\0".
    bad[12] ^= 0xFF;
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    let err = wrapped_decode(&mut decoder, &bad, 1, 8, payload.len()).expect_err("bad FHCRC");
    assert!(err.to_string().contains("CRC"), "{err}");

    // Opting out accepts it.
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip).verify_header_crc(false);
    assert_eq!(
        wrapped_decode(&mut decoder, &bad, 1, 8, payload.len()).expect("opt out"),
        payload
    );
}

#[test]
fn f6_gzip_trailing_zeros_under_each_policy() {
    for (policy, ok) in [
        (TrailingPolicy::AllowZeros, true),
        (TrailingPolicy::Stop, true),
        (TrailingPolicy::Reject, false),
    ] {
        let mut decoder = WrappedInflate::new(InflateWrapper::Gzip)
            .multi_member(true)
            .trailing_policy(policy);
        let result = wrapped_decode(&mut decoder, &PY_GZIP_TRAILING_ZEROS, 1, 8, 6);
        match (ok, result) {
            (true, Ok(out)) => assert_eq!(out, b"hello ", "{policy:?}"),
            (false, Err(_)) => {}
            (true, Err(e)) => panic!("{policy:?} should accept zero padding: {e}"),
            (false, Ok(_)) => panic!("{policy:?} should reject zero padding"),
        }
    }
}

#[test]
fn f7_concatenated_zlib_with_accumulator_residue() {
    // The first member's DEFLATE data ends mid-byte, so aligning and
    // draining the 4-byte trailer leaves 1-3 bytes of member 2 sitting in
    // the accumulator. Those bytes were already reported as consumed and
    // are available nowhere else.
    for chunk in [1usize, 2, 3, 5, 7, 11, 54] {
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib).multi_member(true);
        let out = wrapped_decode(&mut decoder, &PY_ZLIB_CONCAT, chunk, 4, 38)
            .unwrap_or_else(|e| panic!("chunk {chunk}: {e}"));
        assert_eq!(
            out, b"first zlib member. second zlib member!",
            "chunk {chunk}"
        );
        assert_eq!(decoder.members_decoded(), 2, "chunk {chunk}");
    }
}

#[test]
fn f7a_hand_built_concatenation_leaves_residue() {
    // Build a two-member stream whose first member is deliberately not
    // byte-aligned at its end, then confirm every feed schedule agrees.
    let first = zlib_compress(b"leading member payload that compresses", 9).expect("zlib");
    let second = zlib_compress(b"trailing member", 1).expect("zlib");
    let mut joined = first.clone();
    joined.extend_from_slice(&second);
    let expected = b"leading member payload that compressestrailing member";
    for chunk in [1usize, 2, 3, 5, 8, 13, joined.len()] {
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib).multi_member(true);
        let out = wrapped_decode(&mut decoder, &joined, chunk, 7, expected.len())
            .unwrap_or_else(|e| panic!("chunk {chunk}: {e}"));
        assert_eq!(out, expected, "chunk {chunk}");
        assert_eq!(decoder.members_decoded(), 2);
    }
}

#[test]
fn f7b_reset_for_next_member_preserves_the_accumulator() {
    // The direct negative control for f7/f7a. A zlib member whose DEFLATE
    // data ends mid-byte leaves 1-3 bytes of the *next* member in the
    // accumulator once the 4-byte trailer has been drained; `reset()` must
    // clear them and `reset_for_next_member()` must not.
    let mut found = None;
    for n in 1usize..64 {
        let payload: Vec<u8> = (0..n).map(|i| b'a' + (i % 23) as u8).collect();
        let member = zlib_compress(&payload, 9).expect("zlib");
        let follower = zlib_compress(b"next member", 6).expect("zlib");
        let mut joined = member.get(2..).unwrap_or_default().to_vec();
        joined.extend_from_slice(&follower);

        let mut stream = InflateStream::new();
        let mut out = vec![0u8; 256];
        let p = stream
            .inflate(&joined, &mut out, FlushMode::None)
            .expect("member 1");
        assert_eq!(p.status, InflateStatus::StreamEnd);
        stream.align_to_byte();
        for _ in 0..4 {
            stream.take_buffered_byte();
        }
        if stream.buffered_bits() >= 8 {
            found = Some(stream);
            break;
        }
    }
    let mut stream = found.expect("no payload left accumulator residue");
    let residue = stream.buffered_bits();

    stream.reset_for_next_member();
    assert_eq!(
        stream.buffered_bits(),
        residue,
        "reset_for_next_member must preserve the accumulator"
    );

    stream.reset();
    assert_eq!(
        stream.buffered_bits(),
        0,
        "reset must clear the accumulator"
    );
}

#[test]
fn f8_zlib_fdict_streams() {
    let dictionary = b"the shared dictionary bytes";
    let payload = b"the shared dictionary bytes plus more text";
    let compressed = zlib_compress_with_dict(payload, 6, dictionary).expect("compress");

    // Right dictionary.
    let mut decoder = WrappedInflate::new(InflateWrapper::Zlib).with_dictionary(dictionary);
    assert_eq!(
        wrapped_decode(&mut decoder, &compressed, 1, 9, payload.len()).expect("fdict"),
        payload
    );

    // Wrong dictionary: the DICTID check fires before any output.
    let mut decoder =
        WrappedInflate::new(InflateWrapper::Zlib).with_dictionary(b"a different dictionary");
    let err = wrapped_decode(&mut decoder, &compressed, 1, 9, payload.len())
        .expect_err("wrong dictionary");
    assert!(err.to_string().contains("CRC"), "{err}");

    // No dictionary at all.
    let mut decoder = WrappedInflate::new(InflateWrapper::Zlib);
    let err = wrapped_decode(&mut decoder, &compressed, 1, 9, payload.len())
        .expect_err("missing dictionary");
    assert!(err.to_string().contains("dictionary"), "{err}");
}

#[test]
fn f9_auto_tie_break_is_documented_behaviour() {
    let payload = b"auto detection".repeat(8);
    for (name, bytes, expected) in [
        (
            "gzip",
            gzip_compress(&payload, 6).expect("gzip"),
            InflateWrapper::Gzip,
        ),
        (
            "zlib",
            zlib_compress(&payload, 6).expect("zlib"),
            InflateWrapper::Zlib,
        ),
        (
            "raw",
            deflate(&payload, 6).expect("deflate"),
            InflateWrapper::Raw,
        ),
    ] {
        for chunk in [1usize, 3, 4096] {
            let mut decoder = WrappedInflate::new(InflateWrapper::Auto);
            let out = wrapped_decode(&mut decoder, &bytes, chunk, 11, payload.len())
                .unwrap_or_else(|e| panic!("{name} chunk {chunk}: {e}"));
            assert_eq!(out, payload, "{name} chunk {chunk}");
            assert_eq!(decoder.active_wrapper(), expected, "{name}");
        }
    }
}

#[test]
fn f9a_auto_reads_a_zlib_shaped_raw_stream_as_zlib() {
    // A raw DEFLATE stream whose first two bytes satisfy CM == 8, CINFO <= 7
    // and the %31 check is indistinguishable from zlib on a two-byte sniff.
    // Such a stream is constructible: a non-final stored block puts 0b000 in
    // the low three bits, the next five are ignored padding, and the second
    // byte is the low half of LEN.
    //
    //   byte 0 = 0b0000_1000 = 0x08  -> CM = 8, CINFO = 0
    //   byte 1 = 0x1D = 29           -> (0x08 * 256 + 29) % 31 == 0, LEN = 29
    //
    // The documented tie-break is "gzip magic, then a valid zlib header,
    // then raw", so `Auto` reads this as zlib. Assert that rather than
    // leaving it to be discovered later — and assert it cannot panic.
    let payload = b"twenty-nine bytes of payload!";
    assert_eq!(payload.len(), 29);
    let mut raw = vec![0x08u8, 0x1D, 0x00, 0xE2, 0xFF];
    raw.extend_from_slice(payload);
    // A final empty stored block closes the stream.
    raw.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);

    // Named explicitly as raw, it decodes correctly.
    let mut decoder = WrappedInflate::new(InflateWrapper::Raw);
    assert_eq!(
        wrapped_decode(&mut decoder, &raw, 1, 8, payload.len()).expect("raw"),
        payload
    );

    // Sniffed, it is taken for zlib — the documented, deliberate tie-break.
    let mut decoder = WrappedInflate::new(InflateWrapper::Auto);
    let result = wrapped_decode(&mut decoder, &raw, 1, 8, payload.len());
    assert_eq!(decoder.active_wrapper(), InflateWrapper::Zlib);
    // The two header bytes are eaten, so what follows is not a valid
    // DEFLATE stream: an error, never a panic and never a wrong success.
    if let Ok(out) = result {
        assert_ne!(
            out.as_slice(),
            &payload[..],
            "the ambiguous stream must not silently decode as raw"
        );
    }
}

#[test]
fn f10_empty_members() {
    let gz = gzip_compress(b"", 6).expect("gzip");
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    assert!(
        wrapped_decode(&mut decoder, &gz, 1, 8, 0)
            .expect("empty gzip")
            .is_empty()
    );

    let zl = zlib_compress(b"", 6).expect("zlib");
    let mut decoder = WrappedInflate::new(InflateWrapper::Zlib);
    assert!(
        wrapped_decode(&mut decoder, &zl, 1, 8, 0)
            .expect("empty zlib")
            .is_empty()
    );

    // An empty final stored block is the minimal raw stream.
    let mut decoder = WrappedInflate::new(InflateWrapper::Raw);
    let raw = [0x01u8, 0x00, 0x00, 0xFF, 0xFF];
    assert!(
        wrapped_decode(&mut decoder, &raw, 1, 8, 0)
            .expect("empty raw")
            .is_empty()
    );
}

#[test]
fn f11_large_stored_blocks_split_at_every_kib() {
    let data = vec![0xC3u8; 200_000];
    let compressed = deflate(&data, 0).expect("level 0");
    let decoded = push_decode(&compressed, 1024, 4096, data.len()).expect("decode");
    assert_eq!(decoded, data);
}

#[test]
fn f12_sync_flush_boundaries_are_reported() {
    // Three sync-flushed units followed by a final block.
    let mut encoder = Deflater::new(6);
    let mut stream = Vec::new();
    for part in [
        b"first ".as_slice(),
        b"second ".as_slice(),
        b"third ".as_slice(),
    ] {
        encoder.deflate_sync(part, &mut stream).expect("sync flush");
    }
    encoder.deflate(b"last", &mut stream, true).expect("finish");

    let mut decoder = InflateStream::new();
    let mut out = Vec::new();
    let mut scratch = [0u8; 4096];
    let mut fed = 0usize;
    let mut boundaries = 0usize;
    loop {
        let flush = FlushMode::Sync;
        let p = decoder
            .inflate(&stream[fed..], &mut scratch, flush)
            .expect("inflate");
        fed += p.consumed;
        out.extend_from_slice(&scratch[..p.produced]);
        if decoder.at_sync_flush() {
            boundaries += 1;
        }
        if p.status == InflateStatus::StreamEnd {
            break;
        }
        if p.consumed == 0 && p.produced == 0 && !decoder.at_sync_flush() {
            panic!("no progress");
        }
    }
    assert_eq!(out, b"first second third last");
    assert!(
        boundaries >= 3,
        "expected >= 3 sync boundaries, saw {boundaries}"
    );
}

// ---------------------------------------------------------------------------
// R1-R17: robustness — must error, never panic, never hang
// ---------------------------------------------------------------------------

/// Push a stream through the decoder without asserting success, but with a
/// hard call-count bound so a stalled state machine fails instead of
/// hanging. Returns whether the decode completed.
fn probe(input: &[u8], in_chunk: usize, out_size: usize, max_out: usize) -> bool {
    let mut stream = InflateStream::new();
    let mut scratch = vec![0u8; out_size.max(1)];
    let mut fed = 0usize;
    let mut calls = 0usize;
    // Every call must absorb input or fill output, so a decode cannot need
    // more calls than the sum of those two counts (times a small slack for
    // their interleaving). A state machine that returns `NeedInput` without
    // consuming would spin forever in a real adapter; here it trips this.
    let bound = call_bound(input.len(), in_chunk, max_out, out_size);
    loop {
        calls += 1;
        assert!(calls < bound, "no progress after {calls} calls");
        let end = (fed + in_chunk).min(input.len());
        let slice = input.get(fed..end).unwrap_or_default();
        let flush = if end >= input.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        match stream.inflate(slice, &mut scratch, flush) {
            Ok(p) => {
                fed += p.consumed;
                if p.status == InflateStatus::StreamEnd {
                    return true;
                }
            }
            Err(_) => return false,
        }
    }
}

#[test]
fn r1_truncation_at_every_offset_terminates() {
    let data: Vec<u8> = (0..20_000u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    for level in [0u8, 6, 9] {
        let compressed = deflate(&data, level).expect("deflate");
        for cut in 0..compressed.len() {
            // Never a panic, never a hang; a truncated stream under
            // `Finish` must not report `StreamEnd`.
            let completed = probe(
                &compressed[..cut],
                compressed.len().max(1),
                4096,
                data.len(),
            );
            assert!(
                !completed,
                "level {level}: truncation at {cut} reported success"
            );
        }
    }
}

#[test]
fn r2_truncation_fed_byte_at_a_time() {
    let data = b"byte at a time truncation".repeat(60);
    for level in [0u8, 6, 9] {
        let compressed = deflate(&data, level).expect("deflate");
        for cut in 0..compressed.len() {
            let completed = probe(&compressed[..cut], 1, 7, data.len());
            assert!(
                !completed,
                "level {level}: truncation at {cut} reported success"
            );
        }
    }
}

#[test]
fn r3_bit_flips_never_panic() {
    let data = b"bit flip corpus for the push decoder".repeat(40);
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for level in [0u8, 6, 9] {
        let compressed = deflate(&data, level).expect("deflate");
        for _ in 0..700 {
            let mut corrupt = compressed.clone();
            let index = (rng.next() as usize) % corrupt.len();
            let bit = (rng.next() % 8) as u8;
            corrupt[index] ^= 1 << bit;
            // Only "does not panic and does not hang" is asserted: a bit
            // flip can produce a stream that decodes to something else.
            // A bit flip can legally produce a much larger stream than the
            // original, so allow generous headroom before the bound trips.
            let _ = probe(&corrupt, 1, 64, 16 * data.len());
            let _ = probe(&corrupt, corrupt.len(), 4096, 16 * data.len());
        }
    }
}

#[test]
fn r4_arbitrary_bytes_through_every_wrapper() {
    let mut rng = Rng(0x0BAD_C0DE_1234_5678);
    for _ in 0..400 {
        let len = (rng.next() % 200) as usize;
        let junk: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        for wrapper in [
            InflateWrapper::Raw,
            InflateWrapper::Zlib,
            InflateWrapper::Gzip,
            InflateWrapper::Auto,
        ] {
            for policy in [
                TrailingPolicy::Reject,
                TrailingPolicy::AllowZeros,
                TrailingPolicy::Stop,
            ] {
                let mut decoder = WrappedInflate::new(wrapper)
                    .multi_member(true)
                    .trailing_policy(policy);
                let mut scratch = [0u8; 64];
                let mut fed = 0usize;
                let mut calls = 0usize;
                loop {
                    calls += 1;
                    assert!(calls < 4096, "no progress on junk input");
                    let slice = junk.get(fed..).unwrap_or_default();
                    match decoder.inflate(slice, &mut scratch, FlushMode::Finish) {
                        Ok(p) => {
                            fed += p.consumed;
                            if p.status == InflateStatus::StreamEnd {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }
}

#[test]
fn r5_sticky_fault_survives_further_pushes() {
    let mut stream = InflateStream::new();
    let mut out = [0u8; 64];
    let first = stream
        .inflate(&[0b0000_0111], &mut out, FlushMode::Finish)
        .expect_err("reserved block type 3");
    let message = first.to_string();
    for _ in 0..3 {
        let again = stream
            .inflate(b"more bytes", &mut out, FlushMode::None)
            .expect_err("latched");
        assert_eq!(again.to_string(), message);
    }
}

/// A **single-block** DEFLATE bomb: ~812 KB of compressed data expanding to
/// exactly 123 MiB, built bit by bit so the shape is guaranteed rather than
/// left to the encoder.
///
/// The whole expansion lives in one fixed-Huffman block, which is what makes
/// it a test of *in-block* cap enforcement: a decoder that checks its budget
/// only between blocks would materialise all 123 MiB before ever looking.
/// The construction is one literal `0x00` to seed distance 1, then a run of
/// (length 258, distance 1) pairs costing 13 bits each — 8 bits for
/// literal/length code 285 and 5 for distance code 0 — then the
/// end-of-block symbol.
fn single_block_bomb() -> (Vec<u8>, u64) {
    const MATCHES: u64 = 123 * 1024 * 1024 / 258;
    let expanded = 1 + MATCHES * 258;

    let mut bits = BitWriterLsb::new();
    bits.write(1, 1); // BFINAL = 1
    bits.write(1, 2); // BTYPE = 01, fixed Huffman
    // Literal 0x00: fixed codes 0..=143 are 8 bits, base 0x30.
    bits.write_code(0x30, 8);
    for _ in 0..MATCHES {
        // Literal/length code 285 (length 258): 8 bits, 0xC5.
        bits.write_code(0xC5, 8);
        // Distance code 0 (distance 1): 5 bits.
        bits.write_code(0x00, 5);
    }
    // End of block: code 256 is 7 bits, value 0.
    bits.write_code(0x00, 7);
    let stream = bits.finish();
    (stream, expanded)
}

/// Confirm the generator really produces one block that really expands, by
/// decoding it with no cap at all and counting both.
#[test]
fn the_bomb_generator_produces_one_expanding_block() {
    let (bomb, expanded) = single_block_bomb();
    assert!(
        (800_000..830_000).contains(&bomb.len()),
        "expected ~812 KB compressed, got {}",
        bomb.len()
    );
    // BFINAL = 1 and BTYPE = 01 in the first byte: the whole stream is one
    // block, so any cap must be enforced *inside* it.
    assert_eq!(bomb[0] & 0b111, 0b011, "first block must be final + fixed");

    let mut stream = InflateStream::new();
    let mut scratch = vec![0u8; 1 << 16];
    let mut fed = 0usize;
    let mut produced = 0u64;
    loop {
        let p = stream
            .inflate(&bomb[fed..], &mut scratch, FlushMode::Finish)
            .expect("uncapped decode");
        fed += p.consumed;
        produced += p.produced as u64;
        if p.status == InflateStatus::StreamEnd {
            break;
        }
    }
    assert_eq!(produced, expanded);
    assert!(
        expanded / bomb.len() as u64 > 150,
        "expansion ratio too small to be a bomb"
    );
}

#[test]
fn r6_max_output_is_enforced_inside_a_single_block() {
    let (bomb, expanded) = single_block_bomb();
    for cap in [0u64, 1, 4095, 4096, 4097, 1 << 20] {
        let mut stream = InflateStream::new().with_max_output(cap);
        let mut scratch = vec![0u8; 8192];
        let mut fed = 0usize;
        let mut produced = 0u64;
        let outcome = loop {
            match stream.inflate(&bomb[fed..], &mut scratch, FlushMode::Finish) {
                Ok(p) => {
                    fed += p.consumed;
                    produced += p.produced as u64;
                    assert!(produced <= cap, "cap {cap} overshot to {produced}");
                    if p.status == InflateStatus::StreamEnd {
                        break Ok(());
                    }
                }
                Err(e) => break Err(e),
            }
        };
        // Every cap below the expansion must fail, and must hand back
        // exactly its budget of valid bytes before doing so.
        assert!(cap < expanded);
        let err = outcome.expect_err("cap must fire");
        assert!(
            err.to_string().contains("memory budget"),
            "cap {cap}: {err}"
        );
        assert_eq!(produced, cap, "cap {cap} should produce exactly its budget");
        // The error is latched.
        assert!(
            stream.inflate(&[], &mut scratch, FlushMode::None).is_err(),
            "cap {cap}: the budget error must be sticky"
        );
    }

    // A cap at or above the expansion lets the whole stream through.
    let mut stream = InflateStream::new().with_max_output(expanded);
    let mut scratch = vec![0u8; 1 << 16];
    let mut fed = 0usize;
    let mut produced = 0u64;
    loop {
        let p = stream
            .inflate(&bomb[fed..], &mut scratch, FlushMode::Finish)
            .expect("exact cap");
        fed += p.consumed;
        produced += p.produced as u64;
        if p.status == InflateStatus::StreamEnd {
            break;
        }
    }
    assert_eq!(produced, expanded);
}

#[test]
fn r7_ratio_guard_fires_before_the_bomb_expands() {
    let (bomb, _) = single_block_bomb();
    let mut stream = InflateStream::new().with_ratio_guard(100.0, 1 << 20);
    let mut scratch = vec![0u8; 1 << 16];
    let mut fed = 0usize;
    let mut produced = 0u64;
    let err = loop {
        match stream.inflate(&bomb[fed..], &mut scratch, FlushMode::Finish) {
            Ok(p) => {
                fed += p.consumed;
                produced += p.produced as u64;
                assert!(
                    produced < 64 * 1024 * 1024,
                    "ratio guard let {produced} bytes through"
                );
                if p.status == InflateStatus::StreamEnd {
                    panic!("ratio guard never fired");
                }
            }
            Err(e) => break e,
        }
    };
    assert!(err.to_string().contains("zip bomb"), "{err}");
}

/// Build a fixed-Huffman block of `literals` copies of `b'A'` followed by
/// a length-3 match at distance 32768, then end-of-block.
fn window_boundary_stream(literals: usize) -> Vec<u8> {
    let mut bits = BitWriterLsb::new();
    bits.write(1, 1); // BFINAL = 1
    bits.write(1, 2); // BTYPE = 01, fixed Huffman
    for _ in 0..literals {
        // Literal 'A' (0x41): fixed codes 0..=143 are 8 bits, base 0x30.
        bits.write_code(0x30 + 0x41, 8);
    }
    bits.write_code(0x01, 7); // literal/length code 257 -> length 3
    bits.write_code(29, 5); // distance code 29
    bits.write(8191, 13); // maximum extra bits -> distance 32768
    bits.write_code(0x00, 7); // code 256, end of block
    bits.finish()
}

#[test]
fn r9_distance_at_and_past_the_window_boundary() {
    // Exactly 32768 bytes of history: distance 32768 is the largest legal
    // DEFLATE distance and must resolve. The 4 KiB output buffer forces the
    // match to be served out of the 32 KiB history window rather than out
    // of the caller's buffer, which is the path being tested.
    let legal = window_boundary_stream(32_768);
    let decoded = push_decode(&legal, 4096, 4096, 32_771).expect("distance 32768 is legal");
    assert_eq!(decoded.len(), 32_771);
    assert_eq!(&decoded[32_768..], b"AAA");

    // One byte less of history: the same distance now reaches past the
    // start of the stream and must be rejected, not silently zero-filled.
    let illegal = window_boundary_stream(32_767);
    let mut decoder = InflateStream::new();
    let mut out = vec![0u8; 4096];
    let mut fed = 0usize;
    let err = loop {
        match decoder.inflate(&illegal[fed..], &mut out, FlushMode::Finish) {
            Ok(p) => {
                fed += p.consumed;
                assert_ne!(p.status, InflateStatus::StreamEnd, "must not complete");
            }
            Err(e) => break e,
        }
    };
    assert!(err.to_string().contains("distance"), "{err}");
}

#[test]
fn r11_reserved_block_type_is_an_error() {
    let mut stream = InflateStream::new();
    let mut out = [0u8; 16];
    let err = stream
        .inflate(&[0b0000_0111], &mut out, FlushMode::Finish)
        .expect_err("BTYPE 3");
    assert!(err.to_string().contains("Reserved block type 3"), "{err}");
}

#[test]
fn r12_incomplete_code_length_code_is_rejected() {
    // A dynamic block whose 19-symbol code-length alphabet is incomplete
    // (a single 1-bit code). RFC 1951 §3.2.7 requires a complete code.
    let mut bits = BitWriterLsb::new();
    bits.write(1, 1); // BFINAL
    bits.write(2, 2); // BTYPE = dynamic
    bits.write(0, 5); // HLIT  = 257
    bits.write(0, 5); // HDIST = 1
    bits.write(0, 4); // HCLEN = 4
    // Four 3-bit code lengths in CODE_LENGTH_ORDER: 16, 17, 18, 0.
    bits.write(1, 3);
    bits.write(0, 3);
    bits.write(0, 3);
    bits.write(0, 3);
    let stream = bits.finish();
    let mut decoder = InflateStream::new();
    let mut out = [0u8; 64];
    let err = decoder
        .inflate(&stream, &mut out, FlushMode::Finish)
        .expect_err("incomplete code-length code");
    assert!(err.to_string().contains("Incomplete"), "{err}");
}

#[test]
fn r16_reset_clears_a_latched_fault() {
    let mut stream = InflateStream::new();
    let mut out = [0u8; 64];
    assert!(
        stream
            .inflate(&[0b0000_0111], &mut out, FlushMode::Finish)
            .is_err()
    );
    assert!(stream.inflate(&[], &mut out, FlushMode::None).is_err());

    stream.reset();
    let good = deflate(b"after the fault", 6).expect("deflate");
    let p = stream
        .inflate(&good, &mut out, FlushMode::Finish)
        .expect("clean decode after reset");
    assert_eq!(&out[..p.produced], b"after the fault");
    assert_eq!(p.status, InflateStatus::StreamEnd);
}

#[test]
fn r17_short_trailing_fragment_after_a_complete_member() {
    // 1-5 trailing bytes after a complete zlib member, including one
    // crafted to pass both the CM and the %31 header check, must be
    // ignored under `TrailingPolicy::Stop`.
    let member = zlib_compress(b"complete member", 6).expect("zlib");
    for fragment in [
        vec![0x00u8],
        vec![0x78, 0x9c],
        vec![0x78, 0x9c, 0x00],
        vec![b'X', b'Y', b'Z'],
        vec![0x78, 0x9c, 0x01, 0x02, 0x03],
    ] {
        let mut input = member.clone();
        input.extend_from_slice(&fragment);
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib)
            .multi_member(false)
            .trailing_policy(TrailingPolicy::Stop);
        let out = wrapped_decode(&mut decoder, &input, 1, 8, 15)
            .unwrap_or_else(|e| panic!("fragment {fragment:?}: {e}"));
        assert_eq!(out, b"complete member", "fragment {fragment:?}");
    }
}

// ---------------------------------------------------------------------------
// CAB-shaped reuse (R15) and dictionary cycles
// ---------------------------------------------------------------------------

#[test]
fn r15_cab_style_reset_and_dictionary_cycles() {
    // Mirrors oxiarc-archive's CAB/MSZIP folder loop: one long-lived
    // decoder, N blocks, each compressed against the previous blocks'
    // output as a preset dictionary, so blocks 2+ carry cross-block
    // back-references.
    const BLOCK: usize = 6000;
    let mut original = Vec::new();
    for i in 0..8u32 {
        let text = format!("cab folder block {i}: the quick brown fox jumps over the lazy dog. ");
        while original.len() < (i as usize + 1) * BLOCK {
            original.extend_from_slice(text.as_bytes());
        }
        original.truncate((i as usize + 1) * BLOCK);
    }

    // Compress each block with the preceding output as a dictionary.
    let mut blocks = Vec::new();
    for i in 0..8usize {
        let start = i * BLOCK;
        let end = start + BLOCK;
        let dict_start = start.saturating_sub(32 * 1024);
        let compressed = if i == 0 {
            deflate(&original[start..end], 6).expect("deflate")
        } else {
            let mut encoder = Deflater::new(6);
            encoder.set_dictionary(&original[dict_start..start]);
            let mut out = Vec::new();
            encoder
                .deflate(&original[start..end], &mut out, true)
                .expect("deflate with dictionary");
            out
        };
        blocks.push(compressed);
    }

    let mut stream = InflateStream::new();
    let mut rebuilt = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        stream.reset();
        if !rebuilt.is_empty() {
            let start = rebuilt.len().saturating_sub(32 * 1024);
            stream.set_dictionary(&rebuilt[start..]);
        }
        let decoded = stream
            .inflate_to_vec(block)
            .unwrap_or_else(|e| panic!("block {i}: {e}"));
        assert_eq!(decoded.len(), BLOCK, "block {i} length");
        rebuilt.extend_from_slice(&decoded);
    }
    assert_eq!(rebuilt, original);
}

#[test]
fn dictionary_works_through_the_bounded_push_api_too() {
    let dictionary = b"a preset dictionary shared by both sides of the stream";
    let payload = b"a preset dictionary shared by both sides of the stream, then new text";
    let mut encoder = Deflater::new(6);
    encoder.set_dictionary(dictionary);
    let mut compressed = Vec::new();
    encoder
        .deflate(payload, &mut compressed, true)
        .expect("deflate");

    for chunk in [1usize, 3, compressed.len()] {
        let mut stream = InflateStream::new();
        stream.set_dictionary(dictionary);
        let mut out = Vec::new();
        let mut scratch = [0u8; 5];
        let mut fed = 0usize;
        loop {
            let end = (fed + chunk).min(compressed.len());
            let flush = if end >= compressed.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let p = stream
                .inflate(&compressed[fed..end], &mut scratch, flush)
                .unwrap_or_else(|e| panic!("chunk {chunk}: {e}"));
            fed += p.consumed;
            out.extend_from_slice(&scratch[..p.produced]);
            if p.status == InflateStatus::StreamEnd {
                break;
            }
        }
        assert_eq!(out, payload, "chunk {chunk}");
    }
}

// ---------------------------------------------------------------------------
// A minimal LSB-first bit writer for the hand-built streams above
// ---------------------------------------------------------------------------

/// Writes DEFLATE's LSB-first bit order: values go out low bit first, and
/// Huffman codes go out most-significant bit first (RFC 1951 §3.1.1).
struct BitWriterLsb {
    out: Vec<u8>,
    acc: u32,
    bits: u8,
}

impl BitWriterLsb {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            bits: 0,
        }
    }

    /// Write `count` bits of `value`, low bit first (integers, extra bits).
    fn write(&mut self, value: u32, count: u8) {
        for i in 0..count {
            let bit = (value >> i) & 1;
            self.acc |= bit << self.bits;
            self.bits += 1;
            if self.bits == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.bits = 0;
            }
        }
    }

    /// Write a Huffman code of `count` bits, most significant bit first.
    fn write_code(&mut self, code: u32, count: u8) {
        for i in (0..count).rev() {
            self.write((code >> i) & 1, 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

// ---------------------------------------------------------------------------
// S4: proptest over random split points
// ---------------------------------------------------------------------------

mod proptests {
    use super::*;
    use proptest::prelude::*;

    /// Feed `data` through the push decoder using an arbitrary schedule of
    /// input-chunk and output-buffer sizes.
    fn decode_with_schedule(
        compressed: &[u8],
        in_sizes: &[usize],
        out_sizes: &[usize],
        expected_out: usize,
    ) -> oxiarc_core::error::Result<Vec<u8>> {
        let mut stream = InflateStream::new();
        let mut out = Vec::new();
        let mut fed = 0usize;
        let mut i = 0usize;
        let mut calls = 0usize;
        let bound = 8 * (compressed.len() + expected_out + 64);
        loop {
            calls += 1;
            assert!(calls < bound, "no progress after {calls} calls");
            let in_size = in_sizes
                .get(i % in_sizes.len())
                .copied()
                .unwrap_or(1)
                .max(1);
            let out_size = out_sizes
                .get(i % out_sizes.len())
                .copied()
                .unwrap_or(1)
                .max(1);
            i += 1;
            let mut scratch = vec![0u8; out_size];
            let end = (fed + in_size).min(compressed.len());
            let flush = if end >= compressed.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = stream.inflate(&compressed[fed..end], &mut scratch, flush)?;
            fed += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            if progress.status == InflateStatus::StreamEnd {
                return Ok(out);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// S4: every chunking of the input and every output-buffer schedule
        /// must produce byte-identical output.
        #[test]
        fn random_split_points_are_invariant(
            data in proptest::collection::vec(any::<u8>(), 0..4096),
            level in 0u8..=9,
            in_sizes in proptest::collection::vec(1usize..512, 1..8),
            out_sizes in proptest::collection::vec(1usize..4096, 1..8),
        ) {
            let compressed = deflate(&data, level).expect("deflate");
            let decoded =
                decode_with_schedule(&compressed, &in_sizes, &out_sizes, data.len())
                    .expect("push decode");
            prop_assert_eq!(decoded, data);
        }

        /// The same property over compressible input, which exercises long
        /// matches split across calls.
        #[test]
        fn random_split_points_over_matches(
            seed in any::<u64>(),
            len in 0usize..8192,
            level in 0u8..=9,
            in_sizes in proptest::collection::vec(1usize..64, 1..6),
            out_sizes in proptest::collection::vec(1usize..300, 1..6),
        ) {
            let mut data = Vec::with_capacity(len);
            let mut state = seed | 1;
            while data.len() < len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let word = format!("word{} ", state % 32);
                data.extend_from_slice(word.as_bytes());
            }
            data.truncate(len);
            let compressed = deflate(&data, level).expect("deflate");
            let decoded =
                decode_with_schedule(&compressed, &in_sizes, &out_sizes, data.len())
                    .expect("push decode");
            prop_assert_eq!(decoded, data);
        }

        /// Truncating anywhere must error in a bounded number of calls, and
        /// never claim `StreamEnd`.
        #[test]
        fn truncation_never_completes(
            data in proptest::collection::vec(any::<u8>(), 1..2048),
            level in 0u8..=9,
            cut in 0usize..2048,
        ) {
            let compressed = deflate(&data, level).expect("deflate");
            let cut = cut % compressed.len();
            prop_assert!(!probe(&compressed[..cut], 1, 64, data.len()));
        }

        /// Arbitrary bytes through every wrapper: no panic, no hang.
        #[test]
        fn arbitrary_bytes_are_survivable(
            junk in proptest::collection::vec(any::<u8>(), 0..512),
            wrapper_index in 0usize..4,
        ) {
            let wrapper = [
                InflateWrapper::Raw,
                InflateWrapper::Zlib,
                InflateWrapper::Gzip,
                InflateWrapper::Auto,
            ][wrapper_index];
            let mut decoder = WrappedInflate::new(wrapper)
                .multi_member(true)
                .trailing_policy(TrailingPolicy::Stop);
            let mut scratch = [0u8; 128];
            let mut fed = 0usize;
            let mut calls = 0usize;
            loop {
                calls += 1;
                prop_assert!(calls < 8192, "no progress on junk input");
                let slice = junk.get(fed..).unwrap_or_default();
                match decoder.inflate(slice, &mut scratch, FlushMode::Finish) {
                    Ok(p) => {
                        fed += p.consumed;
                        if p.status == InflateStatus::StreamEnd {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// strict_first_member: the one knob that differs at member 0
// ---------------------------------------------------------------------------

#[test]
fn strict_first_member_governs_offset_zero_only() {
    // The legacy `GzipStreamDecoder` contract: non-gzip leading bytes are an
    // empty successful decode, not an error. Every new type is strict.
    let junk = b"not a gzip stream at all";

    let mut lenient = WrappedInflate::new(InflateWrapper::Gzip)
        .strict_first_member(false)
        .trailing_policy(TrailingPolicy::Stop);
    let out = wrapped_decode(&mut lenient, junk, 1, 8, 0).expect("lenient must succeed");
    assert!(out.is_empty(), "lenient decode must produce nothing");
    assert_eq!(lenient.members_decoded(), 0);

    let mut strict = WrappedInflate::new(InflateWrapper::Gzip);
    assert!(
        wrapped_decode(&mut strict, junk, 1, 8, 0).is_err(),
        "the default must reject a non-gzip stream"
    );

    // Empty input is likewise an empty decode under the lenient setting.
    let mut lenient = WrappedInflate::new(InflateWrapper::Gzip).strict_first_member(false);
    assert!(
        wrapped_decode(&mut lenient, b"", 1, 8, 0)
            .expect("empty input")
            .is_empty()
    );

    // zlib: the `ZlibStreamDecoder` contract is the opposite — garbage at
    // member 0 is a hard error even though trailing garbage is tolerated.
    let mut strict = WrappedInflate::new(InflateWrapper::Zlib)
        .multi_member(true)
        .trailing_policy(TrailingPolicy::Stop);
    let err = wrapped_decode(&mut strict, b"XYZ garbage", 1, 8, 0)
        .expect_err("zlib must reject garbage at member 0");
    assert!(
        err.to_string().contains("zlib") || err.to_string().contains("method"),
        "{err}"
    );

    // ...but a *later* member's garbage follows the trailing policy.
    let mut input = zlib_compress(b"one good member", 6).expect("zlib");
    input.extend_from_slice(b"XYZ garbage");
    let mut decoder = WrappedInflate::new(InflateWrapper::Zlib)
        .multi_member(true)
        .trailing_policy(TrailingPolicy::Stop);
    assert_eq!(
        wrapped_decode(&mut decoder, &input, 1, 8, 15).expect("trailing garbage stops"),
        b"one good member"
    );
    assert_eq!(decoder.members_decoded(), 1);
}

#[test]
fn trailing_policy_reject_accepts_an_exact_stream() {
    // `Reject` must not be trigger-happy: a stream with nothing after the
    // last member is fine.
    for (framing, bytes) in [
        (
            InflateWrapper::Gzip,
            gzip_compress(b"exact", 6).expect("gzip"),
        ),
        (
            InflateWrapper::Zlib,
            zlib_compress(b"exact", 6).expect("zlib"),
        ),
    ] {
        let mut decoder = WrappedInflate::new(framing)
            .multi_member(true)
            .trailing_policy(TrailingPolicy::Reject);
        assert_eq!(
            wrapped_decode(&mut decoder, &bytes, 1, 4, 5).expect("exact stream"),
            b"exact",
            "{framing:?}"
        );

        // One stray byte is now an error.
        let mut with_junk = bytes.clone();
        with_junk.push(0x42);
        let mut decoder = WrappedInflate::new(framing)
            .multi_member(true)
            .trailing_policy(TrailingPolicy::Reject);
        assert!(
            wrapped_decode(&mut decoder, &with_junk, 1, 4, 5).is_err(),
            "{framing:?}: Reject must reject a trailing byte"
        );
    }
}

#[test]
fn wrapped_reset_returns_to_a_usable_initial_state() {
    let gz = gzip_compress(b"first use", 6).expect("gzip");
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip);
    assert_eq!(
        wrapped_decode(&mut decoder, &gz, 1, 8, 9).expect("first"),
        b"first use"
    );
    assert_eq!(decoder.members_decoded(), 1);

    // A latched error survives until reset.
    let mut broken = gz.clone();
    let last = broken.len() - 5;
    broken[last] ^= 0xFF;
    decoder.reset();
    assert!(wrapped_decode(&mut decoder, &broken, 1, 8, 9).is_err());
    assert!(
        decoder
            .inflate(&[], &mut [0u8; 8], FlushMode::None)
            .is_err(),
        "the wrapper's fault must be sticky too"
    );

    decoder.reset();
    let gz2 = gzip_compress(b"second use", 6).expect("gzip");
    assert_eq!(
        wrapped_decode(&mut decoder, &gz2, 1, 8, 10).expect("after reset"),
        b"second use"
    );
    assert_eq!(decoder.members_decoded(), 1, "counters reset too");
    assert_eq!(decoder.total_out(), 10);
}

/// Regression: deciding whether another member starts must read **two**
/// bytes.
///
/// `CM == 8` alone matches one byte in sixteen, so a single-byte test turns
/// ordinary trailing garbage into a hard header error and defeats
/// [`TrailingPolicy`] entirely — `b'X'` (0x58) has low nibble 8 and would
/// start a bogus zlib member.
#[test]
fn member_boundary_needs_two_bytes_to_decide() {
    let member = zlib_compress(b"payload", 6).expect("zlib");

    // Every trailing byte whose low nibble is 8 is a single-byte false
    // positive; none of them may start a member.
    for high in 0u8..16 {
        let byte = (high << 4) | 0x08;
        let mut input = member.clone();
        input.extend_from_slice(&[byte, 0x00, 0x00]);
        let mut decoder = WrappedInflate::new(InflateWrapper::Zlib)
            .multi_member(true)
            .trailing_policy(TrailingPolicy::Stop);
        let out = wrapped_decode(&mut decoder, &input, 1, 4, 7)
            .unwrap_or_else(|e| panic!("trailing {byte:#04x}: {e}"));
        assert_eq!(out, b"payload", "trailing {byte:#04x}");
        assert_eq!(decoder.members_decoded(), 1, "trailing {byte:#04x}");
    }

    // The same for gzip: 0x1F alone must not start a member.
    let member = gzip_compress(b"payload", 6).expect("gzip");
    let mut input = member.clone();
    input.extend_from_slice(&[0x1F, 0x42]);
    let mut decoder = WrappedInflate::new(InflateWrapper::Gzip)
        .multi_member(true)
        .trailing_policy(TrailingPolicy::Stop);
    assert_eq!(
        wrapped_decode(&mut decoder, &input, 1, 4, 7).expect("0x1F alone"),
        b"payload"
    );

    // And a genuine second member still starts, at every feed granularity.
    let mut two = member.clone();
    two.extend_from_slice(&gzip_compress(b" and more", 6).expect("gzip"));
    for chunk in [1usize, 2, 3, two.len()] {
        let mut decoder = WrappedInflate::new(InflateWrapper::Gzip).multi_member(true);
        assert_eq!(
            wrapped_decode(&mut decoder, &two, chunk, 4, 16)
                .unwrap_or_else(|e| panic!("chunk {chunk}: {e}")),
            b"payload and more"
        );
        assert_eq!(decoder.members_decoded(), 2, "chunk {chunk}");
    }
}

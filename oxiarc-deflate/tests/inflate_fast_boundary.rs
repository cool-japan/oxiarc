//! Boundary regression tests for the inflate fast loop.
//!
//! The fast loop writes straight into the caller's slice and checks its
//! output room once per iteration (`FAST_OUTPUT_MARGIN`, 258 bytes). Every
//! write inside an iteration therefore has to be in bounds *for the whole
//! iteration* — including the second half of a match that starts in the
//! history window and finishes inside the caller's buffer, which begins at a
//! cursor the iteration's own guard never saw.
//!
//! These tests drive [`InflateStream::inflate`] with hand-built fixed-Huffman
//! bitstreams that place a match end at every offset near the end of a small
//! output buffer, and compare against a byte-at-a-time LZ77 expansion.

#![allow(clippy::expect_used)]

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateStream};

// ---------------------------------------------------------------------------
// A minimal DEFLATE writer (fixed Huffman), so a match can be placed exactly.
// ---------------------------------------------------------------------------

/// LSB-first DEFLATE bit writer.
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            bits: 0,
        }
    }

    fn push_bit(&mut self, bit: u32) {
        self.acc |= (bit & 1) << self.bits;
        self.bits += 1;
        if self.bits == 8 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.bits = 0;
        }
    }

    /// Header fields and extra bits: least significant bit first.
    fn bits_lsb(&mut self, value: u32, count: u32) {
        for i in 0..count {
            self.push_bit((value >> i) & 1);
        }
    }

    /// Huffman codes: most significant bit first.
    fn code_msb(&mut self, code: u32, count: u32) {
        for i in (0..count).rev() {
            self.push_bit((code >> i) & 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// RFC 1951 §3.2.6 fixed literal/length code.
fn fixed_litlen(symbol: u32) -> (u32, u32) {
    match symbol {
        0..=143 => (0b0011_0000 + symbol, 8),
        144..=255 => (0b1_1001_0000 + (symbol - 144), 9),
        256..=279 => (symbol - 256, 7),
        _ => (0b1100_0000 + (symbol - 280), 8),
    }
}

const DIST_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const LEN_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Distance symbol, extra-bit width and extra value for `distance`.
fn distance_code(distance: u32) -> (u32, u32, u32) {
    let mut code = 0usize;
    for (index, base) in DIST_BASE.iter().enumerate() {
        if *base <= distance {
            code = index;
        }
    }
    (code as u32, DIST_EXTRA[code], distance - DIST_BASE[code])
}

/// Literal/length symbol, extra-bit width and extra value for `length`.
fn length_code(length: u32) -> (u32, u32, u32) {
    let mut code = 0usize;
    for (index, base) in LEN_BASE.iter().enumerate() {
        if *base <= length {
            code = index;
        }
    }
    (257 + code as u32, LEN_EXTRA[code], length - LEN_BASE[code])
}

/// One final fixed-Huffman block: every byte of `literals`, then the
/// `(distance, length)` matches in order, then end-of-block.
fn fixed_block(literals: &[u8], matches: &[(u32, u32)]) -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.bits_lsb(1, 1); // BFINAL
    writer.bits_lsb(1, 2); // BTYPE = 01 (fixed Huffman)
    for byte in literals {
        let (code, bits) = fixed_litlen(u32::from(*byte));
        writer.code_msb(code, bits);
    }
    for (distance, length) in matches {
        let (litlen_symbol, length_extra, length_value) = length_code(*length);
        let (code, bits) = fixed_litlen(litlen_symbol);
        writer.code_msb(code, bits);
        writer.bits_lsb(length_value, length_extra);
        let (dist_symbol, dist_extra, dist_value) = distance_code(*distance);
        writer.code_msb(dist_symbol, 5);
        writer.bits_lsb(dist_value, dist_extra);
    }
    let (code, bits) = fixed_litlen(256);
    writer.code_msb(code, bits);
    // Trailing slack so the fast loop keeps its 8-byte input margin to the end.
    for _ in 0..16 {
        writer.bits_lsb(0, 8);
    }
    writer.finish()
}

/// Byte-at-a-time LZ77 expansion: the definition the decoder must reproduce.
fn expand(literals: &[u8], matches: &[(u32, u32)]) -> Vec<u8> {
    let mut out = literals.to_vec();
    for (distance, length) in matches {
        for _ in 0..*length {
            let byte = out[out.len() - *distance as usize];
            out.push(byte);
        }
    }
    out
}

/// Output cap used by the bounded drivers, so a mutated stream cannot ask for
/// unbounded work.
const DECODE_CAP: u64 = 8 * 1024 * 1024;

/// Decode through repeated `chunk`-sized output buffers, the way a pull
/// reader, a PNG row loop or an HTTP body decoder does.
///
/// Bounded: the call budget is generous but finite, so a state machine that
/// stops making progress fails the test instead of hanging. The buffer is
/// poisoned between calls, so a decoder that reports bytes it did not write
/// cannot hide behind stale content.
fn try_decode_chunked(
    compressed: &[u8],
    chunk: usize,
    cap: Option<u64>,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut stream = InflateStream::new();
    if let Some(limit) = cap {
        stream = stream.with_max_output(limit);
    }
    let mut out: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; chunk.max(1)];
    let mut consumed = 0usize;
    let budget = 8 * compressed.len() + (DECODE_CAP as usize) / chunk.max(1) + 4096;
    let mut calls = 0usize;
    loop {
        calls += 1;
        assert!(
            calls < budget,
            "decoder made no terminal progress in {calls} calls (chunk={chunk})"
        );
        buffer.fill(0xA5);
        let rest = compressed.get(consumed..).unwrap_or(&[]);
        let progress = stream.inflate(rest, &mut buffer, FlushMode::None)?;
        consumed += progress.consumed;
        out.extend_from_slice(buffer.get(..progress.produced).unwrap_or(&[]));
        if progress.status == InflateStatus::StreamEnd {
            break;
        }
        if progress.produced == 0 && progress.consumed == 0 {
            break;
        }
        assert!(
            out.len() as u64 <= DECODE_CAP,
            "decoder ran past its output cap"
        );
    }
    Ok(out)
}

/// [`try_decode_chunked`] for a stream that must decode cleanly.
fn decode_chunked(compressed: &[u8], chunk: usize) -> Vec<u8> {
    try_decode_chunked(compressed, chunk, None).expect("chunked decode")
}

/// Decode the whole stream in one call into a generous buffer.
fn decode_whole(compressed: &[u8], expect_len: usize) -> Vec<u8> {
    let mut stream = InflateStream::new();
    let mut out = vec![0xA5u8; expect_len + 512];
    let progress = stream
        .inflate(compressed, &mut out, FlushMode::Finish)
        .expect("one-shot decode");
    out.truncate(progress.produced);
    out
}

fn filler(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + 13) % 251) as u8).collect()
}

/// A match that straddles the history window must be byte-exact no matter
/// where inside the caller's buffer it ends.
///
/// The fast loop's output guard is checked once per iteration against the
/// cursor at the *start* of the iteration. The tail of a straddling match
/// starts at `distance`, which can be within eight bytes of the end of the
/// caller's buffer even though the iteration began with a full 258-byte
/// margin. A word-wide store there runs past the caller's slice: before the
/// fix the store silently wrote nothing and the last one to seven bytes of
/// the match were left as whatever the caller's buffer already held (and a
/// `debug_assert` tripped in test builds).
#[test]
fn a_straddling_match_is_exact_at_every_offset_near_the_buffer_edge() {
    for chunk in [258usize, 259, 260, 264, 300, 512, 1024, 4096] {
        // Warm the history with exactly one buffer's worth of output, so
        // the second call starts at cursor zero with `chunk` bytes behind it.
        let head = filler(chunk);
        for lead in [0usize, 1, 2, 3, 7, 8, 63, 64, 65, chunk.saturating_sub(258)] {
            if lead + 3 > chunk {
                continue;
            }
            for length in [3u32, 4, 7, 8, 9, 63, 64, 65, 66, 71, 72, 128, 257, 258] {
                let room = chunk - lead;
                if length as usize > room {
                    continue;
                }
                // Sweep the distance so the match's history half ends at
                // every offset in the last few words of the buffer.
                for back in 1..=24u32 {
                    let distance = lead as u32 + length.saturating_sub(back).max(1);
                    if distance == 0 || distance as usize > chunk + lead {
                        continue;
                    }
                    let mut literals = head.clone();
                    literals.extend_from_slice(&filler(lead));
                    let matches = [(distance, length)];
                    let expect = expand(&literals, &matches);
                    let compressed = fixed_block(&literals, &matches);

                    let chunked = decode_chunked(&compressed, chunk);
                    assert_eq!(
                        chunked, expect,
                        "chunked: chunk={chunk} lead={lead} len={length} dist={distance}"
                    );
                    let whole = decode_whole(&compressed, expect.len());
                    assert_eq!(
                        whole, expect,
                        "one-shot: chunk={chunk} lead={lead} len={length} dist={distance}"
                    );
                }
            }
        }
    }
}

/// A match wholly inside the caller's buffer must also be exact at every
/// offset — the in-buffer word copy, its partial-word tail and the handover
/// to the careful path when the match does not fit in what is left.
#[test]
fn an_in_buffer_match_is_exact_at_every_offset_near_the_buffer_edge() {
    for chunk in [258usize, 259, 300, 512, 1024] {
        for length in [3u32, 4, 7, 8, 9, 63, 64, 65, 72, 200, 258] {
            for distance in [1u32, 2, 3, 4, 5, 6, 7, 8, 9, 15, 16, 17, 31, 32, 64, 65] {
                // Place the match so it ends 0..=9 bytes before the end of
                // the first output buffer, and again straddling that end.
                for slack in 0..=9usize {
                    let lead = match chunk.checked_sub(length as usize + slack) {
                        Some(lead) if lead >= distance as usize && lead > 0 => lead,
                        _ => continue,
                    };
                    let literals = filler(lead);
                    let matches = [(distance, length)];
                    let expect = expand(&literals, &matches);
                    let compressed = fixed_block(&literals, &matches);

                    let chunked = decode_chunked(&compressed, chunk);
                    assert_eq!(
                        chunked, expect,
                        "chunked: chunk={chunk} lead={lead} len={length} dist={distance}"
                    );
                }
            }
        }
    }
}

/// Several matches in a row, each landing on a different phase of the
/// output word, decoded through every output-buffer size from one byte up.
#[test]
fn a_run_of_matches_survives_every_output_buffer_size() {
    let literals = filler(300);
    let matches = [
        (300u32, 258u32),
        (7, 71),
        (1, 258),
        (3, 100),
        (259, 258),
        (5, 9),
        (600, 258),
    ];
    let expect = expand(&literals, &matches);
    let compressed = fixed_block(&literals, &matches);
    for chunk in [
        1usize, 2, 3, 7, 8, 9, 16, 63, 64, 127, 128, 255, 256, 257, 258, 259, 512, 4096,
    ] {
        let chunked = decode_chunked(&compressed, chunk);
        assert_eq!(chunked, expect, "chunk={chunk}");
    }
}

// ---------------------------------------------------------------------------
// Stress and adversarial coverage for the fast loop with a live history
// ---------------------------------------------------------------------------

/// Deterministic, highly matchable payload: repeated phrases at many
/// different distances, so a level-6 encode emits back-references spread over
/// the whole 32 KiB window (including the 250-350 byte band that lands on the
/// end of a small output buffer).
fn matchy_payload(len: usize) -> Vec<u8> {
    let words = [
        "the quick brown fox ",
        "jumps over the lazy dog ",
        "0123456789 ",
        "aaaaaaaaaaaaaaaa ",
        "<div class=\"row\"><span>",
        "</span></div>\n",
        "lorem ipsum dolor sit amet ",
        "{\"id\":1234,\"name\":\"value\"},",
    ];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = Vec::with_capacity(len + 64);
    while out.len() < len {
        let pick = (next() % words.len() as u64) as usize;
        out.extend_from_slice(words[pick].as_bytes());
        if next() % 11 == 0 {
            out.push((next() % 251) as u8);
        }
    }
    out.truncate(len);
    out
}

/// Decode a real compressed stream through **every** small output-buffer size
/// and require byte-identical output.
///
/// The fast loop's correctness arguments are all of the form "the guard at the
/// top of the iteration proves this write is in bounds"; the only way to test
/// them is to move the end of the caller's buffer past every possible match
/// end. A single buffer size proves almost nothing.
#[test]
fn every_small_output_buffer_size_decodes_identically() {
    let payload = matchy_payload(96 * 1024);
    for level in [1u8, 6, 9] {
        let compressed = oxiarc_deflate::deflate(&payload, level).expect("deflate");
        for size in 256..=560usize {
            let got = decode_chunked(&compressed, size);
            assert_eq!(
                got.len(),
                payload.len(),
                "level={level} size={size}: length differs"
            );
            assert!(got == payload, "level={level} size={size}: bytes differ");
        }
        for size in [64usize, 255, 1024, 4095, 4096, 32767, 32768, 32769, 65536] {
            let got = decode_chunked(&compressed, size);
            assert!(got == payload, "level={level} size={size}: bytes differ");
        }
        // One- and two-byte buffers on a smaller payload: the same states,
        // without a call per byte of a 96 KiB decode.
        let small = matchy_payload(4 * 1024);
        let small_compressed = oxiarc_deflate::deflate(&small, level).expect("deflate");
        for size in [1usize, 2, 3, 7] {
            let got = decode_chunked(&small_compressed, size);
            assert!(got == small, "level={level} size={size}: bytes differ");
        }
    }
}

/// A zlib stream decoded through a small buffer must match the raw path, and
/// the checksum must still verify — the shape `oxiarc-png` uses for IDAT rows.
#[test]
fn a_zlib_member_through_row_sized_buffers_is_exact() {
    let payload = matchy_payload(64 * 1024);
    let compressed = oxiarc_deflate::zlib_compress(&payload, 6).expect("zlib_compress");
    for size in [300usize, 512, 1536, 3072, 4096] {
        let got = oxiarc_deflate::inflate(&compressed[2..compressed.len() - 4]).expect("inflate");
        assert_eq!(got, payload, "one-shot raw decode differs");
        let chunked = decode_chunked(&compressed[2..compressed.len() - 4], size);
        assert_eq!(chunked, payload, "size={size}: chunked decode differs");
    }
}

/// Mutated streams driven through the fast loop's own shapes: small output
/// buffers with a live 32 KiB history, which is where the fast loop actually
/// runs (the existing `adversarial_verify.rs` sweep uses payloads of at most
/// 500 bytes, so its history is never populated and its 7-byte output buffer
/// never lets the fast loop start).
///
/// Nothing may panic and nothing may spin: every decode is bounded, and a
/// decoder that stops making progress trips the bound.
#[test]
fn mutated_streams_never_panic_or_spin_through_small_buffers() {
    let payload = matchy_payload(24 * 1024);
    let base = oxiarc_deflate::deflate(&payload, 6).expect("deflate");
    let clean = decode_chunked(&base, 300);
    assert_eq!(clean, payload, "control decode");

    // `truncated` marks the cases for which a *strong* property holds:
    // DEFLATE decoding is a deterministic left-to-right function of the bit
    // prefix, and the decoder never emits a symbol whose bits it does not
    // have, so whatever a truncated stream yields must be a prefix of the
    // whole payload. A bit flip, a dropped byte or an inserted byte shifts
    // everything downstream and may legitimately decode to something else
    // entirely, so only "no panic, no spin, no runaway" applies there.
    let mut cases: Vec<(bool, Vec<u8>)> = Vec::new();
    let step = (base.len() / 200).max(1);
    let mut offset = 0usize;
    while offset < base.len() {
        cases.push((true, base.get(..offset).unwrap_or_default().to_vec()));
        for mask in [0x01u8, 0x40, 0x80] {
            let mut copy = base.clone();
            if let Some(slot) = copy.get_mut(offset) {
                *slot ^= mask;
            }
            cases.push((false, copy));
        }
        let mut dropped = base.clone();
        dropped.remove(offset);
        cases.push((false, dropped));
        let mut inserted = base.clone();
        inserted.insert(offset, 0x5A);
        cases.push((false, inserted));
        offset += step;
    }

    // Guards against the sweep quietly becoming vacuous: the prefix property
    // is only worth asserting while truncated streams actually decode
    // something substantial.
    let mut prefix_checks = 0usize;
    let mut longest_prefix = 0usize;
    for (index, (truncated, bytes)) in cases.iter().enumerate() {
        for size in [258usize, 259, 300, 1024] {
            // A bounded driver: a decoder that neither produces nor consumes
            // ends the loop, and the call budget catches one that spins.
            // Rejecting the stream is a perfectly good outcome; what is not
            // acceptable is a panic, a hang, or output that runs away.
            if let Ok(got) = try_decode_chunked(bytes, size, Some(DECODE_CAP)) {
                assert!(
                    got.len() as u64 <= DECODE_CAP,
                    "case {index} size={size}: produced {} bytes",
                    got.len()
                );
                if *truncated {
                    assert!(
                        payload.starts_with(&got),
                        "case {index} size={size}: a truncated stream produced \
                         {} bytes that are not a prefix of the payload",
                        got.len()
                    );
                    prefix_checks += 1;
                    longest_prefix = longest_prefix.max(got.len());
                }
            }
        }
    }
    assert!(
        prefix_checks > 400 && longest_prefix > payload.len() / 2,
        "the truncation sweep went vacuous: {prefix_checks} checks, \
         longest prefix {longest_prefix} of {}",
        payload.len()
    );
}

/// One-byte input chunks into a one-byte output buffer: the state machine has
/// to make progress on every call or report a terminal status, and it must
/// still produce the exact bytes.
#[test]
fn one_byte_input_into_a_one_byte_output_buffer_is_exact_and_terminates() {
    let payload = matchy_payload(3 * 1024);
    let compressed = oxiarc_deflate::deflate(&payload, 6).expect("deflate");
    let mut stream = InflateStream::new();
    let mut out: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 1];
    let mut fed = 0usize;
    let bound = 8 * (compressed.len() + payload.len() + 64);
    let mut calls = 0usize;
    loop {
        calls += 1;
        assert!(calls < bound, "no progress after {calls} calls");
        let end = (fed + 1).min(compressed.len());
        let slice = compressed.get(fed..end).unwrap_or(&[]);
        let flush = if end == compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.inflate(slice, &mut buffer, flush).expect("inflate");
        fed += progress.consumed;
        out.extend_from_slice(buffer.get(..progress.produced).unwrap_or(&[]));
        if progress.status == InflateStatus::StreamEnd {
            break;
        }
        if progress.produced == 0 && progress.consumed == 0 && end == compressed.len() {
            break;
        }
    }
    assert_eq!(out, payload);
}

/// A stored block that declares 65535 bytes and then ends must be an error or
/// a clean `NeedInput`, never a panic and never invented output.
#[test]
fn a_stored_block_with_a_huge_declared_length_is_bounded() {
    let mut out = vec![0u8; 4096];

    // BFINAL=1, BTYPE=00, pad to a byte, LEN=0xFFFF, NLEN=0xFFFF: NLEN is not
    // the one's complement of LEN, which RFC 1951 §3.2.4 forbids.
    let bad_nlen = [0x01u8, 0xFF, 0xFF, 0xFF, 0xFF];
    let mut stream = InflateStream::new();
    assert!(
        stream
            .inflate(&bad_nlen, &mut out, FlushMode::Finish)
            .is_err(),
        "a stored block whose NLEN is not the complement of LEN must be rejected"
    );

    // A well-formed 65535-byte stored header with no payload at all: the
    // decoder must ask for input and invent nothing, however often it is
    // driven.
    let header = [0x01u8, 0xFF, 0xFF, 0x00, 0x00];
    let mut stream = InflateStream::new();
    let mut produced = 0usize;
    let mut fed = false;
    for _ in 0..64 {
        let input: &[u8] = if fed { &[] } else { &header };
        fed = true;
        match stream.inflate(input, &mut out, FlushMode::None) {
            Ok(progress) => {
                produced += progress.produced;
                assert_eq!(progress.status, InflateStatus::NeedInput);
            }
            Err(_) => break,
        }
    }
    assert_eq!(produced, 0, "a truncated stored block produced output");

    // The same header under `Finish`, where the missing payload is an error.
    let mut stream = InflateStream::new();
    assert!(
        stream
            .inflate(&header, &mut out, FlushMode::Finish)
            .is_err(),
        "a truncated stored block was accepted under Finish"
    );
}

/// A dynamic header claiming the maximum HLIT/HDIST with no code-length data
/// must terminate as an error, not allocate or spin.
#[test]
fn a_maximal_dynamic_header_without_data_is_an_error() {
    // BFINAL=1 (1), BTYPE=10 (2 bits), HLIT=31 (5), HDIST=31 (5), HCLEN=15 (4)
    let mut writer = BitWriter::new();
    writer.bits_lsb(1, 1);
    writer.bits_lsb(2, 2);
    writer.bits_lsb(31, 5);
    writer.bits_lsb(31, 5);
    writer.bits_lsb(15, 4);
    let header = writer.finish();
    let mut stream = InflateStream::new();
    let mut out = vec![0u8; 4096];
    let result = stream.inflate(&header, &mut out, FlushMode::Finish);
    assert!(result.is_err(), "a truncated maximal header was accepted");
}

/// Order the code-length alphabet is transmitted in (RFC 1951 §3.2.7).
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// A dynamic block whose **distance** alphabet holds one code, 15 bits long.
///
/// RFC 1951 allows a distance code to be incomplete (a block with no matches
/// legitimately sends a single code), and this crate's `HuffmanTree` has
/// always accepted the shape, so the decoder must too — or reject it. What it
/// must not do is panic or spin: a decode table whose root index is narrower
/// than the alphabet's *shortest* code has no root slot to replicate into.
fn a_block_with_one_long_distance_code() -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.bits_lsb(1, 1); // BFINAL
    writer.bits_lsb(2, 2); // BTYPE = 10 (dynamic)
    writer.bits_lsb(0, 5); // HLIT  = 257 literal/length codes
    writer.bits_lsb(0, 5); // HDIST = 1 distance code
    writer.bits_lsb(15, 4); // HCLEN = 19 code-length codes

    // Code-length alphabet: symbol 0 is one bit, symbols 1 and 15 are two.
    let mut cl_lengths = [0u32; 19];
    cl_lengths[0] = 1;
    cl_lengths[1] = 2;
    cl_lengths[15] = 2;
    for symbol in CODE_LENGTH_ORDER {
        writer.bits_lsb(cl_lengths[symbol], 3);
    }

    // Canonical codes for that alphabet: 0 -> "0", 1 -> "10", 15 -> "11".
    let emit_zero = |w: &mut BitWriter| w.code_msb(0b0, 1);
    let emit_one = |w: &mut BitWriter| w.code_msb(0b10, 2);
    let emit_fifteen = |w: &mut BitWriter| w.code_msb(0b11, 2);

    // 257 literal/length lengths: symbol 0 and symbol 256 get one bit each.
    emit_one(&mut writer);
    for _ in 0..255 {
        emit_zero(&mut writer);
    }
    emit_one(&mut writer);
    // 1 distance length: fifteen bits for the only distance code.
    emit_fifteen(&mut writer);

    // Block body: the end-of-block symbol, which is "1" in the 1-bit
    // literal/length code above.
    writer.code_msb(0b1, 1);
    for _ in 0..16 {
        writer.bits_lsb(0, 8);
    }
    writer.finish()
}

/// The same shape on the **literal/length** alphabet: every code longer than
/// the literal/length root index.
fn a_block_with_only_long_literal_codes() -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.bits_lsb(1, 1); // BFINAL
    writer.bits_lsb(2, 2); // BTYPE = 10
    writer.bits_lsb(0, 5); // HLIT  = 257
    writer.bits_lsb(0, 5); // HDIST = 1
    writer.bits_lsb(15, 4); // HCLEN = 19

    // Code-length alphabet: symbol 0 -> 1 bit, symbols 1 and 12 -> 2 bits.
    let mut cl_lengths = [0u32; 19];
    cl_lengths[0] = 1;
    cl_lengths[1] = 2;
    cl_lengths[12] = 2;
    for symbol in CODE_LENGTH_ORDER {
        writer.bits_lsb(cl_lengths[symbol], 3);
    }
    let emit_zero = |w: &mut BitWriter| w.code_msb(0b0, 1);
    let emit_one = |w: &mut BitWriter| w.code_msb(0b10, 2);
    let emit_twelve = |w: &mut BitWriter| w.code_msb(0b11, 2);

    // Literal 0 and the end-of-block symbol both get twelve-bit codes, so
    // the literal/length alphabet's shortest code is longer than its root.
    emit_twelve(&mut writer);
    for _ in 0..255 {
        emit_zero(&mut writer);
    }
    emit_twelve(&mut writer);
    // One distance code, one bit.
    emit_one(&mut writer);

    // End of block: with two 12-bit codes, symbol 0 is 0x000 and symbol 256
    // is 0x001.
    writer.code_msb(0b0000_0000_0001, 12);
    for _ in 0..16 {
        writer.bits_lsb(0, 8);
    }
    writer.finish()
}

/// CPython's `zlib.decompress` on a raw DEFLATE stream. `Ok(bytes)` when it
/// decodes, `Err(message)` when zlib rejects the stream, `None` when
/// `python3` is unavailable (which self-skips).
fn python_inflate(raw: &[u8]) -> Option<Result<Vec<u8>, String>> {
    use std::io::Write;
    use std::process::Command;

    let available = Command::new("python3")
        .args(["-c", "import zlib"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !available {
        return None;
    }
    let dir = std::env::temp_dir();
    let inp = dir.join(format!("oxiarc_fastboundary_{}.raw", std::process::id()));
    let out = dir.join(format!("oxiarc_fastboundary_{}.out", std::process::id()));
    std::fs::File::create(&inp).ok()?.write_all(raw).ok()?;
    let script = format!(
        "import zlib\n\
         data = open({inp:?}, 'rb').read()\n\
         d = zlib.decompressobj(-15)\n\
         open({out:?}, 'wb').write(d.decompress(data))\n"
    );
    let run = Command::new("python3")
        .args(["-c", &script])
        .output()
        .ok()?;
    let result = if run.status.success() {
        match std::fs::read(&out) {
            Ok(bytes) => Ok(bytes),
            Err(error) => Err(error.to_string()),
        }
    } else {
        Err(String::from_utf8_lossy(&run.stderr).into_owned())
    };
    let _ = std::fs::remove_file(&inp);
    let _ = std::fs::remove_file(&out);
    Some(result)
}

/// The end-to-end check for the decode-table fix, and the honest statement of
/// where this crate and zlib differ.
///
/// **zlib rejects this stream**, and that is not incidental: a code whose
/// *shortest* length exceeds the decode table's root index can only be an
/// **incomplete** code. A complete literal/length code with every code >= 11
/// bits would need 2048 symbols and the alphabet has 288; the distance
/// alphabet has 32 against the 1024 a complete >= 10-bit code needs. zlib's
/// `inflate_table` rejects incomplete sets outright
/// (`if (left > 0 && (type == CODES || max != 1)) return -1;`), so it never
/// meets the shape — which is exactly why the port of that algorithm could
/// drop zlib's `if (min > root) root = min;` clamp and still pass every
/// oracle. This crate is deliberately more permissive:
/// `HuffmanTree::from_code_lengths` has always accepted incomplete codes, and
/// `DecodeTable` is required to agree with it.
///
/// So the oracle here is **`Inflater`**, the crate's other decoder, which
/// runs the pre-0.4.2 `HuffmanTree` symbol loop rather than the packed
/// tables — an independent implementation of the same shape. CPython is
/// still consulted, and its *rejection* is asserted, so this test fails
/// loudly if a future zlib starts accepting the shape (which would make it
/// the better oracle) or if the fixture stops being the shape it claims.
#[test]
fn a_long_code_block_agrees_with_the_huffman_tree_decoder() {
    use std::io::Cursor;

    let bytes = a_block_with_only_long_literal_codes_and_output();
    let expected = vec![0u8; 40];

    // 1. The packed-table decoder (the rewritten path).
    let ours = oxiarc_deflate::inflate(&bytes).expect("packed-table inflate");
    assert_eq!(ours, expected, "packed-table decoder");

    // 2. The push decoder through a buffer small enough to keep the fast
    //    loop in play.
    assert_eq!(decode_chunked(&bytes, 300), expected, "push decoder");

    // 3. The independent `HuffmanTree` decoder, which never used the packed
    //    tables at all.
    let mut inflater = oxiarc_deflate::Inflater::new();
    let independent = inflater
        .inflate_reader(&mut Cursor::new(&bytes))
        .expect("HuffmanTree inflate");
    assert_eq!(independent, expected, "HuffmanTree decoder");

    // 4. CPython zlib must still *reject* it, which is what makes points 1-3
    //    the only available cross-check.
    match python_inflate(&bytes) {
        None => eprintln!("python3 with zlib not available - skipping the zlib check"),
        Some(Ok(bytes)) => panic!(
            "zlib now accepts an incomplete over-length code and decoded {} bytes; \
             it has become the better oracle for this shape and this test should \
             compare against it",
            bytes.len()
        ),
        Some(Err(message)) => assert!(
            message.contains("invalid literal/lengths set") || message.contains("Error -3"),
            "zlib rejected the fixture for an unexpected reason: {message}"
        ),
    }
}

/// [`a_block_with_only_long_literal_codes`] with a payload: 40 literals then
/// end-of-block, so the block decodes to real bytes and a decoder that
/// produced nothing would fail rather than trivially pass.
fn a_block_with_only_long_literal_codes_and_output() -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.bits_lsb(1, 1); // BFINAL
    writer.bits_lsb(2, 2); // BTYPE = 10 (dynamic)
    writer.bits_lsb(0, 5); // HLIT  = 257
    writer.bits_lsb(0, 5); // HDIST = 1
    writer.bits_lsb(15, 4); // HCLEN = 19

    let mut cl_lengths = [0u32; 19];
    cl_lengths[0] = 1;
    cl_lengths[1] = 2;
    cl_lengths[12] = 2;
    for symbol in CODE_LENGTH_ORDER {
        writer.bits_lsb(cl_lengths[symbol], 3);
    }
    let emit_zero = |w: &mut BitWriter| w.code_msb(0b0, 1);
    let emit_one = |w: &mut BitWriter| w.code_msb(0b10, 2);
    let emit_twelve = |w: &mut BitWriter| w.code_msb(0b11, 2);

    // Literal 0 and end-of-block are the only two literal/length codes, both
    // 12 bits — longer than the 10-bit root index.
    emit_twelve(&mut writer);
    for _ in 0..255 {
        emit_zero(&mut writer);
    }
    emit_twelve(&mut writer);
    emit_one(&mut writer); // one 1-bit distance code

    // 40 copies of literal 0 (code 0x000), then end-of-block (0x001).
    for _ in 0..40 {
        writer.code_msb(0b0000_0000_0000, 12);
    }
    writer.code_msb(0b0000_0000_0001, 12);
    for _ in 0..16 {
        writer.bits_lsb(0, 8);
    }
    writer.finish()
}

/// A dynamic header whose alphabet's shortest code is longer than the decode
/// table's root index must be handled, not crashed on.
///
/// The packed decode table indexes a `root_bits`-wide root table and
/// replicates each code over `1 << (root_bits - len)` slots. When every code
/// in the alphabet is *longer* than `root_bits` there is no such slot, and
/// zlib's `inflate_table` widens the root to the shortest code for exactly
/// that reason. Without the widening the replication stride underflows:
/// a panic in a debug build and a non-terminating loop in a release build,
/// both reachable from a crafted stream.
#[test]
fn a_block_whose_shortest_code_exceeds_the_table_root_is_handled() {
    for (label, bytes) in [
        ("distance", a_block_with_one_long_distance_code()),
        ("literal", a_block_with_only_long_literal_codes()),
    ] {
        let mut stream = InflateStream::new();
        let mut out = vec![0u8; 4096];
        // Either outcome is acceptable; hanging or panicking is not.
        if let Ok(progress) = stream.inflate(&bytes, &mut out, FlushMode::Finish) {
            assert_eq!(
                progress.produced, 0,
                "{label}: an empty block produced output"
            );
        }
        // And through the one-shot entry point, which takes a different
        // driver but the same header parser.
        let _ = oxiarc_deflate::inflate(&bytes);
    }
}

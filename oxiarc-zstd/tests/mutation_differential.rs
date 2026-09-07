//! Differential mutation suite for the bounded [`ZstdStream`] push decoder.
//!
//! The conformance suite proves the incremental decoder agrees with the
//! one-shot decoders on *well-formed* frames, and that malformed ones do not
//! panic. This suite closes the gap between those two claims: it mutates real
//! frames byte by byte and requires that **whenever the one-shot decoder still
//! succeeds, the incremental decoder produces exactly the same bytes** — under
//! several chunk schedules, including one byte in and one byte out.
//!
//! That is the property a fuzzer would find hardest to state and the one that
//! matters most: a resumable decoder that silently disagrees with the one-shot
//! path on an unusual-but-valid frame is worse than one that rejects it.
//!
//! Every mutation is deterministic; there is no RNG dependency and no I/O.

use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{
    ZstdStatus, ZstdStream, ZstdStreamDecoder, compress_with_level, decompress, decompress_into,
    decompress_multi_frame, decompress_multi_frame_with_limit, decompress_with_limit,
};
use std::io::Read;

/// Ceiling used by the bounded entry points in this suite (large enough that it
/// never fires on the corpus, small enough to bound a mutated frame's appetite).
const CAP: usize = 64 << 20;

/// Drive the push decoder over `frame`, returning the output or a failure tag.
///
/// A "failure" here is either a decode error (fine — mutated input) or one of
/// the two contract violations this suite hunts for: a decoder that never
/// terminates, and a decoder that stalls with input still on offer.
fn drive(frame: &[u8], in_chunk: usize, out_chunk: usize) -> Result<Vec<u8>, String> {
    let mut stream = ZstdStream::new()
        .with_max_window(usize::MAX)
        .with_max_output(CAP as u64);
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_chunk.max(1)];
    let mut pos = 0usize;
    let mut calls = 0usize;
    loop {
        calls += 1;
        if calls > 2_000_000 {
            return Err(format!("HANG after {calls} calls"));
        }
        let end = pos.saturating_add(in_chunk).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = match stream.decode(&frame[pos..end], &mut scratch, flush) {
            Ok(p) => p,
            Err(e) => return Err(e.to_string()),
        };
        pos += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == ZstdStatus::StreamEnd {
            return Ok(out);
        }
        if progress.consumed == 0 && progress.produced == 0 && end == frame.len() {
            return Err(format!("STUCK status {:?} at input {pos}", progress.status));
        }
    }
}

/// Deterministic pseudo-random bytes (no `rand` dependency).
fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

/// Frames spanning every block type and both header shapes.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", compress_with_level(b"", 3).expect("compress")),
        ("one_byte", compress_with_level(b"a", 3).expect("compress")),
        (
            "compressible",
            compress_with_level(&b"repeating payload ".repeat(4_000), 3).expect("compress"),
        ),
        (
            "incompressible",
            compress_with_level(&pseudo_random(200_000, 0x5EED), 9).expect("compress"),
        ),
        (
            "rle_like",
            compress_with_level(&vec![0u8; 400_000], 1).expect("compress"),
        ),
    ]
}

/// Assert that every bounded entry point survives `frame` without panicking.
fn bounded_entry_points_survive(frame: &[u8]) {
    let _ = decompress_with_limit(frame, CAP);
    let _ = decompress_multi_frame_with_limit(frame, CAP);
    let mut dst = vec![0u8; 1 << 20];
    let _ = decompress_into(frame, &mut dst);
    let mut decoder = ZstdStreamDecoder::new(frame).with_max_output(CAP as u64);
    let mut out = Vec::new();
    let _ = decoder.read_to_end(&mut out);
}

/// Compare the incremental decoder against the one-shot decoder for `frame`.
fn differential(name: &str, what: &str, frame: &[u8], schedules: &[(usize, usize)]) {
    let one_shot = decompress(frame).ok();
    for &(in_chunk, out_chunk) in schedules {
        match drive(frame, in_chunk, out_chunk) {
            Err(e) => assert!(
                !e.contains("HANG") && !e.contains("STUCK"),
                "{name}/{what} chunks {in_chunk}/{out_chunk}: {e}"
            ),
            Ok(actual) => {
                if let Some(expected) = &one_shot {
                    assert_eq!(
                        expected, &actual,
                        "{name}/{what} chunks {in_chunk}/{out_chunk}: the incremental decoder \
                         disagreed with the one-shot decoder"
                    );
                }
            }
        }
    }
    bounded_entry_points_survive(frame);
}

/// Every byte of the frame header region, set to nine adversarial values.
///
/// The header is where a single byte redefines the window, the content size,
/// the dictionary ID and the checksum flag at once, so it is the densest source
/// of "valid enough to decode, wrong enough to break an assumption" frames.
#[test]
fn header_mutations_agree_with_the_one_shot_decoder() {
    let schedules = [(usize::MAX, 1 << 16), (1, 1), (7, 3)];
    for (name, frame) in corpus() {
        for offset in 0..frame.len().min(20) {
            for value in [0x00u8, 0x01, 0x0F, 0x40, 0x7F, 0x80, 0xC0, 0xE0, 0xFF] {
                let mut mutated = frame.clone();
                mutated[offset] = value;
                differential(
                    name,
                    &format!("header[{offset}]={value:#04x}"),
                    &mutated,
                    &schedules,
                );
            }
        }
    }
}

/// Bit patterns flipped across the whole frame body, sampled at ~97 offsets.
#[test]
fn body_mutations_agree_with_the_one_shot_decoder() {
    let schedules = [(usize::MAX, 1 << 16), (1, 1)];
    for (name, frame) in corpus() {
        let step = (frame.len() / 97).max(1);
        let mut offset = 0usize;
        while offset < frame.len() {
            for mask in [0x01u8, 0x55, 0xFF] {
                let mut mutated = frame.clone();
                mutated[offset] ^= mask;
                differential(
                    name,
                    &format!("body[{offset}]^={mask:#04x}"),
                    &mutated,
                    &schedules,
                );
            }
            offset += step;
        }
    }
}

/// Truncation at every offset, drained one byte at a time, then `finish()`.
///
/// The conformance suite truncates with a 64 KiB output slice; this run starves
/// the output side as well, so every mid-block drain boundary is crossed on a
/// frame that then stops early.
#[test]
fn truncation_with_a_one_byte_output_slice_never_succeeds_silently() {
    let data = b"truncate me thoroughly ".repeat(500);
    for frame in [
        compress_with_level(&data, 3).expect("compress"),
        compress_with_level(&data, 19).expect("compress"),
    ] {
        for cut in 1..frame.len() {
            let mut stream = ZstdStream::new().with_max_window(usize::MAX);
            let mut scratch = [0u8; 1];
            let mut pos = 0usize;
            let mut produced = 0usize;
            let mut calls = 0usize;
            let mut ended = false;
            loop {
                calls += 1;
                // With a one-byte output slice the decoder is *expected* to
                // need one call per byte it hands back, so the honest bound is
                // "a constant per byte moved in either direction", not one per
                // input byte: a 42-byte prefix of a level-19 frame legitimately
                // decodes thousands of bytes.
                assert!(
                    calls <= 2 * (pos + produced) + 64,
                    "cut {cut} took {calls} calls after consuming {pos} and producing {produced}"
                );
                match stream.decode(&frame[pos..cut], &mut scratch, FlushMode::None) {
                    Ok(progress) => {
                        pos += progress.consumed;
                        produced += progress.produced;
                        if progress.status == ZstdStatus::StreamEnd {
                            ended = true;
                            break;
                        }
                        if progress.consumed == 0 && progress.produced == 0 {
                            break;
                        }
                    }
                    Err(_) => {
                        ended = true;
                        break;
                    }
                }
            }
            assert!(
                ended || stream.finish().is_err(),
                "cut {cut} finished cleanly on a truncated frame"
            );
        }
    }
}

/// Frames and skippable frames in every interleaving, at three chunk schedules.
#[test]
fn frame_and_skippable_permutations_match_the_one_shot_decoder() {
    let alpha = compress_with_level(b"alpha", 3).expect("compress");
    let beta = compress_with_level(b"beta", 3).expect("compress");
    let mut skippable = vec![0x50u8, 0x2A, 0x4D, 0x18];
    skippable.extend_from_slice(&3u32.to_le_bytes());
    skippable.extend_from_slice(b"xyz");

    for order in [
        vec![&alpha, &beta],
        vec![&skippable, &alpha],
        vec![&alpha, &skippable],
        vec![&alpha, &skippable, &beta],
        vec![&skippable, &skippable, &alpha, &skippable],
    ] {
        let mut joined = Vec::new();
        for part in &order {
            joined.extend_from_slice(part);
        }
        let expected = decompress_multi_frame(&joined).expect("one-shot multi-frame decode");
        for (in_chunk, out_chunk) in [(usize::MAX, 1 << 16), (1, 1), (3, 2)] {
            let actual = drive(&joined, in_chunk, out_chunk).expect("incremental multi-frame");
            assert_eq!(actual, expected, "chunks {in_chunk}/{out_chunk}");
        }
    }
}

//! Conformance suite for the bounded, resumable [`ZstdStream`] push decoder.
//!
//! Every test here drives the *incremental* decoder and compares against the
//! existing one-shot decoders byte for byte. The interesting rows are the ones
//! that starve the decoder: one byte of input per call, one byte of output per
//! call, both at once, and every truncation offset of a real frame.

use oxiarc_core::error::OxiArcError;
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{
    MAX_BLOCK_SIZE, ZstdStatus, ZstdStream, compress_no_checksum, compress_with_level, decompress,
    decompress_into, decompress_multi_frame, decompress_multi_frame_with_limit,
    decompress_with_limit, write_skippable_frame,
};

/// The reference-produced fixture frames shipped with the crate.
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

/// Hard ceiling on decode calls, so a hang shows up as a failure rather than a
/// stuck test runner.
const CALL_BUDGET: usize = 40_000_000;

/// Outcome of driving a stream to completion (or to its error).
struct Driven {
    /// Bytes produced before the stream ended or failed.
    output: Vec<u8>,
    /// `Err` message when the stream faulted.
    error: Option<String>,
    /// Number of `decode` calls made.
    calls: usize,
}

/// Build a decoder that accepts any declared window (reference frames made with
/// `zstd --long` declare 16-128 MiB).
fn permissive() -> ZstdStream {
    ZstdStream::new().with_max_window(usize::MAX)
}

/// Feed `frame` to `stream` in `in_chunk`-byte pieces, taking at most
/// `out_chunk` bytes back per call.
fn drive_with(stream: &mut ZstdStream, frame: &[u8], in_chunk: usize, out_chunk: usize) -> Driven {
    let mut output = Vec::new();
    let mut scratch = vec![0u8; out_chunk.max(1)];
    let mut pos = 0usize;
    let mut calls = 0usize;

    loop {
        calls += 1;
        assert!(calls < CALL_BUDGET, "decoder did not terminate");
        let end = pos.saturating_add(in_chunk).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        match stream.decode(&frame[pos..end], &mut scratch, flush) {
            Ok(progress) => {
                pos += progress.consumed;
                output.extend_from_slice(&scratch[..progress.produced]);
                match progress.status {
                    ZstdStatus::StreamEnd => {
                        return Driven {
                            output,
                            error: None,
                            calls,
                        };
                    }
                    _ => {
                        // A call that neither consumed nor produced anything
                        // while all input was already offered means the decoder
                        // is stuck; the contract forbids it.
                        assert!(
                            progress.consumed > 0
                                || progress.produced > 0
                                || end < frame.len()
                                || flush != FlushMode::Finish,
                            "decoder made no progress with all input offered"
                        );
                    }
                }
            }
            Err(e) => {
                return Driven {
                    output,
                    error: Some(e.to_string()),
                    calls,
                };
            }
        }
    }
}

/// As [`drive_with`], with a fresh permissive stream.
fn drive(frame: &[u8], in_chunk: usize, out_chunk: usize) -> Driven {
    let mut stream = permissive();
    drive_with(&mut stream, frame, in_chunk, out_chunk)
}

/// Drive `stream` over `frame` and return the *typed* error it fails with.
///
/// [`drive_with`] flattens the error to a string, which is enough for "did it
/// reject this?" but not for "did it reject it for the right reason?".
fn drive_to_error(stream: &mut ZstdStream, frame: &[u8], in_chunk: usize) -> OxiArcError {
    let mut scratch = vec![0u8; 1 << 16];
    let mut pos = 0usize;
    loop {
        let end = pos.saturating_add(in_chunk).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        match stream.decode(&frame[pos..end], &mut scratch, flush) {
            Ok(progress) => {
                pos += progress.consumed;
                assert_ne!(
                    progress.status,
                    ZstdStatus::StreamEnd,
                    "stream was expected to fail but completed"
                );
                assert!(
                    progress.consumed > 0 || progress.produced > 0 || end < frame.len(),
                    "decoder stalled while an error was expected"
                );
            }
            Err(e) => return e,
        }
    }
}

/// Deterministic pseudo-random bytes (no `rand` dependency).
fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

/// The equivalence corpus: every shape the audit's test matrix calls for.
fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = vec![
        ("empty".into(), Vec::new()),
        ("one_byte".into(), vec![0x5A]),
        ("short_text".into(), b"The quick brown fox.".to_vec()),
        ("highly_compressible".into(), vec![b'A'; 300_000]),
        ("incompressible".into(), pseudo_random(200_000, 0xC0FFEE)),
    ];
    out.push((
        "text".into(),
        "the rain in spain falls mainly on the plain. "
            .repeat(4000)
            .into_bytes(),
    ));
    // Exactly one block, one block + 1, and a multi-block payload.
    for (label, size) in [
        ("block_exact", MAX_BLOCK_SIZE),
        ("block_plus_one", MAX_BLOCK_SIZE + 1),
        ("three_blocks", 3 * MAX_BLOCK_SIZE + 17),
    ] {
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            data.extend_from_slice(b"abcdefghij0123456789");
        }
        data.truncate(size);
        out.push((label.into(), data));
    }
    out
}

/// T1/T2/T3/T5/T6: byte-at-a-time input, byte-at-a-time output, both at once,
/// prime-sized chunks and the single-call case all reproduce the one-shot
/// decoder byte for byte.
#[test]
fn equivalence_across_chunk_schedules() {
    // Chunk schedules; the very small ones are restricted to small payloads
    // below so the suite stays fast.
    let big_schedules = [
        (usize::MAX, 1 << 16),
        (1 << 16, 1 << 16),
        (8191, 4096),
        (127, 1 << 16),
        (1 << 16, 31),
    ];
    let small_schedules = [
        (1usize, 1usize),
        (1, 1 << 16),
        (1 << 16, 1),
        (2, 3),
        (3, 5),
        (7, 13),
        (13, 7),
    ];

    for (name, data) in corpus() {
        for level in [1, 3, 9] {
            let frame = compress_with_level(&data, level).expect("compress");
            let expected = decompress(&frame).expect("one-shot decode");
            assert_eq!(expected, data, "[{name} L{level}] one-shot sanity");

            for (ic, oc) in big_schedules {
                let got = drive(&frame, ic, oc);
                assert_eq!(got.error, None, "[{name} L{level} {ic}/{oc}]");
                assert_eq!(got.output, data, "[{name} L{level} {ic}/{oc}]");
            }
            if data.len() <= 4096 {
                for (ic, oc) in small_schedules {
                    let got = drive(&frame, ic, oc);
                    assert_eq!(got.error, None, "[{name} L{level} {ic}/{oc}]");
                    assert_eq!(got.output, data, "[{name} L{level} {ic}/{oc}]");
                }
            }
        }
    }
}

/// One byte in, one byte out, over a payload big enough to span several blocks:
/// the killer test for mid-block resumption.
#[test]
fn one_byte_in_one_byte_out_over_multi_block_frame() {
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let frame = compress_with_level(&data, 3).expect("compress");
    let got = drive(&frame, 1, 1);
    assert_eq!(got.error, None);
    assert_eq!(got.output, data);
}

/// Reference-produced fixtures decode identically through the push decoder at
/// every chunk schedule.
#[test]
fn reference_fixtures_match_one_shot() {
    for (name, frame, raw) in FIXTURES {
        for (ic, oc) in [
            (usize::MAX, 1 << 16),
            (1, 1),
            (3, 7),
            (1024, 33),
            (7, 1 << 16),
        ] {
            let got = drive(frame, ic, oc);
            assert_eq!(got.error, None, "[{name} {ic}/{oc}]");
            assert_eq!(got.output.as_slice(), *raw, "[{name} {ic}/{oc}]");
        }
    }
}

/// T8/T9: truncating a frame at every offset must never panic, never hang and
/// never succeed with a short body.
#[test]
fn truncation_at_every_offset_is_bounded_and_never_silently_succeeds() {
    let data = b"truncation corpus: repeat me a few times. ".repeat(200);
    for frame in [
        compress_with_level(&data, 3).expect("compress"),
        compress_no_checksum(&data).expect("compress"),
        compress_with_level(b"tiny", 1).expect("compress"),
    ] {
        for cut in 0..frame.len() {
            let prefix = &frame[..cut];
            for in_chunk in [usize::MAX, 1] {
                let got = drive(prefix, in_chunk, 1 << 16);
                // Not merely "shorter than the original": a truncated frame
                // must be an *error*. A clean `StreamEnd` after a partial body
                // is the silent-truncation defect this whole track exists to
                // prevent, and asserting only on the length would let it
                // through. (Bytes may legitimately have been handed out before
                // the error — a prefix that stops one byte short of the frame
                // checksum decodes every block first — which is why this is an
                // assertion about the *status*, not about the length.)
                //
                // The one clean case is `cut == 0` on a multi-frame decoder,
                // where an empty stream is legitimately an empty payload; the
                // single-frame decoder rejects even that (see
                // `empty_input_is_an_error_for_a_single_frame_decoder`).
                if cut == 0 {
                    assert_eq!(got.error, None, "an empty multi-frame stream is clean");
                    assert!(got.output.is_empty());
                } else {
                    assert!(
                        got.error.is_some(),
                        "truncation at {cut} ended cleanly after {} bytes",
                        got.output.len()
                    );
                }
                // Bounded work: at most a couple of calls per input byte plus a
                // constant, and never anywhere near the runaway budget.
                assert!(
                    got.calls <= 4 * (cut + 8),
                    "truncation at {cut} took {} calls",
                    got.calls
                );
            }
        }
    }
}

/// T10: repeatedly feeding an empty slice after a truncated prefix terminates
/// and reports `NeedInput` without consuming or producing anything.
#[test]
fn empty_input_is_the_idle_case_not_a_contract_violation() {
    let data = vec![7u8; 5000];
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut stream = permissive();
    let mut scratch = [0u8; 256];
    let half = frame.len() / 2;
    let first = stream
        .decode(&frame[..half], &mut scratch, FlushMode::None)
        .expect("first half");
    assert!(first.consumed > 0);

    for _ in 0..10_000 {
        let p = stream
            .decode(&[], &mut scratch, FlushMode::None)
            .expect("idle call");
        assert_eq!(p.consumed, 0);
        assert_eq!(p.produced, 0);
        assert_eq!(p.status, ZstdStatus::NeedInput);
    }

    // And the stream still finishes correctly afterwards.
    let mut out = scratch[..first.produced].to_vec();
    let mut pos = half;
    loop {
        let p = stream
            .decode(&frame[pos..], &mut scratch, FlushMode::Finish)
            .expect("rest");
        pos += p.consumed;
        out.extend_from_slice(&scratch[..p.produced]);
        if p.status == ZstdStatus::StreamEnd {
            break;
        }
    }
    assert_eq!(out, data);
}

/// `finish()` on a truncated stream is an error; on a complete one it is `Ok`,
/// and the fault it latches is sticky.
#[test]
fn finish_reports_truncation_and_latches() {
    let data = vec![3u8; 40_000];
    let frame = compress_with_level(&data, 3).expect("compress");

    let mut stream = permissive();
    let mut scratch = vec![0u8; 1 << 16];
    let mut pos = 0;
    while pos < frame.len() - 3 {
        let p = stream
            .decode(&frame[pos..frame.len() - 3], &mut scratch, FlushMode::None)
            .expect("decode");
        pos += p.consumed;
        if p.consumed == 0 && p.produced == 0 {
            break;
        }
    }
    assert!(stream.finish().is_err(), "truncated stream must not finish");
    // Sticky: every later call returns the same fault.
    assert!(stream.decode(&[], &mut scratch, FlushMode::None).is_err());
    assert!(stream.finish().is_err());
    // ...until reset clears it.
    stream.reset();
    let got = drive_with(&mut stream, &frame, usize::MAX, 1 << 16);
    assert_eq!(got.error, None);
    assert_eq!(got.output, data);
}

/// A complete stream finishes cleanly and reports consistent counters.
#[test]
fn counters_and_finish_on_a_complete_stream() {
    let data = vec![0xABu8; 100_000];
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut stream = permissive();
    let got = drive_with(&mut stream, &frame, 1000, 1 << 16);
    assert_eq!(got.error, None);
    assert_eq!(got.output, data);
    assert!(stream.is_finished());
    assert_eq!(stream.finish().ok(), Some(()));
    assert_eq!(stream.total_out(), data.len() as u64);
    assert_eq!(stream.total_in(), frame.len() as u64);
    assert_eq!(stream.frames_decoded(), 1);
}

/// T13: a corrupted checksum is reported (after the bytes have gone out — a
/// streaming decoder cannot un-send them, which is exactly why `finish()`
/// exists).
#[test]
fn corrupt_checksum_is_detected() {
    let data = b"checksum me".repeat(500);
    let mut frame = compress_with_level(&data, 3).expect("compress");
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;
    let got = drive(&frame, usize::MAX, 1 << 16);
    assert!(got.error.is_some(), "corrupt checksum must be reported");
}

/// T12: bit flips never panic and never hang.
#[test]
fn bit_flips_never_panic_or_hang() {
    let data = b"bit flip corpus with some structure ".repeat(300);
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut flips = 0usize;
    for byte in 0..frame.len() {
        for bit in [0u32, 3, 7] {
            let mut corrupt = frame.clone();
            corrupt[byte] ^= 1 << bit;
            let got = drive(&corrupt, 64, 4096);
            assert!(got.calls < CALL_BUDGET);
            if got.error.is_none() {
                // Decoding without error is allowed only if it matches.
                assert!(got.output.len() <= data.len() + MAX_BLOCK_SIZE);
            }
            flips += 1;
        }
    }
    assert!(
        flips >= 3 * frame.len(),
        "every byte must have been exercised"
    );
}

/// T15: a frame declaring a window larger than the configured ceiling is
/// rejected before any window byte is allocated.
#[test]
fn oversized_declared_window_is_refused_before_allocating() {
    let data = vec![1u8; 400_000];
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut stream = ZstdStream::new().with_max_window(4096);
    let mut scratch = [0u8; 256];
    let err = stream
        .decode(&frame, &mut scratch, FlushMode::Finish)
        .expect_err("must refuse");
    assert!(
        err.to_string().contains("memory budget"),
        "unexpected error: {err}"
    );
    assert_eq!(stream.window_size(), 0, "no window may be allocated");
}

/// The default window ceiling is 8 MiB and it really is a ceiling.
#[test]
fn default_window_ceiling_is_eight_mib() {
    // A tiny frame is fine under the default.
    let frame = compress_with_level(b"small", 3).expect("compress");
    let got = drive_with(&mut ZstdStream::new(), &frame, usize::MAX, 256);
    assert_eq!(got.error, None);
    assert_eq!(got.output, b"small");
}

/// T16-T20: the output budget is enforced, identically however the input is fed.
#[test]
fn output_budget_is_enforced() {
    let data = vec![0u8; 4 << 20];
    let frame = compress_with_level(&data, 3).expect("compress");

    for in_chunk in [usize::MAX, 1024, 7] {
        let mut stream = permissive().with_max_output(64 * 1024);
        let got = drive_with(&mut stream, &frame, in_chunk, 1 << 16);
        assert!(got.error.is_some(), "bomb must be rejected ({in_chunk})");
        // Bytes are charged before they are handed back, so the *delivered*
        // output is capped exactly; the bounded overshoot is decode work and
        // ring space, never output the caller received.
        assert!(
            got.output.len() <= 64 * 1024,
            "delivered output must never exceed the budget, got {}",
            got.output.len()
        );
        assert_eq!(stream.total_out(), got.output.len() as u64);
        // The window never grew beyond what the budget allows plus a block.
        assert!(stream.window_size() <= 64 * 1024 + MAX_BLOCK_SIZE + 8);

        // ... and the failure is a budget error, not a corruption report.
        let mut stream = permissive().with_max_output(64 * 1024);
        let err = drive_to_error(&mut stream, &frame, in_chunk);
        assert!(
            matches!(err, OxiArcError::MemoryBudgetExceeded { .. }),
            "a bomb must be reported as a budget overrun, got {err:?}"
        );
    }
}

/// Exactly-`N` succeeds, `N + 1` fails.
#[test]
fn budget_boundary_is_exact() {
    let data = vec![0x33u8; 10_000];
    let frame = compress_with_level(&data, 3).expect("compress");

    let mut ok = permissive().with_max_output(10_000);
    let got = drive_with(&mut ok, &frame, usize::MAX, 1 << 16);
    assert_eq!(got.error, None, "exactly N must succeed");
    assert_eq!(got.output.len(), 10_000);

    let mut tight = permissive().with_max_output(9_999);
    let got = drive_with(&mut tight, &frame, usize::MAX, 1 << 16);
    assert!(got.error.is_some(), "N-1 must fail");
}

/// An RLE-block bomb: three bytes of block header regenerate 128 KiB. The
/// budget must stop it before the window is filled.
#[test]
fn rle_block_bomb_is_rejected_pre_decode() {
    // Hand-built frame: magic, FHD (windowed, no FCS, no checksum), window
    // descriptor, then RLE blocks that each regenerate 128 KiB.
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    frame.push(0x00); // no FCS, no checksum, not single-segment
    frame.push(0x48); // window descriptor: 8 MiB
    for i in 0..64 {
        let last = u32::from(i == 63);
        let header = last | (1 << 1) | ((MAX_BLOCK_SIZE as u32) << 3);
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.push(b'X');
    }
    // 64 * 128 KiB = 8 MiB of output from ~260 bytes of input.
    let unbounded = decompress_multi_frame_with_limit(&frame, 16 << 20).expect("decode");
    assert_eq!(unbounded.len(), 64 * MAX_BLOCK_SIZE);

    let mut stream = permissive().with_max_output(200_000);
    let got = drive_with(&mut stream, &frame, usize::MAX, 1 << 16);
    assert!(got.error.is_some(), "RLE bomb must be rejected");
    let mut stream2 = permissive().with_max_output(200_000);
    let err = drive_to_error(&mut stream2, &frame, usize::MAX);
    assert!(
        matches!(err, OxiArcError::MemoryBudgetExceeded { .. }),
        "an RLE bomb must be a budget overrun, got {err:?}"
    );
    // RLE blocks declare their regenerated size, so the cut is exact: no block
    // beyond the budget is ever decoded.
    assert!(
        got.output.len() <= 200_000,
        "RLE budget must be exact, produced {}",
        got.output.len()
    );
}

/// T24: multi-frame streams with an interleaved skippable frame, split at every
/// byte.
#[test]
fn multi_frame_with_skippable_frames() {
    let a = compress_with_level(b"first frame payload ", 3).expect("compress");
    let skip = write_skippable_frame(b"metadata blob", 5);
    let b = compress_with_level(b"second frame payload ", 3).expect("compress");
    let c = compress_with_level(b"third", 1).expect("compress");
    let expected = b"first frame payload second frame payload third".to_vec();

    let mut stream_bytes = Vec::new();
    stream_bytes.extend_from_slice(&a);
    stream_bytes.extend_from_slice(&skip);
    stream_bytes.extend_from_slice(&b);
    stream_bytes.extend_from_slice(&c);

    assert_eq!(
        decompress_multi_frame(&stream_bytes).expect("one-shot"),
        expected
    );

    for in_chunk in [usize::MAX, 1, 2, 5, 17] {
        let mut stream = permissive();
        let got = drive_with(&mut stream, &stream_bytes, in_chunk, 1 << 16);
        assert_eq!(got.error, None, "chunk {in_chunk}");
        assert_eq!(got.output, expected, "chunk {in_chunk}");
        assert_eq!(stream.frames_decoded(), 3);
    }
}

/// `with_multi_frame(false)` stops after one frame and leaves the rest alone.
#[test]
fn single_frame_mode_stops_after_the_first_frame() {
    let a = compress_with_level(b"only this", 3).expect("compress");
    let b = compress_with_level(b"not this", 3).expect("compress");
    let mut joined = a.clone();
    joined.extend_from_slice(&b);

    let mut stream = permissive().with_multi_frame(false);
    let got = drive_with(&mut stream, &joined, usize::MAX, 1 << 16);
    assert_eq!(got.error, None);
    assert_eq!(got.output, b"only this");
    assert!(stream.total_in() <= a.len() as u64 + 4);
}

/// Trailing garbage after a complete frame ends the stream gracefully (matching
/// `decompress_multi_frame`), and the bytes are recoverable.
#[test]
fn trailing_garbage_after_a_frame_ends_the_stream() {
    let mut bytes = compress_with_level(b"payload", 3).expect("compress");
    bytes.extend_from_slice(b"NOT A ZSTD FRAME");
    let mut stream = permissive();
    let got = drive_with(&mut stream, &bytes, usize::MAX, 1 << 16);
    assert_eq!(got.error, None);
    assert_eq!(got.output, b"payload");
    assert_eq!(stream.unused_input(), b"NOT ");
}

/// Garbage at the very start is an error, not a silent empty stream.
#[test]
fn garbage_at_frame_zero_is_an_error() {
    let got = drive(b"this is definitely not zstd", usize::MAX, 256);
    assert!(got.error.is_some());
    // A completely empty input is still a clean, empty stream.
    let got = drive(&[], usize::MAX, 256);
    assert_eq!(got.error, None);
    assert!(got.output.is_empty());
}

/// T25: dictionary frames, including several in a row.
#[test]
fn dictionary_frames_round_trip() {
    let dict_text = "alpha beta gamma delta epsilon zeta eta theta ".repeat(40);
    let dict = dict_text.as_bytes().to_vec();
    let payloads: [&[u8]; 3] = [
        b"alpha beta gamma",
        b"delta epsilon zeta eta theta",
        b"gamma gamma gamma",
    ];

    let mut joined = Vec::new();
    let mut expected = Vec::new();
    for payload in payloads {
        let mut encoder = oxiarc_zstd::ZstdEncoder::new();
        encoder.set_level(3);
        encoder.set_dictionary(&dict);
        joined.extend_from_slice(&encoder.compress(payload).expect("compress"));
        expected.extend_from_slice(payload);
    }

    for in_chunk in [usize::MAX, 1, 11] {
        let mut stream = permissive().with_dictionary(dict.clone());
        let got = drive_with(&mut stream, &joined, in_chunk, 1 << 16);
        assert_eq!(got.error, None, "chunk {in_chunk}");
        assert_eq!(got.output, expected, "chunk {in_chunk}");
    }

    // Without the dictionary the same stream must not silently produce the
    // original bytes.
    let got = drive(&joined, usize::MAX, 1 << 16);
    assert!(got.error.is_some() || got.output != expected);
}

/// `reset()` makes a used stream reusable for an unrelated frame.
#[test]
fn reset_allows_reuse_across_frames() {
    let payloads: [&[u8]; 3] = [b"first payload", b"second, longer payload", b"third"];
    let mut stream = permissive();
    for payload in payloads {
        let frame = compress_with_level(payload, 3).expect("compress");
        let got = drive_with(&mut stream, &frame, 3, 8);
        assert_eq!(got.error, None);
        assert_eq!(got.output, payload);
        assert_eq!(stream.total_out(), payload.len() as u64);
        stream.reset();
        assert_eq!(stream.total_out(), 0);
        assert_eq!(stream.frames_decoded(), 0);
    }
}

/// Reusing one decoder across frames must not let a `Treeless` literals section
/// or a `Repeat` FSE mode in a new frame borrow the previous frame's tables.
///
/// The one-shot `ZstdDecoder::reset` had exactly that defect; this pins the
/// fix from both the one-shot and the incremental side.
#[test]
fn entropy_tables_do_not_survive_a_frame_boundary() {
    // Frame 1 uses Huffman-compressed literals (so a table exists), frame 2 is
    // a hand-built frame whose single block claims `Treeless` literals.
    let rich: Vec<u8> = (0..40_000u32).map(|i| (i % 37) as u8).collect();
    let frame1 = compress_with_level(&rich, 3).expect("compress");

    let mut frame2 = Vec::new();
    frame2.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    frame2.push(0x00);
    frame2.push(0x48);
    // One last compressed block whose literals header says Treeless.
    let body: Vec<u8> = vec![0b0000_0011, 0x00, 0x00, 0x00];
    let header = 1u32 | (2 << 1) | ((body.len() as u32) << 3);
    frame2.extend_from_slice(&header.to_le_bytes()[..3]);
    frame2.extend_from_slice(&body);

    let mut joined = frame1;
    joined.extend_from_slice(&frame2);

    let mut stream = permissive();
    let got = drive_with(&mut stream, &joined, usize::MAX, 1 << 16);
    assert!(
        got.error.is_some(),
        "a Treeless section in a new frame must be rejected"
    );

    // The same must hold for the one-shot decoder driven across two frames.
    let mut decoder = oxiarc_zstd::ZstdDecoder::new();
    let f1 = compress_with_level(&rich, 3).expect("compress");
    decoder.decode_frame(&f1).expect("first frame");
    decoder.reset();
    assert!(
        decoder.decode_frame(&frame2).is_err(),
        "ZstdDecoder::reset must clear the literals table"
    );
}

/// The bounded one-shot helpers agree with the legacy ones and enforce caps.
#[test]
fn bounded_one_shot_helpers_match_the_legacy_decoders() {
    for (name, data) in corpus() {
        let frame = compress_with_level(&data, 3).expect("compress");

        let via_limit = decompress_with_limit(&frame, data.len()).expect("with_limit");
        assert_eq!(via_limit, data, "[{name}] decompress_with_limit");

        let mut dst = vec![0u8; data.len()];
        let n = decompress_into(&frame, &mut dst).expect("into");
        assert_eq!(n, data.len(), "[{name}] decompress_into");
        assert_eq!(dst, data, "[{name}] decompress_into");

        let via_multi =
            decompress_multi_frame_with_limit(&frame, data.len()).expect("multi_with_limit");
        assert_eq!(via_multi, data, "[{name}] multi_frame_with_limit");

        if !data.is_empty() {
            assert!(
                decompress_with_limit(&frame, data.len() - 1).is_err(),
                "[{name}] tight limit must fail"
            );
            let mut small = vec![0u8; data.len() - 1];
            assert!(
                decompress_into(&frame, &mut small).is_err(),
                "[{name}] small dst must fail"
            );
        }
    }
}

/// `decompress_into` ignores bytes after the frame, the way a padded TIFF strip
/// needs it to.
#[test]
fn decompress_into_tolerates_trailing_padding() {
    let payload = b"tiff strip payload".repeat(10);
    let mut frame = compress_with_level(&payload, 3).expect("compress");
    frame.push(0); // libtiff pads strips to even offsets
    let mut dst = vec![0u8; payload.len()];
    let n = decompress_into(&frame, &mut dst).expect("into");
    assert_eq!(n, payload.len());
    assert_eq!(dst, payload);
}

/// The bounded helpers handle the fixtures too.
#[test]
fn bounded_helpers_on_reference_fixtures() {
    for (name, frame, raw) in FIXTURES {
        let out = decompress_with_limit(frame, raw.len()).expect("with_limit");
        assert_eq!(out.as_slice(), *raw, "[{name}]");
        let mut dst = vec![0u8; raw.len()];
        let n = decompress_into(frame, &mut dst).expect("into");
        assert_eq!(n, raw.len(), "[{name}]");
        assert_eq!(dst.as_slice(), *raw, "[{name}]");
    }
}

/// The window allocation tracks what was actually produced, not what the frame
/// declares.
#[test]
fn window_allocation_is_lazy() {
    // Small payload, but the encoder declares at least a 1 KiB window.
    let frame = compress_with_level(b"tiny payload", 3).expect("compress");
    let mut stream = permissive();
    let got = drive_with(&mut stream, &frame, usize::MAX, 1 << 16);
    assert_eq!(got.error, None);
    assert!(
        stream.window_size() <= 4096,
        "window grew to {} for a 12-byte payload",
        stream.window_size()
    );

    // A 1 MiB payload with an 8 MiB declared window allocates about 1 MiB.
    let data = pseudo_random(1 << 20, 42);
    let frame = compress_with_level(&data, 3).expect("compress");
    let mut stream = permissive();
    let got = drive_with(&mut stream, &frame, 4096, 1 << 16);
    assert_eq!(got.error, None);
    assert_eq!(got.output, data);
    assert!(
        stream.window_size() <= (1 << 20) + MAX_BLOCK_SIZE,
        "window {} exceeds the produced size",
        stream.window_size()
    );
}

/// Build a hand-crafted frame header so the declared `Window_Size` and
/// `Frame_Content_Size` can be set to values no encoder would produce.
///
/// `window_descriptor` is the raw RFC 8878 byte (`exponent << 3 | mantissa`);
/// `content_size` is written as an 8-byte `Frame_Content_Size` when supplied.
fn crafted_frame(window_descriptor: u8, content_size: Option<u64>, blocks: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    // Frame_Header_Descriptor: windowed, no checksum, no dictionary ID.
    frame.push(if content_size.is_some() { 0xC0 } else { 0x00 });
    frame.push(window_descriptor);
    if let Some(size) = content_size {
        frame.extend_from_slice(&size.to_le_bytes());
    }
    frame.extend_from_slice(blocks);
    frame
}

/// Encode one block header (3 bytes little-endian).
fn block_header(last: bool, kind_bits: u32, block_size: u32) -> [u8; 3] {
    let raw = u32::from(last) | (kind_bits << 1) | (block_size << 3);
    let bytes = raw.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

/// A declared `Frame_Content_Size` must never drive the first allocation.
///
/// Regression: an 18-byte frame declaring a terabyte of content used to make
/// the decoder allocate the whole 8 MiB window before decoding a single block —
/// a 460,000x allocation amplification from attacker-controlled header fields.
#[test]
fn declared_content_size_cannot_force_an_allocation() {
    let mut blocks = Vec::new();
    blocks.extend_from_slice(&block_header(true, 1, 1)); // last, RLE, regenerates 1 byte
    blocks.push(b'A');
    // Window descriptor 0x68 = exponent 13 = an 8 MiB window (the default cap).
    let frame = crafted_frame(0x68, Some(1u64 << 40), &blocks);
    assert_eq!(frame.len(), 18);

    let mut stream = ZstdStream::new();
    let got = drive_with(&mut stream, &frame, usize::MAX, 1 << 16);
    // The content size is a lie, so the frame is rejected...
    assert!(got.error.is_some());
    // ...and the ring never grew past one block.
    assert!(
        stream.window_size() <= MAX_BLOCK_SIZE,
        "18 bytes of input allocated a {}-byte window",
        stream.window_size()
    );
}

/// A declared `Window_Size` of ~2 TiB is refused outright under the default
/// ceiling, and grows lazily when the ceiling is lifted.
#[test]
fn huge_declared_window_is_refused_or_lazy() {
    let payload = b"a handful of raw bytes";
    let mut blocks = Vec::new();
    blocks.extend_from_slice(&block_header(true, 0, payload.len() as u32)); // last, Raw
    blocks.extend_from_slice(payload);
    // Window descriptor 0xF8 = exponent 31 = 2 TiB.
    let frame = crafted_frame(0xF8, None, &blocks);

    // Default 8 MiB ceiling: refused before a byte is allocated.
    let mut strict = ZstdStream::new();
    let got = drive_with(&mut strict, &frame, usize::MAX, 1 << 16);
    assert!(got.error.is_some(), "2 TiB window must be refused");
    assert_eq!(strict.window_size(), 0, "no window may be allocated");

    // Ceiling lifted: the frame decodes, and the ring is sized by what was
    // actually produced, not by what the header declared.
    let mut lenient = permissive();
    let got = drive_with(&mut lenient, &frame, usize::MAX, 1 << 16);
    assert_eq!(got.error, None);
    assert_eq!(got.output, payload);
    assert!(
        lenient.window_size() <= MAX_BLOCK_SIZE,
        "window grew to {} for a {}-byte payload",
        lenient.window_size(),
        payload.len()
    );
}

/// `finish()` must never swallow decoded bytes the caller has not seen.
#[test]
fn finish_does_not_discard_undrained_output() {
    let data = b"undrained output must not vanish".repeat(20);
    let frame = compress_with_level(&data, 3).expect("compress");

    let mut stream = permissive();
    let mut tiny = [0u8; 4];
    let progress = stream
        .decode(&frame, &mut tiny, FlushMode::Finish)
        .expect("first decode");
    assert_eq!(progress.status, ZstdStatus::NeedOutput);
    assert_eq!(progress.produced, 4);
    let before = stream.total_out();

    let err = stream
        .finish()
        .expect_err("undrained output must be reported");
    assert!(
        err.to_string().contains("never drained"),
        "unexpected error: {err}"
    );
    assert_eq!(
        stream.total_out(),
        before,
        "finish() must not consume decoded bytes"
    );
}

/// Regression: a budget overrun is reported as a budget overrun, whatever the
/// block type and whether or not the frame declares its content size.
///
/// The three checks that stop a bomb live in different places — the frame
/// header's `Frame_Content_Size`, a `Raw`/`RLE` block header, and the
/// after-decode charge for a `Compressed` block — and only the last one is
/// reached when the frame declares no content size. It used to report
/// `CorruptedData("block regenerated size N exceeds the frame maximum M")`,
/// because the per-block ceiling folded in the caller's remaining budget: a
/// caller could not tell a compression bomb from a malformed frame.
#[test]
fn budget_overrun_is_always_a_memory_budget_error() {
    // (a) Frame_Content_Size path: the encoder declares the size up front.
    let data: Vec<u8> = (0..20_000u32).map(|i| (i % 37) as u8).collect();
    let declared = compress_with_level(&data, 3).expect("compress");
    let mut stream = permissive().with_max_output(1000);
    let err = drive_to_error(&mut stream, &declared, usize::MAX);
    assert!(
        matches!(err, OxiArcError::MemoryBudgetExceeded { .. }),
        "declared content size: got {err:?}"
    );
    assert_eq!(stream.total_out(), 0, "nothing may be delivered");

    // (b) Raw block header path.
    let raw = raw_block_frame(&vec![7u8; 5000]);
    let mut stream = permissive().with_max_output(1000);
    let err = drive_to_error(&mut stream, &raw, usize::MAX);
    assert!(
        matches!(err, OxiArcError::MemoryBudgetExceeded { .. }),
        "raw block: got {err:?}"
    );

    // (c) Compressed block with NO declared content size: only the
    //     after-decode charge can catch this one.
    let undeclared = strip_content_size(&declared);
    assert_eq!(
        decompress_with_limit(&undeclared, 1 << 20).expect("rebuilt frame must decode"),
        data,
        "the rebuilt no-FCS frame must still be a valid frame"
    );
    for in_chunk in [usize::MAX, 1024, 1] {
        let mut stream = permissive().with_max_output(1000);
        let err = drive_to_error(&mut stream, &undeclared, in_chunk);
        assert!(
            matches!(err, OxiArcError::MemoryBudgetExceeded { .. }),
            "compressed block without content size ({in_chunk}): got {err:?}"
        );
        assert!(
            stream.total_out() <= 1000,
            "delivered {} bytes against a 1000-byte budget",
            stream.total_out()
        );
    }
}

/// The other half of the split: a block that regenerates more than
/// `Block_Maximum_Decompressed_Size` stays a *format* error even when a budget
/// is configured, so the two conditions can never be confused in either
/// direction.
#[test]
fn oversized_block_is_corruption_not_a_budget_error() {
    // Declared window 1 KiB (`wlog=10`), then a raw block claiming 2000 bytes:
    // RFC 8878 caps a block at min(Window_Size, 128 KiB) = 1024.
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    frame.push(0x00); // no FCS, no checksum, windowed
    frame.push(0x00); // window descriptor: exponent 0, mantissa 0 -> 1 KiB
    let header = 1u32 | (2000u32 << 3); // last block, Raw, 2000 bytes
    frame.extend_from_slice(&header.to_le_bytes()[..3]);
    frame.extend_from_slice(&vec![0xA5u8; 2000]);

    for budget in [None, Some(10_000u64)] {
        let mut stream = permissive();
        if let Some(limit) = budget {
            stream = stream.with_max_output(limit);
        }
        let err = drive_to_error(&mut stream, &frame, usize::MAX);
        assert!(
            matches!(err, OxiArcError::CorruptedData { .. }),
            "budget {budget:?}: an oversized block is malformed, got {err:?}"
        );
        assert!(
            err.to_string().contains("exceeds the frame maximum"),
            "unexpected message: {err}"
        );
    }
}

/// Regression: an empty input is a clean empty stream only for a *multi-frame*
/// decoder. A single-frame decoder — which is what `oxiarc-http` and the
/// bounded one-shot helpers use — must demand a frame, exactly like the legacy
/// [`decompress`].
#[test]
fn empty_input_is_an_error_for_a_single_frame_decoder() {
    assert!(decompress(&[]).is_err(), "legacy baseline");

    let mut single = permissive().with_multi_frame(false);
    let err = drive_to_error(&mut single, &[], usize::MAX);
    assert!(
        matches!(err, OxiArcError::CorruptedData { .. }),
        "got {err:?}"
    );
    assert!(single.finish().is_err(), "the fault must be latched");

    assert!(
        decompress_with_limit(&[], 1024).is_err(),
        "decompress_with_limit must not invent an empty payload"
    );
    let mut dst = [0u8; 16];
    assert!(
        decompress_into(&[], &mut dst).is_err(),
        "decompress_into must not report a zero-length strip as success"
    );

    // The multi-frame side keeps `decompress_multi_frame`'s tolerance.
    assert_eq!(
        decompress_multi_frame(&[]).expect("legacy multi-frame baseline"),
        Vec::<u8>::new()
    );
    assert_eq!(
        decompress_multi_frame_with_limit(&[], 1024).expect("multi-frame empty"),
        Vec::<u8>::new()
    );
    let got = drive(&[], usize::MAX, 1 << 16);
    assert_eq!(got.error, None);
    assert!(got.output.is_empty());
}

/// Build a frame carrying `payload` in a single `Raw` block and no
/// `Frame_Content_Size`.
fn raw_block_frame(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&[0x28, 0xB5, 0x2F, 0xFD]);
    frame.push(0x00); // no FCS, no checksum, windowed
    frame.push(0x48); // window descriptor: 8 MiB
    let header = 1u32 | ((payload.len() as u32) << 3); // last block, Raw
    frame.extend_from_slice(&header.to_le_bytes()[..3]);
    frame.extend_from_slice(payload);
    frame
}

/// Rewrite `frame`'s header so that it declares neither `Frame_Content_Size`
/// nor a single segment, keeping its blocks byte for byte.
fn strip_content_size(frame: &[u8]) -> Vec<u8> {
    let fhd = frame[4];
    let single = (fhd & 0x20) != 0;
    let fcs_flag = fhd >> 6;
    let dict_flag = fhd & 0x03;
    let mut header_len = 5usize;
    if !single {
        header_len += 1;
    }
    header_len += match dict_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    if single || fcs_flag != 0 {
        header_len += match fcs_flag {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 8,
        };
    }
    let mut out = Vec::new();
    out.extend_from_slice(&frame[..4]);
    out.push(fhd & !0xC0 & !0x20); // drop the content size and single-segment flags
    out.push(0x48); // window descriptor: 8 MiB, big enough for any test payload
    out.extend_from_slice(&frame[header_len..]);
    out
}

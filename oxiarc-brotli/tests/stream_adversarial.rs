//! Adversarial tests for [`oxiarc_brotli::BrotliStream`].
//!
//! The conformance suite proves the push decoder agrees with the one-shot
//! decoder on *valid* streams under any chunking. This file attacks the
//! decoder instead: lengths a stream declares but never delivers, input that
//! saturates the internal carry, corruption that moves more than one bit, and
//! degenerate configurations. Every test bounds its own call count, so a
//! decoder that stops making progress fails here rather than hanging the suite.
//!
//! The properties under test:
//!
//! * a declared length is never trusted — a metadata meta-block claiming a
//!   16 MiB payload or an uncompressed meta-block claiming a 15 MiB body must
//!   be refused when the bytes do not arrive, without allocating for the claim;
//! * a stream larger than the internal carry cannot stall the decoder, even
//!   when it is truncated and the caller has already said `Finish`;
//! * multi-byte corruption reaches the same verdict as the one-shot decoder,
//!   with no panic, no hang and no silently short body;
//! * the bulk literal path (taken only when the buffered input covers a whole
//!   run's worst case) produces the same bytes as the careful per-symbol path.

mod common;

use common::{InputSchedule, decode_incremental, pseudo_random};

use oxiarc_brotli::bit_writer::BitWriter;
use oxiarc_brotli::{
    BrotliError, BrotliStatus, BrotliStream, DEFAULT_MAX_WINDOW, compress, decompress,
};
use oxiarc_core::traits::FlushMode;

/// Build a stream by hand from raw bit fields, terminated by the empty final
/// meta-block (RFC 7932 Section 9).
fn build_stream(build: impl FnOnce(&mut BitWriter) -> Result<(), BrotliError>) -> Vec<u8> {
    let mut writer = BitWriter::new();
    writer.write_bit(false).expect("wbits = 16");
    build(&mut writer).expect("build");
    writer.write_bit(true).expect("islast");
    writer.write_bit(true).expect("islastempty");
    writer.finish()
}

/// Drive the decoder to a verdict with a hard call budget.
///
/// Returns `Ok(bytes)` or the error; a decoder that neither progresses nor
/// terminates trips the budget and fails the test.
fn drive_to_verdict(
    data: &[u8],
    in_chunk: usize,
    out_chunk: usize,
    budget: u64,
) -> Result<Vec<u8>, BrotliError> {
    let mut stream = BrotliStream::new();
    let mut decoded = Vec::new();
    let mut buf = vec![0u8; out_chunk.max(1)];
    let mut pos = 0usize;
    let mut calls = 0u64;
    loop {
        calls += 1;
        assert!(
            calls <= budget,
            "decoder ran {calls} calls without terminating"
        );
        let end = (pos + in_chunk.max(1)).min(data.len());
        let flush = if end == data.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.decode(&data[pos..end], &mut buf, flush)?;
        pos += progress.consumed;
        decoded.extend_from_slice(&buf[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd && pos == data.len() {
            break;
        }
        if progress.consumed == 0 && progress.produced == 0 && end == data.len() {
            // All input offered, nothing happening: only `finish` can resolve
            // this, and for a truncated stream it must be an error.
            stream.finish()?;
            break;
        }
    }
    stream.finish()?;
    Ok(decoded)
}

// ─── Declared lengths that never arrive ─────────────────────────────────────

/// A metadata meta-block may declare an `MSKIPLEN` of up to 2^24. The decoder
/// must not reserve anything for that claim, and a payload that never arrives
/// must be an error rather than a silently accepted stream.
#[test]
fn metadata_declaring_a_huge_skip_is_rejected_without_buffering() {
    // MSKIPLEN = 0x1000000 (16 MiB) declared, 64 bytes delivered.
    let delivered = pseudo_random(64, 0xA11CE);
    let stream = build_stream(|w| {
        w.write_bit(false)?; // ISLAST = 0
        w.write_bits(3, 2)?; // MNIBBLES code 3 => metadata
        w.write_bit(false)?; // reserved
        w.write_bits(3, 2)?; // MSKIPBYTES = 3
        for i in 0..3u32 {
            w.write_bits((0x00FF_FFFFu32 >> (i * 8)) & 0xFF, 8)?;
        }
        w.flush();
        w.write_bytes(&delivered)
    });

    assert!(
        decompress(&stream).is_err(),
        "one-shot accepted a metadata block whose payload never arrives"
    );
    for &in_chunk in &[1usize, 7, 4096] {
        let err = drive_to_verdict(&stream, in_chunk, 64, 100_000)
            .expect_err("incremental accepted an undelivered metadata payload");
        assert!(
            matches!(err, BrotliError::UnexpectedEof),
            "in={in_chunk}: unexpected error {err}"
        );
    }
}

/// An uncompressed meta-block declares `MLEN` up to 2^24 bytes. Those bytes are
/// streamed straight to the caller, so a declaration the sender never honours
/// must surface as truncation — never as a short body reported as success.
#[test]
fn uncompressed_meta_block_declaring_a_huge_length_is_rejected() {
    let delivered = pseudo_random(300, 0xBEE5);
    let stream = build_stream(|w| {
        w.write_bit(false)?; // ISLAST = 0
        w.write_bits(2, 2)?; // MNIBBLES code 2 => 6 nibbles
        w.write_bits(0x00F0_0000, 24)?; // MLEN - 1, top nibble non-zero
        w.write_bit(true)?; // ISUNCOMPRESSED = 1
        w.flush();
        w.write_bytes(&delivered)
    });

    assert!(
        decompress(&stream).is_err(),
        "one-shot accepted an undelivered uncompressed body"
    );
    for &(in_chunk, out_chunk) in &[(1usize, 1usize), (13, 64), (4096, 4096)] {
        let err = drive_to_verdict(&stream, in_chunk, out_chunk, 200_000)
            .expect_err("incremental accepted an undelivered uncompressed body");
        assert!(
            matches!(err, BrotliError::UnexpectedEof),
            "in={in_chunk} out={out_chunk}: unexpected error {err}"
        );
    }
}

/// The same declaration under a *tight* output cap: `MLEN` is checked against
/// the budget before the body is streamed, so the refusal must be the budget
/// error and must arrive before any of the declared bytes are produced.
#[test]
fn a_huge_declared_uncompressed_length_is_caught_by_the_output_cap_first() {
    let delivered = pseudo_random(300, 0xBEE5);
    let stream = build_stream(|w| {
        w.write_bit(false)?;
        w.write_bits(2, 2)?;
        w.write_bits(0x00F0_0000, 24)?;
        w.write_bit(true)?;
        w.flush();
        w.write_bytes(&delivered)
    });

    let mut decoder = BrotliStream::new().with_max_output(64 * 1024);
    let mut out = vec![0u8; 4096];
    let err = decoder
        .decode(&stream, &mut out, FlushMode::Finish)
        .expect_err("an over-cap declared length must be refused");
    assert!(
        matches!(err, BrotliError::MemoryBudgetExceeded { .. }),
        "unexpected error: {err}"
    );
    assert_eq!(
        decoder.total_out(),
        0,
        "no byte of an over-cap meta-block may be produced"
    );
}

// ─── The bounded carry under hostile input ──────────────────────────────────

/// A truncated stream *larger than the internal carry*, offered whole with
/// `Finish` on every call.
///
/// The carry stops accepting input at its cap, so `decode` reports a short
/// `consumed` and — because more of the caller's slice is still outstanding —
/// keeps answering `NeedInput`. Once every byte has been taken the decoder must
/// switch to the strict reading and report the truncation. A decoder that let
/// the carry saturate without ever reporting would spin here forever; the call
/// budget catches that.
#[test]
fn a_truncated_stream_larger_than_the_carry_terminates() {
    let noise = pseudo_random(2 << 20, 0xF00D_F00D);
    let mut data = Vec::new();
    for (i, chunk) in noise.chunks(16).enumerate() {
        data.extend_from_slice(format!("line {i}: ").as_bytes());
        for byte in chunk {
            data.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        data.push(b'\n');
    }
    let compressed = compress(&data, 5).expect("compress");
    assert!(
        compressed.len() > 2 * 1024 * 1024,
        "fixture must exceed the carry cap: {} bytes",
        compressed.len()
    );

    let truncated = &compressed[..compressed.len() - 1024];
    let mut stream = BrotliStream::new();
    let mut out = vec![0u8; 128 * 1024];
    let mut pos = 0usize;
    let mut calls = 0u32;
    let verdict = loop {
        calls += 1;
        assert!(
            calls < 100_000,
            "truncated oversized stream did not terminate"
        );
        match stream.decode(&truncated[pos..], &mut out, FlushMode::Finish) {
            Ok(progress) => {
                assert!(
                    progress.consumed > 0 || progress.produced > 0,
                    "decoder made no progress with {} bytes outstanding",
                    truncated.len() - pos
                );
                pos += progress.consumed;
                if progress.status == BrotliStatus::StreamEnd {
                    break Ok(());
                }
            }
            Err(e) => break Err(e),
        }
    };
    let err = verdict.expect_err("a truncated stream must not report StreamEnd");
    assert!(
        matches!(err, BrotliError::UnexpectedEof),
        "unexpected error: {err}"
    );
    assert!(stream.finish().is_err(), "the fault must latch");
}

// ─── Corruption beyond a single bit ─────────────────────────────────────────

/// Deterministic multi-byte corruption: splices, overwrites and truncations
/// applied to real streams.
///
/// The single-bit-flip test in the conformance suite proves parity for the
/// smallest possible mutation; this covers mutations large enough to change a
/// meta-block header wholesale, which is where an atomic header parser and a
/// one-shot parser could plausibly diverge.
#[test]
fn multi_byte_corruption_agrees_with_the_one_shot_decoder() {
    let payloads: [Vec<u8>; 3] = [
        b"The quick brown fox jumps over the lazy dog. ".repeat(40),
        vec![0x42u8; 40_000],
        pseudo_random(20_000, 0x5EED_5EED),
    ];
    let mut seed = 0x1234_5678_9ABC_DEF0u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize
    };

    let mut checked = 0usize;
    let mut rejected = 0usize;
    let mut accepted = 0usize;
    for payload in &payloads {
        for quality in [0u32, 5, 11] {
            let clean = compress(payload, quality).expect("compress");
            for case in 0..40 {
                let mut broken = clean.clone();
                match case % 4 {
                    0 => {
                        // Overwrite a run of bytes.
                        let at = next() % broken.len();
                        let len = (next() % 32 + 1).min(broken.len() - at);
                        for byte in &mut broken[at..at + len] {
                            *byte = (next() % 256) as u8;
                        }
                    }
                    1 => {
                        // Splice bytes in.
                        let at = next() % broken.len();
                        let len = next() % 16 + 1;
                        let insert: Vec<u8> = (0..len).map(|_| (next() % 256) as u8).collect();
                        broken.splice(at..at, insert);
                    }
                    2 => {
                        // Delete a run.
                        let at = next() % broken.len();
                        let len = (next() % 16 + 1).min(broken.len() - at);
                        broken.drain(at..at + len);
                    }
                    _ => {
                        // Swap two halves around a random point.
                        let at = next() % broken.len();
                        broken.rotate_left(at);
                    }
                }
                if broken.is_empty() {
                    continue;
                }
                let expected = decompress(&broken);
                for &in_chunk in &[1usize, 37, usize::MAX / 2] {
                    let got = decode_incremental(
                        &broken,
                        InputSchedule::Fixed(in_chunk.min(broken.len())),
                        97,
                    );
                    checked += 1;
                    match (&expected, &got) {
                        (Ok(a), Ok(b)) => {
                            assert_eq!(a, b, "q{quality} case {case}: bytes differ");
                            accepted += 1;
                        }
                        (Err(_), Err(_)) => rejected += 1,
                        (Ok(a), Err(e)) => panic!(
                            "q{quality} case {case} in={in_chunk}: one-shot decoded {} bytes, \
                             incremental failed: {e}",
                            a.len()
                        ),
                        (Err(e), Ok(b)) => panic!(
                            "q{quality} case {case} in={in_chunk}: one-shot failed ({e}), \
                             incremental decoded {} bytes",
                            b.len()
                        ),
                    }
                }
            }
        }
    }
    assert!(checked > 1000, "corruption sweep did not run: {checked}");
    assert!(
        rejected > checked / 10,
        "only {rejected}/{checked} corruptions were rejected — the mutations are \
         not reaching the decoder"
    );
    // Both legs of the differential must actually run: an all-rejected sweep
    // would never compare a single output byte.
    assert!(
        accepted > 0,
        "no corrupted stream still decoded, so the byte-equality leg never ran"
    );
}

// ─── The bulk literal path ──────────────────────────────────────────────────

/// A large output slice fed with mid-stream chunk boundaries.
///
/// The command loop skips its per-symbol rollback checkpoint while the buffered
/// input covers a whole run's worst case, and decodes literals straight into
/// the window's linear region with no end-of-input test in the inner loop. That
/// path is only taken with *both* a large output slice and plenty of buffered
/// input, so the one-byte tests never reach it; if its bit-budget arithmetic
/// were off, a chunk boundary would raise a spurious, latching `UnexpectedEof`.
#[test]
fn the_bulk_literal_path_agrees_with_the_careful_path() {
    // Literal-dense: hex text has few long matches, so most output bytes go
    // through the literal loop rather than through a match copy.
    let noise = pseudo_random(200_000, 0xC0FF_EE00);
    let mut data = Vec::new();
    for (i, chunk) in noise.chunks(24).enumerate() {
        data.extend_from_slice(format!("row {i}: ").as_bytes());
        for byte in chunk {
            data.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        data.push(b'\n');
    }
    let compressed = compress(&data, 5).expect("compress");
    let reference = decompress(&compressed).expect("one-shot");

    for &(in_chunk, out_chunk) in &[
        (131_072usize, 1 << 20usize),
        (65_536, 262_144),
        (300_000, 1 << 20),
        (17, 1 << 20),
    ] {
        let got = decode_incremental(
            &compressed,
            InputSchedule::Fixed(in_chunk.min(compressed.len())),
            out_chunk,
        )
        .unwrap_or_else(|e| panic!("in={in_chunk} out={out_chunk}: {e}"));
        assert_eq!(
            got, reference,
            "in={in_chunk} out={out_chunk}: bytes differ"
        );
    }
}

// ─── Degenerate configurations ──────────────────────────────────────────────

/// A window ceiling below every legal Brotli window refuses each stream with
/// the typed error and no panic; the maximum ceiling accepts them all.
#[test]
fn degenerate_window_ceilings_are_handled() {
    let compressed = compress(b"degenerate ceilings", 5).expect("compress");
    let mut out = vec![0u8; 256];

    for ceiling in [0usize, 1, 1023] {
        let mut stream = BrotliStream::new().with_max_window(ceiling);
        let err = stream
            .decode(&compressed, &mut out, FlushMode::Finish)
            .expect_err("a sub-minimum ceiling must refuse every stream");
        assert!(
            matches!(err, BrotliError::WindowTooLarge { .. }),
            "ceiling {ceiling}: unexpected error {err}"
        );
        assert_eq!(stream.total_out(), 0, "ceiling {ceiling}: produced output");
    }

    for ceiling in [DEFAULT_MAX_WINDOW, usize::MAX] {
        let mut stream = BrotliStream::new().with_max_window(ceiling);
        let progress = stream
            .decode(&compressed, &mut out, FlushMode::Finish)
            .expect("a generous ceiling must accept");
        assert_eq!(progress.status, BrotliStatus::StreamEnd);
        assert_eq!(&out[..progress.produced], b"degenerate ceilings");
        stream.finish().expect("complete stream");
    }
}

/// A zero output cap admits only a stream that produces nothing.
#[test]
fn a_zero_output_cap_admits_only_an_empty_stream() {
    let empty = compress(b"", 5).expect("compress empty");
    let mut out = vec![0u8; 64];
    let mut stream = BrotliStream::new().with_max_output(0);
    let progress = stream
        .decode(&empty, &mut out, FlushMode::Finish)
        .expect("an empty stream produces nothing");
    assert_eq!(progress.produced, 0);
    assert_eq!(progress.status, BrotliStatus::StreamEnd);
    stream.finish().expect("complete stream");

    let one = compress(b"x", 5).expect("compress");
    let mut stream = BrotliStream::new().with_max_output(0);
    let err = stream
        .decode(&one, &mut out, FlushMode::Finish)
        .expect_err("a one-byte body exceeds a zero cap");
    assert!(
        matches!(err, BrotliError::MemoryBudgetExceeded { .. }),
        "unexpected error: {err}"
    );
}

/// Every reference fixture, decoded with the input offered *whole* on every
/// call but the output slice one byte wide, and again with the input dribbled
/// in while the output slice is enormous.
///
/// Both are asymmetric schedules the conformance matrix does not run, and they
/// exercise opposite sides of the resumption logic.
#[test]
fn asymmetric_schedules_match_the_one_shot_decoder() {
    for (name, payload) in [
        ("text", b"asymmetric schedules ".repeat(500)),
        ("random", pseudo_random(40_000, 0x77)),
    ] {
        for quality in [0u32, 6, 11] {
            let compressed = compress(&payload, quality).expect("compress");
            let expected = decompress(&compressed).expect("one-shot");

            let wide_in = decode_incremental(&compressed, InputSchedule::Whole, 1)
                .unwrap_or_else(|e| panic!("{name} q{quality} whole-in/1-out: {e}"));
            assert_eq!(wide_in, expected, "{name} q{quality}: whole-in/1-out");

            let wide_out = decode_incremental(&compressed, InputSchedule::Fixed(1), 1 << 20)
                .unwrap_or_else(|e| panic!("{name} q{quality} 1-in/wide-out: {e}"));
            assert_eq!(wide_out, expected, "{name} q{quality}: 1-in/wide-out");
        }
    }
}

/// A decoder that has already reported `StreamEnd` must keep reporting it for
/// empty calls, and must reject the first trailing byte it is actually shown.
#[test]
fn a_finished_decoder_is_idle_until_it_is_shown_trailing_data() {
    let compressed = compress(b"finished and idle", 5).expect("compress");
    let mut stream = BrotliStream::new();
    let mut out = vec![0u8; 64];
    let progress = stream
        .decode(&compressed, &mut out, FlushMode::Finish)
        .expect("decode");
    assert_eq!(progress.status, BrotliStatus::StreamEnd);

    for _ in 0..100 {
        let idle = stream
            .decode(&[], &mut out, FlushMode::Finish)
            .expect("an idle call on a finished stream");
        assert_eq!(idle.status, BrotliStatus::StreamEnd);
        assert_eq!(idle.produced, 0);
        assert_eq!(idle.consumed, 0);
    }
    stream.finish().expect("still complete");

    let err = stream
        .decode(&[0x00], &mut out, FlushMode::Finish)
        .expect_err("a trailing byte after the final meta-block is corruption");
    assert!(
        err.to_string().contains("trailing data"),
        "unexpected error: {err}"
    );
    assert!(!stream.is_finished(), "a faulted decoder is not finished");
}

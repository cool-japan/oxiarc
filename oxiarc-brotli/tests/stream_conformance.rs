//! Conformance suite for the incremental decoder [`oxiarc_brotli::BrotliStream`].
//!
//! Every test here is an *equivalence* test: whatever the one-shot
//! [`oxiarc_brotli::decompress`] does over a complete slice, the push decoder
//! must do over any chunking of the same bytes — same output, same meta-block
//! shapes, same accept/reject decision. The corpus is the crate's own encoder
//! across all twelve quality levels plus the committed reference `brotli` 1.1.0
//! fixtures (`tests/common`), which is where the interesting format surface
//! lives: static-dictionary words with transforms, multiple block types,
//! context maps, uncompressed meta-blocks and the `w10`/`w18`/`w22` window
//! range.
//!
//! The properties under test:
//!
//! * **chunk invariance** — any input chunking and any output slice size, down
//!   to one byte of each simultaneously, yields identical bytes;
//! * **shape equivalence** — the incremental decoder observes the identical
//!   sequence of [`oxiarc_brotli::MetaBlockShape`]s as
//!   [`oxiarc_brotli::decompress_reporting_shapes`]. Identical output does not
//!   prove a resumable header parser read the right fields at the right bit
//!   positions; an identical shape sequence does;
//! * **truncation** — a prefix of a valid stream, at *every* offset, never
//!   panics, never hangs (the drivers carry a call budget) and never returns a
//!   short body as success;
//! * **caps** — the output budget is exact and enforced before the offending
//!   meta-block is decoded, and the declared-window ceiling is enforced before
//!   the window is allocated;
//! * **strictness parity** — corruption is rejected with the same error class
//!   as the one-shot decoder, and the first fault latches.

mod common;

use common::{
    InputSchedule, call_budget, decode_incremental, drive, payload_corpus, pseudo_random,
    reference_vectors,
};

use oxiarc_brotli::bit_writer::BitWriter;
use oxiarc_brotli::{
    BrotliError, BrotliParams, BrotliStatus, BrotliStream, compress, compress_with_params,
    decompress, decompress_reporting_shapes,
};
use oxiarc_core::traits::FlushMode;

/// Chunk sizes that straddle the 64-bit bit-accumulator refill boundary.
const PRIME_CHUNKS: &[usize] = &[1, 2, 3, 5, 7, 13, 31, 127, 8191];

// ─── Equivalence over the crate's own encoder ───────────────────────────────

/// Every payload shape, every quality level, decoded whole: byte-identical to
/// the one-shot decoder.
#[test]
fn whole_input_matches_one_shot_at_every_quality() {
    for (name, data) in payload_corpus() {
        for quality in 0..=11u32 {
            let compressed = compress(&data, quality).expect("compress");
            let expected = decompress(&compressed).expect("one-shot decode");
            assert_eq!(expected, data, "{name} q{quality}: one-shot round-trip");
            let got = decode_incremental(&compressed, InputSchedule::Whole, 1 << 16)
                .unwrap_or_else(|e| panic!("{name} q{quality}: incremental decode failed: {e}"));
            assert_eq!(got, data, "{name} q{quality}: incremental bytes differ");
        }
    }
}

/// One compressed byte in, one decompressed byte out — the test that exercises
/// every mid-symbol, mid-copy and mid-dictionary-word resumption path at once.
#[test]
fn one_byte_in_one_byte_out_matches_one_shot() {
    for (name, data) in payload_corpus() {
        for quality in [0u32, 1, 5, 9, 11] {
            let compressed = compress(&data, quality).expect("compress");
            let got = decode_incremental(&compressed, InputSchedule::Fixed(1), 1)
                .unwrap_or_else(|e| panic!("{name} q{quality}: 1x1 decode failed: {e}"));
            assert_eq!(got, data, "{name} q{quality}: 1x1 bytes differ");
        }
    }
}

/// Prime-sized input chunks against prime-sized output slices.
#[test]
fn prime_chunk_sizes_match_one_shot() {
    for (name, data) in payload_corpus() {
        let compressed = compress(&data, 9).expect("compress");
        for &in_chunk in PRIME_CHUNKS {
            for &out_chunk in &[1usize, 3, 17, 251, 4096] {
                let got =
                    decode_incremental(&compressed, InputSchedule::Fixed(in_chunk), out_chunk)
                        .unwrap_or_else(|e| {
                            panic!("{name}: in={in_chunk} out={out_chunk} decode failed: {e}")
                        });
                assert_eq!(got, data, "{name}: in={in_chunk} out={out_chunk} differs");
            }
        }
    }
}

/// The same stream decoded under many chunkings must produce identical bytes —
/// the chunking must not be observable at all.
#[test]
fn chunking_is_not_observable() {
    for (name, data) in payload_corpus() {
        let compressed = compress(&data, 6).expect("compress");
        let reference = decompress(&compressed).expect("one-shot");
        for &in_chunk in &[1usize, 2, 4, 9, 64, 1000, usize::MAX / 2] {
            for &out_chunk in &[1usize, 2, 5, 64, 65536] {
                let got = decode_incremental(
                    &compressed,
                    InputSchedule::Fixed(in_chunk.min(compressed.len().max(1))),
                    out_chunk,
                )
                .unwrap_or_else(|e| panic!("{name}: chunk {in_chunk}/{out_chunk}: {e}"));
                assert_eq!(
                    got, reference,
                    "{name}: chunk {in_chunk}/{out_chunk} differs"
                );
            }
        }
    }
}

// ─── Equivalence over reference `brotli` 1.1.0 streams ──────────────────────

/// The committed reference fixtures, decoded whole and byte-at-a-time.
#[test]
fn reference_vectors_decode_incrementally() {
    for vector in reference_vectors() {
        let whole = decode_incremental(&vector.compressed, InputSchedule::Whole, 1 << 16)
            .unwrap_or_else(|e| panic!("{}: whole decode failed: {e}", vector.name));
        assert_eq!(
            whole, vector.original,
            "{}: whole bytes differ",
            vector.name
        );

        let bytewise = decode_incremental(&vector.compressed, InputSchedule::Fixed(1), 1)
            .unwrap_or_else(|e| panic!("{}: 1x1 decode failed: {e}", vector.name));
        assert_eq!(
            bytewise, vector.original,
            "{}: 1x1 bytes differ",
            vector.name
        );

        for &in_chunk in PRIME_CHUNKS {
            let got = decode_incremental(&vector.compressed, InputSchedule::Fixed(in_chunk), 13)
                .unwrap_or_else(|e| panic!("{}: in={in_chunk} failed: {e}", vector.name));
            assert_eq!(
                got, vector.original,
                "{}: in={in_chunk} differs",
                vector.name
            );
        }
    }
}

// ─── The MetaBlockShape differential ────────────────────────────────────────

/// The incremental decoder must observe the identical sequence of meta-block
/// shapes as the one-shot decoder, over the whole corpus.
///
/// This is the strong check on the atomic header parser: identical output
/// bytes can be produced by a parser that read `NTREESL` from the wrong bit
/// offset and happened to land on a compatible value, but an identical
/// `MetaBlockShape` sequence cannot.
#[test]
fn meta_block_shapes_match_the_one_shot_decoder() {
    let mut checked = 0usize;
    for (name, data) in payload_corpus() {
        for quality in 0..=11u32 {
            let compressed = compress(&data, quality).expect("compress");
            let (expected_bytes, expected_shapes) =
                decompress_reporting_shapes(&compressed).expect("one-shot with shapes");

            for &in_chunk in &[1usize, 7, 4096] {
                let mut stream = BrotliStream::new().with_shape_recording(true);
                let got = drive(
                    &mut stream,
                    &compressed,
                    InputSchedule::Fixed(in_chunk),
                    if in_chunk == 1 { 1 } else { 1024 },
                    call_budget(&compressed),
                )
                .unwrap_or_else(|e| panic!("{name} q{quality} in={in_chunk}: {e}"));
                assert_eq!(
                    got, expected_bytes,
                    "{name} q{quality} in={in_chunk}: bytes"
                );
                assert_eq!(
                    stream.recorded_shapes(),
                    expected_shapes.as_slice(),
                    "{name} q{quality} in={in_chunk}: meta-block shape sequence differs"
                );
                checked += 1;
            }
        }
    }
    assert!(
        checked > 300,
        "shape differential covered only {checked} cases"
    );
}

/// The same differential over the reference `brotli` fixtures, which carry
/// richer shapes (multiple block types, real context maps) than this crate's
/// encoder emits.
#[test]
fn meta_block_shapes_match_for_reference_streams() {
    let mut saw_split = false;
    for vector in reference_vectors() {
        let (expected_bytes, expected_shapes) =
            decompress_reporting_shapes(&vector.compressed).expect("one-shot with shapes");
        for &in_chunk in &[1usize, 3, 65536] {
            let mut stream = BrotliStream::new().with_shape_recording(true);
            let got = drive(
                &mut stream,
                &vector.compressed,
                InputSchedule::Fixed(in_chunk),
                7,
                call_budget(&vector.compressed),
            )
            .unwrap_or_else(|e| panic!("{}: in={in_chunk}: {e}", vector.name));
            assert_eq!(got, expected_bytes, "{}: in={in_chunk} bytes", vector.name);
            assert_eq!(
                stream.recorded_shapes(),
                expected_shapes.as_slice(),
                "{}: in={in_chunk} shape sequence differs",
                vector.name
            );
        }
        if expected_shapes
            .iter()
            .any(|s| s.literal_trees > 1 || s.literal_types > 1 || s.distance_types > 1)
        {
            saw_split = true;
        }
    }
    assert!(
        saw_split,
        "the reference corpus must contain a block-split / context-mapped meta-block, \
         otherwise this differential proves nothing"
    );
}

// ─── Truncation ─────────────────────────────────────────────────────────────

/// Truncating a valid stream at *every* offset must never panic, never hang
/// and never succeed with a short body.
#[test]
fn truncation_at_every_offset_is_rejected() {
    let mut streams: Vec<Vec<u8>> = Vec::new();
    for quality in [0u32, 1, 6, 11] {
        streams.push(
            compress(
                b"truncate me at every offset, please. "
                    .repeat(20)
                    .as_slice(),
                quality,
            )
            .expect("compress"),
        );
    }
    for vector in reference_vectors() {
        streams.push(vector.compressed);
    }

    for compressed in &streams {
        for cut in 0..compressed.len() {
            let prefix = &compressed[..cut];
            // Fed whole.
            let whole = decode_incremental(prefix, InputSchedule::Whole, 4096);
            assert!(
                whole.is_err(),
                "truncation at {cut}/{} succeeded when fed whole",
                compressed.len()
            );
            // Fed one byte at a time.
            let bytewise = decode_incremental(prefix, InputSchedule::Fixed(1), 1);
            assert!(
                bytewise.is_err(),
                "truncation at {cut}/{} succeeded when fed byte-at-a-time",
                compressed.len()
            );
        }
    }
}

/// A truncated prefix followed by an unbounded number of empty `decode` calls
/// terminates, stays idle, and only becomes an error at `finish`.
#[test]
fn idle_calls_after_a_truncated_prefix_terminate() {
    // A payload whose *compressed* form is comfortably larger than one
    // meta-block prelude, so half of it really does decode to something.
    // Repetitive text would compress to a hundred bytes and prove nothing, so
    // this mixes semi-random content into a textual shell.
    let noise = pseudo_random(200_000, 0x0DDB_A11E);
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
        compressed.len() > 512,
        "fixture must be big enough for half a stream to decode: {} bytes",
        compressed.len()
    );
    let prefix = &compressed[..compressed.len() / 2];
    let mut stream = BrotliStream::new();
    let mut buf = vec![0u8; 4096];
    let mut produced_total = 0usize;
    let first = stream
        .decode(prefix, &mut buf, FlushMode::None)
        .expect("prefix decode");
    produced_total += first.produced;
    for _ in 0..10_000 {
        let p = stream
            .decode(&[], &mut buf, FlushMode::None)
            .expect("idle decode must not error");
        assert_eq!(p.consumed, 0, "idle call consumed input");
        produced_total += p.produced;
        if p.status == BrotliStatus::NeedInput {
            break;
        }
    }
    assert!(produced_total > 0, "half a stream should decode some bytes");
    assert!(
        !stream.is_finished(),
        "a truncated stream must not report finished"
    );
    assert!(
        stream.finish().is_err(),
        "finish must reject a truncated stream"
    );
}

/// A truncated stream produces a *prefix* of the real output, never bytes the
/// full stream would not have produced.
#[test]
fn truncated_output_is_a_prefix_of_the_full_output() {
    let data = b"prefix property over a long, compressible body. ".repeat(400);
    let compressed = compress(&data, 9).expect("compress");
    for cut in (1..compressed.len()).step_by(7) {
        let mut stream = BrotliStream::new();
        let mut decoded = Vec::new();
        let mut buf = vec![0u8; 512];
        let mut pos = 0usize;
        let mut guard = 0u32;
        loop {
            guard += 1;
            assert!(guard < 100_000, "no termination at cut {cut}");
            let end = (pos + 64).min(cut);
            let progress = match stream.decode(&compressed[pos..end], &mut buf, FlushMode::None) {
                Ok(p) => p,
                Err(_) => break,
            };
            pos += progress.consumed;
            decoded.extend_from_slice(&buf[..progress.produced]);
            if progress.status == BrotliStatus::StreamEnd {
                break;
            }
            if progress.consumed == 0 && progress.produced == 0 {
                break;
            }
        }
        assert!(
            data.starts_with(&decoded),
            "cut {cut}: emitted {} bytes that are not a prefix of the real output",
            decoded.len()
        );
    }
}

// ─── Caps ───────────────────────────────────────────────────────────────────

/// The output cap is exact: `N` succeeds, `N - 1` fails, on the same stream.
#[test]
fn output_cap_is_exact() {
    let data = vec![0xC3u8; 200_000];
    let compressed = compress(&data, 6).expect("compress");
    let exact = data.len() as u64;

    let mut ok = BrotliStream::new().with_max_output(exact);
    let got = drive(
        &mut ok,
        &compressed,
        InputSchedule::Fixed(97),
        1024,
        call_budget(&compressed),
    )
    .expect("exact budget must succeed");
    assert_eq!(got, data);

    let mut tight = BrotliStream::new().with_max_output(exact - 1);
    let err = drive(
        &mut tight,
        &compressed,
        InputSchedule::Fixed(97),
        1024,
        call_budget(&compressed),
    )
    .expect_err("one byte under budget must fail");
    assert!(
        matches!(err, BrotliError::MemoryBudgetExceeded { .. }),
        "unexpected error: {err}"
    );
}

/// The cap must be enforced identically whether the stream is fed whole or one
/// byte at a time, and it must not depend on the output slice size.
#[test]
fn output_cap_is_chunking_independent() {
    let data = vec![0x11u8; 300_000];
    let compressed = compress(&data, 5).expect("compress");
    for &in_chunk in &[1usize, 13, 4096, usize::MAX / 2] {
        for &out_chunk in &[1usize, 977, 65536] {
            let mut stream = BrotliStream::new().with_max_output(64 * 1024);
            let err = drive(
                &mut stream,
                &compressed,
                InputSchedule::Fixed(in_chunk.min(compressed.len().max(1))),
                out_chunk,
                call_budget(&compressed),
            )
            .expect_err("over-budget stream must fail");
            assert!(
                matches!(err, BrotliError::MemoryBudgetExceeded { .. }),
                "in={in_chunk} out={out_chunk}: unexpected error: {err}"
            );
        }
    }
}

/// A bomb must be refused before its expansion is produced: the decoder emits
/// strictly less than the budget before failing.
#[test]
fn a_bomb_is_refused_before_it_expands() {
    let bomb_source = vec![0u8; 64 * 1024 * 1024];
    let compressed = compress(&bomb_source, 5).expect("compress");
    assert!(
        compressed.len() < 1 << 20,
        "the bomb fixture must be small: {} bytes",
        compressed.len()
    );
    let mut stream = BrotliStream::new().with_max_output(1 << 20);
    let mut buf = vec![0u8; 64 * 1024];
    let mut pos = 0usize;
    let mut err = None;
    while err.is_none() {
        let end = (pos + 4096).min(compressed.len());
        let flush = if end == compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        match stream.decode(&compressed[pos..end], &mut buf, flush) {
            Ok(p) => {
                pos += p.consumed;
                if p.status == BrotliStatus::StreamEnd {
                    panic!("the bomb decoded within a 1 MiB budget");
                }
            }
            Err(e) => err = Some(e),
        }
    }
    let err = err.expect("bomb must be refused");
    assert!(
        matches!(err, BrotliError::MemoryBudgetExceeded { .. }),
        "unexpected error: {err}"
    );
    assert!(
        stream.total_out() <= 1 << 20,
        "produced {} bytes, more than the 1 MiB budget",
        stream.total_out()
    );
}

/// The declared window ceiling is checked while reading the stream header, so
/// the refusal happens before the ring is allocated and before any output.
#[test]
fn declared_window_ceiling_is_enforced() {
    for lgwin in [18u32, 20, 22, 24] {
        let params = BrotliParams {
            quality: 5,
            lgwin,
            ..BrotliParams::default()
        };
        let data = b"window ceiling probe ".repeat(500);
        let compressed = compress_with_params(&data, &params).expect("compress");

        // A ceiling below the declared window is refused up front.
        let mut strict = BrotliStream::new().with_max_window((1usize << lgwin) - 1);
        let err = drive(
            &mut strict,
            &compressed,
            InputSchedule::Whole,
            4096,
            call_budget(&compressed),
        )
        .expect_err("declared window over the ceiling must be refused");
        assert!(
            matches!(err, BrotliError::WindowTooLarge { .. }),
            "lgwin {lgwin}: unexpected error: {err}"
        );
        assert_eq!(
            strict.total_out(),
            0,
            "lgwin {lgwin}: bytes escaped the refusal"
        );

        // A ceiling at the declared window is accepted.
        let mut ok = BrotliStream::new().with_max_window(1usize << lgwin);
        let got = drive(
            &mut ok,
            &compressed,
            InputSchedule::Fixed(31),
            333,
            call_budget(&compressed),
        )
        .unwrap_or_else(|e| panic!("lgwin {lgwin}: exact ceiling rejected: {e}"));
        assert_eq!(got, data, "lgwin {lgwin}: bytes differ");
        assert_eq!(ok.window_size(), Some((1usize << lgwin) - 16));
    }
}

/// The default ceiling admits every RFC 7932 window, including `WBITS = 24`.
#[test]
fn large_window_streams_decode_by_default() {
    let params = BrotliParams {
        quality: 6,
        lgwin: 24,
        ..BrotliParams::default()
    };
    let data = pseudo_random(300_000, 0xDEAD_BEEF);
    let compressed = compress_with_params(&data, &params).expect("compress");
    let got = decode_incremental(&compressed, InputSchedule::Fixed(1024), 4096).expect("decode");
    assert_eq!(got, data);
}

// ─── Strictness parity and the fault latch ──────────────────────────────────

/// A trailing byte after the final meta-block is corruption (RFC 7932 defines
/// no stream concatenation), exactly as the one-shot decoder treats it.
#[test]
fn trailing_bytes_are_rejected_like_the_one_shot_decoder() {
    let compressed = compress(b"no concatenation allowed", 6).expect("compress");
    for extra in [0x00u8, 0x01, 0xFF] {
        let mut with_tail = compressed.clone();
        with_tail.push(extra);
        assert!(
            decompress(&with_tail).is_err(),
            "one-shot accepted a trailing {extra:#04x}"
        );
        assert!(
            decode_incremental(&with_tail, InputSchedule::Fixed(1), 64).is_err(),
            "incremental accepted a trailing {extra:#04x}"
        );
    }
}

/// Bit flips must be rejected or decoded correctly — never a panic, never a
/// hang, and never a silent disagreement with the one-shot decoder.
#[test]
fn bit_flips_never_panic_and_never_disagree() {
    let data = b"bit flip corpus with dictionary words and repetition. ".repeat(40);
    let compressed = compress(&data, 9).expect("compress");
    let mut agreements = 0usize;
    for byte in 0..compressed.len() {
        for bit in 0..8u32 {
            let mut corrupt = compressed.clone();
            corrupt[byte] ^= 1 << bit;
            let one_shot = decompress(&corrupt);
            let incremental = decode_incremental(&corrupt, InputSchedule::Fixed(5), 71);
            match (one_shot, incremental) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "byte {byte} bit {bit}: decoders disagree");
                    agreements += 1;
                }
                (Err(_), Err(_)) => agreements += 1,
                (Ok(a), Err(e)) => panic!(
                    "byte {byte} bit {bit}: one-shot decoded {} bytes, incremental failed: {e}",
                    a.len()
                ),
                (Err(e), Ok(b)) => panic!(
                    "byte {byte} bit {bit}: one-shot failed ({e}), incremental decoded {} bytes",
                    b.len()
                ),
            }
        }
    }
    assert_eq!(agreements, compressed.len() * 8);
}

/// The first fault latches: every later call returns an error, and only
/// `reset` clears it.
#[test]
fn faults_latch_until_reset() {
    let mut corrupt = compress(b"latching fault", 6).expect("compress");
    corrupt[1] ^= 0xAA;
    corrupt.push(0x5A);
    let mut stream = BrotliStream::new();
    let mut buf = vec![0u8; 256];
    let mut pos = 0usize;
    let mut hit = None;
    while hit.is_none() && pos <= corrupt.len() {
        let end = (pos + 1).min(corrupt.len());
        let flush = if end == corrupt.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        match stream.decode(&corrupt[pos..end], &mut buf, flush) {
            Ok(p) => {
                pos += p.consumed;
                if p.status == BrotliStatus::StreamEnd {
                    break;
                }
            }
            Err(e) => hit = Some(e.to_string()),
        }
    }
    let message = hit.expect("the corrupted stream must fail");
    for _ in 0..8 {
        let again = stream
            .decode(&[], &mut buf, FlushMode::None)
            .expect_err("the fault must latch");
        assert_eq!(again.to_string(), message, "the latched error changed");
    }
    assert!(stream.finish().is_err(), "finish must report the latch");
    stream.reset();
    let clean = compress(b"after reset", 6).expect("compress");
    let got = drive(
        &mut stream,
        &clean,
        InputSchedule::Whole,
        64,
        call_budget(&clean),
    )
    .expect("reset must clear the fault");
    assert_eq!(got, b"after reset");
}

/// One decoder instance, reset between streams, decodes an unlimited sequence
/// of unrelated streams — the distance ring, window and context bytes must not
/// leak across a reset.
#[test]
fn reset_isolates_consecutive_streams() {
    let payloads = payload_corpus();
    let mut stream = BrotliStream::new();
    for (name, data) in &payloads {
        let compressed = compress(data, 7).expect("compress");
        let got = drive(
            &mut stream,
            &compressed,
            InputSchedule::Fixed(101),
            257,
            call_budget(&compressed),
        )
        .unwrap_or_else(|e| panic!("{name}: reuse after reset failed: {e}"));
        assert_eq!(&got, data, "{name}: bytes differ after reuse");
        stream.reset();
        assert_eq!(stream.total_in(), 0);
        assert_eq!(stream.total_out(), 0);
        assert!(stream.window_size().is_none());
    }
}

/// An empty output slice must be reported as [`BrotliStatus::NeedOutput`]
/// without consuming or producing anything, and must not stall the decoder.
#[test]
fn empty_output_slice_is_handled() {
    let data = b"empty output slice handling".repeat(30);
    let compressed = compress(&data, 5).expect("compress");
    let mut stream = BrotliStream::new();
    let mut nothing: [u8; 0] = [];
    let progress = stream
        .decode(&compressed, &mut nothing, FlushMode::None)
        .expect("decode with no room");
    assert_eq!(progress.produced, 0);
    // The stream header and meta-block prelude may be consumed; no output can be.
    let mut decoded = Vec::new();
    let mut buf = vec![0u8; 64];
    let mut guard = 0u32;
    loop {
        guard += 1;
        assert!(
            guard < 100_000,
            "no termination after an empty output slice"
        );
        let p = stream
            .decode(&[], &mut buf, FlushMode::Finish)
            .expect("drain");
        decoded.extend_from_slice(&buf[..p.produced]);
        if p.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    stream.finish().expect("stream must complete");
    assert_eq!(decoded, data);
}

/// `total_in` / `total_out` must track the stream exactly.
#[test]
fn counters_are_exact() {
    for (name, data) in payload_corpus() {
        let compressed = compress(&data, 4).expect("compress");
        let mut stream = BrotliStream::new();
        let got = drive(
            &mut stream,
            &compressed,
            InputSchedule::Fixed(37),
            911,
            call_budget(&compressed),
        )
        .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(got.len() as u64, stream.total_out(), "{name}: total_out");
        assert_eq!(
            compressed.len() as u64,
            stream.total_in(),
            "{name}: total_in"
        );
        assert!(stream.is_finished(), "{name}: not finished");
    }
}

// ─── Meta-block flavours the crate's own encoder never emits ────────────────

/// Build a Brotli stream by hand out of raw bit fields (RFC 7932 Section 9).
///
/// The crate's encoder emits only content and stored meta-blocks, so the
/// decoder's metadata path is otherwise reachable only from a foreign encoder.
/// Constructing the bits directly is the only way to cover it — and the
/// incremental decoder's `MetadataSkip` state, which streams a payload of up to
/// 2^24 bytes across arbitrarily many calls, is exactly the kind of state a
/// resumable decoder gets wrong.
fn build_stream(build: impl FnOnce(&mut BitWriter) -> Result<(), BrotliError>) -> Vec<u8> {
    let mut writer = BitWriter::new();
    // Stream header: a single 0 bit selects WBITS = 16.
    writer.write_bit(false).expect("wbits");
    build(&mut writer).expect("build");
    // Final meta-block: ISLAST = 1, ISLASTEMPTY = 1.
    writer.write_bit(true).expect("islast");
    writer.write_bit(true).expect("islastempty");
    writer.finish()
}

/// Append a metadata meta-block carrying `payload` (RFC 7932 Section 9.2).
fn write_metadata(writer: &mut BitWriter, payload: &[u8]) -> Result<(), BrotliError> {
    writer.write_bit(false)?; // ISLAST = 0
    writer.write_bits(3, 2)?; // MNIBBLES code 3 => metadata
    writer.write_bit(false)?; // reserved
    if payload.is_empty() {
        writer.write_bits(0, 2)?; // MSKIPBYTES = 0 => empty metadata
        writer.flush();
        return Ok(());
    }
    let len = payload.len() - 1;
    let bytes: u32 = if len < 0x100 {
        1
    } else if len < 0x1_0000 {
        2
    } else {
        3
    };
    writer.write_bits(bytes, 2)?; // MSKIPBYTES
    for i in 0..bytes {
        writer.write_bits((len as u32 >> (i * 8)) & 0xFF, 8)?;
    }
    writer.flush(); // pad to a byte boundary with zeros
    writer.write_bytes(payload)?;
    Ok(())
}

/// Metadata meta-blocks produce no output and must be skipped identically by
/// both decoders, at every chunking — including one that splits the payload.
#[test]
fn metadata_meta_blocks_are_skipped_incrementally() {
    for payload_len in [0usize, 1, 2, 255, 256, 257, 4096, 70_000] {
        let payload = pseudo_random(payload_len, 0xBEEF ^ payload_len as u64);
        let stream = build_stream(|w| write_metadata(w, &payload));

        let expected = decompress(&stream)
            .unwrap_or_else(|e| panic!("payload {payload_len}: one-shot rejected it: {e}"));
        assert!(expected.is_empty(), "metadata must produce no output");

        for &in_chunk in &[1usize, 3, 127, usize::MAX / 2] {
            let got = decode_incremental(
                &stream,
                InputSchedule::Fixed(in_chunk.min(stream.len().max(1))),
                5,
            )
            .unwrap_or_else(|e| panic!("payload {payload_len} in={in_chunk}: {e}"));
            assert!(
                got.is_empty(),
                "payload {payload_len} in={in_chunk}: produced {} bytes",
                got.len()
            );
        }
    }
}

/// A metadata meta-block truncated inside its payload must be rejected, not
/// silently accepted as a complete stream.
#[test]
fn truncated_metadata_payload_is_rejected() {
    let payload = pseudo_random(5000, 0xFEED);
    let stream = build_stream(|w| write_metadata(w, &payload));
    for cut in (1..stream.len()).step_by(37) {
        assert!(
            decode_incremental(&stream[..cut], InputSchedule::Fixed(11), 8).is_err(),
            "a {cut}/{}-byte prefix of a metadata stream decoded successfully",
            stream.len()
        );
    }
}

/// Metadata interleaved with content must not disturb the distance ring or the
/// literal-context bytes carried across meta-blocks.
#[test]
fn metadata_between_content_does_not_disturb_decoder_state() {
    // A real content stream, with a metadata block spliced in front of it, is
    // not something this crate can build compositionally (meta-blocks are
    // bit-packed, not byte-aligned), so this checks the weaker but still
    // meaningful property: a metadata block followed by the final empty
    // meta-block leaves the stream valid and empty, under every chunking, and
    // the decoder is reusable afterwards.
    let payload = b"metadata payload that carries no output".to_vec();
    let stream = build_stream(|w| {
        write_metadata(w, &payload)?;
        write_metadata(w, b"a second metadata block")
    });
    let expected = decompress(&stream).expect("one-shot");
    assert!(expected.is_empty());

    let mut decoder = BrotliStream::new();
    for &in_chunk in &[1usize, 2, 9, 1000] {
        let got = drive(
            &mut decoder,
            &stream,
            InputSchedule::Fixed(in_chunk.min(stream.len())),
            3,
            call_budget(&stream),
        )
        .unwrap_or_else(|e| panic!("in={in_chunk}: {e}"));
        assert!(
            got.is_empty(),
            "in={in_chunk}: produced {} bytes",
            got.len()
        );
        decoder.reset();
    }

    // And the same decoder still handles a normal stream afterwards.
    let content = compress(b"content after metadata", 6).expect("compress");
    let got = drive(
        &mut decoder,
        &content,
        InputSchedule::Fixed(1),
        1,
        call_budget(&content),
    )
    .expect("content after metadata");
    assert_eq!(got, b"content after metadata");
}

// ─── Streams larger than the internal carry ─────────────────────────────────

/// A compressed stream bigger than the decoder's internal carry cap, offered in
/// a single `decode` call.
///
/// `decode` then takes only part of what it was handed and reports a short
/// `consumed`; the caller must offer the remainder. This is the one situation
/// in which `consumed < input.len()`, and it is easy to get wrong in two ways:
/// treating the short take as an error, or — as an earlier revision of this
/// decoder did — mistaking a merely *large* buffer for evidence of an oversized
/// meta-block header. Every payload in the main corpus compresses to well under
/// the cap, so nothing else here reaches this path.
#[test]
fn a_stream_larger_than_the_carry_decodes_in_one_call() {
    // ~5.9 MB of semi-random text compresses to ~2.9 MB, past both the 1 MiB
    // header cap and the 2 MiB carry it sizes — so the decoder really does have
    // to take the stream in more than one bite.
    let noise = pseudo_random(2 << 20, 0xCA55_E77E);
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

    // One call per iteration, always offering everything that is left.
    let mut stream = BrotliStream::new();
    let mut decoded = Vec::new();
    let mut out = vec![0u8; 256 * 1024];
    let mut pos = 0usize;
    let mut short_takes = 0usize;
    let mut guard = 0u32;
    loop {
        guard += 1;
        assert!(guard < 100_000, "no termination");
        let progress = stream
            .decode(&compressed[pos..], &mut out, FlushMode::Finish)
            .expect("decode");
        if progress.consumed < compressed.len() - pos {
            short_takes += 1;
        }
        pos += progress.consumed;
        decoded.extend_from_slice(&out[..progress.produced]);
        if progress.status == BrotliStatus::StreamEnd {
            break;
        }
    }
    stream.finish().expect("complete stream");
    assert_eq!(decoded, data);
    assert!(
        short_takes > 0,
        "the fixture did not actually exercise the bounded carry"
    );
    assert_eq!(stream.total_in(), compressed.len() as u64);
}

/// The same oversized stream under a range of chunkings, so the carry's
/// compaction and rebase are exercised at every fill level.
#[test]
fn a_stream_larger_than_the_carry_is_chunk_invariant() {
    let noise = pseudo_random(600_000, 0x5EED);
    let mut data = Vec::new();
    for (i, chunk) in noise.chunks(16).enumerate() {
        data.extend_from_slice(format!("row {i}: ").as_bytes());
        for byte in chunk {
            data.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        data.push(b'\n');
    }
    let compressed = compress(&data, 5).expect("compress");
    let reference = decompress(&compressed).expect("one-shot");
    for &in_chunk in &[7usize, 65_536, 700_000, usize::MAX / 2] {
        let got = decode_incremental(
            &compressed,
            InputSchedule::Fixed(in_chunk.min(compressed.len())),
            4096,
        )
        .unwrap_or_else(|e| panic!("in={in_chunk}: {e}"));
        assert_eq!(got, reference, "in={in_chunk}: bytes differ");
    }
}

//! Output back-pressure and whole-input buffering for
//! [`LzhEncoder`]'s [`Compressor`] implementation.
//!
//! `Compressor::compress_all` drives the encoder with a fixed **32 KiB**
//! output buffer. An encoder that discards whatever does not fit in one call
//! therefore truncates silently at exactly that size, with no error — which is
//! what `LzhEncoder` did before 0.4.2 (a 200 KiB payload came back as 32 768
//! bytes of a valid-looking prefix). These tests pin the fixed behaviour:
//! nothing is ever dropped, whatever the caller's buffer size, and every
//! method reachable through the encoder works through the convenience method.

use oxiarc_core::traits::{CompressStatus, Compressor, Decompressor, FlushMode};
use oxiarc_lzhuf::LzhMethod;
use oxiarc_lzhuf::decode::LzhDecoder;
use oxiarc_lzhuf::encode::LzhEncoder;

/// The buffer size `compress_all` uses internally.
const CONVENIENCE_BUFFER: usize = 32 * 1024;

/// SplitMix64 output: every byte decorrelated, so LZSS finds essentially no
/// matches and the compressed stream stays at least as large as the input.
/// A compressible fixture would never exceed the 32 KiB convenience buffer and
/// so would never exercise the defect at all.
fn incompressible(size: usize) -> Vec<u8> {
    let mut seed: u64 = 0x1234_5678_9abc_def0;
    let mut out = Vec::with_capacity(size + 8);
    while out.len() < size {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.extend_from_slice(&z.to_le_bytes());
    }
    out.truncate(size);
    out
}

/// Every method an `LzhEncoder` can be constructed with.
fn all_methods() -> Vec<LzhMethod> {
    vec![
        LzhMethod::Lh0,
        LzhMethod::Lh1,
        LzhMethod::Lh2,
        LzhMethod::Lh3,
        LzhMethod::Lh4,
        LzhMethod::Lh5,
        LzhMethod::Lh6,
        LzhMethod::Lh7,
        LzhMethod::Lzs,
        LzhMethod::Lz5,
        LzhMethod::Lhd,
    ]
}

/// `compress_all` must return exactly what the one-shot encoder returns —
/// never a prefix of it — for a payload whose compressed form is far larger
/// than the 32 KiB convenience buffer.
#[test]
fn compress_all_does_not_truncate_at_the_convenience_buffer() {
    let original = incompressible(200 * 1024);

    for method in [
        LzhMethod::Lh0,
        LzhMethod::Lh4,
        LzhMethod::Lh5,
        LzhMethod::Lh6,
        LzhMethod::Lh7,
    ] {
        let mut reference = LzhEncoder::new(method);
        let expected = reference
            .compress_to_vec(&original)
            .unwrap_or_else(|e| panic!("{method:?}: compress_to_vec failed: {e}"));
        assert!(
            expected.len() > CONVENIENCE_BUFFER,
            "{method:?}: fixture must compress to more than {CONVENIENCE_BUFFER} bytes \
             to exercise the defect; got {}",
            expected.len()
        );

        let mut encoder = LzhEncoder::new(method);
        let via_trait = encoder
            .compress_all(&original)
            .unwrap_or_else(|e| panic!("{method:?}: compress_all failed: {e}"));

        assert_eq!(
            via_trait.len(),
            expected.len(),
            "{method:?}: compress_all returned {} bytes, one-shot returned {} \
             (silent truncation at the convenience buffer?)",
            via_trait.len(),
            expected.len()
        );
        assert_eq!(
            via_trait, expected,
            "{method:?}: compress_all byte mismatch"
        );

        let mut decoder = LzhDecoder::new(method, original.len() as u64);
        let decoded = decoder
            .decompress_all(&via_trait)
            .unwrap_or_else(|e| panic!("{method:?}: decompress_all failed: {e}"));
        assert_eq!(decoded, original, "{method:?}: round-trip mismatch");
    }
}

/// A one-byte output slice is the harshest back-pressure a caller can apply:
/// the encoder must hand the stream back one byte per call and lose nothing.
#[test]
fn a_one_byte_output_buffer_still_delivers_the_whole_stream() {
    let original = incompressible(70 * 1024);
    let mut reference = LzhEncoder::new(LzhMethod::Lh5);
    let expected = reference.compress_to_vec(&original).expect("one-shot");

    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    let mut input_pos = 0usize;
    // Generous ceiling so a regression to "spins" fails instead of hanging.
    let max_calls = expected.len() * 4 + 10_000;
    let mut calls = 0usize;

    loop {
        calls += 1;
        assert!(
            calls < max_calls,
            "encoder made no progress ({calls} calls)"
        );

        let flush = if input_pos >= original.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let (consumed, produced, status) = encoder
            .compress(&original[input_pos..], &mut byte, flush)
            .expect("compress failed");
        input_pos += consumed;
        out.extend_from_slice(&byte[..produced]);
        if status == CompressStatus::Done {
            break;
        }
    }

    assert_eq!(
        out, expected,
        "byte-at-a-time delivery must reproduce the one-shot stream exactly"
    );
    assert!(encoder.is_finished());
}

/// `is_finished()` must not claim completion while compressed bytes are still
/// staged inside the encoder — that is the predicate a caller uses to decide
/// it may stop reading.
#[test]
fn is_finished_is_false_while_output_is_still_staged() {
    let original = incompressible(70 * 1024);
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let mut small = [0u8; 16];

    let (_, _, status) = encoder
        .compress(&original, &mut small, FlushMode::Finish)
        .expect("compress failed");
    assert_eq!(
        status,
        CompressStatus::NeedsOutput,
        "a 16-byte sink cannot have received the whole stream"
    );
    assert!(
        !encoder.is_finished(),
        "is_finished() must stay false while staged output is undelivered"
    );
}

/// Every method must round-trip through the trait convenience methods,
/// including the whole-stream codecs (`-lh1-`, `-lh2-`, `-lh3-`, `-lzs-`,
/// `-lz5-`) whose encoders cannot compress a prefix: before 0.4.2 those
/// returned "requires a single call with finish=true" from `compress_all` for
/// any non-empty payload, because the driver passes `FlushMode::None` first.
#[test]
fn every_method_round_trips_through_the_convenience_methods() {
    for method in all_methods() {
        for size in [0usize, 1, 100, 4096] {
            let original: Vec<u8> = (0..size).map(|i| ((i * 7) % 251) as u8).collect();

            let mut encoder = LzhEncoder::new(method);
            let compressed = encoder
                .compress_all(&original)
                .unwrap_or_else(|e| panic!("{method:?} size {size}: compress_all failed: {e}"));
            assert!(
                encoder.is_finished(),
                "{method:?} size {size}: not finished"
            );

            let mut reference = LzhEncoder::new(method);
            let expected = reference
                .compress_to_vec(&original)
                .unwrap_or_else(|e| panic!("{method:?} size {size}: compress_to_vec failed: {e}"));
            assert_eq!(
                compressed, expected,
                "{method:?} size {size}: compress_all differs from the one-shot encoder"
            );

            let mut decoder = LzhDecoder::new(method, original.len() as u64);
            let decoded = decoder
                .decompress_all(&compressed)
                .unwrap_or_else(|e| panic!("{method:?} size {size}: decompress_all failed: {e}"));
            assert_eq!(
                decoded, original,
                "{method:?} size {size}: round-trip mismatch"
            );
        }
    }
}

/// `reset()` must clear staged output and staged input, so a reused encoder
/// never prepends the tail of a previous stream to the next one.
#[test]
fn reset_clears_staged_input_and_output() {
    let first = incompressible(70 * 1024);
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let mut small = [0u8; 16];
    let _ = encoder
        .compress(&first, &mut small, FlushMode::Finish)
        .expect("compress failed");
    assert!(!encoder.is_finished());

    Compressor::reset(&mut encoder);

    let second = b"a completely different, short payload".to_vec();
    let compressed = encoder.compress_all(&second).expect("compress_all failed");
    let mut reference = LzhEncoder::new(LzhMethod::Lh5);
    let expected = reference.compress_to_vec(&second).expect("one-shot");
    assert_eq!(
        compressed, expected,
        "a reset encoder must produce a fresh stream, with no residue"
    );

    let mut decoder = LzhDecoder::new(LzhMethod::Lh5, second.len() as u64);
    assert_eq!(
        decoder.decompress_all(&compressed).expect("decompress_all"),
        second
    );
}

/// The whole-stream methods buffer input across calls; a caller that feeds
/// them in many small chunks must still get the same stream as one-shot.
#[test]
fn whole_input_methods_accept_chunked_input() {
    let original: Vec<u8> = (0..3000).map(|i| ((i * 13) % 241) as u8).collect();

    for method in [
        LzhMethod::Lh1,
        LzhMethod::Lh2,
        LzhMethod::Lh3,
        LzhMethod::Lzs,
        LzhMethod::Lz5,
    ] {
        let mut encoder = LzhEncoder::new(method);
        let mut out = Vec::new();
        let mut scratch = vec![0u8; 64];

        for chunk in original.chunks(37) {
            let (consumed, produced, _) = encoder
                .compress(chunk, &mut scratch, FlushMode::None)
                .unwrap_or_else(|e| panic!("{method:?}: chunked compress failed: {e}"));
            assert_eq!(
                consumed,
                chunk.len(),
                "{method:?}: chunk not fully accepted"
            );
            out.extend_from_slice(&scratch[..produced]);
        }
        loop {
            let (_, produced, status) = encoder
                .compress(&[], &mut scratch, FlushMode::Finish)
                .unwrap_or_else(|e| panic!("{method:?}: finish failed: {e}"));
            out.extend_from_slice(&scratch[..produced]);
            if status == CompressStatus::Done {
                break;
            }
        }

        let mut reference = LzhEncoder::new(method);
        let expected = reference.compress_to_vec(&original).expect("one-shot");
        assert_eq!(
            out, expected,
            "{method:?}: chunked encode differs from one-shot"
        );

        let mut decoder = LzhDecoder::new(method, original.len() as u64);
        assert_eq!(
            decoder.decompress_all(&out).expect("decompress_all"),
            original,
            "{method:?}: chunked round-trip mismatch"
        );
    }
}

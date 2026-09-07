//! Adversarial robustness tests for [`StreamingLzhDecoder`] driven purely
//! through the public API.
//!
//! These pin the two properties a streaming decoder handed untrusted bytes
//! must have: it always terminates (no spin, no hang) and it never claims a
//! truncated or corrupted stream decoded completely. They live in an
//! integration test rather than in `src/streaming/decoder.rs` so that file
//! stays well inside the workspace's file-length budget.

use oxiarc_core::DecompressStatus;
use oxiarc_lzhuf::LzhMethod;
use oxiarc_lzhuf::encode::LzhEncoder;
use oxiarc_lzhuf::streaming::StreamingLzhDecoder;

/// What a bounded decode run ended up doing.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Reported `Done`.
    Finished,
    /// Returned an error.
    Errored,
    /// Asked for more input (the honest answer for a truncated stream).
    NeedsMoreInput,
    /// Made no progress within the call ceiling — the hang shape.
    Stalled,
}

/// Drive one decoder over `input` with a hard ceiling on the number of
/// `decompress` calls. Never loops forever: a decoder that neither errors nor
/// progresses shows up as [`Outcome::Stalled`].
fn drive_to_completion(
    method: LzhMethod,
    uncompressed_size: u64,
    input: &[u8],
    output_chunk: usize,
) -> Outcome {
    let mut decoder = StreamingLzhDecoder::new(method, uncompressed_size);
    let mut scratch = vec![0u8; output_chunk];
    let mut fed = false;
    let max_calls = input.len() * 4 + 4096;
    for _ in 0..max_calls {
        let slice: &[u8] = if fed { &[] } else { input };
        let (_consumed, produced, status) = match decoder.decompress(slice, &mut scratch) {
            Ok(triple) => triple,
            Err(_) => return Outcome::Errored,
        };
        fed = true;
        match status {
            DecompressStatus::Done => return Outcome::Finished,
            DecompressStatus::NeedsInput => return Outcome::NeedsMoreInput,
            // No progress and no error: that is the hang shape.
            DecompressStatus::NeedsOutput if produced == 0 => return Outcome::Stalled,
            DecompressStatus::NeedsOutput => {}
            DecompressStatus::BlockEnd => {}
            // `DecompressStatus` is `#[non_exhaustive]`; an unknown future
            // status is treated as "keep going", and the call ceiling still
            // converts a non-terminating decoder into `Stalled`.
            _ => {}
        }
    }
    Outcome::Stalled
}

fn sample_stream() -> (Vec<u8>, Vec<u8>) {
    let original: Vec<u8> = (0..8192u32)
        .map(|i| (i.wrapping_mul(7) >> 3) as u8)
        .collect();
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let compressed = encoder
        .compress_to_vec(&original)
        .expect("compression failed");
    (original, compressed)
}

/// Truncation at every byte offset of a real LZH stream must terminate within
/// a bounded number of `decompress` calls, and must never be reported as a
/// complete decode.
#[test]
fn truncation_at_every_offset_terminates_and_never_reports_done() {
    let (original, compressed) = sample_stream();

    for cut in 0..compressed.len() {
        let outcome = drive_to_completion(
            LzhMethod::Lh5,
            original.len() as u64,
            &compressed[..cut],
            4096,
        );
        assert_ne!(
            outcome,
            Outcome::Stalled,
            "truncation at {cut} did not terminate"
        );
        assert_ne!(
            outcome,
            Outcome::Finished,
            "truncation at {cut} reported a complete decode"
        );
    }
}

/// Single-bit corruption anywhere in the stream, and pure garbage of several
/// lengths, must terminate — with an error or a request for more input, never
/// a spin and never a panic.
#[test]
fn corrupted_and_garbage_input_terminates() {
    let (original, compressed) = sample_stream();

    for bit in (0..compressed.len() * 8).step_by(37) {
        let mut corrupt = compressed.clone();
        corrupt[bit / 8] ^= 1 << (bit % 8);
        let outcome = drive_to_completion(LzhMethod::Lh5, original.len() as u64, &corrupt, 4096);
        assert_ne!(
            outcome,
            Outcome::Stalled,
            "bit flip {bit} did not terminate"
        );
    }

    for len in [0usize, 1, 2, 3, 7, 64, 1024] {
        let garbage: Vec<u8> = (0..len)
            .map(|i| (i as u8).wrapping_mul(31) ^ 0x5A)
            .collect();
        let outcome = drive_to_completion(LzhMethod::Lh5, 1 << 20, &garbage, 4096);
        assert_ne!(
            outcome,
            Outcome::Stalled,
            "garbage of length {len} did not terminate"
        );
    }
}

/// A wildly over-declared uncompressed size (the value comes straight from an
/// untrusted LZH header) must not make the decoder allocate for it, spin, or
/// claim to have finished: it decodes what the input actually contains and
/// then asks for more input.
#[test]
fn over_declared_uncompressed_size_is_not_allocated() {
    let original = vec![b'z'; 4096];
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let compressed = encoder
        .compress_to_vec(&original)
        .expect("compression failed");

    let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, u64::MAX);
    let mut scratch = vec![0u8; 8192];
    let (_consumed, produced, mut status) = decoder
        .decompress(&compressed, &mut scratch)
        .expect("decode with an over-declared size must not fail outright");
    let mut produced_total = produced;

    let mut calls = 0usize;
    while status == DecompressStatus::NeedsOutput {
        calls += 1;
        assert!(calls < 4096, "over-declared size caused a spin");
        let (_c, p, s) = decoder
            .decompress(&[], &mut scratch)
            .expect("continuation must not fail");
        if p == 0 {
            break;
        }
        produced_total += p;
        status = s;
    }

    assert_eq!(
        status,
        DecompressStatus::NeedsInput,
        "an over-declared size must leave the decoder asking for more input"
    );
    assert_eq!(
        produced_total,
        original.len(),
        "must decode exactly what the input contained"
    );
    assert!(
        !decoder.is_finished(),
        "an over-declared size must never look finished"
    );
}

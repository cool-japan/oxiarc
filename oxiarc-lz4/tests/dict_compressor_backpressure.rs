//! Output back-pressure for [`Lz4DictCompressor`]'s [`Compressor`]
//! implementation.
//!
//! `Compressor::compress_all` drives a compressor with a fixed **32 KiB**
//! output buffer. Before 0.4.2 `Lz4DictCompressor` recompressed its whole
//! buffer on every call and returned `(input.len(), 0, NeedsOutput)` whenever
//! the frame did not fit, so it never delivered a single byte: `compress_all`
//! looped forever on any payload whose compressed frame exceeded 32 KiB (and,
//! once `compress_all` grew a no-progress guard, failed outright). The frame
//! is now staged and drained across calls.

use oxiarc_core::traits::{CompressStatus, Compressor, Decompressor, FlushMode};
use oxiarc_lz4::{Lz4Dict, Lz4DictCompressor, Lz4DictDecompressor};

/// The buffer size `compress_all` uses internally.
const CONVENIENCE_BUFFER: usize = 32 * 1024;

/// SplitMix64 output: incompressible, so the frame is guaranteed to exceed the
/// convenience buffer. A compressible fixture would never reach the defect.
fn incompressible(size: usize) -> Vec<u8> {
    let mut seed: u64 = 0x0f1e_2d3c_4b5a_6978;
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

fn dict() -> Lz4Dict {
    Lz4Dict::new(&incompressible(4096))
}

#[test]
fn dict_compress_all_handles_frames_larger_than_the_convenience_buffer() {
    let original = incompressible(200 * 1024);

    let mut encoder = Lz4DictCompressor::new(dict());
    let compressed = encoder
        .compress_all(&original)
        .expect("compress_all must deliver a frame larger than 32 KiB, not stall");
    assert!(
        compressed.len() > CONVENIENCE_BUFFER,
        "fixture must produce a frame larger than {CONVENIENCE_BUFFER} bytes; got {}",
        compressed.len()
    );
    assert!(encoder.is_finished());

    let mut decoder = Lz4DictDecompressor::new(dict());
    let decoded = decoder
        .decompress_all(&compressed)
        .expect("decompress_all failed");
    assert_eq!(decoded, original, "dictionary round-trip mismatch");
}

#[test]
fn dict_compressor_delivers_a_frame_through_a_one_byte_sink() {
    let original = incompressible(80 * 1024);

    let mut reference = Lz4DictCompressor::new(dict());
    let expected = reference
        .compress_all(&original)
        .expect("reference compress_all failed");

    let mut encoder = Lz4DictCompressor::new(dict());
    let mut byte = [0u8; 1];
    let mut out = Vec::new();
    let max_calls = expected.len() * 4 + 10_000;
    let mut calls = 0usize;
    let mut input_pos = 0usize;

    loop {
        calls += 1;
        assert!(
            calls < max_calls,
            "compressor made no progress ({calls} calls)"
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
        "byte-at-a-time delivery lost or reordered bytes"
    );
    assert!(encoder.is_finished());
}

/// `is_finished()` must stay false while the frame is still staged: it is the
/// predicate a caller uses to decide it may stop reading.
#[test]
fn dict_compressor_is_not_finished_while_output_is_staged() {
    let original = incompressible(80 * 1024);
    let mut encoder = Lz4DictCompressor::new(dict());
    let mut small = [0u8; 16];

    let (_, _, status) = encoder
        .compress(&original, &mut small, FlushMode::Finish)
        .expect("compress failed");
    assert_eq!(status, CompressStatus::NeedsOutput);
    assert!(
        !encoder.is_finished(),
        "is_finished() must stay false while staged output is undelivered"
    );
}

/// `reset()` must drop staged output so a reused compressor never prepends the
/// tail of a previous frame to the next one.
#[test]
fn dict_compressor_reset_drops_staged_output() {
    let first = incompressible(80 * 1024);
    let mut encoder = Lz4DictCompressor::new(dict());
    let mut small = [0u8; 16];
    let _ = encoder
        .compress(&first, &mut small, FlushMode::Finish)
        .expect("compress failed");
    assert!(!encoder.is_finished());

    Compressor::reset(&mut encoder);

    let second = b"a short second payload".to_vec();
    let compressed = encoder.compress_all(&second).expect("compress_all failed");
    let mut decoder = Lz4DictDecompressor::new(dict());
    assert_eq!(
        decoder.decompress_all(&compressed).expect("decompress_all"),
        second,
        "a reset compressor must produce a fresh frame, with no residue"
    );
}

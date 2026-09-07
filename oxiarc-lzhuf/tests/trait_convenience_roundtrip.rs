//! Round-trips through the [`Compressor`]/[`Decompressor`] default convenience
//! methods for every real implementation in this crate.
//!
//! `compress_all`/`decompress_all` drive the codec with a fixed **32 KiB**
//! internal buffer, so any decoder that reports `Done` before it has handed
//! back all of its decoded bytes truncates silently at exactly that size.
//! These tests use payloads well past 32 KiB (and the boundary itself) so that
//! failure mode cannot pass unnoticed again.

use oxiarc_core::traits::{Compressor, Decompressor};
use oxiarc_lzhuf::LzhMethod;
use oxiarc_lzhuf::decode::LzhDecoder;
use oxiarc_lzhuf::encode::LzhEncoder;
use oxiarc_lzhuf::streaming::StreamingLzhDecoder;

/// The buffer size `compress_all`/`decompress_all` use internally.
const CONVENIENCE_BUFFER: usize = 32 * 1024;

fn payload(size: usize) -> Vec<u8> {
    // Mildly compressible: repeated structure with a varying byte, so the
    // decoder exercises both literals and matches.
    (0..size)
        .map(|i| ((i % 97) as u8).wrapping_add((i / 4096) as u8))
        .collect()
}

fn sizes() -> Vec<usize> {
    vec![
        1,
        CONVENIENCE_BUFFER - 1,
        CONVENIENCE_BUFFER,
        CONVENIENCE_BUFFER + 1,
        5 * CONVENIENCE_BUFFER + 123,
    ]
}

#[test]
fn lzh_encoder_and_decoder_round_trip_through_convenience_methods() {
    for size in sizes() {
        let original = payload(size);

        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_all(&original)
            .unwrap_or_else(|e| panic!("compress_all failed at size {size}: {e}"));

        let mut decoder = LzhDecoder::new(LzhMethod::Lh5, original.len() as u64);
        let decoded = decoder
            .decompress_all(&compressed)
            .unwrap_or_else(|e| panic!("decompress_all failed at size {size}: {e}"));

        assert_eq!(
            decoded.len(),
            original.len(),
            "size {size}: decompress_all returned {} bytes (silent truncation?)",
            decoded.len()
        );
        assert_eq!(decoded, original, "size {size}: round-trip mismatch");
    }
}

#[test]
fn streaming_lzh_decoder_round_trips_through_decompress_all() {
    for size in sizes() {
        let original = payload(size);

        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = encoder
            .compress_to_vec(&original)
            .unwrap_or_else(|e| panic!("compression failed at size {size}: {e}"));

        let mut decoder = StreamingLzhDecoder::new(LzhMethod::Lh5, original.len() as u64);
        let decoded = Decompressor::decompress_all(&mut decoder, &compressed)
            .unwrap_or_else(|e| panic!("decompress_all failed at size {size}: {e}"));

        assert_eq!(
            decoded.len(),
            original.len(),
            "size {size}: streaming decompress_all returned {} bytes",
            decoded.len()
        );
        assert_eq!(decoded, original, "size {size}: round-trip mismatch");
    }
}

/// A decoder that has handed back everything must keep reporting `Done` (and
/// zero bytes) rather than restarting or erroring, so repeated calls after
/// completion are harmless.
#[test]
fn lzh_decoder_is_idempotent_once_drained() {
    let original = payload(3 * CONVENIENCE_BUFFER);
    let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
    let compressed = encoder.compress_to_vec(&original).expect("compress");

    let mut decoder = LzhDecoder::new(LzhMethod::Lh5, original.len() as u64);
    let decoded = decoder.decompress_all(&compressed).expect("decompress_all");
    assert_eq!(decoded, original);

    let mut scratch = [0u8; 64];
    for _ in 0..3 {
        let (consumed, produced, status) = decoder
            .decompress(&[], &mut scratch)
            .expect("post-completion call must not fail");
        assert_eq!((consumed, produced), (0, 0));
        assert_eq!(status, oxiarc_core::DecompressStatus::Done);
    }
    assert!(decoder.is_finished());
}

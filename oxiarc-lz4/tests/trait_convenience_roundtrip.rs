//! Round-trips through the [`Compressor`]/[`Decompressor`] default convenience
//! methods for every real implementation in this crate.
//!
//! `compress_all`/`decompress_all` drive the codec with a fixed **32 KiB**
//! internal buffer, so a decoder that reports `Done` while it still holds
//! staged output truncates silently at exactly that size. The payloads below
//! straddle and exceed that boundary so the failure mode cannot pass
//! unnoticed again.

use oxiarc_core::traits::{Compressor, Decompressor};
use oxiarc_lz4::{Lz4Compressor, Lz4Decompressor, Lz4Dict, Lz4DictCompressor, Lz4DictDecompressor};

/// The buffer size `compress_all`/`decompress_all` use internally.
const CONVENIENCE_BUFFER: usize = 32 * 1024;

fn payload(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| ((i % 251) as u8).wrapping_add((i / 8192) as u8))
        .collect()
}

fn sizes() -> Vec<usize> {
    vec![
        1,
        CONVENIENCE_BUFFER - 1,
        CONVENIENCE_BUFFER,
        CONVENIENCE_BUFFER + 1,
        6 * CONVENIENCE_BUFFER + 77,
    ]
}

#[test]
fn frame_compressor_and_decompressor_round_trip_through_convenience_methods() {
    for size in sizes() {
        let original = payload(size);

        let mut encoder = Lz4Compressor::new();
        let compressed = encoder
            .compress_all(&original)
            .unwrap_or_else(|e| panic!("compress_all failed at size {size}: {e}"));

        let mut decoder = Lz4Decompressor::new();
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
fn dict_compressor_and_decompressor_round_trip_through_convenience_methods() {
    let dict_bytes = payload(4096);
    for size in sizes() {
        let original = payload(size);

        let mut encoder = Lz4DictCompressor::new(Lz4Dict::new(&dict_bytes));
        let compressed = encoder
            .compress_all(&original)
            .unwrap_or_else(|e| panic!("dict compress_all failed at size {size}: {e}"));

        let mut decoder = Lz4DictDecompressor::new(Lz4Dict::new(&dict_bytes));
        let decoded = decoder
            .decompress_all(&compressed)
            .unwrap_or_else(|e| panic!("dict decompress_all failed at size {size}: {e}"));

        assert_eq!(
            decoded.len(),
            original.len(),
            "size {size}: dict decompress_all returned {} bytes (silent truncation?)",
            decoded.len()
        );
        assert_eq!(decoded, original, "size {size}: round-trip mismatch");
    }
}

/// Once drained, both decoders must keep answering `Done` with zero bytes.
#[test]
fn decoders_are_idempotent_once_drained() {
    let original = payload(4 * CONVENIENCE_BUFFER);
    let mut encoder = Lz4Compressor::new();
    let compressed = encoder.compress_all(&original).expect("compress_all");

    let mut decoder = Lz4Decompressor::new();
    assert_eq!(
        decoder.decompress_all(&compressed).expect("decompress_all"),
        original
    );

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

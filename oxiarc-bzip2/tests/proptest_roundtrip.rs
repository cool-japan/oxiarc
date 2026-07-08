//! Property-based round-trip and no-panic tests for the BZip2 codec.
//!
//! BZip2's Burrows-Wheeler Transform is `O(n log n)` or worse, so inputs and
//! case counts are kept intentionally small to keep this suite fast while
//! still exercising the encode/decode path across arbitrary data.

use oxiarc_bzip2::{CompressionLevel, compress, decompress};
use proptest::prelude::*;
use std::io::Cursor;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// `decompress(compress(data, level)) == data` for arbitrary byte
    /// vectors at any valid compression level.
    #[test]
    fn roundtrip(
        data in prop::collection::vec(any::<u8>(), 0..512),
        level in 1u8..=9,
    ) {
        let compressed = compress(&data, CompressionLevel::new(level))
            .expect("bzip2 compress must not fail on valid input");
        let decompressed = decompress(Cursor::new(compressed.as_slice()))
            .expect("bzip2 decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress` must
    /// only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decompress_never_panics(data in prop::collection::vec(any::<u8>(), 0..512)) {
        let result = std::panic::catch_unwind(|| decompress(Cursor::new(data.as_slice())));
        prop_assert!(result.is_ok(), "decompress() must not panic on arbitrary input");
    }
}

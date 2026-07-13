//! Property-based round-trip and no-panic tests for the Snappy codec.

use oxiarc_snappy::{compress, decompress};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `decompress(compress(data)) == data` for arbitrary byte vectors.
    ///
    /// Snappy's `compress` is infallible (it never fails on any input), so
    /// only the decompression side returns a `Result`.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..8192)) {
        let compressed = compress(&data);
        let decompressed = decompress(&compressed)
            .expect("snappy decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress` must
    /// only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decompress_never_panics(data in prop::collection::vec(any::<u8>(), 0..8192)) {
        let result = std::panic::catch_unwind(|| decompress(&data));
        prop_assert!(result.is_ok(), "decompress() must not panic on arbitrary input");
    }
}

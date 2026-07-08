//! Property-based round-trip and no-panic tests for the Brotli codec.

use oxiarc_brotli::{compress, decompress};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// `decompress(compress(data, quality)) == data` for arbitrary byte
    /// vectors at any valid quality level.
    #[test]
    fn roundtrip(
        data in prop::collection::vec(any::<u8>(), 0..2048),
        quality in 0u32..=11,
    ) {
        let compressed = compress(&data, quality).expect("brotli compress must not fail on valid input");
        let decompressed = decompress(&compressed)
            .expect("brotli decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress` must
    /// only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decompress_never_panics(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let result = std::panic::catch_unwind(|| decompress(&data));
        prop_assert!(result.is_ok(), "decompress() must not panic on arbitrary input");
    }
}

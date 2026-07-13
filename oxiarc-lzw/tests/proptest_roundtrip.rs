//! Property-based round-trip and no-panic tests for the LZW codec.

use oxiarc_lzw::{LzwConfig, compress, decompress};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `decompress(compress(data, TIFF), data.len(), TIFF) == data` for
    /// arbitrary byte vectors.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let compressed = compress(&data, LzwConfig::TIFF)
            .expect("lzw compress must not fail on valid input");
        let decompressed = decompress(&compressed, data.len(), LzwConfig::TIFF)
            .expect("lzw decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress` must
    /// only ever return `Ok` or `Err`, never panic, regardless of the
    /// requested expected output size.
    #[test]
    fn decompress_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        expected_size in 0usize..4096,
    ) {
        let result = std::panic::catch_unwind(|| decompress(&data, expected_size, LzwConfig::TIFF));
        prop_assert!(result.is_ok(), "decompress() must not panic on arbitrary input");
    }
}

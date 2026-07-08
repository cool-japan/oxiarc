//! Property-based round-trip and no-panic tests for the Zstandard codec.

use oxiarc_zstd::{compress, decompress};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// `decompress(compress(data)) == data` for arbitrary byte vectors.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let compressed = compress(&data).expect("zstd compress must not fail on valid input");
        let decompressed = decompress(&compressed)
            .expect("zstd decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress` must
    /// only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decompress_never_panics(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let result = std::panic::catch_unwind(|| decompress(&data));
        prop_assert!(result.is_ok(), "decompress() must not panic on arbitrary input");
    }
}

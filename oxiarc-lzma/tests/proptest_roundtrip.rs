//! Property-based round-trip and no-panic tests for the LZMA codec.

use oxiarc_lzma::{compress_bytes, decompress_bytes};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// `decompress_bytes(compress_bytes(data)) == data` for arbitrary byte
    /// vectors, using the default compression level.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let compressed = compress_bytes(&data).expect("lzma compress must not fail on valid input");
        let decompressed = decompress_bytes(&compressed)
            .expect("lzma decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress_bytes`
    /// must only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decompress_never_panics(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let result = std::panic::catch_unwind(|| decompress_bytes(&data));
        prop_assert!(result.is_ok(), "decompress_bytes() must not panic on arbitrary input");
    }
}

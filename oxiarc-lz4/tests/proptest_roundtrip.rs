//! Property-based round-trip and no-panic tests for the LZ4 block codec.

use oxiarc_lz4::{compress_bytes, decompress_bytes};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `decompress_bytes(compress_bytes(data), data.len()) == data` for
    /// arbitrary byte vectors.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..8192)) {
        let compressed = compress_bytes(&data).expect("lz4 compress must not fail on valid input");
        let decompressed = decompress_bytes(&compressed, data.len())
            .expect("lz4 decompress must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decompress_bytes`
    /// must only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decompress_never_panics(data in prop::collection::vec(any::<u8>(), 0..8192)) {
        // Bound the requested output size generously but not unboundedly, so
        // a malformed length field cannot trigger an out-of-memory abort
        // instead of a clean error return.
        let max_output = data.len().saturating_mul(8).saturating_add(64);
        let result = std::panic::catch_unwind(|| decompress_bytes(&data, max_output));
        prop_assert!(result.is_ok(), "decompress_bytes() must not panic on arbitrary input");
    }
}

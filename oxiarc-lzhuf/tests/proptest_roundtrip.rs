//! Property-based round-trip and no-panic tests for the LZH (LZSS + Huffman)
//! codec.

use oxiarc_lzhuf::{LzhMethod, decode_lzh, encode_lzh};
use proptest::prelude::*;

/// All `LzhMethod` variants that represent actual data-carrying compression
/// methods (`Lhd` is a directory-entry marker with no payload, so it is
/// excluded here).
fn data_method() -> impl Strategy<Value = LzhMethod> {
    prop_oneof![
        Just(LzhMethod::Lh0),
        Just(LzhMethod::Lh1),
        Just(LzhMethod::Lh4),
        Just(LzhMethod::Lh5),
        Just(LzhMethod::Lh6),
        Just(LzhMethod::Lh7),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// `decode_lzh(encode_lzh(data, method), method, data.len()) == data`
    /// for arbitrary byte vectors under every data-carrying LZH method.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096), method in data_method()) {
        let compressed = encode_lzh(&data, method)
            .expect("lzh encode must not fail on valid input");
        let decompressed = decode_lzh(&compressed, method, data.len() as u64)
            .expect("lzh decode must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decode_lzh` must
    /// only ever return `Ok` or `Err`, never panic.
    #[test]
    fn decode_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        method in data_method(),
        uncompressed_size in 0u64..4096,
    ) {
        let result = std::panic::catch_unwind(|| decode_lzh(&data, method, uncompressed_size));
        prop_assert!(result.is_ok(), "decode_lzh() must not panic on arbitrary input");
    }
}

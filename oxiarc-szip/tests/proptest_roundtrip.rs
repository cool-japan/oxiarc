//! Property-based round-trip and no-panic tests for the AEC/SZIP codec.

use oxiarc_szip::{SzipParams, decode, encode_bytes};
use proptest::prelude::*;

/// Byte-oriented (bpp = 8) parameters sized to the given sample count.
fn byte_params(samples: usize) -> SzipParams {
    SzipParams {
        samples,
        ..SzipParams::default()
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// `decode(encode_bytes(data, params), params) == data` for arbitrary
    /// byte vectors under the default byte-oriented (bpp = 8) parameters.
    #[test]
    fn roundtrip(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let params = byte_params(data.len());
        let compressed = encode_bytes(&data, &params)
            .expect("szip encode must not fail on valid input");
        let decompressed = decode(&compressed, &params)
            .expect("szip decode must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `decode` must only
    /// ever return `Ok` or `Err`, never panic, for any sample count the
    /// caller might request.
    #[test]
    fn decode_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        samples in 0usize..4096,
    ) {
        let params = byte_params(samples);
        let result = std::panic::catch_unwind(|| decode(&data, &params));
        prop_assert!(result.is_ok(), "decode() must not panic on arbitrary input");
    }
}

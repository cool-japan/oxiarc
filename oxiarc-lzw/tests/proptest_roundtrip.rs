//! Property-based round-trip and no-panic tests for the LZW codec.

use oxiarc_lzw::{LzwConfig, compress, decompress, decompress_into};
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

    /// `decompress_into` must reproduce the input exactly, for the TIFF
    /// rule and for the old-style (`early_change = false`) rule.
    #[test]
    fn roundtrip_into(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        old_style in any::<bool>(),
    ) {
        let config = if old_style { LzwConfig::TIFF_OLD_STYLE } else { LzwConfig::TIFF };
        let compressed = compress(&data, config)
            .expect("lzw compress must not fail on valid input");
        let mut out = vec![0u8; data.len()];
        let written = decompress_into(&compressed, &mut out, config)
            .expect("lzw decompress_into must not fail on data we just produced");
        prop_assert_eq!(written, data.len());
        prop_assert_eq!(out, data);
    }

    /// `decompress_into` and `decompress` must agree byte-for-byte for any
    /// output-buffer size, including sizes that cut a code's expansion in
    /// half, and `decompress_into` must never write past its buffer.
    #[test]
    fn into_matches_vec_for_any_buffer_size(
        data in prop::collection::vec(any::<u8>(), 0..2048),
        limit in 0usize..2048,
    ) {
        let compressed = compress(&data, LzwConfig::TIFF)
            .expect("lzw compress must not fail on valid input");
        let limit = limit.min(data.len());
        let via_vec = decompress(&compressed, limit, LzwConfig::TIFF)
            .expect("vec decode of our own stream");
        let mut out = vec![0xA5u8; limit + 8];
        let written = decompress_into(&compressed, &mut out[..limit], LzwConfig::TIFF)
            .expect("into decode of our own stream");
        prop_assert_eq!(written, limit);
        prop_assert_eq!(&out[..limit], &via_vec[..]);
        prop_assert!(out[limit..].iter().all(|&b| b == 0xA5), "wrote past the buffer");
    }

    /// Arbitrary bytes into `decompress_into` must only ever return
    /// `Ok`/`Err`, never panic and never overrun the destination.
    #[test]
    fn decompress_into_never_panics(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        dst_len in 0usize..4096,
    ) {
        let result = std::panic::catch_unwind(move || {
            let mut out = vec![0u8; dst_len];
            decompress_into(&data, &mut out, LzwConfig::TIFF).map(|n| n <= dst_len)
        });
        prop_assert!(result.is_ok(), "decompress_into() must not panic on arbitrary input");
        if let Ok(Ok(within_bounds)) = result {
            prop_assert!(within_bounds, "decompress_into() reported more bytes than the buffer holds");
        }
    }
}

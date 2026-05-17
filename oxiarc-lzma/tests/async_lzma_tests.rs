//! Integration tests for async LZMA2 compression and decompression.
//!
//! These tests are gated on the `async-io` feature.

#[cfg(feature = "async-io")]
mod tests {
    use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
    use oxiarc_lzma::{Lzma2Decoder, Lzma2Encoder, LzmaLevel, decode_lzma2, encode_lzma2};
    use std::io::Cursor;

    /// Generate highly compressible data of `size` bytes (cycling byte pattern).
    ///
    /// Using compressible data ensures LZMA always takes the compressed path,
    /// avoiding the encoder's uncompressed-chunk path which has a 64 KiB size limit.
    fn make_compressible(size: usize) -> Vec<u8> {
        // Cycling 64-byte ramp: very compressible at all levels.
        (0..size).map(|i| (i % 64) as u8).collect()
    }

    /// 512 KiB input, async encode + async decode, level 1.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_roundtrip_level_1() {
        let original = make_compressible(512 * 1024);

        let mut encoder = Lzma2Encoder::new(LzmaLevel::new(1));
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        let compressed_len = encoder
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress level 1 failed");

        assert!(compressed_len > 0);
        assert!(!compressed.is_empty());

        let dict_size = LzmaLevel::new(1).dict_size();
        let mut decoder = Lzma2Decoder::new(dict_size);
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_len = decoder
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress level 1 failed");

        assert_eq!(decompressed_len, original.len());
        assert_eq!(decompressed, original);
    }

    /// 512 KiB input, async encode + async decode, level 5.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_roundtrip_level_5() {
        let original = make_compressible(512 * 1024);

        let mut encoder = Lzma2Encoder::new(LzmaLevel::new(5));
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        let compressed_len = encoder
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress level 5 failed");

        assert!(compressed_len > 0);

        let dict_size = LzmaLevel::new(5).dict_size();
        let mut decoder = Lzma2Decoder::new(dict_size);
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_len = decoder
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress level 5 failed");

        assert_eq!(decompressed_len, original.len());
        assert_eq!(decompressed, original);
    }

    /// 64 KiB input (smaller to keep test fast), async encode + async decode, level 9.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_roundtrip_level_9() {
        // Use smaller input (~64 KiB) to keep test fast — level 9 LZMA is slow.
        let original = make_compressible(64 * 1024);

        let mut encoder = Lzma2Encoder::new(LzmaLevel::new(9));
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        let compressed_len = encoder
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress level 9 failed");

        assert!(compressed_len > 0);

        let dict_size = LzmaLevel::new(9).dict_size();
        let mut decoder = Lzma2Decoder::new(dict_size);
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_len = decoder
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress level 9 failed");

        assert_eq!(decompressed_len, original.len());
        assert_eq!(decompressed, original);
    }

    /// Serial encode → async decode → verify identical output.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_decode_serial_output() {
        let original = make_compressible(128 * 1024);

        // Serial encode
        let compressed = encode_lzma2(&original, LzmaLevel::new(3)).expect("serial encode failed");

        // Async decode
        let dict_size = LzmaLevel::new(3).dict_size();
        let mut decoder = Lzma2Decoder::new(dict_size);
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_len = decoder
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decode of serial output failed");

        assert_eq!(decompressed_len, original.len());
        assert_eq!(decompressed, original);
    }

    /// Async encode → serial decode → verify identical output.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_encode_serial_decode() {
        let original = make_compressible(128 * 1024);

        // Async encode
        let mut encoder = Lzma2Encoder::new(LzmaLevel::new(3));
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        encoder
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async encode failed");

        // Serial decode
        let dict_size = LzmaLevel::new(3).dict_size();
        let decompressed =
            decode_lzma2(&compressed, dict_size).expect("serial decode of async output failed");

        assert_eq!(decompressed, original);
    }

    /// Empty input roundtrips without error.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_async_empty() {
        let mut encoder = Lzma2Encoder::new(LzmaLevel::new(1));
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut compressed = Vec::new();

        encoder
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async encode of empty input failed");

        assert!(
            !compressed.is_empty(),
            "encoded empty should produce at least end marker"
        );

        // The LZMA2 empty stream is just the 0x00 end marker (1 byte).
        assert_eq!(compressed, vec![0x00]);

        // Async decode the empty LZMA2 stream
        let dict_size = LzmaLevel::new(1).dict_size();
        let mut decoder = Lzma2Decoder::new(dict_size);
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_len = decoder
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decode of empty stream failed");

        assert_eq!(decompressed_len, 0);
        assert!(decompressed.is_empty());
    }
}

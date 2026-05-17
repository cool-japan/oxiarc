//! Integration tests for async Snappy I/O support.
//!
//! These tests require the `async-io` feature to be enabled.

#[cfg(feature = "async-io")]
mod tests {
    use oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor};
    use oxiarc_snappy::async_snappy::{compress_frame_async, decompress_frame_async};
    use oxiarc_snappy::{
        AsyncSnappyCompressor, AsyncSnappyDecompressor, FrameDecoder, FrameEncoder,
    };
    use std::io::{Cursor, Read, Write};

    /// Compress data synchronously using FrameEncoder, return compressed bytes.
    fn encode_serial(data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut enc = FrameEncoder::new(&mut buf);
            enc.write_all(data).expect("serial encode write failed");
            enc.finish().expect("serial encode finish failed");
        }
        buf
    }

    /// Decompress data synchronously using FrameDecoder.
    fn decode_serial(data: &[u8]) -> Vec<u8> {
        let mut decoder = FrameDecoder::new(Cursor::new(data));
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("serial decode failed");
        out
    }

    /// Test that async compress → async decompress round-trips correctly for 256 KiB of data.
    #[tokio::test]
    async fn test_async_roundtrip() {
        let original: Vec<u8> = (0u32..256 * 1024).map(|i| (i % 251) as u8).collect();

        // Compress
        let mut compressor = AsyncSnappyCompressor;
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        let compressed_bytes = compressor
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress failed");

        assert!(
            compressed_bytes > 0,
            "compressed output should be non-empty"
        );
        assert!(!compressed.is_empty(), "compressed vec should be non-empty");

        // Decompress
        let mut decompressor = AsyncSnappyDecompressor;
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        let decompressed_bytes = decompressor
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress failed");

        assert_eq!(
            decompressed_bytes,
            original.len(),
            "decompressed size mismatch"
        );
        assert_eq!(decompressed, original, "roundtrip data mismatch");
    }

    /// Test that serial FrameEncoder output can be decoded by async decompressor.
    #[tokio::test]
    async fn test_async_decode_serial_output() {
        let original: Vec<u8> = (0u32..50_000).map(|i| (i % 199) as u8).collect();

        // Encode with serial FrameEncoder
        let compressed = encode_serial(&original);

        // Decode with async decompressor
        let mut decompressor = AsyncSnappyDecompressor;
        let mut input = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        decompressor
            .decompress_async(&mut input, &mut decompressed)
            .await
            .expect("async decompress of serial output failed");

        assert_eq!(
            decompressed, original,
            "async decode of serial output mismatch"
        );
    }

    /// Test that async compressor output can be decoded by serial FrameDecoder.
    #[tokio::test]
    async fn test_async_encode_serial_decode() {
        let original: Vec<u8> = (0u32..50_000).map(|i| (i % 197) as u8).collect();

        // Encode with async compressor
        let mut compressor = AsyncSnappyCompressor;
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        compressor
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress failed");

        // Decode with serial FrameDecoder
        let decompressed = decode_serial(&compressed);

        assert_eq!(
            decompressed, original,
            "serial decode of async output mismatch"
        );
    }

    /// Test that compress_parallel output (framing-compatible) can be decoded by async decompressor.
    #[cfg(feature = "parallel")]
    #[tokio::test]
    async fn test_async_parallel_output_decodable() {
        use oxiarc_snappy::compress_parallel;

        let original: Vec<u8> = (0u32..200_000).map(|i| (i % 251) as u8).collect();

        // Compress with parallel compressor (produces Snappy-framed output)
        let compressed = compress_parallel(&original);

        // Decode with async decompressor
        let mut decompressor = AsyncSnappyDecompressor;
        let mut input = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        decompressor
            .decompress_async(&mut input, &mut decompressed)
            .await
            .expect("async decompress of parallel output failed");

        assert_eq!(
            decompressed, original,
            "async decode of parallel output mismatch"
        );
    }

    /// Test that empty input compresses and decompresses to an empty output.
    #[tokio::test]
    async fn test_async_empty() {
        // Compress empty input
        let mut compressor = AsyncSnappyCompressor;
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut compressed = Vec::new();

        compressor
            .compress_async(&mut input, &mut compressed)
            .await
            .expect("async compress of empty input should succeed");

        // Decompress back
        let mut decompressor = AsyncSnappyDecompressor;
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        decompressor
            .decompress_async(&mut comp_cursor, &mut decompressed)
            .await
            .expect("async decompress of empty input should succeed");

        assert!(
            decompressed.is_empty(),
            "decompressed empty input must be empty"
        );
    }

    /// Test the free function `compress_frame_async` and `decompress_frame_async`.
    #[tokio::test]
    async fn test_free_functions_roundtrip() {
        let original = b"Hello from oxiarc-snappy async free functions!".repeat(1000);

        // Compress
        let input = Cursor::new(original.clone());
        let mut compressed = Vec::new();
        compress_frame_async(input, &mut compressed)
            .await
            .expect("compress_frame_async failed");

        assert!(
            !compressed.is_empty(),
            "compress_frame_async must produce output"
        );

        // Decompress
        let comp_input = Cursor::new(compressed);
        let mut decompressed = Vec::new();
        decompress_frame_async(comp_input, &mut decompressed)
            .await
            .expect("decompress_frame_async failed");

        assert_eq!(
            decompressed,
            original.as_slice(),
            "free function roundtrip mismatch"
        );
    }

    /// Test with custom buffer sizes (exercises compress_async_with_buffer /
    /// decompress_async_with_buffer code paths).
    #[tokio::test]
    async fn test_async_with_custom_buffer() {
        let original: Vec<u8> = (0u32..1024).map(|i| (i % 127) as u8).collect();

        // Compress with small buffer
        let mut compressor = AsyncSnappyCompressor;
        let mut input = Cursor::new(original.clone());
        let mut compressed = Vec::new();

        compressor
            .compress_async_with_buffer(&mut input, &mut compressed, 128)
            .await
            .expect("async compress with small buffer failed");

        // Decompress with small buffer
        let mut decompressor = AsyncSnappyDecompressor;
        let mut comp_cursor = Cursor::new(compressed);
        let mut decompressed = Vec::new();

        decompressor
            .decompress_async_with_buffer(&mut comp_cursor, &mut decompressed, 128)
            .await
            .expect("async decompress with small buffer failed");

        assert_eq!(decompressed, original, "custom buffer roundtrip mismatch");
    }
}

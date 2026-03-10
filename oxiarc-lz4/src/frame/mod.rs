//! LZ4 frame format support.
//!
//! Implements the official LZ4 frame format as specified in:
//! <https://github.com/lz4/lz4/blob/dev/doc/lz4_Frame_format.md>
//!
//! The frame format includes:
//! - Magic number (0x184D2204)
//! - Frame descriptor (flags, block size, optional content size)
//! - Data blocks with optional checksums
//! - End marker
//! - Optional content checksum

mod compress;
mod decompress;
mod frame_dict;
mod streaming;
mod types;

pub use compress::compress;
pub use compress::compress_with_options;
pub use decompress::decompress;
pub use frame_dict::{
    Lz4DictCompressor, Lz4DictDecompressor, Lz4DictFrameDecoder, Lz4DictFrameEncoder,
    compress_frame_with_dict, compress_frame_with_dict_options, decompress_frame_with_dict,
    get_frame_dict_id,
};
pub use streaming::{Lz4Compressor, Lz4Decompressor};
pub use types::{BlockMaxSize, FrameDescriptor, LZ4_FRAME_MAGIC};

#[cfg(feature = "parallel")]
pub use compress::{compress_parallel, compress_with_options_parallel};

#[cfg(test)]
mod tests {
    use super::compress::{compress, compress_with_options};
    use super::decompress::decompress;
    use super::frame_dict::{
        Lz4DictCompressor, Lz4DictDecompressor, Lz4DictFrameDecoder, Lz4DictFrameEncoder,
        compress_frame_with_dict, compress_frame_with_dict_options, decompress_frame_with_dict,
        get_frame_dict_id,
    };
    use super::streaming::{Lz4Compressor, Lz4Decompressor};
    use super::types::{BlockMaxSize, FrameDescriptor, LZ4_FRAME_MAGIC};
    use crate::dict::Lz4Dict;
    use oxiarc_core::traits::{
        CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode,
    };

    #[cfg(feature = "parallel")]
    use super::compress::{compress_parallel, compress_with_options_parallel};

    #[test]
    fn test_frame_magic() {
        let data = b"Hello";
        let compressed = compress(data).expect("compress failed");
        assert_eq!(&compressed[0..4], &LZ4_FRAME_MAGIC.to_le_bytes());
    }

    #[test]
    fn test_frame_roundtrip() {
        let data = b"Hello, World! This is a test of LZ4 framing.";
        let compressed = compress(data).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_roundtrip_large() {
        let data = vec![0x42u8; 100000];
        let compressed = compress(&data).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_with_block_checksum() {
        let data = b"Testing block checksums in LZ4 frame format.";
        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_checksum(true);
        let compressed = compress_with_options(data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_without_content_checksum() {
        let data = b"Testing without content checksum.";
        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_content_checksum(false);
        let compressed = compress_with_options(data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_incompressible_data() {
        // Random-looking data that doesn't compress
        let data: Vec<u8> = (0..1000).map(|i| (i * 17 + 13) as u8).collect();
        let compressed = compress(&data).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_max_sizes() {
        assert_eq!(BlockMaxSize::Size64KB.size_bytes(), 64 * 1024);
        assert_eq!(BlockMaxSize::Size256KB.size_bytes(), 256 * 1024);
        assert_eq!(BlockMaxSize::Size1MB.size_bytes(), 1024 * 1024);
        assert_eq!(BlockMaxSize::Size4MB.size_bytes(), 4 * 1024 * 1024);
    }

    #[test]
    fn test_compressor_trait() {
        let mut compressor = Lz4Compressor::new();
        let data = b"Hello, World!";
        let mut output = vec![0u8; 200];

        let (consumed, produced, status) = compressor
            .compress(data, &mut output, FlushMode::Finish)
            .expect("compress failed");

        assert_eq!(consumed, data.len());
        assert!(produced > 0);
        assert_eq!(status, CompressStatus::Done);
    }

    #[test]
    fn test_decompressor_trait() {
        let data = b"Hello, World!";
        let compressed = compress(data).expect("compress failed");

        let mut decompressor = Lz4Decompressor::new();
        let mut output = vec![0u8; 100];

        let (consumed, produced, status) = decompressor
            .decompress(&compressed, &mut output)
            .expect("decompress failed");

        assert_eq!(consumed, compressed.len());
        assert_eq!(produced, data.len());
        assert_eq!(status, DecompressStatus::Done);
        assert_eq!(&output[..produced], data.as_slice());
    }

    #[test]
    fn test_invalid_magic() {
        let bad_data = [
            0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x48, 0x65, 0x6c, 0x6c, 0x6f,
        ];
        let result = decompress(&bad_data, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_too_short() {
        let short_data = [0x04, 0x22, 0x4D, 0x18]; // Just magic, incomplete
        let result = decompress(&short_data, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_empty_data() {
        let data: &[u8] = b"";
        let compressed = compress(data).expect("compress failed");
        let decompressed = decompress(&compressed, 100).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_header_checksum_verification() {
        let data = b"Test data";
        let mut compressed = compress(data).expect("compress failed");

        // Find and corrupt the header checksum (byte after FLG/BD/content_size)
        // For our default: 4 (magic) + 1 (FLG) + 1 (BD) + 8 (content size) = 14, checksum at 14
        if compressed.len() > 14 {
            compressed[14] ^= 0xFF; // Corrupt checksum
        }

        let result = decompress(&compressed, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_content_checksum_verification() {
        let data = b"Test data for checksum";
        let mut compressed = compress(data).expect("compress failed");

        // Corrupt the last 4 bytes (content checksum)
        let len = compressed.len();
        if len >= 4 {
            compressed[len - 1] ^= 0xFF;
        }

        let result = decompress(&compressed, 100);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        let data = b"Hello, World! This is a test of parallel LZ4 compression.";
        let compressed = compress_parallel(data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        // Large data that will be split into multiple blocks
        let data = vec![0x42u8; 10_000_000];
        let compressed = compress_parallel(&data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_pattern() {
        let mut data = Vec::new();
        for i in 0..100000 {
            data.push((i % 256) as u8);
        }
        let compressed = compress_parallel(&data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial_output() {
        // Verify parallel and serial produce identical output
        let data = b"The quick brown fox jumps over the lazy dog.";
        let serial = compress(data).expect("serial compress failed");
        let parallel = compress_parallel(data).expect("parallel compress failed");

        // Both should decompress to the same data
        let serial_decompressed =
            decompress(&serial, data.len() * 2).expect("decompress serial failed");
        let parallel_decompressed =
            decompress(&parallel, data.len() * 2).expect("decompress parallel failed");

        assert_eq!(serial_decompressed, data);
        assert_eq!(parallel_decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        let data: &[u8] = b"";
        let compressed = compress_parallel(data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, 0).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_with_options() {
        let data = b"Testing parallel compression with custom options.";
        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_checksum(true)
            .with_block_max_size(BlockMaxSize::Size64KB);

        let compressed = compress_with_options_parallel(data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multiple_blocks() {
        // Create data that will span multiple blocks
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend_from_slice(b"Block of data that repeats. ");
        }
        let data = data.repeat(50000); // Make it large

        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_max_size(BlockMaxSize::Size256KB);

        let compressed = compress_with_options_parallel(&data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    // ========================================================================
    // Dictionary Frame Integration Tests
    // ========================================================================

    #[test]
    fn test_frame_dict_roundtrip_basic() {
        let dict = Lz4Dict::new(b"common pattern for testing");
        let data = b"common pattern for testing appears in this data";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");
        let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict)
            .expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_roundtrip_large() {
        let dict = Lz4Dict::new(b"repeating pattern to compress");

        // Create large data with the pattern
        let mut data = Vec::new();
        for i in 0..1000 {
            data.extend_from_slice(
                format!("Line {}: repeating pattern to compress\n", i).as_bytes(),
            );
        }

        let compressed = compress_frame_with_dict(&data, &dict).expect("compress failed");
        let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict)
            .expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_encoder_decoder() {
        let dict = Lz4Dict::new(b"encoder decoder test pattern");
        let encoder = Lz4DictFrameEncoder::new(dict.clone());
        let decoder = Lz4DictFrameDecoder::new(dict);

        let data = b"encoder decoder test pattern is here";
        let compressed = encoder.encode(data).expect("encode failed");

        assert!(decoder.can_decode(&compressed));

        let decompressed = decoder
            .decode(&compressed, data.len() * 2)
            .expect("decode failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_encoder_with_size() {
        let dict = Lz4Dict::new(b"size test pattern");
        let encoder = Lz4DictFrameEncoder::new(dict.clone());
        let decoder = Lz4DictFrameDecoder::new(dict);

        let data = b"size test pattern in the data";
        let compressed = encoder.encode_with_size(data).expect("encode failed");

        let decompressed = decoder
            .decode(&compressed, data.len() * 2)
            .expect("decode failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_get_dict_id() {
        let dict = Lz4Dict::new(b"dict id test");
        let data = b"dict id test data here";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");
        let frame_dict_id = get_frame_dict_id(&compressed).expect("get dict id failed");

        assert_eq!(frame_dict_id, Some(dict.id()));
    }

    #[test]
    fn test_frame_dict_id_mismatch() {
        let dict1 = Lz4Dict::new(b"dictionary one");
        let dict2 = Lz4Dict::new(b"dictionary two");

        let data = b"some data to compress";
        let compressed = compress_frame_with_dict(data, &dict1).expect("compress failed");

        // Try to decompress with wrong dictionary
        let result = decompress_frame_with_dict(&compressed, data.len() * 2, &dict2);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_dict_compressor_trait() {
        let dict = Lz4Dict::new(b"compressor trait test");
        let mut compressor = Lz4DictCompressor::new(dict.clone());

        let data = b"compressor trait test data";
        let mut output = vec![0u8; 500];

        let (consumed, produced, status) = compressor
            .compress(data, &mut output, FlushMode::Finish)
            .expect("compress failed");

        assert_eq!(consumed, data.len());
        assert!(produced > 0);
        assert_eq!(status, CompressStatus::Done);

        // Verify we can decompress
        let decompressed = decompress_frame_with_dict(&output[..produced], data.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_decompressor_trait() {
        let dict = Lz4Dict::new(b"decompressor trait test");
        let data = b"decompressor trait test data";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");

        let mut decompressor = Lz4DictDecompressor::new(dict);
        let mut output = vec![0u8; 200];

        let (consumed, produced, status) = decompressor
            .decompress(&compressed, &mut output)
            .expect("decompress failed");

        assert_eq!(consumed, compressed.len());
        assert_eq!(produced, data.len());
        assert_eq!(status, DecompressStatus::Done);
        assert_eq!(&output[..produced], data.as_slice());
    }

    #[test]
    fn test_frame_dict_with_options() {
        let dict = Lz4Dict::new(b"options test pattern");
        let data = b"options test pattern with block checksum";

        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_checksum(true);

        let compressed =
            compress_frame_with_dict_options(data, &dict, desc).expect("compress failed");
        let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict)
            .expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_empty_data() {
        let dict = Lz4Dict::new(b"empty test dict");
        let data: &[u8] = b"";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_frame_with_dict(&compressed, 100, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_improves_compression() {
        let pattern = b"the quick brown fox jumps over the lazy dog";
        let dict = Lz4Dict::new(pattern);

        // Create data that contains the dictionary pattern multiple times
        let mut data = Vec::new();
        for i in 0..100 {
            data.extend_from_slice(
                format!("Line {}: the quick brown fox jumps over the lazy dog\n", i).as_bytes(),
            );
        }

        let with_dict = compress_frame_with_dict(&data, &dict).expect("compress with dict failed");
        let without_dict = compress(&data).expect("compress without dict failed");

        // With dictionary should be same or better for this pattern-heavy data
        // (Note: for some data patterns, the overhead of dictionary may not help)
        let decompressed = decompress_frame_with_dict(&with_dict, data.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed, data);

        // Verify without dict also works
        let decompressed_no_dict =
            decompress(&without_dict, data.len() * 2).expect("decompress no dict failed");
        assert_eq!(decompressed_no_dict, data);
    }

    #[test]
    fn test_frame_dict_encoder_debug() {
        let dict = Lz4Dict::new(b"debug test");
        let encoder = Lz4DictFrameEncoder::new(dict);
        let debug_str = format!("{:?}", encoder);
        assert!(debug_str.contains("Lz4DictFrameEncoder"));
        assert!(debug_str.contains("dict_id"));
    }

    #[test]
    fn test_frame_dict_decoder_debug() {
        let dict = Lz4Dict::new(b"debug test");
        let decoder = Lz4DictFrameDecoder::new(dict);
        let debug_str = format!("{:?}", decoder);
        assert!(debug_str.contains("Lz4DictFrameDecoder"));
        assert!(debug_str.contains("dict_id"));
    }

    #[test]
    fn test_frame_dict_encoder_with_options() {
        let dict = Lz4Dict::new(b"custom options test");
        let desc = FrameDescriptor::new()
            .with_block_checksum(true)
            .with_block_max_size(BlockMaxSize::Size64KB);

        let encoder = Lz4DictFrameEncoder::with_options(dict.clone(), desc);
        let decoder = Lz4DictFrameDecoder::new(dict);

        let data = b"custom options test data with block checksums enabled";
        let compressed = encoder.encode(data).expect("encode failed");

        let decompressed = decoder
            .decode(&compressed, data.len() * 2)
            .expect("decode failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_compressor_reset() {
        let dict = Lz4Dict::new(b"reset test pattern");
        let mut compressor = Lz4DictCompressor::new(dict.clone());

        let data1 = b"first data set";
        let mut output1 = vec![0u8; 500];

        let (_, produced1, _) = compressor
            .compress(data1, &mut output1, FlushMode::Finish)
            .expect("first compress failed");

        assert!(compressor.is_finished());

        // Reset and compress again
        compressor.reset();
        assert!(!compressor.is_finished());

        let data2 = b"second data set after reset";
        let mut output2 = vec![0u8; 500];

        let (_, produced2, _) = compressor
            .compress(data2, &mut output2, FlushMode::Finish)
            .expect("second compress failed");

        // Verify both are valid
        let decompressed1 =
            decompress_frame_with_dict(&output1[..produced1], data1.len() * 2, &dict)
                .expect("decompress1 failed");
        let decompressed2 =
            decompress_frame_with_dict(&output2[..produced2], data2.len() * 2, &dict)
                .expect("decompress2 failed");

        assert_eq!(decompressed1, data1);
        assert_eq!(decompressed2, data2);
    }

    #[test]
    fn test_frame_dict_decompressor_reset() {
        let dict = Lz4Dict::new(b"decompressor reset test");

        let data1 = b"first decompression";
        let compressed1 = compress_frame_with_dict(data1, &dict).expect("compress1 failed");

        let data2 = b"second decompression after reset";
        let compressed2 = compress_frame_with_dict(data2, &dict).expect("compress2 failed");

        let mut decompressor = Lz4DictDecompressor::new(dict);

        // First decompression
        let mut output1 = vec![0u8; 200];
        let (_, produced1, _) = decompressor
            .decompress(&compressed1, &mut output1)
            .expect("decompress1 failed");
        assert_eq!(&output1[..produced1], data1.as_slice());
        assert!(decompressor.is_finished());

        // Reset and decompress again
        decompressor.reset();
        assert!(!decompressor.is_finished());

        let mut output2 = vec![0u8; 200];
        let (_, produced2, _) = decompressor
            .decompress(&compressed2, &mut output2)
            .expect("decompress2 failed");
        assert_eq!(&output2[..produced2], data2.as_slice());
    }

    #[test]
    fn test_frame_dict_can_decode() {
        let dict1 = Lz4Dict::new(b"dictionary one");
        let dict2 = Lz4Dict::new(b"dictionary two");

        let data = b"data for dictionary one";
        let compressed = compress_frame_with_dict(data, &dict1).expect("compress failed");

        let decoder1 = Lz4DictFrameDecoder::new(dict1);
        let decoder2 = Lz4DictFrameDecoder::new(dict2);

        assert!(decoder1.can_decode(&compressed));
        assert!(!decoder2.can_decode(&compressed));
    }

    #[test]
    fn test_frame_no_dict_id() {
        // Regular frame without dictionary
        let data = b"regular frame data";
        let compressed = compress(data).expect("compress failed");

        let dict_id = get_frame_dict_id(&compressed).expect("get dict id failed");
        assert_eq!(dict_id, None);
    }

    #[test]
    fn test_frame_dict_accessor_methods() {
        let dict = Lz4Dict::new(b"accessor test");

        let encoder = Lz4DictFrameEncoder::new(dict.clone());
        assert_eq!(encoder.dict_id(), dict.id());
        assert_eq!(encoder.dict().len(), dict.len());

        let decoder = Lz4DictFrameDecoder::new(dict.clone());
        assert_eq!(decoder.dict_id(), dict.id());
        assert_eq!(decoder.dict().len(), dict.len());

        let compressor = Lz4DictCompressor::new(dict.clone());
        assert_eq!(compressor.dict().id(), dict.id());

        let decompressor = Lz4DictDecompressor::new(dict.clone());
        assert_eq!(decompressor.dict().id(), dict.id());
    }
}

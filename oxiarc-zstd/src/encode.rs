//! Zstandard encoder (frame construction).
//!
//! This module provides Zstandard compression with multiple block types:
//! - Raw blocks: Uncompressed data (used when data doesn't benefit from compression)
//! - RLE blocks: Single byte repeated (efficient for homogeneous data)
//!
//! Creates valid Zstd frames that any decoder can read.

use crate::xxhash::xxhash64_checksum;
use crate::{MAX_BLOCK_SIZE, ZSTD_MAGIC};
use oxiarc_core::error::Result;

/// Compression strategy for block encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionStrategy {
    /// Use raw blocks only (no compression).
    Raw,
    /// Use RLE blocks for homogeneous data, raw otherwise.
    #[default]
    RleOnly,
}

/// Zstandard encoder.
///
/// Supports raw and RLE block encoding for efficient compression.
/// Creates valid Zstd frames compatible with any decoder.
#[derive(Debug, Clone)]
pub struct ZstdEncoder {
    /// Include content checksum in output.
    include_checksum: bool,
    /// Include content size in header.
    include_content_size: bool,
    /// Compression strategy.
    strategy: CompressionStrategy,
}

impl ZstdEncoder {
    /// Create a new encoder with default settings.
    pub fn new() -> Self {
        Self {
            include_checksum: true,
            include_content_size: true,
            strategy: CompressionStrategy::default(),
        }
    }

    /// Set whether to include content checksum.
    pub fn set_checksum(&mut self, include: bool) -> &mut Self {
        self.include_checksum = include;
        self
    }

    /// Set whether to include content size in header.
    pub fn set_content_size(&mut self, include: bool) -> &mut Self {
        self.include_content_size = include;
        self
    }

    /// Set compression strategy.
    pub fn set_strategy(&mut self, strategy: CompressionStrategy) -> &mut Self {
        self.strategy = strategy;
        self
    }

    /// Compress data into a Zstandard frame.
    ///
    /// Uses the configured compression strategy to encode blocks efficiently.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::with_capacity(data.len() + 32);

        // Write magic number
        output.extend_from_slice(&ZSTD_MAGIC);

        // Write frame header
        self.write_frame_header(&mut output, data.len());

        // Write blocks with compression
        self.write_blocks(&mut output, data);

        // Write content checksum if enabled
        if self.include_checksum {
            let checksum = xxhash64_checksum(data);
            output.extend_from_slice(&checksum.to_le_bytes());
        }

        Ok(output)
    }

    /// Write frame header descriptor.
    fn write_frame_header(&self, output: &mut Vec<u8>, content_size: usize) {
        // Frame header descriptor byte:
        // - bits 0-1: Dictionary_ID_flag (0 = no dict)
        // - bit 2: Content_Checksum_flag
        // - bit 3: reserved (0)
        // - bit 4: unused (0)
        // - bit 5: Single_Segment_flag (1 = no window descriptor)
        // - bits 6-7: Frame_Content_Size_flag
        //
        // For simplicity, we use single segment mode with content size.

        let mut descriptor: u8 = 0;

        if self.include_checksum {
            descriptor |= 0x04; // Content_Checksum_flag
        }

        // Single_Segment_flag = 1 (no window descriptor needed)
        descriptor |= 0x20;

        // Determine content size encoding
        let (fcs_flag, fcs_bytes) = if !self.include_content_size || content_size == 0 {
            // For single segment with fcs_flag=0, content size uses 1 byte
            (0u8, 1)
        } else if content_size <= 255 {
            // 1 byte content size (fcs_flag=0 with single segment)
            (0u8, 1)
        } else if content_size <= 65535 + 256 {
            // 2 bytes content size
            (1u8, 2)
        } else if content_size <= u32::MAX as usize {
            // 4 bytes content size
            (2u8, 4)
        } else {
            // 8 bytes content size
            (3u8, 8)
        };

        descriptor |= fcs_flag << 6;
        output.push(descriptor);

        // Write Frame_Content_Size (required for single segment)
        match fcs_bytes {
            1 => {
                output.push(content_size as u8);
            }
            2 => {
                // FCS is (value - 256) for 2-byte encoding
                let adjusted = (content_size - 256) as u16;
                output.extend_from_slice(&adjusted.to_le_bytes());
            }
            4 => {
                output.extend_from_slice(&(content_size as u32).to_le_bytes());
            }
            8 => {
                output.extend_from_slice(&(content_size as u64).to_le_bytes());
            }
            _ => unreachable!(),
        }
    }

    /// Write data as blocks using the configured strategy.
    fn write_blocks(&self, output: &mut Vec<u8>, data: &[u8]) {
        if data.is_empty() {
            // Empty data: write a single empty raw block with last flag
            let block_header: u32 = 1; // last=1, type=Raw(0), size=0
            output.push((block_header & 0xFF) as u8);
            output.push(((block_header >> 8) & 0xFF) as u8);
            output.push(((block_header >> 16) & 0xFF) as u8);
            return;
        }

        let mut offset = 0;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let block_size = remaining.min(MAX_BLOCK_SIZE);
            let is_last = offset + block_size >= data.len();
            let block_data = &data[offset..offset + block_size];

            // Try RLE encoding if strategy allows
            if self.strategy == CompressionStrategy::RleOnly {
                if let Some(rle_byte) = Self::detect_rle(block_data) {
                    self.write_rle_block(output, rle_byte, block_size, is_last);
                    offset += block_size;
                    continue;
                }
            }

            // Fall back to raw block
            self.write_raw_block(output, block_data, is_last);
            offset += block_size;
        }
    }

    /// Detect if block can be encoded as RLE (all bytes the same).
    fn detect_rle(data: &[u8]) -> Option<u8> {
        if data.is_empty() {
            return None;
        }

        let first = data[0];

        // Quick check using chunks for SIMD-friendly comparison
        for chunk in data.chunks(16) {
            if !chunk.iter().all(|&b| b == first) {
                return None;
            }
        }

        Some(first)
    }

    /// Write an RLE block (single byte repeated).
    fn write_rle_block(&self, output: &mut Vec<u8>, byte: u8, size: usize, is_last: bool) {
        // Block header (3 bytes):
        // - bit 0: Last_Block
        // - bits 1-2: Block_Type (1 = RLE)
        // - bits 3-23: Block_Size (regenerated size, not encoded size)
        let last_flag = if is_last { 1u32 } else { 0u32 };
        let block_type = 1u32 << 1; // RLE = 1
        let block_header: u32 = last_flag | block_type | ((size as u32) << 3);

        output.push((block_header & 0xFF) as u8);
        output.push(((block_header >> 8) & 0xFF) as u8);
        output.push(((block_header >> 16) & 0xFF) as u8);

        // RLE block content: single byte
        output.push(byte);
    }

    /// Write a raw (uncompressed) block.
    fn write_raw_block(&self, output: &mut Vec<u8>, data: &[u8], is_last: bool) {
        // Block header (3 bytes):
        // - bit 0: Last_Block
        // - bits 1-2: Block_Type (0 = Raw)
        // - bits 3-23: Block_Size
        let last_flag = if is_last { 1u32 } else { 0u32 };
        let block_header: u32 = last_flag | ((data.len() as u32) << 3);

        output.push((block_header & 0xFF) as u8);
        output.push(((block_header >> 8) & 0xFF) as u8);
        output.push(((block_header >> 16) & 0xFF) as u8);

        // Write raw block data
        output.extend_from_slice(data);
    }
}

impl Default for ZstdEncoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Compress data using raw blocks.
///
/// This is a convenience function that uses default encoder settings.
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    ZstdEncoder::new().compress(data)
}

/// Compress data without checksum.
pub fn compress_no_checksum(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZstdEncoder::new();
    encoder.set_checksum(false);
    encoder.compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress;

    #[test]
    fn test_compress_empty() {
        let data: &[u8] = &[];
        let compressed = compress(data).unwrap();

        // Verify magic
        assert_eq!(&compressed[0..4], &ZSTD_MAGIC);

        // Roundtrip
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_small() {
        let data = b"Hello, Zstandard!";
        let compressed = compress(data).unwrap();

        // Roundtrip
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_compress_larger() {
        let data = vec![0x42u8; 1000];
        let compressed = compress(&data).unwrap();

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_multi_block() {
        // Data larger than MAX_BLOCK_SIZE to test multi-block
        let data = vec![0xABu8; MAX_BLOCK_SIZE + 1000];
        let compressed = compress(&data).unwrap();

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_no_checksum() {
        let data = b"Test without checksum";
        let compressed = compress_no_checksum(data).unwrap();

        // Should still decompress
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_encoder_builder() {
        let data = b"Builder pattern test";

        let mut encoder = ZstdEncoder::new();
        encoder.set_checksum(true).set_content_size(true);
        let compressed = encoder.compress(data).unwrap();

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_various_sizes() {
        for size in [0, 1, 10, 100, 255, 256, 257, 1000, 65535, 65536, 100000] {
            let data = vec![0x55u8; size];
            let compressed = compress(&data).unwrap();
            let decompressed = decompress(&compressed).unwrap();
            assert_eq!(decompressed, data, "Failed for size {}", size);
        }
    }

    #[test]
    fn test_rle_compression() {
        // Data that is all the same byte - should compress well with RLE
        let data = vec![0xAAu8; 10000];
        let compressed = compress(&data).unwrap();

        // RLE should be much smaller (1 byte per block vs full data)
        assert!(
            compressed.len() < data.len() / 10,
            "RLE compression failed: {} vs {}",
            compressed.len(),
            data.len()
        );

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_rle_multi_block() {
        // Multiple blocks of the same byte
        let data = vec![0xBBu8; MAX_BLOCK_SIZE * 3];
        let compressed = compress(&data).unwrap();

        // Should compress very well
        assert!(
            compressed.len() < 100,
            "Expected small output, got {}",
            compressed.len()
        );

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_rle_mixed_data() {
        // Data with some RLE-able parts and some mixed
        let mut data = vec![0xCCu8; 1000]; // RLE-able
        data.extend_from_slice(b"Hello, World!"); // Not RLE-able
        data.extend_from_slice(&vec![0xDDu8; 1000]); // RLE-able

        let compressed = compress(&data).unwrap();
        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_detect_rle() {
        // All same bytes
        assert_eq!(ZstdEncoder::detect_rle(&[0xAA; 100]), Some(0xAA));
        assert_eq!(ZstdEncoder::detect_rle(&[0x00; 50]), Some(0x00));
        assert_eq!(ZstdEncoder::detect_rle(&[0xFF]), Some(0xFF));

        // Mixed bytes
        assert_eq!(ZstdEncoder::detect_rle(&[0xAA, 0xAA, 0xBB]), None);
        assert_eq!(ZstdEncoder::detect_rle(&[0x00, 0x01]), None);

        // Empty
        assert_eq!(ZstdEncoder::detect_rle(&[]), None);
    }

    #[test]
    fn test_raw_strategy() {
        let data = vec![0xEEu8; 1000];

        let mut encoder = ZstdEncoder::new();
        encoder.set_strategy(CompressionStrategy::Raw);
        let compressed = encoder.compress(&data).unwrap();

        // Raw blocks don't compress, so output is slightly larger than input
        assert!(compressed.len() > data.len());

        let decompressed = decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }
}

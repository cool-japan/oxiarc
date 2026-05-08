//! Zstandard encoder (frame construction).
//!
//! This module provides Zstandard compression with multiple strategies:
//! - **Level 0**: Raw/RLE blocks only (no LZ77 compression)
//! - **Levels 1-22**: Full LZ77 + Huffman + FSE compressed blocks
//!
//! Creates valid Zstd frames compatible with any decoder.

use crate::compressed_block::encode_compressed_block;
use crate::lz77::{LevelConfig, MatchFinder};
use crate::xxhash::xxhash64_checksum;
use crate::{MAX_BLOCK_SIZE, ZSTD_MAGIC};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

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
/// Supports multiple compression levels (0-22) with LZ77 matching,
/// Huffman literal encoding, and FSE sequence encoding.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`ZstdEncoder::with_progress`] / [`ZstdEncoder::with_cancel`] builders.
#[derive(Clone)]
pub struct ZstdEncoder {
    /// Include content checksum in output.
    include_checksum: bool,
    /// Include content size in header.
    include_content_size: bool,
    /// Compression strategy (used when level == 0).
    strategy: CompressionStrategy,
    /// Compression level (0 = raw/RLE, 1-22 = LZ77 compression).
    level: i32,
    /// Optional dictionary for improved compression of small data.
    dictionary: Option<Vec<u8>>,
    /// Dictionary ID (XXH64 of dictionary data, lower 32 bits).
    dict_id: Option<u32>,
    /// Optional progress sink. Notified after each block is written.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token. Checked before each block.
    cancel: Option<CancellationToken>,
}

impl std::fmt::Debug for ZstdEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZstdEncoder")
            .field("level", &self.level)
            .field("include_checksum", &self.include_checksum)
            .field("include_content_size", &self.include_content_size)
            .finish()
    }
}

impl ZstdEncoder {
    /// Create a new encoder with default settings (level 0, RLE strategy).
    pub fn new() -> Self {
        Self {
            include_checksum: true,
            include_content_size: true,
            strategy: CompressionStrategy::default(),
            level: 0,
            dictionary: None,
            dict_id: None,
            progress: None,
            cancel: None,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes_processed, None)` is called after each
    /// block is written to the output. `on_finish()` is called after the
    /// content checksum is written.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each block is encoded.
    /// If cancelled, returns [`oxiarc_core::error::OxiArcError::Cancelled`].
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
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

    /// Set compression strategy (only effective when level == 0).
    pub fn set_strategy(&mut self, strategy: CompressionStrategy) -> &mut Self {
        self.strategy = strategy;
        self
    }

    /// Set compression level (0-22).
    ///
    /// - Level 0: Raw/RLE blocks (fastest, no compression)
    /// - Levels 1-3: Fast compression (greedy matching)
    /// - Levels 4-9: Balanced compression (lazy matching)
    /// - Levels 10-22: High compression (deep search)
    pub fn set_level(&mut self, level: i32) -> &mut Self {
        self.level = level.clamp(0, 22);
        self
    }

    /// Set a pre-trained dictionary for improved compression of small data.
    pub fn set_dictionary(&mut self, dict: &[u8]) -> &mut Self {
        if dict.is_empty() {
            self.dictionary = None;
            self.dict_id = None;
        } else {
            let id = crate::xxhash::xxhash64(dict) as u32;
            self.dictionary = Some(dict.to_vec());
            self.dict_id = Some(id);
        }
        self
    }

    /// Compress data into a Zstandard frame.
    ///
    /// Uses the configured compression level and strategy.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        // Cancellation check at the start of the full operation.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        let mut output = Vec::with_capacity(data.len() + 32);

        // Write magic number
        output.extend_from_slice(&ZSTD_MAGIC);

        // Write frame header
        self.write_frame_header(&mut output, data.len());

        // Write blocks with compression
        if self.level > 0 {
            self.write_compressed_blocks(&mut output, data)?;
        } else {
            self.write_blocks(&mut output, data)?;
        }

        // Write content checksum if enabled
        if self.include_checksum {
            let checksum = xxhash64_checksum(data);
            output.extend_from_slice(&checksum.to_le_bytes());
        }

        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }

        Ok(output)
    }

    /// Compress data into a Zstandard frame using parallel block compression
    /// (requires `parallel` feature).
    #[cfg(feature = "parallel")]
    pub fn compress_parallel(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::with_capacity(data.len() + 32);

        // Write magic number
        output.extend_from_slice(&ZSTD_MAGIC);

        // Write frame header
        self.write_frame_header(&mut output, data.len());

        // Split data into blocks
        if data.is_empty() {
            write_empty_block(&mut output);
        } else {
            let chunks: Vec<&[u8]> = data.chunks(MAX_BLOCK_SIZE).collect();

            // Process blocks in parallel
            let block_data: Vec<(bool, Vec<u8>)> = chunks
                .par_iter()
                .enumerate()
                .map(|(idx, chunk)| {
                    let is_last = idx == chunks.len() - 1;

                    // Try RLE encoding if strategy allows
                    if self.strategy == CompressionStrategy::RleOnly {
                        if let Some(rle_byte) = detect_rle(chunk) {
                            let mut block_output = Vec::new();
                            write_rle_block_to(&mut block_output, rle_byte, chunk.len(), is_last);
                            return (is_last, block_output);
                        }
                    }

                    // Fall back to raw block
                    let mut block_output = Vec::new();
                    write_raw_block_to(&mut block_output, chunk, is_last);
                    (is_last, block_output)
                })
                .collect();

            // Assemble blocks sequentially
            for (_is_last, block_bytes) in block_data {
                output.extend_from_slice(&block_bytes);
            }
        }

        // Write content checksum if enabled
        if self.include_checksum {
            let checksum = xxhash64_checksum(data);
            output.extend_from_slice(&checksum.to_le_bytes());
        }

        Ok(output)
    }

    /// Write frame header descriptor.
    fn write_frame_header(&self, output: &mut Vec<u8>, content_size: usize) {
        let mut descriptor: u8 = 0;

        if self.include_checksum {
            descriptor |= 0x04; // Content_Checksum_flag
        }

        // Single_Segment_flag = 1 (no window descriptor needed)
        descriptor |= 0x20;

        // Dictionary ID flag
        let dict_id_flag = if self.dict_id.is_some() { 3u8 } else { 0u8 };
        descriptor |= dict_id_flag;

        // Determine content size encoding
        let (fcs_flag, fcs_bytes) = if !self.include_content_size || content_size <= 255 {
            (0u8, 1)
        } else if content_size <= 65535 + 256 {
            (1u8, 2)
        } else if content_size <= u32::MAX as usize {
            (2u8, 4)
        } else {
            (3u8, 8)
        };

        descriptor |= fcs_flag << 6;
        output.push(descriptor);

        // Write Dictionary_ID (4 bytes if present)
        if let Some(id) = self.dict_id {
            output.extend_from_slice(&id.to_le_bytes());
        }

        // Write Frame_Content_Size (required for single segment)
        match fcs_bytes {
            1 => {
                output.push(content_size as u8);
            }
            2 => {
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

    /// Write data as raw/RLE blocks (level 0).
    fn write_blocks(&self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            write_empty_block(output);
            return Ok(());
        }

        let mut offset = 0;
        let mut bytes_processed: u64 = 0;

        while offset < data.len() {
            // Cooperative cancellation check before each block.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }

            let remaining = data.len() - offset;
            let block_size = remaining.min(MAX_BLOCK_SIZE);
            let is_last = offset + block_size >= data.len();
            let block_data = &data[offset..offset + block_size];

            // Try RLE encoding if strategy allows
            if self.strategy == CompressionStrategy::RleOnly {
                if let Some(rle_byte) = detect_rle(block_data) {
                    write_rle_block_to(output, rle_byte, block_size, is_last);
                    offset += block_size;
                    bytes_processed += block_size as u64;
                    if let Some(ref handle) = self.progress {
                        handle.on_progress(bytes_processed, None);
                    }
                    continue;
                }
            }

            // Fall back to raw block
            write_raw_block_to(output, block_data, is_last);
            offset += block_size;
            bytes_processed += block_size as u64;
            if let Some(ref handle) = self.progress {
                handle.on_progress(bytes_processed, None);
            }
        }

        Ok(())
    }

    /// Write data as compressed blocks using LZ77 (levels 1-22).
    fn write_compressed_blocks(&self, output: &mut Vec<u8>, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            write_empty_block(output);
            return Ok(());
        }

        let config = LevelConfig::for_level(self.level);
        let mut finder = MatchFinder::new(&config);
        let dict = self.dictionary.as_deref().unwrap_or(&[]);

        let mut offset = 0;
        let mut bytes_processed: u64 = 0;

        while offset < data.len() {
            // Cooperative cancellation check before each block.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }

            let remaining = data.len() - offset;
            let block_size = remaining.min(config.target_block_size);
            let is_last = offset + block_size >= data.len();
            let block_data = &data[offset..offset + block_size];

            // Try RLE first (always efficient for homogeneous data)
            if let Some(rle_byte) = detect_rle(block_data) {
                write_rle_block_to(output, rle_byte, block_size, is_last);
                offset += block_size;
                bytes_processed += block_size as u64;
                if let Some(ref handle) = self.progress {
                    handle.on_progress(bytes_processed, None);
                }
                continue;
            }

            // Find LZ77 matches
            let sequences = finder.find_sequences(block_data, dict)?;

            // Try to encode as a compressed block
            match encode_compressed_block(&sequences) {
                Ok(compressed_content) => {
                    // Only use compressed block if it's actually smaller
                    if compressed_content.len() < block_data.len() {
                        write_compressed_block_to(output, &compressed_content, is_last);
                    } else {
                        // Compressed is larger, use raw block
                        write_raw_block_to(output, block_data, is_last);
                    }
                }
                Err(_) => {
                    // Compression failed, fall back to raw block
                    write_raw_block_to(output, block_data, is_last);
                }
            }

            finder.reset();
            offset += block_size;
            bytes_processed += block_size as u64;
            if let Some(ref handle) = self.progress {
                handle.on_progress(bytes_processed, None);
            }
        }

        Ok(())
    }
}

impl Default for ZstdEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// --- Block writing helpers ---

/// Write an empty last block.
fn write_empty_block(output: &mut Vec<u8>) {
    let block_header: u32 = 1; // last=1, type=Raw(0), size=0
    output.push((block_header & 0xFF) as u8);
    output.push(((block_header >> 8) & 0xFF) as u8);
    output.push(((block_header >> 16) & 0xFF) as u8);
}

/// Write a raw (uncompressed) block.
fn write_raw_block_to(output: &mut Vec<u8>, data: &[u8], is_last: bool) {
    let last_flag = if is_last { 1u32 } else { 0u32 };
    let block_header: u32 = last_flag | ((data.len() as u32) << 3);
    output.push((block_header & 0xFF) as u8);
    output.push(((block_header >> 8) & 0xFF) as u8);
    output.push(((block_header >> 16) & 0xFF) as u8);
    output.extend_from_slice(data);
}

/// Write an RLE block.
fn write_rle_block_to(output: &mut Vec<u8>, byte: u8, size: usize, is_last: bool) {
    let last_flag = if is_last { 1u32 } else { 0u32 };
    let block_type = 1u32 << 1; // RLE = 1
    let block_header: u32 = last_flag | block_type | ((size as u32) << 3);
    output.push((block_header & 0xFF) as u8);
    output.push(((block_header >> 8) & 0xFF) as u8);
    output.push(((block_header >> 16) & 0xFF) as u8);
    output.push(byte);
}

/// Write a compressed block.
fn write_compressed_block_to(output: &mut Vec<u8>, content: &[u8], is_last: bool) {
    let last_flag = if is_last { 1u32 } else { 0u32 };
    let block_type = 2u32 << 1; // Compressed = 2
    let block_header: u32 = last_flag | block_type | ((content.len() as u32) << 3);
    output.push((block_header & 0xFF) as u8);
    output.push(((block_header >> 8) & 0xFF) as u8);
    output.push(((block_header >> 16) & 0xFF) as u8);
    output.extend_from_slice(content);
}

/// Detect if block can be encoded as RLE (all bytes the same).
fn detect_rle(data: &[u8]) -> Option<u8> {
    if data.is_empty() {
        return None;
    }
    let first = data[0];
    for chunk in data.chunks(16) {
        if !chunk.iter().all(|&b| b == first) {
            return None;
        }
    }
    Some(first)
}

// --- Convenience functions ---

/// Compress data using default settings (raw/RLE blocks, level 0).
///
/// For actual LZ77 compression, use [`compress_with_level`] or configure
/// [`ZstdEncoder`] with [`set_level`](ZstdEncoder::set_level).
pub fn compress(data: &[u8]) -> Result<Vec<u8>> {
    ZstdEncoder::new().compress(data)
}

/// Compress data with a specific compression level (1-22).
///
/// This is the primary compression function for most use cases.
///
/// # Arguments
/// * `data` - Data to compress
/// * `level` - Compression level (1 = fastest, 22 = best compression)
pub fn compress_with_level(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let mut encoder = ZstdEncoder::new();
    encoder.set_level(level);
    encoder.compress(data)
}

/// Compress data without checksum.
pub fn compress_no_checksum(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZstdEncoder::new();
    encoder.set_checksum(false);
    encoder.compress(data)
}

/// Compress data using parallel block compression (requires `parallel` feature).
#[cfg(feature = "parallel")]
pub fn compress_parallel(data: &[u8]) -> Result<Vec<u8>> {
    ZstdEncoder::new().compress_parallel(data)
}

/// Convenience function: compress data and return bytes (compatible with
/// `zstd::encode_all` pattern).
///
/// # Arguments
/// * `data` - Data to compress (implements `AsRef<[u8]>`)
/// * `level` - Compression level (1-22)
pub fn encode_all(data: &[u8], level: i32) -> Result<Vec<u8>> {
    compress_with_level(data, level)
}

/// Convenience function: decompress data (compatible with `zstd::decode_all` pattern).
pub fn decode_all(data: &[u8]) -> Result<Vec<u8>> {
    crate::decompress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompress;

    #[test]
    fn test_compress_empty() {
        let data: &[u8] = &[];
        let compressed = compress(data).expect("compression failed");
        assert_eq!(&compressed[0..4], &ZSTD_MAGIC);
        let decompressed = decompress(&compressed).expect("decompression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_small() {
        let data = b"Hello, Zstandard!";
        let compressed = compress(data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_compress_larger() {
        let data = vec![0x42u8; 1000];
        let compressed = compress(&data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_multi_block() {
        let data = vec![0xABu8; MAX_BLOCK_SIZE + 1000];
        let compressed = compress(&data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_no_checksum() {
        let data = b"Test without checksum";
        let compressed = compress_no_checksum(data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_encoder_builder() {
        let data = b"Builder pattern test";
        let mut encoder = ZstdEncoder::new();
        encoder.set_checksum(true).set_content_size(true);
        let compressed = encoder.compress(data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_various_sizes() {
        for size in [0, 1, 10, 100, 255, 256, 257, 1000, 65535, 65536, 100000] {
            let data = vec![0x55u8; size];
            let compressed = compress(&data).expect("compression failed");
            let decompressed = decompress(&compressed).expect("compression failed");
            assert_eq!(decompressed, data, "Failed for size {}", size);
        }
    }

    #[test]
    fn test_rle_compression() {
        let data = vec![0xAAu8; 10000];
        let compressed = compress(&data).expect("compression failed");
        assert!(
            compressed.len() < data.len() / 10,
            "RLE compression failed: {} vs {}",
            compressed.len(),
            data.len()
        );
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_rle_multi_block() {
        let data = vec![0xBBu8; MAX_BLOCK_SIZE * 3];
        let compressed = compress(&data).expect("compression failed");
        assert!(
            compressed.len() < 100,
            "Expected small output, got {}",
            compressed.len()
        );
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_rle_mixed_data() {
        let mut data = vec![0xCCu8; 1000];
        data.extend_from_slice(b"Hello, World!");
        data.extend_from_slice(&vec![0xDDu8; 1000]);
        let compressed = compress(&data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_detect_rle() {
        assert_eq!(detect_rle(&[0xAA; 100]), Some(0xAA));
        assert_eq!(detect_rle(&[0x00; 50]), Some(0x00));
        assert_eq!(detect_rle(&[0xFF]), Some(0xFF));
        assert_eq!(detect_rle(&[0xAA, 0xAA, 0xBB]), None);
        assert_eq!(detect_rle(&[0x00, 0x01]), None);
        assert_eq!(detect_rle(&[]), None);
    }

    #[test]
    fn test_raw_strategy() {
        let data = vec![0xEEu8; 1000];
        let mut encoder = ZstdEncoder::new();
        encoder.set_strategy(CompressionStrategy::Raw);
        let compressed = encoder.compress(&data).expect("compression failed");
        assert!(compressed.len() > data.len());
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_compress_with_level() {
        // Test that level-based compression produces valid output
        let data = b"The quick brown fox jumps over the lazy dog. \
                     The quick brown fox jumps over the lazy dog. \
                     The quick brown fox jumps over the lazy dog.";

        for level in [1, 3, 6, 9, 15, 22] {
            let compressed = compress_with_level(data, level).expect("compression failed");
            let decompressed = decompress(&compressed).expect("compression failed");
            assert_eq!(
                decompressed,
                data.as_slice(),
                "Roundtrip failed for level {}",
                level
            );
        }
    }

    #[test]
    fn test_encode_all_decode_all() {
        let data = b"Testing encode_all and decode_all convenience functions";
        let compressed = encode_all(data, 3).expect("compression failed");
        let decompressed = decode_all(&compressed).expect("decompression failed");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    fn test_level_compression_ratio() {
        // Repetitive data should compress with LZ77
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(b"ABCDEFGHIJKLMNOP");
        }

        let raw = compress(&data).expect("compression failed");
        let level3 = compress_with_level(&data, 3).expect("compression failed");

        // Level 3 should produce smaller output than raw for repetitive data
        assert!(
            level3.len() <= raw.len(),
            "Level 3 ({}) should be <= raw ({}) for repetitive data",
            level3.len(),
            raw.len()
        );

        // Both should decompress correctly
        assert_eq!(decompress(&raw).expect("compression failed"), data);
        assert_eq!(decompress(&level3).expect("compression failed"), data);
    }

    #[test]
    fn test_large_data_roundtrip() {
        // Simulate compressible data similar to what network compression tests use.
        let mut data = Vec::with_capacity(16384);
        let pattern = b"RDF triple: <http://example.org/subject> <http://example.org/predicate> \"value\" .\n";
        while data.len() < 16384 {
            data.extend_from_slice(pattern);
        }
        data.truncate(16384);

        for level in [1, 3] {
            let compressed = encode_all(&data, level).expect("compression failed");
            let decompressed = decode_all(&compressed).expect("decompression failed");
            assert_eq!(
                decompressed, data,
                "Large roundtrip failed for level {}",
                level
            );
        }
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        let data = b"Hello, World! Parallel Zstandard compression.";
        let compressed = compress_parallel(data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        let data = vec![0xABu8; 5_000_000];
        let compressed = compress_parallel(&data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_rle_compression() {
        let data = vec![0xCCu8; 2_000_000];
        let compressed = compress_parallel(&data).expect("compression failed");
        assert!(compressed.len() < data.len() / 100);
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        let data: &[u8] = &[];
        let compressed = compress_parallel(data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial() {
        let data = b"Testing parallel vs serial compression output.";
        let serial = compress(data).expect("compression failed");
        let parallel = compress_parallel(data).expect("compression failed");
        let serial_decompressed = decompress(&serial).expect("compression failed");
        let parallel_decompressed = decompress(&parallel).expect("compression failed");
        assert_eq!(serial_decompressed, data.as_slice());
        assert_eq!(parallel_decompressed, data.as_slice());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_encoder_options() {
        let data = vec![0xFFu8; 1_000_000];
        let mut encoder = ZstdEncoder::new();
        encoder
            .set_checksum(false)
            .set_strategy(CompressionStrategy::RleOnly);
        let compressed = encoder
            .compress_parallel(&data)
            .expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multi_block() {
        let data = vec![0x55u8; MAX_BLOCK_SIZE * 3 + 5000];
        let compressed = compress_parallel(&data).expect("compression failed");
        let decompressed = decompress(&compressed).expect("compression failed");
        assert_eq!(decompressed, data);
    }

    use oxiarc_core::cancel::CancellationToken;
    use oxiarc_core::progress::ProgressSink;
    use std::sync::{Arc, Mutex};

    type ProgressLog = Arc<Mutex<Vec<(u64, Option<u64>)>>>;

    struct MockSink(ProgressLog);

    impl ProgressSink for MockSink {
        fn on_progress(&self, processed: u64, total: Option<u64>) {
            self.0
                .lock()
                .expect("lock poisoned")
                .push((processed, total));
        }
    }

    fn make_compressible_data(size: usize) -> Vec<u8> {
        let pattern = b"ZstdEncoder test data with repeating pattern ABCDEFGH ";
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            let remaining = size - data.len();
            let chunk = &pattern[..remaining.min(pattern.len())];
            data.extend_from_slice(chunk);
        }
        data
    }

    #[test]
    fn test_zstd_encoder_progress_reports() {
        let data = make_compressible_data(1024 * 1024); // 1 MB

        let calls: ProgressLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::new(MockSink(calls.clone()));

        let encoder =
            ZstdEncoder::new().with_progress(sink as oxiarc_core::progress::ProgressHandle);
        encoder.compress(&data).expect("compress failed");

        let recorded = calls.lock().expect("lock poisoned");
        assert!(!recorded.is_empty(), "expected at least one progress call");
        let (last_processed, _) = *recorded.last().expect("non-empty");
        assert_eq!(
            last_processed,
            data.len() as u64,
            "final processed count must equal input size"
        );
    }

    #[test]
    fn test_zstd_encoder_cancel_aborts() {
        let data = make_compressible_data(1024 * 1024);
        let token = CancellationToken::new();
        let encoder = ZstdEncoder::new().with_cancel(token.clone());

        token.cancel();
        let result = encoder.compress(&data);
        assert!(result.is_err(), "expected cancellation error");
    }
}

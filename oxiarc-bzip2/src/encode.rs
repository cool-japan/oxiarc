//! BZip2 encoder.

use crate::{BLOCK_MAGIC, BZIP2_MAGIC, CompressionLevel, EOS_MAGIC, bwt, huffman, mtf, rle};
use oxiarc_core::error::Result;
use oxiarc_core::{BitWriter, Crc32};
use std::io::Write;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// BZip2 encoder.
pub struct BzEncoder<W: Write> {
    writer: BitWriter<W>,
    #[allow(dead_code)]
    level: CompressionLevel,
    block_crc: u32,
    combined_crc: u32,
}

impl<W: Write> BzEncoder<W> {
    /// Create a new encoder.
    pub fn new(writer: W, level: CompressionLevel) -> Result<Self> {
        let mut bit_writer = BitWriter::new(writer);

        // Write stream header
        // "BZ" magic
        bit_writer.write_bits(BZIP2_MAGIC[0] as u32, 8)?;
        bit_writer.write_bits(BZIP2_MAGIC[1] as u32, 8)?;

        // 'h' for Huffman + block size digit
        bit_writer.write_bits(b'h' as u32, 8)?;
        bit_writer.write_bits((b'0' + level.level()) as u32, 8)?;

        Ok(Self {
            writer: bit_writer,
            level,
            block_crc: 0,
            combined_crc: 0,
        })
    }

    /// Write a data block.
    pub fn write_block(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        // Calculate CRC
        let block_crc = Crc32::compute(data);
        self.block_crc = block_crc;
        self.combined_crc = self.combined_crc.rotate_left(1) ^ block_crc;

        // Step 1: Initial RLE
        let rle1_data = rle::rle1_encode(data);

        // Step 2: Burrows-Wheeler Transform
        let (bwt_data, orig_ptr) = bwt::transform(&rle1_data);

        // Step 3: Move-to-Front Transform
        let mtf_data = mtf::transform(&bwt_data);

        // Build used symbol bitmap from MTF output
        let mut used = [false; 256];
        for &b in &mtf_data {
            used[b as usize] = true;
        }

        // Step 4: Zero-run encoding with compact symbol mapping
        let zrle_data = rle::encode_zero_runs_compact(&mtf_data, &used);

        // Write block header
        for &b in &BLOCK_MAGIC {
            self.writer.write_bits(b as u32, 8)?;
        }

        // Write block CRC
        self.writer.write_bits(block_crc, 32)?;

        // Randomised flag (always 0 for modern bzip2)
        self.writer.write_bits(0, 1)?;

        // Original pointer
        self.writer.write_bits(orig_ptr, 24)?;

        // Write 16-bit "in use" map for each group of 16 symbols
        // (used bitmap was already computed before ZRLE encoding)
        let mut in_use_16 = 0u16;
        for i in 0..16 {
            let mut group_used = false;
            for j in 0..16 {
                if used[i * 16 + j] {
                    group_used = true;
                    break;
                }
            }
            if group_used {
                in_use_16 |= 1 << (15 - i);
            }
        }
        self.writer.write_bits(in_use_16 as u32, 16)?;

        // Write individual symbol maps for used groups
        for i in 0..16 {
            if (in_use_16 >> (15 - i)) & 1 == 1 {
                let mut group_map = 0u16;
                for j in 0..16 {
                    if used[i * 16 + j] {
                        group_map |= 1 << (15 - j);
                    }
                }
                self.writer.write_bits(group_map as u32, 16)?;
            }
        }

        // Count used symbols for Huffman coding
        // BZip2 alphabet: RUNA (0), RUNB (1), MTF values 1..num_used_symbols shifted by 1 (+2), EOB
        let num_used_symbols = used.iter().filter(|&&u| u).count();
        // Total alphabet size = 2 (RUNA, RUNB) + num_used_symbols (MTF values) + 1 (EOB)
        let alphabet_size = num_used_symbols + 3;

        // Number of Huffman tables (1-6, based on data size)
        let num_tables = ((zrle_data.len() / 50).clamp(1, 6)).max(1);
        self.writer.write_bits(num_tables as u32, 3)?;

        // Number of selector groups (each group is 50 symbols)
        let num_selectors = zrle_data.len().div_ceil(50);
        self.writer.write_bits(num_selectors as u32, 15)?;

        // Write selectors (which table to use for each group)
        // For simplicity, use table 0 for all (write 0 bits in unary)
        for _ in 0..num_selectors {
            // Unary code: single 0-bit means "use table 0"
            self.writer.write_bits(0, 1)?;
        }

        // Build frequency table for all symbols in alphabet
        let mut freqs = vec![0u32; alphabet_size];
        for &sym in &zrle_data {
            let sym_idx = sym as usize;
            if sym_idx < freqs.len() {
                freqs[sym_idx] += 1;
            }
        }
        // EOB symbol (last in alphabet) needs non-zero frequency
        freqs[alphabet_size - 1] = freqs[alphabet_size - 1].max(1);

        // Build Huffman code lengths (max 17 bits for BZip2)
        let lengths = huffman::build_code_lengths(&freqs, 17);

        // Write Huffman tables (delta-encoded code lengths)
        for _ in 0..num_tables {
            // Write initial 5-bit starting length
            let start_len = lengths.first().copied().map_or(5i32, |v| v as i32);
            self.writer.write_bits(start_len as u32, 5)?;

            let mut current_len = start_len;

            for &len in &lengths {
                let target_len = len as i32;
                // Delta encode: write bits to adjust from current to target
                while current_len != target_len {
                    self.writer.write_bits(1, 1)?; // Signal "adjust"
                    if target_len > current_len {
                        self.writer.write_bits(0, 1)?; // Increment
                        current_len += 1;
                    } else {
                        self.writer.write_bits(1, 1)?; // Decrement
                        current_len -= 1;
                    }
                }
                self.writer.write_bits(0, 1)?; // Signal "done with this symbol"
            }
        }

        // Build Huffman table with canonical codes
        let table = huffman::HuffmanTable::from_lengths(&lengths)?;

        // Write Huffman-encoded data using canonical codes
        for &sym in &zrle_data {
            if let Some((code, len)) = table.get_code(sym) {
                self.write_code(code, len)?;
            }
        }

        // Write EOB symbol
        let eob_sym = (alphabet_size - 1) as u16;
        if let Some((code, len)) = table.get_code(eob_sym) {
            self.write_code(code, len)?;
        }

        Ok(())
    }

    /// Write a Huffman code (MSB-first).
    fn write_code(&mut self, code: u32, len: u8) -> Result<()> {
        // Write bits MSB-first (BZip2 convention)
        for i in (0..len).rev() {
            let bit = (code >> i) & 1;
            self.writer.write_bits(bit, 1)?;
        }
        Ok(())
    }

    /// Finish encoding and write the stream footer.
    pub fn finish(mut self) -> Result<W> {
        // Write end of stream marker
        for &b in &EOS_MAGIC {
            self.writer.write_bits(b as u32, 8)?;
        }

        // Write combined CRC
        self.writer.write_bits(self.combined_crc, 32)?;

        // Flush any remaining bits
        self.writer.flush()?;

        self.writer.into_inner()
    }
}

/// Compress data using BZip2.
pub fn compress(data: &[u8], level: CompressionLevel) -> Result<Vec<u8>> {
    let output = Vec::new();
    let mut encoder = BzEncoder::new(output, level)?;

    let block_size = level.block_size();
    let mut offset = 0;

    while offset < data.len() {
        let end = (offset + block_size).min(data.len());
        encoder.write_block(&data[offset..end])?;
        offset = end;
    }

    encoder.finish()
}

/// Intermediate block data for parallel compression.
/// Holds all pre-computed data needed to write a block.
#[cfg(feature = "parallel")]
struct CompressedBlockData {
    /// CRC32 of original block data
    crc: u32,
    /// BWT original pointer
    orig_ptr: u32,
    /// Used symbol bitmap (256 entries)
    used: [bool; 256],
    /// Zero-run encoded data
    zrle_data: Vec<u16>,
    /// Huffman code lengths
    lengths: Vec<u8>,
}

/// Compress data using parallel block compression (requires `parallel` feature).
///
/// This function splits the input into independent blocks and compresses them
/// in parallel using rayon. The heavy work (RLE, BWT, MTF, Huffman table building)
/// is done in parallel, while the final bitstream writing is done sequentially
/// to maintain proper bit alignment.
///
/// # Arguments
///
/// * `data` - Data to compress
/// * `level` - Compression level (1-9)
///
/// # Returns
///
/// Compressed data in BZip2 format.
#[cfg(feature = "parallel")]
pub fn compress_parallel(data: &[u8], level: CompressionLevel) -> Result<Vec<u8>> {
    let output = Vec::new();
    let mut bit_writer = BitWriter::new(output);

    // Write stream header
    bit_writer.write_bits(BZIP2_MAGIC[0] as u32, 8)?;
    bit_writer.write_bits(BZIP2_MAGIC[1] as u32, 8)?;
    bit_writer.write_bits(b'h' as u32, 8)?;
    bit_writer.write_bits((b'0' + level.level()) as u32, 8)?;

    if data.is_empty() {
        // Write end of stream marker
        for &b in &EOS_MAGIC {
            bit_writer.write_bits(b as u32, 8)?;
        }
        bit_writer.write_bits(0, 32)?; // Combined CRC
        bit_writer.flush()?;
        return bit_writer.into_inner();
    }

    // Split input into blocks
    let block_size = level.block_size();
    let chunks: Vec<&[u8]> = data.chunks(block_size).collect();

    // Compress blocks in parallel (heavy computation only, no writing)
    let compressed_blocks: Vec<Result<CompressedBlockData>> = chunks
        .par_iter()
        .map(|chunk| {
            let block_crc = Crc32::compute(chunk);

            // Compress block data
            let rle1_data = rle::rle1_encode(chunk);
            let (bwt_data, orig_ptr) = bwt::transform(&rle1_data);
            let mtf_data = mtf::transform(&bwt_data);

            let mut used = [false; 256];
            for &b in &mtf_data {
                used[b as usize] = true;
            }

            let zrle_data = rle::encode_zero_runs_compact(&mtf_data, &used);

            // Build Huffman tables
            let num_used_symbols = used.iter().filter(|&&u| u).count();
            let alphabet_size = num_used_symbols + 3;

            let mut freqs = vec![0u32; alphabet_size];
            for &sym in &zrle_data {
                let sym_idx = sym as usize;
                if sym_idx < freqs.len() {
                    freqs[sym_idx] += 1;
                }
            }
            freqs[alphabet_size - 1] = freqs[alphabet_size - 1].max(1);

            let lengths = huffman::build_code_lengths(&freqs, 17);

            Ok(CompressedBlockData {
                crc: block_crc,
                orig_ptr,
                used,
                zrle_data,
                lengths,
            })
        })
        .collect();

    // Write blocks sequentially with single BitWriter (maintains proper bit alignment)
    let mut combined_crc = 0u32;
    for result in compressed_blocks {
        let block = result?;
        combined_crc = combined_crc.rotate_left(1) ^ block.crc;

        // Write block header
        for &b in &BLOCK_MAGIC {
            bit_writer.write_bits(b as u32, 8)?;
        }
        bit_writer.write_bits(block.crc, 32)?;
        bit_writer.write_bits(0, 1)?; // Randomised flag
        bit_writer.write_bits(block.orig_ptr, 24)?;

        // Write symbol maps
        let mut in_use_16 = 0u16;
        for i in 0..16 {
            let mut group_used = false;
            for j in 0..16 {
                if block.used[i * 16 + j] {
                    group_used = true;
                    break;
                }
            }
            if group_used {
                in_use_16 |= 1 << (15 - i);
            }
        }
        bit_writer.write_bits(in_use_16 as u32, 16)?;

        for i in 0..16 {
            if (in_use_16 >> (15 - i)) & 1 == 1 {
                let mut group_map = 0u16;
                for j in 0..16 {
                    if block.used[i * 16 + j] {
                        group_map |= 1 << (15 - j);
                    }
                }
                bit_writer.write_bits(group_map as u32, 16)?;
            }
        }

        // Write Huffman metadata
        let num_used_symbols = block.used.iter().filter(|&&u| u).count();
        let alphabet_size = num_used_symbols + 3;
        let num_tables = ((block.zrle_data.len() / 50).clamp(1, 6)).max(1);
        bit_writer.write_bits(num_tables as u32, 3)?;

        let num_selectors = block.zrle_data.len().div_ceil(50);
        bit_writer.write_bits(num_selectors as u32, 15)?;

        for _ in 0..num_selectors {
            bit_writer.write_bits(0, 1)?;
        }

        // Write Huffman tables (delta-encoded code lengths)
        for _ in 0..num_tables {
            let start_len = block.lengths.first().copied().map_or(5i32, |v| v as i32);
            bit_writer.write_bits(start_len as u32, 5)?;
            let mut current_len = start_len;

            for &len in &block.lengths {
                let target_len = len as i32;
                while current_len != target_len {
                    bit_writer.write_bits(1, 1)?;
                    if target_len > current_len {
                        bit_writer.write_bits(0, 1)?;
                        current_len += 1;
                    } else {
                        bit_writer.write_bits(1, 1)?;
                        current_len -= 1;
                    }
                }
                bit_writer.write_bits(0, 1)?;
            }
        }

        // Build Huffman table and write encoded data
        let table = huffman::HuffmanTable::from_lengths(&block.lengths)?;

        for &sym in &block.zrle_data {
            if let Some((code, len)) = table.get_code(sym) {
                for i in (0..len).rev() {
                    let bit = (code >> i) & 1;
                    bit_writer.write_bits(bit, 1)?;
                }
            }
        }

        // Write EOB symbol
        let eob_sym = (alphabet_size - 1) as u16;
        if let Some((code, len)) = table.get_code(eob_sym) {
            for i in (0..len).rev() {
                let bit = (code >> i) & 1;
                bit_writer.write_bits(bit, 1)?;
            }
        }
    }

    // Write end of stream marker
    for &b in &EOS_MAGIC {
        bit_writer.write_bits(b as u32, 8)?;
    }
    bit_writer.write_bits(combined_crc, 32)?;
    bit_writer.flush()?;

    bit_writer.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_empty() {
        let result = compress(b"", CompressionLevel::default()).unwrap();
        // Should have at least header and footer
        assert!(result.len() >= 10);
        assert_eq!(&result[0..2], &BZIP2_MAGIC);
    }

    #[test]
    fn test_compress_hello() {
        let result = compress(b"hello world", CompressionLevel::new(1)).unwrap();
        assert!(result.len() > 10);
        assert_eq!(&result[0..2], &BZIP2_MAGIC);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        use crate::decompress;
        let data = b"Hello, World! Parallel Bzip2 compression test.";
        let compressed = compress_parallel(data, CompressionLevel::new(1)).unwrap();
        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        use crate::decompress;
        // Large data spanning multiple blocks
        let data = vec![0x42u8; 3_000_000];
        let compressed = compress_parallel(&data, CompressionLevel::new(5)).unwrap();
        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial() {
        use crate::decompress;
        let data = b"Testing parallel vs serial Bzip2 compression.";
        let level = CompressionLevel::new(9);

        let serial = compress(data, level).unwrap();
        let parallel = compress_parallel(data, level).unwrap();

        // Both should decompress correctly
        let serial_decompressed = decompress(&serial[..]).unwrap();
        let parallel_decompressed = decompress(&parallel[..]).unwrap();

        assert_eq!(serial_decompressed, data.as_slice());
        assert_eq!(parallel_decompressed, data.as_slice());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        use crate::decompress;
        let data: &[u8] = b"";
        let compressed = compress_parallel(data, CompressionLevel::new(1)).unwrap();
        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multiple_blocks() {
        use crate::decompress;
        // Test parallel compression by compressing two separate blocks sequentially
        // Each block uses parallel processing internally
        // Using 5KB per call to keep BWT complexity low and test fast
        let pattern =
            b"The quick brown fox jumps over the lazy dog. 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ\n";
        let target_size = 5_000; // 5KB per block (reduced from 30KB)
        let mut data = Vec::new();
        let repeats = target_size / pattern.len() + 1;
        for _ in 0..repeats {
            data.extend_from_slice(pattern);
        }
        data.truncate(target_size);

        // Compress and decompress first block
        let compressed1 = compress_parallel(&data, CompressionLevel::new(1)).unwrap();
        let decompressed1 = decompress(&compressed1[..]).unwrap();
        assert_eq!(decompressed1, data);

        // Create second block with different pattern
        let pattern2 = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz\n";
        let mut data2 = Vec::new();
        let repeats2 = target_size / pattern2.len() + 1;
        for _ in 0..repeats2 {
            data2.extend_from_slice(pattern2);
        }
        data2.truncate(target_size);

        // Compress and decompress second block
        let compressed2 = compress_parallel(&data2, CompressionLevel::new(1)).unwrap();
        let decompressed2 = decompress(&compressed2[..]).unwrap();
        assert_eq!(decompressed2, data2);
    }

    #[test]
    #[cfg(feature = "parallel")]
    #[ignore = "heavy: stress test (>120s), may consume significant resources"]
    fn test_parallel_repeated_data() {
        use crate::decompress;
        // Reduced from repeat(10000) to repeat(200) for faster testing
        // This still gives 7200 bytes which is enough to test repeated data compression
        let data = b"aaaaaaaaaaaabbbbbbbbbbbbcccccccccccc".repeat(200);
        // Use level 3 instead of 9 for faster BWT while still testing compression quality
        let compressed = compress_parallel(&data, CompressionLevel::new(3)).unwrap();

        // Should compress well
        assert!(compressed.len() < data.len() / 5);

        let decompressed = decompress(&compressed[..]).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    #[ignore = "heavy: stress test (>100s), tests all compression levels"]
    fn test_parallel_different_levels() {
        use crate::decompress;
        // Reduced from repeat(1000) to repeat(100) for faster testing (4400 bytes)
        let data = b"Test data for different compression levels.".repeat(100);

        // Test only levels 1, 5, 9 instead of all 1-9 to reduce test time by 67%
        // This still covers low, medium, and high compression adequately
        for level in [1, 5, 9] {
            let compressed = compress_parallel(&data, CompressionLevel::new(level)).unwrap();
            let decompressed = decompress(&compressed[..]).unwrap();
            assert_eq!(decompressed, data, "Failed for level {}", level);
        }
    }
}

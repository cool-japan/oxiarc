//! BZip2 encoder.

use crate::{BLOCK_MAGIC, BZIP2_MAGIC, CompressionLevel, EOS_MAGIC, bwt, huffman, mtf, rle};
use oxiarc_core::error::Result;
use oxiarc_core::{BitWriter, Crc32};
use std::io::Write;

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
            let start_len = lengths.first().copied().unwrap_or(5) as i32;
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
}

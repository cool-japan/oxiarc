//! DEFLATE compression.
//!
//! This module implements DEFLATE compression as specified in RFC 1951.
//! It supports:
//! - Stored blocks (no compression)
//! - Fixed Huffman codes
//! - Dynamic Huffman codes

use crate::huffman::HuffmanBuilder;
use crate::lz77::{Lz77Encoder, Lz77Token};
use crate::tables::{distance_to_code, fixed_litlen_lengths, length_to_code};
use oxiarc_core::BitWriter;
use oxiarc_core::error::Result;
use oxiarc_core::traits::{CompressStatus, Compressor, FlushMode};
use std::io::Write;

/// Code length alphabet order for encoding (RFC 1951).
const CODELEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// DEFLATE compressor.
#[derive(Debug)]
pub struct Deflater {
    /// LZ77 encoder.
    lz77: Lz77Encoder,
    /// Compression level.
    level: u8,
    /// Whether compression is finished.
    finished: bool,
}

impl Deflater {
    /// Create a new DEFLATE compressor with the specified level (0-9).
    pub fn new(level: u8) -> Self {
        Self {
            lz77: Lz77Encoder::with_level(level),
            level: level.min(9),
            finished: false,
        }
    }

    /// Reset the compressor.
    pub fn reset(&mut self) {
        self.lz77.reset();
        self.finished = false;
    }

    /// Compress data.
    pub fn deflate<W: Write>(&mut self, data: &[u8], writer: &mut W, finish: bool) -> Result<()> {
        let mut bit_writer = BitWriter::new(writer);

        if self.level == 0 {
            // Store only
            self.write_stored_blocks(data, &mut bit_writer, finish)?;
        } else {
            // Compress with LZ77 and fixed Huffman codes
            self.write_compressed_block(data, &mut bit_writer, finish)?;
        }

        if finish {
            bit_writer.flush()?;
            self.finished = true;
        }

        Ok(())
    }

    /// Write stored (uncompressed) blocks.
    fn write_stored_blocks<W: Write>(
        &self,
        data: &[u8],
        writer: &mut BitWriter<W>,
        is_final: bool,
    ) -> Result<()> {
        const MAX_STORED_BLOCK: usize = 65535;

        let mut offset = 0;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let block_size = remaining.min(MAX_STORED_BLOCK);
            let final_block = is_final && (offset + block_size >= data.len());

            // Block header
            writer.write_bit(final_block)?;
            writer.write_bits(0b00, 2)?; // BTYPE=00 (stored)

            // Align to byte boundary
            writer.align_to_byte()?;

            // LEN and NLEN
            let len = block_size as u16;
            let nlen = !len;
            writer.write_bits(len as u32, 16)?;
            writer.write_bits(nlen as u32, 16)?;

            // Data
            writer.write_bytes(&data[offset..offset + block_size])?;

            offset += block_size;
        }

        // Handle empty input
        if data.is_empty() && is_final {
            writer.write_bit(true)?; // BFINAL=1
            writer.write_bits(0b00, 2)?; // BTYPE=00
            writer.align_to_byte()?;
            writer.write_bits(0, 16)?; // LEN=0
            writer.write_bits(0xFFFF, 16)?; // NLEN=0xFFFF
        }

        Ok(())
    }

    /// Write a compressed block, choosing between fixed and dynamic Huffman.
    fn write_compressed_block<W: Write>(
        &mut self,
        data: &[u8],
        writer: &mut BitWriter<W>,
        is_final: bool,
    ) -> Result<()> {
        // Compress with LZ77
        let tokens = self.lz77.compress(data);

        // Count symbol frequencies
        let (litlen_freq, dist_freq) = Self::count_frequencies(&tokens);

        // Build dynamic Huffman codes
        let mut litlen_builder = HuffmanBuilder::new(286, 15);
        for (sym, &freq) in litlen_freq.iter().enumerate() {
            if freq > 0 {
                litlen_builder.add_count(sym as u16, freq);
            }
        }
        // Always include EOB
        if litlen_freq[256] == 0 {
            litlen_builder.add_count(256, 1);
        }
        let dynamic_litlen_lengths = litlen_builder.build_lengths();

        let mut dist_builder = HuffmanBuilder::new(30, 15);
        for (sym, &freq) in dist_freq.iter().enumerate() {
            if freq > 0 {
                dist_builder.add_count(sym as u16, freq);
            }
        }
        let dynamic_dist_lengths = dist_builder.build_lengths();

        // Estimate sizes for fixed vs dynamic
        let fixed_size = self.estimate_fixed_size(&tokens);
        let (dynamic_size, header_size) =
            self.estimate_dynamic_size(&tokens, &dynamic_litlen_lengths, &dynamic_dist_lengths);

        // Choose better option (dynamic if it saves bytes)
        let use_dynamic = self.level >= 5 && (dynamic_size + header_size) < fixed_size;

        if use_dynamic {
            self.write_dynamic_block(
                writer,
                &tokens,
                &dynamic_litlen_lengths,
                &dynamic_dist_lengths,
                is_final,
            )?;
        } else {
            self.write_fixed_block(writer, &tokens, is_final)?;
        }

        Ok(())
    }

    /// Count symbol frequencies in tokens.
    fn count_frequencies(tokens: &[Lz77Token]) -> ([u32; 286], [u32; 30]) {
        let mut litlen_freq = [0u32; 286];
        let mut dist_freq = [0u32; 30];

        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    litlen_freq[*byte as usize] += 1;
                }
                Lz77Token::Match { length, distance } => {
                    let (len_code, _, _) = length_to_code(*length);
                    litlen_freq[len_code as usize] += 1;

                    let (dist_code, _, _) = distance_to_code(*distance);
                    dist_freq[dist_code as usize] += 1;
                }
            }
        }
        // EOB symbol
        litlen_freq[256] += 1;

        (litlen_freq, dist_freq)
    }

    /// Estimate bit size using fixed Huffman codes.
    fn estimate_fixed_size(&self, tokens: &[Lz77Token]) -> usize {
        let litlen_lengths = fixed_litlen_lengths();
        let mut bits = 3; // Block header

        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    bits += litlen_lengths[*byte as usize] as usize;
                }
                Lz77Token::Match { length, distance } => {
                    let (len_code, len_extra_bits, _) = length_to_code(*length);
                    bits += litlen_lengths[len_code as usize] as usize + len_extra_bits as usize;

                    let (_, dist_extra_bits, _) = distance_to_code(*distance);
                    bits += 5 + dist_extra_bits as usize; // Fixed distance is 5 bits
                }
            }
        }
        bits += litlen_lengths[256] as usize; // EOB

        bits
    }

    /// Estimate bit size using dynamic Huffman codes.
    fn estimate_dynamic_size(
        &self,
        tokens: &[Lz77Token],
        litlen_lengths: &[u8],
        dist_lengths: &[u8],
    ) -> (usize, usize) {
        let mut data_bits = 0;

        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    let len = litlen_lengths.get(*byte as usize).copied().unwrap_or(0);
                    if len > 0 {
                        data_bits += len as usize;
                    }
                }
                Lz77Token::Match { length, distance } => {
                    let (len_code, len_extra_bits, _) = length_to_code(*length);
                    let len = litlen_lengths.get(len_code as usize).copied().unwrap_or(0);
                    if len > 0 {
                        data_bits += len as usize + len_extra_bits as usize;
                    }

                    let (dist_code, dist_extra_bits, _) = distance_to_code(*distance);
                    let dlen = dist_lengths.get(dist_code as usize).copied().unwrap_or(0);
                    if dlen > 0 {
                        data_bits += dlen as usize + dist_extra_bits as usize;
                    }
                }
            }
        }

        // EOB
        let eob_len = litlen_lengths.get(256).copied().unwrap_or(0);
        data_bits += eob_len as usize;

        // Estimate header size (rough approximation)
        let header_bits =
            3 + 5 + 5 + 4 + 19 * 3 + litlen_lengths.len() * 4 + dist_lengths.len() * 4;

        (data_bits, header_bits)
    }

    /// Write a block using fixed Huffman codes.
    fn write_fixed_block<W: Write>(
        &self,
        writer: &mut BitWriter<W>,
        tokens: &[Lz77Token],
        is_final: bool,
    ) -> Result<()> {
        // Block header
        writer.write_bit(is_final)?;
        writer.write_bits(0b01, 2)?; // BTYPE=01 (fixed Huffman)

        // Get fixed Huffman code lengths
        let litlen_lengths = fixed_litlen_lengths();

        // Build encoding table
        let mut codes = [[0u32; 2]; 288]; // [code, length]
        Self::build_codes(&litlen_lengths, &mut codes);

        // Write tokens
        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    let [code, len] = codes[*byte as usize];
                    Self::write_huffman_code(writer, code, len as u8)?;
                }
                Lz77Token::Match { length, distance } => {
                    // Write length code
                    let (len_code, len_extra_bits, len_extra) = length_to_code(*length);
                    let [code, len] = codes[len_code as usize];
                    Self::write_huffman_code(writer, code, len as u8)?;

                    // Write length extra bits
                    if len_extra_bits > 0 {
                        writer.write_bits(len_extra as u32, len_extra_bits)?;
                    }

                    // Write distance code (fixed: 5 bits each)
                    let (dist_code, dist_extra_bits, dist_extra) = distance_to_code(*distance);
                    // Fixed distance codes are 5 bits, reversed
                    let reversed_dist = Self::reverse_bits(dist_code as u32, 5);
                    writer.write_bits(reversed_dist, 5)?;

                    // Write distance extra bits
                    if dist_extra_bits > 0 {
                        writer.write_bits(dist_extra as u32, dist_extra_bits)?;
                    }
                }
            }
        }

        // Write end of block
        let [code, len] = codes[256]; // EOB symbol
        Self::write_huffman_code(writer, code, len as u8)?;

        Ok(())
    }

    /// Write a block using dynamic Huffman codes.
    fn write_dynamic_block<W: Write>(
        &self,
        writer: &mut BitWriter<W>,
        tokens: &[Lz77Token],
        litlen_lengths: &[u8],
        dist_lengths: &[u8],
        is_final: bool,
    ) -> Result<()> {
        // Block header
        writer.write_bit(is_final)?;
        writer.write_bits(0b10, 2)?; // BTYPE=10 (dynamic Huffman)

        // Find HLIT and HDIST (number of codes - base)
        let hlit = Self::find_last_nonzero(litlen_lengths, 257).saturating_sub(257);
        let hdist = Self::find_last_nonzero(dist_lengths, 1).saturating_sub(1);

        // Encode code lengths with RLE
        let combined_lengths = Self::combine_lengths(litlen_lengths, dist_lengths, hlit, hdist);
        let (codelen_symbols, codelen_freqs) = Self::rle_encode_lengths(&combined_lengths);

        // Build code length tree
        let mut codelen_builder = HuffmanBuilder::new(19, 7);
        for (sym, &freq) in codelen_freqs.iter().enumerate() {
            if freq > 0 {
                codelen_builder.add_count(sym as u16, freq);
            }
        }
        let codelen_lengths = codelen_builder.build_lengths();

        // Find HCLEN
        let hclen = Self::find_hclen(&codelen_lengths);

        // Write header values
        writer.write_bits(hlit as u32, 5)?; // HLIT
        writer.write_bits(hdist as u32, 5)?; // HDIST
        writer.write_bits(hclen as u32, 4)?; // HCLEN

        // Write code length code lengths
        for i in 0..hclen + 4 {
            let len = codelen_lengths[CODELEN_ORDER[i]];
            writer.write_bits(len as u32, 3)?;
        }

        // Build codes for code lengths
        let mut codelen_codes = [[0u32; 2]; 19];
        Self::build_codes(&codelen_lengths, &mut codelen_codes);

        // Write encoded lengths
        for (sym, extra, extra_bits) in &codelen_symbols {
            let [code, len] = codelen_codes[*sym as usize];
            if len > 0 {
                Self::write_huffman_code(writer, code, len as u8)?;
                if *extra_bits > 0 {
                    writer.write_bits(*extra as u32, *extra_bits)?;
                }
            }
        }

        // Build litlen and distance codes
        let mut litlen_codes = [[0u32; 2]; 288];
        Self::build_codes(litlen_lengths, &mut litlen_codes);

        let mut dist_codes = [[0u32; 2]; 30];
        Self::build_codes(dist_lengths, &mut dist_codes);

        // Write tokens
        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    let [code, len] = litlen_codes[*byte as usize];
                    if len > 0 {
                        Self::write_huffman_code(writer, code, len as u8)?;
                    }
                }
                Lz77Token::Match { length, distance } => {
                    // Write length code
                    let (len_code, len_extra_bits, len_extra) = length_to_code(*length);
                    let [code, len] = litlen_codes[len_code as usize];
                    if len > 0 {
                        Self::write_huffman_code(writer, code, len as u8)?;
                        if len_extra_bits > 0 {
                            writer.write_bits(len_extra as u32, len_extra_bits)?;
                        }
                    }

                    // Write distance code
                    let (dist_code, dist_extra_bits, dist_extra) = distance_to_code(*distance);
                    let [dcode, dlen] = dist_codes[dist_code as usize];
                    if dlen > 0 {
                        Self::write_huffman_code(writer, dcode, dlen as u8)?;
                        if dist_extra_bits > 0 {
                            writer.write_bits(dist_extra as u32, dist_extra_bits)?;
                        }
                    }
                }
            }
        }

        // Write end of block
        let [code, len] = litlen_codes[256];
        if len > 0 {
            Self::write_huffman_code(writer, code, len as u8)?;
        }

        Ok(())
    }

    /// Find the last non-zero length index, with minimum.
    fn find_last_nonzero(lengths: &[u8], min: usize) -> usize {
        let mut last = min;
        for (i, &len) in lengths.iter().enumerate() {
            if len > 0 && i >= min {
                last = i + 1;
            }
        }
        last.max(min)
    }

    /// Combine literal/length and distance lengths.
    fn combine_lengths(
        litlen_lengths: &[u8],
        dist_lengths: &[u8],
        hlit: usize,
        hdist: usize,
    ) -> Vec<u8> {
        let mut combined = Vec::with_capacity(hlit + 257 + hdist + 1);
        combined.extend_from_slice(&litlen_lengths[..hlit + 257]);
        combined.extend_from_slice(&dist_lengths[..hdist + 1]);
        combined
    }

    /// RLE encode code lengths.
    /// Returns (symbol, extra_value, extra_bits) tuples and frequency counts.
    fn rle_encode_lengths(lengths: &[u8]) -> (Vec<(u8, u8, u8)>, [u32; 19]) {
        let mut symbols = Vec::new();
        let mut freqs = [0u32; 19];
        let mut i = 0;

        while i < lengths.len() {
            let len = lengths[i];

            // Count repeats
            let mut count = 1;
            while i + count < lengths.len() && lengths[i + count] == len && count < 138 {
                count += 1;
            }

            if len == 0 {
                // Encode zeros
                while count > 0 {
                    if count >= 11 {
                        // Use symbol 18 (11-138 zeros)
                        let run = count.min(138);
                        symbols.push((18, (run - 11) as u8, 7));
                        freqs[18] += 1;
                        count -= run;
                    } else if count >= 3 {
                        // Use symbol 17 (3-10 zeros)
                        let run = count.min(10);
                        symbols.push((17, (run - 3) as u8, 3));
                        freqs[17] += 1;
                        count -= run;
                    } else {
                        // Output individual zeros
                        symbols.push((0, 0, 0));
                        freqs[0] += 1;
                        count -= 1;
                    }
                }
            } else {
                // Output the first occurrence
                symbols.push((len, 0, 0));
                freqs[len as usize] += 1;
                count -= 1;

                // Encode repeats with symbol 16
                while count > 0 {
                    if count >= 3 {
                        let run = count.min(6);
                        symbols.push((16, (run - 3) as u8, 2));
                        freqs[16] += 1;
                        count -= run;
                    } else {
                        symbols.push((len, 0, 0));
                        freqs[len as usize] += 1;
                        count -= 1;
                    }
                }
            }

            i += lengths[i..].iter().take_while(|&&l| l == len).count();
        }

        (symbols, freqs)
    }

    /// Find HCLEN (number of code length codes - 4).
    fn find_hclen(codelen_lengths: &[u8]) -> usize {
        let mut hclen = 15; // Maximum is 19-4=15
        for i in (0..=15).rev() {
            if codelen_lengths[CODELEN_ORDER[i + 4 - 1]] != 0 {
                hclen = i;
                break;
            }
        }
        hclen.max(0)
    }

    /// Build canonical Huffman codes from lengths.
    fn build_codes(lengths: &[u8], codes: &mut [[u32; 2]]) {
        // Count codes of each length
        let mut bl_count = [0u32; 16];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Calculate starting codes
        let mut next_code = [0u32; 16];
        let mut code = 0u32;
        for bits in 1..16 {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Assign codes
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 && symbol < codes.len() {
                let code = next_code[len as usize];
                next_code[len as usize] += 1;
                // Reverse for LSB-first output
                codes[symbol] = [Self::reverse_bits(code, len), len as u32];
            }
        }
    }

    /// Reverse bits in a value.
    fn reverse_bits(mut value: u32, length: u8) -> u32 {
        let mut result = 0u32;
        for _ in 0..length {
            result = (result << 1) | (value & 1);
            value >>= 1;
        }
        result
    }

    /// Write a Huffman code (already reversed for LSB-first).
    fn write_huffman_code<W: Write>(
        writer: &mut BitWriter<W>,
        code: u32,
        length: u8,
    ) -> Result<()> {
        writer.write_bits(code, length)?;
        Ok(())
    }

    /// Compress data to a Vec.
    pub fn compress_to_vec(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.deflate(data, &mut output, true)?;
        Ok(output)
    }
}

impl Default for Deflater {
    fn default() -> Self {
        Self::new(6)
    }
}

impl Compressor for Deflater {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        if self.finished {
            return Ok((0, 0, CompressStatus::Done));
        }

        let finish = matches!(flush, FlushMode::Finish);

        let mut buffer = Vec::new();
        self.deflate(input, &mut buffer, finish)?;

        let to_copy = buffer.len().min(output.len());
        output[..to_copy].copy_from_slice(&buffer[..to_copy]);

        let status = if finish {
            CompressStatus::Done
        } else if to_copy < buffer.len() {
            CompressStatus::NeedsOutput
        } else {
            CompressStatus::NeedsInput
        };

        Ok((input.len(), to_copy, status))
    }

    fn reset(&mut self) {
        Deflater::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Compress data using DEFLATE.
pub fn deflate(data: &[u8], level: u8) -> Result<Vec<u8>> {
    let mut deflater = Deflater::new(level);
    deflater.compress_to_vec(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::inflate;

    #[test]
    fn test_deflate_stored() {
        let input = b"Hello, World!";
        let compressed = deflate(input, 0).unwrap();

        // Decompress and verify
        let decompressed = inflate(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_deflate_compressed() {
        let input = b"AAAAAAAAAABBBBBBBBBBCCCCCCCCCC";
        let compressed = deflate(input, 6).unwrap();

        // Should be smaller than input
        assert!(
            compressed.len() < input.len(),
            "Compressed {} bytes to {} bytes",
            input.len(),
            compressed.len()
        );

        // Decompress and verify
        let decompressed = inflate(&compressed).unwrap();
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_deflate_empty() {
        let input = b"";
        let compressed = deflate(input, 0).unwrap();
        let decompressed = inflate(&compressed).unwrap();
        assert!(decompressed.is_empty());
    }

    #[test]
    fn test_deflate_roundtrip() {
        let inputs = [
            b"Hello".to_vec(),
            b"The quick brown fox jumps over the lazy dog".to_vec(),
            vec![0u8; 1000],
            (0..=255).collect::<Vec<u8>>(),
        ];

        for input in &inputs {
            for level in [0, 1, 6, 9] {
                let compressed = deflate(input, level).unwrap();
                let decompressed = inflate(&compressed).unwrap();
                assert_eq!(
                    &decompressed,
                    input,
                    "Roundtrip failed for level {} with {} bytes",
                    level,
                    input.len()
                );
            }
        }
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(Deflater::reverse_bits(0b101, 3), 0b101);
        assert_eq!(Deflater::reverse_bits(0b1100, 4), 0b0011);
        assert_eq!(Deflater::reverse_bits(0b10101010, 8), 0b01010101);
    }

    #[test]
    fn test_deflate_dynamic_huffman() {
        // Large repeating data should trigger dynamic Huffman at level 5+
        let input = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                      BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
                      CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC\
                      DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";

        let compressed_dynamic = deflate(input, 9).unwrap();
        let compressed_fixed = deflate(input, 1).unwrap();

        // Dynamic Huffman should compress better for this pattern
        assert!(
            compressed_dynamic.len() <= compressed_fixed.len(),
            "Dynamic ({} bytes) should be <= fixed ({} bytes)",
            compressed_dynamic.len(),
            compressed_fixed.len()
        );

        // Both should decompress correctly
        let decompressed_dynamic = inflate(&compressed_dynamic).unwrap();
        let decompressed_fixed = inflate(&compressed_fixed).unwrap();
        assert_eq!(decompressed_dynamic, input);
        assert_eq!(decompressed_fixed, input);
    }

    #[test]
    fn test_deflate_level_comparison() {
        let input = vec![b'A'; 1000];

        let mut prev_size = usize::MAX;
        for level in [0, 1, 5, 9] {
            let compressed = deflate(&input, level).unwrap();
            let decompressed = inflate(&compressed).unwrap();
            assert_eq!(decompressed, input);

            // Higher levels should generally compress better (or equal)
            // Level 0 is stored, so it will be larger
            if level > 0 {
                assert!(
                    compressed.len() <= prev_size,
                    "Level {} ({} bytes) should compress <= previous ({} bytes)",
                    level,
                    compressed.len(),
                    prev_size
                );
            }
            if level > 0 {
                prev_size = compressed.len();
            }
        }
    }
}

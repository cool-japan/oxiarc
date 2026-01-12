//! LZH compression (encoding).
//!
//! This module implements LZH compression for methods lh4-lh7.

use crate::lzss::{LzssEncoder, LzssToken};
use crate::methods::LzhMethod;
use crate::methods::constants::{NC, NT};
use oxiarc_core::BitWriter;
use oxiarc_core::error::Result;
use oxiarc_core::traits::{CompressStatus, Compressor, FlushMode};
use std::io::Write;

/// Maximum code length for Huffman codes.
const MAX_CODE_LEN: usize = 16;

/// Block size for encoding.
const BLOCK_SIZE: usize = 0x4000; // 16KB

/// LZH encoder.
#[derive(Debug)]
pub struct LzhEncoder {
    /// Compression method.
    method: LzhMethod,
    /// LZSS encoder.
    lzss: LzssEncoder,
    /// Whether encoding is finished.
    finished: bool,
}

impl LzhEncoder {
    /// Create a new LZH encoder.
    pub fn new(method: LzhMethod) -> Self {
        let window_size = method.window_size().max(256);
        let min_match = method.min_match();
        let max_match = method.max_match();

        Self {
            method,
            lzss: LzssEncoder::new(window_size, min_match, max_match),
            finished: false,
        }
    }

    /// Create a default encoder (lh5).
    pub fn lh5() -> Self {
        Self::new(LzhMethod::Lh5)
    }

    /// Reset the encoder.
    pub fn reset(&mut self) {
        self.lzss.reset();
        self.finished = false;
    }

    /// Encode data.
    pub fn encode<W: Write>(&mut self, data: &[u8], writer: &mut W, finish: bool) -> Result<()> {
        if self.method.is_stored() {
            // lh0: just copy data
            writer.write_all(data)?;
            if finish {
                self.finished = true;
            }
            return Ok(());
        }

        let mut bit_writer = BitWriter::new(writer);

        // Get LZSS tokens
        let tokens = self.lzss.encode(data);

        // Encode tokens in blocks
        let np = self.get_np();
        self.encode_tokens(&tokens, &mut bit_writer, np)?;

        if finish {
            bit_writer.flush()?;
            self.finished = true;
        }

        Ok(())
    }

    /// Get number of position codes for this method.
    fn get_np(&self) -> usize {
        match self.method {
            LzhMethod::Lh4 | LzhMethod::Lh5 => 14,
            LzhMethod::Lh6 => 16,
            LzhMethod::Lh7 => 17,
            LzhMethod::Lh0 => 0,
        }
    }

    /// Encode tokens to the bitstream.
    fn encode_tokens<W: Write>(
        &self,
        tokens: &[LzssToken],
        writer: &mut BitWriter<W>,
        np: usize,
    ) -> Result<()> {
        if tokens.is_empty() {
            return Ok(());
        }

        // Process in blocks
        let mut pos = 0;
        while pos < tokens.len() {
            let block_end = (pos + BLOCK_SIZE).min(tokens.len());
            let block_tokens = &tokens[pos..block_end];

            // Count uncompressed size of this block
            let mut block_size = 0usize;
            for token in block_tokens {
                match token {
                    LzssToken::Literal(_) => block_size += 1,
                    LzssToken::Match { length, .. } => block_size += *length as usize,
                }
            }

            self.encode_block(block_tokens, writer, np, block_size)?;
            pos = block_end;
        }

        Ok(())
    }

    /// Encode a single block of tokens.
    fn encode_block<W: Write>(
        &self,
        tokens: &[LzssToken],
        writer: &mut BitWriter<W>,
        np: usize,
        block_size: usize,
    ) -> Result<()> {
        // Build frequency tables
        let mut c_freq = vec![0u32; NC];
        let mut p_freq = vec![0u32; np];

        for token in tokens {
            match token {
                LzssToken::Literal(b) => {
                    c_freq[*b as usize] += 1;
                }
                LzssToken::Match { length, distance } => {
                    // Length code: length - 3 + 256
                    let len_code = (*length as usize - 3 + 256).min(NC - 1);
                    c_freq[len_code] += 1;

                    // Position code
                    let p_code = Self::get_position_code(*distance);
                    if (p_code as usize) < np {
                        p_freq[p_code as usize] += 1;
                    }
                }
            }
        }

        // Build Huffman code lengths
        let c_lengths = Self::build_code_lengths(&c_freq, MAX_CODE_LEN);
        let p_lengths = Self::build_code_lengths(&p_freq, MAX_CODE_LEN);

        // Build Huffman codes
        let c_codes = Self::build_codes(&c_lengths);
        let p_codes = Self::build_codes(&p_lengths);

        // Write block size
        writer.write_bits(block_size as u32, 16)?;

        // Write C-tree
        self.write_c_tree(writer, &c_lengths)?;

        // Write P-tree
        self.write_p_tree(writer, &p_lengths, np)?;

        // Encode tokens using Huffman codes
        for token in tokens {
            match token {
                LzssToken::Literal(b) => {
                    let code = c_codes[*b as usize];
                    let len = c_lengths[*b as usize];
                    if len > 0 {
                        Self::write_code(writer, code, len)?;
                    }
                }
                LzssToken::Match { length, distance } => {
                    // Write length code
                    let len_code = (*length as usize - 3 + 256).min(NC - 1);
                    let code = c_codes[len_code];
                    let len = c_lengths[len_code];
                    if len > 0 {
                        Self::write_code(writer, code, len)?;
                    }

                    // Write position code
                    let p_code = Self::get_position_code(*distance);
                    if (p_code as usize) < np && p_lengths[p_code as usize] > 0 {
                        Self::write_code(
                            writer,
                            p_codes[p_code as usize],
                            p_lengths[p_code as usize],
                        )?;

                        // Write extra bits for distance
                        // For p_code >= 1, we have p_code extra bits
                        // distance = (1 << p_code) + extra_value
                        if p_code > 0 {
                            let extra_bits = p_code;
                            let extra_value = *distance - (1 << p_code);
                            writer.write_bits(extra_value as u32, extra_bits)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Get position code from distance.
    fn get_position_code(distance: u16) -> u8 {
        if distance == 0 {
            return 0;
        }
        // Position code is floor(log2(distance))
        let mut p = 0u8;
        let mut d = distance;
        while d > 1 {
            d >>= 1;
            p += 1;
        }
        p
    }

    /// Build Huffman code lengths from frequencies using standard Huffman algorithm.
    fn build_code_lengths(freqs: &[u32], max_len: usize) -> Vec<u8> {
        let n = freqs.len();
        if n == 0 {
            return Vec::new();
        }

        // Collect non-zero frequency symbols
        let symbols: Vec<(usize, u32)> = freqs
            .iter()
            .enumerate()
            .filter(|&(_, f)| *f > 0)
            .map(|(i, f)| (i, *f))
            .collect();

        let mut lengths = vec![0u8; n];

        if symbols.is_empty() {
            return lengths;
        }

        if symbols.len() == 1 {
            lengths[symbols[0].0] = 1;
            return lengths;
        }

        if symbols.len() == 2 {
            lengths[symbols[0].0] = 1;
            lengths[symbols[1].0] = 1;
            return lengths;
        }

        // Build Huffman tree using a priority queue approach
        // Each node is (frequency, depth, symbols)
        // We track the depth of each symbol as we merge

        // Initialize: each symbol is a leaf at depth 0
        let mut nodes: Vec<(u64, Vec<usize>)> = symbols
            .iter()
            .map(|&(sym, freq)| (freq as u64, vec![sym]))
            .collect();

        // Sort by frequency (ascending)
        nodes.sort_by_key(|&(freq, _)| freq);

        // Merge nodes until only one remains
        while nodes.len() > 1 {
            // Pop two smallest
            let (freq1, syms1) = nodes.remove(0);
            let (freq2, syms2) = nodes.remove(0);

            // Merge: combined frequency, all symbols increase depth by 1
            let combined_freq = freq1 + freq2;
            let mut combined_syms = syms1;
            combined_syms.extend(syms2);

            // Increase depth for all symbols in this merge
            for &sym in &combined_syms {
                lengths[sym] += 1;
            }

            // Insert merged node in sorted position
            let pos = nodes
                .iter()
                .position(|&(f, _)| f > combined_freq)
                .unwrap_or(nodes.len());
            nodes.insert(pos, (combined_freq, combined_syms));
        }

        // Limit lengths to max_len using a simple approach
        Self::limit_code_lengths(&mut lengths, max_len);

        lengths
    }

    /// Limit code lengths to max_len while maintaining valid Huffman property.
    fn limit_code_lengths(lengths: &mut [u8], max_len: usize) {
        let max_len = max_len as u8;

        // First pass: find codes that exceed max_len
        let mut overflow = false;
        for l in lengths.iter() {
            if *l > max_len {
                overflow = true;
                break;
            }
        }

        if !overflow {
            return;
        }

        // Collect (symbol, length) pairs for non-zero lengths
        let mut items: Vec<(usize, u8)> = lengths
            .iter()
            .enumerate()
            .filter(|&(_, l)| *l > 0)
            .map(|(i, l)| (i, *l))
            .collect();

        // Sort by length descending
        items.sort_by(|a, b| b.1.cmp(&a.1));

        // Cap lengths at max_len
        for &mut (sym, ref mut len) in &mut items {
            if *len > max_len {
                *len = max_len;
                lengths[sym] = max_len;
            }
        }

        // Now we need to fix the Kraft inequality
        // The sum of 2^(-l_i) must equal 1 for a complete code
        // If it's less than 1, we need to shorten some codes

        loop {
            // Calculate Kraft sum using integer arithmetic (scaled by 2^max_len)
            let scale = 1u64 << max_len;
            let kraft_sum: u64 = lengths
                .iter()
                .filter(|&&l| l > 0)
                .map(|&l| scale >> l)
                .sum();

            if kraft_sum <= scale {
                break; // Valid
            }

            // Find the longest code and increase it by 1 (if possible)
            // Actually, we need to redistribute - increase some lengths
            let mut increased = false;
            for len in lengths.iter_mut() {
                if *len > 0 && *len < max_len {
                    *len += 1;
                    increased = true;
                    break;
                }
            }

            if !increased {
                // All codes are at max_len, can't fix
                break;
            }
        }
    }

    /// Build canonical Huffman codes from lengths.
    fn build_codes(lengths: &[u8]) -> Vec<u32> {
        let n = lengths.len();
        let mut codes = vec![0u32; n];

        if n == 0 {
            return codes;
        }

        // Count codes of each length
        let max_len = *lengths.iter().max().unwrap_or(&0) as usize;
        let mut bl_count = vec![0u32; max_len + 1];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Calculate starting codes
        let mut next_code = vec![0u32; max_len + 1];
        let mut code = 0u32;
        for bits in 1..=max_len {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Assign codes
        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                codes[sym] = next_code[len as usize];
                next_code[len as usize] += 1;
            }
        }

        codes
    }

    /// Write a Huffman code (MSB-first, reversed for LZH format).
    fn write_code<W: Write>(writer: &mut BitWriter<W>, code: u32, len: u8) -> Result<()> {
        // LZH uses LSB-first bit packing, so we write bits in reverse order
        for i in (0..len).rev() {
            let bit = (code >> i) & 1;
            writer.write_bits(bit, 1)?;
        }
        Ok(())
    }

    /// Write C-tree (character/length Huffman tree).
    /// Format matches decoder's read_c_tree:
    /// 1. n (9 bits) - number of codes
    /// 2. If n == 0: single code value (9 bits)
    /// 3. If n > 0: PT-tree, then encoded lengths
    fn write_c_tree<W: Write>(&self, writer: &mut BitWriter<W>, lengths: &[u8]) -> Result<()> {
        // Find number of used codes
        let n = lengths
            .iter()
            .rposition(|&l| l > 0)
            .map(|p| p + 1)
            .unwrap_or(0);

        if n == 0 {
            // No codes used - write n=0 and a dummy code
            writer.write_bits(0, 9)?;
            writer.write_bits(0, 9)?;
            return Ok(());
        }

        // Check if only one code is used
        let used_count = lengths.iter().filter(|&&l| l > 0).count();
        if used_count == 1 {
            let code = lengths.iter().position(|&l| l > 0).unwrap_or(0);
            writer.write_bits(0, 9)?; // n = 0 means single code
            writer.write_bits(code as u32, 9)?;
            return Ok(());
        }

        // Write n FIRST (decoder reads this first)
        writer.write_bits(n as u32, 9)?;

        // Build PT-tree for encoding C-tree lengths
        let pt_lengths = self.build_pt_lengths(lengths, n);
        let pt_codes = Self::build_codes(&pt_lengths);

        // Write PT-tree
        self.write_pt_tree(writer, &pt_lengths)?;

        // Encode C-tree lengths using PT-tree
        let mut i = 0;
        while i < n {
            let len = lengths[i];

            if len == 0 {
                // Count consecutive zeros
                let mut count = 1;
                while i + count < n && lengths[i + count] == 0 && count < 512 + 19 {
                    count += 1;
                }

                if count == 1 {
                    // Single zero: PT code 0
                    Self::write_code(writer, pt_codes[0], pt_lengths[0])?;
                } else if count == 2 {
                    // Two zeros: emit two single zeros
                    Self::write_code(writer, pt_codes[0], pt_lengths[0])?;
                    Self::write_code(writer, pt_codes[0], pt_lengths[0])?;
                } else if count <= 18 {
                    // 3-18 zeros: PT code 1 + 4 bits (value 0-15 for count 3-18)
                    Self::write_code(writer, pt_codes[1], pt_lengths[1])?;
                    writer.write_bits((count - 3) as u32, 4)?;
                } else {
                    // 20+ zeros: PT code 2 + 9 bits
                    // Note: count 19 needs special handling
                    if count == 19 {
                        // 18 zeros + 1 zero
                        Self::write_code(writer, pt_codes[1], pt_lengths[1])?;
                        writer.write_bits(15, 4)?; // 18 zeros
                        Self::write_code(writer, pt_codes[0], pt_lengths[0])?; // 1 zero
                    } else {
                        Self::write_code(writer, pt_codes[2], pt_lengths[2])?;
                        writer.write_bits((count - 20) as u32, 9)?;
                    }
                }

                i += count;
            } else {
                // Non-zero length: PT code = len + 3
                // We skip PT code 3 because the PT tree format uses position 3 for a skip count,
                // so PT[3] always has length 0 and PT code 3 cannot be used.
                // Thus: C-length 1 → PT code 4, C-length 2 → PT code 5, etc.
                let pt_code = (len + 3) as usize;
                if pt_code < pt_lengths.len() && pt_lengths[pt_code] > 0 {
                    Self::write_code(writer, pt_codes[pt_code], pt_lengths[pt_code])?;
                }
                i += 1;
            }
        }

        Ok(())
    }

    /// Build PT-tree lengths for encoding C-tree.
    fn build_pt_lengths(&self, c_lengths: &[u8], n: usize) -> Vec<u8> {
        // Count PT code frequencies
        let mut pt_freq = vec![0u32; NT];

        let mut i = 0;
        while i < n {
            let len = c_lengths[i];

            if len == 0 {
                let mut count = 1;
                while i + count < n && c_lengths[i + count] == 0 && count < 512 + 19 {
                    count += 1;
                }

                if count == 1 {
                    pt_freq[0] += 1;
                } else if count == 2 {
                    pt_freq[0] += 2; // Two single zeros
                } else if count <= 18 {
                    pt_freq[1] += 1; // 3-18 zeros
                } else if count == 19 {
                    pt_freq[1] += 1; // 18 zeros
                    pt_freq[0] += 1; // 1 zero
                } else {
                    pt_freq[2] += 1; // 20+ zeros
                }

                i += count;
            } else {
                // Non-zero length: PT code = len + 3 (skip PT code 3)
                let pt_code = (len + 3) as usize;
                if pt_code < NT {
                    pt_freq[pt_code] += 1;
                }
                i += 1;
            }
        }

        Self::build_code_lengths(&pt_freq, 7)
    }

    /// Write PT-tree (for encoding C-tree lengths).
    /// Format matches decoder's read_pt_tree:
    /// 1. n (5 bits) - if 0, read single code (5 bits)
    /// 2. For i in 0..n:
    ///    - At i=3: write skip count (2 bits), continue to i=4
    ///    - Otherwise: write length (3 bits, or 7 + extra 1s + 0)
    fn write_pt_tree<W: Write>(&self, writer: &mut BitWriter<W>, lengths: &[u8]) -> Result<()> {
        // Find number of used codes
        let n = lengths
            .iter()
            .rposition(|&l| l > 0)
            .map(|p| p + 1)
            .unwrap_or(0);

        if n == 0 {
            writer.write_bits(0, 5)?;
            writer.write_bits(0, 5)?;
            return Ok(());
        }

        let used_count = lengths.iter().filter(|&&l| l > 0).count();
        if used_count == 1 {
            let code = lengths.iter().position(|&l| l > 0).unwrap_or(0);
            writer.write_bits(0, 5)?;
            writer.write_bits(code as u32, 5)?;
            return Ok(());
        }

        writer.write_bits(n as u32, 5)?;

        for i in 0..n.min(NT) {
            if i == 3 {
                // Special case: at position 3, write skip count instead of length
                // skip indicates how many of lengths[3..3+skip] are zero
                // but the decoder still continues to i=4 and reads those lengths
                // So this is essentially just metadata, not actual skipping
                let mut skip = 0;
                while skip < 3 && i + skip < n.min(NT) && lengths[i + skip] == 0 {
                    skip += 1;
                }
                writer.write_bits(skip as u32, 2)?;
                // Continue to next iteration (like decoder's continue)
                continue;
            }

            let len = lengths[i];
            if len < 7 {
                writer.write_bits(len as u32, 3)?;
            } else {
                // Length >= 7: write 7 followed by (len - 7) 1-bits and a 0-bit
                writer.write_bits(7, 3)?;
                for _ in 0..(len - 7) {
                    writer.write_bits(1, 1)?;
                }
                writer.write_bits(0, 1)?;
            }
        }

        Ok(())
    }

    /// Write P-tree (position/distance Huffman tree).
    fn write_p_tree<W: Write>(
        &self,
        writer: &mut BitWriter<W>,
        lengths: &[u8],
        np: usize,
    ) -> Result<()> {
        let n = lengths
            .iter()
            .take(np)
            .rposition(|&l| l > 0)
            .map(|p| p + 1)
            .unwrap_or(0);

        if n == 0 {
            writer.write_bits(0, 4)?;
            writer.write_bits(0, 4)?;
            return Ok(());
        }

        let used_count = lengths.iter().take(np).filter(|&&l| l > 0).count();
        if used_count == 1 {
            let code = lengths.iter().take(np).position(|&l| l > 0).unwrap_or(0);
            writer.write_bits(0, 4)?;
            writer.write_bits(code as u32, 4)?;
            return Ok(());
        }

        writer.write_bits(n as u32, 4)?;

        for &len in lengths.iter().take(n.min(np)) {
            if len < 7 {
                writer.write_bits(len as u32, 3)?;
            } else {
                writer.write_bits(7, 3)?;
                for _ in 0..(len - 7) {
                    writer.write_bits(1, 1)?;
                }
                writer.write_bits(0, 1)?;
            }
        }

        Ok(())
    }

    /// Compress data to a Vec.
    pub fn compress_to_vec(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.encode(data, &mut output, true)?;
        Ok(output)
    }

    /// Get the compression method.
    pub fn method(&self) -> LzhMethod {
        self.method
    }
}

impl Default for LzhEncoder {
    fn default() -> Self {
        Self::lh5()
    }
}

impl Compressor for LzhEncoder {
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
        self.encode(input, &mut buffer, finish)?;

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
        LzhEncoder::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Compress data using LZH.
pub fn encode_lzh(data: &[u8], method: LzhMethod) -> Result<Vec<u8>> {
    let mut encoder = LzhEncoder::new(method);
    encoder.compress_to_vec(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_lzh;

    #[test]
    fn test_encode_stored() {
        let data = b"Hello, World!";
        let encoded = encode_lzh(data, LzhMethod::Lh0).unwrap();
        assert_eq!(encoded, data);
    }

    #[test]
    fn test_encoder_creation() {
        let encoder = LzhEncoder::new(LzhMethod::Lh5);
        assert_eq!(encoder.method(), LzhMethod::Lh5);
        assert!(!encoder.finished);
    }

    #[test]
    fn test_encoder_reset() {
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let _ = encoder.compress_to_vec(b"test");
        encoder.reset();
        assert!(!encoder.finished);
    }

    #[test]
    fn test_lh5_roundtrip_simple() {
        // Test the LZSS encoder first
        let data = b"Hello, World!";

        let mut encoder = crate::lzss::LzssEncoder::new(8192, 3, 256);
        let tokens = encoder.encode(data);
        println!("LZSS tokens: {:?}", tokens);

        // Test Huffman code generation
        let mut c_freq = vec![0u32; 510];
        for token in &tokens {
            if let crate::lzss::LzssToken::Literal(b) = token {
                c_freq[*b as usize] += 1;
            }
        }
        let c_lengths = LzhEncoder::build_code_lengths(&c_freq, 16);
        println!("C-tree lengths (non-zero):");
        for (i, &l) in c_lengths.iter().enumerate() {
            if l > 0 {
                println!("  [{}] = {}", i, l);
            }
        }

        // Test PT tree generation
        let enc = LzhEncoder::new(LzhMethod::Lh5);
        let n = c_lengths
            .iter()
            .rposition(|&l| l > 0)
            .map(|p| p + 1)
            .unwrap_or(0);
        let pt_lengths = enc.build_pt_lengths(&c_lengths, n);
        println!("PT-tree lengths: {:?}", pt_lengths);
        let pt_codes = LzhEncoder::build_codes(&pt_lengths);
        println!("PT-tree codes: {:?}", pt_codes);

        // For very short data, it should all be literals
        let encoded = encode_lzh(data, LzhMethod::Lh5).unwrap();
        println!("Encoded {} bytes: {:02x?}", encoded.len(), &encoded);

        // Try to decode
        let decoded = decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_lh5_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        println!("Testing repeated pattern: {} bytes", data.len());

        let mut encoder = crate::lzss::LzssEncoder::new(8192, 3, 256);
        let tokens = encoder.encode(data);
        println!("LZSS tokens: {:?}", tokens);

        // Show C-tree structure
        let mut c_freq = vec![0u32; 510];
        for token in &tokens {
            match token {
                crate::lzss::LzssToken::Literal(b) => c_freq[*b as usize] += 1,
                crate::lzss::LzssToken::Match { length, distance } => {
                    let len_code = (*length as usize - 3 + 256).min(509);
                    c_freq[len_code] += 1;
                    println!("Match token: len_code={}, distance={}", len_code, distance);
                }
            }
        }
        let c_lengths = LzhEncoder::build_code_lengths(&c_freq, 16);
        println!("C-tree lengths (non-zero):");
        for (i, &l) in c_lengths.iter().enumerate() {
            if l > 0 {
                println!("  [{}] = {}", i, l);
            }
        }

        let enc = LzhEncoder::new(LzhMethod::Lh5);
        let n = c_lengths
            .iter()
            .rposition(|&l| l > 0)
            .map(|p| p + 1)
            .unwrap_or(0);
        let pt_lengths = enc.build_pt_lengths(&c_lengths, n);
        println!("PT-tree lengths: {:?}", pt_lengths);

        let encoded = encode_lzh(data, LzhMethod::Lh5).unwrap();
        println!("Encoded {} bytes", encoded.len());
        let decoded = decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_lh5_roundtrip_pattern() {
        let data = b"abcabcabcabcabcabcabc";
        let encoded = encode_lzh(data, LzhMethod::Lh5).unwrap();
        let decoded = decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).unwrap();
        assert_eq!(decoded, data);
    }
}

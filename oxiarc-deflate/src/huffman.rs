//! Huffman coding for DEFLATE compression.
//!
//! This module implements Huffman tree construction and decoding as specified
//! in RFC 1951. DEFLATE uses canonical Huffman codes, where codes of the same
//! length are assigned consecutive values in lexicographic order.
//!
//! # Alphabets
//!
//! DEFLATE uses three Huffman alphabets:
//! - **Literal/Length**: 0-285 (0-255 literals, 256 EOB, 257-285 lengths)
//! - **Distance**: 0-29 (back-reference distances)
//! - **Code Length**: 0-18 (for encoding dynamic Huffman trees)

use oxiarc_core::BitReader;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Maximum code length in DEFLATE (15 bits).
pub const MAX_CODE_LENGTH: usize = 15;

/// Size of the literal/length alphabet (0-285).
pub const LITLEN_ALPHABET_SIZE: usize = 286;

/// Size of the distance alphabet (0-29).
pub const DISTANCE_ALPHABET_SIZE: usize = 30;

/// Size of the code length alphabet (0-18).
pub const CODELEN_ALPHABET_SIZE: usize = 19;

/// End of block symbol.
pub const END_OF_BLOCK: u16 = 256;

/// A Huffman tree for decoding.
///
/// This uses a table-based approach for fast decoding. For codes up to
/// `FAST_BITS` length, we use a direct lookup table. For longer codes,
/// we fall back to bit-by-bit traversal.
#[derive(Debug, Clone)]
pub struct HuffmanTree {
    /// Direct lookup table for fast decoding.
    /// Entry format: (symbol, code_length) or (subtable_index | 0x8000, bits_to_skip)
    fast_table: Vec<(u16, u8)>,
    /// Number of bits for fast lookup.
    fast_bits: u8,
    /// Maximum code length in this tree.
    max_code_length: u8,
    /// Symbol lookup for codes longer than fast_bits.
    /// Indexed by (code - base_code) for each length.
    symbols: Vec<u16>,
    /// Base codes for each length.
    base_codes: [u32; MAX_CODE_LENGTH + 1],
    /// Symbol offsets for each length.
    symbol_offsets: [u16; MAX_CODE_LENGTH + 1],
}

impl HuffmanTree {
    /// Number of bits for fast lookup table.
    const FAST_BITS: u8 = 9;

    /// Build a Huffman tree from code lengths.
    ///
    /// # Arguments
    ///
    /// * `code_lengths` - Array where `code_lengths[i]` is the bit length for symbol `i`.
    ///   A length of 0 means the symbol is not used.
    pub fn from_code_lengths(code_lengths: &[u8]) -> Result<Self> {
        if code_lengths.is_empty() {
            return Err(OxiArcError::invalid_header("Empty code lengths"));
        }

        // Count codes of each length
        let mut bl_count = [0u32; MAX_CODE_LENGTH + 1];
        let mut max_length = 0u8;

        for &len in code_lengths {
            if len > 0 {
                if len as usize > MAX_CODE_LENGTH {
                    return Err(OxiArcError::invalid_header(format!(
                        "Code length {} exceeds maximum {}",
                        len, MAX_CODE_LENGTH
                    )));
                }
                bl_count[len as usize] += 1;
                max_length = max_length.max(len);
            }
        }

        // Check for valid code (at least one symbol)
        if max_length == 0 {
            // Special case: no symbols (all zeros)
            // Create a dummy tree that always returns error
            return Ok(Self {
                fast_table: vec![(0, 0); 1 << Self::FAST_BITS],
                fast_bits: Self::FAST_BITS,
                max_code_length: 0,
                symbols: Vec::new(),
                base_codes: [0; MAX_CODE_LENGTH + 1],
                symbol_offsets: [0; MAX_CODE_LENGTH + 1],
            });
        }

        // Compute first code for each length (RFC 1951 algorithm)
        let mut next_code = [0u32; MAX_CODE_LENGTH + 1];
        let mut code = 0u32;
        for bits in 1..=max_length as usize {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Validate: check that we don't exceed the code space
        let total_codes: u32 = bl_count[1..=max_length as usize].iter().sum();
        if total_codes > 0 {
            let max_codes = 1u32 << max_length;
            if code + bl_count[max_length as usize] > max_codes {
                return Err(OxiArcError::invalid_header("Over-subscribed Huffman tree"));
            }
        }

        // Build symbol table
        let mut symbols = vec![0u16; total_codes as usize];
        let mut symbol_offsets = [0u16; MAX_CODE_LENGTH + 1];
        let mut base_codes = [0u32; MAX_CODE_LENGTH + 1];

        // Calculate offsets
        let mut offset = 0u16;
        for bits in 1..=max_length as usize {
            symbol_offsets[bits] = offset;
            base_codes[bits] = next_code[bits];
            offset += bl_count[bits] as u16;
        }
        // Set the final offset for bounds checking
        if max_length < MAX_CODE_LENGTH as u8 {
            symbol_offsets[max_length as usize + 1] = offset;
        }

        // Assign symbols to codes
        let mut current_code = next_code;
        for (symbol, &len) in code_lengths.iter().enumerate() {
            if len > 0 {
                let len = len as usize;
                let idx =
                    symbol_offsets[len] as usize + (current_code[len] - base_codes[len]) as usize;
                if idx < symbols.len() {
                    symbols[idx] = symbol as u16;
                }
                current_code[len] += 1;
            }
        }

        // Build fast lookup table
        let fast_bits = Self::FAST_BITS.min(max_length);
        let fast_table_size = 1 << fast_bits;
        let mut fast_table = vec![(0u16, 0u8); fast_table_size];

        // Fill fast table
        for (symbol, &len) in code_lengths.iter().enumerate() {
            if len > 0 && len <= fast_bits {
                let len = len as usize;
                let code = Self::reverse_bits(next_code[len] as u16, len as u8);
                next_code[len] += 1;

                // Fill all entries that match this prefix
                let fill_count = 1 << (fast_bits - len as u8);
                for i in 0..fill_count {
                    let index = code as usize | (i << len);
                    if index < fast_table_size {
                        fast_table[index] = (symbol as u16, len as u8);
                    }
                }
            }
        }

        Ok(Self {
            fast_table,
            fast_bits,
            max_code_length: max_length,
            symbols,
            base_codes,
            symbol_offsets,
        })
    }

    /// Reverse bits in a code.
    fn reverse_bits(mut code: u16, length: u8) -> u16 {
        let mut reversed = 0u16;
        for _ in 0..length {
            reversed = (reversed << 1) | (code & 1);
            code >>= 1;
        }
        reversed
    }

    /// Decode a symbol from the bit stream.
    /// This is a hot path - inline for better performance.
    #[inline]
    pub fn decode<R: Read>(&self, reader: &mut BitReader<R>) -> Result<u16> {
        if self.max_code_length == 0 {
            return Err(OxiArcError::invalid_huffman(reader.bit_position()));
        }

        // Try fast lookup (handles 90%+ of symbols)
        // If peek_bits fails (not enough bits remaining), fall back to slow decoding
        match reader.peek_bits(self.fast_bits) {
            Ok(bits) => {
                let (symbol, len) = unsafe {
                    // SAFETY: bits is masked to fast_bits range, guaranteed to be valid index
                    *self.fast_table.get_unchecked(bits as usize)
                };

                if len > 0 {
                    reader.skip_bits(len)?;
                    return Ok(symbol);
                }

                // Slow path for longer codes (rare)
                self.decode_slow(reader)
            }
            Err(_) => {
                // Not enough bits for fast lookup, use slow path
                self.decode_slow(reader)
            }
        }
    }

    /// Slow decoding path for codes longer than fast_bits.
    fn decode_slow<R: Read>(&self, reader: &mut BitReader<R>) -> Result<u16> {
        let mut code = 0u32;

        for len in 1..=self.max_code_length as usize {
            let bit = reader.read_bits(1)?;
            code = (code << 1) | bit;

            let count = if len < MAX_CODE_LENGTH {
                self.symbol_offsets[len + 1] - self.symbol_offsets[len]
            } else {
                self.symbols.len() as u16 - self.symbol_offsets[len]
            };

            if count > 0 && code >= self.base_codes[len] {
                let idx = code - self.base_codes[len];
                if idx < count as u32 {
                    let symbol_idx = self.symbol_offsets[len] as usize + idx as usize;
                    if symbol_idx < self.symbols.len() {
                        return Ok(self.symbols[symbol_idx]);
                    }
                }
            }
        }

        Err(OxiArcError::invalid_huffman(reader.bit_position()))
    }
}

/// Builder for creating Huffman code lengths from frequencies.
#[derive(Debug)]
pub struct HuffmanBuilder {
    frequencies: Vec<u32>,
    max_length: u8,
}

impl HuffmanBuilder {
    /// Create a new Huffman builder.
    pub fn new(alphabet_size: usize, max_length: u8) -> Self {
        Self {
            frequencies: vec![0; alphabet_size],
            max_length,
        }
    }

    /// Add a symbol occurrence.
    pub fn add(&mut self, symbol: u16) {
        if (symbol as usize) < self.frequencies.len() {
            self.frequencies[symbol as usize] += 1;
        }
    }

    /// Add multiple occurrences of a symbol.
    pub fn add_count(&mut self, symbol: u16, count: u32) {
        if (symbol as usize) < self.frequencies.len() {
            self.frequencies[symbol as usize] += count;
        }
    }

    /// Build code lengths from frequencies.
    ///
    /// Returns an array where `result[i]` is the code length for symbol `i`.
    pub fn build_lengths(&self) -> Vec<u8> {
        let n = self.frequencies.len();
        let mut lengths = vec![0u8; n];

        // Count non-zero frequencies
        let mut symbols: Vec<(u32, usize)> = self
            .frequencies
            .iter()
            .enumerate()
            .filter(|&(_, f)| *f > 0)
            .map(|(i, f)| (*f, i))
            .collect();

        if symbols.is_empty() {
            return lengths;
        }

        if symbols.len() == 1 {
            // Single symbol gets length 1
            lengths[symbols[0].1] = 1;
            return lengths;
        }

        // Sort by frequency (ascending)
        symbols.sort_by_key(|&(f, i)| (f, i));

        // Build Huffman tree using package-merge algorithm for length-limited codes
        let code_lengths = self.package_merge(&symbols);

        for (i, (_, symbol)) in symbols.iter().enumerate() {
            lengths[*symbol] = code_lengths[i];
        }

        lengths
    }

    /// Package-merge algorithm for length-limited Huffman codes.
    fn package_merge(&self, symbols: &[(u32, usize)]) -> Vec<u8> {
        let n = symbols.len();
        let max_len = self.max_length as usize;

        // Simple implementation using bit-length limiting
        // For a more optimal implementation, use the full package-merge algorithm

        let mut lengths = vec![0u8; n];

        // Calculate ideal code lengths using Shannon-Fano approximation
        let total: f64 = symbols.iter().map(|(f, _)| *f as f64).sum();

        for (i, (freq, _)) in symbols.iter().enumerate() {
            if *freq > 0 {
                let prob = *freq as f64 / total;
                let ideal_len = (-prob.log2()).ceil() as u8;
                lengths[i] = ideal_len.max(1).min(self.max_length);
            }
        }

        // Adjust lengths to satisfy Kraft inequality
        self.adjust_lengths(&mut lengths, max_len);

        lengths
    }

    /// Adjust code lengths to satisfy Kraft inequality and length limit.
    fn adjust_lengths(&self, lengths: &mut [u8], max_len: usize) {
        // Calculate Kraft sum
        let kraft_sum: f64 = lengths
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| 2.0f64.powi(-(l as i32)))
            .sum();

        if kraft_sum <= 1.0 {
            return; // Already valid
        }

        // If over-subscribed, increase some lengths
        let mut sorted_indices: Vec<usize> =
            (0..lengths.len()).filter(|&i| lengths[i] > 0).collect();
        sorted_indices.sort_by(|&a, &b| lengths[b].cmp(&lengths[a])); // Sort by length descending

        for &i in &sorted_indices {
            if lengths[i] < max_len as u8 {
                lengths[i] += 1;
                let new_kraft: f64 = lengths
                    .iter()
                    .filter(|&&l| l > 0)
                    .map(|&l| 2.0f64.powi(-(l as i32)))
                    .sum();
                if new_kraft <= 1.0 {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_huffman_tree_simple() {
        // Simple tree: A=0, B=10, C=11
        // Code lengths: A=1, B=2, C=2
        // Canonical codes: A=0 (1 bit), B=10 (2 bits), C=11 (2 bits)
        // In LSB-first: A=0, B=01 (reversed from 10), C=11 (reversed from 11)
        let lengths = [1u8, 2, 2];
        let tree = HuffmanTree::from_code_lengths(&lengths).unwrap();

        // Test decoding A B C A
        // Bits needed: 0 (A) + 01 (B) + 11 (C) + 0 (A) = 7 bits
        // Packed LSB-first into byte: bits 0-6 = 0 01 11 0 0 = 0b00011010 = 0x1A
        let data = vec![0b00011010u8];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(tree.decode(&mut reader).unwrap(), 0); // A
        assert_eq!(tree.decode(&mut reader).unwrap(), 1); // B
        assert_eq!(tree.decode(&mut reader).unwrap(), 2); // C
        assert_eq!(tree.decode(&mut reader).unwrap(), 0); // A
    }

    #[test]
    fn test_huffman_builder() {
        let mut builder = HuffmanBuilder::new(4, 15);
        builder.add_count(0, 100); // High frequency
        builder.add_count(1, 50);
        builder.add_count(2, 25);
        builder.add_count(3, 25);

        let lengths = builder.build_lengths();

        // Higher frequency symbols should have shorter codes
        assert!(lengths[0] <= lengths[1]);
        assert!(lengths[1] <= lengths[2]);

        // All used symbols should have non-zero lengths
        assert!(lengths[0] > 0);
        assert!(lengths[1] > 0);
        assert!(lengths[2] > 0);
        assert!(lengths[3] > 0);
    }

    #[test]
    fn test_empty_tree() {
        let lengths: [u8; 4] = [0, 0, 0, 0];
        let tree = HuffmanTree::from_code_lengths(&lengths).unwrap();
        assert_eq!(tree.max_code_length, 0);
    }

    #[test]
    fn test_single_symbol() {
        // Single symbol tree
        let lengths = [1u8, 0, 0, 0];
        let tree = HuffmanTree::from_code_lengths(&lengths).unwrap();

        let data = vec![0b00000000u8];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(tree.decode(&mut reader).unwrap(), 0);
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(HuffmanTree::reverse_bits(0b101, 3), 0b101);
        assert_eq!(HuffmanTree::reverse_bits(0b1100, 4), 0b0011);
        assert_eq!(HuffmanTree::reverse_bits(0b10101010, 8), 0b01010101);
    }
}

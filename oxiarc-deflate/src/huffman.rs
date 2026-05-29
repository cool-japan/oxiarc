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

/// Build a bit-cost table from code lengths.
///
/// Returns a Vec where `result[symbol] = lengths[symbol] as u32`.
/// Symbols with length 0 are unreachable and get cost `u32::MAX`.
pub(crate) fn cost_table_from_lengths(lengths: &[u8]) -> Vec<u32> {
    lengths
        .iter()
        .map(|&l| if l == 0 { u32::MAX } else { l as u32 })
        .collect()
}

/// Compute the total bit cost for encoding a (length, distance) match.
///
/// Returns `u32::MAX` if any required symbol is unreachable (cost == `u32::MAX`)
/// or if integer overflow would occur.
pub(crate) fn cost_of_match(
    length: u16,
    distance: u16,
    litlen_costs: &[u32],
    dist_costs: &[u32],
) -> u32 {
    use crate::tables::{DISTANCE_EXTRA_BITS, LENGTH_EXTRA_BITS, distance_to_code, length_to_code};

    let (len_code, len_extra_bits, _) = length_to_code(length);
    let len_sym_cost = litlen_costs
        .get(len_code as usize)
        .copied()
        .unwrap_or(u32::MAX);
    if len_sym_cost == u32::MAX {
        return u32::MAX;
    }

    let (dist_code, dist_extra_bits, _) = distance_to_code(distance);
    let dist_sym_cost = dist_costs
        .get(dist_code as usize)
        .copied()
        .unwrap_or(u32::MAX);
    if dist_sym_cost == u32::MAX {
        return u32::MAX;
    }

    // Extra bits come from the tables; sanity-check the indices.
    let len_eb = LENGTH_EXTRA_BITS
        .get((len_code as usize).saturating_sub(257))
        .copied()
        .unwrap_or(len_extra_bits) as u32;
    let dist_eb = DISTANCE_EXTRA_BITS
        .get(dist_code as usize)
        .copied()
        .unwrap_or(dist_extra_bits) as u32;

    len_sym_cost
        .saturating_add(len_eb)
        .saturating_add(dist_sym_cost)
        .saturating_add(dist_eb)
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
    ///
    /// The result is guaranteed to be a **complete** length-limited prefix code
    /// (Kraft sum exactly 1.0 over the used symbols), as required by
    /// RFC 1951 §3.2.2 and by spec-compliant decoders (zlib `inflate_table`,
    /// which rejects incomplete code-length / literal-length tables with
    /// "invalid code lengths set"). The lengths are computed with the
    /// package-merge algorithm (optimal under the `max_length` constraint).
    pub fn build_lengths(&self) -> Vec<u8> {
        let n = self.frequencies.len();
        let mut lengths = vec![0u8; n];

        // Collect (frequency, symbol) for every used symbol.
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
            // A code with a single symbol cannot be a *complete* prefix code
            // (a 1-bit code has Kraft sum 0.5, which spec decoders reject for
            // the literal/length and code-length alphabets). Following zlib's
            // deflate, we assign the lone symbol a 1-bit code and synthesise a
            // second 1-bit code for the lowest unused symbol so the resulting
            // table is complete. The phantom symbol has frequency 0 and is
            // therefore never emitted in the data stream.
            let only = symbols[0].1;
            lengths[only] = 1;
            let phantom = if only == 0 { 1.min(n - 1) } else { 0 };
            // `n >= 1` here; if the alphabet has at least two slots we can place
            // the phantom symbol, otherwise the single 1-bit code is the best we
            // can represent (degenerate 1-symbol alphabet).
            if phantom != only {
                lengths[phantom] = 1;
            }
            return lengths;
        }

        // Sort by (frequency, symbol) ascending for deterministic, canonical
        // tie-breaking.
        symbols.sort_by_key(|&(f, i)| (f, i));

        let code_lengths = Self::package_merge(&symbols, self.max_length as usize);

        for (i, (_, symbol)) in symbols.iter().enumerate() {
            lengths[*symbol] = code_lengths[i];
        }

        lengths
    }

    /// Length-limited optimal Huffman code lengths via the package-merge
    /// (Larmore–Hirschberg) algorithm.
    ///
    /// `symbols` must be sorted by weight ascending and contain at least two
    /// entries. `max_len` is the maximum permitted code length (≤ 15 for
    /// DEFLATE literal/length and distance alphabets, ≤ 7 for the code-length
    /// alphabet). Returns a `Vec<u8>` of code lengths parallel to `symbols`.
    ///
    /// The produced code is always **complete** (Kraft sum exactly 1.0): the
    /// package-merge solution selects exactly `2*n - 2` coins, which is
    /// equivalent to the Kraft equality for a full binary tree over `n` leaves.
    fn package_merge(symbols: &[(u32, usize)], max_len: usize) -> Vec<u8> {
        let n = symbols.len();

        // The shortest length that can describe `n` symbols is ceil(log2(n)).
        // If `max_len` is below that the alphabet is unrepresentable; clamp the
        // effective limit up so we still emit a valid (complete) code. For the
        // DEFLATE alphabets this never triggers (15 bits covers 286 symbols,
        // 7 bits covers 19), but we stay defensive rather than panicking.
        let min_bits = {
            let mut b = 1usize;
            while (1usize << b) < n {
                b += 1;
            }
            b
        };
        let limit = max_len.max(min_bits);

        // Each "coin" references the index of an original symbol it covers.
        // A package-merge "list" at a given bit-width is a sorted sequence of
        // items; each item is either an original coin (one symbol) or a package
        // (the merge of two items from the previous, wider list).
        #[derive(Clone)]
        struct Item {
            weight: u64,
            // Symbol indices (into `symbols`) covered by this item.
            coverage: Vec<usize>,
        }

        // Base list: one coin per symbol, sorted ascending by weight (input is
        // already sorted by (weight, symbol)).
        let base: Vec<Item> = symbols
            .iter()
            .enumerate()
            .map(|(idx, &(w, _))| Item {
                weight: w as u64,
                coverage: vec![idx],
            })
            .collect();

        // Build successive lists from the widest bit position (`limit`) down to
        // bit position 1. At each step we package adjacent pairs of the previous
        // list, then merge those packages with the base coins.
        let mut prev: Vec<Item> = base.clone();
        for _ in 1..limit {
            // Package adjacent pairs of `prev`.
            let mut packages: Vec<Item> = Vec::with_capacity(prev.len() / 2);
            let mut i = 0;
            while i + 1 < prev.len() {
                let a = &prev[i];
                let b = &prev[i + 1];
                let mut coverage = Vec::with_capacity(a.coverage.len() + b.coverage.len());
                coverage.extend_from_slice(&a.coverage);
                coverage.extend_from_slice(&b.coverage);
                packages.push(Item {
                    weight: a.weight + b.weight,
                    coverage,
                });
                i += 2;
            }

            // Merge `base` coins with `packages`, keeping ascending weight order.
            let mut merged: Vec<Item> = Vec::with_capacity(base.len() + packages.len());
            let mut bi = 0;
            let mut pi = 0;
            while bi < base.len() || pi < packages.len() {
                let take_base = match (base.get(bi), packages.get(pi)) {
                    (Some(b), Some(p)) => b.weight <= p.weight,
                    (Some(_), None) => true,
                    (None, Some(_)) => false,
                    (None, None) => break,
                };
                if take_base {
                    merged.push(base[bi].clone());
                    bi += 1;
                } else {
                    merged.push(packages[pi].clone());
                    pi += 1;
                }
            }
            prev = merged;
        }

        // Select the first `2*n - 2` items of the final list. The code length of
        // a symbol equals the number of selected items that cover it.
        let select = 2 * n - 2;
        let mut lengths = vec![0u8; n];
        for item in prev.iter().take(select) {
            for &sym_idx in &item.coverage {
                lengths[sym_idx] = lengths[sym_idx].saturating_add(1);
            }
        }

        // Every symbol must receive a positive length and none may exceed the
        // limit (the algorithm guarantees both, but clamp defensively against
        // saturation on pathological inputs).
        for l in lengths.iter_mut() {
            if *l == 0 {
                *l = 1;
            }
            if *l as usize > limit {
                *l = limit as u8;
            }
        }

        lengths
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
        let tree = HuffmanTree::from_code_lengths(&lengths).expect("build huffman tree");

        // Test decoding A B C A
        // Bits needed: 0 (A) + 01 (B) + 11 (C) + 0 (A) = 7 bits
        // Packed LSB-first into byte: bits 0-6 = 0 01 11 0 0 = 0b00011010 = 0x1A
        let data = vec![0b00011010u8];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(tree.decode(&mut reader).expect("decode symbol A"), 0); // A
        assert_eq!(tree.decode(&mut reader).expect("decode symbol B"), 1); // B
        assert_eq!(tree.decode(&mut reader).expect("decode symbol C"), 2); // C
        assert_eq!(tree.decode(&mut reader).expect("decode symbol A again"), 0); // A
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
        let tree = HuffmanTree::from_code_lengths(&lengths).expect("build empty huffman tree");
        assert_eq!(tree.max_code_length, 0);
    }

    #[test]
    fn test_single_symbol() {
        // Single symbol tree
        let lengths = [1u8, 0, 0, 0];
        let tree = HuffmanTree::from_code_lengths(&lengths).expect("build single symbol tree");

        let data = vec![0b00000000u8];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(tree.decode(&mut reader).expect("decode single symbol"), 0);
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(HuffmanTree::reverse_bits(0b101, 3), 0b101);
        assert_eq!(HuffmanTree::reverse_bits(0b1100, 4), 0b0011);
        assert_eq!(HuffmanTree::reverse_bits(0b10101010, 8), 0b01010101);
    }
}

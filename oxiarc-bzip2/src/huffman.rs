//! Huffman coding for BZip2.
//!
//! BZip2 uses multiple Huffman tables (up to 6) and can switch between them
//! every 50 symbols for better compression.

use oxiarc_core::BitReader;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Maximum number of Huffman tables.
#[allow(dead_code)]
pub const MAX_TABLES: usize = 6;

/// Symbols per selector group.
pub const SYMBOLS_PER_GROUP: usize = 50;

/// Maximum code length.
pub const MAX_CODE_LEN: usize = 20;

/// A Huffman table for encoding and decoding.
#[derive(Debug, Clone)]
pub struct HuffmanTable {
    /// Code lengths for each symbol.
    pub lengths: Vec<u8>,
    /// Canonical codes for each symbol (for encoding).
    pub codes: Vec<u32>,
    /// Minimum code length.
    pub min_len: u8,
    /// Maximum code length.
    pub max_len: u8,
    /// First code for each code length (for decoding).
    pub bases: [u32; MAX_CODE_LEN + 1],
    /// Max code value for each code length (for decoding).
    pub limits: [u32; MAX_CODE_LEN + 1],
    /// Base index in perms for each code length (for decoding).
    pub base_index: [u32; MAX_CODE_LEN + 1],
    /// Permutation table mapping decode indices to symbols.
    pub perms: Vec<u16>,
}

impl HuffmanTable {
    /// Create a new Huffman table from code lengths.
    pub fn from_lengths(lengths: &[u8]) -> Result<Self> {
        if lengths.is_empty() {
            return Err(OxiArcError::corrupted(0, "Empty Huffman table"));
        }

        let min_len = *lengths.iter().filter(|&&l| l > 0).min().unwrap_or(&1);
        let max_len = *lengths.iter().max().unwrap_or(&1);

        if max_len > MAX_CODE_LEN as u8 {
            return Err(OxiArcError::corrupted(0, "Huffman code too long"));
        }

        // Count symbols at each length
        let mut counts = [0u32; MAX_CODE_LEN + 1];
        for &len in lengths {
            if len > 0 {
                counts[len as usize] += 1;
            }
        }

        // Calculate first_code and base_index for each length
        // first_code[L] = first canonical code of length L
        // base_index[L] = index in perms where length-L symbols start
        let mut first_code = [0u32; MAX_CODE_LEN + 1];
        let mut base_index = [0u32; MAX_CODE_LEN + 1];
        let mut bases = [0u32; MAX_CODE_LEN + 1];
        let mut limits = [0u32; MAX_CODE_LEN + 1];

        let mut code = 0u32;
        let mut index = 0u32;
        for i in 1..=max_len as usize {
            first_code[i] = code;
            base_index[i] = index;
            bases[i] = code; // For encoding compatibility
            let count = counts[i];
            limits[i] = if count > 0 { code + count - 1 } else { code };
            code = (code + count) << 1;
            index += count;
        }

        // Build canonical codes for encoding (symbol -> code mapping)
        let mut codes = vec![0u32; lengths.len()];
        let mut next_code = first_code;

        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                let len_idx = len as usize;
                codes[sym] = next_code[len_idx];
                next_code[len_idx] += 1;
            }
        }

        // Build permutation table for decoding
        // perms[base_index[L] + (code - first_code[L])] = symbol
        let total_symbols = lengths.iter().filter(|&&l| l > 0).count();
        let mut perms = vec![0u16; total_symbols];
        let mut perm_idx = base_index;

        for (sym, &len) in lengths.iter().enumerate() {
            if len > 0 {
                let len_idx = len as usize;
                let idx = perm_idx[len_idx] as usize;
                if idx < perms.len() {
                    perms[idx] = sym as u16;
                }
                perm_idx[len_idx] += 1;
            }
        }

        Ok(Self {
            lengths: lengths.to_vec(),
            codes,
            min_len,
            max_len,
            bases: first_code,
            limits,
            base_index,
            perms,
        })
    }

    /// Decode a single symbol.
    pub fn decode<R: Read>(&self, reader: &mut BitReader<R>) -> Result<u16> {
        // Read min_len bits to start
        let mut code = 0u32;
        for _ in 0..self.min_len {
            code = (code << 1) | reader.read_bits(1)?;
        }

        // Check at min_len first
        let len_idx = self.min_len as usize;
        if code <= self.limits[len_idx] {
            let idx = self.base_index[len_idx] + (code - self.bases[len_idx]);
            if (idx as usize) < self.perms.len() {
                return Ok(self.perms[idx as usize]);
            }
        }

        // Continue reading bits for longer codes
        for len in (self.min_len + 1)..=self.max_len {
            code = (code << 1) | reader.read_bits(1)?;
            let len_idx = len as usize;

            if code <= self.limits[len_idx] {
                let idx = self.base_index[len_idx] + (code - self.bases[len_idx]);
                if (idx as usize) < self.perms.len() {
                    return Ok(self.perms[idx as usize]);
                }
            }
        }

        Err(OxiArcError::corrupted(0, "Invalid Huffman code"))
    }

    /// Get the code and length for a symbol (for encoding).
    pub fn get_code(&self, symbol: u16) -> Option<(u32, u8)> {
        let sym = symbol as usize;
        if sym < self.lengths.len() && self.lengths[sym] > 0 {
            Some((self.codes[sym], self.lengths[sym]))
        } else {
            None
        }
    }
}

/// Build Huffman code lengths from symbol frequencies.
/// Uses a simple approach that guarantees valid canonical Huffman codes.
pub fn build_code_lengths(freqs: &[u32], max_len: u8) -> Vec<u8> {
    let n = freqs.len();
    if n == 0 {
        return Vec::new();
    }

    // Handle single symbol case
    if n == 1 {
        return vec![1];
    }

    // Handle two symbol case
    if n == 2 {
        return vec![1, 1];
    }

    // Build Huffman tree using heap-based algorithm
    // Each entry is (frequency, is_leaf, symbol_or_left, right)
    #[derive(Clone)]
    struct Node {
        freq: u64,
        symbols: Vec<usize>, // Symbols in this subtree
    }

    let mut heap: Vec<Node> = freqs
        .iter()
        .enumerate()
        .map(|(i, &f)| Node {
            freq: f.max(1) as u64,
            symbols: vec![i],
        })
        .collect();

    // Sort by frequency (min-heap simulation using sorted vec)
    heap.sort_by_key(|n| std::cmp::Reverse(n.freq));

    // Build tree by combining lowest frequency nodes
    while heap.len() > 1 {
        // Safe: loop condition guarantees at least 2 elements
        let right = heap.pop().expect("heap has >1 elements");
        let left = heap.pop().expect("heap has >1 elements");

        let mut combined_symbols = left.symbols;
        combined_symbols.extend(right.symbols);

        let new_node = Node {
            freq: left.freq + right.freq,
            symbols: combined_symbols,
        };

        // Insert in sorted position
        let pos = heap
            .iter()
            .rposition(|n| n.freq >= new_node.freq)
            .map(|p| p + 1)
            .unwrap_or(0);
        heap.insert(pos, new_node);
    }

    // Calculate depths (lengths) for each symbol
    let mut lengths = vec![0u8; n];

    fn assign_lengths(
        symbols: &[usize],
        depth: u8,
        lengths: &mut [u8],
        freqs: &[u32],
        max_len: u8,
    ) {
        if symbols.len() == 1 {
            lengths[symbols[0]] = depth.min(max_len).max(1);
            return;
        }

        // Sort symbols by frequency
        let mut sorted: Vec<usize> = symbols.to_vec();
        sorted.sort_by_key(|&s| std::cmp::Reverse(freqs[s]));

        // Split roughly in half by count (not by frequency sum)
        let mid = sorted.len().div_ceil(2);

        assign_lengths(&sorted[..mid], depth + 1, lengths, freqs, max_len);
        assign_lengths(&sorted[mid..], depth + 1, lengths, freqs, max_len);
    }

    if !heap.is_empty() {
        assign_lengths(&heap[0].symbols, 0, &mut lengths, freqs, max_len);
    }

    // Validate and fix Kraft inequality: sum of 2^(-len) <= 1
    // If violated, we need to adjust lengths
    loop {
        let kraft_sum: f64 = lengths.iter().map(|&l| 2.0f64.powi(-(l as i32))).sum();

        if kraft_sum <= 1.0 + 1e-9 {
            break;
        }

        // Reduce longest codes by 1
        let max_found = *lengths.iter().max().unwrap_or(&1);
        for l in lengths.iter_mut() {
            if *l == max_found && *l > 1 {
                *l -= 1;
                break;
            }
        }
    }

    // Ensure no zero lengths (minimum is 1)
    for l in lengths.iter_mut() {
        if *l == 0 {
            *l = max_len;
        }
    }

    lengths
}

/// Encode code lengths delta-coded.
#[allow(dead_code)]
pub fn encode_lengths_delta(lengths: &[u8], base_len: u8) -> Vec<i8> {
    let mut result = Vec::with_capacity(lengths.len());
    let mut current = base_len as i8;

    for &len in lengths {
        let delta = len as i8 - current;
        result.push(delta);
        current = len as i8;
    }

    result
}

/// Decode delta-coded lengths.
#[allow(dead_code)]
pub fn decode_lengths_delta(deltas: &[i8], base_len: u8) -> Vec<u8> {
    let mut result = Vec::with_capacity(deltas.len());
    let mut current = base_len as i8;

    for &delta in deltas {
        current += delta;
        result.push(current as u8);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_huffman_table_creation() {
        let lengths = vec![2, 2, 3, 3];
        let table = HuffmanTable::from_lengths(&lengths).unwrap();
        assert_eq!(table.min_len, 2);
        assert_eq!(table.max_len, 3);
    }

    #[test]
    fn test_build_code_lengths() {
        let freqs = vec![100, 50, 25, 10];
        let lengths = build_code_lengths(&freqs, 15);
        assert_eq!(lengths.len(), 4);
        // More frequent symbols should have shorter codes
        assert!(lengths[0] <= lengths[3]);
    }

    #[test]
    fn test_delta_encoding() {
        let lengths = vec![3, 4, 4, 5, 3];
        let deltas = encode_lengths_delta(&lengths, 3);
        let decoded = decode_lengths_delta(&deltas, 3);
        assert_eq!(decoded, lengths);
    }
}

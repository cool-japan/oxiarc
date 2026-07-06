//! Huffman coding for BZip2.
//!
//! BZip2 uses multiple Huffman tables (2-6) and can switch between them
//! every 50 symbols for better compression. Decoding uses the canonical
//! `limit`/`base`/`perm` scheme of libbz2's `hbCreateDecodeTables`, and
//! encoding assigns canonical codes with libbz2's `hbAssignCodes` rule
//! (codes ordered by (length, symbol)).

use crate::bitio::MsbBitReader;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Maximum number of Huffman tables.
pub const MAX_TABLES: usize = 6;

/// Minimum number of Huffman tables required by the format.
pub const MIN_TABLES: usize = 2;

/// Symbols per selector group.
pub const SYMBOLS_PER_GROUP: usize = 50;

/// Maximum code length accepted when decoding (libbz2 `BZ_MAX_CODE_LEN - 1`).
pub const MAX_CODE_LEN: usize = 23;

/// Maximum code length produced when encoding (same limit as libbz2).
pub const MAX_ENCODE_LEN: usize = 17;

/// A canonical Huffman table for encoding and decoding.
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
    /// Largest code value (left-justified comparison) per length.
    limit: [i64; MAX_CODE_LEN + 2],
    /// Decode base per length: `perm_index = code - base[len]`.
    base: [i64; MAX_CODE_LEN + 2],
    /// Permutation mapping decode indices to symbols (by length, then symbol).
    perm: Vec<u16>,
}

impl HuffmanTable {
    /// Create a new Huffman table from code lengths.
    ///
    /// Every symbol must have a code length in `1..=MAX_CODE_LEN`; the bzip2
    /// format assigns a code to every symbol of the block alphabet.
    pub fn from_lengths(lengths: &[u8]) -> Result<Self> {
        let min_len = lengths
            .iter()
            .copied()
            .min()
            .ok_or_else(|| OxiArcError::corrupted(0, "Empty Huffman table"))?;
        let max_len = lengths.iter().copied().max().unwrap_or(0);

        if min_len == 0 || usize::from(max_len) > MAX_CODE_LEN {
            return Err(OxiArcError::corrupted(0, "Invalid Huffman code length"));
        }

        // Permutation in (length, symbol) order, as in hbCreateDecodeTables.
        let mut perm = Vec::with_capacity(lengths.len());
        for len in min_len..=max_len {
            for (symbol, &l) in lengths.iter().enumerate() {
                if l == len {
                    perm.push(symbol as u16);
                }
            }
        }

        let mut count = [0i64; MAX_CODE_LEN + 2];
        for &l in lengths {
            count[l as usize + 1] += 1;
        }
        for i in 1..MAX_CODE_LEN + 2 {
            count[i] += count[i - 1];
        }

        let mut limit = [0i64; MAX_CODE_LEN + 2];
        let mut base = [0i64; MAX_CODE_LEN + 2];
        let mut vec = 0i64;
        for len in usize::from(min_len)..=usize::from(max_len) {
            vec += count[len + 1] - count[len];
            limit[len] = vec - 1;
            vec <<= 1;
        }
        for len in (usize::from(min_len) + 1)..=usize::from(max_len) {
            base[len] = ((limit[len - 1] + 1) << 1) - count[len];
        }

        // Canonical encode codes with the same (length, symbol) ordering.
        let codes = assign_codes(lengths, min_len, max_len);

        Ok(Self {
            lengths: lengths.to_vec(),
            codes,
            min_len,
            max_len,
            limit,
            base,
            perm,
        })
    }

    /// Decode a single symbol from an MSB-first bit stream.
    pub fn decode<R: Read>(&self, reader: &mut MsbBitReader<R>) -> Result<u16> {
        let mut len = usize::from(self.min_len);
        let mut value = i64::from(reader.read_bits(len as u32)?);
        loop {
            if len > usize::from(self.max_len) {
                return Err(OxiArcError::corrupted(0, "Invalid Huffman code"));
            }
            if value <= self.limit[len] {
                let index = value - self.base[len];
                if index < 0 {
                    return Err(OxiArcError::corrupted(0, "Invalid Huffman code"));
                }
                return self
                    .perm
                    .get(index as usize)
                    .copied()
                    .ok_or_else(|| OxiArcError::corrupted(0, "Invalid Huffman code"));
            }
            len += 1;
            value = (value << 1) | i64::from(reader.read_bit()?);
        }
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

/// Assign canonical codes from lengths (libbz2 `hbAssignCodes` rule).
fn assign_codes(lengths: &[u8], min_len: u8, max_len: u8) -> Vec<u32> {
    let mut codes = vec![0u32; lengths.len()];
    let mut vec = 0u32;
    for len in min_len..=max_len {
        for (symbol, &l) in lengths.iter().enumerate() {
            if l == len {
                codes[symbol] = vec;
                vec += 1;
            }
        }
        vec <<= 1;
    }
    codes
}

/// Build length-limited Huffman code lengths from symbol frequencies.
///
/// Builds a true Huffman tree over the (floored-to-1) frequencies; when the
/// resulting depth exceeds `max_len`, the weights are halved (libbz2's
/// `BZ2_hbMakeCodeLengths` strategy) and the tree is rebuilt until the code
/// fits. Every symbol receives a length in `1..=max_len`, so the resulting
/// canonical code is always complete enough for the bzip2 format.
pub fn build_code_lengths(freqs: &[u32], max_len: u8) -> Vec<u8> {
    let n = freqs.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![1];
    }

    let mut weights: Vec<u64> = freqs.iter().map(|&f| u64::from(f.max(1))).collect();

    loop {
        // Huffman tree construction over the current weights.
        struct Node {
            weight: u64,
            left: Option<usize>,
            right: Option<usize>,
            symbol: Option<usize>,
        }
        let mut nodes: Vec<Node> = weights
            .iter()
            .enumerate()
            .map(|(symbol, &weight)| Node {
                weight,
                left: None,
                right: None,
                symbol: Some(symbol),
            })
            .collect();
        let mut heap: Vec<usize> = (0..nodes.len()).collect();

        while heap.len() > 1 {
            // Keep the two smallest weights at the tail (descending sort).
            heap.sort_by(|&a, &b| nodes[b].weight.cmp(&nodes[a].weight));
            let a = heap.pop().unwrap_or(0);
            let b = heap.pop().unwrap_or(0);
            let merged = Node {
                weight: nodes[a].weight + nodes[b].weight,
                left: Some(a),
                right: Some(b),
                symbol: None,
            };
            nodes.push(merged);
            heap.push(nodes.len() - 1);
        }

        // Depth of every leaf = code length.
        let mut lengths = vec![0u8; n];
        let mut max_depth = 0usize;
        if let Some(&root) = heap.first() {
            let mut stack = vec![(root, 0usize)];
            while let Some((index, depth)) = stack.pop() {
                let node = &nodes[index];
                match node.symbol {
                    Some(symbol) => {
                        // A single-symbol alphabet still gets length 1.
                        lengths[symbol] = depth.max(1) as u8;
                        max_depth = max_depth.max(depth.max(1));
                    }
                    None => {
                        if let (Some(l), Some(r)) = (node.left, node.right) {
                            stack.push((l, depth + 1));
                            stack.push((r, depth + 1));
                        }
                    }
                }
            }
        }

        if max_depth <= usize::from(max_len) {
            return lengths;
        }

        // Flatten the weight distribution and retry (libbz2 strategy).
        for weight in weights.iter_mut() {
            *weight = 1 + (*weight / 2);
        }
    }
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
    use crate::bitio::MsbBitWriter;
    use std::io::Cursor;

    #[test]
    fn test_huffman_table_creation() {
        let lengths = vec![2, 2, 3, 3];
        let table = HuffmanTable::from_lengths(&lengths).expect("build huffman table from lengths");
        assert_eq!(table.min_len, 2);
        assert_eq!(table.max_len, 3);
    }

    #[test]
    fn test_huffman_rejects_zero_length() {
        assert!(HuffmanTable::from_lengths(&[0, 2, 2]).is_err());
        assert!(HuffmanTable::from_lengths(&[]).is_err());
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
    fn test_build_code_lengths_respects_limit() {
        // Exponential frequencies force deep trees without the length limit.
        let freqs: Vec<u32> = (0..30).map(|i| 1u32 << i.min(20)).collect();
        let lengths = build_code_lengths(&freqs, MAX_ENCODE_LEN as u8);
        assert!(
            lengths
                .iter()
                .all(|&l| (1..=MAX_ENCODE_LEN as u8).contains(&l))
        );
        // Kraft inequality must hold for a decodable code.
        let kraft: f64 = lengths.iter().map(|&l| 2.0f64.powi(-i32::from(l))).sum();
        assert!(kraft <= 1.0 + 1e-9, "Kraft sum {kraft} exceeds 1");
    }

    #[test]
    fn test_encode_decode_symbols_roundtrip() {
        let freqs = vec![90, 40, 20, 10, 5, 3, 2, 1];
        let lengths = build_code_lengths(&freqs, MAX_ENCODE_LEN as u8);
        let table = HuffmanTable::from_lengths(&lengths).expect("table from built lengths");

        let symbols: Vec<u16> = vec![0, 1, 7, 3, 2, 2, 0, 6, 5, 4, 0, 7];
        let mut writer = MsbBitWriter::new(Vec::new());
        for &sym in &symbols {
            let (code, len) = table.get_code(sym).expect("code for symbol");
            writer.write_bits(code, u32::from(len)).expect("write code");
        }
        writer.finish().expect("finish writer");
        let bytes = writer.into_inner();

        let mut reader = MsbBitReader::new(Cursor::new(bytes));
        for &expected in &symbols {
            let decoded = table.decode(&mut reader).expect("decode symbol");
            assert_eq!(decoded, expected);
        }
    }

    #[test]
    fn test_delta_encoding() {
        let lengths = vec![3, 4, 4, 5, 3];
        let deltas = encode_lengths_delta(&lengths, 3);
        let decoded = decode_lengths_delta(&deltas, 3);
        assert_eq!(decoded, lengths);
    }
}

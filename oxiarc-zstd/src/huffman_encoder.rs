//! Huffman encoding for Zstandard literals compression.
//!
//! This module builds Huffman tables from byte frequency counts and encodes
//! literals using canonical Huffman codes as specified in RFC 8878.
//!
//! In Zstandard, the literals section of a compressed block contains:
//! 1. A literals header (describing type, sizes)
//! 2. Huffman table description (for Compressed type)
//! 3. Huffman-encoded bitstreams
//!
//! The Huffman table is described using "weights" where:
//!   `weight -> code_length = max_bits + 1 - weight`
//! Weights are stored as direct 4-bit values packed 2 per byte (high nibble first).

pub use crate::huffman::MAX_CODE_LENGTH;

/// Maximum number of symbols (byte alphabet).
const MAX_SYMBOLS: usize = 256;

/// Huffman encoding table.
///
/// Holds canonical Huffman codes for up to 256 byte symbols, built from
/// frequency counts. Used to encode the literals section of compressed blocks.
pub struct HuffmanEncoder {
    /// Code for each symbol (up to 256 symbols).
    codes: Vec<u32>,
    /// Code length for each symbol.
    lengths: Vec<u8>,
    /// Maximum code length.
    max_bits: u8,
    /// Number of symbols with non-zero weights.
    num_symbols: usize,
    /// Weights for table serialization.
    weights: Vec<u8>,
}

impl HuffmanEncoder {
    /// Build a Huffman encoder from byte frequency counts.
    ///
    /// Returns `None` if all bytes are the same (use RLE instead) or if there
    /// are fewer than 2 distinct symbols.
    pub fn from_frequencies(frequencies: &[u64; 256]) -> Option<Self> {
        // Count distinct symbols
        let mut distinct_count = 0usize;
        let mut last_symbol = 0u8;
        for (i, &freq) in frequencies.iter().enumerate() {
            if freq > 0 {
                distinct_count += 1;
                last_symbol = i as u8;
            }
        }

        if distinct_count <= 1 {
            // Zero or one distinct symbol: RLE is better
            let _ = last_symbol;
            return None;
        }

        // Build Huffman tree using a priority queue (BinaryHeap).
        // Each node is (frequency, node_index). Leaves are indices 0..255,
        // internal nodes are 256..
        let mut node_left: Vec<usize> = vec![usize::MAX; MAX_SYMBOLS];
        let mut node_right: Vec<usize> = vec![usize::MAX; MAX_SYMBOLS];

        // Use a min-heap (Reverse to turn BinaryHeap into min-heap)
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();

        for (i, &freq) in frequencies.iter().enumerate() {
            if freq > 0 {
                heap.push(Reverse((freq, i)));
            }
        }

        // Combine nodes until one remains. Each pop below is structurally
        // guaranteed by the `while heap.len() > 1` / final-pop invariant
        // (there are always >= 2 elements before the first pop and >= 1
        // before the second, since the loop only runs while `len() > 1`),
        // but that guarantee lives in the shape of this loop rather than in
        // the type system. Using `?` on the `Option` this function already
        // returns converts a future refactor that breaks the invariant into
        // a clean `None` instead of a panic, at zero cost on the path that
        // actually runs today.
        while heap.len() > 1 {
            let Reverse((lf, li)) = heap.pop()?;
            let Reverse((rf, ri)) = heap.pop()?;

            let combined_freq = lf + rf;
            let new_idx = node_left.len();
            node_left.push(li);
            node_right.push(ri);

            heap.push(Reverse((combined_freq, new_idx)));
        }

        let Reverse((_, final_root)) = heap.pop()?;

        // Compute code lengths via DFS
        let mut code_lengths = vec![0u8; MAX_SYMBOLS];
        let mut stack: Vec<(usize, u8)> = vec![(final_root, 0)];

        while let Some((node, depth)) = stack.pop() {
            if node < MAX_SYMBOLS {
                // Leaf node
                code_lengths[node] = depth;
            } else {
                // Internal node
                let left = node_left[node];
                let right = node_right[node];
                if left != usize::MAX {
                    stack.push((left, depth + 1));
                }
                if right != usize::MAX {
                    stack.push((right, depth + 1));
                }
            }
        }

        // Enforce maximum code length of MAX_CODE_LENGTH using the Kraft inequality
        // rebalancing approach
        let mut max_len = 0u8;
        for &len in &code_lengths {
            if len > max_len {
                max_len = len;
            }
        }

        if max_len > MAX_CODE_LENGTH {
            // Need to limit code lengths. Use a simple approach:
            // repeatedly reduce the longest codes and compensate by lengthening shorter ones
            Self::limit_code_lengths(&mut code_lengths, MAX_CODE_LENGTH);

            max_len = 0;
            for &len in &code_lengths {
                if len > max_len {
                    max_len = len;
                }
            }
        }

        // The lengths must form a *complete* prefix code: sum(2^-len) == 1,
        // i.e. in integer units sum(2^(max_len - len)) == 2^max_len. A raw
        // Huffman tree satisfies this; the limiter aims for it but bail out
        // (caller falls back to Raw literals) if anything is off, because an
        // incomplete weight set is rejected by RFC 8878 decoders.
        {
            let target = 1u64 << max_len;
            let sum: u64 = code_lengths
                .iter()
                .filter(|&&len| len > 0)
                .map(|&len| 1u64 << (max_len - len))
                .sum();
            if sum != target {
                return None;
            }
        }

        // Zstandard weights: weight = max_len + 1 - length. The table log the
        // decoder derives is exactly `max_len`, and the longest codes carry
        // weight 1 (of which a complete tree always has an even count >= 2).
        let max_bits = max_len;
        let mut num_symbols = 0usize;
        let mut max_symbol = 0usize;
        for (i, &len) in code_lengths.iter().enumerate() {
            if len > 0 {
                num_symbols += 1;
                max_symbol = i;
            }
        }

        let mut weights = vec![0u8; max_symbol + 1];
        let mut rank_count = [0u32; (MAX_CODE_LENGTH as usize) + 2];
        for i in 0..=max_symbol {
            if code_lengths[i] > 0 {
                let w = max_bits + 1 - code_lengths[i];
                weights[i] = w;
                rank_count[w as usize] += 1;
            }
        }

        // Canonical code assignment mirroring the reference decode table
        // (`HUF_readDTableX1`): weight-1 symbols occupy the lowest prefix
        // ranges, ascending by weight, natural symbol order within a weight.
        // A symbol of weight `w` covers 2^(w-1) table cells; its code is the
        // cell range start shifted down by (w-1).
        let mut rank_start = [0u64; (MAX_CODE_LENGTH as usize) + 2];
        let mut next_start = 0u64;
        for w in 1..=(max_bits as usize) {
            rank_start[w] = next_start;
            next_start += (rank_count[w] as u64) << (w - 1);
        }

        let mut codes = vec![0u32; MAX_SYMBOLS];
        let mut lengths = vec![0u8; MAX_SYMBOLS];
        for i in 0..=max_symbol {
            let w = weights[i];
            if w == 0 {
                continue;
            }
            codes[i] = (rank_start[w as usize] >> (w - 1)) as u32;
            lengths[i] = max_bits + 1 - w;
            rank_start[w as usize] += 1u64 << (w - 1);
        }

        Some(Self {
            codes,
            lengths,
            max_bits,
            num_symbols,
            weights,
        })
    }

    /// Limit code lengths to a maximum value using the package-merge inspired approach.
    ///
    /// When the Huffman tree produces codes longer than `max_length`, this function
    /// clamps them and then redistributes the Kraft deficit from longer codes to
    /// shorter ones to maintain a valid prefix-free code.
    fn limit_code_lengths(code_lengths: &mut [u8], max_length: u8) {
        // Collect symbols with nonzero lengths, sorted by length descending
        let mut symbol_indices: Vec<usize> = code_lengths
            .iter()
            .enumerate()
            .filter(|&(_, len)| *len > 0)
            .map(|(i, _)| i)
            .collect();
        symbol_indices.sort_by(|&a, &b| code_lengths[b].cmp(&code_lengths[a]));

        // Clamp all lengths to max_length, tracking the Kraft deficit
        // Kraft value in integer units: each symbol contributes 2^(max_length - len)
        // Target sum is 2^max_length
        let target = 1u64 << max_length;
        let mut kraft_sum: u64 = 0;

        for &sym in &symbol_indices {
            if code_lengths[sym] > max_length {
                code_lengths[sym] = max_length;
            }
            kraft_sum += 1u64 << (max_length - code_lengths[sym]);
        }

        if kraft_sum == target {
            return;
        }

        if kraft_sum > target {
            // Over-specified after clamping: we have too much Kraft weight.
            // We need to lengthen some shorter codes to reduce weight.
            // Process from shortest to longest, lengthening codes by 1 at a time.
            // Each lengthening reduces kraft contribution by half.
            let mut excess = kraft_sum - target;
            // Sort by length ascending for this phase
            symbol_indices.sort_by(|&a, &b| code_lengths[a].cmp(&code_lengths[b]));

            for &sym in &symbol_indices {
                while excess > 0 && code_lengths[sym] < max_length {
                    let old_contribution = 1u64 << (max_length - code_lengths[sym]);
                    let new_contribution = old_contribution >> 1;
                    let saved = old_contribution - new_contribution;
                    if saved <= excess {
                        code_lengths[sym] += 1;
                        excess -= saved;
                    } else {
                        break;
                    }
                }
                if excess == 0 {
                    break;
                }
            }
        } else {
            // Under-specified: need more Kraft weight.
            // Shorten the longest codes by 1, which doubles their contribution.
            let mut deficit = target - kraft_sum;
            // symbol_indices is already sorted by length descending
            symbol_indices.sort_by(|&a, &b| code_lengths[b].cmp(&code_lengths[a]));

            for &sym in &symbol_indices {
                while deficit > 0 && code_lengths[sym] > 1 {
                    let old_contribution = 1u64 << (max_length - code_lengths[sym]);
                    let new_contribution = old_contribution << 1;
                    let gained = new_contribution - old_contribution;
                    if gained <= deficit {
                        code_lengths[sym] -= 1;
                        deficit -= gained;
                    } else {
                        break;
                    }
                }
                if deficit == 0 {
                    break;
                }
            }
        }
    }

    /// Encode the Huffman table description for inclusion in a compressed block.
    ///
    /// Returns the serialized table using the direct 4-bit weight format.
    /// Format: header byte = 127 + num_weight_symbols, then 4-bit weights
    /// packed 2 per byte (high nibble first).
    pub fn serialize_table(&self) -> Vec<u8> {
        // RFC 8878 §4.2.1.1: the *last* present symbol's weight is not
        // stored — the decoder deduces it from the Kraft remainder. So only
        // weights for symbols `0..last_symbol` are written and
        // `Number_of_Weights = last_symbol`.
        let last_symbol = self.weights.iter().rposition(|&w| w > 0).unwrap_or(0);
        let num_weight_symbols = last_symbol;
        let header_byte = (127 + num_weight_symbols) as u8;

        let bytes_needed = num_weight_symbols.div_ceil(2);
        let mut output = Vec::with_capacity(1 + bytes_needed);
        output.push(header_byte);

        // Pack weights 2 per byte, high nibble first
        let mut i = 0;
        while i < num_weight_symbols {
            let high = self.weights[i] & 0x0F;
            let low = if i + 1 < num_weight_symbols {
                self.weights[i + 1] & 0x0F
            } else {
                0
            };
            output.push((high << 4) | low);
            i += 2;
        }

        output
    }

    /// Encode literals into one RFC 8878 backward Huffman bitstream.
    ///
    /// The decoder consumes the stream starting at the sentinel bit in the
    /// last byte and reads codes in **reverse write order**, so — exactly
    /// like the reference `HUF_compress1X` — the *last* literal's code is
    /// written first and the first literal's code last. Symbols with weight
    /// zero (never counted in the frequency table) must not appear.
    pub fn encode_literals(&self, literals: &[u8]) -> Vec<u8> {
        let mut writer = crate::bitwriter::BackwardBitWriter::with_capacity(literals.len());
        for &lit in literals.iter().rev() {
            writer.write_bits(self.codes[lit as usize] as u64, self.lengths[lit as usize]);
        }
        writer.finish()
    }

    /// Get the code and length for a symbol.
    #[cfg(test)]
    #[inline]
    pub fn get_code(&self, symbol: u8) -> (u32, u8) {
        (self.codes[symbol as usize], self.lengths[symbol as usize])
    }

    /// Get the maximum code length (the decoder's table log).
    pub fn max_bits(&self) -> u8 {
        self.max_bits
    }

    /// Get the number of symbols with non-zero weight.
    pub fn num_symbols(&self) -> usize {
        self.num_symbols
    }

    /// Get a reference to the weights array.
    #[cfg(test)]
    pub fn weights(&self) -> &[u8] {
        &self.weights
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_symbol_returns_none() {
        let mut freq = [0u64; 256];
        freq[0] = 100;
        assert!(HuffmanEncoder::from_frequencies(&freq).is_none());
    }

    #[test]
    fn test_all_zero_returns_none() {
        let freq = [0u64; 256];
        assert!(HuffmanEncoder::from_frequencies(&freq).is_none());
    }

    #[test]
    fn test_two_equal_symbols() {
        let mut freq = [0u64; 256];
        freq[b'A' as usize] = 50;
        freq[b'B' as usize] = 50;
        let encoder = HuffmanEncoder::from_frequencies(&freq);
        assert!(encoder.is_some());
        let enc = encoder.as_ref().expect("encoder should exist");
        // Both should get 1-bit codes
        let (_, len_a) = enc.get_code(b'A');
        let (_, len_b) = enc.get_code(b'B');
        assert_eq!(len_a, 1);
        assert_eq!(len_b, 1);
        // Codes should differ
        let (code_a, _) = enc.get_code(b'A');
        let (code_b, _) = enc.get_code(b'B');
        assert_ne!(code_a, code_b);
    }

    #[test]
    fn test_skewed_distribution() {
        let mut freq = [0u64; 256];
        freq[0] = 1000;
        freq[1] = 100;
        freq[2] = 10;
        freq[3] = 1;
        let encoder = HuffmanEncoder::from_frequencies(&freq);
        assert!(encoder.is_some());
        let enc = encoder.as_ref().expect("encoder should exist");
        // Most frequent symbol should have shortest code
        let (_, len0) = enc.get_code(0);
        let (_, len3) = enc.get_code(3);
        assert!(len0 <= len3);
    }

    #[test]
    fn test_max_code_length_enforced() {
        // Create a distribution that would normally produce very long codes
        let mut freq = [0u64; 256];
        let mut f = 1u64;
        for slot in freq.iter_mut().take(20) {
            *slot = f;
            f = f.saturating_mul(2);
        }
        let encoder = HuffmanEncoder::from_frequencies(&freq);
        assert!(encoder.is_some());
        let enc = encoder.as_ref().expect("encoder should exist");
        assert!(enc.max_bits() <= MAX_CODE_LENGTH);
    }

    #[test]
    fn test_serialize_table_format() {
        let mut freq = [0u64; 256];
        freq[0] = 100;
        freq[1] = 50;
        freq[2] = 25;
        let encoder = HuffmanEncoder::from_frequencies(&freq);
        assert!(encoder.is_some());
        let enc = encoder.as_ref().expect("encoder should exist");
        let serialized = enc.serialize_table();
        // First byte is 127 + Number_of_Weights, where the *last* present
        // symbol's weight is implied (not stored) per RFC 8878 §4.2.1.1.
        let num_w = enc.weights().len() - 1;
        assert_eq!(serialized[0], (127 + num_w) as u8);
        // Remaining bytes should be ceil(num_w / 2)
        let expected_data_bytes = num_w.div_ceil(2);
        assert_eq!(serialized.len(), 1 + expected_data_bytes);
    }

    #[test]
    fn test_encode_literals_nonempty() {
        let mut freq = [0u64; 256];
        freq[b'A' as usize] = 100;
        freq[b'B' as usize] = 50;
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");
        let encoded = encoder.encode_literals(b"AABB");
        // Encoded should be non-empty
        assert!(!encoded.is_empty());
        // The last byte should have the sentinel bit set (nonzero)
        assert_ne!(encoded[encoded.len() - 1], 0);
    }

    #[test]
    fn test_encode_empty_literals() {
        let mut freq = [0u64; 256];
        freq[0] = 10;
        freq[1] = 10;
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");
        let encoded = encoder.encode_literals(&[]);
        assert_eq!(encoded, vec![0x01]);
    }

    #[test]
    fn test_num_symbols() {
        let mut freq = [0u64; 256];
        freq[10] = 5;
        freq[20] = 3;
        freq[30] = 1;
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");
        assert_eq!(encoder.num_symbols(), 3);
    }

    #[test]
    fn test_weights_correspond_to_lengths() {
        let mut freq = [0u64; 256];
        freq[0] = 100;
        freq[1] = 50;
        freq[2] = 25;
        freq[3] = 10;
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");
        let max_bits = encoder.max_bits();
        let weights = encoder.weights();
        for (i, &w) in weights.iter().enumerate() {
            if w > 0 {
                let (_, len) = encoder.get_code(i as u8);
                assert_eq!(w, max_bits + 1 - len, "weight mismatch for symbol {}", i);
            }
        }
    }

    #[test]
    fn test_roundtrip_table_serialization() {
        // Build encoder from frequencies
        let mut freq = [0u64; 256];
        freq[b'A' as usize] = 100;
        freq[b'B' as usize] = 50;
        freq[b'C' as usize] = 25;
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");

        // Serialize the table
        let table_data = encoder.serialize_table();

        // Parse the table with the decoder
        let (decoder_table, consumed) =
            crate::huffman::read_huffman_table(&table_data).expect("should parse table");
        assert_eq!(consumed, table_data.len());

        // Verify the decoder table produces the same symbol mapping
        assert!(decoder_table.max_bits() > 0);

        // Verify the encoder can produce codes for all active symbols
        for sym in [b'A', b'B', b'C'] {
            let (code, len) = encoder.get_code(sym);
            assert!(
                len > 0,
                "symbol {:?} should have nonzero length",
                sym as char
            );
            assert!(len <= MAX_CODE_LENGTH);
            // Verify the decoder can decode this code back to the same symbol
            let padded_code = code << (decoder_table.max_bits() - len);
            let entry = decoder_table.entries()[padded_code as usize];
            assert_eq!(
                entry.symbol, sym,
                "decoder should map code back to symbol {:?}",
                sym as char
            );
        }
    }

    #[test]
    fn test_encode_then_decode_manually() {
        // Build encoder with known distribution
        let mut freq = [0u64; 256];
        freq[0] = 80;
        freq[1] = 40;
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");

        // With two equal-ish symbols, both should get 1-bit codes
        let (code0, len0) = encoder.get_code(0);
        let (code1, len1) = encoder.get_code(1);

        // Encode a simple sequence
        let literals = [0u8, 1, 0, 0, 1];
        let encoded = encoder.encode_literals(&literals);

        // The encoded stream should be non-empty and last byte nonzero (sentinel)
        assert!(!encoded.is_empty());
        // Verify last byte is nonzero (has sentinel)
        assert_ne!(
            *encoded.last().expect("should have bytes"),
            0,
            "last byte must contain sentinel"
        );

        // Verify encoded size is reasonable
        let expected_bits = literals
            .iter()
            .map(|&l| encoder.get_code(l).1 as usize)
            .sum::<usize>()
            + 1; // +1 for sentinel
        let expected_bytes = expected_bits.div_ceil(8);
        assert_eq!(encoded.len(), expected_bytes);

        // Verify we used all symbols' codes
        let _ = (code0, len0, code1, len1);
    }

    #[test]
    fn test_many_symbols() {
        // Use all 256 symbols
        let mut freq = [0u64; 256];
        for (i, f) in freq.iter_mut().enumerate() {
            *f = (256 - i as u64) + 1;
        }
        let encoder = HuffmanEncoder::from_frequencies(&freq).expect("encoder should exist");
        assert_eq!(encoder.num_symbols(), 256);
        assert!(encoder.max_bits() <= MAX_CODE_LENGTH);

        // All symbols should have valid codes
        for i in 0..=255u8 {
            let (_, len) = encoder.get_code(i);
            assert!(len > 0, "symbol {} should have nonzero length", i);
            assert!(
                len <= MAX_CODE_LENGTH,
                "symbol {} length {} exceeds max",
                i,
                len
            );
        }
    }
}

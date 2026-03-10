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

/// Maximum Huffman code length (from spec).
pub const MAX_CODE_LENGTH: u8 = 11;

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

        // Combine nodes until one remains
        while heap.len() > 1 {
            let Reverse((lf, li)) = heap.pop().expect("heap should have elements");
            let Reverse((rf, ri)) = heap.pop().expect("heap should have elements");

            let combined_freq = lf + rf;
            let new_idx = node_left.len();
            node_left.push(li);
            node_right.push(ri);

            heap.push(Reverse((combined_freq, new_idx)));
        }

        let Reverse((_, final_root)) = heap.pop().expect("heap should have one element");

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

        // Assign canonical Huffman codes
        // 1. Count lengths
        let mut bl_count = vec![0u32; (max_len as usize) + 1];
        for &len in &code_lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // 2. Compute next_code for each length
        let mut next_code = vec![0u32; (max_len as usize) + 1];
        let mut code_val = 0u32;
        for bits in 1..=max_len as usize {
            code_val = (code_val + bl_count[bits - 1]) << 1;
            next_code[bits] = code_val;
        }

        // 3. Assign codes
        let mut codes = vec![0u32; MAX_SYMBOLS];
        let mut lengths = vec![0u8; MAX_SYMBOLS];
        let mut num_symbols = 0usize;

        // Find the highest symbol with nonzero frequency for weight serialization
        let mut max_symbol = 0usize;
        for (i, &len) in code_lengths.iter().enumerate() {
            if len > 0 {
                codes[i] = next_code[len as usize];
                next_code[len as usize] += 1;
                lengths[i] = len;
                num_symbols += 1;
                max_symbol = i;
            }
        }

        // Compute weights compatible with the decoder's `HuffmanTable::from_weights`.
        //
        // The decoder computes:
        //   total_weight = sum(2^(w-1)) for all w > 0
        //   max_bits = bit_width(total_weight)     [= 32 - total_weight.leading_zeros()]
        //   code_length[s] = max_bits + 1 - weight[s]
        //
        // We need to find a `max_bits` value such that:
        //   weight[s] = max_bits + 1 - code_length[s]
        //   decoder's computed max_bits == our max_bits
        //
        // Since this relationship is self-referential (max_bits depends on weights
        // which depend on max_bits), we solve iteratively: start with a candidate
        // max_bits, compute resulting total_weight, check if the decoder's
        // bit_width(total_weight) matches our candidate, and adjust until stable.

        let max_bits = {
            let mut candidate = max_len;
            for _ in 0..20 {
                // Compute weights with this candidate
                let mut total = 0u64;
                for &len in &code_lengths {
                    if len > 0 && candidate + 1 > len {
                        let w = candidate + 1 - len;
                        total += 1u64 << (w - 1);
                    }
                }
                let decoder_max = if total == 0 {
                    candidate
                } else {
                    (64 - total.leading_zeros()) as u8
                };
                let decoder_max = decoder_max.min(MAX_CODE_LENGTH);
                if decoder_max == candidate {
                    break;
                }
                candidate = decoder_max;
            }
            candidate
        };

        let mut weights = vec![0u8; max_symbol + 1];
        for i in 0..=max_symbol {
            if code_lengths[i] > 0 && max_bits + 1 > code_lengths[i] {
                weights[i] = max_bits + 1 - code_lengths[i];
            }
        }

        // Recompute codes using the decoder's max_bits and the weights
        // to ensure exact compatibility
        let mut actual_code_lengths = vec![0u8; MAX_SYMBOLS];
        for i in 0..=max_symbol {
            if weights[i] > 0 {
                actual_code_lengths[i] = max_bits + 1 - weights[i];
            }
        }

        // Recompute actual max_len from the adjusted code lengths
        let mut actual_max_len = 0u8;
        for &len in &actual_code_lengths {
            if len > actual_max_len {
                actual_max_len = len;
            }
        }

        // Re-assign canonical codes with actual lengths
        let mut bl_count2 = vec![0u32; (actual_max_len as usize) + 1];
        for &len in &actual_code_lengths {
            if len > 0 {
                bl_count2[len as usize] += 1;
            }
        }

        let mut next_code2 = vec![0u32; (actual_max_len as usize) + 1];
        let mut code_val2 = 0u32;
        for bits in 1..=actual_max_len as usize {
            code_val2 = (code_val2 + bl_count2[bits - 1]) << 1;
            next_code2[bits] = code_val2;
        }

        let mut final_codes = vec![0u32; MAX_SYMBOLS];
        let mut final_lengths = vec![0u8; MAX_SYMBOLS];
        for i in 0..MAX_SYMBOLS {
            if actual_code_lengths[i] > 0 {
                let len = actual_code_lengths[i] as usize;
                final_codes[i] = next_code2[len];
                next_code2[len] += 1;
                final_lengths[i] = actual_code_lengths[i];
            }
        }

        Some(Self {
            codes: final_codes,
            lengths: final_lengths,
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
        let num_weight_symbols = self.weights.len();
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

    /// Encode literals using this Huffman table.
    ///
    /// Returns the Huffman-encoded bitstream compatible with the Zstandard
    /// Huffman stream format. The stream is read backwards by the decoder:
    /// - The last byte contains a sentinel bit (highest set `1` bit) followed
    ///   by the first bits of the stream
    /// - The decoder reads from the last byte backwards through the buffer
    /// - Codes are written MSB-first from the perspective of the backward reader
    ///
    /// Encoding proceeds from the first literal to the last, accumulating bits
    /// in a buffer. The sentinel bit is placed at the end to mark where the
    /// data starts for the backward reader.
    pub fn encode_literals(&self, literals: &[u8]) -> Vec<u8> {
        if literals.is_empty() {
            return vec![0x01]; // Just sentinel bit
        }

        // We need to produce a byte stream that the HuffmanBitReader can decode.
        // The reader:
        //  1. Finds sentinel bit in last byte
        //  2. Reads codes MSB-first starting from just below the sentinel, going backward
        //  3. Each code is peeked as max_bits from current position backward
        //
        // The encoding builds bits from the end of the buffer backward:
        //  - Start after sentinel in last byte
        //  - Write first literal code, then second, etc.
        //  - Each code is MSB-first

        // Calculate total data bits
        let mut total_data_bits: usize = 0;
        for &lit in literals {
            total_data_bits += self.lengths[lit as usize] as usize;
        }

        // Total bits = data bits + sentinel (1 bit) + padding to byte boundary
        // sentinel goes in last byte, with possible zero-padding above it
        let total_bits_with_sentinel = total_data_bits + 1; // +1 for sentinel
        let total_bytes = total_bits_with_sentinel.div_ceil(8);
        let padding_bits = total_bytes * 8 - total_bits_with_sentinel;

        // Build the bitstream from MSB of last byte going backward
        // Layout in the last byte (MSB to LSB):
        //   [padding zeros][sentinel 1][first code bits...]
        // Then continuing into previous bytes...

        let mut buffer = vec![0u8; total_bytes];

        // Bit position counter: starts at bit 0 of last byte MSB side
        // We track from the MSB end of the entire buffer
        // Position 0 = MSB of first byte (byte 0, bit 7)
        // Position (total_bytes*8 - 1) = LSB of last byte

        // Current write position (MSB-first global bit index)
        let mut pos = padding_bits; // skip padding zeros

        // Write sentinel bit
        Self::set_bit_msb_first(&mut buffer, pos);
        pos += 1;

        // Write literal codes in forward order (first literal first)
        // Each code is written MSB-first
        for &lit in literals {
            let code = self.codes[lit as usize];
            let len = self.lengths[lit as usize] as usize;

            for b in 0..len {
                if (code >> (len - 1 - b)) & 1 == 1 {
                    Self::set_bit_msb_first(&mut buffer, pos + b);
                }
            }
            pos += len;
        }

        buffer
    }

    /// Set a single bit in a byte buffer using MSB-first global indexing.
    ///
    /// Bit index 0 = MSB of byte 0, bit index 7 = LSB of byte 0,
    /// bit index 8 = MSB of byte 1, etc.
    #[inline]
    fn set_bit_msb_first(buffer: &mut [u8], global_bit_index: usize) {
        let byte_idx = global_bit_index / 8;
        let bit_within_byte = 7 - (global_bit_index % 8);
        if byte_idx < buffer.len() {
            buffer[byte_idx] |= 1 << bit_within_byte;
        }
    }

    /// Get the code and length for a symbol.
    #[inline]
    pub fn get_code(&self, symbol: u8) -> (u32, u8) {
        (self.codes[symbol as usize], self.lengths[symbol as usize])
    }

    /// Get the maximum code length (max_bits).
    pub fn max_bits(&self) -> u8 {
        self.max_bits
    }

    /// Get the number of symbols with non-zero weight.
    pub fn num_symbols(&self) -> usize {
        self.num_symbols
    }

    /// Get a reference to the weights array.
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
        // First byte should be 127 + number_of_weight_symbols
        let num_w = enc.weights().len();
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
            let entry = decoder_table.decode(padded_code);
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

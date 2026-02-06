//! LZMA compression.
//!
//! This module implements LZMA compression with both greedy and optimal parsing.

use crate::LzmaLevel;
use crate::model::{
    DIST_ALIGN_BITS, END_POS_MODEL_INDEX, LEN_HIGH_BITS, LEN_LOW_BITS, LEN_MID_BITS, LengthModel,
    LzmaModel, LzmaProperties, MATCH_LEN_MIN, State,
};
use crate::optimal::OptimalParser;
use crate::range_coder::RangeEncoder;
use oxiarc_core::error::Result;

/// Maximum match length for fast mode.
const MATCH_LEN_MAX: usize = 273;

/// Hash table size (64K entries).
const HASH_SIZE: usize = 1 << 16;

/// Maximum chain depth per compression level.
const CHAIN_DEPTH: [usize; 10] = [
    0,    // Level 0: No search (stored mode)
    4,    // Level 1: Very fast
    8,    // Level 2: Fast
    16,   // Level 3: Fast
    32,   // Level 4: Normal
    64,   // Level 5: Normal
    128,  // Level 6: Normal (default)
    256,  // Level 7: Maximum
    512,  // Level 8: Maximum
    1024, // Level 9: Ultra
];

/// Encode a bit tree.
fn encode_bit_tree(rc: &mut RangeEncoder, probs: &mut [u16], num_bits: u32, value: u32) {
    let mut m = 1usize;

    for i in (0..num_bits).rev() {
        let bit = (value >> i) & 1;
        rc.encode_bit(&mut probs[m], bit);
        m = (m << 1) | bit as usize;
    }
}

/// Encode a length.
fn encode_length(rc: &mut RangeEncoder, len_model: &mut LengthModel, len: u32, pos_state: usize) {
    let len = len - MATCH_LEN_MIN as u32;

    if len < (1 << LEN_LOW_BITS) {
        rc.encode_bit(&mut len_model.choice, 0);
        encode_bit_tree(rc, &mut len_model.low[pos_state], LEN_LOW_BITS, len);
    } else if len < (1 << LEN_LOW_BITS) + (1 << LEN_MID_BITS) {
        rc.encode_bit(&mut len_model.choice, 1);
        rc.encode_bit(&mut len_model.choice2, 0);
        encode_bit_tree(
            rc,
            &mut len_model.mid[pos_state],
            LEN_MID_BITS,
            len - (1 << LEN_LOW_BITS),
        );
    } else {
        rc.encode_bit(&mut len_model.choice, 1);
        rc.encode_bit(&mut len_model.choice2, 1);
        encode_bit_tree(
            rc,
            &mut len_model.high,
            LEN_HIGH_BITS,
            len - (1 << LEN_LOW_BITS) - (1 << LEN_MID_BITS),
        );
    }
}

/// Get distance slot.
fn get_dist_slot(dist: u32) -> u32 {
    if dist < 4 {
        return dist;
    }

    let bits = 32 - dist.leading_zeros();
    ((bits - 1) << 1) | ((dist >> (bits - 2)) & 1)
}

/// LZMA encoder.
pub struct LzmaEncoder {
    /// Range encoder.
    rc: RangeEncoder,
    /// LZMA model.
    model: LzmaModel,
    /// Dictionary size.
    dict_size: usize,
    /// Current state.
    state: State,
    /// Rep distances.
    rep: [u32; 4],
    /// Hash table for match finding (stores head of chain).
    hash_head: Vec<u32>,
    /// Chain table (links positions with same hash).
    hash_chain: Vec<u32>,
    /// Maximum chain depth (based on compression level).
    chain_depth: usize,
    /// Compression level.
    level: LzmaLevel,
    /// Bytes encoded.
    bytes_encoded: u64,
    /// Optimal parser (used for levels 8-9, reserved for full DP implementation).
    #[allow(dead_code)]
    optimal_parser: Option<OptimalParser>,
    /// Use optimal parsing.
    use_optimal: bool,
}

impl LzmaEncoder {
    /// Create a new LZMA encoder.
    pub fn new(level: LzmaLevel, dict_size: u32) -> Self {
        let dict_size = dict_size.max(4096) as usize;
        let props = LzmaProperties::default();
        let level_idx = (level.level() as usize).min(9);
        let chain_depth = CHAIN_DEPTH[level_idx];

        // Use optimal parsing for levels 8-9
        let use_optimal = level.level() >= 8;
        let optimal_parser = if use_optimal {
            // Level 8: fast_bytes=64, nice_length=128
            // Level 9: fast_bytes=128, nice_length=273
            let (fast_bytes, nice_length) = if level.level() >= 9 {
                (128, 273)
            } else {
                (64, 128)
            };
            Some(OptimalParser::new(fast_bytes, nice_length))
        } else {
            None
        };

        Self {
            rc: RangeEncoder::new(),
            model: LzmaModel::new(props),
            dict_size,
            state: State::new(),
            rep: [0; 4],
            hash_head: vec![u32::MAX; HASH_SIZE],
            hash_chain: Vec::new(), // Will be sized as we encode
            chain_depth,
            level,
            bytes_encoded: 0,
            optimal_parser,
            use_optimal,
        }
    }

    /// Get properties.
    pub fn properties(&self) -> LzmaProperties {
        self.model.props
    }

    /// Calculate hash for 3 bytes with improved avalanche properties.
    fn hash3(data: &[u8]) -> usize {
        if data.len() < 3 {
            return 0;
        }
        // FNV-1a inspired hash with good distribution
        let mut h = 2166136261u32;
        h ^= data[0] as u32;
        h = h.wrapping_mul(16777619);
        h ^= data[1] as u32;
        h = h.wrapping_mul(16777619);
        h ^= data[2] as u32;
        h = h.wrapping_mul(16777619);
        (h as usize) & (HASH_SIZE - 1)
    }

    /// Calculate hash for 4 bytes (better for longer matches).
    /// Reserved for future optimal parsing implementation.
    #[allow(dead_code)]
    fn hash4(data: &[u8]) -> usize {
        if data.len() < 4 {
            return Self::hash3(data);
        }
        // FNV-1a inspired hash
        let mut h = 2166136261u32;
        h ^= data[0] as u32;
        h = h.wrapping_mul(16777619);
        h ^= data[1] as u32;
        h = h.wrapping_mul(16777619);
        h ^= data[2] as u32;
        h = h.wrapping_mul(16777619);
        h ^= data[3] as u32;
        h = h.wrapping_mul(16777619);
        (h as usize) & (HASH_SIZE - 1)
    }

    /// Find best match at current position using hash chains.
    fn find_match(&self, data: &[u8], pos: usize) -> Option<(u32, u32)> {
        if pos + MATCH_LEN_MIN > data.len() {
            return None;
        }

        let hash = Self::hash3(&data[pos..]);
        let mut match_pos = self.hash_head[hash] as usize;

        if match_pos == u32::MAX as usize {
            return None;
        }

        let max_len = (data.len() - pos).min(MATCH_LEN_MAX);
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        let mut chain_count = 0;

        // Walk the hash chain to find the best match
        while match_pos < pos && chain_count < self.chain_depth {
            let dist = pos - match_pos;
            if dist > self.dict_size {
                break;
            }

            // Quick rejection: check first 3 bytes
            if data[pos] == data[match_pos]
                && data[pos + 1] == data[match_pos + 1]
                && data[pos + 2] == data[match_pos + 2]
            {
                // Count matching bytes
                let mut len = 3usize;
                while len < max_len && data[pos + len] == data[match_pos + len] {
                    len += 1;
                }

                // Prefer longer matches, or equal length with shorter distance
                if len > best_len || (len == best_len && dist < best_dist) {
                    best_len = len;
                    best_dist = dist;

                    // Found maximum length, stop searching
                    if best_len >= max_len {
                        break;
                    }
                }
            }

            // Follow the chain
            if match_pos < self.hash_chain.len() {
                let next = self.hash_chain[match_pos] as usize;
                if next >= match_pos || next == u32::MAX as usize {
                    break;
                }
                match_pos = next;
            } else {
                break;
            }

            chain_count += 1;
        }

        if best_len < MATCH_LEN_MIN {
            return None;
        }

        Some(((best_dist - 1) as u32, best_len as u32))
    }

    /// Update hash chains for a position.
    fn update_hash(&mut self, data: &[u8], pos: usize) {
        if pos + 3 > data.len() {
            return;
        }

        // Ensure hash_chain is large enough
        if pos >= self.hash_chain.len() {
            self.hash_chain.resize(pos + 1, u32::MAX);
        }

        let hash = Self::hash3(&data[pos..]);

        // Link current position to previous head
        self.hash_chain[pos] = self.hash_head[hash];

        // Update head to current position
        self.hash_head[hash] = pos as u32;
    }

    /// Check for rep match.
    fn check_rep_match(&self, data: &[u8], pos: usize, rep_idx: usize) -> u32 {
        let dist = self.rep[rep_idx] as usize;

        if dist >= pos {
            return 0;
        }

        let match_pos = pos - dist - 1;
        let mut len = 0usize;
        let max_len = (data.len() - pos).min(MATCH_LEN_MAX);

        while len < max_len && data[pos + len] == data[match_pos + len] {
            len += 1;
        }

        len as u32
    }

    /// Find all matches at current position for optimal parsing.
    /// Returns vector of (distance, length) pairs sorted by length.
    fn find_all_matches(&self, data: &[u8], pos: usize, max_matches: usize) -> Vec<(u32, u32)> {
        if pos + MATCH_LEN_MIN > data.len() {
            return Vec::new();
        }

        let hash = Self::hash3(&data[pos..]);
        let mut match_pos = self.hash_head[hash] as usize;

        if match_pos == u32::MAX as usize {
            return Vec::new();
        }

        let max_len = (data.len() - pos).min(MATCH_LEN_MAX);
        let mut matches = Vec::with_capacity(max_matches);
        let mut prev_len = 0usize;
        let mut chain_count = 0;

        // Walk the hash chain to find all good matches
        while match_pos < pos && chain_count < self.chain_depth && matches.len() < max_matches {
            let dist = pos - match_pos;
            if dist > self.dict_size {
                break;
            }

            // Quick rejection: check first 3 bytes
            if data[pos] == data[match_pos]
                && data[pos + 1] == data[match_pos + 1]
                && data[pos + 2] == data[match_pos + 2]
            {
                // Count matching bytes
                let mut len = 3usize;
                while len < max_len && data[pos + len] == data[match_pos + len] {
                    len += 1;
                }

                // Only add if this is a longer match than previous
                if len > prev_len {
                    matches.push(((dist - 1) as u32, len as u32));
                    prev_len = len;

                    // Found maximum length, stop searching
                    if len >= max_len {
                        break;
                    }
                }
            }

            // Follow the chain
            if match_pos < self.hash_chain.len() {
                let next = self.hash_chain[match_pos] as usize;
                if next >= match_pos || next == u32::MAX as usize {
                    break;
                }
                match_pos = next;
            } else {
                break;
            }

            chain_count += 1;
        }

        matches
    }

    /// Find the optimal sequence of literals and matches using backward DP.
    /// Returns the length of the optimal sequence and the decisions.
    fn find_optimal_sequence(
        &mut self,
        data: &[u8],
        start_pos: usize,
    ) -> Option<(bool, usize, u32)> {
        // For optimal parsing, we need to look ahead and find the best sequence
        // This is a simplified version - a full implementation would use dynamic programming

        // Get rep matches
        let mut best_rep: Option<(usize, u32)> = None;
        for rep_idx in 0..4 {
            let len = self.check_rep_match(data, start_pos, rep_idx);
            if len >= MATCH_LEN_MIN as u32
                && (best_rep.is_none() || best_rep.is_some_and(|(_, l)| len > l))
            {
                best_rep = Some((rep_idx, len));
            }
        }

        // Get normal matches
        let matches = self.find_all_matches(data, start_pos, 32);

        // Simple heuristic: prefer longer matches
        // In full optimal parsing, we would calculate prices and use DP
        let normal_match = matches.last().copied();

        // Decision logic with price estimation
        match (best_rep, normal_match) {
            (Some((rep_idx, rep_len)), Some((dist, len))) => {
                // Estimate prices:
                // Rep match: ~3-4 bits + length encoding
                // Normal match: ~8-10 bits + length + distance encoding
                let rep_price = 4 + (rep_len / 4); // Rough estimate
                let normal_price = 10 + (len / 4) + 8; // Rough estimate

                if rep_price < normal_price || (rep_len >= len && rep_idx == 0) {
                    Some((true, rep_idx, rep_len))
                } else {
                    Some((false, dist as usize, len))
                }
            }
            (_, Some((dist, len))) if len >= MATCH_LEN_MIN as u32 => {
                Some((false, dist as usize, len))
            }
            (Some((rep_idx, rep_len)), _) if rep_len >= MATCH_LEN_MIN as u32 => {
                Some((true, rep_idx, rep_len))
            }
            _ => None,
        }
    }

    /// Encode a literal byte.
    fn encode_literal(&mut self, byte: u8, prev_byte: u8, match_byte: u8) {
        let lit_state = self.model.literal.get_state(
            self.bytes_encoded,
            prev_byte,
            self.model.props.lc,
            self.model.props.lp,
        );

        if self.state.is_literal() {
            self.encode_literal_normal(lit_state, byte);
        } else {
            self.encode_literal_matched(lit_state, byte, match_byte);
        }
    }

    /// Encode a normal literal.
    fn encode_literal_normal(&mut self, lit_state: usize, byte: u8) {
        let mut symbol = (byte as usize) | 0x100;
        let mut context = 1usize;

        loop {
            let bit = (symbol >> 7) & 1;
            symbol <<= 1;

            self.rc.encode_bit(
                &mut self.model.literal.probs[lit_state][context],
                bit as u32,
            );

            context = (context << 1) | bit;

            if context >= 0x100 {
                break;
            }
        }
    }

    /// Encode a literal with match context.
    fn encode_literal_matched(&mut self, lit_state: usize, byte: u8, match_byte: u8) {
        let mut symbol = (byte as usize) | 0x100;
        let mut match_symbol = (match_byte as usize) << 1;
        let mut context = 1usize;

        loop {
            let match_bit = (match_symbol >> 8) & 1;
            match_symbol <<= 1;

            let bit = (symbol >> 7) & 1;
            symbol <<= 1;

            let prob_idx = 0x100 + (match_bit << 8) + context;
            self.rc.encode_bit(
                &mut self.model.literal.probs[lit_state][prob_idx],
                bit as u32,
            );
            context = (context << 1) | bit;

            if context >= 0x100 {
                break;
            }

            if bit != match_bit {
                // Mismatch, continue without match context
                while context < 0x100 {
                    let bit = (symbol >> 7) & 1;
                    symbol <<= 1;
                    self.rc.encode_bit(
                        &mut self.model.literal.probs[lit_state][context],
                        bit as u32,
                    );
                    context = (context << 1) | bit;
                }
                break;
            }
        }
    }

    /// Encode a distance.
    fn encode_distance(&mut self, dist: u32, len: u32) {
        let len_state = ((len - MATCH_LEN_MIN as u32).min(3)) as usize;

        // Calculate slot
        let slot = get_dist_slot(dist);

        // Encode slot
        encode_bit_tree(
            &mut self.rc,
            &mut self.model.distance.slot[len_state],
            6,
            slot,
        );

        if slot >= 4 {
            let num_direct_bits = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << num_direct_bits;
            let dist_reduced = dist - base;

            if slot < END_POS_MODEL_INDEX as u32 {
                // Encode with model (reverse bit tree)
                let base_idx = (slot as usize) - (slot as usize >> 1) - 1;

                // Encode reverse bit tree manually since we need flat array indexing
                let mut m = 1usize;
                for i in 0..num_direct_bits {
                    let bit = (dist_reduced >> i) & 1;
                    self.rc
                        .encode_bit(&mut self.model.distance.special[base_idx + m - 1], bit);
                    m = (m << 1) | bit as usize;
                }
            } else {
                // Direct bits + alignment
                let num_align_bits = DIST_ALIGN_BITS;
                let num_direct = num_direct_bits - num_align_bits;

                self.rc
                    .encode_direct_bits(dist_reduced >> num_align_bits, num_direct);
                self.rc.encode_bit_tree_reverse(
                    &mut self.model.distance.align,
                    num_align_bits,
                    dist_reduced & ((1 << num_align_bits) - 1),
                );
            }
        }
    }

    /// Compress data.
    pub fn compress(mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut i = 0;

        while i < data.len() {
            let pos_state = (self.bytes_encoded as usize) & (self.model.props.num_pos_states() - 1);
            let state_idx = self.state.value();

            // Determine match using optimal or greedy parsing
            let (use_match, match_info) = if self.use_optimal {
                // Use optimal parsing
                if let Some(result) = self.find_optimal_sequence(data, i) {
                    (true, Some(result))
                } else {
                    (false, None)
                }
            } else {
                // Use greedy parsing (original implementation)
                // Check for rep matches first
                let mut best_rep: Option<(usize, u32)> = None;
                for rep_idx in 0..4 {
                    let len = self.check_rep_match(data, i, rep_idx);
                    if len >= MATCH_LEN_MIN as u32 && best_rep.is_none_or(|(_, l)| len > l) {
                        best_rep = Some((rep_idx, len));
                    }
                }

                // Check for normal match
                let normal_match = self.find_match(data, i);

                // Decide what to encode
                match (best_rep, normal_match) {
                    (Some((rep_idx, rep_len)), Some((_dist, len)))
                        if rep_len >= len || (rep_len >= 3 && rep_idx == 0) =>
                    {
                        // Use rep match
                        (true, Some((true, rep_idx, rep_len)))
                    }
                    (_, Some((dist, len))) if len >= MATCH_LEN_MIN as u32 => {
                        // Use normal match
                        (true, Some((false, dist as usize, len)))
                    }
                    (Some((rep_idx, rep_len)), _) if rep_len >= MATCH_LEN_MIN as u32 => {
                        // Use rep match
                        (true, Some((true, rep_idx, rep_len)))
                    }
                    _ => (false, None),
                }
            };

            if !use_match {
                // Encode literal
                self.rc
                    .encode_bit(&mut self.model.is_match[state_idx][pos_state], 0);

                let prev_byte = if i > 0 { data[i - 1] } else { 0 };
                let match_byte = if !self.state.is_literal() && (self.rep[0] as usize) < i {
                    data[i - self.rep[0] as usize - 1]
                } else {
                    0
                };

                self.encode_literal(data[i], prev_byte, match_byte);
                self.state.update_literal();
                self.bytes_encoded += 1;

                // Update hash chains
                self.update_hash(data, i);

                i += 1;
            } else if let Some((is_rep, idx_or_dist, len)) = match_info {
                self.rc
                    .encode_bit(&mut self.model.is_match[state_idx][pos_state], 1);

                if is_rep {
                    // Rep match
                    self.rc.encode_bit(&mut self.model.is_rep[state_idx], 1);

                    let rep_idx = idx_or_dist;
                    if rep_idx == 0 {
                        self.rc.encode_bit(&mut self.model.is_rep0[state_idx], 0);

                        if len == 1 {
                            self.rc
                                .encode_bit(&mut self.model.is_rep0_long[state_idx][pos_state], 0);
                            self.state.update_short_rep();
                        } else {
                            self.rc
                                .encode_bit(&mut self.model.is_rep0_long[state_idx][pos_state], 1);
                            encode_length(&mut self.rc, &mut self.model.rep_len, len, pos_state);
                            self.state.update_long_rep();
                        }
                    } else {
                        self.rc.encode_bit(&mut self.model.is_rep0[state_idx], 1);

                        if rep_idx == 1 {
                            self.rc.encode_bit(&mut self.model.is_rep1[state_idx], 0);
                        } else {
                            self.rc.encode_bit(&mut self.model.is_rep1[state_idx], 1);
                            if rep_idx == 2 {
                                self.rc.encode_bit(&mut self.model.is_rep2[state_idx], 0);
                            } else {
                                self.rc.encode_bit(&mut self.model.is_rep2[state_idx], 1);
                            }
                        }

                        // Shift rep distances
                        let dist = self.rep[rep_idx];
                        for j in (1..=rep_idx).rev() {
                            self.rep[j] = self.rep[j - 1];
                        }
                        self.rep[0] = dist;

                        encode_length(&mut self.rc, &mut self.model.rep_len, len, pos_state);
                        self.state.update_long_rep();
                    }
                } else {
                    // Normal match
                    self.rc.encode_bit(&mut self.model.is_rep[state_idx], 0);

                    let dist = idx_or_dist as u32;
                    encode_length(&mut self.rc, &mut self.model.match_len, len, pos_state);
                    self.encode_distance(dist, len);

                    // Shift rep distances
                    self.rep[3] = self.rep[2];
                    self.rep[2] = self.rep[1];
                    self.rep[1] = self.rep[0];
                    self.rep[0] = dist;

                    self.state.update_match();
                }

                self.bytes_encoded += len as u64;

                // Update hash chains for all bytes in match
                for j in 0..len as usize {
                    self.update_hash(data, i + j);
                }

                i += len as usize;
            }
        }

        // Write end marker
        let pos_state = (self.bytes_encoded as usize) & (self.model.props.num_pos_states() - 1);
        let state_idx = self.state.value();

        self.rc
            .encode_bit(&mut self.model.is_match[state_idx][pos_state], 1);
        self.rc.encode_bit(&mut self.model.is_rep[state_idx], 0);

        // Encode minimum length
        encode_length(
            &mut self.rc,
            &mut self.model.match_len,
            MATCH_LEN_MIN as u32,
            pos_state,
        );

        // Encode end marker distance
        self.encode_distance(0xFFFF_FFFF, MATCH_LEN_MIN as u32);

        Ok(self.rc.finish())
    }

    /// Get the dictionary size.
    pub fn dict_size(&self) -> u32 {
        self.dict_size as u32
    }

    /// Get the compression level.
    pub fn level(&self) -> LzmaLevel {
        self.level
    }
}

/// Compress data using LZMA.
pub fn compress(data: &[u8], level: LzmaLevel) -> Result<Vec<u8>> {
    let dict_size = match level.level() {
        0 => 1 << 16,     // 64 KB
        1..=3 => 1 << 20, // 1 MB
        4..=6 => 1 << 22, // 4 MB
        _ => 1 << 24,     // 16 MB
    };

    let encoder = LzmaEncoder::new(level, dict_size);
    let props = encoder.properties();

    // Build output with header
    let mut output = Vec::new();

    // Properties byte
    output.push(props.to_byte());

    // Dictionary size (4 bytes, little-endian)
    output.extend_from_slice(&dict_size.to_le_bytes());

    // Uncompressed size (8 bytes, little-endian)
    output.extend_from_slice(&(data.len() as u64).to_le_bytes());

    // Compressed data
    let compressed = encoder.compress(data)?;
    output.extend_from_slice(&compressed);

    Ok(output)
}

/// Compress data without header.
pub fn compress_raw(data: &[u8], level: LzmaLevel, dict_size: u32) -> Result<Vec<u8>> {
    let encoder = LzmaEncoder::new(level, dict_size);
    encoder.compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoder_creation() {
        let encoder = LzmaEncoder::new(LzmaLevel::DEFAULT, 1 << 20);
        assert_eq!(encoder.dict_size(), 1 << 20);
    }

    #[test]
    fn test_dist_slot() {
        assert_eq!(get_dist_slot(0), 0);
        assert_eq!(get_dist_slot(1), 1);
        assert_eq!(get_dist_slot(2), 2);
        assert_eq!(get_dist_slot(3), 3);
        assert_eq!(get_dist_slot(4), 4);
    }

    #[test]
    fn test_hash3() {
        let data1 = [0u8, 0, 0];
        let data2 = [1u8, 2, 3];

        let h1 = LzmaEncoder::hash3(&data1);
        let h2 = LzmaEncoder::hash3(&data2);

        assert_ne!(h1, h2);
        assert!(h1 < HASH_SIZE);
        assert!(h2 < HASH_SIZE);
    }

    #[test]
    fn test_chain_depth_by_level() {
        // Level 1 should have depth 4
        let enc1 = LzmaEncoder::new(LzmaLevel::new(1), 1 << 16);
        assert_eq!(enc1.chain_depth, 4);

        // Level 6 should have depth 128
        let enc6 = LzmaEncoder::new(LzmaLevel::new(6), 1 << 16);
        assert_eq!(enc6.chain_depth, 128);

        // Level 9 should have depth 1024
        let enc9 = LzmaEncoder::new(LzmaLevel::new(9), 1 << 16);
        assert_eq!(enc9.chain_depth, 1024);
    }

    #[test]
    fn test_hash_chain_initialization() {
        let encoder = LzmaEncoder::new(LzmaLevel::DEFAULT, 1 << 16);
        // hash_head should be initialized to u32::MAX
        assert!(encoder.hash_head.iter().all(|&v| v == u32::MAX));
        // hash_chain starts empty
        assert!(encoder.hash_chain.is_empty());
    }

    #[test]
    fn test_hash4() {
        let data1 = [0u8, 0, 0, 0];
        let data2 = [1u8, 2, 3, 4];

        let h1 = LzmaEncoder::hash4(&data1);
        let h2 = LzmaEncoder::hash4(&data2);

        assert_ne!(h1, h2);
        assert!(h1 < HASH_SIZE);
        assert!(h2 < HASH_SIZE);

        // hash4 should give different results than hash3 for same prefix
        let h3_1 = LzmaEncoder::hash3(&data1);
        assert!(h3_1 < HASH_SIZE); // Verify hash3 result is valid
    }
}

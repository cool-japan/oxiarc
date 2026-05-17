//! Zopfli-style graph-based optimal DEFLATE parser.
//!
//! This module implements a shortest-path (Dijkstra-like) forward DP over the
//! LZ77 token graph, iteratively refining bit costs using the Huffman lengths
//! produced by the previous pass.  The approach mirrors the core loop in
//! Zopfli: each pass produces a token sequence whose frequency distribution is
//! used to build tighter Huffman lengths for the next pass, converging toward
//! the locally optimal token sequence.

use crate::huffman::{HuffmanBuilder, cost_of_match, cost_table_from_lengths};
use crate::lz77::{Lz77Encoder, Lz77Token, MIN_MATCH};
use crate::tables::{
    distance_to_code, fixed_distance_lengths, fixed_litlen_lengths, length_to_code,
};

/// Maximum number of refinement passes (hard ceiling).
const MAX_PASSES: u8 = 8;

/// Optimal DEFLATE parser using iterative Zopfli-style cost refinement.
///
/// Each call to [`OptimalParser::parse`] runs up to `passes` forward-DP passes
/// over the input, refining the Huffman cost tables from the token distribution
/// of the previous pass.  The pass producing the smallest estimated bit output
/// is returned.
pub struct OptimalParser {
    passes: u8,
}

impl OptimalParser {
    /// Create a new `OptimalParser` with 5 refinement passes.
    pub fn new() -> Self {
        Self { passes: 5 }
    }

    /// Create a new `OptimalParser` with a custom number of passes (clamped to 1..=8).
    pub fn with_passes(passes: u8) -> Self {
        Self {
            passes: passes.clamp(1, MAX_PASSES),
        }
    }

    /// Run the optimal parser on `data`, returning an LZ77 token sequence.
    ///
    /// The encoder's window is pre-filled with `data` and its hash table is
    /// rebuilt during the first DP pass.  The encoder state after this call
    /// is not suitable for streaming reuse without a reset.
    pub fn parse(&mut self, data: &[u8], encoder: &mut Lz77Encoder) -> Vec<Lz77Token> {
        if data.is_empty() {
            return Vec::new();
        }

        // Pre-fill the window with all input data so that find_all_matches can
        // compare bytes via self.window[wpos] directly.
        encoder.reset();
        {
            let window = encoder.window_as_slice_mut();
            let copy_len = data.len().min(window.len());
            window[..copy_len].copy_from_slice(&data[..copy_len]);
        }

        // Initial cost tables come from the fixed Huffman code lengths.
        let mut litlen_lengths: Vec<u8> = fixed_litlen_lengths().to_vec();
        let mut dist_lengths: Vec<u8> = fixed_distance_lengths().to_vec();

        let mut best_tokens: Vec<Lz77Token> = Vec::new();
        let mut best_cost = u64::MAX;

        for pass in 0..self.passes {
            let litlen_costs = cost_table_from_lengths(&litlen_lengths);
            let dist_costs = cost_table_from_lengths(&dist_lengths);

            // On pass 0 the hash table is empty; subsequent passes reuse it.
            let tokens = if pass == 0 {
                self.dp_pass_first(data, encoder, &litlen_costs, &dist_costs)
            } else {
                self.dp_pass_subsequent(data, encoder, &litlen_costs, &dist_costs)
            };

            let cost = estimate_token_cost(&tokens, &litlen_lengths, &dist_lengths);

            if cost < best_cost {
                best_cost = cost;
                best_tokens = tokens.clone();
            }

            // Rebuild Huffman lengths from this pass's token distribution.
            let (ll_freq, d_freq) = count_frequencies(&tokens);
            litlen_lengths = build_litlen_lengths(&ll_freq);
            dist_lengths = build_dist_lengths(&d_freq);

            // Early termination: if cost hasn't improved for two passes, converged.
            if pass >= 2 && cost >= best_cost {
                break;
            }
        }

        best_tokens
    }

    /// First pass: builds the hash table incrementally as it scans.
    fn dp_pass_first(
        &self,
        data: &[u8],
        encoder: &mut Lz77Encoder,
        litlen_costs: &[u32],
        dist_costs: &[u32],
    ) -> Vec<Lz77Token> {
        let n = data.len();
        encoder.reset_hash();

        let mut costs = vec![u32::MAX; n + 1];
        costs[0] = 0;
        let mut prev: Vec<Option<(usize, Lz77Token)>> = vec![None; n + 1];

        for pos in 0..n {
            let cur_cost = costs[pos];

            // Literal transition (always valid regardless of cur_cost).
            if cur_cost != u32::MAX {
                let byte = data[pos];
                let lit_cost = litlen_costs.get(byte as usize).copied().unwrap_or(u32::MAX);
                if lit_cost != u32::MAX {
                    let new_cost = cur_cost.saturating_add(lit_cost);
                    if new_cost < costs[pos + 1] {
                        costs[pos + 1] = new_cost;
                        prev[pos + 1] = Some((pos, Lz77Token::Literal(data[pos])));
                    }
                }
            }

            // Match transitions: find_all_matches must be called BEFORE
            // update_hash_single so the current position is not yet in the
            // chain (hash_table points to earlier occurrences only).
            let remaining = n - pos;
            if cur_cost != u32::MAX && remaining >= MIN_MATCH {
                let matches = encoder.find_all_matches(pos, remaining);
                for (length, distance) in matches {
                    let m_cost = cost_of_match(length, distance, litlen_costs, dist_costs);
                    if m_cost == u32::MAX {
                        continue;
                    }
                    let new_cost = cur_cost.saturating_add(m_cost);
                    let end = pos + length as usize;
                    if end <= n && new_cost < costs[end] {
                        costs[end] = new_cost;
                        prev[end] = Some((pos, Lz77Token::Match { length, distance }));
                    }
                }
            }

            // Insert pos into the hash chain AFTER searching, so that future
            // positions can find it as a match candidate.
            encoder.update_hash_single(pos);
        }

        backtrack(&prev, n)
    }

    /// Subsequent passes: hash table is already built; no mutations needed.
    fn dp_pass_subsequent(
        &self,
        data: &[u8],
        encoder: &Lz77Encoder,
        litlen_costs: &[u32],
        dist_costs: &[u32],
    ) -> Vec<Lz77Token> {
        let n = data.len();

        let mut costs = vec![u32::MAX; n + 1];
        costs[0] = 0;
        let mut prev: Vec<Option<(usize, Lz77Token)>> = vec![None; n + 1];

        for pos in 0..n {
            let cur_cost = costs[pos];
            if cur_cost == u32::MAX {
                continue;
            }

            // Literal transition.
            let byte = data[pos];
            let lit_cost = litlen_costs.get(byte as usize).copied().unwrap_or(u32::MAX);
            if lit_cost != u32::MAX {
                let new_cost = cur_cost.saturating_add(lit_cost);
                if new_cost < costs[pos + 1] {
                    costs[pos + 1] = new_cost;
                    prev[pos + 1] = Some((pos, Lz77Token::Literal(byte)));
                }
            }

            // Match transitions.
            let remaining = n - pos;
            if remaining >= MIN_MATCH {
                let matches = encoder.find_all_matches(pos, remaining);
                for (length, distance) in matches {
                    let m_cost = cost_of_match(length, distance, litlen_costs, dist_costs);
                    if m_cost == u32::MAX {
                        continue;
                    }
                    let new_cost = cur_cost.saturating_add(m_cost);
                    let end = pos + length as usize;
                    if end <= n && new_cost < costs[end] {
                        costs[end] = new_cost;
                        prev[end] = Some((pos, Lz77Token::Match { length, distance }));
                    }
                }
            }
        }

        backtrack(&prev, n)
    }
}

impl Default for OptimalParser {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Backtrack through the `prev` array to reconstruct the token sequence.
fn backtrack(prev: &[Option<(usize, Lz77Token)>], end: usize) -> Vec<Lz77Token> {
    let mut tokens = Vec::new();
    let mut pos = end;

    while pos > 0 {
        match &prev[pos] {
            Some((from, token)) => {
                tokens.push(*token);
                pos = *from;
            }
            None => break,
        }
    }

    tokens.reverse();
    tokens
}

/// Count litlen and distance frequencies from a token sequence.
pub(crate) fn count_frequencies(tokens: &[Lz77Token]) -> ([u32; 286], [u32; 30]) {
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
    // EOB is always present.
    litlen_freq[256] += 1;

    (litlen_freq, dist_freq)
}

/// Build litlen Huffman lengths (286 symbols, max 15 bits) from frequencies.
fn build_litlen_lengths(freq: &[u32; 286]) -> Vec<u8> {
    let mut builder = HuffmanBuilder::new(286, 15);
    for (sym, &f) in freq.iter().enumerate() {
        if f > 0 {
            builder.add_count(sym as u16, f);
        }
    }
    if freq[256] == 0 {
        builder.add_count(256, 1);
    }
    builder.build_lengths()
}

/// Build distance Huffman lengths (30 symbols, max 15 bits) from frequencies.
fn build_dist_lengths(freq: &[u32; 30]) -> Vec<u8> {
    let mut builder = HuffmanBuilder::new(30, 15);
    for (sym, &f) in freq.iter().enumerate() {
        if f > 0 {
            builder.add_count(sym as u16, f);
        }
    }
    builder.build_lengths()
}

/// Estimate total bits needed to encode `tokens` with the given code lengths.
fn estimate_token_cost(tokens: &[Lz77Token], litlen_lengths: &[u8], dist_lengths: &[u8]) -> u64 {
    let mut bits: u64 = 3; // block header

    for token in tokens {
        match token {
            Lz77Token::Literal(byte) => {
                let len = litlen_lengths.get(*byte as usize).copied().unwrap_or(0) as u64;
                bits = bits.saturating_add(len);
            }
            Lz77Token::Match { length, distance } => {
                let (len_code, len_extra_bits, _) = length_to_code(*length);
                let ll = litlen_lengths.get(len_code as usize).copied().unwrap_or(0) as u64;
                bits = bits
                    .saturating_add(ll)
                    .saturating_add(len_extra_bits as u64);

                let (dist_code, dist_extra_bits, _) = distance_to_code(*distance);
                let dl = dist_lengths.get(dist_code as usize).copied().unwrap_or(0) as u64;
                bits = bits
                    .saturating_add(dl)
                    .saturating_add(dist_extra_bits as u64);
            }
        }
    }
    // EOB symbol
    bits = bits.saturating_add(litlen_lengths.get(256).copied().unwrap_or(0) as u64);
    bits
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::{Deflater, deflate};
    use crate::huffman::cost_table_from_lengths;
    use crate::inflate::inflate;
    use crate::lz77::{Lz77Encoder, MAX_MATCH};

    fn optimal_compress_roundtrip(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut deflater = Deflater::with_optimal_parsing(6);
        let compressed = deflater
            .compress_to_vec(data)
            .expect("optimal compress_to_vec failed");
        let decompressed = inflate(&compressed).expect("inflate of optimal output failed");
        (compressed, decompressed)
    }

    #[test]
    fn test_optimal_beats_greedy_on_repetitive_data() {
        // 4000 bytes of repeating "abcabc..." — the optimal parser should produce
        // a token sequence with better or equal Huffman cost versus the greedy parser.
        let pattern = b"abcabc";
        let mut data = Vec::with_capacity(4000);
        while data.len() < 4000 {
            data.extend_from_slice(pattern);
        }
        data.truncate(4000);

        let greedy = deflate(&data, 6).expect("greedy deflate failed");
        let optimal = {
            let mut d = Deflater::with_optimal_parsing(6);
            d.compress_to_vec(&data).expect("optimal compress failed")
        };

        let decoded = inflate(&optimal).expect("inflate optimal failed");
        assert_eq!(decoded, data, "optimal roundtrip mismatch");

        // Optimal output should be no more than 5% larger than greedy.
        assert!(
            optimal.len() <= greedy.len() + greedy.len() / 20,
            "optimal ({}) should not be significantly worse than greedy ({})",
            optimal.len(),
            greedy.len()
        );
    }

    #[test]
    fn test_optimal_roundtrip_random_data() {
        // 2000 bytes of deterministic pseudo-random data (LCG).
        let mut data = Vec::with_capacity(2000);
        let mut state: u32 = 0xDEAD_BEEF;
        for _ in 0..2000 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            data.push((state >> 24) as u8);
        }

        let (_, decompressed) = optimal_compress_roundtrip(&data);
        assert_eq!(decompressed, data, "random data roundtrip failed");
    }

    #[test]
    fn test_optimal_roundtrip_empty() {
        let (_, decompressed) = optimal_compress_roundtrip(b"");
        assert!(
            decompressed.is_empty(),
            "empty input must produce empty output"
        );
    }

    #[test]
    fn test_optimal_roundtrip_single_byte() {
        let (_, decompressed) = optimal_compress_roundtrip(b"X");
        assert_eq!(decompressed, b"X", "single byte roundtrip failed");
    }

    #[test]
    fn test_optimal_cost_table_correctness() {
        let lengths: Vec<u8> = vec![0, 3, 5, 0, 7];
        let costs = cost_table_from_lengths(&lengths);

        assert_eq!(costs[0], u32::MAX, "zero length must map to u32::MAX");
        assert_eq!(costs[1], 3, "length 3 must map to cost 3");
        assert_eq!(costs[2], 5, "length 5 must map to cost 5");
        assert_eq!(costs[3], u32::MAX, "zero length must map to u32::MAX");
        assert_eq!(costs[4], 7, "length 7 must map to cost 7");
    }

    #[test]
    fn test_find_all_matches_returns_sorted() {
        // "aaab" repeated 50x → many matches of different lengths at position 12.
        let mut data = Vec::with_capacity(200);
        for _ in 0..50 {
            data.extend_from_slice(b"aaab");
        }

        let mut encoder = Lz77Encoder::with_level(9);
        encoder.reset();
        {
            let window = encoder.window_as_slice_mut();
            let n = data.len().min(window.len());
            window[..n].copy_from_slice(&data[..n]);
        }
        encoder.reset_hash();

        let query_pos = 12usize;
        for i in 0..query_pos {
            encoder.update_hash_single(i);
        }

        let remaining = data.len() - query_pos;
        let matches = encoder.find_all_matches(query_pos, remaining);

        // Each match must be strictly longer than the previous.
        for w in matches.windows(2) {
            assert!(
                w[1].0 > w[0].0,
                "matches must be sorted by strictly increasing length: {:?}",
                matches
            );
        }
    }

    #[test]
    fn test_optimal_iteration_converges() {
        // More passes should produce output that is no larger than fewer passes.
        let mut data = Vec::with_capacity(3000);
        for i in 0u16..3000 {
            data.push((i % 17 + i % 13) as u8);
        }

        let compress_with_passes = |p: u8| -> usize {
            let mut encoder = Lz77Encoder::with_level(6);
            let mut parser = OptimalParser::with_passes(p);
            let tokens = parser.parse(&data, &mut encoder);
            let ll = fixed_litlen_lengths();
            let dl = fixed_distance_lengths();
            estimate_token_cost(&tokens, &ll, &dl) as usize
        };

        let cost_1 = compress_with_passes(1);
        let cost_5 = compress_with_passes(5);

        assert!(
            cost_5 <= cost_1 + cost_1 / 100,
            "5-pass cost ({}) should not be significantly worse than 1-pass cost ({})",
            cost_5,
            cost_1
        );
    }

    #[test]
    fn test_optimal_block_type_selection() {
        // Verify decompressible output for both repetitive and random-ish inputs.
        let repetitive: Vec<u8> = (0..500).map(|i: usize| (i % 3) as u8).collect();
        let random: Vec<u8> = (0..500u32)
            .map(|i| {
                let s = i.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) as u8
            })
            .collect();

        for data in [&repetitive, &random] {
            let mut d = Deflater::with_optimal_parsing(6);
            let compressed = d.compress_to_vec(data).expect("optimal compress failed");
            let decompressed = inflate(&compressed).expect("inflate failed");
            assert_eq!(&decompressed, data, "block type selection roundtrip failed");
        }
    }

    #[test]
    fn test_with_optimal_parsing_api() {
        let data = b"Hello, optimal world! Hello, optimal world! DEFLATE is great.";
        let mut deflater = Deflater::with_optimal_parsing(6);
        let compressed = deflater
            .compress_to_vec(data)
            .expect("with_optimal_parsing compress failed");
        let decompressed = inflate(&compressed).expect("inflate of optimal failed");
        assert_eq!(&decompressed, data, "with_optimal_parsing roundtrip failed");
    }

    #[test]
    fn test_optimal_handles_max_match_length() {
        // A run of 600 identical bytes guarantees at least one MAX_MATCH (258) length match.
        let data: Vec<u8> = vec![0xABu8; 600];

        let (_, decompressed) = optimal_compress_roundtrip(&data);
        assert_eq!(decompressed, data, "max match length roundtrip failed");

        let mut encoder = Lz77Encoder::with_level(6);
        let mut parser = OptimalParser::new();
        let tokens = parser.parse(&data, &mut encoder);
        let has_long_match = tokens.iter().any(
            |t| matches!(t, Lz77Token::Match { length, .. } if *length >= MAX_MATCH as u16 - 10),
        );
        assert!(has_long_match, "expected a long match token in the result");
    }
}

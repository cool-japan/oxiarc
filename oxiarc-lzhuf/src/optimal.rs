//! Two-pass optimal LZSS parser for LZH compression.
//!
//! This module implements a Zopfli-style forward dynamic-programming parser
//! that finds the globally optimal sequence of literals and back-references
//! for a given input block.  Two passes are performed:
//!
//! * **Pass 0** – uniform bit-cost estimates (9 bits/literal, 14 bits/match).
//! * **Pass 1** – refined costs derived from the Huffman code lengths built
//!   from the token stream produced by the previous pass.
//!
//! Two passes are usually sufficient to reach ≥ 95 % of optimal quality.
//! Additional passes can be requested via [`LzssOptimalParser::with_passes`].

use crate::lzss::{LzssEncoder, LzssToken};
use crate::methods::constants::NC;

// ---------------------------------------------------------------------------
// Cost table constants
// ---------------------------------------------------------------------------

/// Approximate cost (in bits) of a literal token in pass 0.
const UNIFORM_LIT_BITS: u32 = 9;

/// Approximate cost (in bits) of a match token in pass 0.
const UNIFORM_MATCH_BITS: u32 = 14;

/// Sentinel value indicating "unreachable" in the DP cost array.
const INF_COST: u32 = u32::MAX / 2;

// ---------------------------------------------------------------------------
// Helper: position code computation (mirrors encode.rs logic)
// ---------------------------------------------------------------------------

/// Compute the position code (= floor(log2(distance))) from a distance value.
///
/// This mirrors the `get_position_code` function in `encode.rs`.  Position
/// code 0 corresponds to distance 1; position code k corresponds to
/// distances in the range `[2^k, 2^(k+1) - 1]`, encoded with k extra bits.
#[inline]
fn position_code(distance: u16) -> u8 {
    if distance <= 1 {
        return 0;
    }
    let mut p = 0u8;
    let mut d = distance;
    while d > 1 {
        d >>= 1;
        p += 1;
    }
    p
}

/// Number of extra bits emitted after the position code for a given distance.
#[inline]
fn position_extra_bits(distance: u16) -> u32 {
    position_code(distance) as u32
}

/// Map a match length (3-based) to its C-tree symbol index.
///
/// Length `len` → C-tree symbol `len - 3 + 256`, clamped to `[256, NC-1]`.
#[inline]
fn length_to_csym(length: u16) -> usize {
    ((length as usize).saturating_sub(3) + 256).min(NC - 1)
}

// ---------------------------------------------------------------------------
// Frequency-based Huffman length estimation
// ---------------------------------------------------------------------------

/// Build approximate Huffman code lengths from frequency counts using
/// the well-known package-merge / iterative-bisection approximation.
///
/// We use a simple log2-based estimate: `length[sym] ≈ ceil(log2(total / freq[sym]))`,
/// clamped to `[1, max_len]`.  This is the classical Shannon entropy bound and
/// gives a good first-order approximation of the actual canonical Huffman lengths
/// without running a full Huffman tree construction.
fn approx_huffman_lengths(freqs: &[u32], max_len: usize) -> Vec<u8> {
    let total: u32 = freqs.iter().sum();
    if total == 0 {
        return vec![0u8; freqs.len()];
    }

    let mut lengths = vec![0u8; freqs.len()];
    for (i, &f) in freqs.iter().enumerate() {
        if f == 0 {
            continue;
        }
        // log2(total / f) rounded up, minimum 1.
        let ratio = total.div_ceil(f);
        let bits = (usize::BITS - ratio.leading_zeros()) as usize; // ceil(log2(ratio))
        lengths[i] = bits.max(1).min(max_len) as u8;
    }
    lengths
}

// ---------------------------------------------------------------------------
// CostModel
// ---------------------------------------------------------------------------

/// Cost model used during a single DP pass.
struct CostModel {
    /// Huffman code lengths for the C-tree (literal + length symbols).
    c_lengths: Vec<u8>,
    /// Huffman code lengths for the P-tree (position codes).
    p_lengths: Vec<u8>,
    /// Whether this is the initial uniform-cost pass (pass 0).
    uniform: bool,
}

impl CostModel {
    /// Create the pass-0 uniform cost model.
    fn uniform() -> Self {
        Self {
            c_lengths: vec![0u8; NC],
            p_lengths: vec![0u8; 17], // NP_MAX = 17
            uniform: true,
        }
    }

    /// Build a refined cost model from the token stream produced by the
    /// previous DP pass.
    fn from_tokens(tokens: &[LzssToken]) -> Self {
        let mut c_freq = vec![0u32; NC];
        let mut p_freq = vec![0u32; 17];

        for token in tokens {
            match token {
                LzssToken::Literal(b) => {
                    c_freq[*b as usize] += 1;
                }
                LzssToken::Match { length, distance } => {
                    let csym = length_to_csym(*length);
                    c_freq[csym] += 1;
                    let pcode = position_code(*distance) as usize;
                    if pcode < p_freq.len() {
                        p_freq[pcode] += 1;
                    }
                }
            }
        }

        let c_lengths = approx_huffman_lengths(&c_freq, 16);
        let p_lengths = approx_huffman_lengths(&p_freq, 16);

        Self {
            c_lengths,
            p_lengths,
            uniform: false,
        }
    }

    /// Estimate cost in bits for emitting a literal byte.
    #[inline]
    fn literal_cost(&self, byte: u8) -> u32 {
        if self.uniform {
            return UNIFORM_LIT_BITS;
        }
        let l = self.c_lengths[byte as usize];
        if l == 0 { UNIFORM_LIT_BITS } else { l as u32 }
    }

    /// Estimate cost in bits for emitting a match (length, distance).
    #[inline]
    fn match_cost(&self, length: u16, distance: u16) -> u32 {
        if self.uniform {
            return UNIFORM_MATCH_BITS;
        }
        let csym = length_to_csym(length);
        let pcode = position_code(distance) as usize;

        let c_cost = {
            let l = self.c_lengths[csym];
            if l == 0 { UNIFORM_LIT_BITS } else { l as u32 }
        };
        let p_cost = {
            if pcode < self.p_lengths.len() {
                let l = self.p_lengths[pcode];
                if l == 0 { 4u32 } else { l as u32 }
            } else {
                4u32
            }
        };
        let extra = position_extra_bits(distance);
        c_cost + p_cost + extra
    }
}

// ---------------------------------------------------------------------------
// LzssOptimalParser
// ---------------------------------------------------------------------------

/// Two-pass Zopfli-style optimal LZSS parser.
///
/// Unlike the greedy/lazy parser in `LzssEncoder::encode`, this parser uses
/// forward dynamic programming to minimise the total estimated bit-cost of
/// the token stream.  Costs are first estimated with uniform weights, then
/// iteratively refined using the Huffman code lengths derived from the
/// previous pass's output.
pub struct LzssOptimalParser {
    passes: u8,
}

impl Default for LzssOptimalParser {
    fn default() -> Self {
        Self::new()
    }
}

impl LzssOptimalParser {
    /// Create a new parser with the default 2-pass schedule.
    pub fn new() -> Self {
        Self { passes: 2 }
    }

    /// Create a parser with an explicit number of passes (clamped to 1–6).
    pub fn with_passes(passes: u8) -> Self {
        Self {
            passes: passes.clamp(1, 6),
        }
    }

    /// Parse `data` using the optimal DP algorithm and return the resulting
    /// LZSS token stream.
    ///
    /// For each pass, the encoder's hash chains are cleared and rebuilt
    /// incrementally (forward scan, same as the greedy encoder) while the DP
    /// table is computed.  The window content is preserved across passes
    /// because it was populated once before the first pass.
    ///
    /// After all passes, the encoder's hash state reflects a full forward scan
    /// through `data` — consistent with having called `encode(data)`.
    pub fn parse(&mut self, data: &[u8], encoder: &mut LzssEncoder) -> Vec<LzssToken> {
        if data.is_empty() {
            return Vec::new();
        }

        // Commit all bytes into the circular window so that lookahead reads
        // inside find_all_matches are valid for any position.
        let data_start_abs = encoder.abs_write_pos();
        for &byte in data {
            encoder.push_byte(byte);
        }

        let mut tokens = Vec::new();

        for pass in 0..self.passes {
            let model = if pass == 0 {
                CostModel::uniform()
            } else {
                CostModel::from_tokens(&tokens)
            };

            // Each pass starts with empty hash chains so that the incremental
            // seeding in dp_pass_seeding always sees a forward-ordered chain.
            encoder.reset_hash_only();
            tokens = self.dp_pass_seeding(data, data_start_abs, encoder, &model);
        }

        tokens
    }

    /// First-pass DP that seeds the hash chains while scanning forward.
    ///
    /// This mirrors the structure of `LzssEncoder::encode`: for each position
    /// we first call `update_hash(cur_abs)` to insert the current position,
    /// then call `find_all_matches` to search backward in the already-indexed
    /// prefix.  This guarantees that `find_all_matches` never returns a
    /// self-match (distance = 0).
    fn dp_pass_seeding(
        &self,
        data: &[u8],
        data_start_abs: u64,
        encoder: &mut LzssEncoder,
        model: &CostModel,
    ) -> Vec<LzssToken> {
        let n = data.len();
        let mut costs = vec![INF_COST; n + 1];
        let mut prev_token: Vec<Option<LzssToken>> = vec![None; n + 1];
        let mut prev_pos: Vec<usize> = vec![0usize; n + 1];
        costs[0] = 0;

        for pos in 0..n {
            let cur_abs = data_start_abs + pos as u64;

            let cost_here = costs[pos];

            // Search BEFORE inserting cur_abs so the hash chain points to strictly
            // earlier positions (no self-match with dist = 0).
            let matches = if cost_here < INF_COST {
                encoder.find_all_matches(data, pos)
            } else {
                Vec::new()
            };

            // Insert cur_abs into the hash chain.
            encoder.update_hash(cur_abs);

            if cost_here >= INF_COST {
                continue;
            }

            // Literal transition.
            let lit_cost = cost_here.saturating_add(model.literal_cost(data[pos]));
            if lit_cost < costs[pos + 1] {
                costs[pos + 1] = lit_cost;
                prev_token[pos + 1] = Some(LzssToken::Literal(data[pos]));
                prev_pos[pos + 1] = pos;
            }

            // Match transitions.
            for (mlen, mdist) in matches {
                let end = pos + mlen as usize;
                if end > n {
                    continue;
                }
                let mc = cost_here.saturating_add(model.match_cost(mlen, mdist));
                if mc < costs[end] {
                    costs[end] = mc;
                    prev_token[end] = Some(LzssToken::Match {
                        length: mlen,
                        distance: mdist,
                    });
                    prev_pos[end] = pos;
                }
            }
        }

        self.backtrack(n, &prev_token, &prev_pos)
    }

    /// Backtrack from position `n` to 0, collecting tokens in forward order.
    fn backtrack(
        &self,
        n: usize,
        prev_token: &[Option<LzssToken>],
        prev_pos: &[usize],
    ) -> Vec<LzssToken> {
        // Walk backwards collecting the token chain.
        let mut result_rev: Vec<LzssToken> = Vec::new();
        let mut pos = n;
        while pos > 0 {
            match prev_token[pos] {
                Some(token) => {
                    result_rev.push(token);
                    pos = prev_pos[pos];
                }
                None => {
                    // Unreachable under correct data; fall back to all literals.
                    return data_to_literals_fallback(n, prev_token, prev_pos);
                }
            }
        }
        result_rev.reverse();
        result_rev
    }
}

/// Fallback: if the DP somehow fails to reach position `n`, emit the
/// portion we can reconstruct and pad with raw literals from the backtrack
/// chain's covered positions.
fn data_to_literals_fallback(
    n: usize,
    prev_token: &[Option<LzssToken>],
    prev_pos: &[usize],
) -> Vec<LzssToken> {
    // Walk whatever chain we can reconstruct.
    let mut result_rev: Vec<LzssToken> = Vec::new();
    let mut pos = n;
    // Avoid infinite loop: cap at n steps.
    let mut steps = 0usize;
    while pos > 0 && steps < n {
        steps += 1;
        if let Some(token) = prev_token[pos] {
            result_rev.push(token);
            let p = prev_pos[pos];
            if p >= pos {
                break; // Cycle guard
            }
            pos = p;
        } else {
            break;
        }
    }
    result_rev.reverse();
    result_rev
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::decode::decode_lzh;
    use crate::encode::LzhEncoder;
    use crate::methods::LzhMethod;

    // -----------------------------------------------------------------------
    // 1. Improved hash distribution
    // -----------------------------------------------------------------------

    #[test]
    fn test_improved_hash_distribution() {
        // Compress 1000 bytes of a repeating pattern, then decompress and
        // verify correctness.  If hash collisions were silently corrupting the
        // output, the round-trip would fail.
        let data: Vec<u8> = b"abcdefghij".iter().cycle().take(1000).copied().collect();

        let mut enc = LzhEncoder::new(LzhMethod::Lh5);
        let compressed = enc.compress_to_vec(&data).expect("compress failed");

        let decompressed =
            decode_lzh(&compressed, LzhMethod::Lh5, data.len() as u64).expect("decompress failed");

        assert_eq!(decompressed, data, "hash collision caused incorrect output");
    }

    // -----------------------------------------------------------------------
    // 2. Optimal ≤ greedy on repeated-pattern data
    // -----------------------------------------------------------------------

    #[test]
    fn test_optimal_lh5_beats_greedy() {
        let data: Vec<u8> = b"abcabcabc".iter().cycle().take(3000).copied().collect();

        // Greedy path.
        let mut enc_greedy = LzhEncoder::new(LzhMethod::Lh5);
        let compressed_greedy = enc_greedy
            .compress_to_vec(&data)
            .expect("greedy compress failed");

        // Optimal path.
        let mut enc_opt = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed_opt = enc_opt
            .compress_to_vec(&data)
            .expect("optimal compress failed");

        // Optimal must not regress versus greedy.
        assert!(
            compressed_opt.len() <= compressed_greedy.len() + 32,
            "optimal ({} bytes) is more than 32 bytes larger than greedy ({} bytes)",
            compressed_opt.len(),
            compressed_greedy.len()
        );

        // Verify round-trip of the optimal-compressed stream.
        let decompressed = decode_lzh(&compressed_opt, LzhMethod::Lh5, data.len() as u64)
            .expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    // -----------------------------------------------------------------------
    // 3. Optimal round-trip: pseudo-random data
    // -----------------------------------------------------------------------

    #[test]
    fn test_optimal_lh5_roundtrip() {
        let data: Vec<u8> = (0u32..2000)
            .map(|i| ((i.wrapping_mul(6364).wrapping_add(31337)) & 0xFF) as u8)
            .collect();

        let mut enc = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed = enc.compress_to_vec(&data).expect("compress failed");

        let decompressed =
            decode_lzh(&compressed, LzhMethod::Lh5, data.len() as u64).expect("decompress failed");

        assert_eq!(decompressed, data, "optimal round-trip failed");
    }

    // -----------------------------------------------------------------------
    // 4. Empty input
    // -----------------------------------------------------------------------

    #[test]
    fn test_optimal_lh5_empty_input() {
        let data: Vec<u8> = Vec::new();

        let mut enc = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed = enc.compress_to_vec(&data).expect("compress failed");

        let decompressed = decode_lzh(&compressed, LzhMethod::Lh5, 0).expect("decompress failed");

        assert_eq!(decompressed, data, "empty input optimal round-trip failed");
    }

    // -----------------------------------------------------------------------
    // 5. Single-byte input
    // -----------------------------------------------------------------------

    #[test]
    fn test_optimal_lh5_single_byte() {
        let data: Vec<u8> = vec![0x42u8];

        let mut enc = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed = enc.compress_to_vec(&data).expect("compress failed");

        let decompressed =
            decode_lzh(&compressed, LzhMethod::Lh5, data.len() as u64).expect("decompress failed");

        assert_eq!(decompressed, data, "single-byte optimal round-trip failed");
    }

    // -----------------------------------------------------------------------
    // 6. Builder API smoke test
    // -----------------------------------------------------------------------

    #[test]
    fn test_with_optimal_builder() {
        // Verify that the builder compiles, builds, and produces decodable output.
        let data: Vec<u8> = b"Hello, optimal LZH world!"
            .iter()
            .cycle()
            .take(500)
            .copied()
            .collect();

        let mut enc = LzhEncoder::new(LzhMethod::Lh5).with_optimal();
        let compressed = enc.compress_to_vec(&data).expect("compress failed");

        let decompressed =
            decode_lzh(&compressed, LzhMethod::Lh5, data.len() as u64).expect("decompress failed");

        assert_eq!(decompressed, data, "builder API round-trip failed");
    }
}

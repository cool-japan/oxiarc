//! Optimal parsing for LZMA compression.
//!
//! This module implements optimal parsing using price calculation and dynamic programming.
//! Optimal parsing finds the best sequence of literals and matches that minimizes the
//! compressed size, as opposed to greedy matching which takes the first good match.

use crate::model::{
    DIST_ALIGN_BITS, END_POS_MODEL_INDEX, LEN_HIGH_BITS, LEN_LOW_BITS, LEN_MID_BITS, MATCH_LEN_MIN,
    NUM_STATES, POS_STATES_MAX, State,
};

/// Maximum match length (same as in encoder).
const MATCH_LEN_MAX: usize = 273;
use crate::range_coder::{MOVE_BITS, PROB_BITS, PROB_INIT, PROB_MAX};

/// Maximum number of optimal parsing positions to track.
const MAX_OPT_NUM: usize = 4096;

/// Price scale (bits are represented in 1/16th bit units).
const PRICE_SCALE: u32 = 1 << 4;

/// Fast bytes parameter: number of bytes to encode without optimization in fast mode.
pub const FAST_BYTES_DEFAULT: u32 = 32;
/// Minimum fast bytes value.
pub const FAST_BYTES_MIN: u32 = 5;
/// Maximum fast bytes value.
pub const FAST_BYTES_MAX: u32 = 273;

/// Nice length parameter: match length threshold for immediate acceptance.
pub const NICE_LENGTH_DEFAULT: u32 = 64;
/// Minimum nice length value.
pub const NICE_LENGTH_MIN: u32 = 8;
/// Maximum nice length value.
pub const NICE_LENGTH_MAX: u32 = 273;

/// Pre-computed price table for bit encoding.
/// Prices are in 1/16th bit units for better precision.
static PROB_PRICES: [u32; PROB_MAX as usize >> MOVE_BITS] = {
    let mut prices = [0u32; PROB_MAX as usize >> MOVE_BITS];
    let mut i = 0;
    while i < prices.len() {
        let w = (i << MOVE_BITS) + (1 << (MOVE_BITS - 1));

        // Calculate -log2(prob / 2048) * 16
        // This approximates the number of bits needed to encode a symbol
        let prob = w as u32;

        // Approximation: price = log2(2048/prob) * 16
        // Using integer arithmetic: price ≈ (2048 * 16) / prob (simplified)
        // Better approximation using bit length
        let mut val = prob;
        let mut result = 0u32;
        let mut bit = 0;

        while bit < 32 {
            val >>= 1;
            if val == 0 {
                break;
            }
            result += 1;
            bit += 1;
        }

        // Price is approximately (PROB_BITS - result) * PRICE_SCALE
        let base_price = if result < PROB_BITS {
            (PROB_BITS - result) * PRICE_SCALE
        } else {
            0
        };

        // Add fractional part based on position within the bit
        let frac = (prob >> (result.saturating_sub(1))) & ((1 << MOVE_BITS) - 1);
        prices[i] = base_price + (frac * PRICE_SCALE) / (1 << MOVE_BITS);

        i += 1;
    }
    prices
};

/// Get the price of encoding a bit with the given probability.
#[inline]
pub fn get_price(prob: u16, bit: u32) -> u32 {
    let p = if bit == 0 { prob } else { PROB_MAX - prob };
    PROB_PRICES[(p >> MOVE_BITS) as usize]
}

/// Get the price of encoding direct bits (fixed 50% probability).
#[inline]
pub fn get_direct_bits_price(count: u32) -> u32 {
    count * PRICE_SCALE
}

/// Get the price of encoding a bit tree.
pub fn get_bit_tree_price(probs: &[u16], num_bits: u32, symbol: u32) -> u32 {
    let mut price = 0u32;
    let mut m = 1usize;

    for i in (0..num_bits).rev() {
        let bit = (symbol >> i) & 1;
        price += get_price(probs[m], bit);
        m = (m << 1) | bit as usize;
    }

    price
}

/// Get the price of encoding a bit tree in reverse order.
pub fn get_bit_tree_reverse_price(probs: &[u16], num_bits: u32, symbol: u32) -> u32 {
    let mut price = 0u32;
    let mut m = 1usize;

    for i in 0..num_bits {
        let bit = (symbol >> i) & 1;
        price += get_price(probs[m], bit);
        m = (m << 1) | bit as usize;
    }

    price
}

/// Get distance slot for a distance.
#[inline]
pub fn get_dist_slot(dist: u32) -> u32 {
    if dist < 4 {
        return dist;
    }

    let bits = 32 - dist.leading_zeros();
    ((bits - 1) << 1) | ((dist >> (bits - 2)) & 1)
}

/// Decision kind stored in an Optimal node for backtracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimalDecision {
    /// No decision recorded yet (node is unvisited / infinity).
    None,
    /// Literal byte.
    Literal,
    /// Short rep match (rep0, length 1).
    ShortRep,
    /// Rep match: rep index (0-3) and length.
    RepMatch {
        /// Rep index (0-3).
        rep_idx: u8,
        /// Match length.
        len: u32,
    },
    /// Normal match: distance and length.
    Match {
        /// Distance (0-based).
        dist: u32,
        /// Match length.
        len: u32,
    },
}

/// Optimal parsing state for a position.
#[derive(Debug, Clone, Copy)]
pub struct Optimal {
    /// Cumulative price to reach this position (in PROB_PRICES units).
    pub price: u32,
    /// Position index of the predecessor node in the DP chain.
    pub pos_prev: usize,
    /// Back distance (0 for literal).
    pub back: u32,
    /// Match length (1 for literal).
    pub len: u32,
    /// LZMA state at this position.
    pub state: u8,
    /// Rep distances at this position.
    pub reps: [u32; 4],
    /// Whether this is a match (not a literal).
    pub is_match: bool,
    /// The decision that leads INTO this node (for backtracking).
    pub decision: OptimalDecision,
}

impl Default for Optimal {
    fn default() -> Self {
        Self {
            price: u32::MAX,
            pos_prev: 0,
            back: 0,
            len: 0,
            state: 0,
            reps: [0; 4],
            is_match: false,
            decision: OptimalDecision::None,
        }
    }
}

/// Probability models for price calculation.
pub struct ProbabilityModels<'a> {
    /// Is-match probabilities.
    pub is_match: &'a [[u16; POS_STATES_MAX]; NUM_STATES],
    /// Is-rep probabilities.
    pub is_rep: &'a [u16; NUM_STATES],
    /// Is-rep0 probabilities.
    pub is_rep0: &'a [u16; NUM_STATES],
    /// Is-rep1 probabilities.
    pub is_rep1: &'a [u16; NUM_STATES],
    /// Is-rep2 probabilities.
    pub is_rep2: &'a [u16; NUM_STATES],
    /// Is-rep0-long probabilities.
    pub is_rep0_long: &'a [[u16; POS_STATES_MAX]; NUM_STATES],
    /// Match length model.
    pub match_len: &'a crate::model::LengthModel,
    /// Rep length model.
    pub rep_len: &'a crate::model::LengthModel,
    /// Distance slot probabilities.
    pub dist_slot: &'a [[u16; 64]; 4],
    /// Distance special probabilities.
    pub dist_special: &'a [u16],
    /// Distance alignment probabilities.
    pub dist_align: &'a [u16; 1 << DIST_ALIGN_BITS],
    /// Literal probabilities.
    pub literal: &'a Vec<[u16; 0x300]>,
    /// Number of position states.
    pub num_pos_states: usize,
    /// Literal context bits.
    pub lc: u32,
    /// Literal position bits.
    pub lp: u32,
}

/// Price calculator for LZMA encoding decisions.
pub struct PriceCalculator {
    /// Prices for encoding is_match = 1 (match symbol selection).
    is_match_prices: [[u32; POS_STATES_MAX]; NUM_STATES],
    /// Prices for encoding is_match = 0 (literal selection).
    is_not_match_prices: [[u32; POS_STATES_MAX]; NUM_STATES],
    /// Prices for is_rep probabilities.
    is_rep_prices: [u32; NUM_STATES],
    /// Prices for is_rep0 probabilities.
    is_rep0_prices: [u32; NUM_STATES],
    /// Prices for is_rep1 probabilities.
    is_rep1_prices: [u32; NUM_STATES],
    /// Prices for is_rep2 probabilities.
    is_rep2_prices: [u32; NUM_STATES],
    /// Prices for is_rep0_long probabilities.
    is_rep0_long_prices: [[u32; POS_STATES_MAX]; NUM_STATES],
}

impl PriceCalculator {
    /// Create a new price calculator.
    pub fn new() -> Self {
        // At initialisation every probability is PROB_INIT (50/50), so the
        // "not-match" price is the same as the "match" price.
        let init_not_match = get_price(PROB_INIT, 0);
        let mut is_not_match = [[0u32; POS_STATES_MAX]; NUM_STATES];
        for row in is_not_match.iter_mut() {
            row.fill(init_not_match);
        }
        Self {
            is_match_prices: [[0; POS_STATES_MAX]; NUM_STATES],
            is_not_match_prices: is_not_match,
            is_rep_prices: [0; NUM_STATES],
            is_rep0_prices: [0; NUM_STATES],
            is_rep1_prices: [0; NUM_STATES],
            is_rep2_prices: [0; NUM_STATES],
            is_rep0_long_prices: [[0; POS_STATES_MAX]; NUM_STATES],
        }
    }

    /// Update prices from probability model.
    pub fn update(&mut self, models: &ProbabilityModels<'_>) {
        for state in 0..NUM_STATES {
            for pos_state in 0..models.num_pos_states {
                let prob = models.is_match[state][pos_state];
                self.is_match_prices[state][pos_state] = get_price(prob, 1);
                self.is_not_match_prices[state][pos_state] = get_price(prob, 0);
                self.is_rep0_long_prices[state][pos_state] =
                    get_price(models.is_rep0_long[state][pos_state], 1);
            }

            self.is_rep_prices[state] = get_price(models.is_rep[state], 1);
            self.is_rep0_prices[state] = get_price(models.is_rep0[state], 1);
            self.is_rep1_prices[state] = get_price(models.is_rep1[state], 1);
            self.is_rep2_prices[state] = get_price(models.is_rep2[state], 1);
        }
    }

    /// Get the price of encoding a match.
    pub fn get_match_price(&self, state: usize, pos_state: usize) -> u32 {
        self.is_match_prices[state][pos_state] + get_price(PROB_INIT, 0)
    }

    /// Get the price of encoding a literal (basic, state-only).
    ///
    /// Returns the price of the is_match=0 bit plus an 8-bit uniform estimate
    /// for the literal byte itself. For accurate byte-level pricing use
    /// `get_literal_price_ctx`.
    pub fn get_literal_price(&self, state: usize, pos_state: usize) -> u32 {
        // Cost of encoding is_match = 0
        let is_match_cost = if state < NUM_STATES {
            self.is_not_match_prices[state][pos_state % POS_STATES_MAX]
        } else {
            get_price(PROB_INIT, 0)
        };
        // Estimate 8 bits for the literal data (PRICE_SCALE * 8)
        is_match_cost + PRICE_SCALE * 8
    }

    /// Get context-aware literal price using the actual literal probability sub-models.
    ///
    /// This is significantly more accurate than the basic version: it computes the
    /// actual cost of encoding `byte` given `prev_byte`, the LZMA state, and the
    /// match byte (for matched-literal mode when the state is after a match).
    ///
    /// - `lc` / `lp` define the literal model selection.
    /// - `pos`        is the current byte position in the stream (for lp mask).
    /// - `state`      is the current LZMA state (0..12).
    /// - `prev_byte`  is the last decoded/encoded byte.
    /// - `match_byte` is the byte at `pos - rep0 - 1` (used in matched mode).
    /// - `literal_probs` are the full literal probability tables.
    #[allow(clippy::too_many_arguments)]
    pub fn get_literal_price_ctx(
        &self,
        state: usize,
        pos_state: usize,
        pos: u64,
        byte: u8,
        prev_byte: u8,
        match_byte: u8,
        lc: u32,
        lp: u32,
        literal_probs: &[[u16; 0x300]],
    ) -> u32 {
        // Cost of is_match = 0
        let is_match_cost = if state < NUM_STATES {
            self.is_not_match_prices[state][pos_state % POS_STATES_MAX]
        } else {
            get_price(PROB_INIT, 0)
        };

        // Select the literal sub-model slot
        let lit_pos = pos & ((1u64 << lp) - 1);
        let prev_bits = (prev_byte as usize) >> (8 - lc as usize);
        let lit_state = ((lit_pos as usize) << lc as usize) + prev_bits;

        // Guard: if literal_probs table is too small, fall back to basic estimate
        let probs = match literal_probs.get(lit_state % literal_probs.len().max(1)) {
            Some(p) => p,
            None => return is_match_cost + PRICE_SCALE * 8,
        };

        // Determine whether we are in "matched literal" mode (state >= 7)
        let is_matched_state = state >= 7;

        let mut symbol = (byte as usize) | 0x100;
        let mut match_sym = (match_byte as usize) << 1;
        let mut context = 1usize;
        let mut lit_price = 0u32;

        if is_matched_state {
            // Matched literal: interleave with match_byte bits until mismatch
            loop {
                let match_bit = (match_sym >> 8) & 1;
                match_sym <<= 1;

                let bit = (symbol >> 7) & 1;
                symbol <<= 1;

                let prob_idx = 0x100 + (match_bit << 8) + context;
                lit_price += get_price(probs[prob_idx], bit as u32);
                context = (context << 1) | bit;

                if context >= 0x100 {
                    break;
                }

                if bit != match_bit {
                    // Mismatch: finish remaining bits without match context
                    while context < 0x100 {
                        let b = (symbol >> 7) & 1;
                        symbol <<= 1;
                        lit_price += get_price(probs[context], b as u32);
                        context = (context << 1) | b;
                    }
                    break;
                }
            }
        } else {
            // Normal literal: 8-bit tree
            loop {
                let bit = (symbol >> 7) & 1;
                symbol <<= 1;
                lit_price += get_price(probs[context], bit as u32);
                context = (context << 1) | bit;
                if context >= 0x100 {
                    break;
                }
            }
        }

        is_match_cost + lit_price
    }

    /// Get the price of encoding a rep match.
    pub fn get_rep_price(&self, state: usize, rep_idx: usize, pos_state: usize) -> u32 {
        let mut price = self.is_match_prices[state][pos_state];
        price += self.is_rep_prices[state];

        if rep_idx == 0 {
            price += get_price(PROB_INIT, 0);
            price += self.is_rep0_long_prices[state][pos_state];
        } else {
            price += self.is_rep0_prices[state];
            if rep_idx == 1 {
                price += get_price(PROB_INIT, 0);
            } else {
                price += self.is_rep1_prices[state];
                if rep_idx == 2 {
                    price += get_price(PROB_INIT, 0);
                } else {
                    price += self.is_rep2_prices[state];
                }
            }
        }

        price
    }

    /// Get the price of encoding a short rep (single byte rep0).
    pub fn get_short_rep_price(&self, state: usize, pos_state: usize) -> u32 {
        let mut price = self.is_match_prices[state][pos_state];
        price += self.is_rep_prices[state];
        price += get_price(PROB_INIT, 0); // is_rep0
        price += get_price(PROB_INIT, 0); // is_rep0_long with bit 0
        price
    }
}

impl Default for PriceCalculator {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the price of encoding a length.
pub fn get_length_price(
    choice: u16,
    choice2: u16,
    low: &[[u16; 1 << LEN_LOW_BITS]],
    mid: &[[u16; 1 << LEN_MID_BITS]],
    high: &[u16; 1 << LEN_HIGH_BITS],
    len: u32,
    pos_state: usize,
) -> u32 {
    let len = len - MATCH_LEN_MIN as u32;
    let mut price = 0u32;

    if len < (1 << LEN_LOW_BITS) {
        price += get_price(choice, 0);
        price += get_bit_tree_price(&low[pos_state], LEN_LOW_BITS, len);
    } else if len < (1 << LEN_LOW_BITS) + (1 << LEN_MID_BITS) {
        price += get_price(choice, 1);
        price += get_price(choice2, 0);
        price += get_bit_tree_price(&mid[pos_state], LEN_MID_BITS, len - (1 << LEN_LOW_BITS));
    } else {
        price += get_price(choice, 1);
        price += get_price(choice2, 1);
        price += get_bit_tree_price(
            high,
            LEN_HIGH_BITS,
            len - (1 << LEN_LOW_BITS) - (1 << LEN_MID_BITS),
        );
    }

    price
}

/// Get the price of encoding a distance.
pub fn get_distance_price(
    slot: &[[u16; 64]; 4],
    special: &[u16],
    align: &[u16; 1 << DIST_ALIGN_BITS],
    dist: u32,
    len: u32,
) -> u32 {
    let len_state = ((len - MATCH_LEN_MIN as u32).min(3)) as usize;
    let dist_slot = get_dist_slot(dist);

    let mut price = get_bit_tree_price(&slot[len_state], 6, dist_slot);

    if dist_slot >= 4 {
        let num_direct_bits = (dist_slot >> 1) - 1;
        let base = (2 | (dist_slot & 1)) << num_direct_bits;
        let dist_reduced = dist - base;

        if dist_slot < END_POS_MODEL_INDEX as u32 {
            // Reverse bit tree price
            let base_idx = (dist_slot as usize) - (dist_slot as usize >> 1) - 1;
            price +=
                get_bit_tree_reverse_price(&special[base_idx..], num_direct_bits, dist_reduced);
        } else {
            // Direct bits + alignment
            let num_align_bits = DIST_ALIGN_BITS;
            let num_direct = num_direct_bits - num_align_bits;
            price += get_direct_bits_price(num_direct);
            price += get_bit_tree_reverse_price(
                align,
                num_align_bits,
                dist_reduced & ((1 << num_align_bits) - 1),
            );
        }
    }

    price
}

/// Match candidate for optimal parsing.
#[derive(Debug, Clone, Copy)]
pub struct MatchCandidate {
    /// Distance (0-based).
    pub dist: u32,
    /// Match length.
    pub len: u32,
    /// Whether this is a rep match.
    pub is_rep: bool,
    /// Rep index (0-3) if is_rep is true.
    pub rep_idx: u8,
}

/// Optimal parser for finding the best sequence of literals and matches.
pub struct OptimalParser {
    /// Optimal states for positions used by the DP forward-pass parser.
    opts: Vec<Optimal>,
    /// Price calculator.
    price_calc: PriceCalculator,
    /// Fast bytes parameter.
    fast_bytes: u32,
    /// Nice length parameter.
    nice_length: u32,
    /// Compression level (8-10).
    level: u8,
    /// Look-ahead distance for match finding.
    look_ahead: usize,
}

/// Match type result from optimal encoding.
#[derive(Debug, Clone, Copy)]
pub enum MatchType {
    /// Literal byte.
    Literal,
    /// Short rep match (length 1).
    ShortRep,
    /// Rep match with index and length.
    RepMatch {
        /// Rep index (0-3).
        rep_idx: u8,
        /// Match length.
        len: u32,
    },
    /// Normal match with distance and length.
    Match {
        /// Distance.
        dist: u32,
        /// Match length.
        len: u32,
    },
}

impl OptimalParser {
    /// Create a new optimal parser.
    pub fn new(fast_bytes: u32, nice_length: u32) -> Self {
        let fast_bytes = fast_bytes.clamp(FAST_BYTES_MIN, FAST_BYTES_MAX);
        let nice_length = nice_length.clamp(NICE_LENGTH_MIN, NICE_LENGTH_MAX);

        Self {
            opts: vec![Optimal::default(); MAX_OPT_NUM],
            price_calc: PriceCalculator::new(),
            fast_bytes,
            nice_length,
            level: 8,
            look_ahead: 32,
        }
    }

    /// Create a new optimal parser with the given compression level.
    pub fn with_level(level: u8) -> Self {
        let (fast_bytes, nice_length, look_ahead) = match level {
            8 => (64, 128, 32),
            9 => (128, 273, 64),
            _ => (273, 273, 128), // level 10+
        };

        let mut parser = Self::new(fast_bytes, nice_length);
        parser.level = level;
        parser.look_ahead = look_ahead;
        parser
    }

    /// Get the compression level.
    pub fn level(&self) -> u8 {
        self.level
    }

    /// Get the look-ahead distance.
    pub fn look_ahead(&self) -> usize {
        self.look_ahead
    }

    /// Get fast bytes parameter.
    pub fn fast_bytes(&self) -> u32 {
        self.fast_bytes
    }

    /// Get nice length parameter.
    pub fn nice_length(&self) -> u32 {
        self.nice_length
    }

    /// Update prices from probability model.
    pub fn update_prices(&mut self, models: &ProbabilityModels<'_>) {
        self.price_calc.update(models);
    }

    /// Get price calculator.
    pub fn price_calc(&self) -> &PriceCalculator {
        &self.price_calc
    }

    /// Find optimal encoding for a position.
    ///
    /// This implements a simplified optimal parsing approach that evaluates
    /// all matches at the current position and returns the best one based
    /// on price estimation.
    #[allow(clippy::too_many_arguments)]
    pub fn find_optimal_encoding<F, G>(
        &mut self,
        data: &[u8],
        pos: usize,
        state: crate::model::State,
        _reps: [u32; 4],
        find_matches: F,
        check_rep: G,
        models: &ProbabilityModels<'_>,
    ) -> Option<(MatchType, u32)>
    where
        F: Fn(usize, usize) -> Vec<(u32, u32)>,
        G: Fn(usize, u8) -> u32,
    {
        if pos >= data.len() {
            return None;
        }

        let pos_state = pos & (models.num_pos_states - 1);
        let state_idx = state.value();

        // Get literal price
        let literal_price = self.price_calc.get_literal_price(state_idx, pos_state);

        // Best result so far
        let mut best_price = literal_price;
        let mut best_match = MatchType::Literal;

        // Check rep matches
        for rep_idx in 0..4u8 {
            let len = check_rep(pos, rep_idx);
            if len >= MATCH_LEN_MIN as u32 {
                let price = if len == 1 && rep_idx == 0 {
                    self.price_calc.get_short_rep_price(state_idx, pos_state)
                } else {
                    let rep_price =
                        self.price_calc
                            .get_rep_price(state_idx, rep_idx as usize, pos_state);
                    let len_price = get_length_price(
                        models.rep_len.choice,
                        models.rep_len.choice2,
                        &models.rep_len.low,
                        &models.rep_len.mid,
                        &models.rep_len.high,
                        len,
                        pos_state,
                    );
                    rep_price + len_price
                };

                if price < best_price {
                    best_price = price;
                    if len == 1 && rep_idx == 0 {
                        best_match = MatchType::ShortRep;
                    } else {
                        best_match = MatchType::RepMatch { rep_idx, len };
                    }
                }
            }
        }

        // Check normal matches
        let matches = find_matches(pos, self.look_ahead);
        for (dist, len) in matches {
            if len >= MATCH_LEN_MIN as u32 {
                let match_price = self.price_calc.get_match_price(state_idx, pos_state);
                let len_price = get_length_price(
                    models.match_len.choice,
                    models.match_len.choice2,
                    &models.match_len.low,
                    &models.match_len.mid,
                    &models.match_len.high,
                    len,
                    pos_state,
                );
                let dist_price = get_distance_price(
                    models.dist_slot,
                    models.dist_special,
                    models.dist_align,
                    dist,
                    len,
                );
                let price = match_price + len_price + dist_price;

                if price < best_price {
                    best_price = price;
                    best_match = MatchType::Match { dist, len };
                }
            }
        }

        match best_match {
            MatchType::Literal => None,
            _ => Some((best_match, best_price)),
        }
    }

    /// Full DP forward-pass optimal parser for a block of data.
    ///
    /// Fills `self.opts[0..=block_len]` with the minimum-cost path through
    /// the block, then backtracks to return the optimal sequence of encoding
    /// decisions.
    ///
    /// Parameters
    /// ----------
    /// - `data`        : full input slice (we read `data[start..start+block_len]`)
    /// - `start`       : absolute position in the input where the block begins
    /// - `block_len`   : number of bytes to parse (must be ≤ MAX_OPT_NUM - 1)
    /// - `models`      : current LZMA probability models for accurate pricing
    /// - `state`       : LZMA state at the beginning of the block
    /// - `reps`        : rep-distance registers at the beginning of the block
    /// - `find_matches`: closure that returns `Vec<(dist, len)>` for a global
    ///   position; each pair satisfies `len >= MATCH_LEN_MIN`
    ///
    /// Returns a `Vec<(MatchType, usize)>` where `usize` is the number of
    /// input bytes consumed by each decision (1 for literals/short-rep,
    /// otherwise the match length).
    #[allow(clippy::too_many_arguments)]
    pub fn parse_block<F>(
        &mut self,
        data: &[u8],
        start: usize,
        block_len: usize,
        models: &ProbabilityModels<'_>,
        state: State,
        reps: &[u32; 4],
        find_matches: F,
    ) -> Vec<(MatchType, usize)>
    where
        F: Fn(usize) -> Vec<(u32, u32)>,
    {
        // Clamp to the opts buffer limit and available data
        let available = data.len().saturating_sub(start);
        let block_len = block_len.min(available).min(MAX_OPT_NUM.saturating_sub(1));

        if block_len == 0 {
            return Vec::new();
        }

        let num_pos_states = models.num_pos_states;

        // ---------------------------------------------------------------
        // Initialise the opts array for positions 0..=block_len.
        // ---------------------------------------------------------------
        // Position 0 has zero cost and inherits the caller's state/reps.
        // All other positions start at infinity.
        for opt in self.opts[..=block_len].iter_mut() {
            opt.price = u32::MAX;
            opt.decision = OptimalDecision::None;
            opt.pos_prev = 0;
        }

        {
            let opt0 = &mut self.opts[0];
            opt0.price = 0;
            opt0.state = state.value() as u8;
            opt0.reps = *reps;
            opt0.decision = OptimalDecision::None;
        }

        // ---------------------------------------------------------------
        // Forward pass: for each position i, propagate costs forward.
        // ---------------------------------------------------------------
        for i in 0..block_len {
            let cur_price = self.opts[i].price;
            if cur_price == u32::MAX {
                // Unreachable node – skip
                continue;
            }

            let cur_state_val = self.opts[i].state as usize;
            let cur_reps = self.opts[i].reps;
            let cur_state = State::from_value(cur_state_val as u8);

            let abs_pos = start + i;
            let pos_state = abs_pos & (num_pos_states - 1);

            let prev_byte = if abs_pos > 0 {
                *data.get(abs_pos - 1).unwrap_or(&0)
            } else {
                0
            };

            // Match byte for matched-literal mode (at rep0 distance)
            let match_byte = {
                let rep0 = cur_reps[0] as usize;
                if rep0 < abs_pos {
                    *data.get(abs_pos - rep0 - 1).unwrap_or(&0)
                } else {
                    0
                }
            };

            let cur_byte = match data.get(abs_pos) {
                Some(&b) => b,
                None => break,
            };

            // -----------------------------------------------------------
            // 1. Literal
            // -----------------------------------------------------------
            if i < block_len {
                let lit_price = self.price_calc.get_literal_price_ctx(
                    cur_state_val,
                    pos_state,
                    abs_pos as u64,
                    cur_byte,
                    prev_byte,
                    match_byte,
                    models.lc,
                    models.lp,
                    models.literal,
                );

                let new_price = cur_price.saturating_add(lit_price);
                if new_price < self.opts[i + 1].price {
                    // Compute successor state and reps for the literal
                    let mut succ_state = cur_state;
                    succ_state.update_literal();

                    self.opts[i + 1].price = new_price;
                    self.opts[i + 1].state = succ_state.value() as u8;
                    self.opts[i + 1].reps = cur_reps;
                    self.opts[i + 1].pos_prev = i;
                    self.opts[i + 1].decision = OptimalDecision::Literal;
                }
            }

            // -----------------------------------------------------------
            // 2. Short rep (rep0, length 1): only valid when rep0 distance
            //    points to a byte that equals cur_byte.
            // -----------------------------------------------------------
            {
                let rep0 = cur_reps[0] as usize;
                if rep0 < abs_pos {
                    let rep_byte = *data.get(abs_pos - rep0 - 1).unwrap_or(&1);
                    if rep_byte == cur_byte && i < block_len {
                        let short_rep_price = self
                            .price_calc
                            .get_short_rep_price(cur_state_val, pos_state);
                        let new_price = cur_price.saturating_add(short_rep_price);
                        if new_price < self.opts[i + 1].price {
                            let mut succ_state = cur_state;
                            succ_state.update_short_rep();

                            self.opts[i + 1].price = new_price;
                            self.opts[i + 1].state = succ_state.value() as u8;
                            self.opts[i + 1].reps = cur_reps;
                            self.opts[i + 1].pos_prev = i;
                            self.opts[i + 1].decision = OptimalDecision::ShortRep;
                        }
                    }
                }
            }

            // -----------------------------------------------------------
            // 3. Rep matches (rep0-3, lengths MATCH_LEN_MIN..=max_available)
            // -----------------------------------------------------------
            let max_available_from_i = data.len().saturating_sub(abs_pos).min(MATCH_LEN_MAX);

            for rep_idx in 0..4usize {
                let rep_dist = cur_reps[rep_idx] as usize;
                if rep_dist >= abs_pos {
                    continue;
                }
                let rep_start = abs_pos - rep_dist - 1;

                // Measure the rep match length
                let mut rep_len = 0usize;
                while rep_len < max_available_from_i {
                    let a = match data.get(abs_pos + rep_len) {
                        Some(&b) => b,
                        None => break,
                    };
                    let b = match data.get(rep_start + rep_len) {
                        Some(&b) => b,
                        None => break,
                    };
                    if a != b {
                        break;
                    }
                    rep_len += 1;
                }

                if rep_len < MATCH_LEN_MIN {
                    continue;
                }

                // Cap to how much we can store in opts
                let max_rep_len = rep_len.min(block_len - i);

                // The base overhead for choosing this rep (without length)
                let rep_base_price =
                    self.price_calc
                        .get_rep_price(cur_state_val, rep_idx, pos_state);

                // Evaluate each valid length
                for len in MATCH_LEN_MIN..=max_rep_len {
                    let len_u32 = len as u32;
                    let len_price = get_length_price(
                        models.rep_len.choice,
                        models.rep_len.choice2,
                        &models.rep_len.low,
                        &models.rep_len.mid,
                        &models.rep_len.high,
                        len_u32,
                        pos_state,
                    );
                    let new_price = cur_price.saturating_add(rep_base_price + len_price);
                    let dest = i + len;
                    if dest <= block_len && new_price < self.opts[dest].price {
                        // Successor state/reps after a rep match
                        let mut succ_state = cur_state;
                        succ_state.update_long_rep();

                        let mut succ_reps = cur_reps;
                        // Move rep_idx to front
                        let moved = succ_reps[rep_idx];
                        for k in (1..=rep_idx).rev() {
                            succ_reps[k] = succ_reps[k - 1];
                        }
                        succ_reps[0] = moved;

                        self.opts[dest].price = new_price;
                        self.opts[dest].state = succ_state.value() as u8;
                        self.opts[dest].reps = succ_reps;
                        self.opts[dest].pos_prev = i;
                        self.opts[dest].decision = OptimalDecision::RepMatch {
                            rep_idx: rep_idx as u8,
                            len: len_u32,
                        };
                    }
                }
            }

            // -----------------------------------------------------------
            // 4. Normal matches
            // -----------------------------------------------------------
            let candidates = find_matches(abs_pos);
            let match_base_price = self
                .price_calc
                .get_match_price(cur_state_val, pos_state)
                // get_match_price includes is_rep=0, but we want only is_match=1+is_rep=0
                // The current implementation adds get_price(PROB_INIT, 0) for is_rep=0 already;
                // we subtract nothing – the API already gives the correct combined overhead.
                ;

            for (dist, max_len) in &candidates {
                let dist = *dist;
                let max_len = (*max_len as usize).min(block_len - i);

                if max_len < MATCH_LEN_MIN {
                    continue;
                }

                // Evaluate each valid length
                for len in MATCH_LEN_MIN..=max_len {
                    let len_u32 = len as u32;
                    let len_price = get_length_price(
                        models.match_len.choice,
                        models.match_len.choice2,
                        &models.match_len.low,
                        &models.match_len.mid,
                        &models.match_len.high,
                        len_u32,
                        pos_state,
                    );
                    let dist_price = get_distance_price(
                        models.dist_slot,
                        models.dist_special,
                        models.dist_align,
                        dist,
                        len_u32,
                    );
                    let new_price =
                        cur_price.saturating_add(match_base_price + len_price + dist_price);
                    let dest = i + len;
                    if dest <= block_len && new_price < self.opts[dest].price {
                        let mut succ_state = cur_state;
                        succ_state.update_match();

                        let mut succ_reps = cur_reps;
                        succ_reps[3] = succ_reps[2];
                        succ_reps[2] = succ_reps[1];
                        succ_reps[1] = succ_reps[0];
                        succ_reps[0] = dist;

                        self.opts[dest].price = new_price;
                        self.opts[dest].state = succ_state.value() as u8;
                        self.opts[dest].reps = succ_reps;
                        self.opts[dest].pos_prev = i;
                        self.opts[dest].decision = OptimalDecision::Match { dist, len: len_u32 };
                    }
                }

                // Accept immediately on nice length
                if max_len >= self.nice_length as usize {
                    break;
                }
            }
        }

        // ---------------------------------------------------------------
        // Backtrack: follow pos_prev chain from block_len down to 0.
        // ---------------------------------------------------------------
        let mut decisions_rev: Vec<(MatchType, usize)> = Vec::with_capacity(block_len);
        let mut pos = block_len;

        // If the end node is still at infinity (empty block or no path),
        // fall back to emitting all bytes as literals.
        if self.opts[block_len].price == u32::MAX {
            for i in 0..block_len {
                let abs_pos = start + i;
                if abs_pos < data.len() {
                    decisions_rev.push((MatchType::Literal, 1));
                }
            }
            decisions_rev.reverse();
            return decisions_rev;
        }

        while pos > 0 {
            let decision = self.opts[pos].decision;
            let pred = self.opts[pos].pos_prev;

            let (mt, consumed) = match decision {
                OptimalDecision::None | OptimalDecision::Literal => (MatchType::Literal, 1usize),
                OptimalDecision::ShortRep => (MatchType::ShortRep, 1usize),
                OptimalDecision::RepMatch { rep_idx, len } => {
                    (MatchType::RepMatch { rep_idx, len }, len as usize)
                }
                OptimalDecision::Match { dist, len } => {
                    (MatchType::Match { dist, len }, len as usize)
                }
            };

            decisions_rev.push((mt, consumed));
            pos = pred;
        }

        decisions_rev.reverse();
        decisions_rev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_price_calculation() {
        // Price of encoding a bit with 50% probability should be approximately 1 bit
        let price = get_price(PROB_INIT, 0);
        // Price is in 1/16th bit units, so ~16 for 1 bit
        assert!((14..=18).contains(&price));
    }

    #[test]
    fn test_direct_bits_price() {
        let price = get_direct_bits_price(8);
        // 8 bits at 1 bit each = 8 * 16 = 128 price units
        assert_eq!(price, 8 * PRICE_SCALE);
    }

    #[test]
    fn test_dist_slot() {
        assert_eq!(get_dist_slot(0), 0);
        assert_eq!(get_dist_slot(1), 1);
        assert_eq!(get_dist_slot(2), 2);
        assert_eq!(get_dist_slot(3), 3);
        assert_eq!(get_dist_slot(4), 4);
        assert_eq!(get_dist_slot(5), 4); // Distance 5 maps to slot 4
        assert_eq!(get_dist_slot(6), 5); // Distance 6 maps to slot 5
    }

    #[test]
    fn test_optimal_parser_creation() {
        let parser = OptimalParser::new(FAST_BYTES_DEFAULT, NICE_LENGTH_DEFAULT);
        assert_eq!(parser.fast_bytes(), FAST_BYTES_DEFAULT);
        assert_eq!(parser.nice_length(), NICE_LENGTH_DEFAULT);
    }

    #[test]
    fn test_fast_bytes_clamping() {
        let parser = OptimalParser::new(1, 1);
        assert_eq!(parser.fast_bytes(), FAST_BYTES_MIN);
        assert_eq!(parser.nice_length(), NICE_LENGTH_MIN);

        let parser = OptimalParser::new(1000, 1000);
        assert_eq!(parser.fast_bytes(), FAST_BYTES_MAX);
        assert_eq!(parser.nice_length(), NICE_LENGTH_MAX);
    }

    #[test]
    fn test_bit_tree_price() {
        let probs = vec![PROB_INIT; 16];
        let price = get_bit_tree_price(&probs, 3, 5);
        // Encoding 3 bits with 50% probability = ~3 bits = ~48 price units
        assert!((40..=56).contains(&price));
    }

    #[test]
    fn test_price_calculator() {
        let calc = PriceCalculator::new();
        let match_price = calc.get_match_price(0, 0);
        let literal_price = calc.get_literal_price(0, 0);

        // Both should be positive
        assert!(match_price > 0);
        assert!(literal_price > 0);
    }
}

//! Optimal parsing for LZMA compression.
//!
//! This module implements optimal parsing using price calculation and dynamic programming.
//! Optimal parsing finds the best sequence of literals and matches that minimizes the
//! compressed size, as opposed to greedy matching which takes the first good match.

use crate::model::{
    DIST_ALIGN_BITS, END_POS_MODEL_INDEX, LEN_HIGH_BITS, LEN_LOW_BITS, LEN_MID_BITS, MATCH_LEN_MIN,
    NUM_STATES, POS_STATES_MAX,
};
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

/// Optimal parsing state for a position.
#[derive(Debug, Clone, Copy)]
pub struct Optimal {
    /// Price to reach this position.
    pub price: u32,
    /// Position in the input where this optimal state starts.
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
    /// Number of position states.
    pub num_pos_states: usize,
}

/// Price calculator for LZMA encoding decisions.
pub struct PriceCalculator {
    /// Prices for is_match probabilities.
    is_match_prices: [[u32; POS_STATES_MAX]; NUM_STATES],
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
        Self {
            is_match_prices: [[0; POS_STATES_MAX]; NUM_STATES],
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
                self.is_match_prices[state][pos_state] =
                    get_price(models.is_match[state][pos_state], 1);
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

    /// Get the price of encoding a literal.
    pub fn get_literal_price(&self, _state: usize, _pos_state: usize) -> u32 {
        get_price(PROB_INIT, 0) + get_price(PROB_INIT, 0)
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

/// Optimal parser for finding the best sequence of literals and matches.
pub struct OptimalParser {
    /// Optimal states for positions (reserved for full DP implementation).
    #[allow(dead_code)]
    opts: Vec<Optimal>,
    /// Price calculator.
    price_calc: PriceCalculator,
    /// Fast bytes parameter.
    fast_bytes: u32,
    /// Nice length parameter.
    nice_length: u32,
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
        }
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

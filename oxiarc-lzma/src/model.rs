//! LZMA probability models.
//!
//! LZMA uses context-dependent probability models for:
//! - Literal encoding (context = previous byte + position)
//! - Match length encoding
//! - Distance encoding
//! - State machine transitions

use crate::range_coder::PROB_INIT;

/// Number of position bits for literal coding (default: 0).
pub const LC_DEFAULT: u32 = 3;

/// Number of literal position bits (default: 0).
pub const LP_DEFAULT: u32 = 0;

/// Number of position bits (default: 2).
pub const PB_DEFAULT: u32 = 2;

/// Maximum number of position states.
pub const POS_STATES_MAX: usize = 1 << 4;

/// Number of states in the LZMA state machine.
pub const NUM_STATES: usize = 12;

/// Number of bits for low length coding.
pub const LEN_LOW_BITS: u32 = 3;
/// Number of bits for mid length coding.
pub const LEN_MID_BITS: u32 = 3;
/// Number of bits for high length coding.
pub const LEN_HIGH_BITS: u32 = 8;

/// Number of low length symbols.
pub const LEN_LOW_SYMBOLS: usize = 1 << LEN_LOW_BITS;
/// Number of mid length symbols.
pub const LEN_MID_SYMBOLS: usize = 1 << LEN_MID_BITS;
/// Number of high length symbols.
pub const LEN_HIGH_SYMBOLS: usize = 1 << LEN_HIGH_BITS;

/// Minimum match length.
pub const MATCH_LEN_MIN: usize = 2;

/// Number of distance slots.
pub const DIST_SLOTS: usize = 64;

/// Number of alignment bits for distance encoding.
pub const DIST_ALIGN_BITS: u32 = 4;
/// Size of alignment table.
pub const DIST_ALIGN_SIZE: usize = 1 << DIST_ALIGN_BITS;

/// Number of full distance symbols.
pub const FULL_DISTANCES: usize = 128;

/// End position model index.
pub const END_POS_MODEL_INDEX: usize = 14;

/// LZMA state machine state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State(u8);

impl State {
    /// Initial state.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Get state value.
    pub fn value(self) -> usize {
        self.0 as usize
    }

    /// Check if state represents a literal.
    pub fn is_literal(self) -> bool {
        self.0 < 7
    }

    /// Update state after literal.
    pub fn update_literal(&mut self) {
        self.0 = match self.0 {
            0..=3 => 0,
            4..=9 => self.0 - 3,
            10 => 6,
            _ => 5,
        };
    }

    /// Update state after match.
    pub fn update_match(&mut self) {
        self.0 = if self.0 < 7 { 7 } else { 10 };
    }

    /// Update state after short rep.
    pub fn update_short_rep(&mut self) {
        self.0 = if self.0 < 7 { 9 } else { 11 };
    }

    /// Update state after long rep.
    pub fn update_long_rep(&mut self) {
        self.0 = if self.0 < 7 { 8 } else { 11 };
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// LZMA properties (lc, lp, pb).
#[derive(Debug, Clone, Copy)]
pub struct LzmaProperties {
    /// Literal context bits.
    pub lc: u32,
    /// Literal position bits.
    pub lp: u32,
    /// Position bits.
    pub pb: u32,
}

impl LzmaProperties {
    /// Create new properties.
    pub fn new(lc: u32, lp: u32, pb: u32) -> Self {
        Self { lc, lp, pb }
    }

    /// Parse from property byte.
    pub fn from_byte(byte: u8) -> Option<Self> {
        let pb = byte as u32 / 45;
        let remaining = byte as u32 - pb * 45;
        let lp = remaining / 9;
        let lc = remaining - lp * 9;

        if lc > 8 || lp > 4 || pb > 4 {
            return None;
        }

        Some(Self { lc, lp, pb })
    }

    /// Encode to property byte.
    pub fn to_byte(&self) -> u8 {
        ((self.pb * 45) + (self.lp * 9) + self.lc) as u8
    }

    /// Get number of literal states.
    pub fn num_lit_states(&self) -> usize {
        1 << (self.lc + self.lp)
    }

    /// Get number of position states.
    pub fn num_pos_states(&self) -> usize {
        1 << self.pb
    }
}

impl Default for LzmaProperties {
    fn default() -> Self {
        Self {
            lc: LC_DEFAULT,
            lp: LP_DEFAULT,
            pb: PB_DEFAULT,
        }
    }
}

/// Length decoder/encoder model.
#[derive(Debug, Clone)]
pub struct LengthModel {
    /// Choice bit (low vs mid+high).
    pub choice: u16,
    /// Choice2 bit (mid vs high).
    pub choice2: u16,
    /// Low length probabilities (per position state).
    pub low: Vec<[u16; LEN_LOW_SYMBOLS]>,
    /// Mid length probabilities (per position state).
    pub mid: Vec<[u16; LEN_MID_SYMBOLS]>,
    /// High length probabilities (shared).
    pub high: [u16; LEN_HIGH_SYMBOLS],
}

impl LengthModel {
    /// Create a new length model.
    pub fn new(num_pos_states: usize) -> Self {
        Self {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: vec![[PROB_INIT; LEN_LOW_SYMBOLS]; num_pos_states],
            mid: vec![[PROB_INIT; LEN_MID_SYMBOLS]; num_pos_states],
            high: [PROB_INIT; LEN_HIGH_SYMBOLS],
        }
    }

    /// Reset the model.
    pub fn reset(&mut self) {
        self.choice = PROB_INIT;
        self.choice2 = PROB_INIT;
        for arr in &mut self.low {
            arr.fill(PROB_INIT);
        }
        for arr in &mut self.mid {
            arr.fill(PROB_INIT);
        }
        self.high.fill(PROB_INIT);
    }
}

/// Literal decoder/encoder model.
#[derive(Debug, Clone)]
pub struct LiteralModel {
    /// Probability table for each literal state.
    /// Each state has 256 entries for decoding a byte.
    pub probs: Vec<[u16; 0x300]>,
}

impl LiteralModel {
    /// Create a new literal model.
    pub fn new(num_lit_states: usize) -> Self {
        Self {
            probs: vec![[PROB_INIT; 0x300]; num_lit_states],
        }
    }

    /// Reset the model.
    pub fn reset(&mut self) {
        for state in &mut self.probs {
            state.fill(PROB_INIT);
        }
    }

    /// Get the literal state index.
    pub fn get_state(&self, pos: u64, prev_byte: u8, lc: u32, lp: u32) -> usize {
        let lit_pos = pos & ((1 << lp) - 1);
        let prev_bits = (prev_byte as usize) >> (8 - lc as usize);
        ((lit_pos as usize) << lc as usize) + prev_bits
    }
}

/// Distance slot model.
#[derive(Debug, Clone)]
pub struct DistanceModel {
    /// Distance slot probabilities (per length state).
    pub slot: [[u16; DIST_SLOTS]; 4],
    /// Special position probabilities (flat array for slots 4-13).
    pub special: [u16; FULL_DISTANCES - END_POS_MODEL_INDEX],
    /// Alignment probabilities.
    pub align: [u16; DIST_ALIGN_SIZE],
}

impl DistanceModel {
    /// Create a new distance model.
    pub fn new() -> Self {
        Self {
            slot: [[PROB_INIT; DIST_SLOTS]; 4],
            special: [PROB_INIT; FULL_DISTANCES - END_POS_MODEL_INDEX],
            align: [PROB_INIT; DIST_ALIGN_SIZE],
        }
    }

    /// Reset the model.
    pub fn reset(&mut self) {
        for s in &mut self.slot {
            s.fill(PROB_INIT);
        }
        self.special.fill(PROB_INIT);
        self.align.fill(PROB_INIT);
    }
}

impl Default for DistanceModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Complete LZMA model containing all probability tables.
#[derive(Debug, Clone)]
pub struct LzmaModel {
    /// LZMA properties.
    pub props: LzmaProperties,

    /// Is-match probabilities.
    pub is_match: [[u16; POS_STATES_MAX]; NUM_STATES],
    /// Is-rep probabilities.
    pub is_rep: [u16; NUM_STATES],
    /// Is-rep0 probabilities.
    pub is_rep0: [u16; NUM_STATES],
    /// Is-rep1 probabilities.
    pub is_rep1: [u16; NUM_STATES],
    /// Is-rep2 probabilities.
    pub is_rep2: [u16; NUM_STATES],
    /// Is-rep0-long probabilities.
    pub is_rep0_long: [[u16; POS_STATES_MAX]; NUM_STATES],

    /// Match length model.
    pub match_len: LengthModel,
    /// Rep match length model.
    pub rep_len: LengthModel,

    /// Literal model.
    pub literal: LiteralModel,

    /// Distance model.
    pub distance: DistanceModel,
}

impl LzmaModel {
    /// Create a new LZMA model with the given properties.
    pub fn new(props: LzmaProperties) -> Self {
        let num_pos_states = props.num_pos_states();
        let num_lit_states = props.num_lit_states();

        Self {
            props,
            is_match: [[PROB_INIT; POS_STATES_MAX]; NUM_STATES],
            is_rep: [PROB_INIT; NUM_STATES],
            is_rep0: [PROB_INIT; NUM_STATES],
            is_rep1: [PROB_INIT; NUM_STATES],
            is_rep2: [PROB_INIT; NUM_STATES],
            is_rep0_long: [[PROB_INIT; POS_STATES_MAX]; NUM_STATES],
            match_len: LengthModel::new(num_pos_states),
            rep_len: LengthModel::new(num_pos_states),
            literal: LiteralModel::new(num_lit_states),
            distance: DistanceModel::new(),
        }
    }

    /// Reset all probabilities to initial values.
    pub fn reset(&mut self) {
        for state in &mut self.is_match {
            state.fill(PROB_INIT);
        }
        self.is_rep.fill(PROB_INIT);
        self.is_rep0.fill(PROB_INIT);
        self.is_rep1.fill(PROB_INIT);
        self.is_rep2.fill(PROB_INIT);
        for state in &mut self.is_rep0_long {
            state.fill(PROB_INIT);
        }
        self.match_len.reset();
        self.rep_len.reset();
        self.literal.reset();
        self.distance.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_transitions() {
        let mut state = State::new();
        assert!(state.is_literal());

        state.update_match();
        assert!(!state.is_literal());
        assert_eq!(state.value(), 7);

        state.update_literal();
        assert!(state.is_literal());
    }

    #[test]
    fn test_properties_encoding() {
        let props = LzmaProperties::new(3, 0, 2);
        let byte = props.to_byte();
        let decoded = LzmaProperties::from_byte(byte).unwrap();

        assert_eq!(decoded.lc, props.lc);
        assert_eq!(decoded.lp, props.lp);
        assert_eq!(decoded.pb, props.pb);
    }

    #[test]
    fn test_default_properties() {
        let props = LzmaProperties::default();
        assert_eq!(props.lc, 3);
        assert_eq!(props.lp, 0);
        assert_eq!(props.pb, 2);
    }

    #[test]
    fn test_model_creation() {
        let props = LzmaProperties::default();
        let model = LzmaModel::new(props);

        assert_eq!(model.is_match.len(), NUM_STATES);
        assert_eq!(model.is_rep.len(), NUM_STATES);
    }
}

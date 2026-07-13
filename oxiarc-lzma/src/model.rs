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

/// Number of special position probabilities.
///
/// Matches the LZMA reference implementation (`LzmaSpec.cpp`):
/// `CProb PosDecoders[1 + kNumFullDistances - kEndPosModelIndex]`.
/// The table is addressed as `special[(dist_base - slot) + m]` where
/// `dist_base = (2 | (slot & 1)) << ((slot >> 1) - 1)` and `m` is the
/// bit-tree node index starting at 1, so index 0 is never touched and the
/// highest index used is `(96 - 13) + 31 = 114`.
pub const SPEC_POS_PROBS: usize = 1 + FULL_DISTANCES - END_POS_MODEL_INDEX;

/// LZMA state machine state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State(u8);

impl State {
    /// Initial state.
    pub const fn new() -> Self {
        Self(0)
    }

    /// Construct from a raw state value (clamped to 0..11).
    pub fn from_value(v: u8) -> Self {
        Self(v.min(11))
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
    ///
    /// Follows the LZMA specification (`LzmaSpec.cpp`, `UpdateState_Literal`):
    /// `state < 4 -> 0`, `state < 10 -> state - 3`, otherwise `state - 6`.
    pub fn update_literal(&mut self) {
        self.0 = match self.0 {
            0..=3 => 0,
            4..=9 => self.0 - 3,
            _ => self.0 - 6,
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

/// Maximum literal context bits (`lc`) permitted by the LZMA format.
pub const LC_MAX: u32 = 8;
/// Maximum literal position bits (`lp`) permitted by the LZMA format.
pub const LP_MAX: u32 = 4;
/// Maximum position bits (`pb`) permitted by the LZMA format.
pub const PB_MAX: u32 = 4;

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
    ///
    /// Out-of-range values are saturated to the LZMA format limits
    /// (`lc <= 8`, `lp <= 4`, `pb <= 4`), mirroring [`crate::LzmaLevel::new`].
    /// Without this clamp, `1 << (lc + lp)` in the literal-model allocation
    /// could request a multi-TiB buffer and abort the process — a safe public
    /// API must never be able to do that.
    pub fn new(lc: u32, lp: u32, pb: u32) -> Self {
        Self {
            lc: lc.min(LC_MAX),
            lp: lp.min(LP_MAX),
            pb: pb.min(PB_MAX),
        }
    }

    /// Whether these properties are within the LZMA format limits
    /// (`lc <= 8`, `lp <= 4`, `pb <= 4`).
    ///
    /// Properties produced by [`Self::new`] and [`Self::from_byte`] are always
    /// valid; a struct-literal-built value may not be.
    pub fn is_valid(&self) -> bool {
        self.lc <= LC_MAX && self.lp <= LP_MAX && self.pb <= PB_MAX
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
    ///
    /// The shift amount is clamped to the format limits so that even a
    /// struct-literal-built value with out-of-range fields (the fields are
    /// public) can never request an allocation beyond `1 << 12` states.
    pub fn num_lit_states(&self) -> usize {
        1 << (self.lc.min(LC_MAX) + self.lp.min(LP_MAX))
    }

    /// Get number of position states.
    ///
    /// The shift amount is clamped to the format limit (see
    /// [`Self::num_lit_states`]).
    pub fn num_pos_states(&self) -> usize {
        1 << self.pb.min(PB_MAX)
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

    // NOTE: a former `reset(&mut self)` helper cascade was removed here; the
    // codec resets probabilities by rebuilding the model (`LzmaModel::new`).
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

    /// Get the literal state index.
    ///
    /// `lc`/`lp` are clamped to the LZMA format limits so an out-of-range
    /// value can neither underflow the `8 - lc` shift nor index past the
    /// table allocated by [`LiteralModel::new`].
    pub fn get_state(&self, pos: u64, prev_byte: u8, lc: u32, lp: u32) -> usize {
        let lc = lc.min(LC_MAX) as usize;
        let lp = lp.min(LP_MAX);
        let lit_pos = pos & ((1 << lp) - 1);
        let prev_bits = (prev_byte as usize) >> (8 - lc);
        ((lit_pos as usize) << lc) + prev_bits
    }
}

/// Distance slot model.
#[derive(Debug, Clone)]
pub struct DistanceModel {
    /// Distance slot probabilities (per length state).
    pub slot: [[u16; DIST_SLOTS]; 4],
    /// Special position probabilities for slots 4-13.
    ///
    /// Uses the LZMA specification layout (`PosDecoders + dist - posSlot`):
    /// the reverse bit tree for slot `s` starts at `dist_base - s` and is
    /// addressed by the bit-tree node index `m` (starting at 1). Adjacent
    /// slots deliberately share the flat array exactly as liblzma does.
    pub special: [u16; SPEC_POS_PROBS],
    /// Alignment probabilities.
    pub align: [u16; DIST_ALIGN_SIZE],
}

impl DistanceModel {
    /// Create a new distance model.
    pub fn new() -> Self {
        Self {
            slot: [[PROB_INIT; DIST_SLOTS]; 4],
            special: [PROB_INIT; SPEC_POS_PROBS],
            align: [PROB_INIT; DIST_ALIGN_SIZE],
        }
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
        let decoded = LzmaProperties::from_byte(byte).expect("valid LZMA operation");

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

    /// LZMA-03 regression: `LzmaProperties::new(20, 20, 4)` used to build a
    /// model requesting a multi-TiB literal table, aborting the process
    /// (SIGABRT) from a safe public API. The constructor now saturates.
    #[test]
    fn test_properties_new_clamps_out_of_range() {
        let props = LzmaProperties::new(20, 20, 20);
        assert_eq!((props.lc, props.lp, props.pb), (LC_MAX, LP_MAX, PB_MAX));
        assert!(props.is_valid());

        // Model construction is bounded (max 2^12 literal states) — this
        // call aborted the process before the fix.
        let model = LzmaModel::new(props);
        assert_eq!(model.literal.probs.len(), 1 << (LC_MAX + LP_MAX));
    }

    /// The fields are public, so a struct literal can bypass `new()`; every
    /// consumer must still be allocation- and panic-safe.
    #[test]
    fn test_struct_literal_props_cannot_force_huge_alloc() {
        let props = LzmaProperties {
            lc: 30,
            lp: 30,
            pb: 30,
        };
        assert!(!props.is_valid());
        assert_eq!(props.num_lit_states(), 1 << (LC_MAX + LP_MAX));
        assert_eq!(props.num_pos_states(), 1 << PB_MAX);

        // Must not abort (bounded allocation) …
        let model = LzmaModel::new(props);
        assert_eq!(model.literal.probs.len(), 1 << (LC_MAX + LP_MAX));

        // … and the literal-state lookup must not underflow `8 - lc` or
        // index out of bounds.
        let state = model.literal.get_state(123, 0xFF, props.lc, props.lp);
        assert!(state < 1 << (LC_MAX + LP_MAX));
    }
}

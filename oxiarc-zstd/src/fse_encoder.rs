//! FSE (Finite State Entropy) encoding for Zstandard sequences.
//!
//! This module provides FSE table building and encoding for the three sequence
//! components in Zstandard compressed blocks: literal lengths, match lengths,
//! and offsets. Each component is encoded using a Finite State Entropy table
//! that maps symbol frequencies to a state machine for entropy coding.
//!
//! FSE encoding works backwards (last symbol encoded first, decoded last).
//! The encoding table is the inverse of the decoding table: given a symbol
//! and the current state, it determines how many bits to output and the new state.

/// FSE encoding table entry.
///
/// Each entry stores the information needed to encode a symbol occurrence:
/// the delta to find the next state, and the number of bits to output.
#[derive(Debug, Clone, Copy)]
pub struct FseEncodeEntry {
    /// Delta to find next state from state index.
    pub delta_find_state: i32,
    /// Delta to number of bits (packed: nb_bits in low 16, delta_nb in high 16).
    pub delta_nb_bits: u32,
}

/// FSE encoding table.
///
/// Built from symbol frequencies, this table enables FSE encoding of a stream
/// of symbols by maintaining a state machine. Each symbol may have multiple
/// entries corresponding to its probability share.
pub struct FseEncodeTable {
    /// Entries indexed by state value. Each entry encodes one state transition.
    /// The table is organized as a flat array of size `1 << accuracy_log`.
    /// For each state, we store the symbol it represents and encoding info.
    state_symbols: Vec<u8>,
    /// For each state: (nb_bits, new_state_base)
    state_encoding: Vec<(u8, u16)>,
    /// Symbol-to-state mapping: for each symbol, the list of states that emit it.
    /// Used during encoding to find the initial state for a symbol.
    symbol_states: Vec<Vec<u16>>,
    /// Per-symbol occurrence counter for encoding (tracks which state to use next).
    symbol_counters: Vec<usize>,
    /// Accuracy log.
    accuracy_log: u8,
    /// Symbol probabilities (for serialization).
    probabilities: Vec<i16>,
    /// Number of symbols.
    num_symbols: usize,
}

impl FseEncodeTable {
    /// Build an FSE encoding table from symbol frequencies.
    ///
    /// The frequencies are normalized to sum to `1 << accuracy_log`, then the
    /// encoding table (inverse of the decoding table) is constructed using the
    /// same spread algorithm as the decoder.
    ///
    /// Returns `None` if only one distinct symbol exists (use RLE mode instead).
    pub fn from_frequencies(frequencies: &[u32], accuracy_log: u8) -> Option<Self> {
        if frequencies.is_empty() {
            return None;
        }

        let total: u64 = frequencies.iter().map(|&f| f as u64).sum();
        if total == 0 {
            return None;
        }

        // Count distinct symbols
        let distinct = frequencies.iter().filter(|&&f| f > 0).count();
        if distinct <= 1 {
            return None;
        }

        let table_size = 1usize << accuracy_log;

        // Normalize frequencies to sum to table_size
        let probabilities = Self::normalize_frequencies(frequencies, table_size);

        // Verify normalization
        let prob_sum: i32 = probabilities
            .iter()
            .map(|&p| if p == -1 { 1 } else { p.max(0) as i32 })
            .sum();
        if prob_sum != table_size as i32 {
            return None;
        }

        let num_symbols = probabilities.len();

        // Build the decoding table (same spread algorithm as decoder) to derive
        // the encoding table from it.
        let mut state_symbols = vec![0u8; table_size];
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;
        let mut position = 0usize;

        for (symbol, &prob) in probabilities.iter().enumerate() {
            let count = if prob == -1 { 1 } else { prob.max(0) as usize };
            for _ in 0..count {
                state_symbols[position] = symbol as u8;
                loop {
                    position = (position + step) & table_mask;
                    if position < table_size {
                        break;
                    }
                }
            }
        }

        // Build symbol_next (same as decoder's second pass)
        let mut symbol_next = vec![0u16; num_symbols];
        let mut cumulative = 0u16;
        for (symbol, &prob) in probabilities.iter().enumerate() {
            if prob == -1 {
                symbol_next[symbol] = (table_size - 1) as u16;
            } else if prob > 0 {
                symbol_next[symbol] = cumulative;
                cumulative += prob as u16;
            }
        }

        // Build state encoding info: for each state, compute (nb_bits, new_state_base)
        let mut state_encoding = vec![(0u8, 0u16); table_size];
        let mut symbol_next_copy = symbol_next.clone();

        for state in 0..table_size {
            let symbol = state_symbols[state] as usize;
            let prob = probabilities[symbol];

            if prob == -1 {
                state_encoding[state] = (accuracy_log, 0);
            } else if prob > 0 {
                let prob_val = prob as u16;
                let nb_bits = accuracy_log - highest_bit_set_u16(prob_val);
                let next = symbol_next_copy[symbol];
                symbol_next_copy[symbol] += 1;
                let baseline = (next << nb_bits).wrapping_sub(prob_val);
                state_encoding[state] = (nb_bits, baseline);
            }
        }

        // Build symbol_states: for each symbol, collect all states that correspond to it
        let mut symbol_states: Vec<Vec<u16>> = vec![Vec::new(); num_symbols];
        for (state, &sym) in state_symbols.iter().enumerate() {
            symbol_states[sym as usize].push(state as u16);
        }

        let symbol_counters = vec![0usize; num_symbols];

        Some(Self {
            state_symbols,
            state_encoding,
            symbol_states,
            symbol_counters,
            accuracy_log,
            probabilities,
            num_symbols,
        })
    }

    /// Normalize frequencies to sum to `table_size`.
    ///
    /// Uses proportional scaling with a "less than 1" marker (-1) for very
    /// rare symbols that still need at least one state allocated.
    fn normalize_frequencies(frequencies: &[u32], table_size: usize) -> Vec<i16> {
        let total: u64 = frequencies.iter().map(|&f| f as u64).sum();
        let mut probabilities = Vec::with_capacity(frequencies.len());
        let mut assigned = 0i32;
        let mut num_nonzero = 0usize;

        for &freq in frequencies {
            if freq == 0 {
                probabilities.push(0);
            } else {
                num_nonzero += 1;
                let prob = ((freq as u64 * table_size as u64) / total) as i16;
                if prob == 0 {
                    // Symbol is too rare for a full slot - mark as "less than 1"
                    probabilities.push(-1);
                    assigned += 1;
                } else {
                    probabilities.push(prob);
                    assigned += prob as i32;
                }
            }
        }

        // Distribute any remainder to the most probable symbols
        let remainder = table_size as i32 - assigned;
        if remainder != 0 {
            // Find the symbol with highest frequency that has prob > 0
            let mut best_idx = None;
            let mut best_freq = 0u32;
            for (i, &freq) in frequencies.iter().enumerate() {
                if probabilities[i] > 0 && freq > best_freq {
                    best_freq = freq;
                    best_idx = Some(i);
                }
            }
            if let Some(idx) = best_idx {
                probabilities[idx] += remainder as i16;
                // Ensure it doesn't go to zero or negative
                if probabilities[idx] <= 0 {
                    // Fallback: spread across all nonzero symbols
                    probabilities[idx] -= remainder as i16; // undo
                    Self::spread_remainder(&mut probabilities, frequencies, remainder, num_nonzero);
                }
            }
        }

        probabilities
    }

    /// Spread remainder across multiple symbols when a single adjustment would fail.
    fn spread_remainder(
        probabilities: &mut [i16],
        frequencies: &[u32],
        mut remainder: i32,
        _num_nonzero: usize,
    ) {
        // Sort indices by frequency descending
        let mut indices: Vec<usize> = (0..frequencies.len())
            .filter(|&i| probabilities[i] > 0)
            .collect();
        indices.sort_by(|&a, &b| frequencies[b].cmp(&frequencies[a]));

        let direction = if remainder > 0 { 1i16 } else { -1i16 };
        let mut idx = 0;
        while remainder != 0 && !indices.is_empty() {
            let i = indices[idx % indices.len()];
            let new_val = probabilities[i] + direction;
            if new_val > 0 {
                probabilities[i] = new_val;
                remainder -= direction as i32;
            }
            idx += 1;
            // Safety: prevent infinite loop
            if idx > indices.len() * (remainder.unsigned_abs() as usize + 1) {
                break;
            }
        }
    }

    /// Serialize the FSE table description (probabilities).
    ///
    /// Format: accuracy_log - 5 (4 bits) followed by variable-length probability
    /// encoding using the Zstandard FSE table description format.
    pub fn serialize(&self) -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();

        // Write accuracy_log - 5 in 4 bits (LSB first)
        let al_val = (self.accuracy_log - 5) as u32;
        for bit_idx in 0..4 {
            bits.push((al_val >> bit_idx) & 1 == 1);
        }

        let table_size = 1usize << self.accuracy_log;
        let mut remaining = table_size as i32;

        for &prob in &self.probabilities {
            if remaining <= 0 {
                break;
            }

            // Variable-length encoding of probability value
            // The value to encode is: if prob == -1 then 0, else prob + 1
            let value = if prob == -1 {
                0u32
            } else if prob == 0 {
                // Zero probability: encode and then handle repeat-zero
                1u32 // value 1 means probability 0
            } else {
                (prob as u32) + 1
            };

            let max_bits_needed = highest_bit_set_u32((remaining + 1) as u32) + 1;
            let low_bits = max_bits_needed - 1;
            let threshold = ((1u32 << max_bits_needed) - 1).wrapping_sub((remaining + 1) as u32);

            if value < threshold {
                // Write low_bits bits
                for bit_idx in 0..low_bits {
                    bits.push((value >> bit_idx) & 1 == 1);
                }
            } else {
                // Write low_bits + 1 bits
                let adjusted = value + threshold;
                for bit_idx in 0..low_bits {
                    bits.push(((adjusted >> 1) >> bit_idx) & 1 == 1);
                }
                bits.push(adjusted & 1 == 1);
            }

            if prob != 0 {
                remaining -= if prob == -1 { 1 } else { prob as i32 };
            }

            // Handle repeat zeros (we don't emit repeat-zero sequences in serialization
            // for simplicity; each zero is encoded individually)
            if prob == 0 {
                // Write a 2-bit repeat count of 0 (meaning no additional zeros)
                bits.push(false);
                bits.push(false);
            }
        }

        // Convert bits to bytes (LSB first within each byte)
        let num_bytes = bits.len().div_ceil(8);
        let mut output = Vec::with_capacity(num_bytes);
        for chunk_start in (0..bits.len()).step_by(8) {
            let mut byte = 0u8;
            for bit_idx in 0..8 {
                if chunk_start + bit_idx < bits.len() && bits[chunk_start + bit_idx] {
                    byte |= 1 << bit_idx;
                }
            }
            output.push(byte);
        }

        output
    }

    /// Get accuracy log.
    pub fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    /// Get the probabilities (for external inspection or debugging).
    pub fn probabilities(&self) -> &[i16] {
        &self.probabilities
    }

    /// Get the number of symbols.
    pub fn num_symbols(&self) -> usize {
        self.num_symbols
    }

    /// Reset symbol occurrence counters (call before encoding a new block).
    pub fn reset_counters(&mut self) {
        for c in &mut self.symbol_counters {
            *c = 0;
        }
    }

    /// Find the initial state for a given symbol.
    ///
    /// Returns the first state associated with the symbol, cycling through
    /// available states on repeated calls.
    pub(crate) fn initial_state_for(&mut self, symbol: u8) -> u16 {
        let sym = symbol as usize;
        if sym >= self.symbol_states.len() || self.symbol_states[sym].is_empty() {
            return 0;
        }
        let states = &self.symbol_states[sym];
        let counter = self.symbol_counters[sym];
        let state = states[counter % states.len()];
        self.symbol_counters[sym] = counter + 1;
        state
    }

    /// Get encoding info for a state: returns (nb_bits, new_state_base).
    pub(crate) fn get_encoding_info(&self, state: u16) -> (u8, u16) {
        self.state_encoding[state as usize]
    }

    /// Get the symbol at a given state.
    pub(crate) fn state_symbol(&self, state: u16) -> u8 {
        self.state_symbols[state as usize]
    }

    /// Encode a symbol transition: output bits from current state, then find
    /// a new state for the given symbol.
    ///
    /// Returns (nb_bits_to_output, bits_value, new_state).
    /// The bits should be written to the backward bitstream before the state update.
    pub(crate) fn encode_symbol(&mut self, state: u16, symbol: u8) -> (u8, u32, u16) {
        // Output bits from the current state
        let table_size = 1usize << self.accuracy_log;
        let (nb_bits, _baseline) = self.state_encoding[state as usize];
        let bits_to_output = (state as u32) & ((1u32 << nb_bits) - 1);

        // Find a new state for the given symbol
        let new_state = self.initial_state_for(symbol);

        // Ensure the new state is within bounds
        debug_assert!((new_state as usize) < table_size);

        (nb_bits, bits_to_output, new_state)
    }
}

/// FSE state encoder.
///
/// Maintains the FSE state machine during backward encoding of a symbol stream.
/// Symbols are encoded in reverse order; the decoder will read them forward.
pub struct FseStateEncoder<'a> {
    /// Mutable reference to the encoding table (needed for state counter tracking).
    table: &'a mut FseEncodeTable,
    /// Current state.
    state: u16,
}

impl<'a> FseStateEncoder<'a> {
    /// Initialize with first symbol.
    ///
    /// The first symbol sets the initial state without outputting any bits.
    pub fn init(table: &'a mut FseEncodeTable, symbol: u8) -> Self {
        let state = table.initial_state_for(symbol);
        Self { table, state }
    }

    /// Encode a symbol: compute bits to output and update state.
    ///
    /// Returns the (nb_bits, bits_value) that should be written to the backward
    /// bitstream. The caller is responsible for writing these bits.
    pub fn encode(&mut self, symbol: u8) -> (u8, u32) {
        let (nb_bits, bits_value, new_state) = self.table.encode_symbol(self.state, symbol);
        self.state = new_state;
        (nb_bits, bits_value)
    }

    /// Flush final state bits.
    ///
    /// After all symbols have been encoded, the final state value must be written
    /// to the bitstream so the decoder can initialize its state.
    pub fn flush(&self) -> (u8, u32) {
        (self.table.accuracy_log(), self.state as u32)
    }

    /// Get the current state.
    pub fn state(&self) -> u16 {
        self.state
    }
}

/// Literal length code table.
///
/// Maps a literal length value to (code, extra_bits, extra_value).
/// Codes 0-15 map directly; codes 16-35 use extra bits for larger values.
pub fn ll_code(literal_length: usize) -> (u8, u8, u32) {
    /// Literal length baselines for codes 0-35.
    const LL_BASELINE: [usize; 36] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ];
    /// Extra bits for each literal length code.
    const LL_EXTRA: [u8; 36] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10,
        11, 12, 13, 14, 15, 16,
    ];

    // Direct mapping for 0-15
    if literal_length <= 15 {
        return (literal_length as u8, 0, 0);
    }

    // Search for the right code bracket
    for code in (16..36).rev() {
        if literal_length >= LL_BASELINE[code] {
            let extra_value = (literal_length - LL_BASELINE[code]) as u32;
            return (code as u8, LL_EXTRA[code], extra_value);
        }
    }

    // Fallback (should not happen for valid input)
    (35, 16, (literal_length - 65536) as u32)
}

/// Match length code table.
///
/// Maps a match length value (minimum 3) to (code, extra_bits, extra_value).
/// Codes 0-31 map to match lengths 3-34; codes 32-52 use extra bits.
pub fn ml_code(match_length: usize) -> (u8, u8, u32) {
    /// Match length baselines for codes 0-52.
    const ML_BASELINE: [usize; 53] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515,
        1027, 2051, 4099, 8195, 16387, 32771, 65539,
    ];
    /// Extra bits for each match length code.
    const ML_EXTRA: [u8; 53] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ];

    // Direct mapping for match lengths 3-34 (codes 0-31)
    if (3..=34).contains(&match_length) {
        return ((match_length - 3) as u8, 0, 0);
    }

    // Search for the right code bracket
    for code in (32..53).rev() {
        if match_length >= ML_BASELINE[code] {
            let extra_value = (match_length - ML_BASELINE[code]) as u32;
            return (code as u8, ML_EXTRA[code], extra_value);
        }
    }

    // Fallback (should not happen for valid input >= 3)
    (52, 16, (match_length - 65539) as u32)
}

/// Offset code.
///
/// Maps an offset value to (code, extra_bits, extra_value).
/// `code = floor(log2(offset))`, `extra_bits = code`, `extra_value = offset - (1 << code)`.
///
/// Note: offset must be >= 1 for valid Zstandard offsets.
pub fn of_code(offset: usize) -> (u8, u8, u32) {
    if offset == 0 {
        return (0, 0, 0);
    }

    // code = floor(log2(offset)) = position of highest set bit
    let code = highest_bit_position(offset);

    if code == 0 {
        // offset == 1: code=0, no extra bits
        return (0, 0, 0);
    }

    let extra_bits = code;
    let extra_value = (offset - (1usize << code)) as u32;

    (code as u8, extra_bits as u8, extra_value)
}

/// Choose the best compression mode for a symbol distribution.
///
/// Analyzes the frequency distribution to select between predefined tables,
/// RLE encoding, or custom FSE tables.
pub fn choose_mode(frequencies: &[u32], total: u32) -> SequenceCompressionMode {
    if total == 0 {
        return SequenceCompressionMode::Predefined;
    }

    // Count distinct symbols
    let mut distinct_count = 0usize;
    let mut single_symbol = 0u8;
    for (i, &freq) in frequencies.iter().enumerate() {
        if freq > 0 {
            distinct_count += 1;
            single_symbol = i as u8;
        }
    }

    if distinct_count == 0 {
        return SequenceCompressionMode::Predefined;
    }

    if distinct_count == 1 {
        return SequenceCompressionMode::Rle(single_symbol);
    }

    // Check if the distribution is close enough to predefined to not warrant
    // a custom table. Use a simple heuristic: if the number of distinct symbols
    // is small and total count is low, predefined may suffice.
    if total < 16 && distinct_count <= 4 {
        return SequenceCompressionMode::Predefined;
    }

    // Choose accuracy log based on total count
    let accuracy_log = choose_accuracy_log(total, distinct_count);

    match FseEncodeTable::from_frequencies(frequencies, accuracy_log) {
        Some(table) => SequenceCompressionMode::Fse(table),
        None => SequenceCompressionMode::Predefined,
    }
}

/// Choose an appropriate accuracy log for FSE table based on data characteristics.
///
/// Higher accuracy logs give better compression but larger tables.
/// Zstandard limits: LL max 9, ML max 9, OF max 8.
fn choose_accuracy_log(total: u32, distinct: usize) -> u8 {
    // Ensure at least 2^accuracy_log >= 2 * distinct for good symbol spread
    let min_log = if distinct <= 2 {
        5
    } else {
        let needed = (distinct * 2).next_power_of_two().trailing_zeros() as u8;
        needed.max(5)
    };

    // Scale with data size
    let size_log = if total < 64 {
        5
    } else if total < 256 {
        6
    } else if total < 1024 {
        7
    } else if total < 4096 {
        8
    } else {
        9
    };

    min_log.max(size_log).min(9)
}

/// Sequence compression mode.
///
/// Determines how a sequence component (literal length, match length, or offset)
/// will be encoded in the compressed block.
pub enum SequenceCompressionMode {
    /// Use predefined FSE table.
    Predefined,
    /// Use RLE (all same symbol).
    Rle(u8),
    /// Use custom FSE table.
    Fse(FseEncodeTable),
}

/// Find the position of the highest set bit (0-indexed from LSB) for u16.
#[inline]
fn highest_bit_set_u16(value: u16) -> u8 {
    if value == 0 {
        0
    } else {
        15 - value.leading_zeros() as u8
    }
}

/// Find the position of the highest set bit (0-indexed from LSB) for u32.
#[inline]
fn highest_bit_set_u32(value: u32) -> u8 {
    if value == 0 {
        0
    } else {
        31 - value.leading_zeros() as u8
    }
}

/// Find the position of the highest set bit for usize.
#[inline]
fn highest_bit_position(value: usize) -> usize {
    if value == 0 {
        0
    } else {
        (usize::BITS - 1 - value.leading_zeros()) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- ll_code tests ---

    #[test]
    fn test_ll_code_direct() {
        for i in 0..=15 {
            let (code, extra_bits, extra_value) = ll_code(i);
            assert_eq!(code, i as u8, "ll_code({}) code mismatch", i);
            assert_eq!(extra_bits, 0, "ll_code({}) should have 0 extra bits", i);
            assert_eq!(extra_value, 0, "ll_code({}) should have 0 extra value", i);
        }
    }

    #[test]
    fn test_ll_code_with_extra_bits() {
        // Code 16: baseline 16, 1 extra bit -> covers 16-17
        let (code, extra_bits, extra_value) = ll_code(16);
        assert_eq!(code, 16);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 0);

        let (code, extra_bits, extra_value) = ll_code(17);
        assert_eq!(code, 16);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 1);

        // Code 17: baseline 18, 1 extra bit -> covers 18-19
        let (code, extra_bits, extra_value) = ll_code(18);
        assert_eq!(code, 17);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 0);

        // Code 20: baseline 24, 2 extra bits -> covers 24-27
        let (code, extra_bits, extra_value) = ll_code(24);
        assert_eq!(code, 20);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_value, 0);

        let (code, extra_bits, extra_value) = ll_code(27);
        assert_eq!(code, 20);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_value, 3);
    }

    #[test]
    fn test_ll_code_large_values() {
        // Code 35: baseline 65536, 16 extra bits
        let (code, extra_bits, _) = ll_code(65536);
        assert_eq!(code, 35);
        assert_eq!(extra_bits, 16);
    }

    // --- ml_code tests ---

    #[test]
    fn test_ml_code_direct() {
        for ml in 3..=34 {
            let (code, extra_bits, extra_value) = ml_code(ml);
            assert_eq!(code, (ml - 3) as u8, "ml_code({}) code mismatch", ml);
            assert_eq!(extra_bits, 0, "ml_code({}) should have 0 extra bits", ml);
            assert_eq!(extra_value, 0, "ml_code({}) should have 0 extra value", ml);
        }
    }

    #[test]
    fn test_ml_code_with_extra_bits() {
        // Code 32: baseline 35, 1 extra bit -> covers 35-36
        let (code, extra_bits, extra_value) = ml_code(35);
        assert_eq!(code, 32);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 0);

        let (code, extra_bits, extra_value) = ml_code(36);
        assert_eq!(code, 32);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 1);

        // Code 36: baseline 43, 2 extra bits -> covers 43-46
        let (code, extra_bits, extra_value) = ml_code(43);
        assert_eq!(code, 36);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_value, 0);
    }

    // --- of_code tests ---

    #[test]
    fn test_of_code_small() {
        // offset=1: code=0, no extra bits
        let (code, extra_bits, extra_value) = of_code(1);
        assert_eq!(code, 0);
        assert_eq!(extra_bits, 0);
        assert_eq!(extra_value, 0);

        // offset=2: code=1, 1 extra bit, extra=0
        let (code, extra_bits, extra_value) = of_code(2);
        assert_eq!(code, 1);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 0);

        // offset=3: code=1, 1 extra bit, extra=1
        let (code, extra_bits, extra_value) = of_code(3);
        assert_eq!(code, 1);
        assert_eq!(extra_bits, 1);
        assert_eq!(extra_value, 1);
    }

    #[test]
    fn test_of_code_powers_of_two() {
        // offset=4: code=2, 2 extra bits, extra=0
        let (code, extra_bits, extra_value) = of_code(4);
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_value, 0);

        // offset=8: code=3, 3 extra bits, extra=0
        let (code, extra_bits, extra_value) = of_code(8);
        assert_eq!(code, 3);
        assert_eq!(extra_bits, 3);
        assert_eq!(extra_value, 0);

        // offset=1024: code=10, 10 extra bits, extra=0
        let (code, extra_bits, extra_value) = of_code(1024);
        assert_eq!(code, 10);
        assert_eq!(extra_bits, 10);
        assert_eq!(extra_value, 0);
    }

    #[test]
    fn test_of_code_non_power() {
        // offset=5: code=2, 2 extra bits, extra=1
        let (code, extra_bits, extra_value) = of_code(5);
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_value, 1);

        // offset=7: code=2, 2 extra bits, extra=3
        let (code, extra_bits, extra_value) = of_code(7);
        assert_eq!(code, 2);
        assert_eq!(extra_bits, 2);
        assert_eq!(extra_value, 3);
    }

    // --- FseEncodeTable tests ---

    #[test]
    fn test_fse_table_empty_returns_none() {
        assert!(FseEncodeTable::from_frequencies(&[], 5).is_none());
    }

    #[test]
    fn test_fse_table_all_zero_returns_none() {
        assert!(FseEncodeTable::from_frequencies(&[0, 0, 0], 5).is_none());
    }

    #[test]
    fn test_fse_table_single_symbol_returns_none() {
        assert!(FseEncodeTable::from_frequencies(&[100, 0, 0], 5).is_none());
    }

    #[test]
    fn test_fse_table_two_equal_symbols() {
        let freqs = [50, 50];
        let table = FseEncodeTable::from_frequencies(&freqs, 5);
        assert!(table.is_some());
        let tbl = table.as_ref().expect("table should exist");
        assert_eq!(tbl.accuracy_log(), 5);
        assert_eq!(tbl.num_symbols(), 2);
    }

    #[test]
    fn test_fse_table_serialize_nonempty() {
        let freqs = [100, 50, 25];
        let table = FseEncodeTable::from_frequencies(&freqs, 6);
        assert!(table.is_some());
        let tbl = table.as_ref().expect("table should exist");
        let serialized = tbl.serialize();
        assert!(!serialized.is_empty());
        // First nibble (4 bits) should encode accuracy_log - 5
        let al_val = serialized[0] & 0x0F;
        assert_eq!(al_val, tbl.accuracy_log() - 5);
    }

    #[test]
    fn test_fse_table_multiple_symbols() {
        let freqs = [100, 80, 60, 40, 20, 10, 5, 1];
        let table = FseEncodeTable::from_frequencies(&freqs, 8);
        assert!(table.is_some());
        let tbl = table.as_ref().expect("table should exist");
        assert_eq!(tbl.num_symbols(), 8);

        // Check probabilities sum to table_size
        let table_size = 1usize << tbl.accuracy_log();
        let prob_sum: i32 = tbl
            .probabilities()
            .iter()
            .map(|&p| if p == -1 { 1 } else { p.max(0) as i32 })
            .sum();
        assert_eq!(prob_sum, table_size as i32);
    }

    // --- choose_mode tests ---

    #[test]
    fn test_choose_mode_empty() {
        match choose_mode(&[0, 0, 0], 0) {
            SequenceCompressionMode::Predefined => {}
            _ => panic!("expected Predefined"),
        }
    }

    #[test]
    fn test_choose_mode_single_symbol() {
        match choose_mode(&[0, 100, 0], 100) {
            SequenceCompressionMode::Rle(sym) => assert_eq!(sym, 1),
            _ => panic!("expected Rle"),
        }
    }

    #[test]
    fn test_choose_mode_fse() {
        let mut freqs = [0u32; 36];
        freqs[0] = 500;
        freqs[1] = 300;
        freqs[2] = 100;
        freqs[3] = 50;
        freqs[4] = 30;
        freqs[5] = 20;
        match choose_mode(&freqs, 1000) {
            SequenceCompressionMode::Fse(table) => {
                assert!(table.accuracy_log() >= 5);
            }
            _ => panic!("expected Fse"),
        }
    }

    // --- FseStateEncoder tests ---

    #[test]
    fn test_fse_state_encoder_init() {
        let freqs = [50, 50];
        let mut table = FseEncodeTable::from_frequencies(&freqs, 5).expect("table should exist");
        let encoder = FseStateEncoder::init(&mut table, 0);
        // State should be a valid state index
        assert!(encoder.state() < (1 << 5));
    }

    #[test]
    fn test_fse_state_encoder_encode_and_flush() {
        let freqs = [60, 40];
        let mut table = FseEncodeTable::from_frequencies(&freqs, 5).expect("table should exist");
        let mut encoder = FseStateEncoder::init(&mut table, 0);
        let (_nb_bits, _bits_val) = encoder.encode(1);
        let (flush_bits, flush_val) = encoder.flush();
        assert_eq!(flush_bits, 5);
        // The flush value represents the final state which should be a valid
        // table index (0..2^accuracy_log)
        let table_size = 1u32 << 5;
        assert!(
            flush_val < table_size,
            "flush_val {} should be < table_size {}",
            flush_val,
            table_size
        );
    }

    // --- Helper function tests ---

    #[test]
    fn test_highest_bit_set_u16() {
        assert_eq!(highest_bit_set_u16(0), 0);
        assert_eq!(highest_bit_set_u16(1), 0);
        assert_eq!(highest_bit_set_u16(2), 1);
        assert_eq!(highest_bit_set_u16(4), 2);
        assert_eq!(highest_bit_set_u16(255), 7);
        assert_eq!(highest_bit_set_u16(256), 8);
    }

    #[test]
    fn test_highest_bit_position() {
        assert_eq!(highest_bit_position(0), 0);
        assert_eq!(highest_bit_position(1), 0);
        assert_eq!(highest_bit_position(2), 1);
        assert_eq!(highest_bit_position(8), 3);
        assert_eq!(highest_bit_position(1024), 10);
    }

    #[test]
    fn test_choose_accuracy_log() {
        assert_eq!(choose_accuracy_log(10, 2), 5);
        assert_eq!(choose_accuracy_log(100, 3), 6);
        assert_eq!(choose_accuracy_log(500, 5), 7);
        assert!(choose_accuracy_log(5000, 10) <= 9);
    }
}

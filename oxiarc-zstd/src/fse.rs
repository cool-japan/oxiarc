//! Finite State Entropy (FSE) codec.
//!
//! FSE is an entropy coding method used in Zstandard for encoding
//! literal lengths, match lengths, and offsets.

use oxiarc_core::error::{OxiArcError, Result};

/// Maximum accuracy log for FSE tables.
pub const MAX_ACCURACY_LOG: u8 = 9;

/// Maximum number of symbols.
#[allow(dead_code)]
pub const MAX_SYMBOLS: usize = 256;

/// FSE decoding table entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct FseTableEntry {
    /// Symbol to emit.
    pub symbol: u8,
    /// Number of bits to read for next state.
    pub num_bits: u8,
    /// Baseline for calculating next state.
    pub baseline: u16,
}

/// FSE decoding table.
#[derive(Debug, Clone)]
pub struct FseTable {
    /// Table entries indexed by state.
    entries: Vec<FseTableEntry>,
    /// Accuracy log (table size = 1 << accuracy_log).
    accuracy_log: u8,
}

impl FseTable {
    /// Create a new FSE table with given accuracy log and symbol probabilities.
    ///
    /// # Arguments
    /// * `accuracy_log` - Log2 of table size (5-9 typically)
    /// * `probabilities` - Normalized probabilities for each symbol (-1 = less than 1)
    pub fn new(accuracy_log: u8, probabilities: &[i16]) -> Result<Self> {
        if accuracy_log > MAX_ACCURACY_LOG {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: format!(
                    "accuracy log {} exceeds maximum {}",
                    accuracy_log, MAX_ACCURACY_LOG
                ),
            });
        }

        let table_size = 1usize << accuracy_log;
        let mut entries = vec![FseTableEntry::default(); table_size];

        // Build the state allocation
        let mut symbol_next = vec![0u16; probabilities.len()];

        // First pass: calculate starting positions
        let mut cumulative = 0u16;
        for (symbol, &prob) in probabilities.iter().enumerate() {
            if prob == -1 {
                // Less than 1 probability - allocate 1 state at the end
                symbol_next[symbol] = (table_size - 1) as u16;
            } else if prob > 0 {
                symbol_next[symbol] = cumulative;
                cumulative += prob as u16;
            }
        }

        // Spread symbols across states using step algorithm
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;
        let mut position = 0usize;

        for (symbol, &prob) in probabilities.iter().enumerate() {
            let count = if prob == -1 { 1 } else { prob.max(0) as usize };

            for _ in 0..count {
                entries[position].symbol = symbol as u8;

                // Find next available position using step
                loop {
                    position = (position + step) & table_mask;
                    // Skip positions that are part of high-state symbols
                    if position < table_size {
                        break;
                    }
                }
            }
        }

        // Second pass: fill in num_bits and baseline
        for entry in &mut entries {
            let symbol = entry.symbol as usize;
            let prob = probabilities[symbol];

            if prob == -1 {
                // Less than 1 probability
                entry.num_bits = accuracy_log;
                entry.baseline = 0;
            } else if prob > 0 {
                let prob = prob as u16;
                // Calculate num_bits: number of bits needed
                let num_bits = accuracy_log - highest_bit_set(prob);
                entry.num_bits = num_bits;

                // Calculate baseline
                let next_state = symbol_next[symbol];
                symbol_next[symbol] += 1;
                entry.baseline = (next_state << num_bits).wrapping_sub(prob);
            }
        }

        Ok(Self {
            entries,
            accuracy_log,
        })
    }

    /// Create FSE table from predefined entries (for predefined tables).
    pub fn from_entries(accuracy_log: u8, entries: Vec<FseTableEntry>) -> Self {
        Self {
            entries,
            accuracy_log,
        }
    }

    /// Get table entry for a given state.
    #[inline]
    pub fn get(&self, state: usize) -> &FseTableEntry {
        &self.entries[state]
    }

    /// Get the accuracy log.
    pub fn accuracy_log(&self) -> u8 {
        self.accuracy_log
    }

    /// Get the table size.
    #[allow(dead_code)]
    pub fn size(&self) -> usize {
        self.entries.len()
    }
}

/// FSE bitstream reader for decoding.
pub struct FseBitReader<'a> {
    /// Input bytes (read backwards).
    data: &'a [u8],
    /// Current bit position from end.
    bit_pos: usize,
    /// Accumulated bits.
    bits: u64,
    /// Number of valid bits in accumulator.
    bits_count: u8,
}

impl<'a> FseBitReader<'a> {
    /// Create a new FSE bit reader.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "empty FSE bitstream".to_string(),
            });
        }

        let mut reader = Self {
            data,
            bit_pos: data.len() * 8,
            bits: 0,
            bits_count: 0,
        };

        // Find the highest set bit in last byte (marks stream start)
        let last_byte = data[data.len() - 1];
        if last_byte == 0 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "FSE stream ends with zero byte".to_string(),
            });
        }

        let padding_bits = 7 - highest_bit_set(last_byte as u16);
        reader.bit_pos -= padding_bits as usize + 1; // Skip padding and sentinel

        // Fill initial bits
        reader.refill();

        Ok(reader)
    }

    /// Refill the bit buffer from input.
    fn refill(&mut self) {
        while self.bits_count <= 56 && self.bit_pos >= 8 {
            self.bit_pos -= 8;
            let byte_idx = self.bit_pos / 8;
            self.bits |= (self.data[byte_idx] as u64) << self.bits_count;
            self.bits_count += 8;
        }
    }

    /// Read n bits from the stream.
    #[inline]
    pub fn read_bits(&mut self, n: u8) -> u32 {
        if n == 0 {
            return 0;
        }

        self.refill();

        let mask = (1u64 << n) - 1;
        let result = (self.bits & mask) as u32;
        self.bits >>= n;
        self.bits_count = self.bits_count.saturating_sub(n);

        result
    }

    /// Check if the stream is exhausted.
    pub fn is_empty(&self) -> bool {
        self.bits_count == 0 && self.bit_pos == 0
    }
}

/// FSE decoder state machine.
pub struct FseDecoder<'a> {
    /// FSE table.
    table: &'a FseTable,
    /// Current state.
    state: usize,
}

impl<'a> FseDecoder<'a> {
    /// Create a new decoder with initial state.
    pub fn new(table: &'a FseTable, reader: &mut FseBitReader) -> Self {
        let state = reader.read_bits(table.accuracy_log()) as usize;
        Self { table, state }
    }

    /// Decode next symbol and update state.
    pub fn decode(&mut self, reader: &mut FseBitReader) -> u8 {
        let entry = self.table.get(self.state);
        let symbol = entry.symbol;

        // Calculate next state
        let bits = reader.read_bits(entry.num_bits);
        self.state = entry.baseline as usize + bits as usize;

        symbol
    }

    /// Peek at current symbol without advancing.
    #[allow(dead_code)]
    pub fn peek(&self) -> u8 {
        self.table.get(self.state).symbol
    }
}

/// Read FSE table description from forward bitstream.
pub fn read_fse_table_description(data: &[u8], max_symbol: u8) -> Result<(FseTable, usize)> {
    if data.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "empty FSE table description".to_string(),
        });
    }

    let mut bit_pos = 0usize;

    // Read accuracy log (4 bits + 5)
    let accuracy_log = read_bits_forward(data, &mut bit_pos, 4)? as u8 + 5;

    if accuracy_log > MAX_ACCURACY_LOG {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("accuracy log {} exceeds maximum", accuracy_log),
        });
    }

    let table_size = 1usize << accuracy_log;
    let mut remaining = table_size as i32;
    let mut probabilities = Vec::with_capacity(max_symbol as usize + 1);
    let mut symbol = 0u8;

    while remaining > 0 && symbol <= max_symbol {
        // Read probability using variable-length encoding
        let max_bits = highest_bit_set((remaining + 1) as u16) + 1;
        let low_bits = max_bits - 1;

        let low_value = read_bits_forward(data, &mut bit_pos, low_bits)?;
        let threshold = (1 << max_bits) - 1 - (remaining + 1) as u32;

        let prob_value = if low_value < threshold {
            low_value
        } else {
            let high_bit = read_bits_forward(data, &mut bit_pos, 1)?;
            (low_value << 1) + high_bit - threshold
        };

        // Convert to probability (-1, 0, or positive)
        let prob = if prob_value == 0 {
            -1i16 // Less than 1
        } else {
            (prob_value as i16) - 1
        };

        probabilities.push(prob);

        if prob != 0 {
            remaining -= if prob == -1 { 1 } else { prob as i32 };
        }

        symbol += 1;

        // Handle repeat zeros
        if prob == 0 {
            loop {
                let repeat = read_bits_forward(data, &mut bit_pos, 2)?;
                probabilities.resize(probabilities.len() + repeat as usize, 0);
                symbol += repeat as u8;
                if repeat < 3 {
                    break;
                }
            }
        }
    }

    // Pad to byte boundary
    let bytes_consumed = bit_pos.div_ceil(8);

    let table = FseTable::new(accuracy_log, &probabilities)?;

    Ok((table, bytes_consumed))
}

/// Read bits from forward bitstream.
fn read_bits_forward(data: &[u8], bit_pos: &mut usize, num_bits: u8) -> Result<u32> {
    if num_bits == 0 {
        return Ok(0);
    }

    let byte_pos = *bit_pos / 8;
    let bit_offset = *bit_pos % 8;

    if byte_pos >= data.len() {
        return Err(OxiArcError::CorruptedData {
            offset: byte_pos as u64,
            message: "unexpected end of FSE data".to_string(),
        });
    }

    // Read up to 4 bytes for safety
    let mut value = 0u64;
    for i in 0..4 {
        if byte_pos + i < data.len() {
            value |= (data[byte_pos + i] as u64) << (i * 8);
        }
    }

    let result = ((value >> bit_offset) & ((1u64 << num_bits) - 1)) as u32;
    *bit_pos += num_bits as usize;

    Ok(result)
}

/// Find the position of the highest set bit (0-indexed from LSB).
#[inline]
fn highest_bit_set(value: u16) -> u8 {
    if value == 0 {
        0
    } else {
        15 - value.leading_zeros() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_highest_bit_set() {
        assert_eq!(highest_bit_set(0), 0);
        assert_eq!(highest_bit_set(1), 0);
        assert_eq!(highest_bit_set(2), 1);
        assert_eq!(highest_bit_set(4), 2);
        assert_eq!(highest_bit_set(8), 3);
        assert_eq!(highest_bit_set(255), 7);
        assert_eq!(highest_bit_set(256), 8);
    }

    #[test]
    fn test_fse_table_creation() {
        // Simple uniform distribution: 4 symbols with equal probability
        let probs = [4i16, 4, 4, 4]; // Each symbol gets 4 states in a 16-state table
        let table = FseTable::new(4, &probs).unwrap();

        assert_eq!(table.accuracy_log(), 4);
        assert_eq!(table.size(), 16);
    }

    #[test]
    fn test_fse_table_with_less_than_one() {
        // Mix of normal and less-than-one probabilities
        let probs = [8i16, 4, 2, 1, -1]; // Total = 15 + 1 = 16
        let table = FseTable::new(4, &probs).unwrap();

        assert_eq!(table.size(), 16);
    }

    #[test]
    fn test_read_bits_forward() {
        let data = [0b10110100, 0b11001010];
        let mut bit_pos = 0;

        assert_eq!(read_bits_forward(&data, &mut bit_pos, 4).unwrap(), 0b0100);
        assert_eq!(read_bits_forward(&data, &mut bit_pos, 4).unwrap(), 0b1011);
        assert_eq!(read_bits_forward(&data, &mut bit_pos, 4).unwrap(), 0b1010);
    }
}

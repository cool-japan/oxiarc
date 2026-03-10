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

        // Step 1: Place -1 (less-than-1 probability) symbols at the high end of the table.
        // These are assigned exactly 1 cell each, starting from tableSize-1 downward.
        let mut high_threshold = table_size - 1;
        let mut symbol_next = vec![0u16; probabilities.len()];

        for (symbol, &prob) in probabilities.iter().enumerate() {
            if prob == -1 {
                entries[high_threshold].symbol = symbol as u8;
                high_threshold = high_threshold.wrapping_sub(1);
                symbol_next[symbol] = 1; // -1 prob symbols get symbolNext = 1
            } else if prob > 0 {
                symbol_next[symbol] = prob as u16;
            }
        }

        // Step 2: Spread positive-probability symbols across the remaining positions
        // using the step algorithm. Skip positions already taken by -1 symbols.
        let table_mask = table_size - 1;
        let step = (table_size >> 1) + (table_size >> 3) + 3;
        let mut position = 0usize;

        for (symbol, &prob) in probabilities.iter().enumerate() {
            if prob <= 0 {
                continue; // -1 symbols already placed; 0 probability = absent
            }
            let count = prob as usize;
            for _ in 0..count {
                entries[position].symbol = symbol as u8;
                // Advance to next position, skipping high-end positions used by -1 symbols
                loop {
                    position = (position + step) & table_mask;
                    if position <= high_threshold {
                        break;
                    }
                }
            }
        }

        // Step 3: Fill in num_bits and baseline for each state.
        // For each state, symbolNext[s] tracks which "instance" of symbol s we're at.
        // baseline = (symbolNext[s] << num_bits) - table_size
        // num_bits = accuracy_log - floor(log2(symbolNext[s]))
        for entry in &mut entries {
            let symbol = entry.symbol as usize;

            let next_state = symbol_next[symbol];
            symbol_next[symbol] += 1;

            let num_bits = accuracy_log - highest_bit_set(next_state);
            entry.num_bits = num_bits;
            entry.baseline =
                ((next_state as u32) << num_bits).wrapping_sub(table_size as u32) as u16;
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
///
/// Reads a backward (reversed) bitstream as used in Zstandard sequence encoding.
/// The last byte contains a sentinel bit (the highest set bit) which marks the
/// start of the data. Bits are read starting just below the sentinel, proceeding
/// towards byte 0.
pub struct FseBitReader<'a> {
    /// Input bytes.
    data: &'a [u8],
    /// Index of the next byte to load (going backwards from the end).
    /// Starts at `data.len() - 2` (byte before the sentinel byte) and decrements.
    next_byte_idx: isize,
    /// Accumulated bits (LSB = next bit to return).
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

        // Find the sentinel in the last byte.
        let last_byte = data[data.len() - 1];
        if last_byte == 0 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "FSE stream ends with zero byte".to_string(),
            });
        }

        // The sentinel is the highest set bit. Data bits are below it.
        let sentinel_pos = highest_bit_set(last_byte as u16);
        let data_bits_in_last = sentinel_pos; // bits 0..sentinel_pos-1

        // Extract the data bits from the last byte (below sentinel).
        let initial_bits = if data_bits_in_last > 0 {
            (last_byte & ((1u8 << data_bits_in_last) - 1)) as u64
        } else {
            0
        };

        let mut reader = Self {
            data,
            next_byte_idx: data.len() as isize - 2,
            bits: initial_bits,
            bits_count: data_bits_in_last,
        };

        // Pre-fill the accumulator with more bytes.
        reader.refill();

        Ok(reader)
    }

    /// Refill the bit buffer from input bytes, loading from the byte just
    /// before the last loaded byte and working towards byte 0.
    fn refill(&mut self) {
        while self.bits_count <= 56 && self.next_byte_idx >= 0 {
            let byte_val = self.data[self.next_byte_idx as usize];
            self.bits |= (byte_val as u64) << self.bits_count;
            self.bits_count += 8;
            self.next_byte_idx -= 1;
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
        self.bits_count == 0 && self.next_byte_idx < 0
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

    #[test]
    fn test_backward_writer_reader_roundtrip() {
        use crate::bitwriter::BackwardBitWriter;

        let mut writer = BackwardBitWriter::new();
        writer.write_bits(42, 6);
        writer.write_bits(7, 5);
        writer.write_bits(100, 8);
        let output = writer.finish();

        let mut reader = FseBitReader::new(&output).expect("should create reader");
        let v1 = reader.read_bits(6);
        let v2 = reader.read_bits(5);
        let v3 = reader.read_bits(8);

        assert_eq!(v1, 42, "first value");
        assert_eq!(v2, 7, "second value");
        assert_eq!(v3, 100, "third value");
    }
}

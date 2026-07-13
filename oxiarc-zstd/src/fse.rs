//! Finite State Entropy (FSE) codec.
//!
//! FSE is an entropy coding method used in Zstandard for encoding
//! literal lengths, match lengths, offsets and Huffman weights.
//!
//! The sequence/weight bitstreams use the RFC 8878 *backward* bitstream:
//! bytes are written forward, but the decoder starts from the **last** byte
//! (which carries a sentinel `1` bit above the final data bit) and consumes
//! bits from the most-recently-written end towards byte 0. This mirrors the
//! reference `BIT_initDStream` / `BIT_readBits` / `BIT_reloadDStream`
//! semantics.

use oxiarc_core::error::{OxiArcError, Result};

/// Maximum accuracy log for FSE tables (sequence literal/match lengths).
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
    ///
    /// The probabilities must sum (counting `-1` entries as one slot each) to
    /// exactly `1 << accuracy_log`, as required by RFC 8878; otherwise the
    /// table description is corrupt.
    pub fn new(accuracy_log: u8, probabilities: &[i16]) -> Result<Self> {
        if accuracy_log > MAX_ACCURACY_LOG {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "accuracy log {} exceeds maximum {}",
                    accuracy_log, MAX_ACCURACY_LOG
                ),
            ));
        }

        let table_size = 1usize << accuracy_log;

        // Validate the distribution: slots must sum to exactly table_size and
        // individual probabilities must be sane.
        let mut slot_sum = 0i64;
        for &prob in probabilities {
            if prob < -1 {
                return Err(OxiArcError::corrupted(
                    0,
                    format!("invalid FSE probability {}", prob),
                ));
            }
            slot_sum += if prob == -1 { 1 } else { prob as i64 };
        }
        if slot_sum != table_size as i64 {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "FSE probabilities sum to {} but table size is {}",
                    slot_sum, table_size
                ),
            ));
        }

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

        // The spread must land exactly back at position 0 (reference decoder
        // performs the same sanity check).
        if position != 0 {
            return Err(OxiArcError::corrupted(
                0,
                "FSE symbol spread did not terminate at position 0",
            ));
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

    /// Get table entry for a given state, bounds-checked.
    #[inline]
    pub fn get(&self, state: usize) -> Result<&FseTableEntry> {
        self.entries.get(state).ok_or_else(|| {
            OxiArcError::corrupted(
                0,
                format!(
                    "FSE state {} out of range (table size {})",
                    state,
                    self.entries.len()
                ),
            )
        })
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

/// FSE/Huffman backward bitstream reader (RFC 8878).
///
/// The encoder appends bit-fields least-significant-bit first into a
/// little-endian bit sequence, then terminates the stream with a single `1`
/// sentinel bit and zero-pads to a byte boundary. The decoder locates the
/// sentinel in the **last** byte and reads fields back in reverse write
/// order: each `read_bits(n)` returns the `n` bits immediately below the
/// current position, exactly like the reference `BIT_readBits`.
///
/// Reads past the beginning of the stream ("overflow") yield zero bits and
/// set an internal flag, matching `BIT_DStream_overflow`; callers decide
/// whether that terminates decoding (Huffman-weight streams) or is an error
/// (sequence streams).
pub struct FseBitReader<'a> {
    /// Input bytes.
    data: &'a [u8],
    /// Number of unread data bits (bit position of the read cursor, counting
    /// from the start of the stream). Negative once over-read.
    bits_remaining: i64,
}

impl<'a> FseBitReader<'a> {
    /// Create a new FSE bit reader.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(OxiArcError::corrupted(0, "empty FSE bitstream"));
        }

        let last_byte = data[data.len() - 1];
        if last_byte == 0 {
            return Err(OxiArcError::corrupted(
                0,
                "FSE bitstream has no sentinel bit (last byte is zero)",
            ));
        }

        // The sentinel is the highest set bit of the last byte; data bits sit
        // below it (and in all preceding bytes).
        let sentinel_pos = 7 - last_byte.leading_zeros() as i64;
        let bits_remaining = (data.len() as i64 - 1) * 8 + sentinel_pos;

        Ok(Self {
            data,
            bits_remaining,
        })
    }

    /// Number of unread data bits. Negative if the stream was over-read.
    pub fn bits_remaining(&self) -> i64 {
        self.bits_remaining
    }

    /// Whether more bits were consumed than the stream contains.
    pub fn is_overflowed(&self) -> bool {
        self.bits_remaining < 0
    }

    /// Whether the stream was consumed exactly (all data bits read, no overflow).
    pub fn is_finished(&self) -> bool {
        self.bits_remaining == 0
    }

    /// Peek `n` bits (n <= 32) below the current position without consuming.
    ///
    /// If fewer than `n` bits remain, the missing low bits read as zero
    /// (mirroring the reference container behaviour near the stream start).
    #[inline]
    pub fn peek_bits(&self, n: u8) -> u32 {
        debug_assert!(n <= 32);
        if n == 0 {
            return 0;
        }
        let end = self.bits_remaining;
        if end <= 0 {
            return 0;
        }
        let start = end - n as i64;
        let lo = start.max(0);
        let first_byte = (lo / 8) as usize;
        // end >= 1 here, so (end - 1) is a valid bit index.
        let last_byte = ((end - 1) / 8) as usize;

        // Gather the covering bytes little-endian (at most 5 for n <= 32).
        let mut window = 0u64;
        for (k, idx) in (first_byte..=last_byte).enumerate() {
            if let Some(&b) = self.data.get(idx) {
                window |= (b as u64) << (8 * k);
            }
        }

        let shift = (lo - first_byte as i64 * 8) as u32;
        let avail = (end - lo) as u32; // 1..=32
        let field = (window >> shift) & ((1u64 << avail) - 1);
        // Bits below the stream start are zero: shift the real bits up.
        let result = if start < 0 {
            field << ((-start) as u32)
        } else {
            field
        };
        result as u32
    }

    /// Consume `n` bits without returning them.
    #[inline]
    pub fn skip_bits(&mut self, n: u8) {
        self.bits_remaining -= n as i64;
    }

    /// Read `n` bits (n <= 32) from the stream.
    ///
    /// Over-reads return zero-padded values and mark the reader overflowed.
    #[inline]
    pub fn read_bits(&mut self, n: u8) -> u32 {
        let value = self.peek_bits(n);
        self.bits_remaining -= n as i64;
        value
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
    pub fn decode(&mut self, reader: &mut FseBitReader) -> Result<u8> {
        let entry = self.table.get(self.state)?;
        let symbol = entry.symbol;

        // Calculate next state
        let bits = reader.read_bits(entry.num_bits);
        self.state = entry.baseline as usize + bits as usize;

        Ok(symbol)
    }

    /// Peek at current symbol without advancing.
    pub fn peek(&self) -> Result<u8> {
        Ok(self.table.get(self.state)?.symbol)
    }
}

/// Read an FSE table description from a forward bitstream (RFC 8878 §4.1.1).
///
/// * `max_symbol` — largest symbol value permitted for this table.
/// * `max_log` — largest accuracy log permitted for this table (9 for
///   literal/match lengths, 8 for offsets, 6 for Huffman weights).
///
/// Returns the decoding table and the number of header bytes consumed.
pub fn read_fse_table_description(
    data: &[u8],
    max_symbol: u8,
    max_log: u8,
) -> Result<(FseTable, usize)> {
    if data.is_empty() {
        return Err(OxiArcError::corrupted(0, "empty FSE table description"));
    }

    let mut bit_pos = 0usize;

    // Read accuracy log (4 bits + 5)
    let accuracy_log = read_bits_forward(data, &mut bit_pos, 4)? as u8 + 5;

    if accuracy_log > max_log {
        return Err(OxiArcError::corrupted(
            0,
            format!(
                "accuracy log {} exceeds maximum {} for this table",
                accuracy_log, max_log
            ),
        ));
    }

    let table_size = 1i32 << accuracy_log;
    // `remaining` follows the RFC/reference convention: starts at
    // table_size + 1 and must end at exactly 1.
    let mut remaining = table_size + 1;
    let mut probabilities: Vec<i16> = Vec::with_capacity(max_symbol as usize + 1);

    while remaining > 1 {
        if probabilities.len() > max_symbol as usize {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "FSE table description has more than {} symbols",
                    max_symbol as usize + 1
                ),
            ));
        }

        // Variable-length probability read (see FSE_readNCount):
        // threshold = largest power of two <= remaining.
        let nb_bits = highest_bit_set(remaining as u16) + 1;
        let threshold = 1u32 << (nb_bits - 1);
        let max_small = (2 * threshold - 1) - remaining as u32;

        let low_value = read_bits_forward(data, &mut bit_pos, nb_bits - 1)?;
        let value = if low_value < max_small {
            low_value
        } else {
            let high_bit = read_bits_forward(data, &mut bit_pos, 1)?;
            let full = low_value + (high_bit << (nb_bits - 1));
            if full >= threshold {
                full - max_small
            } else {
                full
            }
        };

        // Convert to probability (-1, 0, or positive)
        let prob = value as i16 - 1;

        if prob != 0 {
            remaining -= if prob == -1 { 1 } else { prob as i32 };
            if remaining < 1 {
                return Err(OxiArcError::corrupted(
                    0,
                    "FSE table probabilities exceed table size",
                ));
            }
        }

        probabilities.push(prob);

        // A zero probability is followed by 2-bit repeat-zero flags.
        if prob == 0 {
            loop {
                let repeat = read_bits_forward(data, &mut bit_pos, 2)?;
                let new_len = probabilities.len() + repeat as usize;
                if new_len > max_symbol as usize + 1 {
                    return Err(OxiArcError::corrupted(
                        0,
                        "FSE repeat-zero run exceeds maximum symbol",
                    ));
                }
                probabilities.resize(new_len, 0);
                if repeat < 3 {
                    break;
                }
            }
        }
    }

    if remaining != 1 {
        return Err(OxiArcError::corrupted(
            0,
            "FSE table description does not sum to table size",
        ));
    }

    // Pad to byte boundary
    let bytes_consumed = bit_pos.div_ceil(8);
    if bytes_consumed > data.len() {
        return Err(OxiArcError::corrupted(
            bytes_consumed as u64,
            "FSE table description overruns input",
        ));
    }

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
        return Err(OxiArcError::corrupted(
            byte_pos as u64,
            "unexpected end of FSE data",
        ));
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
        let table = FseTable::new(4, &probs).expect("valid FSE operation");

        assert_eq!(table.accuracy_log(), 4);
        assert_eq!(table.size(), 16);
    }

    #[test]
    fn test_fse_table_with_less_than_one() {
        // Mix of normal and less-than-one probabilities
        let probs = [8i16, 4, 2, 1, -1]; // Total = 15 + 1 = 16
        let table = FseTable::new(4, &probs).expect("valid FSE operation");

        assert_eq!(table.size(), 16);
    }

    #[test]
    fn test_fse_table_bad_sum_rejected() {
        // Sums to 15, table size is 16 -> corrupt.
        let probs = [8i16, 4, 2, 1];
        assert!(FseTable::new(4, &probs).is_err());
        // Sums to 17 -> corrupt.
        let probs = [8i16, 4, 2, 2, 1];
        assert!(FseTable::new(4, &probs).is_err());
    }

    #[test]
    fn test_fse_table_get_out_of_range() {
        let probs = [4i16, 4, 4, 4];
        let table = FseTable::new(4, &probs).expect("valid FSE operation");
        assert!(table.get(15).is_ok());
        assert!(table.get(16).is_err());
        assert!(table.get(usize::MAX).is_err());
    }

    #[test]
    fn test_read_bits_forward() {
        let data = [0b10110100, 0b11001010];
        let mut bit_pos = 0;

        assert_eq!(
            read_bits_forward(&data, &mut bit_pos, 4).expect("valid FSE operation"),
            0b0100
        );
        assert_eq!(
            read_bits_forward(&data, &mut bit_pos, 4).expect("valid FSE operation"),
            0b1011
        );
        assert_eq!(
            read_bits_forward(&data, &mut bit_pos, 4).expect("valid FSE operation"),
            0b1010
        );
    }

    #[test]
    fn test_bit_reader_zstd_layout() {
        // Stream built by hand following zstd conventions:
        // fields written LSB-first in order A=0b101 (3 bits), B=0b01 (2 bits),
        // C=0b1111 (4 bits); then sentinel. Bit layout (positions 0..9):
        //   pos 0..2 = A, pos 3..4 = B, pos 5..8 = C, sentinel at pos 9.
        // byte0 = A | B<<3 | (C&0b111)<<5 ; byte1 = C>>3 | 1<<1.
        let byte0: u8 = 0b101 | (0b01 << 3) | ((0b1111 & 0b111) << 5);
        let byte1: u8 = (0b1111 >> 3) | (1 << 1);
        let data = [byte0, byte1];

        let mut reader = FseBitReader::new(&data).expect("valid stream");
        assert_eq!(reader.bits_remaining(), 9);
        // Decoder reads in REVERSE write order: C first, then B, then A.
        assert_eq!(reader.read_bits(4), 0b1111);
        assert_eq!(reader.read_bits(2), 0b01);
        assert_eq!(reader.read_bits(3), 0b101);
        assert!(reader.is_finished());
    }

    #[test]
    fn test_bit_reader_overflow_pads_zero() {
        // Single byte 0b0000_0100: sentinel at bit 2, so 2 data bits (0b00).
        let data = [0b0000_0100u8];
        let mut reader = FseBitReader::new(&data).expect("valid stream");
        assert_eq!(reader.bits_remaining(), 2);
        // Read 4 bits: 2 real (high side) + 2 zero-padded low bits.
        let v = reader.read_bits(4);
        assert_eq!(v & 0b11, 0, "low bits padded with zero");
        assert!(reader.is_overflowed());
    }

    #[test]
    fn test_bit_reader_rejects_zero_last_byte() {
        assert!(FseBitReader::new(&[0x12, 0x00]).is_err());
        assert!(FseBitReader::new(&[]).is_err());
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
        // The decoder reads fields in reverse write order.
        let v3 = reader.read_bits(8);
        let v2 = reader.read_bits(5);
        let v1 = reader.read_bits(6);

        assert_eq!(v1, 42, "first written value read last");
        assert_eq!(v2, 7, "second value");
        assert_eq!(v3, 100, "third written value read first");
        assert!(reader.is_finished());
    }
}

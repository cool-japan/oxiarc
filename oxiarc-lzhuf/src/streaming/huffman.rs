//! Streaming bit reader and Huffman tree for LZH decompression.
//!
//! Contains the low-level bit I/O primitives and Huffman lookup table
//! used by the streaming LZH decoder.

/// Maximum code length for LZH Huffman codes.
pub(super) const MAX_CODE_LENGTH: usize = 16;

// ============================================================================
// Streaming Bit Reader
// ============================================================================

/// A bit reader for streaming decompression that works with byte slices.
///
/// Unlike `BitReader<R: Read>`, this reader:
/// - Works with fixed byte slices, not streams
/// - Can report how much input was consumed
/// - Supports saving/restoring state for resumption
#[derive(Debug, Clone)]
pub struct StreamingBitReader {
    /// Bit buffer (LSB-first).
    buffer: u64,
    /// Number of valid bits in buffer.
    bits_in_buffer: u8,
    /// Current position in input slice.
    input_pos: usize,
    /// Total bits consumed (for progress tracking).
    total_bits_consumed: u64,
}

impl Default for StreamingBitReader {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingBitReader {
    /// Create a new streaming bit reader.
    pub fn new() -> Self {
        Self {
            buffer: 0,
            bits_in_buffer: 0,
            input_pos: 0,
            total_bits_consumed: 0,
        }
    }

    /// Reset the reader state for a new input slice.
    pub fn reset_for_new_input(&mut self) {
        self.input_pos = 0;
    }

    /// Get the number of bytes consumed from the current input.
    pub fn bytes_consumed(&self) -> usize {
        self.input_pos
    }

    /// Get bits currently available in buffer.
    pub fn bits_available(&self) -> u8 {
        self.bits_in_buffer
    }

    /// Read up to 32 bits from the stream.
    /// Returns None if not enough bits are available.
    pub fn read_bits(&mut self, input: &[u8], count: u8) -> Option<u32> {
        debug_assert!(count <= 32, "Cannot read more than 32 bits at once");

        if count == 0 {
            return Some(0);
        }

        // Try to fill buffer if needed
        while self.bits_in_buffer < count && self.input_pos < input.len() {
            self.buffer |= (input[self.input_pos] as u64) << self.bits_in_buffer;
            self.bits_in_buffer += 8;
            self.input_pos += 1;
        }

        if self.bits_in_buffer < count {
            return None; // Not enough input
        }

        // Extract bits from buffer
        let mask = (1u64 << count).wrapping_sub(1);
        let result = (self.buffer & mask) as u32;

        // Remove read bits from buffer
        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_consumed += count as u64;

        Some(result)
    }

    /// Peek at up to 32 bits without consuming them.
    /// Returns None if not enough bits are available.
    pub fn peek_bits(&mut self, input: &[u8], count: u8) -> Option<u32> {
        debug_assert!(count <= 32, "Cannot peek more than 32 bits at once");

        if count == 0 {
            return Some(0);
        }

        // Try to fill buffer if needed
        while self.bits_in_buffer < count && self.input_pos < input.len() {
            self.buffer |= (input[self.input_pos] as u64) << self.bits_in_buffer;
            self.bits_in_buffer += 8;
            self.input_pos += 1;
        }

        if self.bits_in_buffer < count {
            return None;
        }

        let mask = (1u64 << count).wrapping_sub(1);
        Some((self.buffer & mask) as u32)
    }

    /// Skip a number of bits.
    pub fn skip_bits(&mut self, count: u8) {
        if count == 0 || self.bits_in_buffer < count {
            return;
        }

        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_consumed += count as u64;
    }

    /// Read a single bit.
    pub fn read_bit(&mut self, input: &[u8]) -> Option<bool> {
        self.read_bits(input, 1).map(|b| b != 0)
    }

    /// Save the current state for potential rollback.
    pub fn save_state(&self) -> BitReaderState {
        BitReaderState {
            buffer: self.buffer,
            bits_in_buffer: self.bits_in_buffer,
            input_pos: self.input_pos,
            total_bits_consumed: self.total_bits_consumed,
        }
    }

    /// Restore a previously saved state.
    pub fn restore_state(&mut self, state: BitReaderState) {
        self.buffer = state.buffer;
        self.bits_in_buffer = state.bits_in_buffer;
        self.input_pos = state.input_pos;
        self.total_bits_consumed = state.total_bits_consumed;
    }
}

/// Saved state of a StreamingBitReader for rollback.
#[derive(Debug, Clone, Copy)]
pub struct BitReaderState {
    pub(super) buffer: u64,
    pub(super) bits_in_buffer: u8,
    pub(super) input_pos: usize,
    pub(super) total_bits_consumed: u64,
}

// ============================================================================
// Huffman Tree for Streaming
// ============================================================================

/// Entry in the Huffman lookup table.
#[derive(Debug, Clone, Copy)]
pub(super) struct TableEntry(i32);

impl TableEntry {
    pub(super) const INVALID: TableEntry = TableEntry(-1);

    pub(super) fn new(symbol: u16, length: u8) -> Self {
        TableEntry(((length as i32) << 16) | (symbol as i32))
    }

    pub(super) fn is_valid(self) -> bool {
        self.0 >= 0
    }

    pub(super) fn symbol(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    pub(super) fn length(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }
}

/// Streaming Huffman tree for decoding.
#[derive(Debug, Clone)]
pub struct StreamingHuffmanTree {
    /// Lookup table for fast decoding.
    pub(super) table: Vec<TableEntry>,
    /// Table bits for fast lookup.
    pub(super) table_bits: u8,
    /// Maximum code length.
    pub(super) max_length: u8,
}

impl StreamingHuffmanTree {
    /// Create a Huffman tree from code lengths.
    pub fn from_lengths(lengths: &[u8], table_bits: u8) -> oxiarc_core::error::Result<Self> {
        let table_size = 1 << table_bits;
        let mut table = vec![TableEntry::INVALID; table_size];

        if lengths.is_empty() {
            return Ok(Self {
                table,
                table_bits,
                max_length: 0,
            });
        }

        // Find max length
        let max_length = lengths.iter().copied().max().unwrap_or(0);
        if max_length == 0 {
            return Ok(Self {
                table,
                table_bits,
                max_length: 0,
            });
        }

        // Count codes of each length
        let mut bl_count = [0u32; MAX_CODE_LENGTH + 1];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Calculate starting codes
        let mut next_code = [0u32; MAX_CODE_LENGTH + 1];
        let mut code = 0u32;
        for bits in 1..=max_length as usize {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Build lookup table
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 && len <= table_bits {
                let len_usize = len as usize;
                let code = next_code[len_usize];
                next_code[len_usize] += 1;

                // Fill table entries (reversed for LSB-first)
                let reversed = Self::reverse_bits(code as u16, len);
                let fill_count = 1 << (table_bits as usize - len_usize);

                for i in 0..fill_count {
                    let index = reversed as usize | (i << len_usize);
                    if index < table_size {
                        table[index] = TableEntry::new(symbol as u16, len);
                    }
                }
            }
        }

        Ok(Self {
            table,
            table_bits,
            max_length,
        })
    }

    /// Reverse bits.
    fn reverse_bits(mut value: u16, length: u8) -> u16 {
        let mut result = 0u16;
        for _ in 0..length {
            result = (result << 1) | (value & 1);
            value >>= 1;
        }
        result
    }

    /// Decode a symbol using the streaming bit reader.
    /// Returns None if not enough input is available.
    pub fn decode(&self, reader: &mut StreamingBitReader, input: &[u8]) -> Option<u16> {
        if self.max_length == 0 {
            return None;
        }

        // Try to peek `table_bits` for the fast-path table lookup.
        // Track how many bits are actually available so the fallback path can
        // correctly reject table entries that require more bits than we have.
        //
        // BUG RATIONALE: In the fallback path, we peek fewer than `table_bits`
        // bits.  The table entry for those bits may have `entry.length() >
        // available` — i.e. the symbol's canonical code is longer than what we
        // peeked.  Accepting such an entry and calling `skip_bits(entry.length())`
        // when `bits_in_buffer < entry.length()` causes skip_bits to silently
        // NOP (it guards on `bits_in_buffer < count`), leaving bits unconsumed
        // and corrupting the bit stream.  We must only accept the entry when
        // `entry.length() <= available_bits`.
        let (bits, available_bits) = match reader.peek_bits(input, self.table_bits) {
            Some(b) => (b, self.table_bits),
            None => {
                // Try progressively fewer bits
                let mut available = 0u8;
                for i in 1..=self.table_bits {
                    if reader.peek_bits(input, i).is_some() {
                        available = i;
                    } else {
                        break;
                    }
                }
                if available == 0 {
                    return None;
                }
                // Pad the peeked bits to `table_bits` with zeros (high bits).
                // Table entries for short codes fill ALL slots differing only in
                // the high bits, so a zero-padded index still hits the right entry.
                let b = reader.peek_bits(input, available)?;
                (b, available)
            }
        };

        let entry = self.table[bits as usize];

        // Only accept the entry when we actually have enough bits for the code.
        // If `entry.length() > available_bits` the decode is ambiguous — return
        // None to signal "need more input".
        if entry.is_valid() && entry.length() <= available_bits {
            reader.skip_bits(entry.length());
            Some(entry.symbol())
        } else {
            None
        }
    }
}

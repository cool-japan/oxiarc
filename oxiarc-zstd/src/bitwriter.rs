//! Bitstream writers for Zstandard encoding.
//!
//! Zstandard uses two bitstream directions:
//! - **Forward bitstream** (LSB first): used for FSE table descriptions, literals headers,
//!   and other metadata fields.
//! - **Backward bitstream**: used for FSE sequence encoding, where the last symbol written
//!   is the first one read during decoding. The output bytes are stored in reverse order
//!   with a sentinel bit marking the start of data.

/// Forward bitstream writer (LSB first).
///
/// Used for writing FSE table descriptions, various headers, and other
/// forward-direction bitstream data in Zstandard frames.
///
/// Bits are packed into bytes starting from the least significant bit.
/// When a byte is full, it is flushed to the output buffer and the
/// accumulator resets.
pub struct ForwardBitWriter {
    /// Accumulated output bytes.
    output: Vec<u8>,
    /// Current byte being assembled (bits accumulated so far).
    current_byte: u8,
    /// Number of valid bits in `current_byte` (0..8).
    bits_in_current: u8,
}

impl ForwardBitWriter {
    /// Create a new forward bitstream writer.
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            current_byte: 0,
            bits_in_current: 0,
        }
    }

    /// Create a new forward bitstream writer with a capacity hint.
    pub fn with_capacity(byte_capacity: usize) -> Self {
        Self {
            output: Vec::with_capacity(byte_capacity),
            current_byte: 0,
            bits_in_current: 0,
        }
    }

    /// Write `num_bits` bits from `value` (LSB first, up to 25 bits).
    ///
    /// The lowest `num_bits` bits of `value` are written to the stream.
    /// Bits are packed into bytes starting from the least significant bit.
    ///
    /// # Panics
    ///
    /// Panics if `num_bits` exceeds 25.
    pub fn write_bits(&mut self, value: u32, num_bits: u8) {
        debug_assert!(
            num_bits <= 25,
            "ForwardBitWriter supports up to 25 bits per call"
        );

        if num_bits == 0 {
            return;
        }

        // Mask off any extraneous high bits from value.
        let mask = if num_bits >= 32 {
            u32::MAX
        } else {
            (1u32 << num_bits) - 1
        };
        let masked_value = value & mask;

        // Pack bits into current_byte, flushing full bytes as we go.
        let mut remaining_bits = num_bits;
        let mut bits_to_write = masked_value;

        while remaining_bits > 0 {
            let space_in_current = 8 - self.bits_in_current;
            let take = remaining_bits.min(space_in_current);

            // Extract the lowest `take` bits from bits_to_write.
            let take_mask = if take >= 32 {
                u32::MAX
            } else {
                (1u32 << take) - 1
            };
            let chunk = (bits_to_write & take_mask) as u8;

            // Place them at the correct position in current_byte.
            self.current_byte |= chunk << self.bits_in_current;
            self.bits_in_current += take;

            // Advance past the bits we consumed.
            bits_to_write >>= take;
            remaining_bits -= take;

            // If the byte is full, flush it.
            if self.bits_in_current == 8 {
                self.output.push(self.current_byte);
                self.current_byte = 0;
                self.bits_in_current = 0;
            }
        }
    }

    /// Write a single bit (0 or 1).
    pub fn write_bit(&mut self, bit: bool) {
        self.write_bits(if bit { 1 } else { 0 }, 1);
    }

    /// Flush remaining bits, padding with zeros to the next byte boundary.
    ///
    /// Consumes the writer and returns the accumulated output bytes.
    /// If there are any pending bits that do not fill a complete byte,
    /// they are padded with zeros in the high bits.
    pub fn finish(mut self) -> Vec<u8> {
        if self.bits_in_current > 0 {
            // Pad remaining bits with zeros (already zero from initialization).
            self.output.push(self.current_byte);
        }
        self.output
    }

    /// Current bit position (total number of bits written so far).
    pub fn bit_position(&self) -> usize {
        self.output.len() * 8 + self.bits_in_current as usize
    }

    /// Current byte length of the output (not counting partial byte).
    pub fn byte_len(&self) -> usize {
        self.output.len()
    }

    /// Whether no bits have been written yet.
    pub fn is_empty(&self) -> bool {
        self.output.is_empty() && self.bits_in_current == 0
    }

    /// Get a reference to the bytes written so far (not including partial byte).
    pub fn as_bytes(&self) -> &[u8] {
        &self.output
    }
}

impl Default for ForwardBitWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Backward bitstream writer for FSE/Huffman encoding (RFC 8878).
///
/// Mirrors the reference `BIT_addBits` / `BIT_closeCStream` semantics:
/// bit-fields are appended least-significant-bit first into a little-endian
/// bit sequence, and `finish()` terminates the stream with a single `1`
/// sentinel bit, zero-padding to a byte boundary.
///
/// The paired `crate::fse::FseBitReader` starts at the sentinel and reads
/// fields back in **reverse write order** (last written = first read), which
/// is why sequence encoders emit their data back-to-front.
pub struct BackwardBitWriter {
    /// Completed output bytes.
    output: Vec<u8>,
    /// Bit accumulator; bit 0 is the next position to fill.
    container: u64,
    /// Number of pending bits in `container` (0..8 after each write).
    bits_in_container: u8,
}

impl BackwardBitWriter {
    /// Create a new backward bitstream writer.
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            container: 0,
            bits_in_container: 0,
        }
    }

    /// Create a new backward bitstream writer with a capacity hint.
    pub fn with_capacity(byte_capacity: usize) -> Self {
        Self {
            output: Vec::with_capacity(byte_capacity),
            container: 0,
            bits_in_container: 0,
        }
    }

    /// Write the lowest `num_bits` bits of `value` (up to 56 per call).
    ///
    /// The first field written ends up nearest byte 0 and is therefore the
    /// **last** field the decoder reads.
    pub fn write_bits(&mut self, value: u64, num_bits: u8) {
        debug_assert!(num_bits <= 56, "BackwardBitWriter supports up to 56 bits");
        if num_bits == 0 {
            return;
        }

        let mask = if num_bits >= 64 {
            u64::MAX
        } else {
            (1u64 << num_bits) - 1
        };
        self.container |= (value & mask) << self.bits_in_container;
        self.bits_in_container += num_bits;

        // Flush completed bytes.
        while self.bits_in_container >= 8 {
            self.output.push((self.container & 0xFF) as u8);
            self.container >>= 8;
            self.bits_in_container -= 8;
        }
    }

    /// Write a single bit (0 or 1).
    pub fn write_bit(&mut self, bit: bool) {
        self.write_bits(u64::from(bit), 1);
    }

    /// Finalize the stream: append the sentinel `1` bit and pad with zeros to
    /// a byte boundary. An empty stream yields `[0x01]` (sentinel only).
    pub fn finish(mut self) -> Vec<u8> {
        // Sentinel bit marks the end of data (start of decoder reads).
        self.container |= 1u64 << self.bits_in_container;
        self.bits_in_container += 1;
        while self.bits_in_container > 0 {
            self.output.push((self.container & 0xFF) as u8);
            self.container >>= 8;
            self.bits_in_container = self.bits_in_container.saturating_sub(8);
        }
        self.output
    }

    /// Number of data bits written so far (excludes sentinel).
    pub fn len(&self) -> usize {
        self.output.len() * 8 + self.bits_in_container as usize
    }

    /// Whether no bits have been written yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for BackwardBitWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_forward_empty() {
        let writer = ForwardBitWriter::new();
        assert!(writer.is_empty());
        assert_eq!(writer.bit_position(), 0);
        let output = writer.finish();
        assert!(output.is_empty());
    }

    #[test]
    fn test_forward_single_byte() {
        let mut writer = ForwardBitWriter::new();
        writer.write_bits(0xAB, 8);
        assert_eq!(writer.bit_position(), 8);
        let output = writer.finish();
        assert_eq!(output, vec![0xAB]);
    }

    #[test]
    fn test_forward_partial_byte() {
        let mut writer = ForwardBitWriter::new();
        // Write 3 bits: binary 101 = 5
        writer.write_bits(5, 3);
        assert_eq!(writer.bit_position(), 3);
        let output = writer.finish();
        // Should be padded: 0b00000_101 = 0x05
        assert_eq!(output, vec![0x05]);
    }

    #[test]
    fn test_forward_multi_byte() {
        let mut writer = ForwardBitWriter::new();
        // Write 12 bits: 0xABC & 0xFFF = 0xABC
        // LSB first: low 8 bits = 0xBC, then high 4 bits = 0x0A
        writer.write_bits(0xABC, 12);
        let output = writer.finish();
        assert_eq!(output, vec![0xBC, 0x0A]);
    }

    #[test]
    fn test_forward_cross_byte_boundary() {
        let mut writer = ForwardBitWriter::new();
        writer.write_bits(0x07, 3); // bits: 111
        writer.write_bits(0x1F, 5); // bits: 11111
        // Combined: 11111_111 = 0xFF
        let output = writer.finish();
        assert_eq!(output, vec![0xFF]);
    }

    #[test]
    fn test_forward_multiple_writes() {
        let mut writer = ForwardBitWriter::new();
        writer.write_bits(1, 1); // bit 0: 1
        writer.write_bits(0, 1); // bit 1: 0
        writer.write_bits(1, 1); // bit 2: 1
        writer.write_bits(0, 1); // bit 3: 0
        writer.write_bits(1, 1); // bit 4: 1
        writer.write_bits(0, 1); // bit 5: 0
        writer.write_bits(1, 1); // bit 6: 1
        writer.write_bits(0, 1); // bit 7: 0
        // Binary: 01010101 = 0x55
        let output = writer.finish();
        assert_eq!(output, vec![0x55]);
    }

    #[test]
    fn test_forward_write_bit() {
        let mut writer = ForwardBitWriter::new();
        for _ in 0..8 {
            writer.write_bit(true);
        }
        let output = writer.finish();
        assert_eq!(output, vec![0xFF]);
    }

    #[test]
    fn test_forward_zero_bits() {
        let mut writer = ForwardBitWriter::new();
        writer.write_bits(0xFF, 0); // Should write nothing
        assert!(writer.is_empty());
        let output = writer.finish();
        assert!(output.is_empty());
    }

    #[test]
    fn test_forward_25_bits() {
        let mut writer = ForwardBitWriter::new();
        let val = (1u32 << 25) - 1; // 25 bits all ones
        writer.write_bits(val, 25);
        assert_eq!(writer.bit_position(), 25);
        let output = writer.finish();
        // 25 bits = 3 full bytes (24 bits) + 1 partial byte (1 bit)
        assert_eq!(output.len(), 4);
        assert_eq!(output[0], 0xFF);
        assert_eq!(output[1], 0xFF);
        assert_eq!(output[2], 0xFF);
        assert_eq!(output[3], 0x01); // 1 bit set, padded
    }

    #[test]
    fn test_backward_empty() {
        let writer = BackwardBitWriter::new();
        assert!(writer.is_empty());
        assert_eq!(writer.len(), 0);
        let output = writer.finish();
        // Sentinel-only byte.
        assert_eq!(output, vec![0x01]);
    }

    #[test]
    fn test_backward_single_bit() {
        let mut writer = BackwardBitWriter::new();
        writer.write_bit(true);
        let output = writer.finish();
        // 1 data bit = 1, sentinel_data_bits = 1 mod 8 = 1.
        // sentinel = 1 | (1 << 1) = 0x03
        assert_eq!(output, vec![0x03]);
    }

    #[test]
    fn test_backward_single_byte_data() {
        let mut writer = BackwardBitWriter::new();
        // Write 8 bits of data: 0xAB
        writer.write_bits(0xAB, 8);
        let output = writer.finish();
        // 8 data bits: sentinel gets 0 data bits (8 mod 8 = 0).
        // 1 full byte = 0xAB at index 0, sentinel 0x01 at index 1.
        assert_eq!(output, vec![0xAB, 0x01]);
    }

    #[test]
    fn test_backward_partial_bits() {
        let mut writer = BackwardBitWriter::new();
        // Write 5 bits: 0b10110 = 22
        writer.write_bits(22, 5);
        let output = writer.finish();
        // 5 data bits, 0 full bytes, sentinel gets all 5 bits.
        // sentinel = 22 | (1 << 5) = 0x36
        assert_eq!(output, vec![0x36]);
    }

    #[test]
    fn test_backward_multi_byte() {
        let mut writer = BackwardBitWriter::new();
        writer.write_bits(0xFF, 8);
        writer.write_bits(0xAA, 8);
        let output = writer.finish();
        // Little-endian bit sequence: first field written occupies byte 0,
        // second field byte 1, sentinel-only final byte (16 % 8 == 0).
        // The decoder reads 0xAA first, then 0xFF (reverse write order).
        assert_eq!(output, vec![0xFF, 0xAA, 0x01]);
    }

    #[test]
    fn test_backward_len() {
        let mut writer = BackwardBitWriter::new();
        writer.write_bits(0, 3);
        assert_eq!(writer.len(), 3);
        writer.write_bits(0, 10);
        assert_eq!(writer.len(), 13);
    }

    #[test]
    fn test_backward_zero_bits() {
        let mut writer = BackwardBitWriter::new();
        writer.write_bits(0xFF, 0); // Should write nothing
        assert!(writer.is_empty());
    }

    #[test]
    fn test_forward_with_capacity() {
        let writer = ForwardBitWriter::with_capacity(128);
        assert!(writer.is_empty());
        let output = writer.finish();
        assert!(output.is_empty());
    }

    #[test]
    fn test_backward_with_capacity() {
        let writer = BackwardBitWriter::with_capacity(128);
        assert!(writer.is_empty());
    }
}

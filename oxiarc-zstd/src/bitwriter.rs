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

/// Backward bitstream writer for FSE sequence encoding.
///
/// Produces a byte array compatible with the `FseBitReader`:
/// - The last byte contains a sentinel (highest set bit) and the first data bits.
/// - Preceding bytes contain subsequent data bits, with byte at index N-2
///   being read after the sentinel byte, N-3 after that, etc.
///
/// The encoder writes bits in the same order the decoder reads them
/// (first written = first decoded).
///
/// Internally, bits are accumulated into a `Vec<u8>` from MSB of the highest
/// byte down to LSB of byte 0. At `finish()`, a sentinel is added and the
/// output is ready for the decoder.
pub struct BackwardBitWriter {
    /// All data bits collected in a flat bit vector. We track them from the
    /// "first written" end so that we can serialize in the order the reader
    /// expects.
    data_bits: Vec<u8>,
    /// Total number of data bits written.
    total_bits: usize,
}

impl BackwardBitWriter {
    /// Create a new backward bitstream writer.
    pub fn new() -> Self {
        Self {
            data_bits: Vec::new(),
            total_bits: 0,
        }
    }

    /// Create a new backward bitstream writer with a capacity hint.
    pub fn with_capacity(byte_capacity: usize) -> Self {
        Self {
            data_bits: Vec::with_capacity(byte_capacity * 8),
            total_bits: 0,
        }
    }

    /// Write `num_bits` bits from `value` into the backward stream.
    ///
    /// The lowest `num_bits` bits of `value` are appended. The first call's
    /// bits will be the first bits the decoder reads.
    pub fn write_bits(&mut self, value: u64, num_bits: u8) {
        if num_bits == 0 {
            return;
        }

        // Store individual bits (LSB of value first).
        for i in 0..num_bits {
            let bit = ((value >> i) & 1) as u8;
            self.data_bits.push(bit);
        }
        self.total_bits += num_bits as usize;
    }

    /// Write a single bit (0 or 1).
    pub fn write_bit(&mut self, bit: bool) {
        self.data_bits.push(if bit { 1 } else { 0 });
        self.total_bits += 1;
    }

    /// Finalize the backward bitstream.
    ///
    /// Produces a byte array where:
    /// - The last byte contains the sentinel and the first data bits.
    /// - Preceding bytes (read from index N-2 down to 0) contain later data bits.
    ///
    /// The `FseBitReader` loads the sentinel byte's data bits first (into the
    /// accumulator's LSB), then loads byte N-2, N-3, ..., 0 into successively
    /// higher accumulator positions.
    ///
    /// Returns the finalized byte vector. If no bits were written, returns `[0x01]`.
    pub fn finish(self) -> Vec<u8> {
        if self.data_bits.is_empty() {
            return vec![0x01];
        }

        // The FseBitReader reads:
        //   1. Sentinel byte (last byte): data bits below sentinel loaded first (LSB of accumulator)
        //   2. Byte at index N-2: loaded into bits above sentinel data
        //   3. Byte at index N-3: loaded above that
        //   ...
        //   N. Byte at index 0: loaded into highest positions
        //
        // So the first data bits go into the sentinel byte, next 8 bits into byte N-2,
        // next 8 bits into byte N-3, etc.
        //
        // Build the output in reverse: start with byte 0, then byte 1, ..., then sentinel.

        let n = self.data_bits.len();

        // Figure out how many bits go into the sentinel byte.
        // The sentinel byte can hold up to 7 data bits (bits 0-6, sentinel at bit 7 max).
        // If total bits mod 8 == 0, sentinel gets 0 data bits (sentinel-only byte).
        // If total bits mod 8 == k (1..7), sentinel gets k data bits.
        // Actually, we need the total bits to decompose into: sentinel_bits + full_bytes * 8.
        // sentinel_bits can be 0..7. If 0, we need an extra sentinel-only byte.

        // Pack the data bits into bytes. The reader reads:
        //   sentinel_data (first S data bits, S=0..7), then
        //   byte N-2 (next 8 bits), byte N-3 (next 8), ..., byte 0 (last 8 bits).
        //
        // So byte 0 has the LAST 8 data bits, byte 1 has the second-to-last 8, etc.

        let sentinel_data_bits = n % 8;
        let full_bytes = n / 8;

        // Build from byte 0 (which has the last 8 data bits) to the sentinel byte.
        let mut output = Vec::with_capacity(full_bytes + 1);

        // Byte 0 has data bits at indices [n - 8, n - 1] (the last 8 data bits).
        // Byte 1 has data bits at indices [n - 16, n - 9].
        // ...
        // Byte k has data bits at indices [n - 8*(k+1), n - 8*k - 1].
        //
        // If sentinel_data_bits > 0, the sentinel covers indices [0, sentinel_data_bits-1].
        // The remaining full_bytes cover indices [sentinel_data_bits, n-1].

        // Build full bytes: byte 0 = last 8, byte 1 = second-to-last 8, etc.
        for byte_idx in 0..full_bytes {
            // This byte covers data_bits starting at offset:
            // sentinel_data_bits + (full_bytes - 1 - byte_idx) * 8
            let start = sentinel_data_bits + (full_bytes - 1 - byte_idx) * 8;
            let mut byte_val = 0u8;
            for bit in 0..8 {
                if self.data_bits[start + bit] != 0 {
                    byte_val |= 1 << bit;
                }
            }
            output.push(byte_val);
        }

        // Build sentinel byte: first sentinel_data_bits of data_bits.
        let mut sentinel_byte = 0u8;
        for bit in 0..sentinel_data_bits {
            if self.data_bits[bit] != 0 {
                sentinel_byte |= 1 << bit;
            }
        }
        sentinel_byte |= 1 << sentinel_data_bits; // Sentinel bit
        output.push(sentinel_byte);

        output
    }

    /// Number of data bits written so far (excludes sentinel).
    pub fn len(&self) -> usize {
        self.total_bits
    }

    /// Whether no bits have been written yet.
    pub fn is_empty(&self) -> bool {
        self.total_bits == 0
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
        // 16 data bits, 2 full bytes, sentinel gets 0 data bits.
        // Byte 0 = last 8 data bits (0xAA), byte 1 = first 8 data bits (0xFF),
        // sentinel = 0x01.
        // Wait: data_bits in write order = [0xFF bits, 0xAA bits].
        // sentinel_data_bits = 16 % 8 = 0, full_bytes = 2.
        // byte_idx=0: start = 0 + (2-1-0)*8 = 8, data_bits[8..15] = 0xAA bits
        // byte_idx=1: start = 0 + (2-1-1)*8 = 0, data_bits[0..7] = 0xFF bits
        assert_eq!(output, vec![0xAA, 0xFF, 0x01]);
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

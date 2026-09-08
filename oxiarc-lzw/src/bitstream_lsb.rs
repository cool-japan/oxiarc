//! LSB-first bit writing for GIF LZW.
//!
//! GIF LZW packs codes LSB-first (Least Significant Bit first), which
//! differs from TIFF LZW's MSB-first packing. The *read* side lives in
//! [`crate::bits`], which extracts a code from a four-byte window without
//! keeping any state.

/// LSB-first bit writer for GIF LZW compression.
#[derive(Debug)]
pub struct LsbBitWriter {
    /// Internal accumulation buffer (up to 64 bits).
    buffer: u64,
    /// Number of valid bits in buffer (from LSB side).
    bits_in_buffer: usize,
    /// Completed output bytes.
    output: Vec<u8>,
}

impl LsbBitWriter {
    /// Create a new LSB bit writer.
    pub fn new() -> Self {
        Self {
            buffer: 0,
            bits_in_buffer: 0,
            output: Vec::new(),
        }
    }

    /// Write `bits` bits of `code` LSB-first into the output stream.
    ///
    /// The lowest `bits` bits of `code` are packed into the output,
    /// starting from the least significant position in the current byte.
    pub fn write_bits(&mut self, code: u16, bits: usize) {
        // Shift code into buffer at the current bit position (LSB-first).
        self.buffer |= (code as u64) << self.bits_in_buffer;
        self.bits_in_buffer += bits;

        // Flush any complete bytes.
        while self.bits_in_buffer >= 8 {
            self.output.push((self.buffer & 0xFF) as u8);
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
        }
    }

    /// Flush any remaining partial byte, padding with zero bits on the MSB side.
    pub fn flush(&mut self) {
        if self.bits_in_buffer > 0 {
            self.output.push((self.buffer & 0xFF) as u8);
            self.bits_in_buffer = 0;
            self.buffer = 0;
        }
    }

    /// Consume the writer and return the completed byte sequence.
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.flush();
        self.output
    }
}

impl Default for LsbBitWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsb_writer_packs_bits_low_first() {
        let mut writer = LsbBitWriter::new();
        writer.write_bits(0b101, 3);
        writer.write_bits(0b1100, 4);
        writer.write_bits(0b11111111, 8);

        let data = writer.into_bytes();
        // 15 bits: 0b101 | 0b1100 << 3 | 0xFF << 7 = 0x7FE5, low byte first
        assert_eq!(data, vec![0xE5, 0x7F]);
    }

    #[test]
    fn test_lsb_byte_boundary() {
        let mut writer = LsbBitWriter::new();
        writer.write_bits(0xAB, 8);

        let data = writer.into_bytes();
        assert_eq!(data, vec![0xAB]);
    }

    #[test]
    fn test_lsb_variable_widths() {
        // GIF-style codes of width 9 pack three bytes per two codes.
        let codes: &[u16] = &[256, 84, 79, 66, 69, 257];
        let mut writer = LsbBitWriter::new();
        for &c in codes {
            writer.write_bits(c, 9);
        }
        let data = writer.into_bytes();
        assert_eq!(data.len(), (codes.len() * 9).div_ceil(8));
    }
}

//! LSB-first bit stream operations for GIF LZW.
//!
//! GIF LZW uses LSB-first (Least Significant Bit first) bit ordering,
//! which differs from TIFF LZW that uses MSB-first.

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

/// LSB-first bit reader for GIF LZW decompression.
#[derive(Debug)]
pub struct LsbBitReader<'a> {
    /// Input data.
    data: &'a [u8],
    /// Current byte position in `data`.
    pos: usize,
    /// Internal accumulation buffer.
    buffer: u64,
    /// Number of valid bits currently in `buffer`.
    bits_in_buffer: usize,
}

impl<'a> LsbBitReader<'a> {
    /// Create a new LSB bit reader over the given byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            buffer: 0,
            bits_in_buffer: 0,
        }
    }

    /// Read `bits` bits from the stream, LSB-first.
    ///
    /// Returns `None` if the stream is exhausted before `bits` bits are
    /// available.
    pub fn read_bits(&mut self, bits: usize) -> Option<u16> {
        // Refill the buffer until we have enough bits.
        while self.bits_in_buffer < bits {
            if self.pos >= self.data.len() {
                return None;
            }
            // Pack the next byte into the buffer at the current top position.
            self.buffer |= (self.data[self.pos] as u64) << self.bits_in_buffer;
            self.bits_in_buffer += 8;
            self.pos += 1;
        }

        // Extract the lowest `bits` bits.
        let mask = (1u64 << bits) - 1;
        let code = (self.buffer & mask) as u16;
        self.buffer >>= bits;
        self.bits_in_buffer -= bits;

        Some(code)
    }

    /// Return `true` if all input bytes have been consumed and the internal
    /// buffer is empty (no bits remain).
    #[allow(dead_code)]
    pub fn is_exhausted(&self) -> bool {
        self.pos >= self.data.len() && self.bits_in_buffer == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lsb_write_read_roundtrip() {
        let mut writer = LsbBitWriter::new();
        writer.write_bits(0b101, 3);
        writer.write_bits(0b1100, 4);
        writer.write_bits(0b11111111, 8);

        let data = writer.into_bytes();

        let mut reader = LsbBitReader::new(&data);
        assert_eq!(reader.read_bits(3), Some(0b101));
        assert_eq!(reader.read_bits(4), Some(0b1100));
        assert_eq!(reader.read_bits(8), Some(0b11111111));
    }

    #[test]
    fn test_lsb_byte_boundary() {
        let mut writer = LsbBitWriter::new();
        writer.write_bits(0xAB, 8);

        let data = writer.into_bytes();
        assert_eq!(data, vec![0xAB]);

        let mut reader = LsbBitReader::new(&data);
        assert_eq!(reader.read_bits(8), Some(0xAB));
    }

    #[test]
    fn test_lsb_variable_widths() {
        // Simulate GIF-style codes of width 9
        let codes: &[u16] = &[256, 84, 79, 66, 69, 257];
        let mut writer = LsbBitWriter::new();
        for &c in codes {
            writer.write_bits(c, 9);
        }
        let data = writer.into_bytes();

        let mut reader = LsbBitReader::new(&data);
        for &expected in codes {
            assert_eq!(reader.read_bits(9), Some(expected));
        }
    }

    #[test]
    fn test_lsb_exhausted() {
        let data = vec![0xFFu8];
        let mut reader = LsbBitReader::new(&data);
        let _ = reader.read_bits(8);
        assert!(reader.is_exhausted());
    }
}

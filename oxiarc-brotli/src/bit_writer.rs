//! Bit-level writer for Brotli compression.
//!
//! Writes bits in little-endian order (LSB first within each byte),
//! matching the Brotli specification.

use crate::error::{BrotliError, BrotliResult};

/// A bit writer that accumulates bits and outputs complete bytes.
///
/// Bits are written in little-endian order: the first bit written
/// becomes the LSB of the first output byte.
#[derive(Debug)]
pub struct BitWriter {
    /// Output buffer.
    output: Vec<u8>,
    /// Current partial byte being assembled.
    current_byte: u8,
    /// Number of bits written to `current_byte` (0..8).
    bit_count: u32,
}

impl BitWriter {
    /// Create a new bit writer.
    pub fn new() -> Self {
        BitWriter {
            output: Vec::new(),
            current_byte: 0,
            bit_count: 0,
        }
    }

    /// Create a new bit writer with pre-allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        BitWriter {
            output: Vec::with_capacity(capacity),
            current_byte: 0,
            bit_count: 0,
        }
    }

    /// Write `n` bits from `value` (least-significant bits first).
    pub fn write_bits(&mut self, value: u32, n: u32) -> BrotliResult<()> {
        if n > 32 {
            return Err(BrotliError::InvalidParameter(format!(
                "cannot write {n} bits at once (max 32)"
            )));
        }
        let mut val = value;
        let mut remaining = n;

        while remaining > 0 {
            let space = 8 - self.bit_count;
            let to_write = remaining.min(space);
            let mask = if to_write == 32 {
                u32::MAX
            } else {
                (1u32 << to_write) - 1
            };
            self.current_byte |= ((val & mask) as u8) << self.bit_count;
            self.bit_count += to_write;
            val >>= to_write;
            remaining -= to_write;

            if self.bit_count == 8 {
                self.output.push(self.current_byte);
                self.current_byte = 0;
                self.bit_count = 0;
            }
        }
        Ok(())
    }

    /// Write a single bit.
    pub fn write_bit(&mut self, bit: bool) -> BrotliResult<()> {
        self.write_bits(u32::from(bit), 1)
    }

    /// Write a full byte (8 bits).
    pub fn write_byte(&mut self, byte: u8) -> BrotliResult<()> {
        self.write_bits(byte as u32, 8)
    }

    /// Write raw bytes (must be byte-aligned).
    pub fn write_bytes(&mut self, bytes: &[u8]) -> BrotliResult<()> {
        if self.bit_count == 0 {
            // Fast path: byte-aligned
            self.output.extend_from_slice(bytes);
        } else {
            for &b in bytes {
                self.write_byte(b)?;
            }
        }
        Ok(())
    }

    /// Flush any remaining partial byte (padded with zeros).
    pub fn flush(&mut self) {
        if self.bit_count > 0 {
            self.output.push(self.current_byte);
            self.current_byte = 0;
            self.bit_count = 0;
        }
    }

    /// Consume the writer and return the output bytes.
    /// Flushes any remaining partial byte first.
    pub fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.output
    }

    /// Return the number of bits written so far (including partial byte).
    pub fn bits_written(&self) -> usize {
        self.output.len() * 8 + self.bit_count as usize
    }

    /// Return reference to the current output buffer.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Write a Brotli-style variable-length integer.
    /// Writes 0 as a single 0 bit. Writes non-zero as a 1 bit followed by `n` bits of value.
    pub fn write_variable_length(&mut self, value: u32, n: u32) -> BrotliResult<()> {
        if value == 0 {
            self.write_bit(false)
        } else {
            self.write_bit(true)?;
            self.write_bits(value, n)
        }
    }
}

impl Default for BitWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bit_reader::BitReader;

    #[test]
    fn test_write_and_read_roundtrip() {
        let mut writer = BitWriter::new();
        writer.write_bits(0b1010, 4).ok();
        writer.write_bits(0b110, 3).ok();
        writer.write_bits(1, 1).ok();
        let data = writer.finish();

        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(4).ok(), Some(0b1010));
        assert_eq!(reader.read_bits(3).ok(), Some(0b110));
        assert_eq!(reader.read_bits(1).ok(), Some(1));
    }

    #[test]
    fn test_write_single_bits() {
        let mut writer = BitWriter::new();
        // Write bits: 1, 0, 1, 1, 0, 0, 0, 1
        for bit in [true, false, true, true, false, false, false, true] {
            writer.write_bit(bit).ok();
        }
        let data = writer.finish();
        assert_eq!(data, vec![0b10001101]);
    }

    #[test]
    fn test_write_cross_byte() {
        let mut writer = BitWriter::new();
        writer.write_bits(0xFFF, 12).ok();
        let data = writer.finish();
        assert_eq!(data, vec![0xFF, 0x0F]);
    }

    #[test]
    fn test_write_bytes_aligned() {
        let mut writer = BitWriter::new();
        writer.write_bytes(&[0xAB, 0xCD]).ok();
        let data = writer.finish();
        assert_eq!(data, vec![0xAB, 0xCD]);
    }

    #[test]
    fn test_write_bytes_unaligned() {
        let mut writer = BitWriter::new();
        writer.write_bits(0x01, 4).ok();
        writer.write_bytes(&[0xAB]).ok();
        let data = writer.finish();
        // 4 bits: 0001, then 8 bits: 10101011
        // byte 0: 0001 + lower 4 of 0xAB (1011) = 0b10110001 = 0xB1
        // byte 1: upper 4 of 0xAB (1010) = 0b00001010 = 0x0A
        assert_eq!(data, vec![0xB1, 0x0A]);
    }

    #[test]
    fn test_variable_length() {
        let mut writer = BitWriter::new();
        writer.write_variable_length(0, 4).ok();
        writer.write_variable_length(5, 4).ok();
        let data = writer.finish();

        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_variable_length(4).ok(), Some(0));
        assert_eq!(reader.read_variable_length(4).ok(), Some(5));
    }
}

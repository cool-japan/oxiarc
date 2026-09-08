//! MSB-first bit writing for TIFF LZW.
//!
//! TIFF LZW packs codes MSB-first (Most Significant Bit first), which
//! differs from DEFLATE/LZH's LSB-first packing. The *read* side lives in
//! [`crate::bits`], which extracts a code from a four-byte window without
//! keeping any state.

use crate::error::{LzwError, Result};

/// MSB-first bit writer for LZW compression.
#[derive(Debug)]
pub struct MsbBitWriter {
    /// Output buffer.
    output: Vec<u8>,
    /// Bit buffer (MSB-first).
    buffer: u32,
    /// Number of bits in buffer.
    bits_in_buffer: u8,
}

impl MsbBitWriter {
    /// Create a new MSB bit writer.
    pub fn new() -> Self {
        Self {
            output: Vec::new(),
            buffer: 0,
            bits_in_buffer: 0,
        }
    }

    /// Write up to 16 bits to the stream (MSB-first).
    pub fn write_bits(&mut self, value: u16, count: u8) -> Result<()> {
        if count == 0 || count > 16 {
            return Err(LzwError::InvalidBitWidth(count));
        }

        // Add bits to buffer (shift left to make room)
        self.buffer = (self.buffer << count) | (value as u32 & ((1u32 << count) - 1));
        self.bits_in_buffer += count;

        // Flush complete bytes (from MSB side)
        while self.bits_in_buffer >= 8 {
            let byte = (self.buffer >> (self.bits_in_buffer - 8)) as u8;
            self.output.push(byte);
            self.bits_in_buffer -= 8;
        }

        Ok(())
    }

    /// Flush remaining bits, padding with zeros if needed.
    pub fn flush(&mut self) -> Result<()> {
        if self.bits_in_buffer > 0 {
            // Pad with zeros and flush
            let remaining = 8 - self.bits_in_buffer;
            self.buffer <<= remaining;
            let byte = (self.buffer & 0xFF) as u8;
            self.output.push(byte);
            self.buffer = 0;
            self.bits_in_buffer = 0;
        }
        Ok(())
    }

    /// Get the output data.
    pub fn into_vec(mut self) -> Result<Vec<u8>> {
        self.flush()?;
        Ok(self.output)
    }
}

impl Default for MsbBitWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_msb_writer_packs_bits_high_first() {
        let mut writer = MsbBitWriter::new();

        writer.write_bits(0b101, 3).expect("write 3 msb bits");
        writer.write_bits(0b1100, 4).expect("write 4 msb bits");
        writer.write_bits(0b11111111, 8).expect("write 8 msb bits");

        let data = writer.into_vec().expect("flush msb writer to vec");
        // 15 bits, 101 1100 11111111, packed high-first and zero-padded:
        // 1011_1001 1111_111|0
        assert_eq!(data, vec![0b1011_1001, 0b1111_1110]);
    }

    #[test]
    fn test_msb_byte_boundary() {
        let mut writer = MsbBitWriter::new();

        // Write exactly one byte
        writer.write_bits(0xAB, 8).expect("write byte 0xAB msb");

        let data = writer.into_vec().expect("flush msb writer byte boundary");
        assert_eq!(data, vec![0xAB]);
    }
}

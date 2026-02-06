//! MSB-first bit stream operations for TIFF LZW.
//!
//! TIFF LZW uses MSB-first (Most Significant Bit first) bit ordering,
//! which differs from DEFLATE/LZH that use LSB-first.

use crate::error::{LzwError, Result};

/// MSB-first bit reader for LZW decompression.
#[derive(Debug)]
pub struct MsbBitReader<'a> {
    /// Input data.
    data: &'a [u8],
    /// Current byte position.
    byte_pos: usize,
    /// Bit buffer (MSB-first).
    buffer: u32,
    /// Number of valid bits in buffer (from MSB).
    bits_in_buffer: u8,
    /// Total bits read (for error reporting).
    total_bits_read: u64,
}

impl<'a> MsbBitReader<'a> {
    /// Create a new MSB bit reader.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            buffer: 0,
            bits_in_buffer: 0,
            total_bits_read: 0,
        }
    }

    /// Fill buffer with at least `count` bits.
    #[inline]
    fn fill_buffer(&mut self, count: u8) -> Result<()> {
        while self.bits_in_buffer < count && self.byte_pos < self.data.len() {
            let byte = self.data[self.byte_pos];
            self.byte_pos += 1;

            // Add byte to buffer (MSB-first)
            self.buffer = (self.buffer << 8) | (byte as u32);
            self.bits_in_buffer += 8;
        }

        if self.bits_in_buffer < count {
            return Err(LzwError::UnexpectedEof {
                position: self.total_bits_read,
            });
        }

        Ok(())
    }

    /// Read up to 16 bits from the stream (MSB-first).
    pub fn read_bits(&mut self, count: u8) -> Result<u16> {
        if count == 0 || count > 16 {
            return Err(LzwError::InvalidBitWidth(count));
        }

        self.fill_buffer(count)?;

        // Extract bits from MSB side of buffer
        let shift = self.bits_in_buffer - count;
        let mask = (1u32 << count) - 1;
        let value = (self.buffer >> shift) & mask;

        self.bits_in_buffer -= count;
        self.total_bits_read += count as u64;

        Ok(value as u16)
    }

    /// Get total bits read.
    pub fn bits_read(&self) -> u64 {
        self.total_bits_read
    }
}

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
    fn test_msb_roundtrip() {
        let mut writer = MsbBitWriter::new();

        // Write some bits
        writer.write_bits(0b101, 3).unwrap();
        writer.write_bits(0b1100, 4).unwrap();
        writer.write_bits(0b11111111, 8).unwrap();

        let data = writer.into_vec().unwrap();

        // Read them back
        let mut reader = MsbBitReader::new(&data);
        assert_eq!(reader.read_bits(3).unwrap(), 0b101);
        assert_eq!(reader.read_bits(4).unwrap(), 0b1100);
        assert_eq!(reader.read_bits(8).unwrap(), 0b11111111);
    }

    #[test]
    fn test_msb_byte_boundary() {
        let mut writer = MsbBitWriter::new();

        // Write exactly one byte
        writer.write_bits(0xAB, 8).unwrap();

        let data = writer.into_vec().unwrap();
        assert_eq!(data, vec![0xAB]);

        // Read it back
        let mut reader = MsbBitReader::new(&data);
        assert_eq!(reader.read_bits(8).unwrap(), 0xAB);
    }
}

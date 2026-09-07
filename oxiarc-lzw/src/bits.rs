//! Bit-order abstraction shared by the LZW encoder and decoder.
//!
//! LZW codes are packed either most-significant-bit-first (TIFF 6.0 §13) or
//! least-significant-bit-first (GIF 89a, UNIX `compress(1)`). The two
//! packings are not interchangeable: the same byte sequence decodes to
//! different codes under each. [`crate::LzwBitOrder`] selects one, and the
//! two traits below let the single decode/encode loop work with either
//! concrete bit reader/writer without dynamic dispatch in the hot path.

use crate::bitstream_lsb::{LsbBitReader, LsbBitWriter};
use crate::bitstream_msb::{MsbBitReader, MsbBitWriter};
use crate::error::{LzwError, Result};

/// Reader of variable-width LZW codes in a fixed bit order.
pub(crate) trait LzwCodeReader {
    /// Read the next `count`-bit code (`1 <= count <= 16`).
    ///
    /// # Errors
    ///
    /// [`LzwError::UnexpectedEof`] when fewer than `count` bits remain.
    fn read_code(&mut self, count: u8) -> Result<u16>;

    /// Total number of code bits consumed so far (for error positions).
    fn code_bits_read(&self) -> u64;
}

/// Writer of variable-width LZW codes in a fixed bit order.
pub(crate) trait LzwCodeWriter {
    /// Append a `count`-bit code (`1 <= count <= 16`).
    ///
    /// # Errors
    ///
    /// [`LzwError::InvalidBitWidth`] when `count` is outside `1..=16`.
    fn write_code(&mut self, value: u16, count: u8) -> Result<()>;

    /// Flush the trailing partial byte (zero-padded) and return the bytes.
    ///
    /// # Errors
    ///
    /// Propagates any error from the underlying writer.
    fn finish(self) -> Result<Vec<u8>>;
}

impl LzwCodeReader for MsbBitReader<'_> {
    #[inline]
    fn read_code(&mut self, count: u8) -> Result<u16> {
        self.read_bits(count)
    }

    #[inline]
    fn code_bits_read(&self) -> u64 {
        self.bits_read()
    }
}

impl LzwCodeReader for LsbBitReader<'_> {
    #[inline]
    fn read_code(&mut self, count: u8) -> Result<u16> {
        if count == 0 || count > 16 {
            return Err(LzwError::InvalidBitWidth(count));
        }
        let position = self.bits_read();
        self.read_bits(usize::from(count))
            .ok_or(LzwError::UnexpectedEof { position })
    }

    #[inline]
    fn code_bits_read(&self) -> u64 {
        self.bits_read()
    }
}

impl LzwCodeWriter for MsbBitWriter {
    #[inline]
    fn write_code(&mut self, value: u16, count: u8) -> Result<()> {
        self.write_bits(value, count)
    }

    fn finish(self) -> Result<Vec<u8>> {
        self.into_vec()
    }
}

impl LzwCodeWriter for LsbBitWriter {
    #[inline]
    fn write_code(&mut self, value: u16, count: u8) -> Result<()> {
        if count == 0 || count > 16 {
            return Err(LzwError::InvalidBitWidth(count));
        }
        self.write_bits(value, usize::from(count));
        Ok(())
    }

    fn finish(self) -> Result<Vec<u8>> {
        Ok(self.into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_orders_round_trip_every_width() {
        let codes: [u16; 6] = [0, 1, 255, 256, 4095, 65535];
        for width in 9u8..=16 {
            let mask = ((1u32 << width) - 1) as u16;
            let wanted: Vec<u16> = codes.iter().map(|c| c & mask).collect();

            let mut msb = MsbBitWriter::new();
            let mut lsb = LsbBitWriter::new();
            for &code in &wanted {
                msb.write_code(code, width).expect("msb write");
                lsb.write_code(code, width).expect("lsb write");
            }
            let msb_bytes = msb.finish().expect("msb finish");
            let lsb_bytes = lsb.finish().expect("lsb finish");

            let mut msb_reader = MsbBitReader::new(&msb_bytes);
            let mut lsb_reader = LsbBitReader::new(&lsb_bytes);
            for &code in &wanted {
                assert_eq!(msb_reader.read_code(width).expect("msb read"), code);
                assert_eq!(lsb_reader.read_code(width).expect("lsb read"), code);
            }
            assert_eq!(msb_reader.code_bits_read(), lsb_reader.code_bits_read());
        }
    }

    #[test]
    fn lsb_reader_reports_eof_with_a_position() {
        let bytes = [0xFFu8, 0x01];
        let mut reader = LsbBitReader::new(&bytes);
        assert_eq!(reader.read_code(9).expect("first code"), 0x1FF);
        match reader.read_code(9) {
            Err(LzwError::UnexpectedEof { position }) => assert_eq!(position, 9),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    #[test]
    fn zero_and_oversized_widths_are_rejected() {
        let bytes = [0u8; 4];
        let mut reader = LsbBitReader::new(&bytes);
        assert!(matches!(
            reader.read_code(0),
            Err(LzwError::InvalidBitWidth(0))
        ));
        assert!(matches!(
            reader.read_code(17),
            Err(LzwError::InvalidBitWidth(17))
        ));
        let mut writer = LsbBitWriter::new();
        assert!(matches!(
            writer.write_code(0, 17),
            Err(LzwError::InvalidBitWidth(17))
        ));
    }
}

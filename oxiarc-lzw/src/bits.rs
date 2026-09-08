//! Bit-order abstraction shared by the LZW encoder and decoder.
//!
//! LZW codes are packed either most-significant-bit-first (TIFF 6.0 §13) or
//! least-significant-bit-first (GIF 89a, UNIX `compress(1)`). The two
//! packings are not interchangeable: the same byte sequence decodes to
//! different codes under each. [`crate::LzwBitOrder`] selects one, and the
//! types below let the single decode/encode loop work with either without
//! dynamic dispatch in the hot path.
//!
//! # Why the reader is stateless
//!
//! A 16-bit code starting at any bit offset spans at most three bytes, so a
//! four-byte window always covers one whole code. [`CodeOrder`] therefore
//! only loads that window and shifts the code out of it: the *position* is
//! a plain local in the decode loop, not a field the loop has to keep in
//! sync with memory.
//!
//! That is not a micro-detail. With a stateful reader behind `&mut`, the
//! compiler wrote the bit position back to the reader's struct on every
//! single code (measured in the emitted arm64: an extra load of the reader
//! pointer plus a store, per code, because the error paths could observe
//! it). Holding the position in a register instead is worth roughly a fifth
//! of the decoder's throughput on data LZW cannot compress, where one code
//! is one output byte and per-code overhead is all there is.

use crate::bitstream_lsb::LsbBitWriter;
use crate::bitstream_msb::MsbBitWriter;
use crate::error::{LzwError, Result};

/// Extraction of variable-width LZW codes in a fixed bit order.
///
/// Implementors are zero-sized markers; every method is an associated
/// function, so a decode loop generic over `O: CodeOrder` compiles to
/// straight-line code with no branch on the bit order.
pub(crate) trait CodeOrder {
    /// The four bytes starting at `byte`, as one word in this order's
    /// endianness. Bytes past the end of `data` read as zero — a code that
    /// reaches them would have failed the caller's length check first.
    fn window(data: &[u8], byte: usize) -> u32;

    /// The `width`-bit code that begins `shift` bits into `window`.
    ///
    /// `mask` is `(1 << width) - 1`, which the decode loop keeps alongside
    /// the width so it is not recomputed per code. `shift` is `0..=7` and
    /// `width` is `1..=16`; anything else yields a well-defined but
    /// meaningless value rather than a panic.
    fn extract(window: u32, shift: u32, width: u32, mask: u32) -> u16;
}

/// MSB-first packing: the first code occupies the high bits of the first
/// byte. Used by TIFF 6.0.
#[derive(Debug)]
pub(crate) struct MsbCodes;

/// LSB-first packing: the first code occupies the low bits of the first
/// byte. Used by GIF 89a and UNIX `compress(1)`.
#[derive(Debug)]
pub(crate) struct LsbCodes;

/// The four bytes at `byte`, zero-padded past the end of `data`.
///
/// The fast arm is one unaligned four-byte load; the padded arm only runs
/// within four bytes of the end of the input.
#[inline(always)]
fn window_bytes(data: &[u8], byte: usize) -> [u8; 4] {
    // `get(byte..byte + 4)` is one runtime comparison: the returned slice's
    // length is the constant 4, so `first_chunk` folds away. Asking for
    // `get(byte..)` instead costs a second comparison per code.
    match data.get(byte..byte + 4).and_then(<[u8]>::first_chunk::<4>) {
        Some(chunk) => *chunk,
        None => {
            let mut padded = [0u8; 4];
            for (offset, slot) in padded.iter_mut().enumerate() {
                *slot = data.get(byte + offset).copied().unwrap_or(0);
            }
            padded
        }
    }
}

impl CodeOrder for MsbCodes {
    #[inline(always)]
    fn window(data: &[u8], byte: usize) -> u32 {
        u32::from_be_bytes(window_bytes(data, byte))
    }

    #[inline(always)]
    fn extract(window: u32, shift: u32, width: u32, mask: u32) -> u16 {
        // The code's last bit sits `32 - shift - width` bits from the
        // bottom of the big-endian window. The `& 31` cannot bind for a
        // valid (shift, width) pair; it is there so that an invalid one
        // wraps instead of panicking on a shift overflow.
        let drop = 32u32.wrapping_sub(shift).wrapping_sub(width) & 31;
        ((window >> drop) & mask) as u16
    }
}

impl CodeOrder for LsbCodes {
    #[inline(always)]
    fn window(data: &[u8], byte: usize) -> u32 {
        u32::from_le_bytes(window_bytes(data, byte))
    }

    #[inline(always)]
    fn extract(window: u32, shift: u32, _width: u32, mask: u32) -> u16 {
        ((window >> (shift & 31)) & mask) as u16
    }
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

    /// Read a whole stream of fixed-width codes through [`CodeOrder`].
    fn read_all<O: CodeOrder>(data: &[u8], width: u32, count: usize) -> Vec<u16> {
        let mask = (1u32 << width) - 1;
        let mut out = Vec::with_capacity(count);
        let mut bit = 0u64;
        for _ in 0..count {
            let byte = (bit >> 3) as usize;
            let shift = (bit & 7) as u32;
            out.push(O::extract(O::window(data, byte), shift, width, mask));
            bit += u64::from(width);
        }
        out
    }

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

            assert_eq!(
                read_all::<MsbCodes>(&msb_bytes, u32::from(width), wanted.len()),
                wanted,
                "msb width {width}"
            );
            assert_eq!(
                read_all::<LsbCodes>(&lsb_bytes, u32::from(width), wanted.len()),
                wanted,
                "lsb width {width}"
            );
        }
    }

    #[test]
    fn oversized_widths_do_not_panic() {
        let bytes = [0xAAu8; 4];
        for width in [0u32, 17, 31, 32, 33] {
            let mask = 0xFFFFu32;
            let _ = MsbCodes::extract(MsbCodes::window(&bytes, 0), 3, width, mask);
            let _ = LsbCodes::extract(LsbCodes::window(&bytes, 0), 3, width, mask);
        }
    }

    #[test]
    fn zero_width_writes_are_rejected() {
        let mut writer = LsbBitWriter::new();
        assert!(matches!(
            writer.write_code(0, 17),
            Err(LzwError::InvalidBitWidth(17))
        ));
        let mut writer = MsbBitWriter::new();
        assert!(matches!(
            writer.write_code(0, 0),
            Err(LzwError::InvalidBitWidth(0))
        ));
    }

    /// A code may start at any bit offset and span three bytes; both orders
    /// must agree with a straightforward bit-by-bit reference at every
    /// offset and width, including the zero-padded window at the very end
    /// of the input.
    #[test]
    fn windowed_reads_match_a_bit_by_bit_reference_at_every_offset() {
        let data: Vec<u8> = (0u16..40).map(|i| (i * 37 + 11) as u8).collect();
        let total = (data.len() as u64) * 8;
        for skip in 0u32..=7 {
            for width in 1u32..=16 {
                let mask = (1u32 << width) - 1;
                let mut bit = u64::from(skip);
                while bit + u64::from(width) <= total {
                    let byte = (bit >> 3) as usize;
                    let shift = (bit & 7) as u32;
                    let mut msb_ref = 0u32;
                    let mut lsb_ref = 0u32;
                    for index in 0..u64::from(width) {
                        let position = bit + index;
                        let source = data[(position / 8) as usize];
                        msb_ref = (msb_ref << 1) | u32::from((source >> (7 - (position % 8))) & 1);
                        lsb_ref |= u32::from((source >> (position % 8)) & 1) << index;
                    }
                    assert_eq!(
                        MsbCodes::extract(MsbCodes::window(&data, byte), shift, width, mask),
                        msb_ref as u16,
                        "msb skip {skip} width {width} bit {bit}"
                    );
                    assert_eq!(
                        LsbCodes::extract(LsbCodes::window(&data, byte), shift, width, mask),
                        lsb_ref as u16,
                        "lsb skip {skip} width {width} bit {bit}"
                    );
                    bit += u64::from(width);
                }
            }
        }
    }
}

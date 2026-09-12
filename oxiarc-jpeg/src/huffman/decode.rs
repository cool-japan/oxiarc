//! The entropy-coded segment bit reader.
//!
//! JPEG entropy data is a byte stream in which `0xFF` is escaped as
//! `0xFF 0x00`, any run of `0xFF` before a non-zero code is marker fill, and
//! `0xFF <code>` ends the segment. Past the end of the segment libjpeg feeds
//! zero bits forever so that a truncated final MCU still decodes; this reader
//! does the same, and counts the fabricated bits so a caller that does not
//! tolerate truncation can turn them into an error.

use super::{HuffmanTable, LOOKUP_BITS};
use crate::error::{JpegError, Result};

/// MSB-first bit reader over one entropy-coded segment.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the next byte to feed into the accumulator.
    pos: usize,
    /// Accumulator; the low `count` bits are valid, most significant first.
    bits: u64,
    /// Number of valid bits in `bits`.
    count: u32,
    /// How many of those `count` bits came from real bytes (they are the
    /// leading ones).
    real: u32,
    /// Number of fabricated zero bits that have actually been consumed.
    fabricated: u64,
    /// Marker code found at `marker_at`, if the reader stopped at one.
    marker: Option<u8>,
    /// Offset of the `0xFF` that starts the pending marker.
    marker_at: usize,
}

impl<'a> BitReader<'a> {
    /// Create a reader over an entropy-coded segment.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bits: 0,
            count: 0,
            real: 0,
            fabricated: 0,
            marker: None,
            marker_at: data.len(),
        }
    }

    /// Byte offset just past everything the reader has consumed, i.e. where a
    /// caller should resume marker parsing.
    pub(crate) fn byte_offset(&self) -> usize {
        if self.marker.is_some() {
            self.marker_at
        } else {
            self.pos
        }
    }

    /// The marker the reader stopped at, if any.
    #[cfg(test)]
    pub(crate) fn pending_marker(&self) -> Option<u8> {
        self.marker
    }

    /// Number of zero bits fabricated past the end of the segment and then
    /// consumed. Non-zero means the scan data was short.
    pub(crate) fn fabricated_bits(&self) -> u64 {
        self.fabricated
    }

    /// Pull one entropy byte, honouring `0xFF 0x00` stuffing and stopping at
    /// a marker.
    #[inline]
    fn next_byte(&mut self) -> Option<u8> {
        if self.marker.is_some() {
            return None;
        }
        let byte = *self.data.get(self.pos)?;
        if byte != 0xFF {
            self.pos += 1;
            return Some(byte);
        }
        // Skip any run of fill bytes, then classify.
        let start = self.pos;
        let mut probe = self.pos + 1;
        while self.data.get(probe) == Some(&0xFF) {
            probe += 1;
        }
        match self.data.get(probe) {
            None => {
                // A trailing `0xFF` run with no code: treat as exhausted.
                self.pos = start;
                None
            }
            Some(0) => {
                self.pos = probe + 1;
                Some(0xFF)
            }
            Some(&code) => {
                self.marker = Some(code);
                self.marker_at = start;
                self.pos = start;
                None
            }
        }
    }

    /// Fill the accumulator to at least 57 bits, fabricating zeros past the
    /// end of the segment.
    #[inline]
    fn fill(&mut self) {
        while self.count <= 56 {
            match self.next_byte() {
                Some(byte) => {
                    self.bits = (self.bits << 8) | u64::from(byte);
                    self.count += 8;
                    self.real += 8;
                }
                None => {
                    self.bits <<= 8;
                    self.count += 8;
                }
            }
        }
    }

    /// Ensure at least `n` bits are buffered.
    #[inline]
    fn ensure(&mut self, n: u32) {
        if self.count < n {
            self.fill();
        }
    }

    /// Look at the next `n` bits (`n <= 32`) without consuming them.
    #[inline]
    fn peek(&mut self, n: u32) -> u32 {
        self.ensure(n);
        ((self.bits >> (self.count - n)) & ((1u64 << n) - 1)) as u32
    }

    /// Drop `n` bits that a previous [`Self::peek`] examined.
    #[inline]
    fn drop_bits(&mut self, n: u32) {
        debug_assert!(self.count >= n);
        if self.real >= n {
            self.real -= n;
        } else {
            self.fabricated += u64::from(n - self.real);
            self.real = 0;
        }
        self.count -= n;
    }

    /// Read `n` bits (`n <= 32`) MSB-first.
    #[inline]
    pub(crate) fn get_bits(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = self.peek(n);
        self.drop_bits(n);
        v
    }

    /// Read a single bit.
    #[inline]
    pub(crate) fn get_bit(&mut self) -> u32 {
        self.get_bits(1)
    }

    /// Decode one Huffman symbol.
    #[inline]
    pub(crate) fn decode(&mut self, table: &HuffmanTable, mcu: u64) -> Result<u8> {
        let prefix = self.peek(LOOKUP_BITS) as usize;
        if let Some((length, symbol)) = table.fast(prefix) {
            self.drop_bits(length);
            return Ok(symbol);
        }
        // Codes longer than the lookup: extend one bit at a time.
        let mut code = prefix as i32;
        let mut length = LOOKUP_BITS as usize;
        self.drop_bits(LOOKUP_BITS);
        while length < 16 {
            if table.fits(code, length) {
                if let Some(symbol) = table.slow(code, length) {
                    return Ok(symbol);
                }
                break;
            }
            code = (code << 1) | self.get_bit() as i32;
            length += 1;
        }
        if let Some(symbol) = table.slow(code, length) {
            return Ok(symbol);
        }
        Err(JpegError::InvalidHuffmanCode { mcu })
    }

    /// Read `size` additional bits and sign-extend them (T.81 figure F.12).
    #[inline]
    pub(crate) fn receive_extend(&mut self, size: u32) -> i32 {
        if size == 0 {
            return 0;
        }
        let value = self.get_bits(size) as i32;
        extend(value, size)
    }

    /// Discard buffered bits and position the reader on the next marker.
    ///
    /// Returns the marker code, or `None` when the segment simply ended.
    pub(crate) fn seek_marker(&mut self) -> Option<u8> {
        self.bits = 0;
        self.count = 0;
        self.real = 0;
        if let Some(code) = self.marker {
            return Some(code);
        }
        let mut i = self.pos;
        while i < self.data.len() {
            if self.data[i] != 0xFF {
                i += 1;
                continue;
            }
            let start = i;
            let mut probe = i + 1;
            while self.data.get(probe) == Some(&0xFF) {
                probe += 1;
            }
            match self.data.get(probe) {
                None => break,
                Some(0) => {
                    i = probe + 1;
                }
                Some(&code) => {
                    self.marker = Some(code);
                    self.marker_at = start;
                    self.pos = start;
                    return Some(code);
                }
            }
        }
        self.pos = self.data.len();
        None
    }

    /// Consume the pending marker (which must be the one `seek_marker`
    /// reported) and continue reading entropy data after it.
    pub(crate) fn consume_marker(&mut self) {
        if self.marker.take().is_some() {
            let mut probe = self.marker_at + 1;
            while self.data.get(probe) == Some(&0xFF) {
                probe += 1;
            }
            self.pos = (probe + 1).min(self.data.len());
            self.marker_at = self.data.len();
            self.bits = 0;
            self.count = 0;
            self.real = 0;
        }
    }
}

/// Sign-extend a `size`-bit magnitude (T.81 figure F.12, `EXTEND`).
#[inline]
pub(crate) fn extend(value: i32, size: u32) -> i32 {
    if size == 0 {
        return 0;
    }
    if value < (1 << (size - 1)) {
        value - (1 << size) + 1
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES};

    #[test]
    fn reads_bits_msb_first() {
        let data = [0b1011_0010u8, 0b0100_0001];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(4), 0b1011);
        assert_eq!(r.get_bits(4), 0b0010);
        assert_eq!(r.get_bits(8), 0b0100_0001);
        assert_eq!(r.fabricated_bits(), 0);
    }

    #[test]
    fn unstuffs_ff00() {
        let data = [0xFFu8, 0x00, 0x5A];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(8), 0xFF);
        assert_eq!(r.get_bits(8), 0x5A);
        assert_eq!(r.pending_marker(), None);
    }

    #[test]
    fn stops_at_a_marker_and_reports_its_offset() {
        let data = [0x12u8, 0xFF, 0xD9];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(8), 0x12);
        // The next fill hits the marker and fabricates zeros.
        assert_eq!(r.get_bits(8), 0);
        assert_eq!(r.pending_marker(), Some(0xD9));
        assert_eq!(r.byte_offset(), 1);
        assert_eq!(r.fabricated_bits(), 8);
    }

    #[test]
    fn skips_fill_bytes_before_a_marker() {
        let data = [0x12u8, 0xFF, 0xFF, 0xFF, 0xD0];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(8), 0x12);
        assert_eq!(r.seek_marker(), Some(0xD0));
        assert_eq!(r.byte_offset(), 1);
        r.consume_marker();
        assert_eq!(r.byte_offset(), 5);
    }

    #[test]
    fn fabricates_zeros_past_the_end() {
        let data = [0xABu8];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(8), 0xAB);
        assert_eq!(r.fabricated_bits(), 0);
        assert_eq!(r.get_bits(16), 0);
        assert_eq!(r.fabricated_bits(), 16);
        assert_eq!(r.pending_marker(), None);
    }

    #[test]
    fn trailing_ff_run_without_a_code_is_end_of_data() {
        let data = [0x01u8, 0xFF, 0xFF];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(8), 0x01);
        assert_eq!(r.get_bits(8), 0);
        assert_eq!(r.pending_marker(), None);
        assert_eq!(r.seek_marker(), None);
    }

    #[test]
    fn decodes_annex_k_dc_symbols() {
        let table = super::HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec())
            .expect("valid");
        // `00` -> 0, `010` -> 1, `011` -> 2, `100` -> 3.
        let data = [0b0001_0011u8, 0b1000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.decode(&table, 0).expect("symbol"), 0);
        assert_eq!(r.decode(&table, 0).expect("symbol"), 1);
        assert_eq!(r.decode(&table, 0).expect("symbol"), 2);
        assert_eq!(r.decode(&table, 0).expect("symbol"), 3);
    }

    #[test]
    fn long_codes_take_the_slow_path() {
        // One 16-bit code, value 0xAA: canonical code is 0.
        let mut bits = [0u8; 16];
        bits[15] = 1;
        let table = super::HuffmanTable::new(bits, vec![0xAA]).expect("valid");
        let data = [0x00u8, 0x00];
        let mut r = BitReader::new(&data);
        assert_eq!(r.decode(&table, 0).expect("symbol"), 0xAA);
    }

    #[test]
    fn undecodable_code_is_an_error_not_a_panic() {
        let mut bits = [0u8; 16];
        bits[1] = 1; // only `00` is defined
        let table = super::HuffmanTable::new(bits, vec![7]).expect("valid");
        let data = [0xFFu8, 0x00, 0xFF, 0x00];
        let mut r = BitReader::new(&data);
        assert!(matches!(
            r.decode(&table, 42),
            Err(JpegError::InvalidHuffmanCode { mcu: 42 })
        ));
    }

    #[test]
    fn extend_matches_figure_f12() {
        assert_eq!(extend(0, 0), 0);
        assert_eq!(extend(0, 1), -1);
        assert_eq!(extend(1, 1), 1);
        assert_eq!(extend(0b00, 2), -3);
        assert_eq!(extend(0b01, 2), -2);
        assert_eq!(extend(0b10, 2), 2);
        assert_eq!(extend(0b11, 2), 3);
        assert_eq!(extend(0, 16), -65535);
        assert_eq!(extend(0xFFFF, 16), 65535);
    }

    #[test]
    fn receive_extend_reads_then_extends() {
        let data = [0b0100_0000u8];
        let mut r = BitReader::new(&data);
        // Two bits `01` -> -2.
        assert_eq!(r.receive_extend(2), -2);
        assert_eq!(r.receive_extend(0), 0);
    }

    #[test]
    fn consume_marker_resumes_after_a_restart() {
        let data = [0x11u8, 0xFF, 0xD0, 0x22];
        let mut r = BitReader::new(&data);
        assert_eq!(r.get_bits(8), 0x11);
        assert_eq!(r.seek_marker(), Some(0xD0));
        r.consume_marker();
        assert_eq!(r.get_bits(8), 0x22);
        assert_eq!(r.pending_marker(), None);
    }

    #[test]
    fn seek_marker_scans_forward_over_entropy_bytes() {
        let data = [0x00u8, 0x01, 0x02, 0xFF, 0x00, 0x03, 0xFF, 0xD1, 0x04];
        let mut r = BitReader::new(&data);
        assert_eq!(r.seek_marker(), Some(0xD1));
        assert_eq!(r.byte_offset(), 6);
        r.consume_marker();
        assert_eq!(r.byte_offset(), 8);
    }
}

//! Canonical Huffman table construction (ITU-T T.81 Annex C.2) with a
//! nine-bit fast lookup.
//!
//! Codes of nine bits or fewer resolve in one array read; longer codes fall
//! back to the classic `max_code`/`val_offset` walk. Over-subscribed tables
//! (more codes than the length allows, or a code of all ones) are rejected;
//! *under*-subscribed tables are accepted, because real scanner output
//! contains them and libjpeg tolerates them — a code that lands in an unused
//! slot yields [`crate::JpegError::InvalidHuffmanCode`] at decode time.

use super::{LOOKUP_BITS, LOOKUP_SIZE};
use crate::error::{JpegError, Result};

/// A decoding-ready canonical Huffman table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuffmanTable {
    /// `(length << 8) | value`, or `0` when no code of nine bits or fewer
    /// starts with this prefix.
    lookup: Box<[u16; LOOKUP_SIZE]>,
    /// Largest code of each length (`1..=16`), or `-1` when unused. Index 17
    /// is a sentinel that terminates the walk.
    max_code: [i32; 18],
    /// `val_ptr[l] - min_code[l]`, so `values[(val_offset[l] + code)]` is the
    /// decoded symbol.
    val_offset: [i32; 18],
    /// The `HUFFVAL` list, in canonical order.
    values: Vec<u8>,
    /// The `BITS` list, retained so a parsed table can be re-emitted exactly.
    bits: [u8; 16],
}

impl HuffmanTable {
    /// Build a table from a `DHT` segment's `BITS` and `HUFFVAL` lists.
    ///
    /// # Errors
    ///
    /// Returns [`crate::JpegError::MalformedSegment`] when `bits` sums to more
    /// than 256, when it disagrees with `values.len()`, or when the implied
    /// code assignment over-subscribes any length.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::HuffmanTable;
    ///
    /// // Two symbols, one bit each: codes `0` and `1`.
    /// let mut bits = [0u8; 16];
    /// bits[0] = 2;
    /// let table = HuffmanTable::new(bits, vec![0x0A, 0x0B]).expect("valid table");
    /// assert_eq!(table.bits(), &bits);
    /// assert_eq!(table.values(), &[0x0A, 0x0B]);
    /// ```
    pub fn new(bits: [u8; 16], values: Vec<u8>) -> Result<Self> {
        Self::build(bits, values, 0)
    }

    pub(crate) fn build(bits: [u8; 16], values: Vec<u8>, offset: usize) -> Result<Self> {
        let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
        if total > 256 {
            return Err(JpegError::malformed(
                "DHT",
                offset,
                "BITS sums to more than 256 codes",
            ));
        }
        if total != values.len() {
            return Err(JpegError::malformed(
                "DHT",
                offset,
                "HUFFVAL length disagrees with BITS",
            ));
        }

        // Canonical code assignment (Annex C.2, figures C.1 and C.2).
        let mut huffcode = vec![0u32; total];
        let mut code: u32 = 0;
        let mut index = 0usize;
        for length in 1..=16u32 {
            let count = usize::from(bits[(length - 1) as usize]);
            for _ in 0..count {
                huffcode[index] = code;
                index += 1;
                code += 1;
            }
            // No code may be all ones, so `code` must still fit in `length`
            // bits after the last assignment of that length.
            if code > (1u32 << length) {
                return Err(JpegError::malformed(
                    "DHT",
                    offset,
                    "Huffman code table is over-subscribed",
                ));
            }
            code <<= 1;
        }

        let mut max_code = [-1i32; 18];
        let mut val_offset = [0i32; 18];
        let mut p = 0usize;
        for length in 1..=16usize {
            let count = usize::from(bits[length - 1]);
            if count > 0 {
                val_offset[length] = p as i32 - huffcode[p] as i32;
                p += count;
                max_code[length] = huffcode[p - 1] as i32;
            } else {
                max_code[length] = -1;
            }
        }
        val_offset[17] = 0;
        max_code[17] = 0x000F_FFFF;

        let mut lookup = Box::new([0u16; LOOKUP_SIZE]);
        let mut p = 0usize;
        for length in 1..=LOOKUP_BITS as usize {
            let count = usize::from(bits[length - 1]);
            for _ in 0..count {
                let shift = LOOKUP_BITS as usize - length;
                let base = (huffcode[p] as usize) << shift;
                let entry = ((length as u16) << 8) | u16::from(values[p]);
                for slot in lookup.iter_mut().skip(base).take(1usize << shift) {
                    *slot = entry;
                }
                p += 1;
            }
        }

        Ok(Self {
            lookup,
            max_code,
            val_offset,
            values,
            bits,
        })
    }

    /// The `BITS` list this table was built from.
    #[must_use]
    pub fn bits(&self) -> &[u8; 16] {
        &self.bits
    }

    /// The `HUFFVAL` list this table was built from.
    #[must_use]
    pub fn values(&self) -> &[u8] {
        &self.values
    }

    /// Fast-path lookup entry for a nine-bit prefix.
    ///
    /// Returns `(code_length, symbol)`, or `None` when no code of nine bits or
    /// fewer starts with this prefix.
    #[inline]
    pub(crate) fn fast(&self, prefix: usize) -> Option<(u32, u8)> {
        let entry = self.lookup[prefix & (LOOKUP_SIZE - 1)];
        if entry == 0 {
            None
        } else {
            Some((u32::from(entry >> 8), (entry & 0xFF) as u8))
        }
    }

    /// Slow-path lookup: the caller supplies the code accumulated so far and
    /// its length, and this walks the remaining lengths.
    #[inline]
    pub(crate) fn slow(&self, code: i32, length: usize) -> Option<u8> {
        if length > 16 || self.max_code[length] < 0 || code > self.max_code[length] {
            return None;
        }
        let index = self.val_offset[length] + code;
        usize::try_from(index)
            .ok()
            .and_then(|i| self.values.get(i))
            .copied()
    }

    /// `true` when a code of `length` bits equal to `code` is defined.
    #[inline]
    pub(crate) fn fits(&self, code: i32, length: usize) -> bool {
        length <= 16 && self.max_code[length] >= 0 && code <= self.max_code[length]
    }
}

/// Parse a `DHT` payload into the DC and AC table slots.
pub(crate) fn parse_dht(
    payload: &[u8],
    offset: usize,
    dc: &mut [Option<HuffmanTable>; 4],
    ac: &mut [Option<HuffmanTable>; 4],
) -> Result<()> {
    let mut pos = 0usize;
    while pos < payload.len() {
        let tc_th = payload[pos];
        pos += 1;
        let tc = tc_th >> 4;
        let th = tc_th & 0x0F;
        if tc > 1 {
            return Err(JpegError::malformed("DHT", offset, "Tc must be 0 or 1"));
        }
        if th > 3 {
            return Err(JpegError::malformed("DHT", offset, "Th must be 0..=3"));
        }
        if payload.len() - pos < 16 {
            return Err(JpegError::malformed("DHT", offset, "truncated BITS"));
        }
        let mut bits = [0u8; 16];
        bits.copy_from_slice(&payload[pos..pos + 16]);
        pos += 16;
        let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
        if payload.len() - pos < total {
            return Err(JpegError::malformed("DHT", offset, "truncated HUFFVAL"));
        }
        let values = payload[pos..pos + total].to_vec();
        pos += total;

        let table = HuffmanTable::build(bits, values, offset)?;
        if tc == 0 {
            dc[usize::from(th)] = Some(table);
        } else {
            ac[usize::from(th)] = Some(table);
        }
    }
    Ok(())
}

/// Emit `DHT` segments for every defined slot.
///
/// The order is per slot — `DC0`, `AC0`, `DC1`, `AC1`, ... — which is what
/// libjpeg and libtiff write. Emitting every DC table before every AC table
/// produces a semantically identical stream that is *not* byte-identical to
/// libtiff's `JPEGTables` tag, so the order is load-bearing.
pub(crate) fn emit_dht(
    dc: &[Option<HuffmanTable>; 4],
    ac: &[Option<HuffmanTable>; 4],
    out: &mut Vec<u8>,
) {
    for slot in 0..4usize {
        for (class, table) in [(0u8, &dc[slot]), (1u8, &ac[slot])] {
            let Some(table) = table else { continue };
            let len = 2 + 1 + 16 + table.values.len();
            out.push(0xFF);
            out.push(0xC4);
            out.push((len >> 8) as u8);
            out.push((len & 0xFF) as u8);
            out.push((class << 4) | (slot as u8));
            out.extend_from_slice(&table.bits);
            out.extend_from_slice(&table.values);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{
        ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES, ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES,
    };

    fn annex_k_dc_luma() -> HuffmanTable {
        HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec())
            .expect("Annex K.3.1 is a valid table")
    }

    #[test]
    fn annex_k_tables_build() {
        let dc = annex_k_dc_luma();
        assert_eq!(dc.values().len(), 12);
        let ac = HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec())
            .expect("Annex K.3.2 is a valid table");
        assert_eq!(ac.values().len(), 162);
    }

    /// Annex K.3.1 assigns `00` to symbol 0 and `010`..`110` to symbols 1..5.
    #[test]
    fn canonical_codes_match_annex_k() {
        let dc = annex_k_dc_luma();
        // `00` followed by seven arbitrary bits.
        assert_eq!(dc.fast(0b00_0000000), Some((2, 0)));
        // `010` -> symbol 1.
        assert_eq!(dc.fast(0b010_000000), Some((3, 1)));
        // `011` -> symbol 2.
        assert_eq!(dc.fast(0b011_000000), Some((3, 2)));
        // `111110` -> symbol 8 (length 6).
        assert_eq!(dc.fast(0b111_110_000), Some((6, 8)));
    }

    #[test]
    fn every_symbol_round_trips_through_the_decoder_paths() {
        let table = HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec())
            .expect("valid");
        // Rebuild the canonical codes and check each one decodes back.
        let mut code = 0u32;
        let mut index = 0usize;
        for length in 1..=16usize {
            for _ in 0..table.bits[length - 1] {
                let symbol = table.values[index];
                if length <= LOOKUP_BITS as usize {
                    let prefix = (code as usize) << (LOOKUP_BITS as usize - length);
                    assert_eq!(table.fast(prefix), Some((length as u32, symbol)));
                } else {
                    assert!(table.fits(code as i32, length));
                    assert_eq!(table.slow(code as i32, length), Some(symbol));
                }
                code += 1;
                index += 1;
            }
            code <<= 1;
        }
        assert_eq!(index, 162);
    }

    #[test]
    fn over_subscribed_table_is_rejected() {
        let mut bits = [0u8; 16];
        bits[0] = 3; // three codes of one bit is impossible
        assert!(HuffmanTable::new(bits, vec![1, 2, 3]).is_err());
    }

    /// Two one-bit codes exhaust length 1, so a two-bit code cannot follow:
    /// its canonical value `100` needs three bits.
    #[test]
    fn over_subscription_across_lengths_is_rejected() {
        let mut bits = [0u8; 16];
        bits[0] = 2;
        bits[1] = 1;
        assert!(HuffmanTable::new(bits, vec![1, 2, 3]).is_err());
    }

    /// A table that exactly fills one length is complete, not over-subscribed.
    #[test]
    fn complete_table_at_one_length_is_accepted() {
        let mut bits = [0u8; 16];
        bits[0] = 2;
        let table = HuffmanTable::new(bits, vec![1, 2]).expect("complete table");
        assert_eq!(table.fast(0b0_00000000), Some((1, 1)));
        assert_eq!(table.fast(0b1_00000000), Some((1, 2)));
    }

    #[test]
    fn under_subscribed_table_is_accepted() {
        let mut bits = [0u8; 16];
        bits[1] = 1; // a single two-bit code `00`
        let table = HuffmanTable::new(bits, vec![7]).expect("incomplete tables are legal");
        assert_eq!(table.fast(0), Some((2, 7)));
        // `01` is undefined at length 2 and never becomes defined.
        assert!(!table.fits(0b01, 2));
        assert_eq!(table.slow(0b01, 2), None);
    }

    #[test]
    fn empty_table_is_structurally_valid_but_decodes_nothing() {
        let table = HuffmanTable::new([0u8; 16], Vec::new()).expect("empty BITS is structural");
        assert_eq!(table.fast(0), None);
        for length in 1..=16 {
            assert!(!table.fits(0, length));
        }
    }

    #[test]
    fn bits_and_values_must_agree() {
        let mut bits = [0u8; 16];
        bits[0] = 2;
        assert!(HuffmanTable::new(bits, vec![1]).is_err());
    }

    #[test]
    fn dht_round_trips_byte_exactly() {
        let mut dc: [Option<HuffmanTable>; 4] = [None, None, None, None];
        let mut ac: [Option<HuffmanTable>; 4] = [None, None, None, None];
        dc[0] = Some(annex_k_dc_luma());
        ac[0] = Some(
            HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec())
                .expect("valid"),
        );
        let mut emitted = Vec::new();
        emit_dht(&dc, &ac, &mut emitted);
        assert_eq!(&emitted[..2], &[0xFF, 0xC4]);
        assert_eq!(
            u16::from_be_bytes([emitted[2], emitted[3]]),
            2 + 1 + 16 + 12
        );

        let mut dc2: [Option<HuffmanTable>; 4] = [None, None, None, None];
        let mut ac2: [Option<HuffmanTable>; 4] = [None, None, None, None];
        let first_len = usize::from(u16::from_be_bytes([emitted[2], emitted[3]]));
        parse_dht(&emitted[4..2 + first_len], 0, &mut dc2, &mut ac2).expect("dc");
        let rest = &emitted[2 + first_len..];
        let second_len = usize::from(u16::from_be_bytes([rest[2], rest[3]]));
        parse_dht(&rest[4..2 + second_len], 0, &mut dc2, &mut ac2).expect("ac");
        assert_eq!(dc2[0], dc[0]);
        assert_eq!(ac2[0], ac[0]);
    }

    #[test]
    fn malformed_dht_payloads_are_rejected() {
        let mut dc: [Option<HuffmanTable>; 4] = [None, None, None, None];
        let mut ac: [Option<HuffmanTable>; 4] = [None, None, None, None];
        assert!(parse_dht(&[0x20], 0, &mut dc, &mut ac).is_err(), "Tc=2");
        assert!(parse_dht(&[0x04], 0, &mut dc, &mut ac).is_err(), "Th=4");
        assert!(
            parse_dht(&[0x00, 1, 2], 0, &mut dc, &mut ac).is_err(),
            "short"
        );
        let mut payload = vec![0x00];
        payload.extend_from_slice(&[1u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(
            parse_dht(&payload, 0, &mut dc, &mut ac).is_err(),
            "missing HUFFVAL"
        );
    }
}

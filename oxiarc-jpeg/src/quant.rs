//! Quantisation tables: the `DQT` segment, and libjpeg's quality scaling.

use crate::error::{JpegError, Result};
use crate::tables::{
    ANNEX_K_CHROMA_QUANT, ANNEX_K_LUMA_QUANT, NATURAL_TO_ZIGZAG, ZIGZAG_TO_NATURAL,
};

/// One 8x8 quantisation table.
///
/// Values are stored in **natural (row-major) order**, which is what the
/// dequantise step wants; the zig-zag order used on the wire is applied on
/// parse and re-applied on emit, so a parse/emit round-trip is byte-exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuantTable {
    values: [u16; 64],
    precision: u8,
}

impl Default for QuantTable {
    fn default() -> Self {
        Self {
            values: [1; 64],
            precision: 0,
        }
    }
}

impl QuantTable {
    /// Build a table from 64 values already in row-major order.
    ///
    /// The `Pq` precision flag is derived from the values: `1` (16-bit
    /// entries) if any value exceeds 255, else `0`.
    #[must_use]
    pub fn from_natural(values: [u16; 64]) -> Self {
        let precision = u8::from(values.iter().any(|&v| v > 255));
        Self { values, precision }
    }

    /// Build a table from 64 values in zig-zag order.
    #[must_use]
    pub fn from_zigzag(zigzag: [u16; 64]) -> Self {
        let mut values = [0u16; 64];
        for (k, &v) in zigzag.iter().enumerate() {
            values[ZIGZAG_TO_NATURAL[k]] = v;
        }
        Self::from_natural(values)
    }

    /// The table in row-major order.
    #[must_use]
    pub fn natural(&self) -> &[u16; 64] {
        &self.values
    }

    /// The table in zig-zag order, as it appears in a `DQT` payload.
    #[must_use]
    pub fn zigzag(&self) -> [u16; 64] {
        let mut out = [0u16; 64];
        for (k, slot) in out.iter_mut().enumerate() {
            *slot = self.values[ZIGZAG_TO_NATURAL[k]];
        }
        out
    }

    /// The `Pq` flag this table was parsed with (`0` = 8-bit, `1` = 16-bit).
    #[must_use]
    pub fn precision_flag(&self) -> u8 {
        self.precision
    }

    /// The `Pq` flag the table's values actually require.
    #[must_use]
    pub fn required_precision(&self) -> u8 {
        u8::from(self.values.iter().any(|&v| v > 255))
    }

    /// The value at row-major index `i`.
    #[must_use]
    pub fn value(&self, index: usize) -> u16 {
        self.values[index & 63]
    }

    /// Annex K.1's example luminance table, unscaled.
    #[must_use]
    pub fn annex_k_luma() -> Self {
        Self::from_natural(ANNEX_K_LUMA_QUANT)
    }

    /// Annex K.2's example chrominance table, unscaled.
    #[must_use]
    pub fn annex_k_chroma() -> Self {
        Self::from_natural(ANNEX_K_CHROMA_QUANT)
    }

    /// Scale this table for `quality` using libjpeg's `jpeg_quality_scaling`.
    ///
    /// `baseline` clamps entries to `1..=255` (required for `SOF0`); when it
    /// is `false` entries are clamped to `1..=32767`, which `SOF1`/`SOF2`
    /// permit and 12-bit encoding needs.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::QuantTable;
    ///
    /// // Quality 50 is the identity scaling: the table is Annex K verbatim.
    /// let scaled = QuantTable::annex_k_luma().scaled_for_quality(50, true);
    /// assert_eq!(scaled.natural()[0], 16);
    /// // Quality 75 halves it (scale = 50).
    /// let scaled = QuantTable::annex_k_luma().scaled_for_quality(75, true);
    /// assert_eq!(scaled.natural()[0], 8);
    /// ```
    #[must_use]
    pub fn scaled_for_quality(&self, quality: u8, baseline: bool) -> Self {
        let scale = quality_scaling_factor(quality);
        let max = if baseline { 255i64 } else { 32_767i64 };
        let mut out = [0u16; 64];
        for (slot, &base) in out.iter_mut().zip(self.values.iter()) {
            let scaled = (i64::from(base) * scale + 50) / 100;
            *slot = scaled.clamp(1, max) as u16;
        }
        Self::from_natural(out)
    }
}

/// libjpeg's `jpeg_quality_scaling`: the percentage applied to a base table.
///
/// # Examples
///
/// ```
/// use oxiarc_jpeg::quality_scaling_factor;
///
/// assert_eq!(quality_scaling_factor(50), 100);
/// assert_eq!(quality_scaling_factor(75), 50);
/// assert_eq!(quality_scaling_factor(100), 0);
/// assert_eq!(quality_scaling_factor(25), 200);
/// ```
#[must_use]
pub fn quality_scaling_factor(quality: u8) -> i64 {
    let q = i64::from(quality).clamp(1, 100);
    if q < 50 { 5000 / q } else { 200 - 2 * q }
}

/// Parse a `DQT` payload (everything after the length field) into the four
/// table slots, overwriting any slot the payload defines.
pub(crate) fn parse_dqt(
    payload: &[u8],
    offset: usize,
    slots: &mut [Option<QuantTable>; 4],
) -> Result<()> {
    let mut pos = 0usize;
    while pos < payload.len() {
        let pq_tq = payload[pos];
        pos += 1;
        let pq = pq_tq >> 4;
        let tq = pq_tq & 0x0F;
        if pq > 1 {
            return Err(JpegError::malformed("DQT", offset, "Pq must be 0 or 1"));
        }
        if tq > 3 {
            return Err(JpegError::malformed("DQT", offset, "Tq must be 0..=3"));
        }
        let width = if pq == 1 { 2usize } else { 1 };
        let need = 64 * width;
        if payload.len() - pos < need {
            return Err(JpegError::malformed("DQT", offset, "truncated table"));
        }
        let mut zig = [0u16; 64];
        for (k, slot) in zig.iter_mut().enumerate() {
            *slot = if pq == 1 {
                u16::from(payload[pos + 2 * k]) << 8 | u16::from(payload[pos + 2 * k + 1])
            } else {
                u16::from(payload[pos + k])
            };
        }
        pos += need;

        let mut values = [0u16; 64];
        for (k, &v) in zig.iter().enumerate() {
            if v == 0 {
                return Err(JpegError::malformed("DQT", offset, "zero quantiser value"));
            }
            values[ZIGZAG_TO_NATURAL[k]] = v;
        }
        slots[usize::from(tq)] = Some(QuantTable {
            values,
            precision: pq,
        });
    }
    Ok(())
}

/// Emit a `DQT` segment (marker, length and payload) for every defined slot.
pub(crate) fn emit_dqt(slots: &[Option<QuantTable>; 4], out: &mut Vec<u8>) {
    for (tq, table) in slots.iter().enumerate() {
        let Some(table) = table else { continue };
        let pq = table.precision.max(table.required_precision());
        let width = if pq == 1 { 2usize } else { 1 };
        let len = 2 + 1 + 64 * width;
        out.push(0xFF);
        out.push(0xDB);
        out.push((len >> 8) as u8);
        out.push((len & 0xFF) as u8);
        out.push((pq << 4) | (tq as u8));
        for &natural in &ZIGZAG_TO_NATURAL {
            let v = table.values[natural];
            if pq == 1 {
                out.push((v >> 8) as u8);
                out.push((v & 0xFF) as u8);
            } else {
                out.push(v as u8);
            }
        }
    }
}

/// Zig-zag position of a row-major index; re-exported for the encoder track.
#[must_use]
pub fn natural_to_zigzag(index: usize) -> usize {
    NATURAL_TO_ZIGZAG[index & 63]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_scaling_matches_libjpeg() {
        assert_eq!(quality_scaling_factor(0), 5000);
        assert_eq!(quality_scaling_factor(1), 5000);
        assert_eq!(quality_scaling_factor(10), 500);
        assert_eq!(quality_scaling_factor(49), 5000 / 49);
        assert_eq!(quality_scaling_factor(50), 100);
        assert_eq!(quality_scaling_factor(75), 50);
        assert_eq!(quality_scaling_factor(90), 20);
        assert_eq!(quality_scaling_factor(100), 0);
    }

    /// The exact bytes libtiff writes at its default quality 75, verified in
    /// the design investigation against a real `JPEGTables` blob.
    #[test]
    fn quality_75_luma_prefix_matches_libtiff() {
        let scaled = QuantTable::annex_k_luma().scaled_for_quality(75, true);
        let zig = scaled.zigzag();
        assert_eq!(&zig[..7], &[8, 6, 6, 7, 6, 5, 8]);
    }

    #[test]
    fn quality_75_chroma_prefix_matches_libtiff() {
        let scaled = QuantTable::annex_k_chroma().scaled_for_quality(75, true);
        let zig = scaled.zigzag();
        assert_eq!(
            &zig[..15],
            &[9, 9, 9, 12, 11, 12, 24, 13, 13, 24, 50, 33, 28, 33, 50]
        );
    }

    #[test]
    fn baseline_clamps_to_255_and_extended_to_32767() {
        let table = QuantTable::annex_k_chroma();
        let base = table.scaled_for_quality(1, true);
        assert!(base.natural().iter().all(|&v| (1..=255).contains(&v)));
        let ext = table.scaled_for_quality(1, false);
        assert!(ext.natural().iter().any(|&v| v > 255));
        assert!(ext.natural().iter().all(|&v| v <= 32_767));
        assert_eq!(ext.required_precision(), 1);
    }

    #[test]
    fn quality_100_is_all_ones() {
        let table = QuantTable::annex_k_luma().scaled_for_quality(100, true);
        assert!(table.natural().iter().all(|&v| v == 1));
    }

    #[test]
    fn dqt_round_trips_byte_exactly() {
        let mut slots: [Option<QuantTable>; 4] = [None, None, None, None];
        slots[0] = Some(QuantTable::annex_k_luma().scaled_for_quality(75, true));
        slots[1] = Some(QuantTable::annex_k_chroma().scaled_for_quality(75, true));
        let mut emitted = Vec::new();
        emit_dqt(&slots, &mut emitted);

        // Two segments of 2 + 2 + 1 + 64 bytes each.
        assert_eq!(emitted.len(), 2 * 69);
        assert_eq!(&emitted[..2], &[0xFF, 0xDB]);
        assert_eq!(&emitted[2..4], &[0x00, 0x43]);

        let mut parsed: [Option<QuantTable>; 4] = [None, None, None, None];
        parse_dqt(&emitted[4..69], 0, &mut parsed).expect("parse first");
        parse_dqt(&emitted[73..], 0, &mut parsed).expect("parse second");
        assert_eq!(parsed[0], slots[0]);
        assert_eq!(parsed[1], slots[1]);
    }

    #[test]
    fn sixteen_bit_dqt_round_trips() {
        let table = QuantTable::annex_k_chroma().scaled_for_quality(1, false);
        let mut slots: [Option<QuantTable>; 4] = [None, None, None, None];
        slots[3] = Some(table);
        let mut emitted = Vec::new();
        emit_dqt(&slots, &mut emitted);
        assert_eq!(emitted[4], 0x13, "Pq=1, Tq=3");
        assert_eq!(emitted.len(), 4 + 1 + 128);

        let mut parsed: [Option<QuantTable>; 4] = [None, None, None, None];
        parse_dqt(&emitted[4..], 0, &mut parsed).expect("parse");
        assert_eq!(parsed[3].expect("table").natural(), table.natural());
    }

    #[test]
    fn rejects_bad_dqt_payloads() {
        let mut slots: [Option<QuantTable>; 4] = [None, None, None, None];
        assert!(parse_dqt(&[0x24], 0, &mut slots).is_err(), "Pq=2");
        assert!(parse_dqt(&[0x04], 0, &mut slots).is_err(), "Tq=4");
        assert!(parse_dqt(&[0x00, 1, 2, 3], 0, &mut slots).is_err(), "short");
        let mut payload = vec![0x00];
        payload.extend(std::iter::repeat_n(1u8, 64));
        payload[1] = 0;
        assert!(parse_dqt(&payload, 0, &mut slots).is_err(), "zero entry");
    }

    #[test]
    fn zigzag_helpers_agree() {
        let table = QuantTable::annex_k_luma();
        let zig = table.zigzag();
        let rebuilt = QuantTable::from_zigzag(zig);
        assert_eq!(rebuilt.natural(), table.natural());
        assert_eq!(natural_to_zigzag(0), 0);
        assert_eq!(natural_to_zigzag(8), 2);
    }
}

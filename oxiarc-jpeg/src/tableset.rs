//! Abbreviated table streams: TIFF's `JPEGTables` tag and libjpeg's
//! `jpeg_write_tables` output.
//!
//! A tables-only datastream is `SOI (DQT | DHT | DRI | DAC | APPn | COM)* EOI`
//! with **no** `SOF` and **no** `SOS`. TIFF tag 347 holds exactly such a
//! stream and every strip or tile is a matching *scan-only* stream, complete
//! from `SOI` to `EOI` but carrying no tables of its own.
//!
//! The pair is decoded without ever concatenating them: parse the tables once
//! into a [`TableSet`], hand it to [`crate::Decoder::load_tables`] (or to
//! [`crate::decode_abbreviated_into`]) and decode each strip directly. On a
//! large tiled image that saves one allocation and one full copy per tile.

use crate::error::{JpegError, Result};
use crate::frame::ArithmeticConditioning;
use crate::huffman;
use crate::huffman::HuffmanTable;
use crate::marker::{EOI, SOI};
use crate::parser::Scanner;
use crate::quant::{QuantTable, emit_dqt, parse_dqt};

/// Which table families a tables-only stream carries.
///
/// Mirrors libtiff's `TIFFTAG_JPEGTABLESMODE` (65539): bit 0 is
/// `JPEGTABLESMODE_QUANT`, bit 1 is `JPEGTABLESMODE_HUFF`, and the default is
/// both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TablesMode {
    /// Emit the quantisation tables.
    pub quant: bool,
    /// Emit the Huffman tables.
    pub huffman: bool,
}

impl TablesMode {
    /// Both families — libtiff's default.
    pub const BOTH: Self = Self {
        quant: true,
        huffman: true,
    };
    /// Quantisation tables only (`JPEGTABLESMODE_QUANT`).
    pub const QUANT: Self = Self {
        quant: true,
        huffman: false,
    };
    /// Huffman tables only (`JPEGTABLESMODE_HUFF`).
    pub const HUFF: Self = Self {
        quant: false,
        huffman: true,
    };
    /// Neither: every strip is self-contained.
    pub const NONE: Self = Self {
        quant: false,
        huffman: false,
    };

    /// Build a mode from libtiff's bit flags.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::TablesMode;
    ///
    /// assert_eq!(TablesMode::from_bits(3), TablesMode::BOTH);
    /// assert_eq!(TablesMode::from_bits(1), TablesMode::QUANT);
    /// assert_eq!(TablesMode::from_bits(0), TablesMode::NONE);
    /// ```
    #[must_use]
    pub const fn from_bits(bits: u16) -> Self {
        Self {
            quant: bits & 1 != 0,
            huffman: bits & 2 != 0,
        }
    }

    /// The libtiff bit flags for this mode.
    #[must_use]
    pub const fn to_bits(self) -> u16 {
        (self.quant as u16) | ((self.huffman as u16) << 1)
    }
}

impl Default for TablesMode {
    fn default() -> Self {
        Self::BOTH
    }
}

/// The tables an abbreviated datastream carries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableSet {
    /// Quantisation tables by `Tq`.
    pub quant: [Option<QuantTable>; 4],
    /// DC Huffman tables by `Th`.
    pub dc_huffman: [Option<HuffmanTable>; 4],
    /// AC Huffman tables by `Th`.
    pub ac_huffman: [Option<HuffmanTable>; 4],
    /// The restart interval from `DRI`, if the stream carried one.
    pub restart_interval: Option<u16>,
    /// Arithmetic conditioning from `DAC`.
    ///
    /// Parsed and re-emitted whether or not arithmetic decoding is available,
    /// so a table stream survives a round-trip unchanged.
    ///
    /// [`TableSet::parse`] can only ever produce values inside T.81 B.2.4.3's
    /// bounds (`0 <= L <= U <= 15` per DC slot, `1 <= Kx <= 63` per AC slot)
    /// and [`Default`] is inside them, but the field is public: a caller that
    /// writes an out-of-range value here and calls [`TableSet::emit`] produces
    /// a `DAC` segment that [`TableSet::parse`] — and every conforming decoder
    /// — refuses. [`crate::table_set`] and [`crate::Encoder::write_tables_only`]
    /// build the set from [`crate::EncodeOptions`] and validate it, so the
    /// TIFF `JPEGTables` path cannot reach that state.
    pub arithmetic: ArithmeticConditioning,
}

impl TableSet {
    /// Parse a tables-only datastream.
    ///
    /// `SOI` and `EOI` are optional, `APPn`/`COM` segments are skipped, and
    /// bytes after `EOI` are ignored. A `SOF` or `SOS` is never required — and
    /// if one appears, parsing stops there rather than failing, so the same
    /// function can read the leading tables of a full datastream.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::{TableSet, TablesMode};
    ///
    /// let tables = TableSet::default();
    /// let bytes = tables.emit(TablesMode::BOTH);
    /// // An empty table set is still a well-formed SOI ... EOI stream.
    /// assert_eq!(bytes, vec![0xFF, 0xD8, 0xFF, 0xD9]);
    /// assert_eq!(TableSet::parse(&bytes).expect("round-trip"), tables);
    /// ```
    pub fn parse(tables: &[u8]) -> Result<Self> {
        let mut set = Self::default();
        let mut scanner = Scanner::new(tables);
        while let Some(segment) = scanner.next_segment()? {
            match segment.code {
                0xD8 => {}
                0xD9 => break,
                0xDB => parse_dqt(segment.payload, segment.offset, &mut set.quant)?,
                0xC4 => huffman::parse_dht(
                    segment.payload,
                    segment.offset,
                    &mut set.dc_huffman,
                    &mut set.ac_huffman,
                )?,
                0xCC => set.arithmetic.parse(segment.payload, segment.offset)?,
                0xDD => {
                    if segment.payload.len() < 2 {
                        return Err(JpegError::malformed(
                            "DRI",
                            segment.offset,
                            "segment too short",
                        ));
                    }
                    set.restart_interval =
                        Some(u16::from_be_bytes([segment.payload[0], segment.payload[1]]));
                }
                // A frame or scan header ends the table stream.
                0xDA => break,
                code if (0xC0..=0xCF).contains(&code) => break,
                _ => {}
            }
        }
        Ok(set)
    }

    /// Emit a tables-only datastream: `SOI`, the selected tables, an optional
    /// `DRI`, then `EOI`.
    ///
    /// The byte layout matches libtiff's `JPEGTables` tag exactly: `DQT`
    /// segments in `Tq` order, then `DHT` segments in slot order with each
    /// slot's DC table immediately before its AC table (`DC0 AC0 DC1 AC1`).
    ///
    /// `mode` selects only the two families libtiff's `JPEGTablesMode` knows
    /// about. `DRI` and `DAC` are written whenever the set carries them,
    /// which no libtiff-produced blob does: the T.81 default conditioning
    /// emits nothing at all. [`TableSet::arithmetic`] must hold values inside
    /// T.81 B.2.4.3's bounds; this method has no way to report that it does
    /// not.
    #[must_use]
    pub fn emit(&self, mode: TablesMode) -> Vec<u8> {
        let mut out = Vec::with_capacity(640);
        out.push(0xFF);
        out.push(SOI);
        if mode.quant {
            emit_dqt(&self.quant, &mut out);
        }
        if mode.huffman {
            huffman::emit_dht(&self.dc_huffman, &self.ac_huffman, &mut out);
        }
        self.arithmetic.emit(&mut out);
        if let Some(interval) = self.restart_interval {
            out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
            out.extend_from_slice(&interval.to_be_bytes());
        }
        out.push(0xFF);
        out.push(EOI);
        out
    }

    /// Copy every table `other` defines over this set, leaving slots `other`
    /// does not define alone.
    pub fn merge_from(&mut self, other: &TableSet) {
        for (slot, source) in self.quant.iter_mut().zip(other.quant.iter()) {
            if source.is_some() {
                *slot = *source;
            }
        }
        for (slot, source) in self.dc_huffman.iter_mut().zip(other.dc_huffman.iter()) {
            if source.is_some() {
                slot.clone_from(source);
            }
        }
        for (slot, source) in self.ac_huffman.iter_mut().zip(other.ac_huffman.iter()) {
            if source.is_some() {
                slot.clone_from(source);
            }
        }
        if other.restart_interval.is_some() {
            self.restart_interval = other.restart_interval;
        }
        if other.arithmetic != ArithmeticConditioning::default() {
            self.arithmetic = other.arithmetic;
        }
    }

    /// `true` when no table of any kind is defined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quant.iter().all(Option::is_none)
            && self.dc_huffman.iter().all(Option::is_none)
            && self.ac_huffman.iter().all(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{
        ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES, ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES,
    };

    fn libtiff_style_tables() -> TableSet {
        let mut set = TableSet::default();
        set.quant[0] = Some(QuantTable::annex_k_luma().scaled_for_quality(75, true));
        set.quant[1] = Some(QuantTable::annex_k_chroma().scaled_for_quality(75, true));
        set.dc_huffman[0] = Some(
            HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec())
                .expect("valid"),
        );
        set.ac_huffman[0] = Some(
            HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec())
                .expect("valid"),
        );
        set
    }

    #[test]
    fn emit_then_parse_is_lossless() {
        let set = libtiff_style_tables();
        let bytes = set.emit(TablesMode::BOTH);
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert_eq!(&bytes[bytes.len() - 2..], &[0xFF, 0xD9]);
        assert_eq!(TableSet::parse(&bytes).expect("parse"), set);
    }

    /// libtiff's 289-byte one-DQT/two-DHT blob and its 574-byte
    /// two-DQT/four-DHT blob are exactly what this layout produces.
    #[test]
    fn emitted_length_matches_libtiff_blobs() {
        let mut set = TableSet::default();
        set.quant[0] = Some(QuantTable::annex_k_luma().scaled_for_quality(75, true));
        set.dc_huffman[0] = Some(
            HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec())
                .expect("valid"),
        );
        set.ac_huffman[0] = Some(
            HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec())
                .expect("valid"),
        );
        assert_eq!(set.emit(TablesMode::BOTH).len(), 289);

        set.quant[1] = Some(QuantTable::annex_k_chroma().scaled_for_quality(75, true));
        set.dc_huffman[1] = set.dc_huffman[0].clone();
        set.ac_huffman[1] = set.ac_huffman[0].clone();
        assert_eq!(set.emit(TablesMode::BOTH).len(), 574);
    }

    #[test]
    fn modes_select_table_families() {
        let set = libtiff_style_tables();
        let quant_only = TableSet::parse(&set.emit(TablesMode::QUANT)).expect("parse");
        assert!(quant_only.quant[0].is_some());
        assert!(quant_only.dc_huffman[0].is_none());

        let huff_only = TableSet::parse(&set.emit(TablesMode::HUFF)).expect("parse");
        assert!(huff_only.quant[0].is_none());
        assert!(huff_only.ac_huffman[0].is_some());

        let none = TableSet::parse(&set.emit(TablesMode::NONE)).expect("parse");
        assert!(none.is_empty());
    }

    #[test]
    fn mode_bits_round_trip() {
        for bits in 0..4u16 {
            assert_eq!(TablesMode::from_bits(bits).to_bits(), bits);
        }
        assert_eq!(TablesMode::default(), TablesMode::BOTH);
    }

    #[test]
    fn restart_interval_round_trips() {
        let mut set = libtiff_style_tables();
        set.restart_interval = Some(17);
        let parsed = TableSet::parse(&set.emit(TablesMode::BOTH)).expect("parse");
        assert_eq!(parsed.restart_interval, Some(17));
    }

    #[test]
    fn arithmetic_conditioning_round_trips() {
        let mut set = TableSet::default();
        set.arithmetic.ac[2] = 9;
        let parsed = TableSet::parse(&set.emit(TablesMode::BOTH)).expect("parse");
        assert_eq!(parsed.arithmetic.ac[2], 9);
    }

    #[test]
    fn parse_tolerates_missing_soi_eoi_and_trailing_bytes() {
        let set = libtiff_style_tables();
        let full = set.emit(TablesMode::BOTH);
        let body = &full[2..full.len() - 2];
        assert_eq!(TableSet::parse(body).expect("no SOI/EOI"), set);

        let mut padded = full.clone();
        padded.extend_from_slice(&[0x00, 0x11, 0x22]);
        assert_eq!(TableSet::parse(&padded).expect("trailing"), set);
    }

    #[test]
    fn parse_skips_app_and_com_segments() {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x41, 0x42];
        bytes.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x03, 0x5A]);
        bytes.extend_from_slice(&libtiff_style_tables().emit(TablesMode::BOTH)[2..]);
        let parsed = TableSet::parse(&bytes).expect("parse");
        assert_eq!(parsed, libtiff_style_tables());
    }

    #[test]
    fn parse_stops_at_a_frame_or_scan_header() {
        let mut bytes = libtiff_style_tables().emit(TablesMode::BOTH);
        bytes.truncate(bytes.len() - 2); // drop EOI
        // A SOF followed by a DQT that must NOT be picked up.
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
        let mut trailing = TableSet::default();
        trailing.quant[3] = Some(QuantTable::annex_k_chroma());
        bytes.extend_from_slice(&trailing.emit(TablesMode::QUANT)[2..]);
        let parsed = TableSet::parse(&bytes).expect("parse");
        assert!(parsed.quant[3].is_none());
    }

    #[test]
    fn merge_overwrites_only_defined_slots() {
        let mut base = libtiff_style_tables();
        let mut other = TableSet::default();
        other.quant[0] = Some(QuantTable::annex_k_luma());
        other.restart_interval = Some(5);
        base.merge_from(&other);
        assert_eq!(base.quant[0], Some(QuantTable::annex_k_luma()));
        assert!(base.quant[1].is_some(), "untouched slot survives");
        assert!(base.dc_huffman[0].is_some());
        assert_eq!(base.restart_interval, Some(5));
    }

    #[test]
    fn empty_set_reports_empty() {
        assert!(TableSet::default().is_empty());
        assert!(!libtiff_style_tables().is_empty());
    }

    #[test]
    fn malformed_streams_are_errors_not_panics() {
        assert!(TableSet::parse(&[0xFF, 0xDB, 0x00, 0x01]).is_err());
        assert!(TableSet::parse(&[0xFF, 0xDB, 0x00, 0x43, 0x00]).is_err());
        assert!(TableSet::parse(&[0xFF, 0xDD, 0x00, 0x03, 0x01]).is_err());
        assert!(TableSet::parse(&[]).expect("empty is fine").is_empty());
    }
}

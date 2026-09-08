//! TIFF interop helpers for `Compression = 7` (TIFF Technical Note 2).
//!
//! A JPEG-compressed TIFF splits one datastream in two: tag 347 `JPEGTables`
//! holds `SOI DQT* DHT* [DRI] EOI` with no frame or scan header, and every
//! strip or tile holds `SOI SOF SOS <entropy> EOI` with no tables. The
//! preferred way to read that pair is
//! [`crate::Decoder::load_tables`] or [`crate::decode_abbreviated_into`],
//! which decode the strip against the parsed tables **without materialising a
//! merged buffer** — one `Vec` and one full copy saved per strip or tile,
//! which on a 50 000-tile GeoTIFF is the difference between fine and unusable.
//!
//! [`merge_jpeg_tables`] exists only for handing a self-contained datastream
//! to a third-party decoder that cannot take tables out of band.
//!
//! # Legacy OJPEG (`Compression = 6`)
//!
//! [`reconstruct_ojpeg`] and [`decode_ojpeg`] cover TIFF 6.0's withdrawn JPEG
//! encoding, whose strips may carry no headers at all. Their own
//! documentation covers the three spellings found in the wild and what the
//! TIFF layer has to resolve before calling in.

mod ojpeg;

pub use ojpeg::{
    OJpegGeometry, OJpegTags, decode_ojpeg, decode_ojpeg_into, decode_ojpeg_into_u16,
    reconstruct_ojpeg,
};

use crate::error::{JpegError, Result};
use crate::marker::{EOI, SOI};
use crate::tableset::{TableSet, TablesMode};

/// Parse a TIFF `JPEGTables` blob (tag 347) into a [`TableSet`].
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::tiff::parse_jpeg_tables;
///
/// let blob = &oxiarc_jpeg::sample::GRAY_1X1_TABLES;
/// let tables = parse_jpeg_tables(blob)?;
/// assert!(tables.quant[0].is_some());
/// assert!(tables.dc_huffman[0].is_some());
/// assert!(tables.ac_huffman[0].is_some());
/// # Ok(())
/// # }
/// ```
pub fn parse_jpeg_tables(tables: &[u8]) -> Result<TableSet> {
    TableSet::parse(tables)
}

/// Build a `JPEGTables` blob from a [`TableSet`], honouring libtiff's
/// `TIFFTAG_JPEGTABLESMODE`.
#[must_use]
pub fn build_jpeg_tables(tables: &TableSet, mode: TablesMode) -> Vec<u8> {
    tables.emit(mode)
}

/// Concatenate a tables-only stream and a scan-only stream into one
/// self-contained datastream.
///
/// Provided **only** for interop with third-party decoders that cannot accept
/// out-of-band tables. `oxiarc-tiff` must use [`crate::Decoder::load_tables`]
/// or [`crate::decode_abbreviated_into`] instead.
///
/// The rule is TTN2's: drop the trailing `EOI` from the tables stream and the
/// leading `SOI` from the strip, then chain them.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::tiff::merge_jpeg_tables;
///
/// let merged = merge_jpeg_tables(
///     &oxiarc_jpeg::sample::GRAY_1X1_TABLES,
///     &oxiarc_jpeg::sample::GRAY_1X1_SCAN,
/// )?;
/// assert_eq!(&merged[..2], &[0xFF, 0xD8]);
/// assert_eq!(&merged[merged.len() - 2..], &[0xFF, 0xD9]);
/// # Ok(())
/// # }
/// ```
pub fn merge_jpeg_tables(tables: &[u8], strip: &[u8]) -> Result<Vec<u8>> {
    let table_body = table_body(tables)?;
    let strip_body = strip_body(strip);
    let mut out = Vec::with_capacity(2 + table_body.len() + strip_body.len());
    out.push(0xFF);
    out.push(SOI);
    out.extend_from_slice(table_body);
    out.extend_from_slice(strip_body);
    Ok(out)
}

/// The tables stream with its leading `SOI` and trailing `EOI` removed.
fn table_body(tables: &[u8]) -> Result<&[u8]> {
    if tables.len() < 2 {
        return Err(JpegError::eof("JPEGTables"));
    }
    let start = if tables[0] == 0xFF && tables[1] == SOI {
        2
    } else {
        0
    };
    let mut end = tables.len();
    if end >= start + 2 && tables[end - 2] == 0xFF && tables[end - 1] == EOI {
        end -= 2;
    }
    if end < start {
        return Err(JpegError::eof("JPEGTables"));
    }
    Ok(&tables[start..end])
}

/// The strip with its leading `SOI` removed.
fn strip_body(strip: &[u8]) -> &[u8] {
    if strip.len() >= 2 && strip[0] == 0xFF && strip[1] == SOI {
        &strip[2..]
    } else {
        strip
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample::{GRAY_1X1_SCAN, GRAY_1X1_TABLES};
    use crate::{DecodeOptions, decode_abbreviated_into};

    #[test]
    fn parses_the_libtiff_style_blob() {
        let tables = parse_jpeg_tables(&GRAY_1X1_TABLES).expect("parse");
        assert!(tables.quant[0].is_some());
        assert!(tables.quant[1].is_none());
        assert!(tables.dc_huffman[0].is_some());
        assert!(tables.ac_huffman[0].is_some());
        assert_eq!(tables.restart_interval, None);
    }

    #[test]
    fn build_round_trips_through_parse() {
        let tables = parse_jpeg_tables(&GRAY_1X1_TABLES).expect("parse");
        let blob = build_jpeg_tables(&tables, TablesMode::BOTH);
        assert_eq!(blob, GRAY_1X1_TABLES.to_vec());
    }

    #[test]
    fn merge_produces_a_decodable_stream() {
        let merged = merge_jpeg_tables(&GRAY_1X1_TABLES, &GRAY_1X1_SCAN).expect("merge");
        assert_eq!(&merged[..2], &[0xFF, 0xD8]);
        assert_eq!(&merged[merged.len() - 2..], &[0xFF, 0xD9]);
        let mut out = [0u8; 1];
        let info = decode_abbreviated_into(None, &merged, &DecodeOptions::default(), &mut out)
            .expect("decode merged");
        assert_eq!((info.width, info.height), (1, 1));
    }

    #[test]
    fn merging_and_loading_tables_agree() {
        let merged = merge_jpeg_tables(&GRAY_1X1_TABLES, &GRAY_1X1_SCAN).expect("merge");
        let mut via_merge = [0u8; 1];
        decode_abbreviated_into(None, &merged, &DecodeOptions::default(), &mut via_merge)
            .expect("merged");

        let tables = parse_jpeg_tables(&GRAY_1X1_TABLES).expect("parse");
        let mut via_tables = [0u8; 1];
        decode_abbreviated_into(
            Some(&tables),
            &GRAY_1X1_SCAN,
            &DecodeOptions::default(),
            &mut via_tables,
        )
        .expect("abbreviated");
        assert_eq!(via_merge, via_tables);
    }

    #[test]
    fn merge_tolerates_missing_soi_and_eoi() {
        let body = &GRAY_1X1_TABLES[2..GRAY_1X1_TABLES.len() - 2];
        let merged = merge_jpeg_tables(body, &GRAY_1X1_SCAN[2..]).expect("merge");
        assert_eq!(&merged[..2], &[0xFF, 0xD8]);
        let mut out = [0u8; 1];
        decode_abbreviated_into(None, &merged, &DecodeOptions::default(), &mut out)
            .expect("decode");
    }

    #[test]
    fn merge_rejects_a_stream_that_is_too_short() {
        assert!(merge_jpeg_tables(&[0xFF], &GRAY_1X1_SCAN).is_err());
    }

    #[test]
    fn tables_mode_selects_families_on_build() {
        let tables = parse_jpeg_tables(&GRAY_1X1_TABLES).expect("parse");
        let quant_only = build_jpeg_tables(&tables, TablesMode::QUANT);
        let reparsed = parse_jpeg_tables(&quant_only).expect("parse");
        assert!(reparsed.quant[0].is_some());
        assert!(reparsed.dc_huffman[0].is_none());
    }
}

//! Compression 2, 3, 4 and 32771: the CCITT fax codes.
//!
//! * **2 — `CCITTRLE`**, Modified Huffman: one-dimensional runs, every row
//!   padded to a byte boundary, no end-of-line codes.
//! * **32771 — `CCITTRLEW`**: the same code, rows padded to a 16-bit boundary.
//! * **3 — Group 3 (T.4)**: `T4Options` (tag 292) bit 0 allows two-dimensional
//!   rows, bit 1 allows uncompressed mode, bit 2 (`EncodedByteAlign`) pads with
//!   fill bits so every end-of-line code *ends* on a byte boundary. With bit 0
//!   set, each EOL is followed by one tag bit: `1` means the next row is coded
//!   one-dimensionally, `0` two-dimensionally.
//! * **4 — Group 4 (T.6, MMR)**: two-dimensional only, no end-of-line codes,
//!   terminated by `EOFB` (two EOLs). `T6Options` (tag 293) bit 1 allows
//!   uncompressed mode.
//!
//! # Two things this module does *not* take from the design report
//!
//! Both were measured against libtiff 4.7.1 rather than assumed.
//!
//! 1. **The codes are photometric-agnostic.** `tiffcp -c g3` and `-c g4` write
//!    byte-identical strips for the same bits whether the page is
//!    `MinIsWhite` or `MinIsBlack`, and a `tiffcp -c none` round trip returns
//!    the original bits either way: a coded *white* run is a run of **zero**
//!    bits, always. `PhotometricInterpretation` decides how those bits are
//!    displayed, not how they are coded — which is why this module never looks
//!    at it.
//! 2. **libtiff writes no RTC.** T.4 ends a page with six EOLs; `tiffcp -c g3`
//!    writes the last row and stops. Group 4 does get its `EOFB`. This module
//!    writes what libtiff writes and accepts both on read.
//!
//! # `FillOrder`
//!
//! The CCITT codecs consume `FillOrder` themselves (libtiff's
//! `Fax3SetupState` sets `TIFF_NOBITREV`, so the chunk-level reversal is
//! skipped for exactly these methods — see
//! [`handles_fill_order`](super::handles_fill_order)). The bit reader and
//! writer reverse each byte as they go, so no buffer is ever reversed twice.
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, encode, CodecContext, CodecLevel};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! // Four rows of eight pixels: 0 is white, 1 is black.
//! let rows = [0b1111_0000u8, 0b0000_1111, 0b1010_1010, 0b0000_0000];
//! let cx = CodecContext::new(CompressionMethod::CcittFax4, 8, 4, &[1], 1, Endian::Little);
//! let coded = encode(&rows, &cx, CodecLevel::Default)?;
//! let mut out = [0u8; 4];
//! assert_eq!(decode_into(&coded, &mut out, &cx)?, 4);
//! assert_eq!(out, rows);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

mod bits;
mod decode;
mod encode;
mod tables;
mod uncompressed;

use bits::BitWriter;
use decode::{FaxDecoder, RowFault};

use super::pool::Pool;
use super::{CodecContext, codec_error};
use crate::error::{Result, TiffError, UnsupportedError};
use crate::tags::{CompressionMethod, FillOrder};

/// The changing-element buffers of one image, kept between chunks.
///
/// A row is a list of changing elements, so the fax engine needs two growable
/// buffers whose size follows the row's *content*, not its width. Allocating
/// them per strip is what the design report's "zero per-chunk allocations in
/// the steady state" forbids, so [`CodecState`](crate::CodecState) owns a
/// [`Pool`] of pairs per image and every chunk borrows one.
///
/// A pool rather than one pair, for the reason the inflate machines next door
/// are pooled: a chunk holds its pair for as long as it decodes, so a
/// `rayon` decode of a fax page would otherwise serialise every worker behind
/// one mutex. Both buffers are cleared at the head of every row, so a pair
/// carries nothing between chunks but its capacity.
#[derive(Default)]
pub(crate) struct FaxScratch(Pool<(Vec<u32>, Vec<u32>)>);

impl core::fmt::Debug for FaxScratch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The largest pooled capacity, which is what "the buffers survived"
        // means; an empty pool reports zero.
        let held = self.0.inspect(|entries| {
            entries
                .iter()
                .map(|(changes, _)| changes.capacity())
                .max()
                .unwrap_or(0)
        });
        f.debug_tuple("FaxScratch").field(&held).finish()
    }
}

impl FaxScratch {
    /// Runs `body` on a decoder over a pooled pair, returning it after.
    fn with<T>(
        &self,
        data: &[u8],
        width: u32,
        reversed: bool,
        body: impl FnOnce(&mut FaxDecoder<'_>) -> T,
    ) -> T {
        let scratch = self.0.take().unwrap_or_default();
        let mut decoder = FaxDecoder::with_scratch(data, width, reversed, scratch);
        let out = body(&mut decoder);
        self.0.put(decoder.into_scratch());
        out
    }
}

/// How a row is coded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowCoding {
    /// Modified Huffman runs.
    OneDimensional,
    /// T.4/T.6 modes against the row above.
    TwoDimensional,
}

/// What to do at the start of every row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowAlignment {
    /// Nothing: rows follow each other bit by bit.
    None,
    /// Pad to the next byte (compression 2).
    Byte,
    /// Pad to the next 16-bit word (compression 32771).
    Word,
}

/// The dialect one compression value selects.
#[derive(Clone, Copy, Debug)]
struct Dialect {
    /// Whether end-of-line codes delimit rows.
    end_of_line: bool,
    /// Whether two-dimensional rows are allowed.
    two_dimensional: bool,
    /// Whether every row is coded two-dimensionally (Group 4).
    always_two_dimensional: bool,
    /// Whether EOLs are padded to end on a byte boundary.
    byte_align: bool,
    /// Row padding for the RLE dialects.
    alignment: RowAlignment,
}

impl Dialect {
    /// The dialect for one compression value and its option tags.
    fn of(cx: &CodecContext<'_>) -> Result<Self> {
        Ok(match cx.compression {
            CompressionMethod::CcittRle => Self {
                end_of_line: false,
                two_dimensional: false,
                always_two_dimensional: false,
                byte_align: false,
                alignment: RowAlignment::Byte,
            },
            CompressionMethod::CcittRleWord => Self {
                end_of_line: false,
                two_dimensional: false,
                always_two_dimensional: false,
                byte_align: false,
                alignment: RowAlignment::Word,
            },
            CompressionMethod::CcittFax3 => Self {
                end_of_line: true,
                two_dimensional: cx.t4_options.two_dimensional(),
                always_two_dimensional: false,
                byte_align: cx.t4_options.byte_aligned_eol(),
                alignment: RowAlignment::None,
            },
            CompressionMethod::CcittFax4 => Self {
                end_of_line: false,
                two_dimensional: true,
                always_two_dimensional: true,
                byte_align: false,
                alignment: RowAlignment::None,
            },
            other => {
                return Err(TiffError::Unsupported(UnsupportedError::Compression(
                    other.to_u16(),
                )));
            }
        })
    }
}

/// Rejects the geometries the fax codes are not defined for.
fn check_geometry(cx: &CodecContext<'_>) -> Result<(u32, u32)> {
    if cx.samples_per_pixel != 1 || cx.bits_per_sample.iter().any(|bits| *bits != 1) {
        return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
            cx.bits_per_sample.to_vec(),
        )));
    }
    let width = u32::try_from(cx.width).map_err(|_| TiffError::IntOverflow)?;
    let height = u32::try_from(cx.height).map_err(|_| TiffError::IntOverflow)?;
    if width == 0 {
        return Err(TiffError::Format(
            crate::error::FormatError::ZeroDimension { width, height },
        ));
    }
    Ok((width, height))
}

/// Turns a row-level fault into the error the message should carry.
fn fault_error(method: CompressionMethod, row: u32, fault: RowFault) -> TiffError {
    let message = match fault {
        RowFault::InvalidCode { position } => {
            format!("row {row}: no code word at bit {position}")
        }
        RowFault::Overrun { position } => {
            format!("row {row}: a run ends at pixel {position}, past the row")
        }
        RowFault::Truncated => format!("row {row}: the chunk ended mid-row"),
        RowFault::ShortRow { missing } => {
            format!("row {row}: an end-of-line code arrived {missing} pixels early")
        }
        RowFault::UncompressedMode => format!(
            "row {row}: uncompressed mode (T.4 §4.2.1.3) is not decodable; \
             libtiff 4.7.1 reports the same data as \"Uncompressed data (not supported)\""
        ),
        RowFault::UnknownExtension { code } => {
            format!("row {row}: extension code {code:03b} is not defined")
        }
    };
    codec_error(method, message)
}

/// Decodes one CCITT strip or tile into `dst`.
///
/// Returns the number of bytes written: whole rows only, so a short return
/// always means "this many complete rows".
///
/// # Errors
/// [`crate::FormatError::Codec`] naming the row and the bit offset for a
/// stream defect, and [`UnsupportedError::BitsPerSample`] for anything that is
/// not single-channel bilevel data. Under [`crate::Leniency::Lenient`] a defect stops
/// the decode and the rest of the chunk is filled white instead.
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let (width, height) = check_geometry(cx)?;
    let dialect = Dialect::of(cx)?;
    let row_bytes = (width as usize).div_ceil(8);
    if row_bytes == 0 {
        return Ok(0);
    }
    let reversed = cx.fill_order == FillOrder::Lsb2Msb;
    match cx.state {
        // The image's buffers: no allocation after the first chunk.
        Some(state) => state.fax().with(src, width, reversed, |decoder| {
            run(decoder, dst, cx, dialect, (width, height, row_bytes))
        }),
        // A codec called without an image behind it still has to work.
        None => run(
            &mut FaxDecoder::new(src, width, reversed),
            dst,
            cx,
            dialect,
            (width, height, row_bytes),
        ),
    }
}

/// Decodes the rows of one chunk with a decoder that is already built.
fn run(
    decoder: &mut FaxDecoder<'_>,
    dst: &mut [u8],
    cx: &CodecContext<'_>,
    dialect: Dialect,
    geometry: (u32, u32, usize),
) -> Result<usize> {
    let (width, height, row_bytes) = geometry;
    let rows = height.min((dst.len() / row_bytes) as u32);
    let mut coding = if dialect.always_two_dimensional {
        RowCoding::TwoDimensional
    } else {
        RowCoding::OneDimensional
    };
    let mut produced = 0usize;

    for row in 0..rows {
        let start = (row as usize) * row_bytes;
        let Some(slot) = dst.get_mut(start..start + row_bytes) else {
            break;
        };
        match dialect.alignment {
            RowAlignment::None => {}
            RowAlignment::Byte => decoder.bits.align_to_byte(),
            RowAlignment::Word => decoder.bits.align_to_word(),
        }
        if dialect.end_of_line {
            let had_eol = decoder.consume_eol();
            if dialect.two_dimensional {
                if had_eol {
                    // The tag bit: 1 = the next row is one-dimensional.
                    coding = if decoder.bits.read_bit() == 1 {
                        RowCoding::OneDimensional
                    } else {
                        RowCoding::TwoDimensional
                    };
                } else if row == 0 {
                    // No EOL at all: the first row of a strip can only be 1D,
                    // because there is no reference line to code against.
                    coding = RowCoding::OneDimensional;
                }
            }
        }
        if decoder.bits.remaining() == 0
            || (decoder.bits.remaining() < 64 && decoder.bits.rest_is_padding())
        {
            // Out of data, or only fill bits are left: the caller decides
            // whether a short chunk is fatal. The bound has to be *zero*
            // bits, not "too few to be interesting": a two-dimensional row
            // that repeats the row above is a single `V0` bit, so a strip can
            // legitimately end with one bit of data left to read.
            break;
        }
        if dialect.always_two_dimensional && decoder.consume_eol_here() {
            // `EOFB` — the rows really did end here.
            break;
        }
        let outcome = match coding {
            RowCoding::OneDimensional => decoder.decode_1d_row(),
            RowCoding::TwoDimensional => decoder.decode_2d_row(),
        };
        match outcome {
            Ok(()) => {
                decoder.paint(slot, 0);
                decoder.promote_row();
                produced += row_bytes;
            }
            Err(fault) => {
                let recoverable = matches!(fault, RowFault::ShortRow { .. });
                if cx.leniency.is_strict() || (!recoverable && !cx.leniency.is_lenient()) {
                    // A truncated chunk is not a stream defect: report what
                    // decoded and let the pipeline apply its own policy.
                    if fault == RowFault::Truncated && !cx.leniency.is_strict() {
                        return Ok(produced);
                    }
                    return Err(fault_error(cx.compression, row, fault));
                }
                if recoverable {
                    if let RowFault::ShortRow { missing } = fault {
                        decoder.truncate_row(width.saturating_sub(missing));
                    }
                    decoder.paint(slot, 0);
                    decoder.promote_row();
                    produced += row_bytes;
                    continue;
                }
                // Lenient: keep what decoded and leave the rest white.
                fill_white(dst, start, 0);
                return Ok(dst.len());
            }
        }
    }
    if cx.leniency.is_lenient() && produced < dst.len() {
        fill_white(dst, produced, 0);
        return Ok(dst.len());
    }
    Ok(produced)
}

/// Fills `dst[from..]` with white pixels.
fn fill_white(dst: &mut [u8], from: usize, white_bit: u8) {
    if let Some(tail) = dst.get_mut(from..) {
        FaxDecoder::paint_white(tail, white_bit);
    }
}

/// Encodes one strip or tile.
///
/// The output matches `tiffcp` for the same dialect: Group 3 gets an
/// end-of-line code before every row (plus the tag bit when `T4Options` allows
/// two-dimensional rows) and no RTC; Group 4 gets `EOFB` after the last row;
/// the RLE dialects pad every row to a byte or word boundary.
///
/// # Errors
/// [`UnsupportedError::BitsPerSample`] for anything that is not single-channel
/// bilevel data.
pub fn encode(src: &[u8], cx: &CodecContext<'_>) -> Result<Vec<u8>> {
    let (width, height) = check_geometry(cx)?;
    let dialect = Dialect::of(cx)?;
    let row_bytes = (width as usize).div_ceil(8);
    let reversed = cx.fill_order == FillOrder::Lsb2Msb;
    let mut writer = BitWriter::new();
    let mut changes: Vec<u32> = Vec::with_capacity(64);
    let mut reference: Vec<u32> = Vec::with_capacity(64);
    reference.clear();

    for row in 0..height {
        let start = (row as usize) * row_bytes;
        let slot = src.get(start..start + row_bytes).unwrap_or(&[]);
        encode::row_changes(slot, width, 0, &mut changes);
        let two_dimensional =
            dialect.always_two_dimensional || (dialect.two_dimensional && row > 0);
        if dialect.end_of_line {
            encode::write_eol(&mut writer, dialect.byte_align);
            if dialect.two_dimensional {
                writer.write(u16::from(!two_dimensional), 1);
            }
        }
        match dialect.alignment {
            RowAlignment::None => {}
            RowAlignment::Byte => writer.align_to_byte(),
            RowAlignment::Word => writer.align_to_word(),
        }
        if two_dimensional {
            encode::encode_2d_row(&mut writer, &changes, &reference, width);
        } else {
            encode::encode_1d_row(&mut writer, &changes, width);
        }
        core::mem::swap(&mut reference, &mut changes);
    }
    if dialect.always_two_dimensional {
        // `EOFB`, exactly as libtiff writes it.
        encode::write_eol(&mut writer, false);
        encode::write_eol(&mut writer, false);
    }
    match dialect.alignment {
        RowAlignment::None => {}
        RowAlignment::Byte => writer.align_to_byte(),
        RowAlignment::Word => writer.align_to_word(),
    }
    Ok(writer.finish(reversed))
}

/// Whether a chunk of this dialect may be re-encoded losslessly.
///
/// Used by the writer's own tests and by [`crate::Encoder`] documentation: the
/// fax codes are lossless for bilevel data and defined for nothing else.
#[must_use]
pub const fn is_bilevel_only() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::tags::{PhotometricInterpretation, T4Options};

    fn context<'a>(
        method: CompressionMethod,
        width: usize,
        height: usize,
        bits: &'a [u16],
    ) -> CodecContext<'a> {
        CodecContext::new(method, width, height, bits, 1, Endian::Little)
    }

    /// A deterministic bilevel image, packed one bit per pixel.
    fn image(width: usize, height: usize) -> Vec<u8> {
        let row_bytes = width.div_ceil(8);
        let mut data = vec![0u8; row_bytes * height];
        for y in 0..height {
            for x in 0..width {
                let black = ((x / 3) + y) % 5 == 0 || (x > width / 2 && y % 7 == 0);
                if black {
                    data[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        data
    }

    fn round_trip(method: CompressionMethod, options: u32, width: usize, height: usize) {
        let bits = [1u16];
        let mut cx = context(method, width, height, &bits);
        cx.t4_options = T4Options::from_u32(options);
        let data = image(width, height);
        let coded = encode(&data, &cx).expect("encode");
        let mut out = vec![0u8; data.len()];
        let written = decode_into(&coded, &mut out, &cx).expect("decode");
        assert_eq!(written, data.len(), "{method} {options:#x}");
        assert_eq!(out, data, "{method} {options:#x} {width}x{height}");
    }

    #[test]
    fn the_changing_element_buffers_are_reused_between_chunks() {
        // The pooled buffers must survive from one chunk to the next: this is
        // the fax half of "no per-chunk allocations in the steady state".
        let bits = [1u16];
        let state = crate::CodecState::new();
        let data = image(200, 8);
        let mut cx = context(CompressionMethod::CcittFax4, 200, 8, &bits);
        let coded = encode(&data, &cx).expect("encode");
        cx.state = Some(&state);

        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&coded, &mut out, &cx).expect("first chunk"),
            data.len()
        );
        assert_eq!(out, data);
        let after_first = format!("{:?}", state.fax());
        assert_ne!(
            after_first, "FaxScratch(0)",
            "the first chunk left no capacity behind"
        );

        // A second chunk reuses that capacity instead of allocating again.
        out.iter_mut().for_each(|byte| *byte = 0);
        assert_eq!(
            decode_into(&coded, &mut out, &cx).expect("second chunk"),
            data.len()
        );
        assert_eq!(out, data);
        assert_eq!(format!("{:?}", state.fax()), after_first);
    }

    #[test]
    fn a_pooled_decode_matches_an_unpooled_one() {
        // Reuse must not leak a row of the previous chunk into the next.
        let bits = [1u16];
        let state = crate::CodecState::new();
        for (width, height) in [(13, 5), (200, 8), (61, 3)] {
            for method in [
                CompressionMethod::CcittRle,
                CompressionMethod::CcittFax3,
                CompressionMethod::CcittFax4,
            ] {
                let data = image(width, height);
                let cx = context(method, width, height, &bits);
                let coded = encode(&data, &cx).expect("encode");
                let mut pooled = vec![0u8; data.len()];
                let mut fresh = vec![0u8; data.len()];
                let mut with_state = context(method, width, height, &bits);
                with_state.state = Some(&state);
                assert_eq!(
                    decode_into(&coded, &mut pooled, &with_state).expect("pooled"),
                    decode_into(&coded, &mut fresh, &cx).expect("fresh")
                );
                assert_eq!(pooled, fresh, "{method} {width}x{height}");
                assert_eq!(pooled, data, "{method} {width}x{height}");
            }
        }
    }

    #[test]
    fn every_dialect_round_trips() {
        for (width, height) in [(8, 4), (13, 9), (64, 3), (100, 10), (1728, 4), (2600, 2)] {
            round_trip(CompressionMethod::CcittRle, 0, width, height);
            round_trip(CompressionMethod::CcittRleWord, 0, width, height);
            round_trip(CompressionMethod::CcittFax3, 0, width, height);
            round_trip(CompressionMethod::CcittFax3, 1, width, height);
            round_trip(CompressionMethod::CcittFax3, 4, width, height);
            round_trip(CompressionMethod::CcittFax3, 5, width, height);
            round_trip(CompressionMethod::CcittFax4, 0, width, height);
        }
    }

    #[test]
    fn fill_order_two_round_trips_through_the_codec_itself() {
        let bits = [1u16];
        let data = image(37, 6);
        for method in [
            CompressionMethod::CcittRle,
            CompressionMethod::CcittFax3,
            CompressionMethod::CcittFax4,
        ] {
            let mut cx = context(method, 37, 6, &bits);
            cx.fill_order = FillOrder::Lsb2Msb;
            let coded = encode(&data, &cx).expect("encode");
            let mut plain = context(method, 37, 6, &bits);
            plain.fill_order = FillOrder::Msb2Lsb;
            let mut reversed = coded.clone();
            for byte in &mut reversed {
                *byte = byte.reverse_bits();
            }
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&coded, &mut out, &cx).expect("decode"),
                data.len()
            );
            assert_eq!(out, data, "{method}");
            let mut out2 = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&reversed, &mut out2, &plain).expect("decode"),
                data.len()
            );
            assert_eq!(out2, data, "{method} reversed by hand");
        }
    }

    #[test]
    fn the_codes_ignore_the_photometric_interpretation() {
        // Measured: `tiffcp -c g4` writes byte-identical strips for
        // MinIsWhite and MinIsBlack pages, so neither may change our output.
        let bits = [1u16];
        let data = image(24, 4);
        let mut white = context(CompressionMethod::CcittFax4, 24, 4, &bits);
        white.photometric = PhotometricInterpretation::WhiteIsZero;
        let mut black = context(CompressionMethod::CcittFax4, 24, 4, &bits);
        black.photometric = PhotometricInterpretation::BlackIsZero;
        let a = encode(&data, &white).expect("encode");
        let b = encode(&data, &black).expect("encode");
        assert_eq!(a, b);
        let mut out = vec![0u8; data.len()];
        decode_into(&a, &mut out, &black).expect("decode");
        assert_eq!(out, data);
    }

    #[test]
    fn a_group_four_strip_ends_with_eofb() {
        let bits = [1u16];
        let cx = context(CompressionMethod::CcittFax4, 8, 2, &bits);
        let coded = encode(&[0u8, 0], &cx).expect("encode");
        // Two rows of eight white pixels: V0 twice per row is impossible
        // (there is no change), so each row is one horizontal code; then EOFB.
        let mut reader = bits::BitReader::new(&coded, false);
        let mut eols = 0;
        while reader.remaining() >= 12 {
            if reader.peek(12) == 1 {
                eols += 1;
                reader.skip(12);
            } else {
                reader.skip(1);
            }
        }
        assert!(eols >= 2, "EOFB must be present");
    }

    #[test]
    fn a_strip_may_end_with_a_single_bit_row() {
        // A two-dimensional row that repeats the row above is one `V0` bit, so
        // a strip can legitimately end with a single bit of data left. A
        // decoder that stops when "too few bits are left to be interesting"
        // silently drops that row — which is exactly what this crate did until
        // a 60 000-case sweep of the fax round trip found an 8x28 page with
        // five-row strips.
        let bits = [1u16];
        for (width, height, method, options) in [
            (8usize, 5usize, CompressionMethod::CcittFax3, 1u32),
            (16, 11, CompressionMethod::CcittFax3, 1),
            (8, 5, CompressionMethod::CcittFax4, 0),
            (24, 7, CompressionMethod::CcittFax4, 0),
        ] {
            let row_bytes = width.div_ceil(8);
            // Every row identical: after the first, each is one `V0` per
            // changing element and nothing else.
            let mut data = vec![0u8; row_bytes * height];
            for row in 0..height {
                data[row * row_bytes] = 0b1110_0000;
            }
            let mut cx = context(method, width, height, &bits);
            cx.t4_options = T4Options::from_u32(options);
            let coded = encode(&data, &cx).expect("encode");
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&coded, &mut out, &cx).expect("decode"),
                data.len(),
                "{method} {width}x{height}: the last row was dropped"
            );
            assert_eq!(out, data, "{method} {width}x{height}");
        }
    }

    #[test]
    fn a_truncated_chunk_reports_whole_rows_only() {
        let bits = [1u16];
        let cx = context(CompressionMethod::CcittFax4, 64, 8, &bits);
        let data = image(64, 8);
        let coded = encode(&data, &cx).expect("encode");
        let row_bytes = 8usize;
        for cut in 1..coded.len() {
            let mut out = vec![0u8; data.len()];
            match decode_into(&coded[..cut], &mut out, &cx) {
                Ok(written) => {
                    assert_eq!(written % row_bytes, 0, "cut {cut}");
                    assert!(written <= data.len());
                    assert_eq!(
                        out.get(..written),
                        data.get(..written),
                        "rows decoded before the cut must be right (cut {cut})"
                    );
                }
                Err(err) => assert!(err.to_string().contains("compression 4"), "{err}"),
            }
        }
    }

    #[test]
    fn a_lenient_reader_fills_the_rest_of_a_broken_chunk_with_white() {
        let bits = [1u16];
        let mut cx = context(CompressionMethod::CcittFax4, 64, 8, &bits);
        cx.leniency = crate::limits::Leniency::Lenient;
        let data = image(64, 8);
        let mut coded = encode(&data, &cx).expect("encode");
        for byte in coded.iter_mut().skip(3).take(4) {
            *byte = 0xff;
        }
        let mut out = vec![0u8; data.len()];
        let written = decode_into(&coded, &mut out, &cx).expect("lenient");
        assert_eq!(written, data.len());
    }

    #[test]
    fn strict_rejects_what_lenient_repairs() {
        let bits = [1u16];
        let mut strict = context(CompressionMethod::CcittFax4, 64, 8, &bits);
        strict.leniency = crate::limits::Leniency::Strict;
        let data = image(64, 8);
        let mut coded = encode(&data, &strict).expect("encode");
        for byte in coded.iter_mut().skip(3).take(4) {
            *byte = 0xff;
        }
        let mut out = vec![0u8; data.len()];
        assert!(decode_into(&coded, &mut out, &strict).is_err());
    }

    #[test]
    fn multi_channel_or_deep_data_is_refused() {
        let bits = [8u16];
        let cx = context(CompressionMethod::CcittFax4, 8, 1, &bits);
        assert!(matches!(
            decode_into(&[0; 4], &mut [0u8; 8], &cx),
            Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(_)))
        ));
        assert!(matches!(
            encode(&[0; 8], &cx),
            Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(_)))
        ));
    }

    #[test]
    fn uncompressed_mode_is_named_the_way_libtiff_names_it() {
        let bits = [1u16];
        let cx = context(CompressionMethod::CcittFax4, 32, 1, &bits);
        // `0000001111` is the uncompressed-mode extension code.
        let mut writer = BitWriter::new();
        writer.write(0b00_0000_1111, 10);
        let coded = writer.finish(false);
        let mut out = vec![0u8; 4];
        let err = decode_into(&coded, &mut out, &cx).expect_err("named error");
        assert!(err.to_string().contains("uncompressed mode"), "{err}");
    }

    #[test]
    fn an_all_white_and_an_all_black_page_round_trip() {
        let bits = [1u16];
        for fill in [0x00u8, 0xff] {
            for method in [
                CompressionMethod::CcittRle,
                CompressionMethod::CcittFax3,
                CompressionMethod::CcittFax4,
            ] {
                let cx = context(method, 100, 5, &bits);
                let data = vec![fill; 13 * 5];
                let coded = encode(&data, &cx).expect("encode");
                let mut out = vec![0u8; data.len()];
                assert_eq!(
                    decode_into(&coded, &mut out, &cx).expect("decode"),
                    data.len()
                );
                // The bits past the row width are padding and are not
                // required to round trip; compare the pixels only.
                for row in 0..5 {
                    for x in 0..100 {
                        let index = row * 13 + x / 8;
                        let mask = 0x80 >> (x % 8);
                        assert_eq!(out[index] & mask, data[index] & mask, "{method} {fill:#x}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_dispatch_routes_every_ccitt_value_here() {
        use crate::compression::{
            CodecLevel, decode_into as dispatch_decode, encode as dispatch_encode,
        };
        let bits = [1u16];
        let data = image(32, 3);
        for method in [
            CompressionMethod::CcittRle,
            CompressionMethod::CcittRleWord,
            CompressionMethod::CcittFax3,
            CompressionMethod::CcittFax4,
        ] {
            let cx = context(method, 32, 3, &bits);
            let coded = dispatch_encode(&data, &cx, CodecLevel::Default).expect("encode");
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                dispatch_decode(&coded, &mut out, &cx).expect("decode"),
                data.len(),
                "{method}"
            );
            assert_eq!(out, data, "{method}");
        }
    }

    #[test]
    fn the_dialect_table_covers_every_ccitt_value() {
        let bits = [1u16];
        for method in [
            CompressionMethod::CcittRle,
            CompressionMethod::CcittRleWord,
            CompressionMethod::CcittFax3,
            CompressionMethod::CcittFax4,
        ] {
            let cx = context(method, 8, 1, &bits);
            assert!(Dialect::of(&cx).is_ok(), "{method}");
        }
        let cx = context(CompressionMethod::Lzw, 8, 1, &bits);
        assert!(Dialect::of(&cx).is_err());
        assert!(is_bilevel_only());
    }
}

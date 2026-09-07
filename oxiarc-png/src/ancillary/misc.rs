//! Structural and metadata chunks: `PLTE`, `tRNS`, `pHYs`, `tIME`, `oFFs`,
//! `sCAL`, `pCAL` and `sTER`.

use crate::ancillary::{be_i32, be_u32, split_null};
use crate::chunk;
use crate::common::{PixelDimensions, StereoLayout, Unit};
use crate::error::{DecodingError, FormatErrorKind};
use crate::header::{BitDepth, ColorType};
use crate::info::{ImageOffset, OffsetUnit, PhysicalScale, PixelCalibration, ScalUnit, Time};
use crate::text_metadata::{decode_latin1, validate_keyword_bytes};

/// Validate a `PLTE` chunk and return its bytes.
///
/// The length must be a non-zero multiple of three and at most 768 bytes, and
/// the chunk is forbidden on grayscale colour types.
pub fn parse_plte(data: &[u8], color_type: ColorType) -> Result<Vec<u8>, DecodingError> {
    if data.is_empty() || data.len() % 3 != 0 || data.len() > 768 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::PLTE }.into());
    }
    if matches!(color_type, ColorType::Grayscale | ColorType::GrayscaleAlpha) {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::PLTE }.into());
    }
    Ok(data.to_vec())
}

/// The result of parsing a `tRNS` chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trns {
    /// The value stored in [`crate::Info::trns`]: truncated to one byte for
    /// grayscale and three for RGB when the bit depth is below 16, which is
    /// what the `png` crate does and what the row transforms expect.
    pub normalized: Vec<u8>,
    /// The chunk's bytes exactly as stored.
    pub raw: Vec<u8>,
}

/// Parse and normalise a `tRNS` chunk.
///
/// * Grayscale needs exactly two bytes, RGB exactly six.
/// * Indexed takes one alpha byte per palette entry; a chunk with **more**
///   entries than the palette is ignored entirely, matching the `png` crate.
/// * `GrayscaleAlpha` and `Rgba` may not carry `tRNS` at all.
pub fn parse_trns(
    data: &[u8],
    color_type: ColorType,
    bit_depth: BitDepth,
    palette_entries: usize,
) -> Result<Option<Trns>, DecodingError> {
    let raw = data.to_vec();
    match color_type {
        ColorType::GrayscaleAlpha | ColorType::Rgba => {
            Err(FormatErrorKind::ColorWithBadTrns.into())
        }
        ColorType::Grayscale => {
            if data.len() < 2 {
                return Err(FormatErrorKind::ShortPalette {
                    expected: 2,
                    len: data.len(),
                }
                .into());
            }
            let normalized = if bit_depth == BitDepth::Sixteen {
                data[..2].to_vec()
            } else {
                vec![data[1]]
            };
            Ok(Some(Trns { normalized, raw }))
        }
        ColorType::Rgb => {
            if data.len() < 6 {
                return Err(FormatErrorKind::ShortPalette {
                    expected: 6,
                    len: data.len(),
                }
                .into());
            }
            let normalized = if bit_depth == BitDepth::Sixteen {
                data[..6].to_vec()
            } else {
                vec![data[1], data[3], data[5]]
            };
            Ok(Some(Trns { normalized, raw }))
        }
        ColorType::Indexed => {
            if palette_entries == 0 {
                return Err(FormatErrorKind::PaletteRequired.into());
            }
            if data.len() > palette_entries {
                // Oversized indexed tRNS is discarded rather than fatal.
                return Ok(None);
            }
            Ok(Some(Trns {
                normalized: raw.clone(),
                raw,
            }))
        }
    }
}

/// Parse a `pHYs` chunk: two resolutions and a unit byte.
pub fn parse_phys(data: &[u8]) -> Result<PixelDimensions, DecodingError> {
    if data.len() != 9 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::pHYs }.into());
    }
    let unit =
        Unit::from_u8(data[8]).ok_or(FormatErrorKind::MalformedChunk { kind: chunk::pHYs })?;
    Ok(PixelDimensions {
        xppu: be_u32(data, 0, chunk::pHYs)?,
        yppu: be_u32(data, 4, chunk::pHYs)?,
        unit,
    })
}

/// Parse a `tIME` chunk: a seven-byte UTC timestamp.
pub fn parse_time(data: &[u8]) -> Result<Time, DecodingError> {
    if data.len() != 7 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::tIME }.into());
    }
    let time = Time {
        year: u16::from_be_bytes([data[0], data[1]]),
        month: data[2],
        day: data[3],
        hour: data[4],
        minute: data[5],
        second: data[6],
    };
    let valid = (1..=12).contains(&time.month)
        && (1..=31).contains(&time.day)
        && time.hour <= 23
        && time.minute <= 59
        && time.second <= 60;
    if !valid {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::tIME }.into());
    }
    Ok(time)
}

/// Parse an `oFFs` chunk: a signed position and a unit byte.
pub fn parse_offs(data: &[u8]) -> Result<ImageOffset, DecodingError> {
    if data.len() != 9 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::oFFs }.into());
    }
    let unit = match data[8] {
        0 => OffsetUnit::Pixel,
        1 => OffsetUnit::Micrometer,
        _ => return Err(FormatErrorKind::MalformedChunk { kind: chunk::oFFs }.into()),
    };
    Ok(ImageOffset {
        x: be_i32(data, 0, chunk::oFFs)?,
        y: be_i32(data, 4, chunk::oFFs)?,
        unit,
    })
}

/// True when `s` is an ASCII floating-point number in the form `sCAL` and
/// `pCAL` use: an optional sign, digits, an optional fraction and an optional
/// exponent.
fn is_ascii_float(s: &[u8]) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut i = 0;
    if s[i] == b'+' || s[i] == b'-' {
        i += 1;
    }
    let digits_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - digits_start;
    let mut frac_digits = 0;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - start;
    }
    if int_digits + frac_digits == 0 {
        return false;
    }
    if i < s.len() && (s[i] == b'e' || s[i] == b'E') {
        i += 1;
        if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
            i += 1;
        }
        let start = i;
        while i < s.len() && s[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == s.len()
}

/// Parse an `sCAL` chunk: a unit byte and two ASCII floating-point strings.
pub fn parse_scal(data: &[u8]) -> Result<PhysicalScale, DecodingError> {
    let unit = match data.first() {
        Some(1) => ScalUnit::Meter,
        Some(2) => ScalUnit::Radian,
        _ => return Err(FormatErrorKind::MalformedChunk { kind: chunk::sCAL }.into()),
    };
    let (width, height) = split_null(&data[1..], chunk::sCAL)?;
    if !is_ascii_float(width) || !is_ascii_float(height) {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::sCAL }.into());
    }
    Ok(PhysicalScale {
        unit,
        width: decode_latin1(width),
        height: decode_latin1(height),
    })
}

/// Parse a `pCAL` chunk.
///
/// Layout: `name \0 x0:i32 x1:i32 equation:u8 nparams:u8 unit \0 param \0 ...`
/// with `nparams` parameters, the last of which is not null-terminated.
pub fn parse_pcal(data: &[u8]) -> Result<PixelCalibration, DecodingError> {
    let (name, rest) = split_null(data, chunk::pCAL)?;
    validate_keyword_bytes(name)?;
    if rest.len() < 10 {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::pCAL }.into());
    }
    let x0 = be_i32(rest, 0, chunk::pCAL)?;
    let x1 = be_i32(rest, 4, chunk::pCAL)?;
    let equation_type = rest[8];
    if equation_type > 3 {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::pCAL }.into());
    }
    let nparams = rest[9];
    let expected = match equation_type {
        0 | 2 => 2u8,
        1 => 3,
        _ => 3,
    };
    if nparams != expected {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::pCAL }.into());
    }
    let (unit_name, mut rest) = split_null(&rest[10..], chunk::pCAL)?;
    let mut parameters = Vec::with_capacity(usize::from(nparams));
    for index in 0..nparams {
        let (param, tail) = if index + 1 == nparams {
            (rest, &rest[rest.len()..])
        } else {
            split_null(rest, chunk::pCAL)?
        };
        if !is_ascii_float(param) {
            return Err(FormatErrorKind::MalformedChunk { kind: chunk::pCAL }.into());
        }
        parameters.push(decode_latin1(param));
        rest = tail;
    }
    Ok(PixelCalibration {
        name: decode_latin1(name),
        x0,
        x1,
        equation_type,
        unit_name: decode_latin1(unit_name),
        parameters,
    })
}

/// Parse an `sTER` chunk: one stereo-layout byte.
pub fn parse_ster(data: &[u8]) -> Result<StereoLayout, DecodingError> {
    let raw = *data
        .first()
        .ok_or(FormatErrorKind::ChunkLengthWrong { kind: chunk::sTER })?;
    if data.len() != 1 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::sTER }.into());
    }
    StereoLayout::from_u8(raw)
        .ok_or_else(|| FormatErrorKind::MalformedChunk { kind: chunk::sTER }.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plte_rules() {
        assert!(parse_plte(&[1, 2, 3], ColorType::Indexed).is_ok());
        assert!(parse_plte(&[1, 2, 3], ColorType::Rgb).is_ok());
        assert!(parse_plte(&[1, 2, 3], ColorType::Grayscale).is_err());
        assert!(parse_plte(&[1, 2, 3], ColorType::GrayscaleAlpha).is_err());
        assert!(parse_plte(&[], ColorType::Indexed).is_err());
        assert!(parse_plte(&[1, 2], ColorType::Indexed).is_err());
        assert!(parse_plte(&vec![0u8; 771], ColorType::Indexed).is_err());
        assert!(parse_plte(&vec![0u8; 768], ColorType::Indexed).is_ok());
    }

    #[test]
    fn trns_normalisation_matches_png() {
        // Grayscale, depth 8: the two stored bytes truncate to the low one.
        let t = parse_trns(&[0x12, 0x34], ColorType::Grayscale, BitDepth::Eight, 0)
            .expect("parse")
            .expect("present");
        assert_eq!(t.normalized, vec![0x34]);
        assert_eq!(t.raw, vec![0x12, 0x34]);
        // Grayscale, depth 16: kept as two bytes.
        let t = parse_trns(&[0x12, 0x34], ColorType::Grayscale, BitDepth::Sixteen, 0)
            .expect("parse")
            .expect("present");
        assert_eq!(t.normalized, vec![0x12, 0x34]);
        // RGB, depth 8: three low bytes.
        let t = parse_trns(&[0, 1, 0, 2, 0, 3], ColorType::Rgb, BitDepth::Eight, 0)
            .expect("parse")
            .expect("present");
        assert_eq!(t.normalized, vec![1, 2, 3]);
    }

    #[test]
    fn trns_rejects_short_and_alpha_colour_types() {
        assert!(parse_trns(&[1], ColorType::Grayscale, BitDepth::Eight, 0).is_err());
        assert!(parse_trns(&[0; 5], ColorType::Rgb, BitDepth::Eight, 0).is_err());
        assert!(parse_trns(&[0; 2], ColorType::GrayscaleAlpha, BitDepth::Eight, 0).is_err());
        assert!(parse_trns(&[0; 6], ColorType::Rgba, BitDepth::Eight, 0).is_err());
    }

    #[test]
    fn oversized_indexed_trns_is_ignored_not_fatal() {
        let out = parse_trns(&[1, 2, 3, 4], ColorType::Indexed, BitDepth::Eight, 2).expect("parse");
        assert!(out.is_none());
        let out = parse_trns(&[1, 2], ColorType::Indexed, BitDepth::Eight, 2)
            .expect("parse")
            .expect("present");
        assert_eq!(out.normalized, vec![1, 2]);
        assert!(parse_trns(&[1], ColorType::Indexed, BitDepth::Eight, 0).is_err());
    }

    #[test]
    fn phys_and_time_and_offs() {
        let mut data = [0u8; 9];
        data[3] = 100;
        data[7] = 100;
        data[8] = 1;
        let dims = parse_phys(&data).expect("phys");
        assert_eq!(dims.xppu, 100);
        assert_eq!(dims.unit, Unit::Meter);
        data[8] = 2;
        assert!(parse_phys(&data).is_err());
        assert!(parse_phys(&data[..8]).is_err());

        let t = parse_time(&[0x07, 0xEA, 9, 7, 12, 30, 60]).expect("time");
        assert_eq!(t.year, 2026);
        assert_eq!(t.second, 60);
        assert!(parse_time(&[0x07, 0xEA, 13, 7, 12, 30, 0]).is_err());
        assert!(parse_time(&[0x07, 0xEA, 9, 0, 12, 30, 0]).is_err());
        assert!(parse_time(&[0; 6]).is_err());

        let mut data = [0u8; 9];
        data[0..4].copy_from_slice(&(-5i32).to_be_bytes());
        data[8] = 1;
        let offs = parse_offs(&data).expect("offs");
        assert_eq!(offs.x, -5);
        assert_eq!(offs.unit, OffsetUnit::Micrometer);
        data[8] = 3;
        assert!(parse_offs(&data).is_err());
    }

    #[test]
    fn scal_requires_ascii_floats() {
        let mut data = vec![1u8];
        data.extend_from_slice(b"1.5\0-2.5e3");
        let scal = parse_scal(&data).expect("scal");
        assert_eq!(scal.unit, ScalUnit::Meter);
        assert_eq!(scal.width, "1.5");
        assert_eq!(scal.height, "-2.5e3");

        let mut bad = vec![1u8];
        bad.extend_from_slice(b"1.5\0abc");
        assert!(parse_scal(&bad).is_err());
        let mut bad = vec![3u8];
        bad.extend_from_slice(b"1\x002");
        assert!(parse_scal(&bad).is_err());
        assert!(parse_scal(&[]).is_err());
    }

    #[test]
    fn ascii_float_grammar() {
        for good in ["1", "-1", "+1", "1.5", ".5", "5.", "1e5", "1.5E-3"] {
            assert!(is_ascii_float(good.as_bytes()), "{good}");
        }
        for bad in ["", "-", ".", "1e", "1e+", "abc", "1 5", "1.5x"] {
            assert!(!is_ascii_float(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn pcal_parses_all_equation_types() {
        let mut data = b"Calib\0".to_vec();
        data.extend_from_slice(&0i32.to_be_bytes());
        data.extend_from_slice(&255i32.to_be_bytes());
        data.push(0); // linear
        data.push(2); // two parameters
        data.extend_from_slice(b"kg\0");
        data.extend_from_slice(b"0.0\x00100.0");
        let pcal = parse_pcal(&data).expect("pcal");
        assert_eq!(pcal.name, "Calib");
        assert_eq!(pcal.x1, 255);
        assert_eq!(pcal.unit_name, "kg");
        assert_eq!(pcal.parameters, vec!["0.0", "100.0"]);

        let mut bad = data.clone();
        bad[b"Calib\0".len() + 8] = 4; // invalid equation type
        assert!(parse_pcal(&bad).is_err());
        let mut bad = data.clone();
        bad[b"Calib\0".len() + 9] = 3; // wrong parameter count for type 0
        assert!(parse_pcal(&bad).is_err());
        assert!(parse_pcal(b"Calib\0").is_err());
    }

    #[test]
    fn ster_layouts() {
        assert_eq!(parse_ster(&[0]).expect("ster"), StereoLayout::CrossFuse);
        assert_eq!(parse_ster(&[1]).expect("ster"), StereoLayout::DivergingFuse);
        assert!(parse_ster(&[2]).is_err());
        assert!(parse_ster(&[]).is_err());
        assert!(parse_ster(&[0, 0]).is_err());
    }
}

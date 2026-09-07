//! Colour-related ancillary chunks: `gAMA`, `cHRM`, `sRGB`, `iCCP`, `cICP`,
//! `mDCv`, `cLLi`, `sBIT`, `bKGD`, `hIST` and `sPLT`.

use crate::ancillary::{be_u16, be_u32, split_null};
use crate::chunk;
use crate::common::{
    CodingIndependentCodePoints, ContentLightLevelInfo, MasteringDisplayColorVolume, ScaledFloat,
    SourceChromaticities, SrgbRenderingIntent,
};
use crate::error::{DecodingError, FormatErrorKind};
use crate::header::{BitDepth, ColorType};
use crate::info::{SuggestedPalette, SuggestedPaletteEntry};
use crate::text_metadata::{decode_latin1, validate_keyword_bytes};

/// Parse a `gAMA` chunk: one scaled gamma value.
pub fn parse_gama(data: &[u8]) -> Result<ScaledFloat, DecodingError> {
    Ok(ScaledFloat::from_scaled(be_u32(data, 0, chunk::gAMA)?))
}

/// Parse a `cHRM` chunk: eight scaled chromaticity values.
pub fn parse_chrm(data: &[u8]) -> Result<SourceChromaticities, DecodingError> {
    if data.len() != 32 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::cHRM }.into());
    }
    let mut v = [ScaledFloat::from_scaled(0); 8];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = ScaledFloat::from_scaled(be_u32(data, i * 4, chunk::cHRM)?);
    }
    Ok(SourceChromaticities {
        white: (v[0], v[1]),
        red: (v[2], v[3]),
        green: (v[4], v[5]),
        blue: (v[6], v[7]),
    })
}

/// Parse an `sRGB` chunk: one rendering-intent byte.
pub fn parse_srgb(data: &[u8]) -> Result<SrgbRenderingIntent, DecodingError> {
    let raw = *data
        .first()
        .ok_or(FormatErrorKind::ChunkLengthWrong { kind: chunk::sRGB })?;
    SrgbRenderingIntent::from_raw(raw)
        .ok_or_else(|| FormatErrorKind::MalformedChunk { kind: chunk::sRGB }.into())
}

/// Parse an `iCCP` chunk, decompressing the profile under `limit` bytes.
///
/// Returns `(profile name, profile bytes)`.
pub fn parse_iccp(data: &[u8], limit: usize) -> Result<(String, Vec<u8>), DecodingError> {
    let (name, rest) = split_null(data, chunk::iCCP)?;
    validate_keyword_bytes(name)?;
    let method = *rest
        .first()
        .ok_or(FormatErrorKind::MalformedChunk { kind: chunk::iCCP })?;
    if method != 0 {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::iCCP }.into());
    }
    let profile = crate::zlib::inflate_zlib_capped(&rest[1..], limit)?;
    Ok((decode_latin1(name), profile))
}

/// Parse a `cICP` chunk: four code-point bytes.
pub fn parse_cicp(data: &[u8]) -> Result<CodingIndependentCodePoints, DecodingError> {
    if data.len() != 4 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::cICP }.into());
    }
    // PNG requires MatrixCoefficients to be 0 (identity / RGB).
    if data[2] != 0 {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::cICP }.into());
    }
    let full_range = match data[3] {
        0 => false,
        1 => true,
        _ => return Err(FormatErrorKind::MalformedChunk { kind: chunk::cICP }.into()),
    };
    Ok(CodingIndependentCodePoints {
        color_primaries: data[0],
        transfer_function: data[1],
        matrix_coefficients: data[2],
        is_video_full_range_image: full_range,
    })
}

/// Parse an `mDCv` chunk: chromaticities plus a luminance range.
///
/// The chromaticities are stored at a scale of 0.00002, unlike `cHRM`'s
/// 0.00001, so each value is doubled on the way in.
pub fn parse_mdcv(data: &[u8]) -> Result<MasteringDisplayColorVolume, DecodingError> {
    if data.len() != 24 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::mDCV }.into());
    }
    let mut v = [ScaledFloat::from_scaled(0); 8];
    // Stored order is red, green, blue, white.
    for (i, slot) in v.iter_mut().enumerate() {
        let raw = u32::from(be_u16(data, i * 2, chunk::mDCV)?);
        *slot = ScaledFloat::from_scaled(raw.saturating_mul(2));
    }
    Ok(MasteringDisplayColorVolume {
        chromaticities: SourceChromaticities {
            red: (v[0], v[1]),
            green: (v[2], v[3]),
            blue: (v[4], v[5]),
            white: (v[6], v[7]),
        },
        max_luminance: be_u32(data, 16, chunk::mDCV)?,
        min_luminance: be_u32(data, 20, chunk::mDCV)?,
    })
}

/// Parse a `cLLi` chunk: two light-level values.
pub fn parse_clli(data: &[u8]) -> Result<ContentLightLevelInfo, DecodingError> {
    if data.len() != 8 {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::cLLI }.into());
    }
    Ok(ContentLightLevelInfo {
        max_content_light_level: be_u32(data, 0, chunk::cLLI)?,
        max_frame_average_light_level: be_u32(data, 4, chunk::cLLI)?,
    })
}

/// Parse an `sBIT` chunk against the image's colour type.
///
/// The length must be exactly the number of channels the colour type has —
/// three for `Indexed`, whose significant bits describe the palette's RGB —
/// and every value must satisfy `1 <= sbit <= sample_depth`, where the sample
/// depth of an indexed image is fixed at 8.
pub fn parse_sbit(
    data: &[u8],
    color_type: ColorType,
    bit_depth: BitDepth,
) -> Result<Vec<u8>, DecodingError> {
    let expected = match color_type {
        ColorType::Grayscale => 1,
        ColorType::Rgb | ColorType::Indexed => 3,
        ColorType::GrayscaleAlpha => 2,
        ColorType::Rgba => 4,
    };
    if data.len() != expected {
        return Err(FormatErrorKind::InvalidSbitChunkSize.into());
    }
    let sample_depth = if color_type == ColorType::Indexed {
        8
    } else {
        bit_depth as u8
    };
    for &value in data {
        if value == 0 || value > sample_depth {
            return Err(FormatErrorKind::InvalidSbit.into());
        }
    }
    Ok(data.to_vec())
}

/// Validate a `bKGD` chunk against the image's colour type and return its
/// bytes unchanged.
pub fn parse_bkgd(data: &[u8], color_type: ColorType) -> Result<Vec<u8>, DecodingError> {
    let expected = match color_type {
        ColorType::Indexed => 1,
        ColorType::Grayscale | ColorType::GrayscaleAlpha => 2,
        ColorType::Rgb | ColorType::Rgba => 6,
    };
    if data.len() != expected {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::bKGD }.into());
    }
    Ok(data.to_vec())
}

/// Parse a `hIST` chunk: one frequency per palette entry.
pub fn parse_hist(data: &[u8], palette_entries: usize) -> Result<Vec<u16>, DecodingError> {
    if data.len() % 2 != 0 || data.len() / 2 != palette_entries {
        return Err(FormatErrorKind::ChunkLengthWrong { kind: chunk::hIST }.into());
    }
    Ok(data
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect())
}

/// Parse an `sPLT` chunk: `name \0 sample_depth entries...`.
pub fn parse_splt(data: &[u8]) -> Result<SuggestedPalette, DecodingError> {
    let (name, rest) = split_null(data, chunk::sPLT)?;
    validate_keyword_bytes(name)?;
    let sample_depth = *rest
        .first()
        .ok_or(FormatErrorKind::MalformedChunk { kind: chunk::sPLT })?;
    let body = &rest[1..];
    let entry_len = match sample_depth {
        8 => 6,
        16 => 10,
        _ => return Err(FormatErrorKind::MalformedChunk { kind: chunk::sPLT }.into()),
    };
    if body.len() % entry_len != 0 {
        return Err(FormatErrorKind::MalformedChunk { kind: chunk::sPLT }.into());
    }
    let entries = body
        .chunks_exact(entry_len)
        .map(|e| {
            if sample_depth == 8 {
                SuggestedPaletteEntry {
                    red: u16::from(e[0]),
                    green: u16::from(e[1]),
                    blue: u16::from(e[2]),
                    alpha: u16::from(e[3]),
                    frequency: u16::from_be_bytes([e[4], e[5]]),
                }
            } else {
                SuggestedPaletteEntry {
                    red: u16::from_be_bytes([e[0], e[1]]),
                    green: u16::from_be_bytes([e[2], e[3]]),
                    blue: u16::from_be_bytes([e[4], e[5]]),
                    alpha: u16::from_be_bytes([e[6], e[7]]),
                    frequency: u16::from_be_bytes([e[8], e[9]]),
                }
            }
        })
        .collect();
    Ok(SuggestedPalette {
        name: decode_latin1(name),
        sample_depth,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gama_and_chrm() {
        assert_eq!(
            parse_gama(&45455u32.to_be_bytes())
                .expect("gama")
                .into_scaled(),
            45455
        );
        assert!(parse_gama(&[0, 0]).is_err());
        let bytes = SourceChromaticities::from_srgb().to_be_bytes();
        let parsed = parse_chrm(&bytes).expect("chrm");
        assert_eq!(parsed, SourceChromaticities::from_srgb());
        assert!(parse_chrm(&bytes[..31]).is_err());
    }

    #[test]
    fn srgb_and_cicp_and_clli() {
        assert_eq!(
            parse_srgb(&[1]).expect("srgb"),
            SrgbRenderingIntent::RelativeColorimetric
        );
        assert!(parse_srgb(&[4]).is_err());
        assert!(parse_srgb(&[]).is_err());

        let cicp = parse_cicp(&[9, 16, 0, 1]).expect("cicp");
        assert_eq!(cicp.color_primaries, 9);
        assert!(cicp.is_video_full_range_image);
        assert!(parse_cicp(&[9, 16, 1, 1]).is_err(), "matrix must be 0");
        assert!(parse_cicp(&[9, 16, 0, 2]).is_err());
        assert!(parse_cicp(&[9, 16, 0]).is_err());

        let mut data = [0u8; 8];
        data[3] = 5;
        data[7] = 7;
        let clli = parse_clli(&data).expect("clli");
        assert_eq!(clli.max_content_light_level, 5);
        assert_eq!(clli.max_frame_average_light_level, 7);
        assert!(parse_clli(&data[..7]).is_err());
    }

    #[test]
    fn mdcv_doubles_the_chromaticity_scale() {
        let mut data = [0u8; 24];
        data[0..2].copy_from_slice(&17000u16.to_be_bytes()); // red x
        data[16..20].copy_from_slice(&1000u32.to_be_bytes());
        data[20..24].copy_from_slice(&5u32.to_be_bytes());
        let parsed = parse_mdcv(&data).expect("mdcv");
        assert_eq!(parsed.chromaticities.red.0.into_scaled(), 34000);
        assert_eq!(parsed.max_luminance, 1000);
        assert_eq!(parsed.min_luminance, 5);
        assert!(parse_mdcv(&data[..23]).is_err());
    }

    #[test]
    fn sbit_length_and_range_rules() {
        assert!(parse_sbit(&[8], ColorType::Grayscale, BitDepth::Eight).is_ok());
        assert!(parse_sbit(&[8, 8], ColorType::Grayscale, BitDepth::Eight).is_err());
        assert!(parse_sbit(&[5, 5, 5], ColorType::Rgb, BitDepth::Eight).is_ok());
        assert!(parse_sbit(&[8, 8, 8, 8], ColorType::Rgba, BitDepth::Eight).is_ok());
        assert!(parse_sbit(&[8, 8], ColorType::GrayscaleAlpha, BitDepth::Eight).is_ok());
        // Indexed always uses a sample depth of 8 and needs three values.
        assert!(parse_sbit(&[8, 8, 8], ColorType::Indexed, BitDepth::Four).is_ok());
        assert!(parse_sbit(&[8], ColorType::Indexed, BitDepth::Four).is_err());
        // Zero and out-of-range values are rejected.
        assert!(parse_sbit(&[0], ColorType::Grayscale, BitDepth::Eight).is_err());
        assert!(parse_sbit(&[9], ColorType::Grayscale, BitDepth::Eight).is_err());
        assert!(parse_sbit(&[16], ColorType::Grayscale, BitDepth::Sixteen).is_ok());
    }

    #[test]
    fn bkgd_length_depends_on_colour_type() {
        assert!(parse_bkgd(&[0], ColorType::Indexed).is_ok());
        assert!(parse_bkgd(&[0, 0], ColorType::Grayscale).is_ok());
        assert!(parse_bkgd(&[0; 6], ColorType::Rgb).is_ok());
        assert!(parse_bkgd(&[0; 6], ColorType::Rgba).is_ok());
        assert!(parse_bkgd(&[0], ColorType::Rgb).is_err());
    }

    #[test]
    fn hist_matches_the_palette() {
        assert_eq!(parse_hist(&[0, 1, 0, 2], 2).expect("hist"), vec![1, 2]);
        assert!(parse_hist(&[0, 1], 2).is_err());
        assert!(parse_hist(&[0], 1).is_err());
    }

    #[test]
    fn splt_supports_both_sample_depths() {
        let mut data = b"pal\0".to_vec();
        data.push(8);
        data.extend_from_slice(&[1, 2, 3, 4, 0, 9]);
        let p = parse_splt(&data).expect("splt");
        assert_eq!(p.name, "pal");
        assert_eq!(p.sample_depth, 8);
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].red, 1);
        assert_eq!(p.entries[0].frequency, 9);

        let mut data = b"pal16\0".to_vec();
        data.push(16);
        data.extend_from_slice(&[0, 1, 0, 2, 0, 3, 0, 4, 0, 5]);
        let p = parse_splt(&data).expect("splt16");
        assert_eq!(p.entries[0].blue, 3);
        assert_eq!(p.entries[0].frequency, 5);

        let mut bad = b"pal\0".to_vec();
        bad.push(4);
        assert!(parse_splt(&bad).is_err());
        let mut bad = b"pal\0".to_vec();
        bad.push(8);
        bad.push(1);
        assert!(parse_splt(&bad).is_err());
        assert!(parse_splt(b"no null").is_err());
    }

    #[test]
    fn iccp_decompresses_under_a_cap() {
        let profile = vec![7u8; 5000];
        let compressed = oxiarc_deflate::zlib_compress(&profile, 6).expect("compress");
        let mut data = b"ICC profile\0".to_vec();
        data.push(0);
        data.extend_from_slice(&compressed);
        let (name, out) = parse_iccp(&data, 1 << 20).expect("iccp");
        assert_eq!(name, "ICC profile");
        assert_eq!(out, profile);
        assert!(parse_iccp(&data, 16).is_err());
        let mut bad = b"ICC\0".to_vec();
        bad.push(1);
        bad.extend_from_slice(&compressed);
        assert!(parse_iccp(&bad, 1 << 20).is_err());
    }
}

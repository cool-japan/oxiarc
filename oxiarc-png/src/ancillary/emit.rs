//! Payload builders for the ancillary chunks, the deterministic inverse of
//! [`crate::ancillary::color`] and [`crate::ancillary::misc`]'s parsers.
//!
//! Every function here returns exactly the bytes a chunk's payload should
//! hold; framing (length, type, CRC) is [`crate::chunk::write_chunk`]'s job,
//! one level up in [`crate::encoder`]. Where a payload's shape is
//! non-trivial enough that a caller-supplied byte string (via
//! `Encoder::with_info`, say) could be wrong, the builder validates by
//! feeding its own output back through the matching `parse_*` function,
//! which both catches mistakes and guarantees write/read symmetry for free.

use crate::ancillary::color;
use crate::ancillary::misc;
use crate::common::{
    CodingIndependentCodePoints, ContentLightLevelInfo, MasteringDisplayColorVolume, ScaledFloat,
    SourceChromaticities, SrgbRenderingIntent,
};
use crate::error::{EncodingError, EncodingFormatErrorKind};
use crate::header::{BitDepth, ColorType};
use crate::info::{ImageOffset, OffsetUnit, PhysicalScale, PixelCalibration, ScalUnit, Time};
use crate::text_metadata::{encode_latin1, validate_keyword_bytes};

fn bad(msg: &'static str) -> EncodingError {
    EncodingFormatErrorKind::BadTextEncoding(msg).into()
}

/// `gAMA`: one scaled gamma value.
#[must_use]
pub(crate) fn gama_payload(gamma: ScaledFloat) -> [u8; 4] {
    gamma.into_scaled().to_be_bytes()
}

/// `cHRM`: eight scaled chromaticity values.
#[must_use]
pub(crate) fn chrm_payload(chroma: SourceChromaticities) -> [u8; 32] {
    chroma.to_be_bytes()
}

/// `sRGB`: one rendering-intent byte.
#[must_use]
pub(crate) fn srgb_payload(intent: SrgbRenderingIntent) -> [u8; 1] {
    [intent.into_raw()]
}

/// `cICP`: four code-point bytes.
#[must_use]
pub(crate) fn cicp_payload(c: CodingIndependentCodePoints) -> [u8; 4] {
    [
        c.color_primaries,
        c.transfer_function,
        c.matrix_coefficients,
        u8::from(c.is_video_full_range_image),
    ]
}

/// `mDCv`: chromaticities (at half `cHRM`'s scale) plus a luminance range.
///
/// # Errors
///
/// A chromaticity whose `cHRM`-scale value exceeds `2 * u16::MAX` (i.e.
/// 1.31 in real units) has no `mDCv` representation at all — the chunk
/// stores each coordinate in a `u16` at half `cHRM`'s scale
/// ([`color::parse_mdcv`] multiplies by 2 on the way back). Reporting that
/// is the point of this being fallible: an `as u16` cast here used to wrap
/// such a value silently, writing a chunk whose coordinates bear no
/// relation to what the caller asked for.
pub(crate) fn mdcv_payload(m: MasteringDisplayColorVolume) -> Result<[u8; 24], EncodingError> {
    let mut out = [0u8; 24];
    let c = m.chromaticities;
    let vals = [
        c.red.0, c.red.1, c.green.0, c.green.1, c.blue.0, c.blue.1, c.white.0, c.white.1,
    ];
    for (i, v) in vals.iter().enumerate() {
        let halved = u16::try_from(v.into_scaled() / 2)
            .map_err(|_| bad("mDCv chromaticity is outside the chunk's representable range"))?;
        out[i * 2..i * 2 + 2].copy_from_slice(&halved.to_be_bytes());
    }
    out[16..20].copy_from_slice(&m.max_luminance.to_be_bytes());
    out[20..24].copy_from_slice(&m.min_luminance.to_be_bytes());
    Ok(out)
}

/// `cLLi`: two light-level values.
#[must_use]
pub(crate) fn clli_payload(c: ContentLightLevelInfo) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&c.max_content_light_level.to_be_bytes());
    out[4..8].copy_from_slice(&c.max_frame_average_light_level.to_be_bytes());
    out
}

/// `pHYs`: two resolutions and a unit byte.
#[must_use]
pub(crate) fn phys_payload(dims: crate::common::PixelDimensions) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0..4].copy_from_slice(&dims.xppu.to_be_bytes());
    out[4..8].copy_from_slice(&dims.yppu.to_be_bytes());
    out[8] = dims.unit as u8;
    out
}

/// `tIME`: a seven-byte UTC timestamp.
pub(crate) fn time_payload(t: Time) -> Result<[u8; 7], EncodingError> {
    let mut out = [0u8; 7];
    out[0..2].copy_from_slice(&t.year.to_be_bytes());
    out[2] = t.month;
    out[3] = t.day;
    out[4] = t.hour;
    out[5] = t.minute;
    out[6] = t.second;
    misc::parse_time(&out).map_err(|_| bad("tIME fields are out of range"))?;
    Ok(out)
}

/// `oFFs`: a signed position and a unit byte.
#[must_use]
pub(crate) fn offs_payload(o: ImageOffset) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0..4].copy_from_slice(&o.x.to_be_bytes());
    out[4..8].copy_from_slice(&o.y.to_be_bytes());
    out[8] = match o.unit {
        OffsetUnit::Pixel => 0,
        OffsetUnit::Micrometer => 1,
    };
    out
}

/// `sCAL`: a unit byte and two ASCII floating-point strings.
pub(crate) fn scal_payload(s: &PhysicalScale) -> Result<Vec<u8>, EncodingError> {
    let mut out = vec![match s.unit {
        ScalUnit::Meter => 1,
        ScalUnit::Radian => 2,
    }];
    out.extend_from_slice(
        encode_latin1(&s.width)
            .ok_or_else(|| bad("sCAL width is not Latin-1"))?
            .as_slice(),
    );
    out.push(0);
    out.extend_from_slice(
        encode_latin1(&s.height)
            .ok_or_else(|| bad("sCAL height is not Latin-1"))?
            .as_slice(),
    );
    misc::parse_scal(&out).map_err(|_| bad("sCAL fields are not valid ASCII floats"))?;
    Ok(out)
}

/// `pCAL`: name, range, equation type and ASCII floating-point parameters.
pub(crate) fn pcal_payload(p: &PixelCalibration) -> Result<Vec<u8>, EncodingError> {
    let name = encode_latin1(&p.name).ok_or_else(|| bad("pCAL name is not Latin-1"))?;
    validate_keyword_bytes(&name).map_err(|_| bad("pCAL name is not a valid keyword"))?;
    let mut out = name;
    out.push(0);
    out.extend_from_slice(&p.x0.to_be_bytes());
    out.extend_from_slice(&p.x1.to_be_bytes());
    out.push(p.equation_type);
    let nparams =
        u8::try_from(p.parameters.len()).map_err(|_| bad("pCAL has too many parameters"))?;
    out.push(nparams);
    out.extend_from_slice(
        encode_latin1(&p.unit_name)
            .ok_or_else(|| bad("pCAL unit name is not Latin-1"))?
            .as_slice(),
    );
    out.push(0);
    for (i, param) in p.parameters.iter().enumerate() {
        out.extend_from_slice(
            encode_latin1(param)
                .ok_or_else(|| bad("pCAL parameter is not Latin-1"))?
                .as_slice(),
        );
        if i + 1 != p.parameters.len() {
            out.push(0);
        }
    }
    misc::parse_pcal(&out).map_err(|_| bad("pCAL payload failed its own validity check"))?;
    Ok(out)
}

/// `sTER`: one stereo-layout byte.
#[must_use]
pub(crate) fn ster_payload(layout: crate::common::StereoLayout) -> [u8; 1] {
    [layout as u8]
}

/// `PLTE`: the palette bytes, validated against the colour type.
pub(crate) fn plte_payload(
    palette: &[u8],
    color_type: ColorType,
) -> Result<Vec<u8>, EncodingError> {
    misc::parse_plte(palette, color_type)
        .map_err(|_| EncodingFormatErrorKind::InvalidColorCombination.into())
}

/// `tRNS`: reconstruct the wire form from [`crate::Info`]'s normalised
/// storage (or use `trns_original` verbatim when a decode supplied it, which
/// preserves the exact bytes on a decode-then-encode round trip).
pub(crate) fn trns_payload(
    color_type: ColorType,
    bit_depth: BitDepth,
    normalized: &[u8],
    original: Option<&[u8]>,
) -> Result<Vec<u8>, EncodingError> {
    if let Some(raw) = original {
        return Ok(raw.to_vec());
    }
    let wire = match color_type {
        ColorType::Grayscale => {
            let v = *normalized
                .first()
                .ok_or_else(|| bad("tRNS gray key is empty"))?;
            if bit_depth == BitDepth::Sixteen {
                normalized
                    .get(..2)
                    .ok_or_else(|| bad("16-bit tRNS gray key needs two bytes"))?
                    .to_vec()
            } else {
                vec![0, v]
            }
        }
        ColorType::Rgb => {
            if bit_depth == BitDepth::Sixteen {
                normalized
                    .get(..6)
                    .ok_or_else(|| bad("16-bit tRNS rgb key needs six bytes"))?
                    .to_vec()
            } else {
                let [r, g, b] = normalized
                    .get(..3)
                    .and_then(|s| <[u8; 3]>::try_from(s).ok())
                    .ok_or_else(|| bad("tRNS rgb key needs three bytes"))?;
                vec![0, r, 0, g, 0, b]
            }
        }
        ColorType::Indexed => normalized.to_vec(),
        ColorType::GrayscaleAlpha | ColorType::Rgba => {
            return Err(EncodingFormatErrorKind::InvalidColorCombination.into());
        }
    };
    Ok(wire)
}

/// `sBIT`: the significant-bits payload, validated against the colour type.
pub(crate) fn sbit_payload(
    sbit: &[u8],
    color_type: ColorType,
    bit_depth: BitDepth,
) -> Result<Vec<u8>, EncodingError> {
    color::parse_sbit(sbit, color_type, bit_depth)
        .map_err(|_| bad("sBIT does not match the colour type"))
}

/// `bKGD`: the background payload, validated against the colour type.
pub(crate) fn bkgd_payload(bkgd: &[u8], color_type: ColorType) -> Result<Vec<u8>, EncodingError> {
    color::parse_bkgd(bkgd, color_type).map_err(|_| bad("bKGD does not match the colour type"))
}

/// `hIST`: one big-endian frequency per palette entry.
pub(crate) fn hist_payload(
    counts: &[u16],
    palette_entries: usize,
) -> Result<Vec<u8>, EncodingError> {
    if counts.len() != palette_entries {
        return Err(bad("hIST entry count does not match the palette"));
    }
    let mut out = Vec::with_capacity(counts.len() * 2);
    for c in counts {
        out.extend_from_slice(&c.to_be_bytes());
    }
    Ok(out)
}

/// `sPLT`: `name \0 sample_depth entries...`.
pub(crate) fn splt_payload(sp: &crate::info::SuggestedPalette) -> Result<Vec<u8>, EncodingError> {
    let name = encode_latin1(&sp.name).ok_or_else(|| bad("sPLT name is not Latin-1"))?;
    validate_keyword_bytes(&name).map_err(|_| bad("sPLT name is not a valid keyword"))?;
    let mut out = name;
    out.push(0);
    out.push(sp.sample_depth);
    match sp.sample_depth {
        8 => {
            for e in &sp.entries {
                // An 8-bit sPLT stores each sample in one byte. A `as u8`
                // cast here silently wrapped a caller's out-of-range sample
                // (300 became 44) into a palette entry that was simply a
                // different colour; say so instead.
                let byte =
                    |v: u16| u8::try_from(v).map_err(|_| bad("sPLT 8-bit sample exceeds 255"));
                out.extend_from_slice(&[
                    byte(e.red)?,
                    byte(e.green)?,
                    byte(e.blue)?,
                    byte(e.alpha)?,
                ]);
                out.extend_from_slice(&e.frequency.to_be_bytes());
            }
        }
        16 => {
            for e in &sp.entries {
                for v in [e.red, e.green, e.blue, e.alpha, e.frequency] {
                    out.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        _ => return Err(bad("sPLT sample depth must be 8 or 16")),
    }
    Ok(out)
}

/// `iCCP`: `name \0 method(0) zlib-compressed-profile`.
///
/// The profile's original name is not carried by [`crate::Info`] (matching
/// `png` 0.18's own `Info` shape, which has no `iccp_name` field), so a
/// decode-then-encode round trip always writes back the fixed name `"icc"`.
pub(crate) fn iccp_payload(profile: &[u8], level: u8) -> Result<Vec<u8>, EncodingError> {
    let mut out = b"icc".to_vec();
    out.push(0);
    out.push(0); // compression method: zlib
    let compressed = oxiarc_deflate::zlib_compress(profile, level)
        .map_err(|err| EncodingError::IoError(std::io::Error::other(err.to_string())))?;
    out.extend_from_slice(&compressed);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_size_payloads_round_trip_through_their_parsers() {
        let gamma = ScaledFloat::new(0.45455);
        assert_eq!(
            color::parse_gama(&gama_payload(gamma)).expect("gama"),
            gamma
        );

        let chroma = SourceChromaticities::from_srgb();
        assert_eq!(
            color::parse_chrm(&chrm_payload(chroma)).expect("chrm"),
            chroma
        );

        let intent = SrgbRenderingIntent::Perceptual;
        assert_eq!(
            color::parse_srgb(&srgb_payload(intent)).expect("srgb"),
            intent
        );

        let cicp = CodingIndependentCodePoints {
            color_primaries: 9,
            transfer_function: 16,
            matrix_coefficients: 0,
            is_video_full_range_image: true,
        };
        assert_eq!(color::parse_cicp(&cicp_payload(cicp)).expect("cicp"), cicp);

        let clli = ContentLightLevelInfo {
            max_content_light_level: 1000,
            max_frame_average_light_level: 400,
        };
        assert_eq!(color::parse_clli(&clli_payload(clli)).expect("clli"), clli);

        let mdcv = MasteringDisplayColorVolume {
            chromaticities: chroma,
            max_luminance: 10_000_000,
            min_luminance: 1,
        };
        let parsed = color::parse_mdcv(&mdcv_payload(mdcv).expect("in range")).expect("mdcv");
        assert_eq!(parsed.max_luminance, mdcv.max_luminance);
        assert_eq!(parsed.min_luminance, mdcv.min_luminance);
    }

    #[test]
    fn phys_offs_time_round_trip() {
        let dims = crate::common::PixelDimensions {
            xppu: 2835,
            yppu: 2835,
            unit: crate::common::Unit::Meter,
        };
        assert_eq!(misc::parse_phys(&phys_payload(dims)).expect("phys"), dims);

        let offset = ImageOffset {
            x: -5,
            y: 12,
            unit: OffsetUnit::Pixel,
        };
        assert_eq!(
            misc::parse_offs(&offs_payload(offset)).expect("offs"),
            offset
        );

        let t = Time {
            year: 2026,
            month: 9,
            day: 7,
            hour: 12,
            minute: 30,
            second: 0,
        };
        assert_eq!(
            misc::parse_time(&time_payload(t).expect("time")).expect("parse"),
            t
        );

        let mut bad_t = t;
        bad_t.month = 13;
        assert!(time_payload(bad_t).is_err());
    }

    #[test]
    fn scal_and_pcal_round_trip() {
        let s = PhysicalScale {
            unit: ScalUnit::Meter,
            width: "0.5".to_string(),
            height: "1.25e-3".to_string(),
        };
        let payload = scal_payload(&s).expect("scal");
        let parsed = misc::parse_scal(&payload).expect("parse");
        assert_eq!(parsed.width, s.width);
        assert_eq!(parsed.height, s.height);

        let p = PixelCalibration {
            name: "Cal".to_string(),
            x0: 0,
            x1: 255,
            equation_type: 0,
            unit_name: "nm".to_string(),
            parameters: vec!["1.0".to_string(), "2.0".to_string()],
        };
        let payload = pcal_payload(&p).expect("pcal");
        let parsed = misc::parse_pcal(&payload).expect("parse");
        assert_eq!(parsed.parameters, p.parameters);
        assert_eq!(parsed.name, p.name);
    }

    #[test]
    fn plte_trns_sbit_bkgd_hist_splt() {
        let palette = vec![10u8, 20, 30, 40, 50, 60];
        assert_eq!(
            plte_payload(&palette, ColorType::Indexed).expect("plte"),
            palette
        );
        assert!(plte_payload(&palette, ColorType::Grayscale).is_err());

        // 8-bit grayscale tRNS: normalized is one byte, wire form is two.
        let wire = trns_payload(ColorType::Grayscale, BitDepth::Eight, &[7], None).expect("trns");
        assert_eq!(wire, vec![0, 7]);
        let trns = misc::parse_trns(&wire, ColorType::Grayscale, BitDepth::Eight, 0)
            .expect("parse")
            .expect("some");
        assert_eq!(trns.normalized, vec![7]);

        // trns_original, when present, is used verbatim.
        let original = vec![0xAB, 0xCD];
        let wire2 = trns_payload(
            ColorType::Grayscale,
            BitDepth::Sixteen,
            &[0xAB, 0xCD],
            Some(&original),
        )
        .expect("trns original");
        assert_eq!(wire2, original);

        let sbit = sbit_payload(&[5, 5, 5], ColorType::Rgb, BitDepth::Eight).expect("sbit");
        assert_eq!(sbit, vec![5, 5, 5]);
        assert!(sbit_payload(&[5, 5, 5, 5], ColorType::Rgb, BitDepth::Eight).is_err());

        let bkgd = bkgd_payload(&[0, 0], ColorType::Grayscale).expect("bkgd");
        assert_eq!(bkgd, vec![0, 0]);

        let hist = hist_payload(&[1, 2], 2).expect("hist");
        assert_eq!(color::parse_hist(&hist, 2).expect("parse"), vec![1, 2]);
        assert!(hist_payload(&[1, 2], 3).is_err());

        let sp = crate::info::SuggestedPalette {
            name: "pal".to_string(),
            sample_depth: 8,
            entries: vec![crate::info::SuggestedPaletteEntry {
                red: 1,
                green: 2,
                blue: 3,
                alpha: 4,
                frequency: 9,
            }],
        };
        let payload = splt_payload(&sp).expect("splt");
        let parsed = color::parse_splt(&payload).expect("parse");
        assert_eq!(parsed, sp);
    }

    #[test]
    fn iccp_round_trips_through_its_own_parser() {
        let profile = vec![7u8; 2000];
        let payload = iccp_payload(&profile, 6).expect("iccp");
        let (name, out) = color::parse_iccp(&payload, 1 << 20).expect("parse");
        assert_eq!(name, "icc");
        assert_eq!(out, profile);
    }

    #[test]
    fn ster_payload_round_trips() {
        let layout = crate::common::StereoLayout::DivergingFuse;
        assert_eq!(
            misc::parse_ster(&ster_payload(layout)).expect("ster"),
            layout
        );
    }

    /// An `mDCv` chromaticity larger than the chunk can represent used to
    /// wrap through an `as u16` cast and be written as a different colour.
    #[test]
    fn an_unrepresentable_mdcv_chromaticity_is_an_error_not_a_wrapped_value() {
        let big = ScaledFloat::from_scaled(400_000);
        let m = MasteringDisplayColorVolume {
            chromaticities: SourceChromaticities {
                red: (big, big),
                green: (big, big),
                blue: (big, big),
                white: (big, big),
            },
            max_luminance: 1,
            min_luminance: 0,
        };
        assert!(mdcv_payload(m).is_err());
    }

    /// An 8-bit `sPLT` sample above 255 used to be truncated into a
    /// silently different palette entry.
    #[test]
    fn an_out_of_range_eight_bit_splt_sample_is_an_error_not_a_truncated_one() {
        use crate::info::{SuggestedPalette, SuggestedPaletteEntry};
        let sp = SuggestedPalette {
            name: "p".to_string(),
            sample_depth: 8,
            entries: vec![SuggestedPaletteEntry {
                red: 300,
                green: 1,
                blue: 2,
                alpha: 3,
                frequency: 4,
            }],
        };
        assert!(splt_payload(&sp).is_err());

        let ok = SuggestedPalette {
            name: "p".to_string(),
            sample_depth: 8,
            entries: vec![SuggestedPaletteEntry {
                red: 200,
                green: 1,
                blue: 2,
                alpha: 3,
                frequency: 4,
            }],
        };
        assert!(splt_payload(&ok).is_ok());
    }
}

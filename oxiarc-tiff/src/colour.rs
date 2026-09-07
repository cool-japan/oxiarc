//! Photometric conversions.
//!
//! Every conversion here is **opt-in**. The raw read path never rewrites
//! colour: `MinIsWhite` images come back inverted-as-stored, palette images
//! come back as indices, and YCbCr images come back as luma/chroma. Callers
//! that want display-ready pixels ask for them through
//! [`crate::Decoder::read_image_rgb8`] / [`crate::Decoder::read_image_rgba8`],
//! which route through this module.
//!
//! ```
//! use oxiarc_tiff::colour::{expand_palette_rgb8, scale_to_u8};
//!
//! assert_eq!(scale_to_u8(15, 4), 255);
//! assert_eq!(scale_to_u8(0, 4), 0);
//!
//! // A two-entry palette: index 0 -> black, index 1 -> white.
//! let map = vec![0, 0xFFFF, 0, 0xFFFF, 0, 0xFFFF];
//! let rgb = expand_palette_rgb8(&[0u16, 1], &map, 1)?;
//! assert_eq!(rgb, vec![0, 0, 0, 255, 255, 255]);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use crate::error::{FormatError, Result, TiffError, UnsupportedError};
use crate::image::ImageInfo;
use crate::sample::{SampleType, Samples};
use crate::tags::{ExtraSamples, InkSet, PhotometricInterpretation};

/// Scales a `bits`-wide unsigned value to the full 0..=255 range.
#[must_use]
pub fn scale_to_u8(value: u64, bits: u16) -> u8 {
    if bits == 0 {
        return 0;
    }
    if bits >= 8 {
        let shift = u32::from(bits) - 8;
        return (value >> shift) as u8;
    }
    let max = (1u64 << bits) - 1;
    if max == 0 {
        return 0;
    }
    ((value.min(max) * 255) / max) as u8
}

/// Reads the samples of a decoded buffer as `u64`s in channel order.
fn sample_at(samples: &Samples, index: usize) -> u64 {
    match samples {
        Samples::U8(v) => u64::from(v.get(index).copied().unwrap_or(0)),
        Samples::U16(v) => u64::from(v.get(index).copied().unwrap_or(0)),
        Samples::U32(v) => u64::from(v.get(index).copied().unwrap_or(0)),
        Samples::U64(v) => v.get(index).copied().unwrap_or(0),
        Samples::I8(v) => v.get(index).copied().unwrap_or(0) as u64,
        Samples::I16(v) => v.get(index).copied().unwrap_or(0) as u64,
        Samples::I32(v) => v.get(index).copied().unwrap_or(0) as u64,
        Samples::I64(v) => v.get(index).copied().unwrap_or(0) as u64,
        Samples::F16(v) => u64::from(v.get(index).copied().unwrap_or(0)),
        Samples::F32(v) => v.get(index).copied().unwrap_or(0.0) as u64,
        Samples::F64(v) => v.get(index).copied().unwrap_or(0.0) as u64,
    }
}

/// Reads a sample as an `f64`, expanding binary16 bit patterns.
fn sample_as_f64(samples: &Samples, index: usize) -> f64 {
    match samples {
        Samples::F16(v) => f64::from(crate::sample::f16_bits_to_f32(
            v.get(index).copied().unwrap_or(0),
        )),
        Samples::F32(v) => f64::from(v.get(index).copied().unwrap_or(0.0)),
        Samples::F64(v) => v.get(index).copied().unwrap_or(0.0),
        Samples::I8(v) => f64::from(v.get(index).copied().unwrap_or(0)),
        Samples::I16(v) => f64::from(v.get(index).copied().unwrap_or(0)),
        Samples::I32(v) => f64::from(v.get(index).copied().unwrap_or(0)),
        Samples::I64(v) => v.get(index).copied().unwrap_or(0) as f64,
        other => sample_at(other, index) as f64,
    }
}

/// Inverts a `WhiteIsZero` buffer in place so 0 means black.
///
/// Floating-point samples are inverted around 1.0, which is what the
/// `MinSampleValue`/`MaxSampleValue` convention implies for normalised data.
pub fn invert_min_is_white(samples: &mut Samples, bits: u16) {
    let max = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    match samples {
        Samples::U8(v) => {
            for s in v.iter_mut() {
                *s = (max as u8).wrapping_sub(*s);
            }
        }
        Samples::U16(v) => {
            for s in v.iter_mut() {
                *s = (max as u16).wrapping_sub(*s);
            }
        }
        Samples::U32(v) => {
            for s in v.iter_mut() {
                *s = (max as u32).wrapping_sub(*s);
            }
        }
        Samples::U64(v) => {
            for s in v.iter_mut() {
                *s = max.wrapping_sub(*s);
            }
        }
        Samples::F32(v) => {
            for s in v.iter_mut() {
                *s = 1.0 - *s;
            }
        }
        Samples::F64(v) => {
            for s in v.iter_mut() {
                *s = 1.0 - *s;
            }
        }
        Samples::F16(v) => {
            for s in v.iter_mut() {
                *s = crate::sample::f32_to_f16_bits(1.0 - crate::sample::f16_bits_to_f32(*s));
            }
        }
        Samples::I8(v) => {
            for s in v.iter_mut() {
                *s = s.wrapping_neg();
            }
        }
        Samples::I16(v) => {
            for s in v.iter_mut() {
                *s = s.wrapping_neg();
            }
        }
        Samples::I32(v) => {
            for s in v.iter_mut() {
                *s = s.wrapping_neg();
            }
        }
        Samples::I64(v) => {
            for s in v.iter_mut() {
                *s = s.wrapping_neg();
            }
        }
    }
}

/// Expands palette indices to 8-bit RGB through a `ColorMap`.
///
/// The map is laid out as *all reds*, then *all greens*, then *all blues*,
/// each entry scaled to 0..=65535.
///
/// # Errors
/// [`FormatError::ColorMapWrongLength`] when the map is not `3 << bits` long.
pub fn expand_palette_rgb8(indices: &[u16], color_map: &[u16], bits: u16) -> Result<Vec<u8>> {
    let entries = 1usize << bits.min(16);
    if color_map.len() < entries * 3 {
        return Err(TiffError::Format(FormatError::ColorMapWrongLength {
            expected: entries * 3,
            got: color_map.len(),
        }));
    }
    let mut out = Vec::with_capacity(indices.len() * 3);
    for index in indices {
        let i = usize::from(*index).min(entries.saturating_sub(1));
        let r = color_map.get(i).copied().unwrap_or(0);
        let g = color_map.get(entries + i).copied().unwrap_or(0);
        let b = color_map.get(2 * entries + i).copied().unwrap_or(0);
        out.push((r >> 8) as u8);
        out.push((g >> 8) as u8);
        out.push((b >> 8) as u8);
    }
    Ok(out)
}

/// The `ReferenceBlackWhite` defaults, per TIFF 6.0 section 21.
#[must_use]
pub fn default_reference_black_white(
    photometric: PhotometricInterpretation,
    bits: u16,
) -> [f64; 6] {
    let max = if bits >= 64 {
        u64::MAX as f64
    } else {
        ((1u64 << bits) - 1) as f64
    };
    let half = ((max + 1.0) / 2.0).floor();
    if photometric == PhotometricInterpretation::YCbCr {
        [0.0, max, half, max, half, max]
    } else {
        [0.0, max, 0.0, max, 0.0, max]
    }
}

/// Converts one YCbCr triple to RGB using the TIFF 6.0 matrix.
///
/// `coefficients` is `YCbCrCoefficients` (luma red/green/blue) and `rbw` is the
/// six `ReferenceBlackWhite` values.
#[must_use]
pub fn ycbcr_to_rgb(
    y: f64,
    cb: f64,
    cr: f64,
    coefficients: [f64; 3],
    rbw: [f64; 6],
    max: f64,
) -> [f64; 3] {
    let luma_red = coefficients[0];
    let luma_green = if coefficients[1] == 0.0 {
        1.0 - coefficients[0] - coefficients[2]
    } else {
        coefficients[1]
    };
    let luma_blue = coefficients[2];

    let y_range = rbw[1] - rbw[0];
    let cb_range = rbw[3] - rbw[2];
    let cr_range = rbw[5] - rbw[4];

    let yn = if y_range == 0.0 {
        y
    } else {
        (y - rbw[0]) * (max / y_range)
    };
    let cbn = if cb_range == 0.0 {
        cb
    } else {
        (cb - rbw[2]) * ((max / 2.0) / cb_range)
    };
    let crn = if cr_range == 0.0 {
        cr
    } else {
        (cr - rbw[4]) * ((max / 2.0) / cr_range)
    };

    let r = crn * (2.0 - 2.0 * luma_red) + yn;
    let b = cbn * (2.0 - 2.0 * luma_blue) + yn;
    let g = if luma_green == 0.0 {
        yn
    } else {
        (yn - luma_blue * b - luma_red * r) / luma_green
    };
    [r, g, b]
}

/// Expands a chunk stored in YCbCr subsampling units into full-resolution
/// interleaved YCbCr samples.
///
/// TIFF stores each `h * v` block as `h * v` luma samples followed by one Cb
/// and one Cr, with the units in raster order across the chunk. The expansion
/// must happen before the colour matrix and before the chunk is placed into the
/// image.
///
/// # Errors
/// [`FormatError::IllegalSubsampling`] for an unusable factor and
/// [`crate::UsageError::BufferTooSmall`] when `dst` cannot hold the result.
pub fn expand_ycbcr_subsampling(
    src: &[u8],
    width: u32,
    height: u32,
    subsampling: (u16, u16),
    bytes_per_sample: usize,
    dst: &mut [u8],
) -> Result<()> {
    let (h, v) = subsampling;
    if h == 0 || v == 0 {
        return Err(TiffError::Format(FormatError::IllegalSubsampling(h, v)));
    }
    let need = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(3))
        .and_then(|n| n.checked_mul(bytes_per_sample))
        .ok_or(TiffError::IntOverflow)?;
    if dst.len() < need {
        return Err(TiffError::Usage(crate::error::UsageError::BufferTooSmall {
            needed: need,
            got: dst.len(),
        }));
    }

    let hu = usize::from(h);
    let vu = usize::from(v);
    let unit_samples = hu * vu + 2;
    let unit_bytes = unit_samples * bytes_per_sample;
    let units_across = (width as usize).div_ceil(hu);
    let unit_rows = (height as usize).div_ceil(vu);

    for unit_row in 0..unit_rows {
        for unit_col in 0..units_across {
            let unit_index = unit_row * units_across + unit_col;
            let base = unit_index * unit_bytes;
            let cb_off = base + hu * vu * bytes_per_sample;
            let cr_off = cb_off + bytes_per_sample;
            for dy in 0..vu {
                let y = unit_row * vu + dy;
                if y >= height as usize {
                    break;
                }
                for dx in 0..hu {
                    let x = unit_col * hu + dx;
                    if x >= width as usize {
                        break;
                    }
                    let luma_off = base + (dy * hu + dx) * bytes_per_sample;
                    let out = (y * width as usize + x) * 3 * bytes_per_sample;
                    copy_sample(src, luma_off, dst, out, bytes_per_sample);
                    copy_sample(src, cb_off, dst, out + bytes_per_sample, bytes_per_sample);
                    copy_sample(
                        src,
                        cr_off,
                        dst,
                        out + 2 * bytes_per_sample,
                        bytes_per_sample,
                    );
                }
            }
        }
    }
    Ok(())
}

/// Packs full-resolution interleaved YCbCr samples into subsampling units.
///
/// The exact inverse of [`expand_ycbcr_subsampling`]; chroma is taken from the
/// top-left pixel of each block (box sampling with no filtering, which is what
/// libtiff's uncompressed path does).
///
/// # Errors
/// [`FormatError::IllegalSubsampling`] for an unusable factor and
/// [`crate::UsageError::BufferTooSmall`] when `dst` cannot hold the result.
pub fn pack_ycbcr_subsampling(
    src: &[u8],
    width: u32,
    height: u32,
    subsampling: (u16, u16),
    bytes_per_sample: usize,
    dst: &mut [u8],
) -> Result<()> {
    let (h, v) = subsampling;
    if h == 0 || v == 0 {
        return Err(TiffError::Format(FormatError::IllegalSubsampling(h, v)));
    }
    let hu = usize::from(h);
    let vu = usize::from(v);
    let unit_samples = hu * vu + 2;
    let unit_bytes = unit_samples * bytes_per_sample;
    let units_across = (width as usize).div_ceil(hu);
    let unit_rows = (height as usize).div_ceil(vu);
    let need = units_across
        .checked_mul(unit_rows)
        .and_then(|n| n.checked_mul(unit_bytes))
        .ok_or(TiffError::IntOverflow)?;
    if dst.len() < need {
        return Err(TiffError::Usage(crate::error::UsageError::BufferTooSmall {
            needed: need,
            got: dst.len(),
        }));
    }

    for unit_row in 0..unit_rows {
        for unit_col in 0..units_across {
            let unit_index = unit_row * units_across + unit_col;
            let base = unit_index * unit_bytes;
            for dy in 0..vu {
                for dx in 0..hu {
                    let y = (unit_row * vu + dy).min(height.saturating_sub(1) as usize);
                    let x = (unit_col * hu + dx).min(width.saturating_sub(1) as usize);
                    let input = (y * width as usize + x) * 3 * bytes_per_sample;
                    let luma_off = base + (dy * hu + dx) * bytes_per_sample;
                    copy_sample(src, input, dst, luma_off, bytes_per_sample);
                }
            }
            let y = (unit_row * vu).min(height.saturating_sub(1) as usize);
            let x = (unit_col * hu).min(width.saturating_sub(1) as usize);
            let input = (y * width as usize + x) * 3 * bytes_per_sample;
            let cb_off = base + hu * vu * bytes_per_sample;
            copy_sample(src, input + bytes_per_sample, dst, cb_off, bytes_per_sample);
            copy_sample(
                src,
                input + 2 * bytes_per_sample,
                dst,
                cb_off + bytes_per_sample,
                bytes_per_sample,
            );
        }
    }
    Ok(())
}

fn copy_sample(src: &[u8], from: usize, dst: &mut [u8], to: usize, width: usize) {
    let Some(input) = src.get(from..from + width) else {
        return;
    };
    let Some(output) = dst.get_mut(to..to + width) else {
        return;
    };
    output.copy_from_slice(input);
}

/// Converts a CMYK quadruple to 8-bit RGB.
#[must_use]
pub fn cmyk_to_rgb8(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let inv_k = u32::from(255 - k);
    [
        ((u32::from(255 - c) * inv_k) / 255) as u8,
        ((u32::from(255 - m) * inv_k) / 255) as u8,
        ((u32::from(255 - y) * inv_k) / 255) as u8,
    ]
}

/// Converts a CIE L*a*b* triple (already in `L` 0..100, `a`/`b` signed) to
/// 8-bit sRGB using the D50 white point TIFF specifies.
#[must_use]
pub fn lab_to_rgb8(l: f64, a: f64, b: f64) -> [u8; 3] {
    // Lab -> XYZ (D50).
    let fy = (l + 16.0) / 116.0;
    let fx = fy + a / 500.0;
    let fz = fy - b / 200.0;
    let finv = |t: f64| {
        if t > 6.0 / 29.0 {
            t * t * t
        } else {
            3.0 * (6.0f64 / 29.0).powi(2) * (t - 4.0 / 29.0)
        }
    };
    let x = 0.964_212 * finv(fx);
    let y = 1.0 * finv(fy);
    let z = 0.825_188 * finv(fz);

    // XYZ (D50) -> linear sRGB (Bradford-adapted matrix).
    let r = 3.134_136 * x - 1.617_386 * y - 0.490_662 * z;
    let g = -0.978_795 * x + 1.916_254 * y + 0.033_442 * z;
    let bl = 0.071_955 * x - 0.228_977 * y + 1.405_386 * z;

    let gamma = |v: f64| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    };
    [
        (gamma(r) * 255.0).round().clamp(0.0, 255.0) as u8,
        (gamma(g) * 255.0).round().clamp(0.0, 255.0) as u8,
        (gamma(bl) * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

/// Un-premultiplies a colour channel by an associated alpha.
#[must_use]
pub fn unpremultiply(colour: u8, alpha: u8) -> u8 {
    if alpha == 0 {
        return 0;
    }
    ((u32::from(colour) * 255) / u32::from(alpha)).min(255) as u8
}

/// Converts a decoded image to interleaved 8-bit RGB.
///
/// # Errors
/// [`UnsupportedError::Conversion`] for a photometric with no defined RGB
/// mapping, [`UnsupportedError::InkSet`] for a non-CMYK separated image, and
/// [`FormatError::ColorMapWrongLength`] for a broken palette.
pub fn to_rgb8(info: &ImageInfo, samples: &Samples) -> Result<Vec<u8>> {
    let rgba = to_rgba8(info, samples)?;
    let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        out.extend_from_slice(pixel.get(..3).unwrap_or(&[0, 0, 0]));
    }
    Ok(out)
}

/// Converts a decoded image to interleaved 8-bit RGBA with straight alpha.
///
/// Associated (premultiplied) alpha is undone, and the fact that it was is
/// documented here rather than left to the caller to discover.
///
/// # Errors
/// The same set as [`to_rgb8`].
pub fn to_rgba8(info: &ImageInfo, samples: &Samples) -> Result<Vec<u8>> {
    let pixels = (info.width as usize)
        .checked_mul(info.height as usize)
        .ok_or(TiffError::IntOverflow)?;
    let spp = usize::from(info.samples_per_pixel);
    let bits = info.bits_per_sample.first().copied().unwrap_or(8);
    let max = if bits >= 64 {
        u64::MAX as f64
    } else {
        ((1u64 << bits) - 1) as f64
    };
    let alpha_index = info
        .extra_samples
        .iter()
        .position(|e| {
            matches!(
                e,
                ExtraSamples::AssociatedAlpha | ExtraSamples::UnassociatedAlpha
            )
        })
        .map(|i| i + spp - info.extra_samples.len());
    let associated = info
        .extra_samples
        .first()
        .copied()
        .map(|e| e == ExtraSamples::AssociatedAlpha)
        .unwrap_or(false);

    let mut out = Vec::with_capacity(pixels * 4);
    match info.photometric {
        PhotometricInterpretation::Palette => {
            let map = info.color_map.as_deref().ok_or_else(|| {
                TiffError::Format(FormatError::ColorMapWrongLength {
                    expected: 3 << bits.min(16),
                    got: 0,
                })
            })?;
            let indices: Vec<u16> = (0..pixels)
                .map(|i| sample_at(samples, i * spp) as u16)
                .collect();
            let rgb = expand_palette_rgb8(&indices, map, bits)?;
            for pixel in rgb.chunks_exact(3) {
                out.extend_from_slice(pixel);
                out.push(255);
            }
        }
        PhotometricInterpretation::WhiteIsZero
        | PhotometricInterpretation::BlackIsZero
        | PhotometricInterpretation::TransparencyMask
        | PhotometricInterpretation::ColorFilterArray => {
            let invert = info.photometric == PhotometricInterpretation::WhiteIsZero
                || info.photometric == PhotometricInterpretation::TransparencyMask;
            for i in 0..pixels {
                let raw = sample_at(samples, i * spp);
                let mut grey = scale_to_u8(raw, bits);
                if invert {
                    grey = 255 - grey;
                }
                let alpha = alpha_index
                    .map(|a| scale_to_u8(sample_at(samples, i * spp + a), bits))
                    .unwrap_or(255);
                let value = if associated {
                    unpremultiply(grey, alpha)
                } else {
                    grey
                };
                out.extend_from_slice(&[value, value, value, alpha]);
            }
        }
        PhotometricInterpretation::Rgb | PhotometricInterpretation::LinearRaw => {
            for i in 0..pixels {
                let base = i * spp;
                let mut rgb = [
                    scale_to_u8(sample_at(samples, base), bits),
                    scale_to_u8(sample_at(samples, base + 1), bits),
                    scale_to_u8(sample_at(samples, base + 2), bits),
                ];
                let alpha = alpha_index
                    .map(|a| scale_to_u8(sample_at(samples, base + a), bits))
                    .unwrap_or(255);
                if associated {
                    for channel in &mut rgb {
                        *channel = unpremultiply(*channel, alpha);
                    }
                }
                out.extend_from_slice(&rgb);
                out.push(alpha);
            }
        }
        PhotometricInterpretation::YCbCr => {
            let rbw = info
                .reference_black_white
                .map(|pairs| {
                    let mut values = [0.0f64; 6];
                    for (slot, pair) in values.iter_mut().zip(pairs.iter()) {
                        *slot = pair.as_f64().unwrap_or(0.0);
                    }
                    values
                })
                .unwrap_or_else(|| {
                    default_reference_black_white(PhotometricInterpretation::YCbCr, bits)
                });
            for i in 0..pixels {
                let base = i * spp;
                let [r, g, b] = ycbcr_to_rgb(
                    sample_as_f64(samples, base),
                    sample_as_f64(samples, base + 1),
                    sample_as_f64(samples, base + 2),
                    info.ycbcr_coefficients,
                    rbw,
                    max,
                );
                out.extend_from_slice(&[
                    scale_to_u8(r.clamp(0.0, max) as u64, bits),
                    scale_to_u8(g.clamp(0.0, max) as u64, bits),
                    scale_to_u8(b.clamp(0.0, max) as u64, bits),
                    255,
                ]);
            }
        }
        PhotometricInterpretation::Separated => {
            if info.samples_per_pixel < 4 {
                return Err(TiffError::Unsupported(UnsupportedError::Conversion(
                    "a separated image needs at least four channels to convert to RGB",
                )));
            }
            for i in 0..pixels {
                let base = i * spp;
                let rgb = cmyk_to_rgb8(
                    scale_to_u8(sample_at(samples, base), bits),
                    scale_to_u8(sample_at(samples, base + 1), bits),
                    scale_to_u8(sample_at(samples, base + 2), bits),
                    scale_to_u8(sample_at(samples, base + 3), bits),
                );
                let alpha = if spp > 4 {
                    scale_to_u8(sample_at(samples, base + 4), bits)
                } else {
                    255
                };
                out.extend_from_slice(&rgb);
                out.push(alpha);
            }
        }
        PhotometricInterpretation::CieLab
        | PhotometricInterpretation::IccLab
        | PhotometricInterpretation::ItuLab => {
            let biased = info.photometric != PhotometricInterpretation::CieLab;
            let l_scale = if bits >= 16 { 65280.0 } else { 255.0 };
            for i in 0..pixels {
                let base = i * spp;
                let l = sample_at(samples, base) as f64 / l_scale * 100.0;
                let (a, b) = if biased {
                    (
                        sample_at(samples, base + 1) as f64 - 128.0,
                        sample_at(samples, base + 2) as f64 - 128.0,
                    )
                } else {
                    (
                        signed_component(samples, base + 1, bits),
                        signed_component(samples, base + 2, bits),
                    )
                };
                let rgb = lab_to_rgb8(l, a, b);
                out.extend_from_slice(&rgb);
                out.push(255);
            }
        }
        other => {
            let _ = other;
            return Err(TiffError::Unsupported(UnsupportedError::Photometric(
                info.photometric.to_u16(),
            )));
        }
    }
    Ok(out)
}

/// Reads a channel that CIE L*a*b* stores as a signed value.
fn signed_component(samples: &Samples, index: usize, bits: u16) -> f64 {
    let raw = sample_at(samples, index);
    if bits <= 8 {
        (raw as u8) as i8 as f64
    } else if bits <= 16 {
        (raw as u16) as i16 as f64
    } else {
        raw as i64 as f64
    }
}

/// Rejects a separated image whose `InkSet` is not CMYK.
///
/// # Errors
/// [`UnsupportedError::InkSet`] for `InkSet` 2 (not-CMYK).
pub fn check_ink_set(ink_set: InkSet) -> Result<()> {
    if ink_set == InkSet::NotCmyk {
        return Err(TiffError::Unsupported(UnsupportedError::InkSet(
            ink_set.to_u16(),
        )));
    }
    Ok(())
}

/// The slot type a converted RGB(A) buffer uses.
#[must_use]
pub const fn rgb_sample_type() -> SampleType {
    SampleType::U8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaling_saturates_at_both_ends() {
        assert_eq!(scale_to_u8(0, 1), 0);
        assert_eq!(scale_to_u8(1, 1), 255);
        assert_eq!(scale_to_u8(0, 4), 0);
        assert_eq!(scale_to_u8(15, 4), 255);
        assert_eq!(scale_to_u8(8, 4), 136);
        assert_eq!(scale_to_u8(255, 8), 255);
        assert_eq!(scale_to_u8(0xFFFF, 16), 255);
        assert_eq!(scale_to_u8(0x8000, 16), 128);
        assert_eq!(scale_to_u8(5, 0), 0);
    }

    #[test]
    fn min_is_white_inverts_every_integer_width() {
        let mut s = Samples::U8(vec![0, 255, 10]);
        invert_min_is_white(&mut s, 8);
        assert_eq!(s.as_u8(), Some(&[255u8, 0, 245][..]));
        let mut s = Samples::U16(vec![0, 0xFFFF]);
        invert_min_is_white(&mut s, 16);
        assert_eq!(s.as_u16(), Some(&[0xFFFFu16, 0][..]));
        let mut s = Samples::U8(vec![0, 1]);
        invert_min_is_white(&mut s, 1);
        assert_eq!(s.as_u8(), Some(&[1u8, 0][..]));
        let mut s = Samples::F32(vec![0.25]);
        invert_min_is_white(&mut s, 32);
        assert_eq!(s.as_f32(), Some(&[0.75f32][..]));
    }

    #[test]
    fn palette_expansion_follows_the_planar_colormap_layout() {
        // Four entries: reds, then greens, then blues.
        let map = vec![
            0x0000, 0xFFFF, 0x0000, 0x0000, // reds
            0x0000, 0x0000, 0xFFFF, 0x0000, // greens
            0x0000, 0x0000, 0x0000, 0xFFFF, // blues
        ];
        let rgb = expand_palette_rgb8(&[0, 1, 2, 3], &map, 2).expect("expand");
        assert_eq!(rgb, vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn a_short_colormap_is_rejected() {
        let err = expand_palette_rgb8(&[0], &[0, 0], 4).expect_err("short map");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::ColorMapWrongLength { .. })
        ));
    }

    #[test]
    fn out_of_range_palette_indices_are_clamped_not_panicked_on() {
        let map = vec![0u16; 6];
        let rgb = expand_palette_rgb8(&[7], &map, 1).expect("clamped");
        assert_eq!(rgb, vec![0, 0, 0]);
    }

    #[test]
    fn ycbcr_defaults_match_the_spec() {
        assert_eq!(
            default_reference_black_white(PhotometricInterpretation::YCbCr, 8),
            [0.0, 255.0, 128.0, 255.0, 128.0, 255.0]
        );
        assert_eq!(
            default_reference_black_white(PhotometricInterpretation::Rgb, 8),
            [0.0, 255.0, 0.0, 255.0, 0.0, 255.0]
        );
    }

    #[test]
    fn ycbcr_neutral_chroma_is_grey() {
        let rbw = default_reference_black_white(PhotometricInterpretation::YCbCr, 8);
        let [r, g, b] = ycbcr_to_rgb(128.0, 128.0, 128.0, [0.299, 0.587, 0.114], rbw, 255.0);
        assert!((r - 128.0).abs() < 1.0, "r = {r}");
        assert!((g - 128.0).abs() < 1.0, "g = {g}");
        assert!((b - 128.0).abs() < 1.0, "b = {b}");
    }

    #[test]
    fn ycbcr_primaries_round_trip_approximately() {
        let rbw = default_reference_black_white(PhotometricInterpretation::YCbCr, 8);
        // Pure red in ITU-R BT.601: Y = 76.245, Cb = 84.97, Cr = 255.
        let [r, g, b] = ycbcr_to_rgb(76.245, 84.97, 255.0, [0.299, 0.587, 0.114], rbw, 255.0);
        assert!(r > 250.0, "r = {r}");
        assert!(g.abs() < 2.0, "g = {g}");
        assert!(b.abs() < 2.0, "b = {b}");
    }

    #[test]
    fn subsampling_expansion_round_trips() {
        for subsampling in [(1u16, 1u16), (2, 1), (2, 2), (4, 2), (4, 4)] {
            let (w, h) = (7u32, 5u32);
            let mut full = vec![0u8; (w * h * 3) as usize];
            for (i, byte) in full.iter_mut().enumerate() {
                *byte = (i % 251) as u8;
            }
            let hu = usize::from(subsampling.0);
            let vu = usize::from(subsampling.1);
            let units = (w as usize).div_ceil(hu) * (h as usize).div_ceil(vu);
            let mut packed = vec![0u8; units * (hu * vu + 2)];
            pack_ycbcr_subsampling(&full, w, h, subsampling, 1, &mut packed).expect("pack");
            let mut back = vec![0u8; full.len()];
            expand_ycbcr_subsampling(&packed, w, h, subsampling, 1, &mut back).expect("expand");
            // Luma survives exactly; chroma is block-constant.
            for i in 0..(w * h) as usize {
                assert_eq!(back[i * 3], full[i * 3], "luma {i} at {subsampling:?}");
            }
            if subsampling == (1, 1) {
                assert_eq!(back, full);
            }
        }
    }

    #[test]
    fn subsampling_rejects_a_zero_factor_and_a_short_buffer() {
        let mut dst = vec![0u8; 12];
        assert!(expand_ycbcr_subsampling(&[0; 12], 2, 2, (0, 2), 1, &mut dst).is_err());
        let mut tiny = vec![0u8; 2];
        assert!(expand_ycbcr_subsampling(&[0; 12], 2, 2, (2, 2), 1, &mut tiny).is_err());
        assert!(pack_ycbcr_subsampling(&[0; 12], 2, 2, (2, 0), 1, &mut dst).is_err());
        assert!(pack_ycbcr_subsampling(&[0; 12], 2, 2, (2, 2), 1, &mut tiny).is_err());
    }

    #[test]
    fn cmyk_conversion_matches_the_naive_formula() {
        assert_eq!(cmyk_to_rgb8(0, 0, 0, 0), [255, 255, 255]);
        assert_eq!(cmyk_to_rgb8(0, 0, 0, 255), [0, 0, 0]);
        assert_eq!(cmyk_to_rgb8(255, 0, 0, 0), [0, 255, 255]);
    }

    #[test]
    fn lab_white_and_black_map_to_white_and_black() {
        assert_eq!(lab_to_rgb8(100.0, 0.0, 0.0), [255, 255, 255]);
        assert_eq!(lab_to_rgb8(0.0, 0.0, 0.0), [0, 0, 0]);
    }

    #[test]
    fn unpremultiply_guards_zero_alpha() {
        assert_eq!(unpremultiply(128, 0), 0);
        assert_eq!(unpremultiply(128, 255), 128);
        assert_eq!(unpremultiply(64, 128), 127);
        assert_eq!(unpremultiply(200, 100), 255);
    }

    #[test]
    fn ink_set_two_refuses_rgb_conversion() {
        assert!(check_ink_set(InkSet::Cmyk).is_ok());
        assert!(check_ink_set(InkSet::NotCmyk).is_err());
        assert_eq!(rgb_sample_type(), SampleType::U8);
    }
}

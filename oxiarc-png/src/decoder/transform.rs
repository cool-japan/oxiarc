//! Output shaping: turning a reconstructed scanline into the colour type and
//! bit depth the caller asked for.
//!
//! The output colour-type table is reproduced exactly from `png` 0.18, because
//! the `image` crate refuses any surviving `Indexed` or sub-byte output and a
//! single divergence would silently break every downstream consumer. See
//! `output_color_type` and the table test in `tests/transform_table.rs`.

use crate::common::Transformations;
use crate::error::{DecodingError, FormatErrorKind};
use crate::header::{BitDepth, ColorType};
use crate::info::Info;

/// The colour type and bit depth rows are delivered in, for a given input and
/// a given set of requested transformations.
///
/// ```
/// use oxiarc_png::decoder::output_color_type;
/// use oxiarc_png::{BitDepth, ColorType, Transformations};
/// // An indexed image without transparency expands to RGB8.
/// assert_eq!(
///     output_color_type(ColorType::Indexed, BitDepth::Four, false, Transformations::EXPAND),
///     (ColorType::Rgb, BitDepth::Eight)
/// );
/// // ... and to RGBA8 once a tRNS chunk is present.
/// assert_eq!(
///     output_color_type(ColorType::Indexed, BitDepth::Four, true, Transformations::EXPAND),
///     (ColorType::Rgba, BitDepth::Eight)
/// );
/// ```
#[must_use]
pub fn output_color_type(
    color_type: ColorType,
    bit_depth: BitDepth,
    has_trns_chunk: bool,
    transform: Transformations,
) -> (ColorType, BitDepth) {
    if transform == Transformations::IDENTITY {
        return (color_type, bit_depth);
    }
    let depth = bit_depth as u8;
    let expand =
        transform.contains(Transformations::EXPAND) || transform.contains(Transformations::ALPHA);
    // Both 16-bit-with-STRIP_16 and sub-byte-with-EXPAND land on eight bits;
    // everything else keeps the file's own depth.
    let to_eight =
        (depth == 16 && transform.intersects(Transformations::STRIP_16)) || (depth < 8 && expand);
    let bits = if to_eight { 8 } else { depth };
    let out_color = if expand {
        let has_trns = has_trns_chunk || transform.contains(Transformations::ALPHA);
        match color_type {
            ColorType::Grayscale if has_trns => ColorType::GrayscaleAlpha,
            ColorType::Rgb if has_trns => ColorType::Rgba,
            ColorType::Indexed if has_trns => ColorType::Rgba,
            ColorType::Indexed => ColorType::Rgb,
            other => other,
        }
    } else {
        color_type
    };
    let out_depth = BitDepth::from_u8(bits).unwrap_or(bit_depth);
    (out_color, out_depth)
}

/// The row transformation selected for an image.
#[derive(Clone, Debug)]
pub(crate) enum Transform {
    /// Rows are already in output form.
    Copy,
    /// Keep the high byte of every 16-bit sample.
    Strip16,
    /// Unpack sub-byte grayscale to 8 bits, scaling by replication.
    ExpandGray { scale: u8 },
    /// Unpack sub-byte grayscale to 8-bit grayscale-alpha.
    ExpandGrayTrns { scale: u8, key: Option<u8> },
    /// Add an alpha channel to 8-bit samples by comparing against a key.
    ExpandTrns8 {
        channels: usize,
        key: Option<Vec<u8>>,
    },
    /// Add an alpha channel to 16-bit samples by comparing against a key.
    ExpandTrns16 {
        channels: usize,
        key: Option<Vec<u8>>,
    },
    /// Add alpha and strip to 8 bits in one pass.
    ExpandTrnsStrip16 {
        channels: usize,
        key: Option<Vec<u8>>,
    },
    /// Expand palette indices to RGB8.
    PaletteRgb {
        lut: Box<[[u8; 3]; 256]>,
        entries: usize,
        depth: u8,
    },
    /// Expand palette indices to RGBA8.
    PaletteRgba {
        lut: Box<[[u8; 4]; 256]>,
        entries: usize,
        depth: u8,
    },
}

/// Build the transformation for an image and a set of requested output
/// changes.
pub(crate) fn create_transform(
    info: &Info<'_>,
    transform: Transformations,
) -> Result<Transform, DecodingError> {
    let depth = info.bit_depth as u8;
    let expand =
        transform.contains(Transformations::EXPAND) || transform.contains(Transformations::ALPHA);
    let has_trns = info.trns.is_some() || transform.contains(Transformations::ALPHA);
    let strip16 = depth == 16 && transform.intersects(Transformations::STRIP_16);
    let key = info.trns.as_ref().map(|t| t.to_vec());

    match info.color_type {
        ColorType::Indexed if expand => {
            let Some(palette) = info.palette.as_deref() else {
                return Err(FormatErrorKind::PaletteRequired.into());
            };
            if info.bit_depth == BitDepth::Sixteen {
                return Err(FormatErrorKind::InvalidColorBitDepth {
                    color_type: ColorType::Indexed,
                    bit_depth: BitDepth::Sixteen,
                }
                .into());
            }
            let entries = palette.len() / 3;
            if has_trns {
                // Unlisted indices render as opaque black, which is what
                // libpng and `png` 0.18 do.
                let mut lut = Box::new([[0u8, 0, 0, 0xFF]; 256]);
                for (i, entry) in palette.chunks_exact(3).take(256).enumerate() {
                    lut[i][0] = entry[0];
                    lut[i][1] = entry[1];
                    lut[i][2] = entry[2];
                }
                if let Some(alpha) = info.trns.as_deref() {
                    for (i, a) in alpha.iter().take(entries.min(256)).enumerate() {
                        lut[i][3] = *a;
                    }
                }
                Ok(Transform::PaletteRgba {
                    lut,
                    entries,
                    depth,
                })
            } else {
                let mut lut = Box::new([[0u8; 3]; 256]);
                for (i, entry) in palette.chunks_exact(3).take(256).enumerate() {
                    lut[i].copy_from_slice(entry);
                }
                Ok(Transform::PaletteRgb {
                    lut,
                    entries,
                    depth,
                })
            }
        }
        ColorType::Grayscale | ColorType::GrayscaleAlpha if depth < 8 && expand => {
            let scale = 255 / ((1u16 << depth) - 1) as u8;
            if has_trns {
                Ok(Transform::ExpandGrayTrns {
                    scale,
                    key: key.and_then(|k| k.first().copied()),
                })
            } else {
                Ok(Transform::ExpandGray { scale })
            }
        }
        ColorType::Grayscale | ColorType::Rgb if expand && has_trns => {
            let channels = info.color_type.samples();
            if depth == 8 {
                Ok(Transform::ExpandTrns8 { channels, key })
            } else if strip16 {
                Ok(Transform::ExpandTrnsStrip16 { channels, key })
            } else {
                Ok(Transform::ExpandTrns16 { channels, key })
            }
        }
        _ if strip16 => Ok(Transform::Strip16),
        _ => Ok(Transform::Copy),
    }
}

/// Read the `index`-th sub-byte sample of a packed row, MSB first.
#[inline]
fn sample(row: &[u8], index: usize, depth: u8) -> u8 {
    if depth == 8 {
        return row.get(index).copied().unwrap_or(0);
    }
    let bit = index * usize::from(depth);
    let byte = bit / 8;
    let shift = 8 - depth - (bit % 8) as u8;
    let mask = (1u16 << depth) as u8 - 1;
    (row.get(byte).copied().unwrap_or(0) >> shift) & mask
}

impl Transform {
    /// Apply the transformation to one reconstructed scanline.
    ///
    /// `out` must be exactly the output line size for the row's pixel count.
    pub(crate) fn apply(
        &self,
        row: &[u8],
        out: &mut [u8],
        strict_palette: bool,
    ) -> Result<(), DecodingError> {
        match self {
            Transform::Copy => {
                let n = row.len().min(out.len());
                out[..n].copy_from_slice(&row[..n]);
            }
            Transform::Strip16 => {
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = row.get(i * 2).copied().unwrap_or(0);
                }
            }
            Transform::ExpandGray { scale } => {
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = sample(row, i, self.depth_hint()) * scale;
                }
            }
            Transform::ExpandGrayTrns { scale, key } => {
                let depth = self.depth_hint();
                for (i, pair) in out.chunks_exact_mut(2).enumerate() {
                    let value = sample(row, i, depth);
                    pair[0] = value * scale;
                    pair[1] = match key {
                        Some(k) if *k == value => 0,
                        _ => 0xFF,
                    };
                }
            }
            Transform::ExpandTrns8 { channels, key } => {
                let key = key.as_deref();
                for (src, dst) in row
                    .chunks_exact(*channels)
                    .zip(out.chunks_exact_mut(channels + 1))
                {
                    dst[..*channels].copy_from_slice(src);
                    dst[*channels] = if key == Some(src) { 0 } else { 0xFF };
                }
            }
            Transform::ExpandTrns16 { channels, key } => {
                let key = key.as_deref();
                let n = channels * 2;
                for (src, dst) in row.chunks_exact(n).zip(out.chunks_exact_mut(n + 2)) {
                    dst[..n].copy_from_slice(src);
                    let opaque = if key == Some(src) { 0x00 } else { 0xFF };
                    dst[n] = opaque;
                    dst[n + 1] = opaque;
                }
            }
            Transform::ExpandTrnsStrip16 { channels, key } => {
                let key = key.as_deref();
                let n = channels * 2;
                for (src, dst) in row.chunks_exact(n).zip(out.chunks_exact_mut(channels + 1)) {
                    for c in 0..*channels {
                        dst[c] = src[c * 2];
                    }
                    dst[*channels] = if key == Some(src) { 0 } else { 0xFF };
                }
            }
            Transform::PaletteRgb {
                lut,
                entries,
                depth,
            } => {
                for (i, dst) in out.chunks_exact_mut(3).enumerate() {
                    let index = sample(row, i, *depth);
                    if strict_palette && usize::from(index) >= *entries {
                        return Err(FormatErrorKind::PaletteIndexOutOfRange {
                            index: u16::from(index),
                            entries: *entries,
                        }
                        .into());
                    }
                    dst.copy_from_slice(&lut[usize::from(index)]);
                }
            }
            Transform::PaletteRgba {
                lut,
                entries,
                depth,
            } => {
                for (i, dst) in out.chunks_exact_mut(4).enumerate() {
                    let index = sample(row, i, *depth);
                    if strict_palette && usize::from(index) >= *entries {
                        return Err(FormatErrorKind::PaletteIndexOutOfRange {
                            index: u16::from(index),
                            entries: *entries,
                        }
                        .into());
                    }
                    dst.copy_from_slice(&lut[usize::from(index)]);
                }
            }
        }
        Ok(())
    }

    /// The source bit depth the sub-byte variants unpack from.
    fn depth_hint(&self) -> u8 {
        match self {
            Transform::PaletteRgb { depth, .. } | Transform::PaletteRgba { depth, .. } => *depth,
            Transform::ExpandGray { scale } | Transform::ExpandGrayTrns { scale, .. } => {
                // The replication factor determines the depth uniquely:
                // 255 -> 1 bit, 85 -> 2 bits, 17 -> 4 bits.
                match scale {
                    255 => 1,
                    85 => 2,
                    _ => 4,
                }
            }
            _ => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn info(color_type: ColorType, bit_depth: BitDepth) -> Info<'static> {
        let mut info = Info::with_size(4, 1);
        info.color_type = color_type;
        info.bit_depth = bit_depth;
        info
    }

    #[test]
    fn identity_never_changes_the_output_type() {
        for ct in [
            ColorType::Grayscale,
            ColorType::Rgb,
            ColorType::Indexed,
            ColorType::GrayscaleAlpha,
            ColorType::Rgba,
        ] {
            for bd in [BitDepth::One, BitDepth::Eight, BitDepth::Sixteen] {
                if ColorType::is_combination_invalid(ct, bd) {
                    continue;
                }
                assert_eq!(
                    output_color_type(ct, bd, true, Transformations::IDENTITY),
                    (ct, bd)
                );
            }
        }
    }

    #[test]
    fn strip16_only_touches_sixteen_bit_images() {
        assert_eq!(
            output_color_type(
                ColorType::Rgb,
                BitDepth::Sixteen,
                false,
                Transformations::STRIP_16
            ),
            (ColorType::Rgb, BitDepth::Eight)
        );
        assert_eq!(
            output_color_type(
                ColorType::Rgb,
                BitDepth::Eight,
                false,
                Transformations::STRIP_16
            ),
            (ColorType::Rgb, BitDepth::Eight)
        );
        assert_eq!(
            output_color_type(
                ColorType::Grayscale,
                BitDepth::Four,
                false,
                Transformations::STRIP_16
            ),
            (ColorType::Grayscale, BitDepth::Four)
        );
    }

    #[test]
    fn expand_gray_unpacks_and_scales_by_replication() {
        let mut i = info(ColorType::Grayscale, BitDepth::Two);
        i.width = 4;
        let t = create_transform(&i, Transformations::EXPAND).expect("transform");
        let mut out = [0u8; 4];
        t.apply(&[0b00_01_10_11], &mut out, false).expect("apply");
        assert_eq!(out, [0, 85, 170, 255]);
    }

    #[test]
    fn expand_gray_with_trns_adds_alpha_at_the_source_depth() {
        let mut i = info(ColorType::Grayscale, BitDepth::One);
        i.trns = Some(Cow::Owned(vec![1]));
        let t = create_transform(&i, Transformations::EXPAND).expect("transform");
        let mut out = [0u8; 4];
        t.apply(&[0b1010_0000], &mut out, false).expect("apply");
        // Pixels: 1, 0 -> (255, transparent), (0, opaque)
        assert_eq!(out, [255, 0, 0, 255]);
    }

    #[test]
    fn expand_trns_eight_bit_compares_whole_pixels() {
        let mut i = info(ColorType::Rgb, BitDepth::Eight);
        i.trns = Some(Cow::Owned(vec![1, 2, 3]));
        let t = create_transform(&i, Transformations::EXPAND).expect("transform");
        let mut out = [0u8; 8];
        t.apply(&[1, 2, 3, 1, 2, 4], &mut out, false)
            .expect("apply");
        assert_eq!(out, [1, 2, 3, 0, 1, 2, 4, 0xFF]);
    }

    #[test]
    fn expand_trns_sixteen_bit_and_strip_variants() {
        let mut i = info(ColorType::Grayscale, BitDepth::Sixteen);
        i.trns = Some(Cow::Owned(vec![0x12, 0x34]));
        let t = create_transform(&i, Transformations::EXPAND).expect("transform");
        let mut out = [0u8; 8];
        t.apply(&[0x12, 0x34, 0x56, 0x78], &mut out, false)
            .expect("apply");
        assert_eq!(out, [0x12, 0x34, 0, 0, 0x56, 0x78, 0xFF, 0xFF]);

        let t = create_transform(&i, Transformations::EXPAND | Transformations::STRIP_16)
            .expect("transform");
        let mut out = [0u8; 4];
        t.apply(&[0x12, 0x34, 0x56, 0x78], &mut out, false)
            .expect("apply");
        assert_eq!(out, [0x12, 0, 0x56, 0xFF]);
    }

    #[test]
    fn alpha_without_a_trns_chunk_is_fully_opaque() {
        let i = info(ColorType::Rgb, BitDepth::Eight);
        let t = create_transform(&i, Transformations::ALPHA).expect("transform");
        let mut out = [0u8; 8];
        t.apply(&[1, 2, 3, 4, 5, 6], &mut out, false)
            .expect("apply");
        assert_eq!(out, [1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);
    }

    #[test]
    fn palette_expansion_and_out_of_range_policy() {
        let mut i = info(ColorType::Indexed, BitDepth::Two);
        i.palette = Some(Cow::Owned(vec![10, 11, 12, 20, 21, 22]));
        let t = create_transform(&i, Transformations::EXPAND).expect("transform");
        let mut out = [0u8; 12];
        // Indices 0, 1, 2, 3 -- the last two have no palette entry.
        t.apply(&[0b00_01_10_11], &mut out, false).expect("apply");
        assert_eq!(out, [10, 11, 12, 20, 21, 22, 0, 0, 0, 0, 0, 0]);
        assert!(t.apply(&[0b00_01_10_11], &mut out, true).is_err());
    }

    #[test]
    fn palette_with_trns_produces_rgba_and_opaque_black_for_gaps() {
        let mut i = info(ColorType::Indexed, BitDepth::Eight);
        i.palette = Some(Cow::Owned(vec![10, 11, 12, 20, 21, 22]));
        i.trns = Some(Cow::Owned(vec![0x40]));
        let t = create_transform(&i, Transformations::EXPAND).expect("transform");
        let mut out = [0u8; 12];
        t.apply(&[0, 1, 7], &mut out, false).expect("apply");
        assert_eq!(out, [10, 11, 12, 0x40, 20, 21, 22, 0xFF, 0, 0, 0, 0xFF]);
    }

    #[test]
    fn indexed_without_a_palette_is_an_error() {
        let i = info(ColorType::Indexed, BitDepth::Eight);
        assert!(create_transform(&i, Transformations::EXPAND).is_err());
    }

    #[test]
    fn copy_is_selected_when_nothing_has_to_change() {
        let i = info(ColorType::Rgba, BitDepth::Eight);
        assert!(matches!(
            create_transform(&i, Transformations::EXPAND).expect("transform"),
            Transform::Copy
        ));
        assert!(matches!(
            create_transform(&i, Transformations::IDENTITY).expect("transform"),
            Transform::Copy
        ));
    }
}

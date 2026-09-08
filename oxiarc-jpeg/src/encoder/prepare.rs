//! Turning the caller's interleaved pixels into per-component sample planes.
//!
//! The order is libjpeg's: colour-convert at full resolution, replicate the
//! last real row and column out to the MCU grid, then decimate. Replicating
//! before or after the (pointwise) colour conversion gives the same samples,
//! but decimating before replicating does not — the edge blocks would average
//! in whatever the allocation happened to contain.

use super::plan::EncodePlan;
use crate::color::ColorSpace;
use crate::color::forward::{rgb_to_cb, rgb_to_cr, rgb_to_y, rgb8_to_ycbcr};
use crate::downsample::{Downsampling, Plane, downsample_component};
use crate::encoder::options::InputColor;
use crate::error::{JpegError, Result};

/// A borrowed pixel buffer of either width.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Samples<'a> {
    /// Eight-bit samples.
    Eight(&'a [u8]),
    /// Samples of nine to sixteen bits, right-aligned.
    Wide(&'a [u16]),
}

impl Samples<'_> {
    /// Number of samples in the buffer.
    pub(crate) fn len(&self) -> usize {
        match self {
            Samples::Eight(data) => data.len(),
            Samples::Wide(data) => data.len(),
        }
    }
}

/// Which source channels feed the red, green and blue slots.
const fn rgb_order(input: InputColor) -> (usize, usize, usize) {
    match input {
        InputColor::Bgr | InputColor::Bgra => (2, 1, 0),
        _ => (0, 1, 2),
    }
}

/// The per-pixel transform, chosen once for the whole image.
///
/// Deciding this inside the pixel loop costs a two-enum match and a row
/// lookup per sample, which on a 1920x1080 frame is six million branches that
/// always go the same way.
#[derive(Debug, Clone, Copy)]
enum Conversion {
    /// Copy channel 0 into the single plane.
    Luma,
    /// RGB in, luminance only out.
    RgbToLuma,
    /// The full forward matrix.
    RgbToYcbcr,
    /// Three channels straight through, possibly reordered.
    RgbPassthrough,
    /// `n` channels straight through.
    Passthrough(usize),
    /// Complement CMY into RGB, run the matrix, pass K through.
    CmykToYcck,
}

/// Pick the transform for one input/output colour space pair.
fn conversion_for(input: InputColor, color: ColorSpace) -> Result<Conversion> {
    Ok(match (input, color) {
        (InputColor::Luma | InputColor::LumaAlpha, ColorSpace::Luma) => Conversion::Luma,
        // The alpha channel survives here instead of being dropped: it
        // becomes the frame's second, untransformed component.
        (InputColor::LumaAlpha, ColorSpace::Unknown(2)) => Conversion::Passthrough(2),
        (InputColor::Ycbcr, ColorSpace::Luma) => Conversion::Luma,
        (
            InputColor::Rgb | InputColor::Rgba | InputColor::Bgr | InputColor::Bgra,
            ColorSpace::Luma,
        ) => Conversion::RgbToLuma,
        (
            InputColor::Rgb | InputColor::Rgba | InputColor::Bgr | InputColor::Bgra,
            ColorSpace::Ycbcr,
        ) => Conversion::RgbToYcbcr,
        (
            InputColor::Rgb | InputColor::Rgba | InputColor::Bgr | InputColor::Bgra,
            ColorSpace::Rgb,
        ) => Conversion::RgbPassthrough,
        (InputColor::Ycbcr, ColorSpace::Ycbcr) => Conversion::Passthrough(3),
        (InputColor::Cmyk, ColorSpace::Cmyk) | (InputColor::Ycck, ColorSpace::Ycck) => {
            Conversion::Passthrough(4)
        }
        (InputColor::Cmyk, ColorSpace::Ycck) => Conversion::CmykToYcck,
        _ => {
            return Err(JpegError::InvalidEncodeParameter {
                parameter: "jpeg_color_space",
                reason: "this crate does not convert between those two colour spaces",
            });
        }
    })
}

/// Everything the per-row conversion loops need, gathered once.
#[derive(Debug, Clone, Copy)]
struct RowContext {
    width: usize,
    height: usize,
    channels: usize,
    /// Which source channels feed red, green and blue.
    order: (usize, usize, usize),
    center: i64,
    maxval: i64,
    conversion: Conversion,
}

/// Run one row of a three-plane conversion.
fn convert_rows_three<T: Copy + Into<i64>>(
    source: &[T],
    planes: &mut [Plane],
    y: usize,
    context: &RowContext,
) {
    let RowContext {
        width,
        channels,
        order,
        center,
        conversion,
        ..
    } = *context;
    let (first, rest) = planes.split_at_mut(1);
    let (second, third) = rest.split_at_mut(1);
    let a = first[0].row_mut(y);
    let b = second[0].row_mut(y);
    let c = third[0].row_mut(y);
    let (ri, gi, bi) = order;
    for x in 0..width {
        let base = x * channels;
        match conversion {
            Conversion::RgbToYcbcr => {
                let r: i64 = source[base + ri].into();
                let g: i64 = source[base + gi].into();
                let bl: i64 = source[base + bi].into();
                a[x] = rgb_to_y(r, g, bl) as u16;
                b[x] = rgb_to_cb(r, g, bl, center) as u16;
                c[x] = rgb_to_cr(r, g, bl, center) as u16;
            }
            Conversion::RgbPassthrough => {
                a[x] = Into::<i64>::into(source[base + ri]) as u16;
                b[x] = Into::<i64>::into(source[base + gi]) as u16;
                c[x] = Into::<i64>::into(source[base + bi]) as u16;
            }
            _ => {
                a[x] = Into::<i64>::into(source[base]) as u16;
                b[x] = Into::<i64>::into(source[base + 1]) as u16;
                c[x] = Into::<i64>::into(source[base + 2]) as u16;
            }
        }
    }
}

/// Run one row of a two-plane passthrough conversion.
///
/// The only caller is `Conversion::Passthrough(2)`
/// ([`InputColor::LumaAlpha`] into [`ColorSpace::Unknown`]'s two-component
/// case), so there is no reordering or matrix to select between — every
/// source channel copies straight into the plane of the same index.
fn convert_rows_two<T: Copy + Into<i64>>(
    source: &[T],
    planes: &mut [Plane],
    y: usize,
    context: &RowContext,
) {
    let RowContext {
        width, channels, ..
    } = *context;
    let (first, second) = planes.split_at_mut(1);
    let a = first[0].row_mut(y);
    let b = second[0].row_mut(y);
    for x in 0..width {
        let base = x * channels;
        a[x] = Into::<i64>::into(source[base]) as u16;
        b[x] = Into::<i64>::into(source[base + 1]) as u16;
    }
}

/// Run one row of a four-plane conversion.
fn convert_rows_four<T: Copy + Into<i64>>(
    source: &[T],
    planes: &mut [Plane],
    y: usize,
    context: &RowContext,
) {
    let RowContext {
        width,
        center,
        maxval,
        conversion,
        ..
    } = *context;
    let (first, rest) = planes.split_at_mut(1);
    let (second, rest) = rest.split_at_mut(1);
    let (third, fourth) = rest.split_at_mut(1);
    let a = first[0].row_mut(y);
    let b = second[0].row_mut(y);
    let c = third[0].row_mut(y);
    let d = fourth[0].row_mut(y);
    for x in 0..width {
        let base = x * 4;
        match conversion {
            Conversion::CmykToYcck => {
                let r = maxval - Into::<i64>::into(source[base]);
                let g = maxval - Into::<i64>::into(source[base + 1]);
                let bl = maxval - Into::<i64>::into(source[base + 2]);
                a[x] = rgb_to_y(r, g, bl) as u16;
                b[x] = rgb_to_cb(r, g, bl, center) as u16;
                c[x] = rgb_to_cr(r, g, bl, center) as u16;
                d[x] = Into::<i64>::into(source[base + 3]) as u16;
            }
            _ => {
                a[x] = Into::<i64>::into(source[base]) as u16;
                b[x] = Into::<i64>::into(source[base + 1]) as u16;
                c[x] = Into::<i64>::into(source[base + 2]) as u16;
                d[x] = Into::<i64>::into(source[base + 3]) as u16;
            }
        }
    }
}

/// The eight-bit RGB to YCbCr conversion, specialised on the channel count so
/// the source stride is a compile-time constant.
///
/// This is the single hottest loop in the encoder — on a 1920x1080 frame it
/// runs two million times — so it gets a monomorphic, bounds-check-free form
/// that LLVM can vectorise.
fn rgb8_rows<const CH: usize>(
    source: &[u8],
    planes: &mut [Plane],
    y: usize,
    order: (usize, usize, usize),
) {
    let (first, rest) = planes.split_at_mut(1);
    let (second, third) = rest.split_at_mut(1);
    let a = first[0].row_mut(y);
    let b = second[0].row_mut(y);
    let c = third[0].row_mut(y);
    let (ri, gi, bi) = order;
    for (((luma, cb), cr), pixel) in a
        .iter_mut()
        .zip(b.iter_mut())
        .zip(c.iter_mut())
        .zip(source.chunks_exact(CH))
    {
        let (ly, lcb, lcr) = rgb8_to_ycbcr(
            i32::from(pixel[ri]),
            i32::from(pixel[gi]),
            i32::from(pixel[bi]),
        );
        *luma = ly;
        *cb = lcb;
        *cr = lcr;
    }
}

/// Colour-convert the whole image into full-resolution planes, one
/// monomorphic loop per sample width.
fn convert_typed<T: Copy + Into<i64>>(pixels: &[T], planes: &mut [Plane], context: &RowContext) {
    let RowContext {
        width,
        height,
        channels,
        order,
        conversion,
        ..
    } = *context;
    for y in 0..height {
        let source = &pixels[y * width * channels..(y + 1) * width * channels];
        match conversion {
            Conversion::Luma => {
                let row = planes[0].row_mut(y);
                for (x, slot) in row.iter_mut().enumerate().take(width) {
                    *slot = Into::<i64>::into(source[x * channels]) as u16;
                }
            }
            Conversion::RgbToLuma => {
                let (ri, gi, bi) = order;
                let row = planes[0].row_mut(y);
                for (x, slot) in row.iter_mut().enumerate().take(width) {
                    let base = x * channels;
                    let r: i64 = source[base + ri].into();
                    let g: i64 = source[base + gi].into();
                    let b: i64 = source[base + bi].into();
                    *slot = rgb_to_y(r, g, b) as u16;
                }
            }
            Conversion::RgbToYcbcr | Conversion::RgbPassthrough | Conversion::Passthrough(3) => {
                convert_rows_three(source, planes, y, context);
            }
            Conversion::Passthrough(2) => convert_rows_two(source, planes, y, context),
            _ => convert_rows_four(source, planes, y, context),
        }
    }
}

/// Colour-convert the whole image into full-resolution planes of
/// `plane_width` x `plane_height`, leaving the padding untouched.
fn convert(
    plan: &EncodePlan,
    pixels: &Samples<'_>,
    plane_width: usize,
    plane_height: usize,
) -> Result<Vec<Plane>> {
    let width = usize::from(plan.width);
    let height = usize::from(plan.height);
    let channels = plan.input.channels();
    let need = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(channels))
        .ok_or(JpegError::InvalidEncodeParameter {
            parameter: "dimensions",
            reason: "width * height * channels overflows",
        })?;
    if pixels.len() < need {
        return Err(JpegError::BufferTooSmall {
            need,
            got: pixels.len(),
        });
    }

    let conversion = conversion_for(plan.input, plan.color)?;
    let mut planes: Vec<Plane> = (0..plan.components.len())
        .map(|_| Plane::new(plane_width, plane_height))
        .collect();
    let center = plan.center();
    let maxval = i64::from(plan.maxval());

    // The default path — eight-bit RGB-like input becoming YCbCr — gets a
    // specialised loop; everything else shares the general one.
    if let (Samples::Eight(data), Conversion::RgbToYcbcr) = (pixels, conversion) {
        let order = rgb_order(plan.input);
        match channels {
            3 => {
                for y in 0..height {
                    let row = &data[y * width * 3..(y + 1) * width * 3];
                    rgb8_rows::<3>(row, &mut planes, y, order);
                }
                return Ok(planes);
            }
            4 => {
                for y in 0..height {
                    let row = &data[y * width * 4..(y + 1) * width * 4];
                    rgb8_rows::<4>(row, &mut planes, y, order);
                }
                return Ok(planes);
            }
            _ => {}
        }
    }

    let context = RowContext {
        width,
        height,
        channels,
        order: rgb_order(plan.input),
        center,
        maxval,
        conversion,
    };
    match pixels {
        Samples::Eight(data) => convert_typed(data, &mut planes, &context),
        Samples::Wide(data) => convert_typed(data, &mut planes, &context),
    }
    Ok(planes)
}

/// Build the per-component sample planes a DCT frame's coefficient stage
/// reads: padded to whole MCUs and already decimated.
pub(crate) fn build_dct_planes(plan: &EncodePlan, pixels: &Samples<'_>) -> Result<Vec<Plane>> {
    let aligned_width = 8 * usize::from(plan.hmax) * plan.mcus_per_line;
    let aligned_height = 8 * usize::from(plan.vmax) * plan.mcus_per_column;
    let mut full = convert(plan, pixels, aligned_width, aligned_height)?;
    for plane in &mut full {
        plane.extend_edges(usize::from(plan.width), usize::from(plan.height));
    }

    let maxval = plan.maxval();
    let context_mode = uses_context_rows(plan);
    let smoothing = matches!(plan.downsampling, Downsampling::Smooth(_));
    Ok(plan
        .components
        .iter()
        .zip(full.iter_mut())
        .map(|(component, source)| {
            // A component sampled at the frame maximum needs no decimation at
            // all, and its padded geometry is exactly the full-resolution
            // plane's, so the plane moves through instead of being copied.
            // On a 4:4:4 frame that is three full-image copies saved.
            if !smoothing
                && component.h == plan.hmax
                && component.v == plan.vmax
                && source.width() == component.padded_width()
                && source.height() == component.padded_height()
            {
                return std::mem::replace(source, Plane::new(0, 0));
            }
            // Rows libjpeg actually decimates: one row group per
            // `Vmax` input rows, each producing `v` output rows. Everything
            // below is a copy of the last of them — except in context mode,
            // where `jcprepct.c` has no output-padding step at all and keeps
            // decimating the replicated source instead.
            let groups = usize::from(plan.height).div_ceil(usize::from(plan.vmax));
            let computed_rows = if context_mode {
                component.padded_height()
            } else {
                groups * usize::from(component.v)
            };
            downsample_component(
                source,
                component.h,
                component.v,
                plan.hmax,
                plan.vmax,
                component.padded_width(),
                component.padded_height(),
                computed_rows,
                plan.downsampling,
                maxval,
            )
        })
        .collect())
}

/// Whether libjpeg would run its downsampler in "context rows" mode.
///
/// The flag is per frame, not per component (`jcsample.c`'s
/// `jinit_downsampler` sets one `need_context_rows`), and it is set only when
/// smoothing is on **and** some component actually gets a smoothing kernel —
/// which means a full-size or an exactly-halved component. In context mode
/// `jcprepct.c` uses `pre_process_context`, which has no output-plane padding
/// step: the rows below the image are decimated from the replicated source
/// instead of copied from the last decimated row. Those two differ as soon as
/// the kernel mixes rows.
fn uses_context_rows(plan: &EncodePlan) -> bool {
    if !matches!(plan.downsampling, Downsampling::Smooth(_)) {
        return false;
    }
    plan.components.iter().any(|component| {
        let fullsize = component.h == plan.hmax && component.v == plan.vmax;
        let halved = component.h * 2 == plan.hmax && component.v * 2 == plan.vmax;
        fullsize || halved
    })
}

/// Build the planes a lossless frame reads: full resolution, no padding and
/// no decimation, because either would stop the frame being lossless.
pub(crate) fn build_lossless_planes(plan: &EncodePlan, pixels: &Samples<'_>) -> Result<Vec<Plane>> {
    convert(
        plan,
        pixels,
        usize::from(plan.width),
        usize::from(plan.height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::options::{EncodeOptions, Subsampling};
    use crate::encoder::plan::build_plan;

    fn build(
        options: &EncodeOptions,
        w: u16,
        h: u16,
        input: InputColor,
        data: &[u8],
    ) -> Vec<Plane> {
        let plan = build_plan(options, w, h, input).expect("plan");
        build_dct_planes(&plan, &Samples::Eight(data)).expect("planes")
    }

    #[test]
    fn grey_input_is_copied_and_edge_extended() {
        let options = EncodeOptions::default();
        let planes = build(&options, 3, 2, InputColor::Luma, &[10, 20, 30, 40, 50, 60]);
        assert_eq!(planes.len(), 1);
        assert_eq!(planes[0].width(), 8);
        assert_eq!(planes[0].height(), 8);
        assert_eq!(planes[0].row(0), &[10, 20, 30, 30, 30, 30, 30, 30]);
        assert_eq!(planes[0].row(1), &[40, 50, 60, 60, 60, 60, 60, 60]);
        assert_eq!(planes[0].row(7), &[40, 50, 60, 60, 60, 60, 60, 60]);
    }

    #[test]
    fn rgb_becomes_ycbcr_with_the_expected_neutral_chroma() {
        let options = EncodeOptions {
            subsampling: Subsampling::S444,
            ..Default::default()
        };
        let pixels: Vec<u8> = std::iter::repeat_n([90u8, 90, 90], 4).flatten().collect();
        let planes = build(&options, 2, 2, InputColor::Rgb, &pixels);
        assert_eq!(planes[0].row(0)[0], 90);
        assert_eq!(planes[1].row(0)[0], 128);
        assert_eq!(planes[2].row(0)[0], 128);
    }

    #[test]
    fn bgr_input_swaps_the_channels_back() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Rgb),
            ..Default::default()
        };
        let planes = build(&options, 1, 1, InputColor::Bgr, &[1, 2, 3]);
        assert_eq!(planes[0].row(0)[0], 3);
        assert_eq!(planes[1].row(0)[0], 2);
        assert_eq!(planes[2].row(0)[0], 1);
    }

    #[test]
    fn alpha_is_dropped() {
        let options = EncodeOptions::default();
        let planes = build(&options, 1, 1, InputColor::LumaAlpha, &[77, 255]);
        assert_eq!(planes[0].row(0)[0], 77);
    }

    /// The paired case: asking for `ColorSpace::Unknown(2)` instead of the
    /// default keeps the alpha channel as a second, untransformed component
    /// rather than dropping it.
    #[test]
    fn alpha_is_kept_as_a_second_component_when_asked() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            ..Default::default()
        };
        let planes = build(&options, 2, 1, InputColor::LumaAlpha, &[77, 255, 12, 34]);
        assert_eq!(planes.len(), 2);
        assert_eq!(planes[0].row(0)[..2], [77, 12], "luma column untouched");
        assert_eq!(planes[1].row(0)[..2], [255, 34], "alpha column preserved");
    }

    #[test]
    fn cmyk_to_ycck_complements_cmy_and_passes_k_through() {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Ycck),
            subsampling: Subsampling::S444,
            ..Default::default()
        };
        let planes = build(&options, 1, 1, InputColor::Cmyk, &[255, 255, 255, 40]);
        // C = M = Y = 255 complements to black, so Y = 0 and chroma neutral.
        assert_eq!(planes[0].row(0)[0], 0);
        assert_eq!(planes[1].row(0)[0], 128);
        assert_eq!(planes[2].row(0)[0], 128);
        assert_eq!(planes[3].row(0)[0], 40, "K must not be inverted");
    }

    #[test]
    fn subsampled_planes_have_the_padded_geometry() {
        let options = EncodeOptions::default();
        let pixels = vec![128u8; 17 * 19 * 3];
        let planes = build(&options, 17, 19, InputColor::Rgb, &pixels);
        assert_eq!((planes[0].width(), planes[0].height()), (32, 32));
        assert_eq!((planes[1].width(), planes[1].height()), (16, 16));
    }

    #[test]
    fn a_short_buffer_is_rejected() {
        let plan = build_plan(&EncodeOptions::default(), 4, 4, InputColor::Rgb).expect("plan");
        let short = vec![0u8; 4 * 4 * 3 - 1];
        assert!(matches!(
            build_dct_planes(&plan, &Samples::Eight(&short)),
            Err(JpegError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn twelve_bit_input_keeps_its_range() {
        let options = EncodeOptions {
            precision: 12,
            ..Default::default()
        };
        let plan = build_plan(&options, 2, 2, InputColor::Luma).expect("plan");
        let pixels = [4095u16, 0, 2048, 1];
        let planes = build_dct_planes(&plan, &Samples::Wide(&pixels)).expect("planes");
        assert_eq!(planes[0].row(0)[0], 4095);
        assert_eq!(planes[0].row(1)[1], 1);
    }

    #[test]
    fn lossless_planes_are_unpadded() {
        let options = EncodeOptions {
            process: crate::encoder::options::EncodeProcess::Lossless {
                predictor: 1,
                point_transform: 0,
            },
            ..Default::default()
        };
        let plan = build_plan(&options, 3, 2, InputColor::Luma).expect("plan");
        let planes =
            build_lossless_planes(&plan, &Samples::Eight(&[1, 2, 3, 4, 5, 6])).expect("planes");
        assert_eq!((planes[0].width(), planes[0].height()), (3, 2));
        assert_eq!(planes[0].row(1), &[4, 5, 6]);
    }
}

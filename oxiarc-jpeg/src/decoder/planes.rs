//! Component sample planes.
//!
//! All components live in one allocation with per-component offsets, so a
//! decode performs exactly one plane allocation regardless of component count.
//! Each plane is padded out to whole MCUs, which lets the MCU loop write
//! 8x8 blocks without a bounds test on the right and bottom edges; the
//! upsampler reads only the unpadded extent, so the padding never reaches the
//! output.

use super::Scale;
use crate::error::{JpegError, LimitKind, Result};
use crate::frame::{Component, FrameHeader};
use crate::limits::{DecodeLimits, checked_product3};

/// One decode's component planes.
#[derive(Debug, Default)]
pub(crate) struct Planes {
    data: Vec<u16>,
    offsets: Vec<usize>,
    strides: Vec<usize>,
    padded_heights: Vec<usize>,
    widths: Vec<usize>,
    heights: Vec<usize>,
    /// Samples per axis the scaled IDCT writes for one `8x8` coefficient
    /// block of this component — libjpeg's `_DCT_scaled_size`. `8` for every
    /// component at [`Scale::FULL`] and always for a lossless frame; a
    /// subsampled component's own value can exceed the frame's requested
    /// [`Scale::numerator`] (see [`component_output_size`]).
    output_sizes: Vec<u8>,
}

impl Planes {
    /// Allocate the planes a frame needs, checking the total against
    /// [`DecodeLimits::max_output_bytes`] first.
    ///
    /// `scale` is ignored for a lossless frame — see [`super::DecodeOptions::scale`].
    pub(crate) fn allocate(
        frame: &FrameHeader,
        scale: Scale,
        limits: &DecodeLimits,
    ) -> Result<Self> {
        let count = frame.components.len();
        let mut offsets = Vec::with_capacity(count);
        let mut strides = Vec::with_capacity(count);
        let mut padded_heights = Vec::with_capacity(count);
        let mut widths = Vec::with_capacity(count);
        let mut heights = Vec::with_capacity(count);
        let mut output_sizes = Vec::with_capacity(count);
        let mut samples: u64 = 0;

        // T.81 lossless has no DCT to scale, and libjpeg hardwires "no
        // scaling" for it (`jdmaster.c`: "Hardwire it to 'no scaling'");
        // every component simply renders at its native block size.
        let numerator = if frame.is_lossless() {
            8
        } else {
            scale.numerator()
        };

        for component in &frame.components {
            let output_size = if frame.is_lossless() {
                8
            } else {
                component_output_size(component, frame, numerator)
            };
            let (width, height) = if frame.is_lossless() {
                (component.width_samples, component.height_samples)
            } else {
                (
                    component_downsampled_dim(frame.width, component.h, output_size, frame.hmax),
                    component_downsampled_dim(frame.height, component.v, output_size, frame.vmax),
                )
            };
            let (padded_width, padded_height) = padded_extent(frame, component, output_size);
            let plane_samples = checked_product3(
                u64::from(padded_width),
                u64::from(padded_height),
                1,
                LimitKind::OutputBytes,
            )?;
            offsets.push(
                usize::try_from(samples)
                    .map_err(|_| JpegError::LimitExceeded(LimitKind::OutputBytes))?,
            );
            samples = samples
                .checked_add(plane_samples)
                .ok_or(JpegError::LimitExceeded(LimitKind::OutputBytes))?;
            limits.check_output_bytes(samples.saturating_mul(2))?;

            strides.push(padded_width as usize);
            padded_heights.push(padded_height as usize);
            widths.push(width as usize);
            heights.push(height as usize);
            output_sizes.push(output_size);
        }

        let samples = usize::try_from(samples)
            .map_err(|_| JpegError::LimitExceeded(LimitKind::OutputBytes))?;
        Ok(Self {
            data: vec![0u16; samples],
            offsets,
            strides,
            padded_heights,
            widths,
            heights,
            output_sizes,
        })
    }

    /// Raw storage for every plane.
    pub(crate) fn data_mut(&mut self) -> &mut [u16] {
        &mut self.data
    }

    /// Byte offset of component `index`'s first sample.
    pub(crate) fn offset(&self, index: usize) -> usize {
        self.offsets[index]
    }

    /// Row stride of component `index`, in samples.
    pub(crate) fn stride(&self, index: usize) -> usize {
        self.strides[index]
    }

    /// Unpadded sample width of component `index`.
    pub(crate) fn width(&self, index: usize) -> usize {
        self.widths[index]
    }

    /// Unpadded sample height of component `index`.
    pub(crate) fn height(&self, index: usize) -> usize {
        self.heights[index]
    }

    /// Component `index`'s plane.
    pub(crate) fn plane(&self, index: usize) -> &[u16] {
        let start = self.offsets[index];
        let end = start + self.strides[index] * self.padded_height(index);
        &self.data[start..end]
    }

    /// Copy a band of decoded rows in from a smaller, band-shaped set of
    /// planes.
    ///
    /// `first_row` is the destination row of each component's band, which for
    /// a band of whole MCU rows is `mcu_row * v * output_size` — the
    /// component's own [`Planes::output_size`], **not** a hardwired `8`, or a
    /// reduced-scale decode would scatter every band but the first across the
    /// wrong plane rows. Rows past the
    /// destination's padded extent are dropped: the last band of a frame
    /// whose height is not a whole number of MCUs decodes padding rows that
    /// the full-frame planes do not carry.
    #[cfg(feature = "rayon")]
    pub(crate) fn copy_band_from(&mut self, band: &Planes, first_rows: &[usize]) {
        for (index, &first_row) in first_rows.iter().enumerate() {
            if index >= self.strides.len() || index >= band.strides.len() {
                break;
            }
            let stride = self.strides[index].min(band.strides[index]);
            let available = self.padded_heights[index].saturating_sub(first_row);
            let rows = band.padded_heights[index].min(available);
            for row in 0..rows {
                let source = band.offsets[index] + row * band.strides[index];
                let target = self.offsets[index] + (first_row + row) * self.strides[index];
                self.data[target..target + stride]
                    .copy_from_slice(&band.data[source..source + stride]);
            }
        }
    }

    /// Padded row count of component `index`.
    pub(crate) fn padded_height(&self, index: usize) -> usize {
        self.padded_heights[index]
    }

    /// Samples per axis the scaled IDCT wrote for one coefficient block of
    /// component `index` — `8` unless [`super::DecodeOptions::scale`]
    /// requested otherwise.
    pub(crate) fn output_size(&self, index: usize) -> u8 {
        self.output_sizes[index]
    }

    /// Every component's [`Planes::output_size`], in component order.
    ///
    /// The parallel band merge needs the whole array before it starts, to
    /// place each band's rows at `mcu_row * Vi * output_size(i)` in the
    /// destination plane rather than at the unscaled `mcu_row * Vi * 8`.
    #[cfg(feature = "rayon")]
    pub(crate) fn output_sizes(&self) -> &[u8] {
        &self.output_sizes
    }

    /// The smallest [`Planes::output_size`] over every component — libjpeg's
    /// `_min_DCT_scaled_size`, always equal to the requested
    /// [`Scale::numerator`] (a subsampled component's size only ever grows
    /// past it, never shrinks below it — see [`component_output_size`]).
    /// `8` when there are no components, matching [`Scale::FULL`].
    pub(crate) fn min_output_size(&self) -> u8 {
        self.output_sizes.iter().copied().min().unwrap_or(8)
    }

    /// `true` when no plane has been allocated.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
}

/// Padded plane extent for one component, in samples.
///
/// DCT frames pad to whole MCUs of `output_size x output_size` blocks
/// (`output_size` is `8` unless [`super::DecodeOptions::scale`] requested
/// otherwise); lossless frames have no blocks, so they pad to whole MCUs of
/// `Hi` x `Vi` samples and `output_size` is ignored (pass `8`, the caller's
/// unconditional value for a lossless frame).
pub(crate) fn padded_extent(
    frame: &FrameHeader,
    component: &Component,
    output_size: u8,
) -> (u32, u32) {
    if frame.is_lossless() {
        (
            frame.mcus_per_line * u32::from(component.h),
            frame.mcus_per_column * u32::from(component.v),
        )
    } else {
        (
            component.blocks_per_line_padded * u32::from(output_size),
            component.blocks_per_column_padded * u32::from(output_size),
        )
    }
}

/// libjpeg's per-component IDCT-scaled-size selection
/// (`jdmaster.c::jpeg_calc_output_dimensions`'s first loop, transcribed
/// exactly): try to grow a subsampled component's own output block size so
/// that, after scaling, its pixel width and height match the frame's full
/// resolution — which lets the upsampler treat it as already full size
/// instead of resampling. Doubling only continues while it keeps *both* axes
/// exactly divisible, so an odd sampling ratio (or one that only matches on
/// one axis) simply keeps `numerator`, which is what the loop's C original
/// does too (a component's block is `N x N`; the two axes cannot be grown
/// independently).
///
/// A no-op whenever `numerator >= 8` (`_min_DCT_scaled_size` is already the
/// native block size, so nothing can grow into it) and for the frame's own
/// most-sampled component (its ratio to `hmax`/`vmax` is `1`, which never
/// clears the divisibility check either) — both exactly matching libjpeg.
fn component_output_size(component: &Component, frame: &FrameHeader, numerator: u8) -> u8 {
    let hmax = u32::from(frame.hmax);
    let vmax = u32::from(frame.vmax);
    let h = u32::from(component.h).max(1);
    let v = u32::from(component.v).max(1);
    let n = u32::from(numerator);
    let mut size = n;
    while size < 8 && (hmax * n) % (h * size * 2) == 0 && (vmax * n) % (v * size * 2) == 0 {
        size *= 2;
    }
    // Unreachable past 14 (the largest value any `numerator in 1..=7` can
    // double to before the `size < 8` guard stops it), far under `u8::MAX`.
    size as u8
}

/// libjpeg's `downsampled_width` / `downsampled_height`
/// (`jdmaster.c::jpeg_calc_output_dimensions`'s second loop): a component's
/// true (unpadded) sample count on one axis once its own
/// [`component_output_size`] is applied — `ceil(dim * h_or_v * output_size /
/// (hmax_or_vmax * 8))`.
fn component_downsampled_dim(dim: u16, h_or_v: u8, output_size: u8, hmax_or_vmax: u8) -> u32 {
    let numerator = u64::from(dim) * u64::from(h_or_v) * u64::from(output_size);
    let denominator = u64::from(hmax_or_vmax.max(1)) * 8;
    numerator.div_ceil(denominator) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::parse_sof;

    fn frame(marker: u8, y: u16, x: u16, comps: &[(u8, u8, u8, u8)]) -> FrameHeader {
        let mut payload = vec![8u8];
        payload.extend_from_slice(&y.to_be_bytes());
        payload.extend_from_slice(&x.to_be_bytes());
        payload.push(comps.len() as u8);
        for &(id, h, v, tq) in comps {
            payload.push(id);
            payload.push((h << 4) | v);
            payload.push(tq);
        }
        parse_sof(marker, &payload, 0, &DecodeLimits::default()).expect("SOF")
    }

    #[test]
    fn allocates_mcu_padded_planes() {
        let frame = frame(0xC0, 97, 131, &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let planes =
            Planes::allocate(&frame, Scale::FULL, &DecodeLimits::default()).expect("allocate");
        // Luma: 9 MCUs across x 2 blocks x 8 = 144 wide, 7 x 2 x 8 = 112 tall.
        assert_eq!(planes.stride(0), 144);
        assert_eq!(planes.width(0), 131);
        assert_eq!(planes.height(0), 97);
        assert_eq!(planes.plane(0).len(), 144 * 112);
        // Chroma: 9 x 8 = 72 wide, 7 x 8 = 56 tall.
        assert_eq!(planes.stride(1), 72);
        assert_eq!(planes.plane(1).len(), 72 * 56);
        assert_eq!(planes.offset(0), 0);
        assert_eq!(planes.offset(1), 144 * 112);
        assert_eq!(planes.offset(2), 144 * 112 + 72 * 56);
    }

    #[test]
    fn lossless_planes_are_not_block_padded() {
        let frame = frame(0xC3, 10, 10, &[(1, 1, 1, 0)]);
        let planes =
            Planes::allocate(&frame, Scale::FULL, &DecodeLimits::default()).expect("allocate");
        assert_eq!(planes.stride(0), 10);
        assert_eq!(planes.plane(0).len(), 100);
    }

    #[test]
    fn lossless_interleaved_planes_pad_to_the_sampling_grid() {
        let frame = frame(0xC3, 9, 9, &[(1, 2, 2, 0), (2, 1, 1, 0)]);
        // Hmax = Vmax = 2 so there are 5 MCUs across and down.
        assert_eq!(frame.mcus_per_line, 5);
        let planes =
            Planes::allocate(&frame, Scale::FULL, &DecodeLimits::default()).expect("allocate");
        assert_eq!(planes.stride(0), 10);
        assert_eq!(planes.stride(1), 5);
        assert_eq!(planes.width(0), 9);
        assert_eq!(planes.width(1), 5);
    }

    #[test]
    fn output_budget_is_enforced_before_allocating() {
        let frame = frame(
            0xC0,
            8000,
            8000,
            &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0)],
        );
        let limits = DecodeLimits {
            max_output_bytes: 1 << 20,
            ..DecodeLimits::default()
        };
        assert!(matches!(
            Planes::allocate(&frame, Scale::FULL, &limits),
            Err(JpegError::LimitExceeded(LimitKind::OutputBytes))
        ));
    }

    #[test]
    fn zero_height_frame_allocates_nothing() {
        let frame = frame(0xC0, 0, 16, &[(1, 1, 1, 0)]);
        let planes =
            Planes::allocate(&frame, Scale::FULL, &DecodeLimits::default()).expect("allocate");
        assert_eq!(planes.plane(0).len(), 0);
        assert!(!planes.is_empty());
    }
}

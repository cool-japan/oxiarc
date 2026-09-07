//! Component sample planes.
//!
//! All components live in one allocation with per-component offsets, so a
//! decode performs exactly one plane allocation regardless of component count.
//! Each plane is padded out to whole MCUs, which lets the MCU loop write
//! 8x8 blocks without a bounds test on the right and bottom edges; the
//! upsampler reads only the unpadded extent, so the padding never reaches the
//! output.

use crate::error::{JpegError, LimitKind, Result};
use crate::frame::FrameHeader;
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
}

impl Planes {
    /// Allocate the planes a frame needs, checking the total against
    /// [`DecodeLimits::max_output_bytes`] first.
    pub(crate) fn allocate(frame: &FrameHeader, limits: &DecodeLimits) -> Result<Self> {
        let count = frame.components.len();
        let mut offsets = Vec::with_capacity(count);
        let mut strides = Vec::with_capacity(count);
        let mut padded_heights = Vec::with_capacity(count);
        let mut widths = Vec::with_capacity(count);
        let mut heights = Vec::with_capacity(count);
        let mut samples: u64 = 0;

        for component in &frame.components {
            let (padded_width, padded_height) = padded_extent(frame, component);
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
            widths.push(component.width_samples as usize);
            heights.push(component.height_samples as usize);
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

    /// Padded row count of component `index`.
    pub(crate) fn padded_height(&self, index: usize) -> usize {
        self.padded_heights[index]
    }

    /// `true` when no plane has been allocated.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
}

/// Padded plane extent for one component, in samples.
///
/// DCT frames pad to whole MCUs of 8x8 blocks; lossless frames have no
/// blocks, so they pad to whole MCUs of `Hi` x `Vi` samples.
pub(crate) fn padded_extent(
    frame: &FrameHeader,
    component: &crate::frame::Component,
) -> (u32, u32) {
    if frame.is_lossless() {
        (
            frame.mcus_per_line * u32::from(component.h),
            frame.mcus_per_column * u32::from(component.v),
        )
    } else {
        (
            component.blocks_per_line_padded * 8,
            component.blocks_per_column_padded * 8,
        )
    }
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
        let planes = Planes::allocate(&frame, &DecodeLimits::default()).expect("allocate");
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
        let planes = Planes::allocate(&frame, &DecodeLimits::default()).expect("allocate");
        assert_eq!(planes.stride(0), 10);
        assert_eq!(planes.plane(0).len(), 100);
    }

    #[test]
    fn lossless_interleaved_planes_pad_to_the_sampling_grid() {
        let frame = frame(0xC3, 9, 9, &[(1, 2, 2, 0), (2, 1, 1, 0)]);
        // Hmax = Vmax = 2 so there are 5 MCUs across and down.
        assert_eq!(frame.mcus_per_line, 5);
        let planes = Planes::allocate(&frame, &DecodeLimits::default()).expect("allocate");
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
            Planes::allocate(&frame, &limits),
            Err(JpegError::LimitExceeded(LimitKind::OutputBytes))
        ));
    }

    #[test]
    fn zero_height_frame_allocates_nothing() {
        let frame = frame(0xC0, 0, 16, &[(1, 1, 1, 0)]);
        let planes = Planes::allocate(&frame, &DecodeLimits::default()).expect("allocate");
        assert_eq!(planes.plane(0).len(), 0);
        assert!(!planes.is_empty());
    }
}

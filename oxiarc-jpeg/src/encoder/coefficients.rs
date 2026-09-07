//! Sample planes to quantised coefficients, dummy blocks included.
//!
//! # Why the dummy blocks are materialised
//!
//! A component's real block grid is `ceil(width_samples / 8)` wide, but the
//! MCU grid may be wider. libjpeg does **not** transform replicated pixels
//! into those extra blocks (`jccoefct.c`'s `compress_first_pass`): it writes
//! an all-zero block whose DC is copied from the block before it in MCU
//! order. The DC difference is then zero and the block costs a DC symbol plus
//! an `EOB`.
//!
//! Storing zero there instead is invisible in a sequential frame — the
//! difference is still coded — but wrong in a progressive one, where a DC
//! refinement scan emits `(coefficient >> Al) & 1` for every block including
//! the dummies. That is why the copied DC is written into the array rather
//! than special-cased in the entropy coder.
//!
//! Edge *pixels* inside a real block are a separate mechanism: those come
//! from [`crate::downsample::Plane::extend_edges`], which replicates the last
//! real row and column.

use super::plan::{ComponentPlan, EncodePlan};
use crate::downsample::Plane;
use crate::fdct::{fdct_islow, load_block, pass1_bits, quantise_block};
use crate::quant::QuantTable;

/// One component's quantised coefficients, block-row major.
///
/// Block `(bx, by)` starts at `(by * blocks_wide_padded + bx) * 64` and is in
/// natural (row-major) coefficient order — the entropy coders apply the
/// zig-zag as they read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoefficientPlane {
    pub data: Vec<i16>,
    pub blocks_wide: usize,
    pub blocks_high: usize,
}

impl CoefficientPlane {
    /// One block, as a fixed-size array so the entropy coders can scan it
    /// without a bounds check per coefficient.
    #[inline]
    pub(crate) fn block(&self, bx: usize, by: usize) -> &[i16; 64] {
        const EMPTY: [i16; 64] = [0; 64];
        let start = (by * self.blocks_wide + bx) * 64;
        self.data
            .get(start..)
            .and_then(<[i16]>::first_chunk::<64>)
            .unwrap_or(&EMPTY)
    }

    #[inline]
    fn dc(&self, bx: usize, by: usize) -> i16 {
        self.data[(by * self.blocks_wide + bx) * 64]
    }

    #[inline]
    fn set_dc(&mut self, bx: usize, by: usize, value: i16) {
        self.data[(by * self.blocks_wide + bx) * 64] = value;
    }
}

/// Transform and quantise one component.
fn transform_component(
    component: &ComponentPlan,
    plane: &Plane,
    quant: &QuantTable,
    precision: u8,
    center: i32,
) -> CoefficientPlane {
    let blocks_wide = component.blocks_wide_padded;
    let blocks_high = component.blocks_high_padded;
    let mut out = CoefficientPlane {
        data: vec![0i16; blocks_wide * blocks_high * 64],
        blocks_wide,
        blocks_high,
    };
    let bits = pass1_bits(precision);
    let stride = plane.width();
    let table = quant.zigzag();
    let mut block = [0i32; 64];
    let mut quantised = [0i16; 64];

    for by in 0..component.blocks_high.min(blocks_high) {
        for bx in 0..component.blocks_wide.min(blocks_wide) {
            load_block(plane.samples(), stride, bx * 8, by * 8, center, &mut block);
            fdct_islow(&mut block, bits);
            quantise_block(&block, &table, &mut quantised);
            let start = (by * blocks_wide + bx) * 64;
            out.data[start..start + 64].copy_from_slice(&quantised);
        }
    }

    fill_dummy_blocks(&mut out, component);
    out
}

/// Write the copied DC values into the MCU-alignment dummy blocks.
fn fill_dummy_blocks(out: &mut CoefficientPlane, component: &ComponentPlan) {
    let real_wide = component.blocks_wide.min(out.blocks_wide);
    let real_high = component.blocks_high.min(out.blocks_high);
    if real_wide == 0 || real_high == 0 {
        return;
    }

    // Right margin: each dummy takes the DC of the block to its left.
    for by in 0..real_high {
        for bx in real_wide..out.blocks_wide {
            let previous = out.dc(bx - 1, by);
            out.set_dc(bx, by, previous);
        }
    }

    // Bottom margin: within each MCU, every block of the dummy row takes the
    // DC of that MCU's last block in the row above.
    let h = usize::from(component.h);
    let mcus_across = out.blocks_wide / h.max(1);
    for by in real_high..out.blocks_high {
        for mcu in 0..mcus_across {
            let last = out.dc(mcu * h + h - 1, by - 1);
            for bi in 0..h {
                out.set_dc(mcu * h + bi, by, last);
            }
        }
    }
}

/// Transform and quantise every component of a DCT frame.
pub(crate) fn build_coefficients(plan: &EncodePlan, planes: &[Plane]) -> Vec<CoefficientPlane> {
    let center = plan.center() as i32;
    plan.components
        .iter()
        .zip(planes.iter())
        .map(|(component, plane)| {
            let quant = plan.quant[usize::from(component.quant_slot)].unwrap_or_default();
            transform_component(component, plane, &quant, plan.precision, center)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::options::{EncodeOptions, InputColor, Subsampling};
    use crate::encoder::plan::build_plan;
    use crate::encoder::prepare::{Samples, build_dct_planes};

    fn coefficients(
        options: &EncodeOptions,
        w: u16,
        h: u16,
        input: InputColor,
        pixels: &[u8],
    ) -> Vec<CoefficientPlane> {
        let plan = build_plan(options, w, h, input).expect("plan");
        let planes = build_dct_planes(&plan, &Samples::Eight(pixels)).expect("planes");
        build_coefficients(&plan, &planes)
    }

    /// A luma-only source, replicated into an RGB buffer so the frame is a
    /// real 4:2:0 one whose luma plane carries the pattern.
    fn grey_as_rgb(pixels: &[u8]) -> Vec<u8> {
        pixels.iter().flat_map(|&v| [v, v, v]).collect()
    }

    #[test]
    fn a_flat_image_has_only_dc() {
        let options = EncodeOptions {
            quality: 100,
            ..Default::default()
        };
        let coefficients = coefficients(&options, 8, 8, InputColor::Luma, &[200u8; 64]);
        let block = coefficients[0].block(0, 0);
        assert_eq!(block[0], (200 - 128) * 8, "DC = 8 * (sample - centre)");
        assert!(block[1..].iter().all(|&c| c == 0));
    }

    /// 17x19 luma at 4:2:0 has three real block columns and four encoded; the
    /// fourth is a dummy whose DC repeats the third's, so the coded DC
    /// difference is zero.
    #[test]
    fn right_margin_dummies_repeat_the_previous_dc() {
        let options = EncodeOptions::default();
        let pixels: Vec<u8> = (0..17 * 19).map(|i| ((i * 7) % 256) as u8).collect();
        let coefficients = coefficients(&options, 17, 19, InputColor::Rgb, &grey_as_rgb(&pixels));
        let plane = &coefficients[0];
        assert_eq!((plane.blocks_wide, plane.blocks_high), (4, 4));
        for by in 0..3 {
            assert_eq!(plane.dc(3, by), plane.dc(2, by), "row {by}");
            assert!(
                plane.block(3, by)[1..].iter().all(|&c| c == 0),
                "dummy AC must be zero"
            );
        }
    }

    #[test]
    fn bottom_margin_dummies_repeat_the_row_above() {
        let options = EncodeOptions::default();
        let pixels: Vec<u8> = (0..17 * 19).map(|i| ((i * 7) % 256) as u8).collect();
        let coefficients = coefficients(&options, 17, 19, InputColor::Rgb, &grey_as_rgb(&pixels));
        let plane = &coefficients[0];
        // Luma has h = 2, so the dummy row copies the DC of each MCU's second
        // block in the row above.
        for mcu in 0..2 {
            let expected = plane.dc(mcu * 2 + 1, 2);
            assert_eq!(plane.dc(mcu * 2, 3), expected);
            assert_eq!(plane.dc(mcu * 2 + 1, 3), expected);
        }
        assert!(plane.block(0, 3)[1..].iter().all(|&c| c == 0));
    }

    #[test]
    fn edge_pixels_inside_a_real_block_are_replicated_not_zeroed() {
        // A 1x1 white image: the whole 8x8 block is white, so the DC is the
        // white level and every AC is zero. Zero padding would leave a huge
        // AC signature.
        let options = EncodeOptions {
            quality: 100,
            subsampling: Subsampling::S444,
            ..Default::default()
        };
        let coefficients = coefficients(&options, 1, 1, InputColor::Luma, &[255u8]);
        let block = coefficients[0].block(0, 0);
        assert_eq!(block[0], (255 - 128) * 8);
        assert!(block[1..].iter().all(|&c| c == 0));
    }

    #[test]
    fn every_component_is_transformed() {
        let options = EncodeOptions::default();
        let plan = build_plan(&options, 16, 16, InputColor::Rgb).expect("plan");
        let pixels = vec![64u8; 16 * 16 * 3];
        let planes = build_dct_planes(&plan, &Samples::Eight(&pixels)).expect("planes");
        let coefficients = build_coefficients(&plan, &planes);
        assert_eq!(coefficients.len(), 3);
        assert_eq!(coefficients[0].blocks_wide, 2);
        assert_eq!(coefficients[1].blocks_wide, 1);
    }
}

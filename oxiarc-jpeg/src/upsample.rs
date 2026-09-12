//! Chroma upsampling, reproducing libjpeg's kernels exactly.
//!
//! libjpeg installs a *fancy* (triangle-filter) upsampler for exactly three
//! ratios — `h2v1`, `h1v2` and `h2v2` — and falls back to sample replication
//! for every other ratio. That choice was re-measured against
//! `libjpeg-turbo 3.1.4.1` while this module was written (encode a chroma
//! pattern at each ratio, then diff `djpeg -dct int` against
//! `djpeg -dct int -nosmooth`): 2x1, 1x2 and 2x2 differ, 4x1, 1x4, 2x4 and
//! 4x2 do not. Matching that table is required for byte parity with `djpeg`.
//!
//! The rounding constants are libjpeg's and are deliberately asymmetric: the
//! even output of `h2v1` rounds with `+1` against the *left* neighbour and the
//! odd output with `+2` against the *right* one.

/// Which kernel a component's ratio selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kernel {
    /// The component is already at full resolution.
    Identity,
    /// Sample replication (libjpeg `int_upsample`), generalised to
    /// non-integer ratios as nearest-neighbour.
    Nearest,
    /// libjpeg `h2v1_fancy_upsample`.
    FancyH2V1,
    /// libjpeg `h1v2_fancy_upsample`.
    FancyH1V2,
    /// libjpeg `h2v2_fancy_upsample`.
    FancyH2V2,
}

/// Everything one component's upsampler needs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Upsampler {
    kernel: Kernel,
    /// Samples per row the component actually carries.
    comp_width: usize,
    /// Sample rows the component actually carries.
    comp_height: usize,
    /// Row stride of the component plane.
    stride: usize,
    /// Horizontal sampling factor of this component.
    h: u32,
    /// Vertical sampling factor of this component.
    v: u32,
    /// Frame-wide maxima.
    hmax: u32,
    vmax: u32,
}

impl Upsampler {
    /// Choose the kernel libjpeg would choose for this component.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        comp_width: usize,
        comp_height: usize,
        stride: usize,
        h: u8,
        v: u8,
        hmax: u8,
        vmax: u8,
        fancy: bool,
    ) -> Self {
        let (h, v, hmax, vmax) = (u32::from(h), u32::from(v), u32::from(hmax), u32::from(vmax));
        // libjpeg installs the horizontal fancy kernels only when the
        // component has more than two samples per row (`jdsample.c`:
        // `do_fancy && compptr->downsampled_width > 2`), because they read a
        // neighbour on both sides. The vertical-only kernel has no such guard.
        let wide_enough = comp_width > 2;
        let kernel = if h == hmax && v == vmax {
            Kernel::Identity
        } else if !fancy {
            Kernel::Nearest
        } else if h * 2 == hmax && v == vmax && wide_enough {
            Kernel::FancyH2V1
        } else if h == hmax && v * 2 == vmax {
            Kernel::FancyH1V2
        } else if h * 2 == hmax && v * 2 == vmax && wide_enough {
            Kernel::FancyH2V2
        } else {
            Kernel::Nearest
        };
        Self {
            kernel,
            comp_width,
            comp_height,
            stride,
            h,
            v,
            hmax,
            vmax,
        }
    }

    /// The kernel in use, for tests and diagnostics.
    #[cfg(test)]
    pub(crate) fn kernel(&self) -> Kernel {
        self.kernel
    }

    /// Source row index for output row `y` under plain scaling.
    #[inline]
    fn source_row(&self, y: usize) -> usize {
        let idx = (y as u64 * u64::from(self.v) / u64::from(self.vmax)) as usize;
        idx.min(self.comp_height.saturating_sub(1))
    }

    /// Write one full-resolution output row of `out.len()` samples.
    pub(crate) fn row(&self, plane: &[u16], y: usize, out: &mut [u16]) {
        if self.comp_width == 0 || self.comp_height == 0 {
            out.fill(0);
            return;
        }
        match self.kernel {
            Kernel::Identity => {
                let base = self.source_row(y) * self.stride;
                let src = &plane[base..base + self.comp_width];
                let n = out.len().min(self.comp_width);
                out[..n].copy_from_slice(&src[..n]);
                if n < out.len() {
                    let last = src[self.comp_width - 1];
                    out[n..].fill(last);
                }
            }
            Kernel::Nearest => {
                let base = self.source_row(y) * self.stride;
                let src = &plane[base..base + self.comp_width];
                for (x, slot) in out.iter_mut().enumerate() {
                    let sx = (x as u64 * u64::from(self.h) / u64::from(self.hmax)) as usize;
                    *slot = src[sx.min(self.comp_width - 1)];
                }
            }
            Kernel::FancyH2V1 => {
                let base = self.source_row(y) * self.stride;
                let src = &plane[base..base + self.comp_width];
                h2v1_fancy(src, out);
            }
            Kernel::FancyH1V2 => {
                let (near, far) = self.vertical_rows(y);
                let near = &plane[near..near + self.comp_width];
                let far = &plane[far..far + self.comp_width];
                // libjpeg rounds the two halves of the pair differently: the
                // upper output row (`v == 0`, whose far tap is the row above)
                // adds 1, the lower one adds 2. Using 2 for both is a
                // one-LSB error on roughly one row in six.
                let round = if y % 2 == 0 { 1 } else { 2 };
                for (x, slot) in out.iter_mut().enumerate() {
                    let x = x.min(self.comp_width - 1);
                    let sum = 3 * u32::from(near[x]) + u32::from(far[x]);
                    *slot = ((sum + round) >> 2) as u16;
                }
            }
            Kernel::FancyH2V2 => {
                let (near, far) = self.vertical_rows(y);
                let near = &plane[near..near + self.comp_width];
                let far = &plane[far..far + self.comp_width];
                h2v2_fancy(near, far, out);
            }
        }
    }

    /// Offsets of the nearest and next-nearest source rows for output row `y`
    /// in a 2x vertical expansion.
    #[inline]
    fn vertical_rows(&self, y: usize) -> (usize, usize) {
        let inrow = (y / 2).min(self.comp_height - 1);
        let far = if y % 2 == 0 {
            inrow.saturating_sub(1)
        } else {
            (inrow + 1).min(self.comp_height - 1)
        };
        (inrow * self.stride, far * self.stride)
    }
}

/// libjpeg `h2v1_fancy_upsample` for one row.
fn h2v1_fancy(src: &[u16], out: &mut [u16]) {
    let n = src.len();
    if n == 1 {
        out.fill(src[0]);
        return;
    }
    let scratch = |index: usize| -> u16 {
        // Output index `index` -> libjpeg's alternating taps.
        let i = index / 2;
        if index == 0 {
            src[0]
        } else if index == 2 * n - 1 {
            src[n - 1]
        } else if index % 2 == 0 {
            let value = 3 * u32::from(src[i]) + u32::from(src[i - 1]) + 1;
            (value >> 2) as u16
        } else {
            let value = 3 * u32::from(src[i]) + u32::from(src[i + 1]) + 2;
            (value >> 2) as u16
        }
    };
    let limit = 2 * n;
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = scratch(index.min(limit - 1));
    }
}

/// libjpeg `h2v2_fancy_upsample` for one output row.
///
/// `near` and `far` are the two source rows; the column sums
/// `3 * near[i] + far[i]` are then filtered horizontally with the 16-scale
/// version of the `h2v1` kernel.
fn h2v2_fancy(near: &[u16], far: &[u16], out: &mut [u16]) {
    let n = near.len();
    let colsum = |i: usize| -> u32 { 3 * u32::from(near[i]) + u32::from(far[i]) };
    if n == 1 {
        let value = ((colsum(0) * 4 + 8) >> 4) as u16;
        out.fill(value);
        return;
    }
    let limit = 2 * n;
    for (index, slot) in out.iter_mut().enumerate() {
        let index = index.min(limit - 1);
        let i = index / 2;
        *slot = if index == 0 {
            ((colsum(0) * 4 + 8) >> 4) as u16
        } else if index == limit - 1 {
            ((colsum(n - 1) * 4 + 7) >> 4) as u16
        } else if index % 2 == 0 {
            ((colsum(i) * 3 + colsum(i - 1) + 8) >> 4) as u16
        } else {
            ((colsum(i) * 3 + colsum(i + 1) + 7) >> 4) as u16
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsampler(kernel_h: u8, kernel_v: u8, hmax: u8, vmax: u8, w: usize, h: usize) -> Upsampler {
        Upsampler::new(w, h, w, kernel_h, kernel_v, hmax, vmax, true)
    }

    #[test]
    fn narrow_components_fall_back_to_replication() {
        // `downsampled_width <= 2` disables the horizontal fancy kernels but
        // not the vertical-only one, exactly as libjpeg does.
        assert_eq!(upsampler(1, 1, 2, 1, 2, 4).kernel(), Kernel::Nearest);
        assert_eq!(upsampler(1, 1, 2, 2, 2, 4).kernel(), Kernel::Nearest);
        assert_eq!(upsampler(1, 1, 1, 2, 1, 4).kernel(), Kernel::FancyH1V2);
        assert_eq!(upsampler(1, 1, 2, 1, 3, 4).kernel(), Kernel::FancyH2V1);
    }

    #[test]
    fn kernel_selection_matches_libjpeg() {
        assert_eq!(upsampler(2, 2, 2, 2, 4, 4).kernel(), Kernel::Identity);
        assert_eq!(upsampler(1, 1, 2, 1, 4, 4).kernel(), Kernel::FancyH2V1);
        assert_eq!(upsampler(1, 1, 1, 2, 4, 4).kernel(), Kernel::FancyH1V2);
        assert_eq!(upsampler(1, 1, 2, 2, 4, 4).kernel(), Kernel::FancyH2V2);
        // 4:1:1 and friends fall back to replication, exactly as libjpeg does.
        assert_eq!(upsampler(1, 1, 4, 1, 4, 4).kernel(), Kernel::Nearest);
        assert_eq!(upsampler(1, 1, 1, 4, 4, 4).kernel(), Kernel::Nearest);
        assert_eq!(upsampler(1, 1, 2, 4, 4, 4).kernel(), Kernel::Nearest);
        assert_eq!(upsampler(1, 1, 4, 2, 4, 4).kernel(), Kernel::Nearest);
        // Non-integer ratios also fall back rather than erroring.
        assert_eq!(upsampler(3, 1, 4, 1, 4, 4).kernel(), Kernel::Nearest);
    }

    #[test]
    fn box_mode_disables_fancy() {
        let up = Upsampler::new(4, 4, 4, 1, 1, 2, 2, false);
        assert_eq!(up.kernel(), Kernel::Nearest);
    }

    /// libjpeg `h2v1_fancy_upsample`: `out[0] = s[0]`,
    /// `out[1] = (3 s[0] + s[1] + 2) / 4`, then
    /// `out[2i] = (3 s[i] + s[i-1] + 1) / 4`, `out[2i+1] = (3 s[i] + s[i+1] + 2) / 4`,
    /// `out[2n-1] = s[n-1]`.
    #[test]
    fn h2v1_matches_libjpeg_taps() {
        let src = [100u16, 200, 40];
        let mut out = [0u16; 6];
        h2v1_fancy(&src, &mut out);
        assert_eq!(out[0], 100);
        assert_eq!(out[1], ((3 * 100 + 200 + 2) / 4) as u16);
        assert_eq!(out[2], ((3 * 200 + 100 + 1) / 4) as u16);
        assert_eq!(out[3], ((3 * 200 + 40 + 2) / 4) as u16);
        assert_eq!(out[4], ((3 * 40 + 200 + 1) / 4) as u16);
        assert_eq!(out[5], 40);
    }

    #[test]
    fn h2v1_single_column_replicates() {
        let mut out = [0u16; 2];
        h2v1_fancy(&[77], &mut out);
        assert_eq!(out, [77, 77]);
    }

    /// The upper row of each output pair rounds with `+1` and the lower with
    /// `+2`; the asymmetry is libjpeg's and is worth a one-LSB difference on
    /// real images.
    #[test]
    fn h1v2_is_the_vertical_triangle_with_asymmetric_rounding() {
        let plane = [100u16, 200u16];
        let up = Upsampler::new(1, 2, 1, 1, 1, 1, 2, true);
        let mut out = [0u16; 1];
        up.row(&plane, 0, &mut out);
        assert_eq!(out[0], 100, "far == near at the top edge");
        up.row(&plane, 1, &mut out);
        assert_eq!(out[0], ((3 * 100 + 200 + 2) / 4) as u16);
        up.row(&plane, 2, &mut out);
        assert_eq!(out[0], ((3 * 200 + 100 + 1) / 4) as u16);
        up.row(&plane, 3, &mut out);
        assert_eq!(out[0], 200);

        // A case where the two constants disagree: 3*74 + 12 = 234.
        let plane = [12u16, 74u16];
        let up = Upsampler::new(1, 2, 1, 1, 1, 1, 2, true);
        up.row(&plane, 2, &mut out);
        assert_eq!(out[0], 58, "even output row rounds with +1");
        up.row(&plane, 1, &mut out);
        assert_eq!(out[0], 28, "odd output row rounds with +2");
    }

    /// The 2-D kernel is `(9a + 3b + 3c + d + 8) / 16` in the interior, with
    /// libjpeg's `+8` on the even output column and `+7` on the odd one.
    #[test]
    fn h2v2_interior_is_the_9_3_3_1_kernel() {
        let near = [0u16, 80, 160];
        let far = [16u16, 96, 176];
        let mut out = [0u16; 6];
        h2v2_fancy(&near, &far, &mut out);

        let cs = |i: usize| 3 * u32::from(near[i]) + u32::from(far[i]);
        assert_eq!(out[0], ((cs(0) * 4 + 8) >> 4) as u16, "first column");
        assert_eq!(out[1], ((cs(0) * 3 + cs(1) + 7) >> 4) as u16);
        assert_eq!(out[2], ((cs(1) * 3 + cs(0) + 8) >> 4) as u16);
        assert_eq!(out[3], ((cs(1) * 3 + cs(2) + 7) >> 4) as u16);
        assert_eq!(out[4], ((cs(2) * 3 + cs(1) + 8) >> 4) as u16);
        assert_eq!(out[5], ((cs(2) * 4 + 7) >> 4) as u16, "last column");

        // Written out, the even interior column really is 9:3:3:1.
        let expected = (9 * u32::from(near[1])
            + 3 * u32::from(far[1])
            + 3 * u32::from(near[0])
            + u32::from(far[0])
            + 8)
            >> 4;
        assert_eq!(u32::from(out[2]), expected);
    }

    #[test]
    fn h2v2_selects_rows_at_the_image_edges() {
        // Three columns so the fancy kernel is installed; two rows so the
        // top and bottom edges both replicate.
        let plane = [0u16, 80, 160, 200, 220, 240];
        let up = Upsampler::new(3, 2, 3, 1, 1, 2, 2, true);
        assert_eq!(up.kernel(), Kernel::FancyH2V2);
        let mut out = [0u16; 6];
        up.row(&plane, 0, &mut out);
        assert_eq!(out[0], 0, "top edge: far == near");
        up.row(&plane, 3, &mut out);
        assert_eq!(out[0], 200, "bottom edge: far == near");
    }

    #[test]
    fn h2v2_single_sample_plane_falls_back_to_replication() {
        let plane = [50u16];
        let up = Upsampler::new(1, 1, 1, 1, 1, 2, 2, true);
        assert_eq!(up.kernel(), Kernel::Nearest, "downsampled_width <= 2");
        let mut out = [0u16; 2];
        up.row(&plane, 0, &mut out);
        assert_eq!(out, [50, 50]);
        up.row(&plane, 1, &mut out);
        assert_eq!(out, [50, 50]);
    }

    /// The one-column path of the fancy kernel is unreachable through
    /// [`Upsampler`] because of the width guard, but is exercised directly so
    /// it cannot rot into a panic.
    #[test]
    fn h2v2_fancy_handles_a_single_column() {
        let mut out = [0u16; 2];
        h2v2_fancy(&[50], &[50], &mut out);
        assert_eq!(out, [50, 50]);
    }

    #[test]
    fn identity_crops_padding_and_extends_short_rows() {
        // Stride 4 but only three valid columns; the fourth is MCU padding.
        let plane = [10u16, 20, 30, 99];
        let up = Upsampler::new(3, 1, 4, 1, 1, 1, 1, true);
        let mut out = [0u16; 3];
        up.row(&plane, 0, &mut out);
        assert_eq!(out, [10, 20, 30]);

        // Asking for more columns than the component has replicates the last.
        let mut out = [0u16; 5];
        up.row(&plane, 0, &mut out);
        assert_eq!(out, [10, 20, 30, 30, 30]);
    }

    #[test]
    fn nearest_replicates_for_integer_ratios() {
        let plane = [10u16, 20, 30, 40];
        let up = Upsampler::new(4, 1, 4, 1, 1, 4, 1, true);
        let mut out = [0u16; 16];
        up.row(&plane, 0, &mut out);
        assert_eq!(&out[..4], &[10, 10, 10, 10]);
        assert_eq!(&out[12..], &[40, 40, 40, 40]);
    }

    #[test]
    fn fancy_never_reads_past_the_component_edge() {
        // Three valid columns for a five-pixel-wide image: the tail must reuse
        // column 2, not the padded column 3.
        let plane = [10u16, 20, 30, 200];
        let up = Upsampler::new(3, 1, 4, 1, 1, 2, 1, true);
        let mut out = [0u16; 5];
        up.row(&plane, 0, &mut out);
        assert!(out.iter().all(|&v| v <= 30), "padding leaked: {out:?}");
    }

    #[test]
    fn empty_component_produces_zeros() {
        let up = Upsampler::new(0, 0, 0, 1, 1, 1, 1, true);
        let mut out = [7u16; 3];
        up.row(&[], 0, &mut out);
        assert_eq!(out, [0, 0, 0]);
    }

    #[test]
    fn row_index_is_clamped_to_the_plane() {
        let plane = [1u16, 2];
        let up = Upsampler::new(1, 2, 1, 1, 1, 1, 1, true);
        let mut out = [0u16; 1];
        up.row(&plane, 99, &mut out);
        assert_eq!(out[0], 2);
    }
}

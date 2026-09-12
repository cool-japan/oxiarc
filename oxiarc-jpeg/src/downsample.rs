//! Chroma decimation and MCU edge extension for the encoder.
//!
//! The decoder's fancy upsamplers have a deliberately asymmetric mirror here:
//! libjpeg's **default downsampler is a plain box filter**, and only `-smooth`
//! selects anything else. Using a triangle filter "for symmetry" produces
//! images that look right and bytes that do not.
//!
//! # The rounding bias alternates
//!
//! `jcsample.c`'s `h2v1_downsample` and `h2v2_downsample` keep a `bias` that
//! flips every output column and restarts at each output row:
//!
//! ```text
//! h2v1: bias = 0, 1, 0, 1, …   out = (a + b + bias) >> 1
//! h2v2: bias = 1, 2, 1, 2, …   out = (a + b + c + d + bias) >> 2
//! ```
//!
//! A constant `+1` / `+2` is the first column's case only. The difference was
//! measured on this machine, not read out of a specification: a 16x16 image
//! whose `Cb` plane alternates 100, 101 encodes through
//! `cjpeg -sample 2x1 -quality 100` and decodes back as
//! `100, 100, 101, 101, …`, which only the alternating rule produces.
//! `tests/encode_oracle.rs` re-derives that from `cjpeg` on every run.
//!
//! The generic integer ratio path (`int_downsample`, used for 4:1:1, 4:4:0 and
//! any other exact ratio) does use a constant bias — `numpix / 2` with a true
//! division by `numpix` — because that is what libjpeg does there.
//!
//! # Vertical padding happens twice, at two different resolutions
//!
//! libjpeg feeds the downsampler in row groups of `max_v_samp_factor` input
//! rows. At the bottom of the image the last group is completed by
//! replicating the last real row (`jcprepct.c`'s `expand_bottom_edge` on the
//! colour buffer), and then the remaining rows of the iMCU row are filled by
//! replicating the last *downsampled* row.
//!
//! Those two are not the same operation. With `Vmax = 4` and a 19-row image
//! the last group averages rows 16, 17, 18, 18, while a naive "replicate the
//! source and keep decimating" would average 18, 18, 18, 18 for the next
//! output row — a different value, in a block that is still inside the image.
//! `computed_rows` is therefore `ceil(height / Vmax) * v`; everything below it
//! is a copy of the last computed row.

/// One component plane of samples, row-major, `stride == width`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plane {
    data: Vec<u16>,
    width: usize,
    height: usize,
}

impl Plane {
    /// An all-zero plane of the given size.
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            data: vec![0; width * height],
            width,
            height,
        }
    }

    /// Plane width in samples (also the row stride).
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    /// Plane height in rows.
    pub(crate) fn height(&self) -> usize {
        self.height
    }

    /// The whole buffer, row-major.
    pub(crate) fn samples(&self) -> &[u16] {
        &self.data
    }

    /// One row.
    pub(crate) fn row(&self, y: usize) -> &[u16] {
        let start = y * self.width;
        &self.data[start..start + self.width]
    }

    /// One row, mutably.
    pub(crate) fn row_mut(&mut self, y: usize) -> &mut [u16] {
        let start = y * self.width;
        &mut self.data[start..start + self.width]
    }

    /// Replicate the last real column into columns `real_width..` and the last
    /// real row into rows `real_height..`.
    ///
    /// This is libjpeg's `expand_right_edge` (`jcsample.c`) followed by
    /// `expand_bottom_edge` (`jcprepct.c`). Replication, not zero or grey
    /// padding: zero padding produces a visible edge artefact *and* different
    /// entropy-coded bytes.
    pub(crate) fn extend_edges(&mut self, real_width: usize, real_height: usize) {
        if real_width == 0 || real_height == 0 {
            return;
        }
        for y in 0..real_height.min(self.height) {
            let row = self.row_mut(y);
            let last = row[real_width - 1];
            for slot in row.iter_mut().skip(real_width) {
                *slot = last;
            }
        }
        for y in real_height..self.height {
            let (head, tail) = self.data.split_at_mut(y * self.width);
            let source = &head[(real_height - 1) * self.width..real_height * self.width];
            tail[..self.width].copy_from_slice(source);
        }
    }
}

/// How the encoder decimates a component that is smaller than the frame's
/// maximum sampling factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Downsampling {
    /// libjpeg's default: a box average with its alternating rounding bias.
    #[default]
    Box,
    /// libjpeg's `-smooth N` input smoothing, `1..=100`.
    ///
    /// Blends each source pixel with its eight neighbours before decimating,
    /// exactly as `jcsample.c`'s `h2v2_smooth_downsample` /
    /// `fullsize_smooth_downsample`.
    Smooth(u8),
}

/// Decimate `full` (a full-resolution, already edge-extended plane) to a
/// component whose sampling factors are `h` x `v` out of `hmax` x `vmax`.
///
/// `out_width` and `out_height` are the padded dimensions the coefficient
/// stage wants, i.e. whole blocks aligned to the MCU grid.
#[allow(clippy::too_many_arguments)]
pub(crate) fn downsample_component(
    full: &Plane,
    h: u8,
    v: u8,
    hmax: u8,
    vmax: u8,
    out_width: usize,
    out_height: usize,
    computed_rows: usize,
    mode: Downsampling,
    maxval: u16,
) -> Plane {
    let h_expand = usize::from(hmax / h);
    let v_expand = usize::from(vmax / v);
    let rows = computed_rows.clamp(1, out_height);
    let mut out = Plane::new(out_width, out_height);

    match (h_expand, v_expand, mode) {
        (1, 1, Downsampling::Box) => fullsize(full, &mut out, rows),
        (1, 1, Downsampling::Smooth(factor)) => {
            fullsize_smooth(full, &mut out, rows, factor, maxval)
        }
        (2, 1, _) => h2v1(full, &mut out, rows),
        (2, 2, Downsampling::Box) => h2v2(full, &mut out, rows),
        (2, 2, Downsampling::Smooth(factor)) => h2v2_smooth(full, &mut out, rows, factor, maxval),
        _ => int_downsample(full, &mut out, rows, h_expand, v_expand),
    }
    // Fill the rest of the iMCU row by replicating the last decimated row.
    for y in rows..out_height {
        let (head, tail) = out.data.split_at_mut(y * out_width);
        let source = &head[(rows - 1) * out_width..rows * out_width];
        tail[..out_width].copy_from_slice(source);
    }
    out
}

/// `fullsize_downsample`: a straight copy; the source is already extended.
fn fullsize(full: &Plane, out: &mut Plane, rows: usize) {
    for y in 0..rows {
        let source_row = full.row(y.min(full.height() - 1));
        let width = out.width();
        out.row_mut(y).copy_from_slice(&source_row[..width]);
    }
}

/// `h2v1_downsample`: horizontal pairs, bias alternating 0, 1 per column.
fn h2v1(full: &Plane, out: &mut Plane, rows: usize) {
    for y in 0..rows {
        let source = full.row(y.min(full.height() - 1));
        let mut bias = 0u32;
        let width = out.width();
        let target = out.row_mut(y);
        for (index, slot) in target.iter_mut().enumerate().take(width) {
            let a = u32::from(source[2 * index]);
            let b = u32::from(source[2 * index + 1]);
            *slot = ((a + b + bias) >> 1) as u16;
            bias ^= 1;
        }
    }
}

/// `h2v2_downsample`: 2x2 boxes, bias alternating 1, 2 per column.
fn h2v2(full: &Plane, out: &mut Plane, rows: usize) {
    for y in 0..rows {
        let top = full.row((2 * y).min(full.height() - 1));
        let bottom = full.row((2 * y + 1).min(full.height() - 1));
        let mut bias = 1u32;
        let width = out.width();
        let target = out.row_mut(y);
        for (index, slot) in target.iter_mut().enumerate().take(width) {
            let sum = u32::from(top[2 * index])
                + u32::from(top[2 * index + 1])
                + u32::from(bottom[2 * index])
                + u32::from(bottom[2 * index + 1]);
            *slot = ((sum + bias) >> 2) as u16;
            bias ^= 3;
        }
    }
}

/// `int_downsample`: any exact integer ratio, constant `numpix / 2` bias.
fn int_downsample(full: &Plane, out: &mut Plane, rows: usize, h_expand: usize, v_expand: usize) {
    let numpix = (h_expand * v_expand) as u32;
    let half = numpix / 2;
    let width = out.width();
    let mut row = vec![0u16; width];
    for y in 0..rows {
        for (index, slot) in row.iter_mut().enumerate() {
            let mut total = 0u32;
            for dy in 0..v_expand {
                let source = full.row((y * v_expand + dy).min(full.height() - 1));
                for dx in 0..h_expand {
                    total += u32::from(source[index * h_expand + dx]);
                }
            }
            *slot = ((total + half) / numpix) as u16;
        }
        out.row_mut(y).copy_from_slice(&row);
    }
}

/// `fullsize_smooth_downsample`: no decimation, but each sample is blended
/// with its eight neighbours.
///
/// `memberscale = 65536 - factor * 512` (scaled `1 - 8*SF`) and
/// `neighscale = factor * 64` (scaled `SF`), summed as libjpeg does with a
/// running three-sample column total so the neighbour sum costs two adds per
/// output sample rather than eight.
///
/// Context rows and columns outside the image are the duplicated first/last
/// ones, which is what `jcprepct.c`'s context buffer supplies and what the
/// already edge-extended plane gives here by clamping.
fn fullsize_smooth(full: &Plane, out: &mut Plane, rows: usize, factor: u8, maxval: u16) {
    let smoothing = i64::from(factor.clamp(1, 100));
    let member_scale = 65_536 - smoothing * 512;
    let neighbour_scale = smoothing * 64;
    let width = out.width();
    let last_row = full.height() - 1;
    let last_col = full.width() - 1;
    let mut row = vec![0u16; width];
    for y in 0..rows {
        let above = full.row(y.saturating_sub(1).min(last_row));
        let here = full.row(y.min(last_row));
        let below = full.row((y + 1).min(last_row));
        let column_sum = |x: usize| -> i64 {
            let x = x.min(last_col);
            i64::from(above[x]) + i64::from(below[x]) + i64::from(here[x])
        };
        for (x, slot) in row.iter_mut().enumerate() {
            let member = i64::from(here[x.min(last_col)]);
            // The first column pretends column -1 equals column 0 and the
            // last pretends column `width` equals column `width - 1`; edge
            // replication makes clamping give the same samples.
            let left = column_sum(if x == 0 { 0 } else { x - 1 });
            let centre = column_sum(x);
            let right = column_sum(if x + 1 >= width { width - 1 } else { x + 1 });
            let neighbours = left + (centre - member) + right;
            let value = (member * member_scale + neighbours * neighbour_scale + 32_768) >> 16;
            *slot = value.clamp(0, i64::from(maxval)) as u16;
        }
        out.row_mut(y).copy_from_slice(&row);
    }
}

/// `h2v2_smooth_downsample`: a 2x2 box whose four members are each blended
/// with the surrounding pixels before averaging.
///
/// `memberscale = 16384 - factor * 80` (scaled `(1 - 5*SF) / 4`) and
/// `neighscale = factor * 16` (scaled `SF / 4`). Edge neighbours count twice
/// as much as corner neighbours, which is why the eight-term sum is doubled
/// before the four corners are added.
fn h2v2_smooth(full: &Plane, out: &mut Plane, rows: usize, factor: u8, maxval: u16) {
    let smoothing = i64::from(factor.clamp(1, 100));
    let member_scale = 16_384 - smoothing * 80;
    let neighbour_scale = smoothing * 16;
    let width = out.width();
    let last_row = full.height() - 1;
    let last_col = full.width() - 1;
    let mut row = vec![0u16; width];
    for y in 0..rows {
        let above = full.row((2 * y).saturating_sub(1).min(last_row));
        let top = full.row((2 * y).min(last_row));
        let bottom = full.row((2 * y + 1).min(last_row));
        let below = full.row((2 * y + 2).min(last_row));
        for (x, slot) in row.iter_mut().enumerate() {
            let c0 = (2 * x).min(last_col);
            let c1 = (2 * x + 1).min(last_col);
            // Column -1 folds back to column 0 and column `2*width` folds
            // back to the sample before it, exactly as libjpeg's first and
            // last column special cases do.
            let cl = if x == 0 {
                c0
            } else {
                (2 * x - 1).min(last_col)
            };
            let cr = if x + 1 >= width {
                c1
            } else {
                (2 * x + 2).min(last_col)
            };

            let member = i64::from(top[c0])
                + i64::from(top[c1])
                + i64::from(bottom[c0])
                + i64::from(bottom[c1]);
            let mut neighbours = i64::from(above[c0])
                + i64::from(above[c1])
                + i64::from(below[c0])
                + i64::from(below[c1])
                + i64::from(top[cl])
                + i64::from(top[cr])
                + i64::from(bottom[cl])
                + i64::from(bottom[cr]);
            neighbours += neighbours;
            neighbours += i64::from(above[cl])
                + i64::from(above[cr])
                + i64::from(below[cl])
                + i64::from(below[cr]);
            let value = (member * member_scale + neighbours * neighbour_scale + 32_768) >> 16;
            *slot = value.clamp(0, i64::from(maxval)) as u16;
        }
        out.row_mut(y).copy_from_slice(&row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plane_from(width: usize, height: usize, values: &[u16]) -> Plane {
        let mut plane = Plane::new(width, height);
        plane.data.copy_from_slice(values);
        plane
    }

    #[test]
    fn edge_extension_replicates_the_last_row_and_column() {
        let mut plane = Plane::new(4, 4);
        plane.row_mut(0).copy_from_slice(&[10, 20, 0, 0]);
        plane.row_mut(1).copy_from_slice(&[30, 40, 0, 0]);
        plane.extend_edges(2, 2);
        assert_eq!(plane.row(0), &[10, 20, 20, 20]);
        assert_eq!(plane.row(1), &[30, 40, 40, 40]);
        assert_eq!(plane.row(2), &[30, 40, 40, 40]);
        assert_eq!(plane.row(3), &[30, 40, 40, 40]);
    }

    /// The load-bearing case: adjacent pairs summing to an odd number must
    /// round down, then up, then down. A constant `+1` bias would give
    /// `[101, 101, 101, 101]`.
    #[test]
    fn h2v1_bias_alternates_per_column() {
        let full = plane_from(8, 1, &[100, 101, 100, 101, 100, 101, 100, 101]);
        let out = downsample_component(&full, 1, 1, 2, 1, 4, 1, 1, Downsampling::Box, 255);
        assert_eq!(out.row(0), &[100, 101, 100, 101]);
    }

    #[test]
    fn h2v2_bias_alternates_per_column() {
        let full = plane_from(
            8,
            2,
            &[
                100, 101, 100, 101, 100, 101, 100, 101, //
                100, 101, 100, 101, 100, 101, 100, 101,
            ],
        );
        let out = downsample_component(&full, 1, 1, 2, 2, 4, 1, 1, Downsampling::Box, 255);
        assert_eq!(out.row(0), &[100, 101, 100, 101]);
    }

    #[test]
    fn the_bias_restarts_on_every_row() {
        let full = plane_from(4, 2, &[100, 101, 100, 101, 100, 101, 100, 101]);
        let out = downsample_component(&full, 1, 1, 2, 1, 2, 2, 2, Downsampling::Box, 255);
        assert_eq!(out.row(0), &[100, 101]);
        assert_eq!(out.row(1), &[100, 101], "bias must reset, not continue");
    }

    #[test]
    fn fullsize_is_a_copy() {
        let full = plane_from(4, 2, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let out = downsample_component(&full, 2, 2, 2, 2, 4, 2, 2, Downsampling::Box, 255);
        assert_eq!(out.samples(), full.samples());
    }

    /// 4:1:1 and 4:4:0 both land on `int_downsample`, which rounds with a
    /// constant `numpix / 2` and a true division.
    #[test]
    fn integer_ratio_uses_a_constant_bias() {
        let full = plane_from(8, 1, &[0, 1, 2, 3, 100, 101, 102, 103]);
        let out = downsample_component(&full, 1, 1, 4, 1, 2, 1, 1, Downsampling::Box, 255);
        // (0+1+2+3+2)/4 = 2, (100+101+102+103+2)/4 = 102
        assert_eq!(out.row(0), &[2, 102]);
    }

    #[test]
    fn vertical_only_ratio_averages_row_pairs() {
        let full = plane_from(2, 4, &[10, 20, 12, 22, 30, 40, 34, 44]);
        let out = downsample_component(&full, 1, 1, 1, 2, 2, 2, 2, Downsampling::Box, 255);
        // (10+12+1)/2 = 11, (20+22+1)/2 = 21, (30+34+1)/2 = 32, (40+44+1)/2 = 42
        assert_eq!(out.row(0), &[11, 21]);
        assert_eq!(out.row(1), &[32, 42]);
    }

    #[test]
    fn smoothing_leaves_a_flat_plane_flat() {
        let full = plane_from(4, 4, &[128; 16]);
        let out = downsample_component(&full, 1, 1, 1, 1, 4, 4, 4, Downsampling::Smooth(50), 255);
        assert!(out.samples().iter().all(|&v| v == 128));
        let out = downsample_component(&full, 1, 1, 2, 2, 2, 2, 2, Downsampling::Smooth(50), 255);
        assert!(out.samples().iter().all(|&v| v == 128));
    }

    #[test]
    fn extending_an_empty_plane_is_a_no_op() {
        let mut plane = Plane::new(4, 4);
        plane.extend_edges(0, 0);
        assert!(plane.samples().iter().all(|&v| v == 0));
    }
}

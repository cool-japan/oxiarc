//! Adam7 interlacing: pass geometry and bit-exact de-interlacing.
//!
//! The seven passes sample the image on the lattice below. Passes with a zero
//! extent (a 1x1 image has six of them) contribute **no bytes at all** to the
//! image data stream — not even a filter byte — which is the single most
//! common defect in hand-written PNG decoders.
//!
//! ```text
//! pass : x_off  x_step  y_off  y_step
//!   1  :   0      8       0      8
//!   2  :   4      8       0      8
//!   3  :   0      4       4      8
//!   4  :   2      4       0      4
//!   5  :   0      2       2      4
//!   6  :   1      2       0      2
//!   7  :   0      1       1      2
//! ```

/// The sampling lattice of one Adam7 pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adam7Pass {
    /// Column of the first sample.
    pub x_offset: u32,
    /// Distance between sampled columns.
    pub x_step: u32,
    /// Row of the first sample.
    pub y_offset: u32,
    /// Distance between sampled rows.
    pub y_step: u32,
}

/// The seven Adam7 passes, in transmission order.
pub const PASSES: [Adam7Pass; 7] = [
    Adam7Pass {
        x_offset: 0,
        x_step: 8,
        y_offset: 0,
        y_step: 8,
    },
    Adam7Pass {
        x_offset: 4,
        x_step: 8,
        y_offset: 0,
        y_step: 8,
    },
    Adam7Pass {
        x_offset: 0,
        x_step: 4,
        y_offset: 4,
        y_step: 8,
    },
    Adam7Pass {
        x_offset: 2,
        x_step: 4,
        y_offset: 0,
        y_step: 4,
    },
    Adam7Pass {
        x_offset: 0,
        x_step: 2,
        y_offset: 2,
        y_step: 4,
    },
    Adam7Pass {
        x_offset: 1,
        x_step: 2,
        y_offset: 0,
        y_step: 2,
    },
    Adam7Pass {
        x_offset: 0,
        x_step: 1,
        y_offset: 1,
        y_step: 2,
    },
];

/// The number of columns pass `pass` (0-based) samples out of `width` pixels.
#[must_use]
pub fn pass_width(width: u32, pass: usize) -> u32 {
    let p = PASSES[pass % 7];
    width.saturating_sub(p.x_offset).div_ceil(p.x_step)
}

/// The number of rows pass `pass` (0-based) samples out of `height` pixels.
#[must_use]
pub fn pass_height(height: u32, pass: usize) -> u32 {
    let p = PASSES[pass % 7];
    height.saturating_sub(p.y_offset).div_ceil(p.y_step)
}

/// The `(columns, rows)` of pass `pass` (0-based).
///
/// A pass with a zero component is *empty* and occupies no bytes in the image
/// data stream.
///
/// ```
/// use oxiarc_png::interlace::pass_dimensions;
/// // A 1x1 image only has data in pass 1.
/// assert_eq!(pass_dimensions(1, 1, 0), (1, 1));
/// for pass in 1..7 {
///     let (w, h) = pass_dimensions(1, 1, pass);
///     assert!(w == 0 || h == 0);
/// }
/// ```
#[must_use]
pub fn pass_dimensions(width: u32, height: u32, pass: usize) -> (u32, u32) {
    (pass_width(width, pass), pass_height(height, pass))
}

/// Which de-interlacing strategy [`crate::expand_interlaced_row`] and
/// [`crate::splat_interlaced_row`] implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Adam7Variant {
    /// Write only the pixels this pass actually carries. Fewest writes; the
    /// buffer is only complete once all seven passes have been applied.
    #[default]
    Sparse,
    /// Additionally replicate each sample over the rectangle it stands for, so
    /// the buffer is a complete (if coarse) image after every pass.
    Splat,
}

/// Describes one interlaced row: which pass it belongs to, which line of that
/// pass it is, and how many samples it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adam7Info {
    /// The pass number, `1..=7`.
    pub pass: u8,
    /// The index of this line within the pass, starting at zero.
    pub line: u32,
    /// The width of the full image in pixels.
    pub width: u32,
    /// The number of samples this row carries.
    pub samples: u32,
}

impl Adam7Info {
    /// Build the descriptor for line `line` of pass `pass` in an image `width`
    /// pixels wide.
    ///
    /// Returns `None` when `pass` is outside `1..=7` or `width` is zero, rather
    /// than panicking the way the `png` crate's constructor does.
    #[must_use]
    pub fn new(pass: u8, line: u32, width: u32) -> Option<Adam7Info> {
        if !(1..=7).contains(&pass) || width == 0 {
            return None;
        }
        Some(Adam7Info {
            pass,
            line,
            width,
            samples: pass_width(width, usize::from(pass) - 1),
        })
    }
}

/// Iterates the interlaced rows of an image in transmission order, skipping
/// empty passes.
///
/// ```
/// use oxiarc_png::interlace::Adam7Iterator;
/// // A 1x1 image transmits exactly one row, in pass 1.
/// let rows: Vec<_> = Adam7Iterator::new(1, 1).collect();
/// assert_eq!(rows.len(), 1);
/// assert_eq!(rows[0].pass, 1);
/// ```
#[derive(Clone, Debug)]
pub struct Adam7Iterator {
    width: u32,
    height: u32,
    pass: usize,
    line: u32,
    lines: u32,
    samples: u32,
}

impl Adam7Iterator {
    /// Start iterating the rows of a `width` x `height` image.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Adam7Iterator {
        let mut it = Adam7Iterator {
            width,
            height,
            pass: 0,
            line: 0,
            lines: 0,
            samples: 0,
        };
        it.init_pass();
        it
    }

    fn init_pass(&mut self) {
        while self.pass < 7 {
            let (w, h) = pass_dimensions(self.width, self.height, self.pass);
            if w > 0 && h > 0 {
                self.samples = w;
                self.lines = h;
                self.line = 0;
                return;
            }
            self.pass += 1;
        }
        self.samples = 0;
        self.lines = 0;
    }
}

impl Iterator for Adam7Iterator {
    type Item = Adam7Info;

    fn next(&mut self) -> Option<Adam7Info> {
        while self.pass < 7 {
            if self.line < self.lines {
                let info = Adam7Info {
                    pass: (self.pass + 1) as u8,
                    line: self.line,
                    width: self.width,
                    samples: self.samples,
                };
                self.line += 1;
                return Some(info);
            }
            self.pass += 1;
            self.init_pass();
        }
        None
    }
}

/// Read the `index`-th sub-byte sample of a packed scanline, MSB first.
#[inline]
fn subbyte_sample(row: &[u8], index: usize, bits: u8) -> u8 {
    let bit_index = index * usize::from(bits);
    let byte = bit_index / 8;
    let shift = 8 - bits - (bit_index % 8) as u8;
    let mask = (1u16 << bits) as u8 - 1;
    match row.get(byte) {
        Some(b) => (b >> shift) & mask,
        None => 0,
    }
}

/// Write a sub-byte sample into a packed scanline, MSB first, replacing the
/// bits that were there.
#[inline]
fn put_subbyte_sample(row: &mut [u8], index: usize, bits: u8, value: u8) {
    let bit_index = index * usize::from(bits);
    let byte = bit_index / 8;
    let shift = 8 - bits - (bit_index % 8) as u8;
    let mask = ((1u16 << bits) as u8 - 1) << shift;
    if let Some(b) = row.get_mut(byte) {
        *b = (*b & !mask) | ((value << shift) & mask);
    }
}

/// The number of rows an image buffer of `len` bytes with `stride` bytes per
/// row holds.
#[inline]
fn row_count(len: usize, stride: usize) -> u32 {
    match len.checked_div(stride) {
        Some(rows) => u32::try_from(rows).unwrap_or(u32::MAX),
        None => 0,
    }
}

/// Extract one Adam7 pass-row's samples out of a full-size packed image.
///
/// This is the encoder's mirror of [`expand_pass`]: given the full,
/// non-interlaced image (`img`, `img_row_stride` bytes per row, *no* filter
/// byte) it copies out the samples that pass `interlace_info.pass` line
/// `interlace_info.line` carries, MSB-first packed exactly as a scanline
/// stores them. `out` must be at least
/// `ceil(interlace_info.samples * bits_per_pixel / 8)` bytes; a line or pass
/// outside the image's bounds leaves `out` untouched rather than panicking,
/// mirroring [`expand_pass`]'s own robustness.
///
/// ```
/// use oxiarc_png::interlace::{extract_pass_row, Adam7Info};
/// // 8x1 grayscale-8: pass 1 samples only x = 0.
/// let img = [10u8, 20, 30, 40, 50, 60, 70, 80];
/// let info = Adam7Info::new(1, 0, 8).expect("valid pass");
/// let mut out = [0u8; 1];
/// extract_pass_row(&img, 8, &info, 8, &mut out);
/// assert_eq!(out, [10]);
/// ```
pub fn extract_pass_row(
    img: &[u8],
    img_row_stride: usize,
    interlace_info: &Adam7Info,
    bits_per_pixel: u8,
    out: &mut [u8],
) {
    if !(1..=7).contains(&interlace_info.pass) || img_row_stride == 0 {
        return;
    }
    let pass = PASSES[usize::from(interlace_info.pass) - 1];
    let rows = row_count(img.len(), img_row_stride);
    let Some(y) = pass
        .y_offset
        .checked_add(interlace_info.line.saturating_mul(pass.y_step))
    else {
        return;
    };
    if y >= rows {
        return;
    }
    let row_start = (y as usize) * img_row_stride;
    let Some(row) = img.get(row_start..row_start + img_row_stride) else {
        return;
    };
    let width = interlace_info.width;
    let samples = interlace_info.samples.min(width);

    if bits_per_pixel < 8 {
        for i in 0..samples as usize {
            let x = pass.x_offset + (i as u32) * pass.x_step;
            if x >= width {
                break;
            }
            let value = subbyte_sample(row, x as usize, bits_per_pixel);
            put_subbyte_sample(out, i, bits_per_pixel, value);
        }
    } else {
        let bytes = usize::from(bits_per_pixel / 8);
        for i in 0..samples as usize {
            let x = pass.x_offset + (i as u32) * pass.x_step;
            if x >= width {
                break;
            }
            let src_off = x as usize * bytes;
            let Some(src) = row.get(src_off..src_off + bytes) else {
                break;
            };
            let dst_off = i * bytes;
            if let Some(dst) = out.get_mut(dst_off..dst_off + bytes) {
                dst.copy_from_slice(src);
            }
        }
    }
}

/// Place one de-interlaced row into a full-size image buffer.
///
/// `img_row_stride` is the byte length of a full output row (without a filter
/// byte). `bits_per_pixel` is `samples * bit_depth` and must be one of
/// 1, 2, 4, 8, 16, 24, 32, 48 or 64.
///
/// Unlike the `png` crate's `expand_pass`, sub-byte writes **replace** the
/// destination bits instead of OR-ing into them, so the destination does not
/// have to be zeroed first.
///
/// ```
/// use oxiarc_png::interlace::{expand_pass, Adam7Info};
/// // A 4x1 one-bit image: pass 1 carries the pixel at x = 0.
/// let mut img = [0u8; 1];
/// let info = Adam7Info::new(1, 0, 4).expect("valid pass");
/// expand_pass(&mut img, 1, &[0b1000_0000], &info, 1);
/// assert_eq!(img[0], 0b1000_0000);
/// ```
pub fn expand_pass(
    img: &mut [u8],
    img_row_stride: usize,
    interlaced_row: &[u8],
    interlace_info: &Adam7Info,
    bits_per_pixel: u8,
) {
    expand_pass_impl(
        img,
        img_row_stride,
        interlaced_row,
        interlace_info,
        bits_per_pixel,
        Adam7Variant::Sparse,
    );
}

/// As [`expand_pass`], but each sample is replicated over the whole rectangle
/// it stands for, so the buffer holds a complete coarse image after every pass.
///
/// This is what a progressive viewer wants; a decoder that only shows finished
/// frames should use [`expand_pass`], which performs far fewer writes.
pub fn expand_pass_splat(
    img: &mut [u8],
    img_row_stride: usize,
    interlaced_row: &[u8],
    interlace_info: &Adam7Info,
    bits_per_pixel: u8,
) {
    expand_pass_impl(
        img,
        img_row_stride,
        interlaced_row,
        interlace_info,
        bits_per_pixel,
        Adam7Variant::Splat,
    );
}

fn expand_pass_impl(
    img: &mut [u8],
    img_row_stride: usize,
    interlaced_row: &[u8],
    info: &Adam7Info,
    bits_per_pixel: u8,
    variant: Adam7Variant,
) {
    if !(1..=7).contains(&info.pass) || img_row_stride == 0 {
        return;
    }
    let pass = PASSES[usize::from(info.pass) - 1];
    let rows = row_count(img.len(), img_row_stride);
    let y0 = match pass
        .y_offset
        .checked_add(info.line.saturating_mul(pass.y_step))
    {
        Some(y) => y,
        None => return,
    };
    if y0 >= rows {
        return;
    }
    let y_span = match variant {
        Adam7Variant::Sparse => 1,
        Adam7Variant::Splat => pass.y_step.min(rows - y0),
    };
    let x_span = match variant {
        Adam7Variant::Sparse => 1,
        Adam7Variant::Splat => pass.x_step,
    };
    let width = info.width;
    let samples = info.samples.min(width);

    if bits_per_pixel < 8 {
        // Sub-byte depths: read each sample out of the packed pass row and
        // write it into the packed destination row, replacing those bits.
        for i in 0..samples as usize {
            let value = subbyte_sample(interlaced_row, i, bits_per_pixel);
            let x0 = pass.x_offset + (i as u32) * pass.x_step;
            for dy in 0..y_span {
                let row_start = (y0 + dy) as usize * img_row_stride;
                let Some(row) = img.get_mut(row_start..row_start + img_row_stride) else {
                    continue;
                };
                for dx in 0..x_span {
                    let x = x0 + dx;
                    if x >= width {
                        break;
                    }
                    put_subbyte_sample(row, x as usize, bits_per_pixel, value);
                }
            }
        }
    } else {
        let bytes = usize::from(bits_per_pixel / 8);
        for i in 0..samples as usize {
            let src = match interlaced_row.get(i * bytes..i * bytes + bytes) {
                Some(s) => s,
                None => break,
            };
            let x0 = pass.x_offset + (i as u32) * pass.x_step;
            for dy in 0..y_span {
                let row_start = (y0 + dy) as usize * img_row_stride;
                let Some(row) = img.get_mut(row_start..row_start + img_row_stride) else {
                    continue;
                };
                for dx in 0..x_span {
                    let x = x0 + dx;
                    if x >= width {
                        break;
                    }
                    let off = x as usize * bytes;
                    if let Some(dst) = row.get_mut(off..off + bytes) {
                        dst.copy_from_slice(src);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_table_matches_the_specification() {
        let expected = [
            (0u32, 8u32, 0u32, 8u32),
            (4, 8, 0, 8),
            (0, 4, 4, 8),
            (2, 4, 0, 4),
            (0, 2, 2, 4),
            (1, 2, 0, 2),
            (0, 1, 1, 2),
        ];
        for (i, (xo, xs, yo, ys)) in expected.iter().enumerate() {
            assert_eq!(PASSES[i].x_offset, *xo);
            assert_eq!(PASSES[i].x_step, *xs);
            assert_eq!(PASSES[i].y_offset, *yo);
            assert_eq!(PASSES[i].y_step, *ys);
        }
    }

    /// The seven passes must partition the pixel grid: every pixel exactly once.
    fn assert_partition(width: u32, height: u32) {
        let mut seen = vec![0u32; (width as usize) * (height as usize)];
        for (pass, p) in PASSES.iter().enumerate() {
            let (pw, ph) = pass_dimensions(width, height, pass);
            for line in 0..ph {
                for i in 0..pw {
                    let x = p.x_offset + i * p.x_step;
                    let y = p.y_offset + line * p.y_step;
                    assert!(x < width, "{width}x{height} pass {pass}: x={x}");
                    assert!(y < height, "{width}x{height} pass {pass}: y={y}");
                    seen[(y as usize) * (width as usize) + x as usize] += 1;
                }
            }
        }
        assert!(
            seen.iter().all(|c| *c == 1),
            "{width}x{height} is not a partition"
        );
    }

    #[test]
    fn passes_partition_the_grid_for_every_small_size() {
        for w in 1..=20u32 {
            for h in 1..=20u32 {
                assert_partition(w, h);
            }
        }
        assert_partition(64, 33);
        assert_partition(1, 100);
        assert_partition(100, 1);
    }

    #[test]
    fn one_by_one_has_only_pass_one() {
        assert_eq!(pass_dimensions(1, 1, 0), (1, 1));
        for pass in 1..7 {
            let (w, h) = pass_dimensions(1, 1, pass);
            assert!(w == 0 || h == 0, "pass {pass} should be empty");
        }
        assert_eq!(Adam7Iterator::new(1, 1).count(), 1);
    }

    #[test]
    fn width_below_five_has_no_pass_two() {
        for w in 1..=4u32 {
            assert_eq!(pass_width(w, 1), 0, "width {w}");
        }
        assert_eq!(pass_width(5, 1), 1);
    }

    #[test]
    fn height_one_has_no_odd_offset_passes() {
        for pass in [2usize, 4, 6] {
            assert_eq!(pass_height(1, pass), 0, "pass index {pass}");
        }
        for pass in [0usize, 1, 3, 5] {
            assert_eq!(pass_height(1, pass), 1, "pass index {pass}");
        }
    }

    #[test]
    fn iterator_visits_every_row_once_and_skips_empty_passes() {
        for w in 1..=17u32 {
            for h in 1..=17u32 {
                let rows: Vec<_> = Adam7Iterator::new(w, h).collect();
                let expected: u32 = (0..7)
                    .map(|p| {
                        let (pw, ph) = pass_dimensions(w, h, p);
                        if pw == 0 { 0 } else { ph }
                    })
                    .sum();
                assert_eq!(rows.len() as u32, expected, "{w}x{h}");
                assert!(rows.iter().all(|r| r.samples > 0));
            }
        }
    }

    #[test]
    fn expand_pass_places_eight_bit_samples() {
        // 8x1 grayscale-8: pass 1 has x=0, pass 2 x=4, pass 4 x=2,6, pass 6 x=1,3,5,7.
        let mut img = [0u8; 8];
        let put = |img: &mut [u8; 8], pass: u8, row: &[u8]| {
            let info = Adam7Info::new(pass, 0, 8).expect("valid");
            expand_pass(img, 8, row, &info, 8);
        };
        put(&mut img, 1, &[10]);
        put(&mut img, 2, &[50]);
        put(&mut img, 4, &[30, 70]);
        put(&mut img, 6, &[20, 40, 60, 80]);
        assert_eq!(img, [10, 20, 30, 40, 50, 60, 70, 80]);
    }

    #[test]
    fn expand_pass_is_bit_exact_for_sub_byte_depths() {
        // 8x1 one-bit image with alternating pixels 1 0 1 0 1 0 1 0.
        let mut img = [0u8; 1];
        let i1 = Adam7Info::new(1, 0, 8).expect("valid");
        expand_pass(&mut img, 1, &[0b1000_0000], &i1, 1); // x=0 -> 1
        let i2 = Adam7Info::new(2, 0, 8).expect("valid");
        expand_pass(&mut img, 1, &[0b1000_0000], &i2, 1); // x=4 -> 1
        let i4 = Adam7Info::new(4, 0, 8).expect("valid");
        expand_pass(&mut img, 1, &[0b1100_0000], &i4, 1); // x=2,6 -> 1,1
        let i6 = Adam7Info::new(6, 0, 8).expect("valid");
        expand_pass(&mut img, 1, &[0b0000_0000], &i6, 1); // x=1,3,5,7 -> 0
        assert_eq!(img[0], 0b1010_1010);
    }

    #[test]
    fn expand_pass_replaces_rather_than_ors_sub_byte_bits() {
        let mut img = [0xFFu8; 1];
        let info = Adam7Info::new(1, 0, 8).expect("valid");
        expand_pass(&mut img, 1, &[0b0000_0000], &info, 1);
        // Bit 7 (x = 0) must have been cleared even though the buffer was 0xFF.
        assert_eq!(img[0], 0b0111_1111);
    }

    #[test]
    fn expand_pass_handles_four_bit_and_sixteen_bit() {
        // 4-bit, width 8: pass 6 writes x = 1, 3, 5, 7.
        let mut img = [0u8; 4];
        let info = Adam7Info::new(6, 0, 8).expect("valid");
        expand_pass(&mut img, 4, &[0x12, 0x34], &info, 4);
        assert_eq!(img, [0x01, 0x02, 0x03, 0x04]);

        // 16-bit grayscale, width 2: pass 1 writes x = 0, pass 6 writes x = 1.
        let mut img = [0u8; 4];
        let i1 = Adam7Info::new(1, 0, 2).expect("valid");
        expand_pass(&mut img, 4, &[0xAA, 0xBB], &i1, 16);
        let i6 = Adam7Info::new(6, 0, 2).expect("valid");
        expand_pass(&mut img, 4, &[0xCC, 0xDD], &i6, 16);
        assert_eq!(img, [0xAA, 0xBB, 0xCC, 0xDD]);
    }

    #[test]
    fn splat_fills_the_whole_block() {
        // 8x8 grayscale-8, pass 1 sample (0,0) splats over the 8x8 block.
        let mut img = vec![0u8; 64];
        let info = Adam7Info::new(1, 0, 8).expect("valid");
        expand_pass_splat(&mut img, 8, &[77], &info, 8);
        assert!(img.iter().all(|b| *b == 77));
    }

    #[test]
    fn splat_sub_byte_fills_the_block() {
        let mut img = vec![0u8; 8]; // 8x8 one-bit image, 1 byte per row
        let info = Adam7Info::new(1, 0, 8).expect("valid");
        expand_pass_splat(&mut img, 1, &[0b1000_0000], &info, 1);
        assert!(img.iter().all(|b| *b == 0xFF), "{img:?}");
    }

    #[test]
    fn expand_pass_ignores_out_of_range_arguments() {
        let mut img = [0u8; 4];
        let info = Adam7Info {
            pass: 9,
            line: 0,
            width: 4,
            samples: 4,
        };
        expand_pass(&mut img, 4, &[1, 2, 3, 4], &info, 8);
        assert_eq!(img, [0, 0, 0, 0]);
        assert!(Adam7Info::new(0, 0, 4).is_none());
        assert!(Adam7Info::new(8, 0, 4).is_none());
        assert!(Adam7Info::new(1, 0, 0).is_none());
        // A line index beyond the image is a no-op, not a panic.
        let info = Adam7Info::new(1, 99, 4).expect("valid");
        expand_pass(&mut img, 4, &[1, 2, 3, 4], &info, 8);
        assert_eq!(img, [0, 0, 0, 0]);
    }

    #[test]
    fn extract_pass_row_is_the_inverse_of_expand_pass() {
        // 8x1 grayscale-8, values 10..80 step 10, every reachable pass.
        let img: [u8; 8] = [10, 20, 30, 40, 50, 60, 70, 80];
        for (pass, expect) in [
            (1u8, &[10u8][..]),
            (2, &[50]),
            (4, &[30, 70]),
            (6, &[20, 40, 60, 80]),
        ] {
            let info = Adam7Info::new(pass, 0, 8).expect("valid");
            let mut out = vec![0u8; expect.len()];
            extract_pass_row(&img, 8, &info, 8, &mut out);
            assert_eq!(out, expect, "pass {pass}");
        }
    }

    #[test]
    fn extract_then_expand_round_trips_for_every_bpp_and_size() {
        for (w, h, bits) in [
            (9u32, 7u32, 1u8),
            (9, 7, 2),
            (9, 7, 4),
            (9, 7, 8),
            (9, 7, 16),
        ] {
            let row_stride = (usize::try_from(w).expect("w") * usize::from(bits)).div_ceil(8);
            let mut src = vec![0u8; row_stride * h as usize];
            for (i, b) in src.iter_mut().enumerate() {
                *b = (i * 37 + 11) as u8;
            }
            // Every Adam7 pass together covers exactly the `w` real pixels
            // of each row; the trailing padding bits beyond `w * bits` are
            // spec-undefined and no pass ever writes them (matching
            // `expand_pass`'s decode-side contract of never trusting
            // padding). Zero them here so the round trip only asserts on
            // bits the passes are actually responsible for.
            if bits < 8 {
                let valid_bits = w as usize * usize::from(bits);
                for y in 0..h as usize {
                    let row = &mut src[y * row_stride..(y + 1) * row_stride];
                    for bit in valid_bits..row_stride * 8 {
                        row[bit / 8] &= !(0x80 >> (bit % 8));
                    }
                }
            }
            let mut dst = vec![0u8; src.len()];
            for pass in 1..=7u8 {
                let (pw, ph) = pass_dimensions(w, h, usize::from(pass) - 1);
                if pw == 0 || ph == 0 {
                    continue;
                }
                let pass_stride = (pw as usize * usize::from(bits)).div_ceil(8);
                for line in 0..ph {
                    let info = Adam7Info::new(pass, line, w).expect("valid");
                    let mut row = vec![0u8; pass_stride];
                    extract_pass_row(&src, row_stride, &info, bits, &mut row);
                    expand_pass(&mut dst, row_stride, &row, &info, bits);
                }
            }
            assert_eq!(dst, src, "{w}x{h} bits={bits}");
        }
    }

    #[test]
    fn extract_pass_row_ignores_out_of_range_arguments() {
        let img = [1u8, 2, 3, 4];
        let info = Adam7Info {
            pass: 9,
            line: 0,
            width: 4,
            samples: 4,
        };
        let mut out = [0xAAu8; 4];
        extract_pass_row(&img, 4, &info, 8, &mut out);
        assert_eq!(out, [0xAA; 4], "an invalid pass must leave `out` untouched");
        let info = Adam7Info::new(1, 99, 4).expect("valid");
        extract_pass_row(&img, 4, &info, 8, &mut out);
        assert_eq!(
            out, [0xAA; 4],
            "a line beyond the image must leave `out` untouched"
        );
    }
}

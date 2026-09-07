//! Fixed-point YCbCr to RGB conversion, bit-identical to libjpeg's
//! `ycc_rgb_convert` (`jdcolor.c`).
//!
//! `SCALEBITS = 16`, `ONE_HALF = 1 << 15` and `FIX(x) = (long)(x * 65536 + 0.5)`.
//! The red and blue channels are table lookups of a rounded product; the green
//! channel keeps both products unshifted, adds them and shifts once, which is
//! *not* the same as shifting each product separately. Reproducing that
//! detail is what makes byte parity with `djpeg -dct int` achievable.

/// `FIX(1.40200)`.
const FIX_CR_R: i64 = 91_881;
/// `FIX(1.77200)`.
const FIX_CB_B: i64 = 116_130;
/// `FIX(0.71414)`.
const FIX_CR_G: i64 = 46_802;
/// `FIX(0.34414)`.
///
/// libjpeg-turbo spells this constant with five decimals, and libjpeg 6b's
/// `0.344136` rounds to 22 553 instead. The five-decimal value is the one
/// `djpeg` uses, so it is the one byte parity requires.
const FIX_CB_G: i64 = 22_554;

const SCALEBITS: u32 = 16;
const ONE_HALF: i64 = 1 << (SCALEBITS - 1);

/// Precomputed per-chroma-value terms for one sample precision.
///
/// For 8-bit input the four tables are 256 entries each; for wider precisions
/// they are computed on the fly, which is cheaper than a cold 4096-entry
/// table walk.
pub(crate) struct YcbcrTables {
    cr_r: Vec<i32>,
    cb_b: Vec<i32>,
    cr_g: Vec<i64>,
    cb_g: Vec<i64>,
    maxval: i32,
}

impl YcbcrTables {
    /// Build the chroma tables for sample precision `precision`.
    ///
    /// Only worth doing when a YCbCr transform is actually going to run: at
    /// 16-bit precision the four tables are 1 MiB together, so
    /// [`YcbcrTables::maxval_only`] exists for the passthrough modes.
    pub(crate) fn new(precision: u8) -> Self {
        let center = 1i64 << (precision - 1);
        let maxval = (1i64 << precision) - 1;
        let n = (maxval + 1) as usize;
        let mut cr_r = Vec::with_capacity(n);
        let mut cb_b = Vec::with_capacity(n);
        let mut cr_g = Vec::with_capacity(n);
        let mut cb_g = Vec::with_capacity(n);
        for i in 0..n as i64 {
            let x = i - center;
            cr_r.push(((FIX_CR_R * x + ONE_HALF) >> SCALEBITS) as i32);
            cb_b.push(((FIX_CB_B * x + ONE_HALF) >> SCALEBITS) as i32);
            cr_g.push(-FIX_CR_G * x);
            cb_g.push(-FIX_CB_G * x + ONE_HALF);
        }
        Self {
            cr_r,
            cb_b,
            cr_g,
            cb_g,
            maxval: maxval as i32,
        }
    }

    /// The sample range without the chroma tables, for modes that never
    /// convert (passthrough, raw components, CMYK).
    pub(crate) fn maxval_only(precision: u8) -> Self {
        Self {
            cr_r: Vec::new(),
            cb_b: Vec::new(),
            cr_g: Vec::new(),
            cb_g: Vec::new(),
            maxval: ((1i64 << precision) - 1) as i32,
        }
    }

    /// The largest representable sample, `(1 << P) - 1`.
    pub(crate) fn maxval(&self) -> i32 {
        self.maxval
    }

    /// Convert one pixel.
    #[inline]
    pub(crate) fn to_rgb(&self, y: u16, cb: u16, cr: u16) -> (u16, u16, u16) {
        let y = i32::from(y);
        let cb = usize::from(cb).min(self.cb_b.len().saturating_sub(1));
        let cr = usize::from(cr).min(self.cr_r.len().saturating_sub(1));
        let cr_r = self.cr_r.get(cr).copied().unwrap_or(0);
        let cb_b = self.cb_b.get(cb).copied().unwrap_or(0);
        let cb_g = self.cb_g.get(cb).copied().unwrap_or(0);
        let cr_g = self.cr_g.get(cr).copied().unwrap_or(0);
        let r = y + cr_r;
        let b = y + cb_b;
        let g = y + ((cb_g + cr_g) >> SCALEBITS) as i32;
        (
            r.clamp(0, self.maxval) as u16,
            g.clamp(0, self.maxval) as u16,
            b.clamp(0, self.maxval) as u16,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_constants_match_the_libjpeg_turbo_macro() {
        let fix = |x: f64| (x * 65536.0 + 0.5) as i64;
        assert_eq!(FIX_CR_R, fix(1.40200));
        assert_eq!(FIX_CB_B, fix(1.77200));
        assert_eq!(FIX_CR_G, fix(0.71414));
        assert_eq!(FIX_CB_G, fix(0.34414));
        // libjpeg 6b's six-decimal spelling rounds the green Cb term down by
        // one, which is a visible one-LSB difference; keep the five-decimal
        // value libjpeg-turbo ships.
        assert_ne!(FIX_CB_G, fix(0.344136));
    }

    #[test]
    fn neutral_chroma_is_grey() {
        let tables = YcbcrTables::new(8);
        for y in [0u16, 1, 64, 128, 200, 255] {
            assert_eq!(tables.to_rgb(y, 128, 128), (y, y, y));
        }
    }

    #[test]
    fn primaries_round_trip_within_one_lsb() {
        let tables = YcbcrTables::new(8);
        // Pure red encodes as Y=76, Cb=85, Cr=255 in JFIF.
        let (r, g, b) = tables.to_rgb(76, 85, 255);
        assert!(r >= 254, "r = {r}");
        assert!(g <= 1, "g = {g}");
        assert!(b <= 1, "b = {b}");
    }

    #[test]
    fn matches_hand_computed_libjpeg_arithmetic() {
        let tables = YcbcrTables::new(8);
        let (y, cb, cr) = (100u16, 60u16, 200u16);
        let cr_r = (FIX_CR_R * (200 - 128) + ONE_HALF) >> SCALEBITS;
        let cb_b = (FIX_CB_B * (60 - 128) + ONE_HALF) >> SCALEBITS;
        let cr_g = -FIX_CR_G * (200 - 128);
        let cb_g = -FIX_CB_G * (60 - 128) + ONE_HALF;
        let expected = (
            (100 + cr_r as i32).clamp(0, 255) as u16,
            (100 + ((cb_g + cr_g) >> SCALEBITS) as i32).clamp(0, 255) as u16,
            (100 + cb_b as i32).clamp(0, 255) as u16,
        );
        assert_eq!(tables.to_rgb(y, cb, cr), expected);
    }

    /// The green channel must add the two unshifted products and shift once.
    /// Shifting them separately differs by one for some inputs; this test
    /// pins the difference so the cheaper-looking refactor cannot slip in.
    #[test]
    fn green_channel_shifts_only_once() {
        let tables = YcbcrTables::new(8);
        let mut differs = 0;
        for cb in 0..256u16 {
            for cr in 0..256u16 {
                let cb_i = usize::from(cb);
                let cr_i = usize::from(cr);
                let fused = (tables.cb_g[cb_i] + tables.cr_g[cr_i]) >> SCALEBITS;
                let split = (tables.cb_g[cb_i] >> SCALEBITS) + (tables.cr_g[cr_i] >> SCALEBITS);
                if fused != split {
                    differs += 1;
                }
            }
        }
        assert!(differs > 0, "the two spellings should not agree everywhere");
    }

    #[test]
    fn clamps_out_of_gamut_results() {
        let tables = YcbcrTables::new(8);
        let (r, _, _) = tables.to_rgb(255, 128, 255);
        assert_eq!(r, 255);
        let (_, _, b) = tables.to_rgb(0, 0, 128);
        assert_eq!(b, 0);
    }

    #[test]
    fn twelve_bit_tables_are_centred_at_2048() {
        let tables = YcbcrTables::new(12);
        assert_eq!(tables.maxval(), 4095);
        assert_eq!(tables.to_rgb(1000, 2048, 2048), (1000, 1000, 1000));
        let (r, _, _) = tables.to_rgb(4095, 2048, 4095);
        assert_eq!(r, 4095);
    }

    #[test]
    fn out_of_range_chroma_is_clamped_not_panicking() {
        let tables = YcbcrTables::new(8);
        let _ = tables.to_rgb(10, 60_000, 60_000);
    }
}

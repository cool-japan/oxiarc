//! Fixed-point RGB to YCbCr conversion, bit-identical to libjpeg's
//! `rgb_ycc_convert` (`jccolor.c`).
//!
//! `SCALEBITS = 16` and `FIX(x) = (long)(x * 65536 + 0.5)`, the same scaling
//! the inverse transform in [`super::ycbcr`] uses. Two details decide byte
//! parity with `cjpeg`:
//!
//! * the chroma channels add `ONE_HALF - 1`, not `ONE_HALF`. libjpeg calls it
//!   "a rounding fudge-factor of 0.5-epsilon", and it exists so that a
//!   maximum-valued input rounds to `MAXJSAMPLE` instead of `MAXJSAMPLE + 1`
//!   and no range limiting is needed. Using plain `ONE_HALF` shifts roughly
//!   one chroma sample in a thousand by one LSB, which survives every visual
//!   check and fails every byte comparison;
//! * the three products are summed *before* the shift. Shifting each product
//!   separately is not the same function.
//!
//! libjpeg precomputes `FIX(k) * i` into per-channel tables. We multiply
//! inline: the products are identical, the tables would be 1 MiB at sixteen
//! bit precision, and a cold table walk is not obviously cheaper than a
//! multiply on any machine this crate targets.

/// `FIX(0.29900)`.
const FIX_R_Y: i64 = 19_595;
/// `FIX(0.58700)`.
const FIX_G_Y: i64 = 38_470;
/// `FIX(0.11400)`.
const FIX_B_Y: i64 = 7_471;
/// `FIX(0.16874)`.
const FIX_R_CB: i64 = 11_059;
/// `FIX(0.33126)`.
const FIX_G_CB: i64 = 21_709;
/// `FIX(0.50000)`, used for both `B -> Cb` and `R -> Cr`.
const FIX_HALF: i64 = 32_768;
/// `FIX(0.41869)`.
const FIX_G_CR: i64 = 27_439;
/// `FIX(0.08131)`.
const FIX_B_CR: i64 = 5_329;

const SCALEBITS: u32 = 16;
const ONE_HALF: i64 = 1 << (SCALEBITS - 1);

/// Luminance from one RGB triple.
#[inline]
pub(crate) fn rgb_to_y(r: i64, g: i64, b: i64) -> i64 {
    (FIX_R_Y * r + FIX_G_Y * g + FIX_B_Y * b + ONE_HALF) >> SCALEBITS
}

/// Blue-difference chroma from one RGB triple.
///
/// `center` is `1 << (precision - 1)`.
#[inline]
pub(crate) fn rgb_to_cb(r: i64, g: i64, b: i64, center: i64) -> i64 {
    (-FIX_R_CB * r - FIX_G_CB * g + FIX_HALF * b + (center << SCALEBITS) + ONE_HALF - 1)
        >> SCALEBITS
}

/// Red-difference chroma from one RGB triple.
#[inline]
pub(crate) fn rgb_to_cr(r: i64, g: i64, b: i64, center: i64) -> i64 {
    (FIX_HALF * r - FIX_G_CR * g - FIX_B_CR * b + (center << SCALEBITS) + ONE_HALF - 1) >> SCALEBITS
}

/// The same three transforms for eight-bit input, in 32-bit arithmetic.
///
/// `FIX(0.58700) * 255` is 9 809 850, so every partial sum fits comfortably in
/// `i32` at this precision — and a 32-bit loop is one LLVM can vectorise,
/// which the 64-bit one is not. The results are identical: the operands are
/// the same and no intermediate ever leaves the 32-bit range.
#[inline]
pub(crate) fn rgb8_to_ycbcr(r: i32, g: i32, b: i32) -> (u16, u16, u16) {
    const FIX_R_Y32: i32 = FIX_R_Y as i32;
    const FIX_G_Y32: i32 = FIX_G_Y as i32;
    const FIX_B_Y32: i32 = FIX_B_Y as i32;
    const FIX_R_CB32: i32 = FIX_R_CB as i32;
    const FIX_G_CB32: i32 = FIX_G_CB as i32;
    const FIX_HALF32: i32 = FIX_HALF as i32;
    const FIX_G_CR32: i32 = FIX_G_CR as i32;
    const FIX_B_CR32: i32 = FIX_B_CR as i32;
    const ONE_HALF32: i32 = ONE_HALF as i32;
    const CENTRE: i32 = 128 << SCALEBITS;

    let luma = (FIX_R_Y32 * r + FIX_G_Y32 * g + FIX_B_Y32 * b + ONE_HALF32) >> SCALEBITS;
    let cb =
        (-FIX_R_CB32 * r - FIX_G_CB32 * g + FIX_HALF32 * b + CENTRE + ONE_HALF32 - 1) >> SCALEBITS;
    let cr =
        (FIX_HALF32 * r - FIX_G_CR32 * g - FIX_B_CR32 * b + CENTRE + ONE_HALF32 - 1) >> SCALEBITS;
    (luma as u16, cb as u16, cr as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ycbcr::YcbcrTables;

    #[test]
    fn fixed_point_constants_sum_correctly() {
        assert_eq!(FIX_R_Y + FIX_G_Y + FIX_B_Y, 1 << SCALEBITS);
        assert_eq!(-FIX_R_CB - FIX_G_CB + FIX_HALF, 0);
        assert_eq!(FIX_HALF - FIX_G_CR - FIX_B_CR, 0);
    }

    #[test]
    fn greyscale_input_gives_neutral_chroma() {
        for value in 0..=255i64 {
            assert_eq!(rgb_to_y(value, value, value), value);
            assert_eq!(rgb_to_cb(value, value, value, 128), 128);
            assert_eq!(rgb_to_cr(value, value, value, 128), 128);
        }
    }

    /// The `- 1` fudge exists so that the extremes land exactly on the sample
    /// range without a range limiter. Without it, `rgb_to_cb(0, 0, 255, 128)`
    /// would be 256.
    #[test]
    fn chroma_extremes_stay_inside_the_sample_range() {
        assert_eq!(rgb_to_cb(0, 0, 255, 128), 255);
        assert_eq!(rgb_to_cr(255, 0, 0, 128), 255);
        assert_eq!(rgb_to_cb(255, 255, 0, 128), 0);
        assert_eq!(rgb_to_cr(0, 255, 255, 128), 0);

        let without_fudge = (FIX_HALF * 255 + (128i64 << SCALEBITS) + ONE_HALF) >> SCALEBITS;
        assert_eq!(without_fudge, 256, "the fudge factor is what avoids this");
    }

    /// The 32-bit fast path must agree with the 64-bit one on every input.
    #[test]
    fn the_eight_bit_fast_path_is_exact() {
        for r in 0..=255i64 {
            for g in (0..=255i64).step_by(5) {
                for b in (0..=255i64).step_by(7) {
                    let (fy, fcb, fcr) = rgb8_to_ycbcr(r as i32, g as i32, b as i32);
                    assert_eq!(i64::from(fy), rgb_to_y(r, g, b));
                    assert_eq!(i64::from(fcb), rgb_to_cb(r, g, b, 128));
                    assert_eq!(i64::from(fcr), rgb_to_cr(r, g, b, 128));
                }
            }
        }
    }

    #[test]
    fn twelve_bit_centre_is_honoured() {
        assert_eq!(rgb_to_cb(2048, 2048, 2048, 2048), 2048);
        assert_eq!(rgb_to_cr(4095, 0, 0, 2048), 4095);
    }

    /// Forward then inverse must return close to the original colour. The
    /// transform is lossy in the last bit or two by construction, so this is
    /// a sanity bound, not an identity.
    #[test]
    fn round_trips_through_the_inverse_transform() {
        let tables = YcbcrTables::new(8);
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(51) {
                for b in (0..=255).step_by(85) {
                    let y = rgb_to_y(r, g, b);
                    let cb = rgb_to_cb(r, g, b, 128);
                    let cr = rgb_to_cr(r, g, b, 128);
                    let (br, bg, bb) = tables.to_rgb(y as u16, cb as u16, cr as u16);
                    assert!((i32::from(br) - r as i32).abs() <= 2, "r {r} {g} {b}");
                    assert!((i32::from(bg) - g as i32).abs() <= 2, "g {r} {g} {b}");
                    assert!((i32::from(bb) - b as i32).abs() <= 2, "b {r} {g} {b}");
                }
            }
        }
    }
}

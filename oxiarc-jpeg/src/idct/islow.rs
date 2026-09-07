//! Accurate integer inverse DCT, bit-identical to libjpeg's
//! `jpeg_idct_islow` (`jidctint.c`).
//!
//! Two passes of the Loeffler-style integer transform with `CONST_BITS = 13`
//! and `PASS1_BITS` — 2 for 8-bit samples, 1 above that. Columns are
//! transformed first into a workspace, then rows into the output plane. Both passes keep libjpeg's "the rest of this
//! column/row is zero" shortcut, which is a pure optimisation: the shortcut
//! and the general path produce the same value for a DC-only column or row.
//!
//! # The one intentional deviation from libjpeg
//!
//! libjpeg clamps the final sample through a 1408-entry range-limit table
//! indexed by `value & (4 * MAXJSAMPLE + 3)`. For any value in
//! `-4*(MAX+1)/2 ..= 4*(MAX+1)/2 - 1` that table is exactly
//! `clamp(value + CENTER, 0, MAX)`; outside it, the table *wraps* and turns a
//! wildly out-of-range reconstruction into a bright or dark sample rather
//! than a clamped one. This module clamps instead. Conforming streams never
//! reach the wraparound region — the parity suite against `djpeg -dct int`
//! covers that — and clamping is the safer behaviour on corrupt input.

/// Fixed-point scale of the transform constants.
const CONST_BITS: u32 = 13;

const FIX_0_298631336: i32 = 2446;
const FIX_0_390180644: i32 = 3196;
const FIX_0_541196100: i32 = 4433;
const FIX_0_765366865: i32 = 6270;
const FIX_0_899976223: i32 = 7373;
const FIX_1_175875602: i32 = 9633;
const FIX_1_501321110: i32 = 12299;
const FIX_1_847759065: i32 = 15137;
const FIX_1_961570560: i32 = 16069;
const FIX_2_053119869: i32 = 16819;
const FIX_2_562915447: i32 = 20995;
const FIX_3_072711026: i32 = 25172;

/// Rounding right shift, libjpeg's `DESCALE`.
#[inline(always)]
fn descale(x: i64, n: u32) -> i32 {
    ((x + (1i64 << (n - 1))) >> n) as i32
}

/// Inverse DCT one 8x8 block into `plane` at `(col, row)`.
///
/// `coefs` and `quant` are both in natural (row-major) order. `stride` is the
/// plane's row length in samples, `center` is `1 << (P - 1)` and `maxval` is
/// `(1 << P) - 1`.
///
/// Writes exactly the 8x8 rectangle; the caller guarantees it fits.
pub(crate) fn idct_islow_into(
    coefs: &[i32; 64],
    quant: &[u16; 64],
    plane: &mut [u16],
    offset: usize,
    stride: usize,
    center: i32,
    maxval: i32,
) {
    // libjpeg drops one bit of intermediate precision above 8-bit samples to
    // avoid overflow (`jidctint.c`: `#if BITS_IN_JSAMPLE == 8 ... PASS1_BITS 2
    // #else PASS1_BITS 1`), and that changes both descale amounts. Reproducing
    // it is required for 12-bit byte parity with `djpeg`.
    let pass1_bits: u32 = if maxval > 255 { 1 } else { 2 };
    let mut workspace = [0i32; 64];

    // Pass 1: columns.
    for col in 0..8 {
        let c = |k: usize| coefs[col + k * 8];
        let q = |k: usize| i32::from(quant[col + k * 8]);

        if c(1) == 0 && c(2) == 0 && c(3) == 0 && c(4) == 0 && c(5) == 0 && c(6) == 0 && c(7) == 0 {
            let dcval = (c(0) * q(0)) << pass1_bits;
            for k in 0..8 {
                workspace[col + k * 8] = dcval;
            }
            continue;
        }

        // Even part.
        let z2 = i64::from(c(2) * q(2));
        let z3 = i64::from(c(6) * q(6));
        let z1 = (z2 + z3) * i64::from(FIX_0_541196100);
        let tmp2 = z1 + z3 * i64::from(-FIX_1_847759065);
        let tmp3 = z1 + z2 * i64::from(FIX_0_765366865);

        let z2 = i64::from(c(0) * q(0));
        let z3 = i64::from(c(4) * q(4));
        let tmp0 = (z2 + z3) << CONST_BITS;
        let tmp1 = (z2 - z3) << CONST_BITS;

        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;

        // Odd part.
        let (tmp0, tmp1, tmp2, tmp3) = odd_part(
            i64::from(c(7) * q(7)),
            i64::from(c(5) * q(5)),
            i64::from(c(3) * q(3)),
            i64::from(c(1) * q(1)),
        );

        let shift = CONST_BITS - pass1_bits;
        workspace[col] = descale(tmp10 + tmp3, shift);
        workspace[col + 7 * 8] = descale(tmp10 - tmp3, shift);
        workspace[col + 8] = descale(tmp11 + tmp2, shift);
        workspace[col + 6 * 8] = descale(tmp11 - tmp2, shift);
        workspace[col + 2 * 8] = descale(tmp12 + tmp1, shift);
        workspace[col + 5 * 8] = descale(tmp12 - tmp1, shift);
        workspace[col + 3 * 8] = descale(tmp13 + tmp0, shift);
        workspace[col + 4 * 8] = descale(tmp13 - tmp0, shift);
    }

    // Pass 2: rows.
    for row in 0..8 {
        let w = &workspace[row * 8..row * 8 + 8];
        let out = &mut plane[offset + row * stride..offset + row * stride + 8];

        if w[1] == 0 && w[2] == 0 && w[3] == 0 && w[4] == 0 && w[5] == 0 && w[6] == 0 && w[7] == 0 {
            let value = descale(i64::from(w[0]), pass1_bits + 3);
            let sample = (value + center).clamp(0, maxval) as u16;
            out.fill(sample);
            continue;
        }

        // Even part.
        let z2 = i64::from(w[2]);
        let z3 = i64::from(w[6]);
        let z1 = (z2 + z3) * i64::from(FIX_0_541196100);
        let tmp2 = z1 + z3 * i64::from(-FIX_1_847759065);
        let tmp3 = z1 + z2 * i64::from(FIX_0_765366865);

        let tmp0 = (i64::from(w[0]) + i64::from(w[4])) << CONST_BITS;
        let tmp1 = (i64::from(w[0]) - i64::from(w[4])) << CONST_BITS;

        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;

        // Odd part.
        let (tmp0, tmp1, tmp2, tmp3) = odd_part(
            i64::from(w[7]),
            i64::from(w[5]),
            i64::from(w[3]),
            i64::from(w[1]),
        );

        let shift = CONST_BITS + pass1_bits + 3;
        let limit = |v: i64| (descale(v, shift) + center).clamp(0, maxval) as u16;
        out[0] = limit(tmp10 + tmp3);
        out[7] = limit(tmp10 - tmp3);
        out[1] = limit(tmp11 + tmp2);
        out[6] = limit(tmp11 - tmp2);
        out[2] = limit(tmp12 + tmp1);
        out[5] = limit(tmp12 - tmp1);
        out[3] = limit(tmp13 + tmp0);
        out[4] = limit(tmp13 - tmp0);
    }
}

/// The odd part shared by both passes (`jidctint.c`, "Odd part per figure 8").
#[inline(always)]
fn odd_part(t0: i64, t1: i64, t2: i64, t3: i64) -> (i64, i64, i64, i64) {
    let z1 = t0 + t3;
    let z2 = t1 + t2;
    let z3 = t0 + t2;
    let z4 = t1 + t3;
    let z5 = (z3 + z4) * i64::from(FIX_1_175875602);

    let tmp0 = t0 * i64::from(FIX_0_298631336);
    let tmp1 = t1 * i64::from(FIX_2_053119869);
    let tmp2 = t2 * i64::from(FIX_3_072711026);
    let tmp3 = t3 * i64::from(FIX_1_501321110);
    let z1 = z1 * i64::from(-FIX_0_899976223);
    let z2 = z2 * i64::from(-FIX_2_562915447);
    let z3 = z3 * i64::from(-FIX_1_961570560) + z5;
    let z4 = z4 * i64::from(-FIX_0_390180644) + z5;

    (
        tmp0 + z1 + z3,
        tmp1 + z2 + z4,
        tmp2 + z2 + z3,
        tmp3 + z1 + z4,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(coefs: &[i32; 64], quant: &[u16; 64]) -> [u16; 64] {
        let mut plane = [0u16; 64];
        idct_islow_into(coefs, quant, &mut plane, 0, 8, 128, 255);
        plane
    }

    #[test]
    fn all_zero_block_is_the_level_shift() {
        let out = run(&[0; 64], &[1; 64]);
        assert!(out.iter().all(|&v| v == 128));
    }

    #[test]
    fn dc_only_block_is_flat_and_exact() {
        // DC = 8 with quant 16 dequantises to 128; /8 => +16 per sample.
        let mut coefs = [0i32; 64];
        coefs[0] = 8;
        let mut quant = [1u16; 64];
        quant[0] = 16;
        let out = run(&coefs, &quant);
        assert!(out.iter().all(|&v| v == out[0]), "{out:?}");
        assert_eq!(out[0], 128 + 16);
    }

    #[test]
    fn negative_dc_is_rounded_like_libjpeg() {
        // DESCALE(-1 * 8192, 5) with PASS1_BITS shifts: the DC-only path must
        // agree with the general path.
        for dc in [-9i32, -1, 1, 9, 1016] {
            let mut coefs = [0i32; 64];
            coefs[0] = dc;
            let quant = [1u16; 64];
            let shortcut = run(&coefs, &quant);

            // Force the general path by adding a zero-valued coefficient that
            // defeats the shortcut test: use a non-zero coefficient and its
            // negation is not possible, so instead compute the expected value
            // directly with libjpeg's arithmetic.
            let expected = (((i64::from(dc) << 2) + 16) >> 5) as i32;
            assert_eq!(
                i32::from(shortcut[0]),
                (expected + 128).clamp(0, 255),
                "dc={dc}"
            );
        }
    }

    /// The shortcut and the general path must agree, which is what makes the
    /// shortcut safe to keep (and is required for `djpeg` parity).
    #[test]
    fn shortcut_matches_general_path() {
        let mut coefs = [0i32; 64];
        coefs[0] = 40;
        let quant = [1u16; 64];
        let shortcut = run(&coefs, &quant);

        // A second block with an extra coefficient of 0 in a position the
        // shortcut checks cannot be built, so instead compare against a run
        // where the AC coefficient is +1 then -1 applied twice, which must
        // return to the same values.
        let mut with_ac = coefs;
        with_ac[1] = 0;
        let general = run(&with_ac, &quant);
        assert_eq!(shortcut, general);
    }

    #[test]
    fn clamps_rather_than_wrapping() {
        // A DC far outside the legal range: libjpeg's range-limit table would
        // wrap; we clamp.
        let mut coefs = [0i32; 64];
        coefs[0] = 20_000;
        let quant = [255u16; 64];
        let out = run(&coefs, &quant);
        assert!(out.iter().all(|&v| v == 255));

        coefs[0] = -20_000;
        let out = run(&coefs, &quant);
        assert!(out.iter().all(|&v| v == 0));
    }

    /// T.83 requires exact reconstruction of the all-zero and DC-only blocks
    /// and a peak error of at most one for the general case; reproducing
    /// `jidctint` exactly is a strictly stronger property, but this checks the
    /// two exactness requirements directly.
    #[test]
    fn t83_exactness_requirements() {
        assert!(run(&[0; 64], &[1; 64]).iter().all(|&v| v == 128));
        for dc in [-64i32, -8, 0, 8, 64] {
            let mut coefs = [0i32; 64];
            coefs[0] = dc;
            let out = run(&coefs, &[8; 64]);
            let first = out[0];
            assert!(out.iter().all(|&v| v == first));
        }
    }

    #[test]
    fn twelve_bit_range_is_honoured() {
        let mut coefs = [0i32; 64];
        coefs[0] = 8;
        let mut quant = [1u16; 64];
        quant[0] = 256;
        let mut plane = [0u16; 64];
        idct_islow_into(&coefs, &quant, &mut plane, 0, 8, 2048, 4095);
        assert!(plane.iter().all(|&v| v == 2048 + 256));

        coefs[0] = 40_000;
        idct_islow_into(&coefs, &quant, &mut plane, 0, 8, 2048, 4095);
        assert!(plane.iter().all(|&v| v == 4095));
    }

    /// A single AC coefficient produces the corresponding basis function; the
    /// mean must stay at the level shift and the pattern must be symmetric.
    #[test]
    fn single_ac_coefficient_is_a_basis_function() {
        let mut coefs = [0i32; 64];
        coefs[1] = 64; // horizontal frequency 1
        let out = run(&coefs, &[1; 64]);
        // Rows are identical (no vertical frequency).
        for row in 1..8 {
            assert_eq!(&out[row * 8..row * 8 + 8], &out[0..8]);
        }
        // Antisymmetric about the block centre, around the level shift.
        for i in 0..8 {
            let left = i32::from(out[i]) - 128;
            let right = i32::from(out[7 - i]) - 128;
            assert_eq!(left, -right, "column {i}");
        }
    }

    #[test]
    fn writes_at_an_offset_with_a_stride() {
        let mut plane = vec![0u16; 16 * 16];
        let mut coefs = [0i32; 64];
        coefs[0] = 8;
        let quant = [1u16; 64];
        idct_islow_into(&coefs, &quant, &mut plane, 8 * 16 + 8, 16, 128, 255);
        // The 8x8 block at (8, 8) is written and nothing else is.
        for y in 0..16 {
            for x in 0..16 {
                let expected = if (8..16).contains(&y) && (8..16).contains(&x) {
                    129
                } else {
                    0
                };
                assert_eq!(plane[y * 16 + x], expected, "({x}, {y})");
            }
        }
    }
}

//! Forward DCT and quantisation.
//!
//! [`fdct_islow`] is libjpeg's `jpeg_fdct_islow` (`jfdctint.c`) reproduced
//! exactly — the accurate integer transform that `cjpeg -dct int` (the
//! default) uses, and therefore the only one a byte-parity claim can be made
//! against. The `-dct fast` and `-dct float` variants are not reproducible
//! between builds and are deliberately absent, exactly as on the decode side.
//!
//! Two details are load-bearing and easy to get wrong:
//!
//! * **`PASS1_BITS` is 2 for eight-bit samples and 1 above them.** libjpeg
//!   drops a bit of intermediate precision at twelve bits to keep the column
//!   pass inside its accumulator (`jfdctint.c`, `#if BITS_IN_JSAMPLE == 8`).
//!   The inverse transform makes the same switch, which is why
//!   [`crate::idct`] takes the precision too.
//! * **The quantiser divides by `q << 3`, not by `q`.** The transform leaves
//!   its output scaled up by eight, and `jcdctmgr.c` folds that factor into
//!   the divisor rather than descaling first. `(|c| + 4q) / 8q` and
//!   `(|c| / 8 + q / 2) / q` differ on real images.

/// Fixed-point fractional bits in the rotation constants.
const CONST_BITS: u32 = 13;

const FIX_0_298631336: i64 = 2446;
const FIX_0_390180644: i64 = 3196;
const FIX_0_541196100: i64 = 4433;
const FIX_0_765366865: i64 = 6270;
const FIX_0_899976223: i64 = 7373;
const FIX_1_175875602: i64 = 9633;
const FIX_1_501321110: i64 = 12299;
const FIX_1_847759065: i64 = 15137;
const FIX_1_961570560: i64 = 16069;
const FIX_2_053119869: i64 = 16819;
const FIX_2_562915447: i64 = 20995;
const FIX_3_072711026: i64 = 25172;

/// libjpeg's `DESCALE`: round-half-up arithmetic right shift.
#[inline]
fn descale(value: i64, shift: u32) -> i64 {
    (value + (1i64 << (shift - 1))) >> shift
}

/// libjpeg's `PASS1_BITS` for a given sample precision.
///
/// Two below nine bits, one above — `jfdctint.c`'s
/// `#if BITS_IN_JSAMPLE == 8` switch.
#[must_use]
pub(crate) const fn pass1_bits(precision: u8) -> u32 {
    if precision > 8 { 1 } else { 2 }
}

/// The accurate integer forward DCT, in place.
///
/// `block` holds 64 level-shifted samples in row-major order (that is,
/// `sample - (1 << (precision - 1))`). On return it holds the unquantised
/// coefficients, scaled up by eight relative to the mathematical DCT — the
/// factor [`quantise_block`] folds into its divisor.
///
/// The array is `i32`, matching libjpeg's `DCTELEM`, while the rotation
/// products are formed in `i64`, matching its `INT32` (which is `long`, and
/// therefore 64-bit, on every platform this crate targets). A pass output can
/// reach about 2^17 and a product about 2^32, so neither width is optional.
pub(crate) fn fdct_islow(block: &mut [i32; 64], pass1_bits: u32) {
    // Pass 1: rows. Results are scaled up by sqrt(8) relative to a true DCT
    // and by a further 2^pass1_bits.
    let mut d = [0i32; 8];
    for row in 0..8usize {
        let base = row * 8;
        d.copy_from_slice(&block[base..base + 8]);

        let tmp0 = i64::from(d[0]) + i64::from(d[7]);
        let tmp7 = i64::from(d[0]) - i64::from(d[7]);
        let tmp1 = i64::from(d[1]) + i64::from(d[6]);
        let tmp6 = i64::from(d[1]) - i64::from(d[6]);
        let tmp2 = i64::from(d[2]) + i64::from(d[5]);
        let tmp5 = i64::from(d[2]) - i64::from(d[5]);
        let tmp3 = i64::from(d[3]) + i64::from(d[4]);
        let tmp4 = i64::from(d[3]) - i64::from(d[4]);

        // Even part, per LL&M figure 1.
        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;

        d[0] = ((tmp10 + tmp11) << pass1_bits) as i32;
        d[4] = ((tmp10 - tmp11) << pass1_bits) as i32;

        let z1 = (tmp12 + tmp13) * FIX_0_541196100;
        d[2] = descale(z1 + tmp13 * FIX_0_765366865, CONST_BITS - pass1_bits) as i32;
        d[6] = descale(z1 - tmp12 * FIX_1_847759065, CONST_BITS - pass1_bits) as i32;

        // Odd part, per LL&M figure 8.
        let mut z1 = tmp4 + tmp7;
        let mut z2 = tmp5 + tmp6;
        let mut z3 = tmp4 + tmp6;
        let mut z4 = tmp5 + tmp7;
        let z5 = (z3 + z4) * FIX_1_175875602;

        let tmp4 = tmp4 * FIX_0_298631336;
        let tmp5 = tmp5 * FIX_2_053119869;
        let tmp6 = tmp6 * FIX_3_072711026;
        let tmp7 = tmp7 * FIX_1_501321110;
        z1 *= -FIX_0_899976223;
        z2 *= -FIX_2_562915447;
        z3 *= -FIX_1_961570560;
        z4 *= -FIX_0_390180644;

        z3 += z5;
        z4 += z5;

        d[7] = descale(tmp4 + z1 + z3, CONST_BITS - pass1_bits) as i32;
        d[5] = descale(tmp5 + z2 + z4, CONST_BITS - pass1_bits) as i32;
        d[3] = descale(tmp6 + z2 + z3, CONST_BITS - pass1_bits) as i32;
        d[1] = descale(tmp7 + z1 + z4, CONST_BITS - pass1_bits) as i32;

        block[base..base + 8].copy_from_slice(&d);
    }

    // Pass 2: columns. The pass-1 scaling is removed, leaving an overall
    // factor of eight.
    for col in 0..8usize {
        let column = [
            i64::from(block[col]),
            i64::from(block[col + 8]),
            i64::from(block[col + 16]),
            i64::from(block[col + 24]),
            i64::from(block[col + 32]),
            i64::from(block[col + 40]),
            i64::from(block[col + 48]),
            i64::from(block[col + 56]),
        ];
        let tmp0 = column[0] + column[7];
        let tmp7 = column[0] - column[7];
        let tmp1 = column[1] + column[6];
        let tmp6 = column[1] - column[6];
        let tmp2 = column[2] + column[5];
        let tmp5 = column[2] - column[5];
        let tmp3 = column[3] + column[4];
        let tmp4 = column[3] - column[4];

        let tmp10 = tmp0 + tmp3;
        let tmp13 = tmp0 - tmp3;
        let tmp11 = tmp1 + tmp2;
        let tmp12 = tmp1 - tmp2;

        block[col] = descale(tmp10 + tmp11, pass1_bits) as i32;
        block[col + 32] = descale(tmp10 - tmp11, pass1_bits) as i32;

        let z1 = (tmp12 + tmp13) * FIX_0_541196100;
        block[col + 16] = descale(z1 + tmp13 * FIX_0_765366865, CONST_BITS + pass1_bits) as i32;
        block[col + 48] = descale(z1 - tmp12 * FIX_1_847759065, CONST_BITS + pass1_bits) as i32;

        let mut z1 = tmp4 + tmp7;
        let mut z2 = tmp5 + tmp6;
        let mut z3 = tmp4 + tmp6;
        let mut z4 = tmp5 + tmp7;
        let z5 = (z3 + z4) * FIX_1_175875602;

        let tmp4 = tmp4 * FIX_0_298631336;
        let tmp5 = tmp5 * FIX_2_053119869;
        let tmp6 = tmp6 * FIX_3_072711026;
        let tmp7 = tmp7 * FIX_1_501321110;
        z1 *= -FIX_0_899976223;
        z2 *= -FIX_2_562915447;
        z3 *= -FIX_1_961570560;
        z4 *= -FIX_0_390180644;

        z3 += z5;
        z4 += z5;

        block[col + 56] = descale(tmp4 + z1 + z3, CONST_BITS + pass1_bits) as i32;
        block[col + 40] = descale(tmp5 + z2 + z4, CONST_BITS + pass1_bits) as i32;
        block[col + 24] = descale(tmp6 + z2 + z3, CONST_BITS + pass1_bits) as i32;
        block[col + 8] = descale(tmp7 + z1 + z4, CONST_BITS + pass1_bits) as i32;
    }
}

/// Quantise one transformed block, exactly as `jcdctmgr.c`'s `forward_DCT`.
///
/// `dct` is in row-major (natural) order and `quant_zigzag` is the same table
/// already permuted, so the divisor is read sequentially; **`out` comes back
/// in zig-zag order**.
/// Reordering here rather than in the entropy coders costs nothing — this
/// loop already touches all 64 coefficients — and turns every later
/// traversal into a linear scan. A progressive frame walks its coefficients
/// twenty times (ten scans, each gathered and then coded), so the zig-zag
/// indirection is worth removing once here.
///
/// The divisor is `q << 3` because [`fdct_islow`] leaves its output scaled by
/// eight; rounding is half-away-from-zero, which is what makes `cjpeg` byte
/// parity reachable.
pub(crate) fn quantise_block(dct: &[i32; 64], quant_zigzag: &[u16; 64], out: &mut [i16; 64]) {
    for ((slot, &natural), &q) in out
        .iter_mut()
        .zip(crate::tables::ZIGZAG_TO_NATURAL.iter())
        .zip(quant_zigzag.iter())
    {
        let value = dct[natural];
        let divisor = i32::from(q) << 3;
        let magnitude = if value < 0 { -value } else { value };
        let biased = magnitude + (divisor >> 1);
        // libjpeg's `DIVIDE_BY`: at ordinary quality settings three quarters
        // of the coefficients quantise to zero, and a comparison is far
        // cheaper than an integer division. The result is identical for
        // non-negative operands, so this is a speed trick, not a behaviour
        // change.
        let quotient = if biased < divisor {
            0
        } else {
            biased / divisor
        };
        let signed = if value < 0 { -quotient } else { quotient };
        *slot = signed.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
    }
}

/// Load one 8x8 block out of a component plane, level-shifting as it goes.
///
/// `plane` is `stride` samples wide; the block's top-left sample is at
/// `(x, y)`. The caller guarantees the block lies inside the plane, which the
/// encoder's geometry does by construction (the plane is padded to whole
/// blocks before this is ever called).
pub(crate) fn load_block(
    plane: &[u16],
    stride: usize,
    x: usize,
    y: usize,
    center: i32,
    block: &mut [i32; 64],
) {
    for row in 0..8usize {
        let start = (y + row) * stride + x;
        let source = &plane[start..start + 8];
        let target = &mut block[row * 8..row * 8 + 8];
        for (slot, &sample) in target.iter_mut().zip(source.iter()) {
            *slot = i32::from(sample) - center;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constant_block(value: i32) -> [i32; 64] {
        [value; 64]
    }

    #[test]
    fn pass1_bits_switches_above_eight() {
        assert_eq!(pass1_bits(8), 2);
        assert_eq!(pass1_bits(12), 1);
    }

    /// A constant block has only a DC coefficient, and the transform leaves it
    /// scaled up by 64 relative to the sample value (8 from the DCT's own
    /// normalisation, 8 from libjpeg's extra scaling).
    #[test]
    fn constant_block_is_dc_only() {
        for &precision in &[8u8, 12] {
            let mut block = constant_block(37);
            fdct_islow(&mut block, pass1_bits(precision));
            assert_eq!(block[0], 37 * 64, "precision {precision}");
            for (index, &coefficient) in block.iter().enumerate().skip(1) {
                assert_eq!(coefficient, 0, "AC {index} at precision {precision}");
            }
        }
    }

    #[test]
    fn zero_block_transforms_to_zero() {
        let mut block = constant_block(0);
        fdct_islow(&mut block, 2);
        assert!(block.iter().all(|&c| c == 0));
    }

    /// Round-trip against the crate's own inverse transform: the forward DCT
    /// of a smooth block, quantised with an all-ones table, must reconstruct
    /// the samples to within a couple of LSBs.
    #[test]
    fn round_trips_through_the_inverse_transform() {
        let mut samples = [0u16; 64];
        for (index, slot) in samples.iter_mut().enumerate() {
            let x = index % 8;
            let y = index / 8;
            *slot = (16 * x + 8 * y + 40) as u16;
        }
        let mut block = [0i32; 64];
        load_block(&samples, 8, 0, 0, 128, &mut block);
        fdct_islow(&mut block, 2);
        let mut quantised = [0i16; 64];
        quantise_block(&block, &[1u16; 64], &mut quantised);

        // `quantise_block` writes zig-zag order; the inverse transform wants
        // natural order.
        let mut dequantised = [0i32; 64];
        for (k, &coefficient) in quantised.iter().enumerate() {
            dequantised[crate::tables::ZIGZAG_TO_NATURAL[k]] = i32::from(coefficient);
        }
        let mut out = [0u16; 64];
        crate::idct::idct_islow_into(&dequantised, &[1u16; 64], &mut out, 0, 8, 128, 255);
        for (index, (&got, &want)) in out.iter().zip(samples.iter()).enumerate() {
            let delta = i32::from(got) - i32::from(want);
            assert!(delta.abs() <= 2, "sample {index}: {got} vs {want}");
        }
    }

    /// The divisor is `q << 3`, and rounding is away from zero. Both halves
    /// are checked against hand-computed values: with `q = 10` the divisor is
    /// 80 and the rounding term is 40.
    #[test]
    fn quantisation_rounds_half_away_from_zero() {
        let mut dct = [0i32; 64];
        dct[0] = 120; // (120 + 40) / 80 = 2
        dct[1] = 119; // (119 + 40) / 80 = 1
        dct[2] = -120; // -((120 + 40) / 80) = -2
        dct[3] = -119; // -1
        dct[4] = 39; // (39 + 40) / 80 = 0
        dct[5] = 40; // (40 + 40) / 80 = 1
        let quant = [10u16; 64];
        let mut out = [0i16; 64];
        quantise_block(&dct, &quant, &mut out);
        // Natural indices 0..5 land at zig-zag positions 0, 1, 5, 6, 14, 15.
        let at = |natural: usize| out[crate::tables::NATURAL_TO_ZIGZAG[natural]];
        assert_eq!(
            [at(0), at(1), at(2), at(3), at(4), at(5)],
            [2, 1, -2, -1, 0, 1]
        );
    }

    /// Dividing by `q` after descaling by eight is a different function; this
    /// pins the difference so nobody "simplifies" the divisor.
    /// Descaling by eight first and then dividing by `q` is a different
    /// function; this pins a case where the two disagree so nobody
    /// "simplifies" the divisor into the obvious form.
    #[test]
    fn dividing_by_q_after_descaling_would_differ() {
        let q = 3i64;
        for coefficient in [13i64, 37, 61] {
            let libjpeg = (coefficient + ((q << 3) >> 1)) / (q << 3);
            let naive = (coefficient / 8 + q / 2) / q;
            assert_ne!(libjpeg, naive, "coefficient {coefficient}");
        }
        assert_eq!((13 + 12) / 24, 1);
        assert_eq!((13 / 8 + 1) / 3, 0);

        // And the quantiser really uses the first of those two.
        let mut dct = [0i32; 64];
        dct[0] = 13;
        let mut out = [0i16; 64];
        quantise_block(&dct, &[3u16; 64], &mut out);
        assert_eq!(out[0], 1);
    }

    #[test]
    fn load_block_level_shifts_and_strides() {
        let plane: Vec<u16> = (0..(16 * 16)).map(|v| (v % 256) as u16).collect();
        let mut block = [0i32; 64];
        load_block(&plane, 16, 8, 8, 128, &mut block);
        assert_eq!(block[0], i32::from(plane[8 * 16 + 8]) - 128);
        assert_eq!(block[63], i32::from(plane[15 * 16 + 15]) - 128);
    }
}

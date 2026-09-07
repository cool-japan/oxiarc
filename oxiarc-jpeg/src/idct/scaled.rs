//! Reduced- and enlarged-size inverse DCT: [`DecodeOptions::scale`]'s
//! implementation.
//!
//! [`DecodeOptions::scale`]: crate::DecodeOptions::scale
//!
//! `idct_1x1_into`, `idct_2x2_into` and `idct_4x4_into` are bit-identical
//! ports of libjpeg's `jpeg_idct_1x1`/`_2x2`/`_4x4` (`jidctred.c`) — the same
//! Loeffler-derived integer transform as [`super::islow::idct_islow_into`],
//! simplified for an output that is smaller than the coefficient block.
//! Byte parity with `djpeg -dct int -scale M/8` for `M` in `{1, 2, 4}` (`8`
//! is the unscaled [`super::islow::idct_islow_into`] itself) depends on
//! matching `jidctred.c` exactly, including which input terms each reduced
//! transform simply never reads — that is not an omission to "complete", it
//! is the mathematical content of the reduction (a 4-point partial IDCT of
//! an 8-point block cannot depend on the frequency-4 term because the
//! reduction's own derivation eliminates it; libjpeg's comment for the 4x4
//! case states as much).
//!
//! `idct_general_into` is this crate's own kernel for the twelve `M` values
//! `{3, 5, 6, 7, 9..=16}`, none of which need byte parity (see
//! [`DecodeOptions::scale`]'s rustdoc). libjpeg hand-derives a dedicated
//! fixed-point kernel for each one in `jidctint.c` (`jpeg_idct_3x3` through
//! `jpeg_idct_16x16`, none of them under 100 lines); this module instead
//! evaluates the same *mathematical* transform directly from a precomputed
//! cosine basis, in `f64`, over one shared implementation instead of twelve.
//!
//! **`jidctred.c`'s `{1, 2, 4}` and `jidctint.c`'s `{3, 5, 6, 7}` are not the
//! same algorithm**, discovered the hard way (an early version of this
//! kernel used `jidctred.c`'s approach uniformly and measured a peak error
//! of 134 against `djpeg` before this was found): `jidctred.c` resamples
//! *all 8* frequencies at `M` output positions (only `M` in `{1, 2, 4}`
//! happens to make one particular frequency's contribution cancel to
//! exactly zero, which is why those functions each skip one input term —
//! confirmed by reading `jidctint.c`'s `_jpeg_idct_9x9`, which for `M = 9`
//! reads all 8 input frequencies with nothing to cancel). `jidctint.c`'s `M
//! < 8` family instead *truncates* to the lowest `M` frequencies per axis
//! and runs a genuine `M`-point inverse transform of just those — a
//! brick-wall low-pass filter in the DCT domain before decimating, standard
//! practice for downscaling, and a real algorithmic choice, not an
//! approximation. `idct_general_into` follows `jidctint.c`'s convention
//! (truncate for `n < 8`, use all 8 for `n >= 8`, confirmed against `_jpeg_
//! idct_3x3`'s and `_jpeg_idct_9x9`'s actual coefficient reads) because that
//! is what `djpeg -scale M/8` actually produces for those `M`. It therefore
//! does **not**, and is not expected to, agree with the ported `jidctred.c`
//! kernels above on an AC-heavy block at `n` in `{1, 2, 4}` — only on a
//! DC-only one, where truncation cannot matter either way (see this
//! module's tests).

use std::sync::OnceLock;

use super::islow::{coefficient, descale};

// ---------------------------------------------------------------------
// Exact ports of jidctred.c
// ---------------------------------------------------------------------

const CONST_BITS: u32 = 13;

const FIX_0_211164243: i64 = 1730;
const FIX_0_509795579: i64 = 4176;
const FIX_0_601344887: i64 = 4926;
const FIX_0_720959822: i64 = 5906;
const FIX_0_765366865: i64 = 6270;
const FIX_0_850430095: i64 = 6967;
const FIX_0_899976223: i64 = 7373;
const FIX_1_061594337: i64 = 8697;
const FIX_1_272758580: i64 = 10426;
const FIX_1_451774981: i64 = 11893;
const FIX_1_847759065: i64 = 15137;
const FIX_2_172734803: i64 = 17799;
const FIX_2_562915447: i64 = 20995;
const FIX_3_624509785: i64 = 29692;

/// `jpeg_idct_4x4`: dequantise and inverse-DCT one block into a 4x4 output.
///
/// `coefs`/`quant` are natural order; `stride` is the destination plane's row
/// length in samples. Writes exactly the 4x4 rectangle at `(col, row)` =
/// `offset`.
pub(crate) fn idct_4x4_into(
    coefs: &[i32; 64],
    quant: &[u16; 64],
    plane: &mut [u16],
    offset: usize,
    stride: usize,
    center: i32,
    maxval: i32,
) {
    let pass1_bits: u32 = if maxval > 255 { 1 } else { 2 };
    // `workspace[row * 8 + col]`, but only columns 0,1,2,3,5,6,7 of rows 0..4
    // are ever written or read — column 4 is structurally dead (see the
    // module doc).
    let mut workspace = [0i32; 32];

    for col in [0usize, 1, 2, 3, 5, 6, 7] {
        let c = |k: usize| coefficient(coefs[col + k * 8]);
        let q = |k: usize| i32::from(quant[col + k * 8]);

        if c(1) == 0 && c(2) == 0 && c(3) == 0 && c(5) == 0 && c(6) == 0 && c(7) == 0 {
            let dcval = (c(0) * q(0)) << pass1_bits;
            workspace[col] = dcval;
            workspace[col + 8] = dcval;
            workspace[col + 16] = dcval;
            workspace[col + 24] = dcval;
            continue;
        }

        let tmp0 = i64::from(c(0) * q(0)) << (CONST_BITS + 1);
        let z2 = i64::from(c(2) * q(2));
        let z3 = i64::from(c(6) * q(6));
        let tmp2 = z2 * FIX_1_847759065 + z3 * -FIX_0_765366865;
        let tmp10 = tmp0 + tmp2;
        let tmp12 = tmp0 - tmp2;

        let z1 = i64::from(c(7) * q(7));
        let z2 = i64::from(c(5) * q(5));
        let z3 = i64::from(c(3) * q(3));
        let z4 = i64::from(c(1) * q(1));
        let odd0 = z1 * -FIX_0_211164243
            + z2 * FIX_1_451774981
            + z3 * -FIX_2_172734803
            + z4 * FIX_1_061594337;
        let odd2 = z1 * -FIX_0_509795579
            + z2 * -FIX_0_601344887
            + z3 * FIX_0_899976223
            + z4 * FIX_2_562915447;

        let shift = CONST_BITS - pass1_bits + 1;
        workspace[col] = descale(tmp10 + odd2, shift);
        workspace[col + 24] = descale(tmp10 - odd2, shift);
        workspace[col + 8] = descale(tmp12 + odd0, shift);
        workspace[col + 16] = descale(tmp12 - odd0, shift);
    }

    for row in 0..4 {
        let w = &workspace[row * 8..row * 8 + 8];
        let out = &mut plane[offset + row * stride..offset + row * stride + 4];

        if w[1] == 0 && w[2] == 0 && w[3] == 0 && w[5] == 0 && w[6] == 0 && w[7] == 0 {
            let value = descale(i64::from(w[0]), pass1_bits + 3);
            let sample = value.saturating_add(center).clamp(0, maxval) as u16;
            out.fill(sample);
            continue;
        }

        let tmp0 = i64::from(w[0]) << (CONST_BITS + 1);
        let tmp2 = i64::from(w[2]) * FIX_1_847759065 + i64::from(w[6]) * -FIX_0_765366865;
        let tmp10 = tmp0 + tmp2;
        let tmp12 = tmp0 - tmp2;

        let z1 = i64::from(w[7]);
        let z2 = i64::from(w[5]);
        let z3 = i64::from(w[3]);
        let z4 = i64::from(w[1]);
        let odd0 = z1 * -FIX_0_211164243
            + z2 * FIX_1_451774981
            + z3 * -FIX_2_172734803
            + z4 * FIX_1_061594337;
        let odd2 = z1 * -FIX_0_509795579
            + z2 * -FIX_0_601344887
            + z3 * FIX_0_899976223
            + z4 * FIX_2_562915447;

        let shift = CONST_BITS + pass1_bits + 4;
        let limit = |v: i64| descale(v, shift).saturating_add(center).clamp(0, maxval) as u16;
        out[0] = limit(tmp10 + odd2);
        out[3] = limit(tmp10 - odd2);
        out[1] = limit(tmp12 + odd0);
        out[2] = limit(tmp12 - odd0);
    }
}

/// `jpeg_idct_2x2`: dequantise and inverse-DCT one block into a 2x2 output.
pub(crate) fn idct_2x2_into(
    coefs: &[i32; 64],
    quant: &[u16; 64],
    plane: &mut [u16],
    offset: usize,
    stride: usize,
    center: i32,
    maxval: i32,
) {
    let pass1_bits: u32 = if maxval > 255 { 1 } else { 2 };
    // `workspace[row * 8 + col]`; only columns 0,1,3,5,7 of rows 0..2 matter.
    let mut workspace = [0i32; 16];

    for col in [0usize, 1, 3, 5, 7] {
        let c = |k: usize| coefficient(coefs[col + k * 8]);
        let q = |k: usize| i32::from(quant[col + k * 8]);

        if c(1) == 0 && c(3) == 0 && c(5) == 0 && c(7) == 0 {
            let dcval = (c(0) * q(0)) << pass1_bits;
            workspace[col] = dcval;
            workspace[col + 8] = dcval;
            continue;
        }

        let tmp10 = i64::from(c(0) * q(0)) << (CONST_BITS + 2);
        let z1 = i64::from(c(7) * q(7));
        let mut tmp0 = z1 * -FIX_0_720959822;
        let z1 = i64::from(c(5) * q(5));
        tmp0 += z1 * FIX_0_850430095;
        let z1 = i64::from(c(3) * q(3));
        tmp0 += z1 * -FIX_1_272758580;
        let z1 = i64::from(c(1) * q(1));
        tmp0 += z1 * FIX_3_624509785;

        let shift = CONST_BITS - pass1_bits + 2;
        workspace[col] = descale(tmp10 + tmp0, shift);
        workspace[col + 8] = descale(tmp10 - tmp0, shift);
    }

    for row in 0..2 {
        let w = &workspace[row * 8..row * 8 + 8];
        let out = &mut plane[offset + row * stride..offset + row * stride + 2];

        if w[1] == 0 && w[3] == 0 && w[5] == 0 && w[7] == 0 {
            let value = descale(i64::from(w[0]), pass1_bits + 3);
            let sample = value.saturating_add(center).clamp(0, maxval) as u16;
            out.fill(sample);
            continue;
        }

        let tmp10 = i64::from(w[0]) << (CONST_BITS + 2);
        let tmp0 = i64::from(w[7]) * -FIX_0_720959822
            + i64::from(w[5]) * FIX_0_850430095
            + i64::from(w[3]) * -FIX_1_272758580
            + i64::from(w[1]) * FIX_3_624509785;

        let shift = CONST_BITS + pass1_bits + 5;
        let limit = |v: i64| descale(v, shift).saturating_add(center).clamp(0, maxval) as u16;
        out[0] = limit(tmp10 + tmp0);
        out[1] = limit(tmp10 - tmp0);
    }
}

/// `jpeg_idct_1x1`: the average sample value, `DC / 8`. libjpeg's variant has
/// no `PASS1_BITS` shift at all — there is only one pass, and it is a plain
/// `DESCALE(dc, 3)`.
pub(crate) fn idct_1x1_into(
    coefs: &[i32; 64],
    quant: &[u16; 64],
    plane: &mut [u16],
    offset: usize,
    _stride: usize,
    center: i32,
    maxval: i32,
) {
    let dc = coefficient(coefs[0]) * i32::from(quant[0]);
    let value = descale(i64::from(dc), 3);
    plane[offset] = value.saturating_add(center).clamp(0, maxval) as u16;
}

// ---------------------------------------------------------------------
// General kernel: every other M/8 (M in 3, 5, 6, 7, 9..=16)
// ---------------------------------------------------------------------

/// `basis[u][i] = C(u) * cos((2i+1) * u * pi / (2n))` for frequency `u` in
/// `0..8` and output position `i` in `0..n` (`0..16` are computed; only the
/// first `n` are ever read), `C(0) = 1/sqrt(2)`, `C(u) = 1` otherwise. T.81
/// A.3.3's cosine basis, sampled at `n` output positions per axis; the DC
/// row (`u == 0`) is identical for every `n`, which is why the DC
/// contribution to [`idct_general_into`]'s output does not depend on `n`
/// (see this module's tests) regardless of how many of the remaining rows
/// [`idct_general_into`] actually sums over.
fn cosine_basis(n: usize) -> [[f64; 16]; 8] {
    let mut basis = [[0f64; 16]; 8];
    for (u, row) in basis.iter_mut().enumerate() {
        let scale = if u == 0 {
            std::f64::consts::FRAC_1_SQRT_2
        } else {
            1.0
        };
        for (i, slot) in row.iter_mut().take(n).enumerate() {
            let angle = (2 * i + 1) as f64 * u as f64 * std::f64::consts::PI / (2.0 * n as f64);
            *slot = scale * angle.cos();
        }
    }
    basis
}

/// [`cosine_basis`] for every `n` in `1..=16`, evaluated once per process.
///
/// The basis depends only on `n`, so recomputing it inside
/// [`idct_general_into`] evaluated 128 `f64::cos` calls for **every 8x8
/// block** — for a `512x512` 4:2:0 frame at `M = 3` that is roughly 786 000
/// transcendental calls, hundreds of times the arithmetic of the transform
/// they feed. Hoisting them into a `OnceLock` table (17 x 8 x 16 `f64`, about
/// 17 KiB, index `0` unused so `n` indexes directly) is bit-identical by
/// construction: the same `cosine_basis(n)` values, computed once instead of
/// per block, summed in exactly the same order.
fn basis_table() -> &'static [[[f64; 16]; 8]; 17] {
    static TABLE: OnceLock<[[[f64; 16]; 8]; 17]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [[[0f64; 16]; 8]; 17];
        for (n, slot) in table.iter_mut().enumerate().skip(1) {
            *slot = cosine_basis(n);
        }
        table
    })
}

/// This crate's kernel for the `M` values libjpeg gives their own dedicated
/// fixed-point routine and this crate does not: `{3, 5, 6, 7, 9..=16}` — see
/// the module doc and [`crate::Scale`]'s rustdoc for what "byte parity" means
/// for these values (a numeric tolerance against `djpeg`, not exact).
///
/// Same two-pass separable structure as every kernel above, and the same
/// `coefficient()` narrowing on the way in; the two passes just share one
/// `n`-point building block (evaluated in `f64`) instead of each having a
/// hand-unrolled fixed-point one.
#[allow(clippy::needless_range_loop, clippy::too_many_arguments)]
pub(crate) fn idct_general_into(
    coefs: &[i32; 64],
    quant: &[u16; 64],
    plane: &mut [u16],
    offset: usize,
    stride: usize,
    center: i32,
    maxval: i32,
    n: u8,
) {
    let n = usize::from(n).clamp(1, 16);
    // libjpeg's own `M < 8` kernels (`jidctint.c`'s `jpeg_idct_3x3` etc.,
    // confirmed by reading the source: `for (ctr = 0; ctr < 3; ...)` walks
    // only 3 of the 8 input columns, and each pass reads only
    // `inptr[DCTSIZE*0..2]`) do not resample the full 8-frequency block —
    // they *truncate* it to the lowest `M` frequencies per axis and run a
    // genuine `M`-point inverse transform of just those, discarding the
    // rest. That is a real algorithmic choice (a brick-wall low-pass filter
    // in the DCT domain before decimating, standard signal-processing
    // practice for downscaling), not an approximation of the full-spectrum
    // resampling `jidctred.c`'s `M` in `{1, 2, 4}` happens to reduce to.
    // `M > 8` has no extra frequencies to discard, so it necessarily uses
    // all 8 (confirmed the same way: `jpeg_idct_9x9`'s pass 1 reads
    // `inptr[DCTSIZE*0..7]`). Mirroring which frequencies contribute is what
    // brings this kernel from "a different but plausible reconstruction"
    // (peak error over 100 against `djpeg`, measured) down to a few LSBs.
    let freqs = n.min(8);
    // `n` is clamped to `1..=16` above, so this index is always in range.
    let basis = &basis_table()[n];

    // Pass 1: for each of the (at most 8) input columns actually used,
    // an n-point partial inverse transform along the row
    // (vertical-frequency) axis, written into an `n`-row-by-8-column
    // workspace (flattened, row-major, only the first `n` rows and
    // `freqs` columns are ever populated or read).
    let mut workspace = [0f64; 16 * 8];
    for col in 0..freqs {
        let mut x = [0f64; 8];
        for u in 0..freqs {
            x[u] = f64::from(coefficient(coefs[col + u * 8]) * i32::from(quant[col + u * 8]));
        }
        for row in 0..n {
            let value: f64 = (0..freqs).map(|u| basis[u][row] * x[u]).sum();
            workspace[row * 8 + col] = 0.5 * value;
        }
    }

    // Pass 2: for each of the `n` intermediate rows, an n-point partial
    // inverse transform along the column (horizontal-frequency) axis.
    for row in 0..n {
        let w = &workspace[row * 8..row * 8 + 8];
        let out = &mut plane[offset + row * stride..offset + row * stride + n];
        for (col, slot) in out.iter_mut().enumerate() {
            let value: f64 = (0..freqs).map(|u| basis[u][col] * w[u]).sum();
            let sample = (0.5 * value).round() as i32;
            *slot = sample.saturating_add(center).clamp(0, maxval) as u16;
        }
    }
}

/// Dispatch to whichever kernel reconstructs `output_size x output_size`
/// samples from one `8x8` coefficient block: the untouched, always-present
/// [`super::islow::idct_islow_into`] at `8` (native resolution, unaffected
/// by anything in this module), the three ported [`jidctred.c`] kernels at
/// `1`, `2` and `4`, and [`idct_general_into`] otherwise.
///
/// [`jidctred.c`]: https://github.com/libjpeg-turbo/libjpeg-turbo/blob/main/src/jidctred.c
#[allow(clippy::too_many_arguments)]
pub(crate) fn idct_scaled_into(
    coefs: &[i32; 64],
    quant: &[u16; 64],
    plane: &mut [u16],
    offset: usize,
    stride: usize,
    center: i32,
    maxval: i32,
    output_size: u8,
) {
    match output_size {
        8 => super::idct_islow_into(coefs, quant, plane, offset, stride, center, maxval),
        1 => idct_1x1_into(coefs, quant, plane, offset, stride, center, maxval),
        2 => idct_2x2_into(coefs, quant, plane, offset, stride, center, maxval),
        4 => idct_4x4_into(coefs, quant, plane, offset, stride, center, maxval),
        n => idct_general_into(coefs, quant, plane, offset, stride, center, maxval, n),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic corpus: all-zero, DC-only, one AC term, and two
    /// "dense" blocks with different sign patterns. Fixed rather than random
    /// so a failure is reproducible without a seed.
    fn sample_blocks() -> Vec<[i32; 64]> {
        let mut blocks = vec![[0i32; 64]];

        let mut dc_only = [0i32; 64];
        dc_only[0] = 40;
        blocks.push(dc_only);

        let mut single_ac = [0i32; 64];
        single_ac[0] = 16;
        single_ac[9] = 12;
        blocks.push(single_ac);

        let mut ramp = [0i32; 64];
        for (i, slot) in ramp.iter_mut().enumerate() {
            *slot = (i as i32 * 37 % 61) - 30;
        }
        blocks.push(ramp);

        let mut checker = [0i32; 64];
        for (i, slot) in checker.iter_mut().enumerate() {
            *slot = if i % 2 == 0 { 25 } else { -18 };
        }
        blocks.push(checker);

        blocks
    }

    fn render(coefs: &[i32; 64], quant: &[u16; 64], n: u8, center: i32, maxval: i32) -> Vec<i32> {
        let size = usize::from(n);
        let mut plane = vec![0u16; size * size];
        idct_scaled_into(coefs, quant, &mut plane, 0, size, center, maxval, n);
        plane.into_iter().map(i32::from).collect()
    }

    /// [`idct_general_into`] directly, bypassing [`idct_scaled_into`]'s
    /// dispatch — needed because the dispatcher routes every `n` in
    /// `{1, 2, 4, 8}` to a *different* kernel, so a test that goes through
    /// [`render`] at one of those sizes never actually reaches this module's
    /// general kernel at all (an earlier version of this file's tests made
    /// exactly that mistake: both sides of a "does the general kernel agree
    /// with the exact one" comparison called `render`, so both sides took
    /// the exact-kernel branch and the comparison passed by never exercising
    /// [`idct_general_into`]).
    fn render_general(
        coefs: &[i32; 64],
        quant: &[u16; 64],
        n: u8,
        center: i32,
        maxval: i32,
    ) -> Vec<i32> {
        let size = usize::from(n);
        let mut plane = vec![0u16; size * size];
        idct_general_into(coefs, quant, &mut plane, 0, size, center, maxval, n);
        plane.into_iter().map(i32::from).collect()
    }

    /// The DC contribution to every pass of [`idct_general_into`] is
    /// independent of the output size `n` (the module doc derives why: two
    /// `C(0) = 1/sqrt(2)` factors and two `1/2` pass prefactors combine to
    /// exactly `1/8` regardless of `n`), so a DC-only block must reconstruct
    /// to the flat level `dc_dequantised / 8` for every `n` in `1..=16` —
    /// the same level [`idct_1x1_into`]'s one-line formula computes
    /// directly. Chosen `dc * quant` products are exact multiples of 8 so
    /// the comparison needs no tolerance: `descale`'s round-half-up and
    /// `f64::round`'s round-half-away-from-zero conventions only disagree on
    /// a negative exact half-integer, which an exact multiple of 8 can never
    /// be.
    #[test]
    fn general_kernel_dc_level_is_independent_of_output_size() {
        for (dc, quant_dc) in [(8i32, 8u16), (-8, 8), (100, 8), (-50, 16), (30, 4)] {
            let mut coefs = [0i32; 64];
            coefs[0] = dc;
            let mut quant = [1u16; 64];
            quant[0] = quant_dc;
            let expected = (dc * i32::from(quant_dc) / 8 + 128).clamp(0, 255);

            for n in 1u8..=16 {
                let out = render(&coefs, &quant, n, 128, 255);
                assert!(
                    out.iter().all(|&v| v == expected),
                    "n={n} dc={dc} quant={quant_dc}: {out:?} (want all {expected})"
                );
            }
        }
    }

    /// [`idct_general_into`] at `n = 8` must agree with the byte-parity
    /// [`super::super::islow::idct_islow_into`] (reached through
    /// [`idct_scaled_into`]'s own `8` arm for the right-hand side) to within
    /// one LSB: they compute the same mathematical transform two different
    /// ways, so a real bug in either shows up as a large, not marginal,
    /// disagreement.
    #[test]
    fn general_kernel_agrees_with_islow_within_one_lsb_at_n8() {
        let quant = [3u16; 64];
        for coefs in sample_blocks() {
            let mut islow_plane = [0u16; 64];
            idct_scaled_into(&coefs, &quant, &mut islow_plane, 0, 8, 128, 255, 8);
            let general = render_general(&coefs, &quant, 8, 128, 255);
            for (i, (&islow, &computed)) in islow_plane.iter().zip(general.iter()).enumerate() {
                let delta = i32::from(islow) - computed;
                assert!(
                    delta.abs() <= 1,
                    "index {i}: islow={islow} general={computed}"
                );
            }
        }
    }

    /// [`idct_general_into`] (called directly, *not* through
    /// [`idct_scaled_into`]'s dispatch — `n` in `{1, 2, 4}` never reaches it
    /// there) deliberately does **not** match [`idct_1x1_into`] /
    /// [`idct_2x2_into`] / [`idct_4x4_into`] on an AC-heavy block at those
    /// sizes: the module doc explains why — the ported `jidctred.c` kernels
    /// resample all 8 frequencies (with an algebraic cancellation specific
    /// to `n` dividing 8 evenly), while [`idct_general_into`] truncates to
    /// the lowest `n` frequencies for `n < 8`, matching `jidctint.c`'s own
    /// separate `n < 8` family instead. This test pins that the two *do*
    /// still agree on the one input where truncation cannot matter — a
    /// DC-only block, where there are no higher frequencies to disagree
    /// about — and records (via the assertion message, should it ever start
    /// passing) that they are not expected to agree in general so nobody
    /// "fixes" that difference away by accident.
    #[test]
    fn general_kernel_matches_ported_kernels_only_where_truncation_cannot_matter() {
        let mut quant = [1u16; 64];
        quant[0] = 6;
        let mut dc_only = [0i32; 64];
        dc_only[0] = 24;

        for n in [1u8, 2, 4] {
            let ported = render(&dc_only, &quant, n, 128, 255);
            let general = render_general(&dc_only, &quant, n, 128, 255);
            assert_eq!(ported, general, "n={n}: DC-only blocks must still agree");
        }

        // An AC-heavy block is *not* required to agree — confirmed, not
        // merely assumed, so a future change to either family that happens
        // to make them coincide is visible here rather than silently
        // relied upon.
        let ac_heavy = sample_blocks()[3]; // the "ramp" block
        let mut any_difference = false;
        for n in [1u8, 2, 4] {
            let ported = render(&ac_heavy, &quant, n, 128, 255);
            let general = render_general(&ac_heavy, &quant, n, 128, 255);
            any_difference |= ported != general;
        }
        assert!(
            any_difference,
            "the two families reconstructed an AC-heavy block identically at every n in {{1,2,4}}; \
             if that is now expected, replace this assertion with the specific reason"
        );
    }

    /// The most extreme coefficient/quantiser combination the `coefficient()`
    /// narrowing admits (T.81 B.2.4.1's `Pq = 1` allows quantisers up to
    /// `0xFFFF`), at every output size, must not panic — the boundary this
    /// track's `saturating_add` defends. Mirrors the spirit of the F-verify
    /// track's D2 fix for `idct_islow_into`, generalised to every kernel this
    /// module adds.
    #[test]
    fn extreme_coefficients_never_panic_at_any_output_size() {
        let quant = [u16::MAX; 64];
        for dc in [i32::MIN, i32::MAX, 32_767, -32_768, 65_534, -65_534] {
            let mut coefs = [0i32; 64];
            coefs[0] = dc;
            coefs[9] = dc;
            coefs[18] = dc;
            for n in 1u8..=16 {
                let size = usize::from(n);
                let mut plane = vec![0u16; size * size];
                idct_scaled_into(&coefs, &quant, &mut plane, 0, size, 128, 255, n);
                assert!(plane.iter().all(|&v| v <= 255), "n={n} dc={dc}: {plane:?}");
            }
        }
    }

    /// [`idct_1x1_into`], [`idct_2x2_into`] and [`idct_4x4_into`] each have
    /// their own all-zero and DC-only shortcut path, pinned directly (not
    /// only through the general-kernel cross-check above) exactly as
    /// `idct_islow_into`'s own unit tests pin its shortcuts.
    /// The `OnceLock` table [`idct_general_into`] reads must hold exactly
    /// what [`cosine_basis`] computes, bit for bit, for every supported `n`.
    ///
    /// The hoist out of the per-block path is only sound because the two are
    /// the same numbers; `assert_eq!` on `f64` is deliberate here (not an
    /// epsilon comparison) because anything short of bit equality would move
    /// the decoder's output.
    #[test]
    fn the_memoised_basis_table_is_bit_identical_to_computing_it_per_call() {
        for n in 1usize..=16 {
            assert_eq!(
                basis_table()[n],
                cosine_basis(n),
                "n={n}: the memoised basis drifted from cosine_basis"
            );
        }
    }

    #[test]
    fn ported_kernels_reproduce_the_level_shift_on_an_all_zero_block() {
        let quant = [1u16; 64];
        for n in [1u8, 2, 4] {
            let out = render(&[0i32; 64], &quant, n, 128, 255);
            assert!(out.iter().all(|&v| v == 128), "n={n}: {out:?}");
        }
    }

    #[test]
    fn ported_kernels_agree_on_a_dc_only_block_exactly() {
        let mut coefs = [0i32; 64];
        coefs[0] = 8;
        let mut quant = [1u16; 64];
        quant[0] = 16;
        // 8 * 16 / 8 = 16.
        for n in [1u8, 2, 4] {
            let out = render(&coefs, &quant, n, 128, 255);
            assert!(out.iter().all(|&v| v == 144), "n={n}: {out:?}");
        }
    }

    #[test]
    fn ported_kernels_clamp_rather_than_wrap() {
        let quant = [255u16; 64];
        for n in [1u8, 2, 4] {
            let mut coefs = [0i32; 64];
            coefs[0] = 20_000;
            let out = render(&coefs, &quant, n, 128, 255);
            assert!(out.iter().all(|&v| v == 255), "n={n}: {out:?}");

            coefs[0] = -20_000;
            let out = render(&coefs, &quant, n, 128, 255);
            assert!(out.iter().all(|&v| v == 0), "n={n}: {out:?}");
        }
    }
}

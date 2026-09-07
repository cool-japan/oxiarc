//! The Paeth predictor, in three provably equivalent formulations.
//!
//! * [`paeth_spec`] is the literal form from ISO/IEC 15948 clause 9.4. It is
//!   used as the test oracle and nowhere else.
//! * [`paeth_stbi`] is a branch-free threshold form. It has the shortest
//!   dependency chain and is what the *decode* kernels use.
//! * [`paeth_fpnge`] works entirely on unsigned 8-bit quantities, which is what
//!   an autovectoriser wants; it is used by the *encode* kernels.
//!
//! `filter::paeth::tests::all_three_forms_agree` proves the three equal over
//! all 2^24 inputs.

/// The literal specification form.
///
/// ```
/// use oxiarc_png::filter::paeth_spec;
/// // With no left or upper-left neighbour the predictor degenerates to `b`.
/// assert_eq!(paeth_spec(0, 42, 0), 42);
/// ```
#[must_use]
#[inline]
pub fn paeth_spec(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// The branch-free threshold form used by the decode kernels.
#[must_use]
#[inline(always)]
pub fn paeth_stbi(a: u8, b: u8, c: u8) -> u8 {
    let (a, b, c) = (i16::from(a), i16::from(b), i16::from(c));
    let thresh = c * 3 - (a + b);
    let lo = a.min(b);
    let hi = a.max(b);
    let t0 = if hi <= thresh { lo } else { c };
    let t1 = if thresh <= lo { hi } else { t0 };
    t1 as u8
}

/// The all-unsigned form used by the encode kernels.
#[must_use]
#[inline(always)]
pub fn paeth_fpnge(a: u8, b: u8, c: u8) -> u8 {
    // |b - c| and |a - c| without sign extension.
    let pa = b.max(c) - b.min(c);
    let pb = a.max(c) - a.min(c);
    // `pc = |(b - c) + (a - c)|`. When `c` is a strict outlier relative to `a`
    // and `b`, both differences have the same sign and `pc` is provably larger
    // than both `pa` and `pb`, so any saturating stand-in works; otherwise the
    // two differences have opposite signs and `pc` is their absolute
    // difference.
    let pc = if (a < c) == (c < b) {
        pa.max(pb) - pa.min(pb)
    } else {
        u8::MAX
    };
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_three_forms_agree() {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                for c in 0..=255u8 {
                    let want = paeth_spec(a, b, c);
                    assert_eq!(paeth_stbi(a, b, c), want, "stbi at ({a},{b},{c})");
                    assert_eq!(paeth_fpnge(a, b, c), want, "fpnge at ({a},{b},{c})");
                }
            }
        }
    }

    #[test]
    fn known_values() {
        assert_eq!(paeth_spec(0, 0, 0), 0);
        assert_eq!(paeth_spec(10, 20, 30), 10);
        // p = 150; pa = pb = 50, pc = 0, so the upper-left neighbour wins.
        assert_eq!(paeth_spec(200, 100, 150), 150);
        // Ties resolve to `a`, then `b`, then `c`.
        assert_eq!(paeth_spec(5, 5, 5), 5);
        assert_eq!(paeth_spec(1, 2, 3), 1);
    }
}

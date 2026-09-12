//! YCCK and CMYK output, reproducing libjpeg's `ycck_cmyk_convert`.
//!
//! # The Adobe inversion, and why it is conditional
//!
//! libjpeg's YCCK conversion is `C = MAX - R`, `M = MAX - G`, `Y = MAX - B`
//! with `K` passed through, and its CMYK conversion is the identity. On top of
//! that, Adobe's own writers store all four channels inverted, which is why
//! Pillow reads any four-component JPEG through its `CMYK;I` raw mode.
//!
//! `libtiff`'s `Compression = 7` CMYK strips carry **no** `APP14` marker and
//! are **not** inverted (verified against `tiffcp -c jpeg` output on a CMYK
//! source, whose `K = 40` reads back as `K = 40`). Applying the inversion
//! unconditionally would therefore corrupt every JPEG-in-TIFF CMYK image, and
//! never applying it would invert every Photoshop CMYK JPEG. This module keys
//! the inversion on the presence of the `Adobe` `APP14` marker, which gets
//! both right; `DecodeOptions::raw_components` bypasses the question entirely
//! and is what `oxiarc-tiff` uses.

use super::ycbcr::YcbcrTables;

/// Convert one YCCK pixel to CMYK.
#[inline]
pub(crate) fn ycck_to_cmyk(
    tables: &YcbcrTables,
    y: u16,
    cb: u16,
    cr: u16,
    k: u16,
    invert: bool,
) -> [u16; 4] {
    let maxval = tables.maxval();
    let (r, g, b) = tables.to_rgb(y, cb, cr);
    let c = maxval - i32::from(r);
    let m = maxval - i32::from(g);
    let yy = maxval - i32::from(b);
    let kk = i32::from(k).clamp(0, maxval);
    finish([c, m, yy, kk], maxval, invert)
}

/// Pass a CMYK pixel through, applying the Adobe inversion if asked.
#[inline]
pub(crate) fn cmyk_passthrough(maxval: i32, pixel: [u16; 4], invert: bool) -> [u16; 4] {
    finish(
        [
            i32::from(pixel[0]).clamp(0, maxval),
            i32::from(pixel[1]).clamp(0, maxval),
            i32::from(pixel[2]).clamp(0, maxval),
            i32::from(pixel[3]).clamp(0, maxval),
        ],
        maxval,
        invert,
    )
}

#[inline]
fn finish(values: [i32; 4], maxval: i32, invert: bool) -> [u16; 4] {
    let mut out = [0u16; 4];
    for (slot, value) in out.iter_mut().zip(values) {
        let v = if invert { maxval - value } else { value };
        *slot = v.clamp(0, maxval) as u16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ycck_neutral_is_full_ink_complement() {
        let tables = YcbcrTables::new(8);
        // Y = 255, neutral chroma -> RGB white -> CMY zero.
        assert_eq!(
            ycck_to_cmyk(&tables, 255, 128, 128, 10, false),
            [0, 0, 0, 10]
        );
        // Y = 0 -> RGB black -> CMY full.
        assert_eq!(
            ycck_to_cmyk(&tables, 0, 128, 128, 200, false),
            [255, 255, 255, 200]
        );
    }

    #[test]
    fn ycck_matches_libjpeg_max_minus_rgb() {
        let tables = YcbcrTables::new(8);
        let (r, g, b) = tables.to_rgb(90, 40, 210);
        let expected = [255 - r, 255 - g, 255 - b, 77];
        assert_eq!(ycck_to_cmyk(&tables, 90, 40, 210, 77, false), expected);
    }

    #[test]
    fn adobe_inversion_is_an_involution() {
        let tables = YcbcrTables::new(8);
        let plain = ycck_to_cmyk(&tables, 90, 40, 210, 77, false);
        let inverted = ycck_to_cmyk(&tables, 90, 40, 210, 77, true);
        for i in 0..4 {
            assert_eq!(u32::from(plain[i]) + u32::from(inverted[i]), 255);
        }
    }

    #[test]
    fn cmyk_passthrough_is_the_identity_without_adobe() {
        assert_eq!(
            cmyk_passthrough(255, [10, 20, 30, 40], false),
            [10, 20, 30, 40]
        );
        assert_eq!(
            cmyk_passthrough(255, [10, 20, 30, 40], true),
            [245, 235, 225, 215]
        );
    }

    #[test]
    fn twelve_bit_cmyk_uses_the_wider_maximum() {
        let tables = YcbcrTables::new(12);
        assert_eq!(
            ycck_to_cmyk(&tables, 4095, 2048, 2048, 100, false),
            [0, 0, 0, 100]
        );
        assert_eq!(
            cmyk_passthrough(4095, [0, 0, 0, 100], true),
            [4095, 4095, 4095, 3995]
        );
    }

    #[test]
    fn out_of_range_input_is_clamped() {
        assert_eq!(
            cmyk_passthrough(255, [60_000, 0, 0, 0], false),
            [255, 0, 0, 0]
        );
    }
}

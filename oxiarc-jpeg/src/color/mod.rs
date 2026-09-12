//! Colour spaces and the fixed-point conversions between them.

pub(crate) mod cmyk;
pub(crate) mod forward;
pub(crate) mod ycbcr;

/// A colour space a JPEG datastream's components can carry, or that a decode
/// can be asked to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorSpace {
    /// One component of luminance.
    Luma,
    /// Three components of `Y`, `Cb`, `Cr` (JFIF / ITU-R BT.601).
    Ycbcr,
    /// Three components of red, green and blue.
    Rgb,
    /// Four components of cyan, magenta, yellow and black.
    Cmyk,
    /// Four components of `Y`, `Cb`, `Cr`, `K` (Adobe transform 2).
    Ycck,
    /// The component count is outside `1..=4`, or no heuristic applied: the
    /// samples are delivered untransformed. This is what a decode reports
    /// for any frame whose component count the crate's own colour-space
    /// heuristic cannot name (or a caller's own
    /// [`crate::DecodeOptions::output_color_space`] chooses), and libjpeg
    /// reaches it only through `JCS_UNKNOWN`.
    ///
    /// The encoder accepts it as an explicit
    /// [`EncodeOptions::jpeg_color_space`](crate::EncodeOptions::jpeg_color_space)
    /// only at `Unknown(2)`: sequential ids `1`/`2`, one quantisation and
    /// Huffman slot, no subsampling and no colour transform — libjpeg's
    /// `JCS_UNKNOWN` shape, and the one two-component JPEG frame exists for
    /// at all. One, three and four components already have named colour
    /// spaces above, so `Unknown(1)`, `Unknown(3)` and `Unknown(4)` have no
    /// [`InputColor`](crate::InputColor) wired to them and are refused with
    /// [`crate::JpegError::InvalidEncodeParameter`], as is any count above
    /// four (T.81 allows more, but no table slot, sampling-factor or
    /// component-identifier array in this crate is sized past four).
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
    /// use oxiarc_jpeg::{ColorSpace, Decoder, EncodeOptions, InputColor, encode_to_vec_with_options};
    ///
    /// // Luminance and alpha, both kept — the alpha survives as a second,
    /// // untransformed component instead of being dropped.
    /// let pixels = [10u8, 200, 20, 210, 30, 220]; // (Y, A) x 3 pixels
    /// let options = EncodeOptions {
    ///     jpeg_color_space: Some(ColorSpace::Unknown(2)),
    ///     ..Default::default()
    /// };
    /// let jpeg = encode_to_vec_with_options(&pixels, 3, 1, InputColor::LumaAlpha, &options)?;
    ///
    /// let mut decoder = Decoder::new(&jpeg[..]);
    /// let info = decoder.read_info()?;
    /// assert_eq!(info.num_components, 2);
    /// assert_eq!(info.output_color_space, ColorSpace::Unknown(2));
    /// assert!(!info.has_jfif && !info.has_adobe, "JCS_UNKNOWN writes neither marker");
    /// assert_eq!(decoder.decode()?.len(), pixels.len());
    /// # Ok(())
    /// # }
    /// ```
    Unknown(u8),
}

impl ColorSpace {
    /// How many components this colour space has.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_jpeg::ColorSpace;
    ///
    /// assert_eq!(ColorSpace::Luma.num_components(), 1);
    /// assert_eq!(ColorSpace::Ycbcr.num_components(), 3);
    /// assert_eq!(ColorSpace::Cmyk.num_components(), 4);
    /// assert_eq!(ColorSpace::Unknown(2).num_components(), 2);
    /// ```
    #[must_use]
    pub const fn num_components(self) -> usize {
        match self {
            ColorSpace::Luma => 1,
            ColorSpace::Ycbcr | ColorSpace::Rgb => 3,
            ColorSpace::Cmyk | ColorSpace::Ycck => 4,
            ColorSpace::Unknown(n) => n as usize,
        }
    }
}

/// The libjpeg `default_decompress_parms` heuristic (`jdapimin.c`).
///
/// TIFF's `Compression = 7` strips carry no `JFIF` and no `Adobe` marker, so
/// the component identifiers are the only colour signal — `'R','G','B'` means
/// RGB and `'C','M','Y','K'` lands on CMYK through the four-component branch.
pub(crate) fn guess_input_color_space(
    component_ids: &[u8],
    saw_jfif: bool,
    adobe_transform: Option<u8>,
) -> ColorSpace {
    match component_ids.len() {
        1 => ColorSpace::Luma,
        3 => {
            if saw_jfif {
                ColorSpace::Ycbcr
            } else if let Some(transform) = adobe_transform {
                match transform {
                    0 => ColorSpace::Rgb,
                    _ => ColorSpace::Ycbcr,
                }
            } else if component_ids == [1, 2, 3] {
                ColorSpace::Ycbcr
            } else if component_ids == *b"RGB" {
                ColorSpace::Rgb
            } else {
                ColorSpace::Ycbcr
            }
        }
        4 => match adobe_transform {
            Some(2) => ColorSpace::Ycck,
            _ => ColorSpace::Cmyk,
        },
        other => ColorSpace::Unknown(other as u8),
    }
}

/// The colour space a decode produces by default for a given input space.
pub(crate) fn default_output_color_space(input: ColorSpace) -> ColorSpace {
    match input {
        ColorSpace::Ycbcr => ColorSpace::Rgb,
        ColorSpace::Ycck => ColorSpace::Cmyk,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_component_is_always_luma() {
        assert_eq!(guess_input_color_space(&[1], false, None), ColorSpace::Luma);
        assert_eq!(
            guess_input_color_space(&[0], true, Some(0)),
            ColorSpace::Luma
        );
    }

    #[test]
    fn three_components_follow_the_libjpeg_ladder() {
        // JFIF wins over everything.
        assert_eq!(
            guess_input_color_space(b"RGB", true, None),
            ColorSpace::Ycbcr
        );
        // Adobe transform 0 means RGB, 1 means YCbCr, anything else YCbCr.
        assert_eq!(
            guess_input_color_space(&[1, 2, 3], false, Some(0)),
            ColorSpace::Rgb
        );
        assert_eq!(
            guess_input_color_space(b"RGB", false, Some(1)),
            ColorSpace::Ycbcr
        );
        assert_eq!(
            guess_input_color_space(&[1, 2, 3], false, Some(9)),
            ColorSpace::Ycbcr
        );
        // Identifier heuristics, which is what libtiff strips rely on.
        assert_eq!(
            guess_input_color_space(&[1, 2, 3], false, None),
            ColorSpace::Ycbcr
        );
        assert_eq!(
            guess_input_color_space(b"RGB", false, None),
            ColorSpace::Rgb
        );
        assert_eq!(
            guess_input_color_space(&[7, 8, 9], false, None),
            ColorSpace::Ycbcr
        );
    }

    #[test]
    fn four_components_land_on_cmyk_unless_adobe_says_ycck() {
        assert_eq!(
            guess_input_color_space(b"CMYK", false, None),
            ColorSpace::Cmyk
        );
        assert_eq!(
            guess_input_color_space(&[1, 2, 3, 4], false, Some(0)),
            ColorSpace::Cmyk
        );
        assert_eq!(
            guess_input_color_space(&[1, 2, 3, 4], false, Some(2)),
            ColorSpace::Ycck
        );
    }

    #[test]
    fn other_counts_are_unknown() {
        assert_eq!(
            guess_input_color_space(&[1, 2], false, None),
            ColorSpace::Unknown(2)
        );
        assert_eq!(ColorSpace::Unknown(2).num_components(), 2);
    }

    #[test]
    fn default_output_maps_the_transforms() {
        assert_eq!(
            default_output_color_space(ColorSpace::Ycbcr),
            ColorSpace::Rgb
        );
        assert_eq!(
            default_output_color_space(ColorSpace::Ycck),
            ColorSpace::Cmyk
        );
        assert_eq!(
            default_output_color_space(ColorSpace::Luma),
            ColorSpace::Luma
        );
        assert_eq!(default_output_color_space(ColorSpace::Rgb), ColorSpace::Rgb);
        assert_eq!(
            default_output_color_space(ColorSpace::Cmyk),
            ColorSpace::Cmyk
        );
    }
}

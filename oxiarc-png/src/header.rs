//! The `IHDR` image header: colour types, bit depths and the derived row
//! geometry every other module is built on.

use crate::error::{DecodingError, FormatErrorKind};

/// The colour type of a PNG image, i.e. how samples map to channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ColorType {
    /// One grayscale sample per pixel.
    Grayscale = 0,
    /// Three samples per pixel: red, green, blue.
    Rgb = 2,
    /// One palette index per pixel; requires a `PLTE` chunk.
    Indexed = 3,
    /// Two samples per pixel: grayscale and alpha.
    GrayscaleAlpha = 4,
    /// Four samples per pixel: red, green, blue and alpha.
    Rgba = 6,
}

impl ColorType {
    /// The number of samples (channels) each pixel is stored with.
    ///
    /// ```
    /// use oxiarc_png::ColorType;
    /// assert_eq!(ColorType::Rgba.samples(), 4);
    /// assert_eq!(ColorType::Indexed.samples(), 1);
    /// ```
    #[must_use]
    pub fn samples(self) -> usize {
        self.samples_u8() as usize
    }

    pub(crate) fn samples_u8(self) -> u8 {
        match self {
            ColorType::Grayscale | ColorType::Indexed => 1,
            ColorType::Rgb => 3,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Rgba => 4,
        }
    }

    /// Parse from the byte stored in `IHDR`.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<ColorType> {
        match n {
            0 => Some(ColorType::Grayscale),
            2 => Some(ColorType::Rgb),
            3 => Some(ColorType::Indexed),
            4 => Some(ColorType::GrayscaleAlpha),
            6 => Some(ColorType::Rgba),
            _ => None,
        }
    }

    /// Whether the pair `(self, bit_depth)` is forbidden by Table 11.1 of the
    /// specification.
    ///
    /// ```
    /// use oxiarc_png::{BitDepth, ColorType};
    /// assert!(ColorType::is_combination_invalid(ColorType::Rgb, BitDepth::Four));
    /// assert!(ColorType::is_combination_invalid(ColorType::Indexed, BitDepth::Sixteen));
    /// assert!(!ColorType::is_combination_invalid(ColorType::Grayscale, BitDepth::One));
    /// ```
    #[must_use]
    pub fn is_combination_invalid(color_type: ColorType, bit_depth: BitDepth) -> bool {
        // Depths below 8 are only legal for Grayscale and Indexed; depth 16 is
        // illegal for Indexed.
        ((bit_depth == BitDepth::One || bit_depth == BitDepth::Two || bit_depth == BitDepth::Four)
            && (color_type == ColorType::Rgb
                || color_type == ColorType::GrayscaleAlpha
                || color_type == ColorType::Rgba))
            || (bit_depth == BitDepth::Sixteen && color_type == ColorType::Indexed)
    }

    /// The number of bytes a scanline of `width` pixels occupies, **including**
    /// the leading filter byte. This matches the `png` crate's convention.
    ///
    /// Panics are impossible: the arithmetic is done on `u64` and the result is
    /// saturated. Use [`ColorType::checked_raw_row_length`] where an oversized
    /// image must be rejected instead.
    #[must_use]
    pub fn raw_row_length_from_width(self, depth: BitDepth, width: u32) -> usize {
        self.checked_raw_row_length(depth, width)
            .unwrap_or(usize::MAX)
    }

    /// As [`ColorType::raw_row_length_from_width`] but `None` on overflow.
    #[must_use]
    pub fn checked_raw_row_length(self, depth: BitDepth, width: u32) -> Option<usize> {
        let bits = u64::from(width) * u64::from(self.samples_u8()) * u64::from(depth as u8);
        let bytes = bits.div_ceil(8).checked_add(1)?;
        usize::try_from(bytes).ok()
    }
}

/// The bit depth of a single sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BitDepth {
    /// One bit per sample.
    One = 1,
    /// Two bits per sample.
    Two = 2,
    /// Four bits per sample.
    Four = 4,
    /// Eight bits per sample.
    Eight = 8,
    /// Sixteen bits per sample, stored big-endian.
    Sixteen = 16,
}

impl BitDepth {
    /// Parse from the byte stored in `IHDR`.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<BitDepth> {
        match n {
            1 => Some(BitDepth::One),
            2 => Some(BitDepth::Two),
            4 => Some(BitDepth::Four),
            8 => Some(BitDepth::Eight),
            16 => Some(BitDepth::Sixteen),
            _ => None,
        }
    }

    /// The depth as a plain integer.
    #[must_use]
    pub fn into_u8(self) -> u8 {
        self as u8
    }
}

/// Whether the image is stored progressively (Adam7) or as plain scanlines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Interlace {
    /// Plain top-to-bottom scanlines.
    #[default]
    None = 0,
    /// Adam7 seven-pass interlacing.
    Adam7 = 1,
}

impl Interlace {
    /// Parse from the byte stored in `IHDR`.
    #[must_use]
    pub fn from_u8(n: u8) -> Option<Interlace> {
        match n {
            0 => Some(Interlace::None),
            1 => Some(Interlace::Adam7),
            _ => None,
        }
    }

    /// True for [`Interlace::Adam7`].
    #[must_use]
    pub fn is_interlaced(self) -> bool {
        matches!(self, Interlace::Adam7)
    }
}

/// The distance, in bytes, between a filtered byte and its left neighbour.
///
/// PNG filtering works on bytes with a stride of `max(1, bits_per_pixel / 8)`.
/// Only these six values are reachable across the whole of Table 11.1, so the
/// unfilter kernels are specialised for exactly this set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BytesPerPixel {
    /// 1 byte: any depth below 8, or 8-bit grayscale/indexed.
    One = 1,
    /// 2 bytes: 16-bit grayscale, or 8-bit grayscale+alpha.
    Two = 2,
    /// 3 bytes: 8-bit RGB.
    Three = 3,
    /// 4 bytes: 8-bit RGBA, or 16-bit grayscale+alpha.
    Four = 4,
    /// 6 bytes: 16-bit RGB.
    Six = 6,
    /// 8 bytes: 16-bit RGBA.
    Eight = 8,
}

impl BytesPerPixel {
    /// The stride as a `usize`.
    #[must_use]
    pub fn into_usize(self) -> usize {
        self as usize
    }

    /// Compute the filter stride for a colour type and depth.
    #[must_use]
    pub fn from_color_and_depth(color: ColorType, depth: BitDepth) -> BytesPerPixel {
        let bits = u16::from(color.samples_u8()) * u16::from(depth as u8);
        match bits.div_ceil(8) {
            0 | 1 => BytesPerPixel::One,
            2 => BytesPerPixel::Two,
            3 => BytesPerPixel::Three,
            4 => BytesPerPixel::Four,
            6 => BytesPerPixel::Six,
            _ => BytesPerPixel::Eight,
        }
    }
}

/// A parsed `IHDR` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ihdr {
    /// Image width in pixels; at least 1.
    pub width: u32,
    /// Image height in pixels; at least 1.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: BitDepth,
    /// How samples map to channels.
    pub color_type: ColorType,
    /// Interlace method.
    pub interlace: Interlace,
}

impl Ihdr {
    /// Parse the 13-byte `IHDR` payload.
    ///
    /// Every field is validated: dimensions must be non-zero, the compression
    /// and filter methods must both be `0`, the interlace method must be `0` or
    /// `1`, and the `(colour type, bit depth)` pair must satisfy Table 11.1.
    ///
    /// ```
    /// use oxiarc_png::header::Ihdr;
    /// let mut data = [0u8; 13];
    /// data[0..4].copy_from_slice(&2u32.to_be_bytes());
    /// data[4..8].copy_from_slice(&3u32.to_be_bytes());
    /// data[8] = 8;  // bit depth
    /// data[9] = 6;  // RGBA
    /// let ihdr = Ihdr::parse(&data).expect("valid header");
    /// assert_eq!((ihdr.width, ihdr.height), (2, 3));
    /// assert_eq!(ihdr.row_stride(), 8);
    /// ```
    pub fn parse(data: &[u8]) -> Result<Ihdr, DecodingError> {
        if data.len() != 13 {
            return Err(FormatErrorKind::ChunkLengthWrong {
                kind: crate::chunk::IHDR,
            }
            .into());
        }
        let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        if width == 0 {
            return Err(FormatErrorKind::InvalidDimensions.into());
        }
        if height == 0 {
            return Err(FormatErrorKind::InvalidDimensions.into());
        }
        // Both dimensions are 31-bit per the specification.
        if width > 0x7FFF_FFFF || height > 0x7FFF_FFFF {
            return Err(FormatErrorKind::InvalidDimensions.into());
        }
        let bit_depth = BitDepth::from_u8(data[8])
            .ok_or(FormatErrorKind::InvalidBitDepth { depth: data[8] })?;
        let color_type = ColorType::from_u8(data[9]).ok_or(FormatErrorKind::InvalidColorType {
            color_type: data[9],
        })?;
        if ColorType::is_combination_invalid(color_type, bit_depth) {
            return Err(FormatErrorKind::InvalidColorBitDepth {
                color_type,
                bit_depth,
            }
            .into());
        }
        if data[10] != 0 {
            return Err(FormatErrorKind::UnknownCompressionMethod { method: data[10] }.into());
        }
        if data[11] != 0 {
            return Err(FormatErrorKind::UnknownFilterMethod { method: data[11] }.into());
        }
        let interlace = Interlace::from_u8(data[12])
            .ok_or(FormatErrorKind::UnknownInterlaceMethod { method: data[12] })?;
        Ok(Ihdr {
            width,
            height,
            bit_depth,
            color_type,
            interlace,
        })
    }

    /// Serialise back to the 13-byte payload.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 13] {
        let mut out = [0u8; 13];
        out[0..4].copy_from_slice(&self.width.to_be_bytes());
        out[4..8].copy_from_slice(&self.height.to_be_bytes());
        out[8] = self.bit_depth as u8;
        out[9] = self.color_type as u8;
        out[12] = self.interlace as u8;
        out
    }

    /// Bits occupied by one pixel.
    #[must_use]
    pub fn bits_per_pixel(&self) -> u16 {
        u16::from(self.color_type.samples_u8()) * u16::from(self.bit_depth as u8)
    }

    /// The filter stride, `max(1, bits_per_pixel / 8)`.
    #[must_use]
    pub fn bytes_per_pixel(&self) -> BytesPerPixel {
        BytesPerPixel::from_color_and_depth(self.color_type, self.bit_depth)
    }

    /// Bytes in one scanline of the full image, **excluding** the filter byte.
    #[must_use]
    pub fn row_stride(&self) -> usize {
        self.row_stride_for_width(self.width)
    }

    /// Bytes in one scanline of `width` pixels, excluding the filter byte.
    #[must_use]
    pub fn row_stride_for_width(&self, width: u32) -> usize {
        let bits = u64::from(width) * u64::from(self.bits_per_pixel());
        usize::try_from(bits.div_ceil(8)).unwrap_or(usize::MAX)
    }

    /// Bytes in one scanline of `width` pixels, including the filter byte.
    #[must_use]
    pub fn raw_row_length_for_width(&self, width: u32) -> usize {
        self.row_stride_for_width(width).saturating_add(1)
    }

    /// The exact number of raw (filtered, uncompressed) bytes the image data
    /// stream must produce.
    ///
    /// This is computable from `IHDR` alone, before a single compressed byte is
    /// inflated, and is the crate's decompression-bomb bound: the decoder never
    /// hands the inflater more output space than remains of this count.
    ///
    /// For interlaced images it is the sum over the seven Adam7 passes, taking
    /// only passes with a non-zero extent into account.
    ///
    /// ```
    /// use oxiarc_png::header::Ihdr;
    /// # let mut d = [0u8; 13];
    /// # d[3] = 4; d[7] = 4; d[8] = 8; d[9] = 0;
    /// let ihdr = Ihdr::parse(&d).expect("valid");
    /// assert_eq!(ihdr.expected_raw_bytes(), Some(4 * (1 + 4)));
    /// ```
    #[must_use]
    pub fn expected_raw_bytes(&self) -> Option<u64> {
        match self.interlace {
            Interlace::None => {
                let row = u64::try_from(self.raw_row_length_for_width(self.width)).ok()?;
                row.checked_mul(u64::from(self.height))
            }
            Interlace::Adam7 => {
                let mut total: u64 = 0;
                for pass in 0..7 {
                    let (pw, ph) = crate::interlace::pass_dimensions(self.width, self.height, pass);
                    if pw == 0 || ph == 0 {
                        continue;
                    }
                    let row = u64::try_from(self.raw_row_length_for_width(pw)).ok()?;
                    total = total.checked_add(row.checked_mul(u64::from(ph))?)?;
                }
                Some(total)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ihdr(w: u32, h: u32, depth: u8, ct: u8, interlace: u8) -> [u8; 13] {
        let mut d = [0u8; 13];
        d[0..4].copy_from_slice(&w.to_be_bytes());
        d[4..8].copy_from_slice(&h.to_be_bytes());
        d[8] = depth;
        d[9] = ct;
        d[12] = interlace;
        d
    }

    #[test]
    fn table_11_1_accepts_exactly_the_legal_combinations() {
        let legal: &[(u8, &[u8])] = &[
            (0, &[1, 2, 4, 8, 16]),
            (2, &[8, 16]),
            (3, &[1, 2, 4, 8]),
            (4, &[8, 16]),
            (6, &[8, 16]),
        ];
        for depth in [1u8, 2, 4, 8, 16] {
            for ct in [0u8, 2, 3, 4, 6] {
                let allowed = legal
                    .iter()
                    .find(|(c, _)| *c == ct)
                    .map(|(_, ds)| ds.contains(&depth))
                    .unwrap_or(false);
                let parsed = Ihdr::parse(&ihdr(1, 1, depth, ct, 0));
                assert_eq!(
                    parsed.is_ok(),
                    allowed,
                    "color type {ct} depth {depth} allowed={allowed}"
                );
            }
        }
    }

    #[test]
    fn rejects_bad_scalar_fields() {
        assert!(Ihdr::parse(&ihdr(0, 1, 8, 0, 0)).is_err());
        assert!(Ihdr::parse(&ihdr(1, 0, 8, 0, 0)).is_err());
        assert!(Ihdr::parse(&ihdr(1, 1, 3, 0, 0)).is_err());
        assert!(Ihdr::parse(&ihdr(1, 1, 8, 1, 0)).is_err());
        assert!(Ihdr::parse(&ihdr(1, 1, 8, 0, 2)).is_err());
        let mut d = ihdr(1, 1, 8, 0, 0);
        d[10] = 1;
        assert!(Ihdr::parse(&d).is_err());
        let mut d = ihdr(1, 1, 8, 0, 0);
        d[11] = 1;
        assert!(Ihdr::parse(&d).is_err());
        assert!(Ihdr::parse(&[0u8; 12]).is_err());
    }

    #[test]
    fn bytes_per_pixel_covers_the_six_reachable_values() {
        use BitDepth::*;
        use ColorType::*;
        let cases = [
            (Grayscale, One, BytesPerPixel::One),
            (Grayscale, Four, BytesPerPixel::One),
            (Grayscale, Eight, BytesPerPixel::One),
            (Grayscale, Sixteen, BytesPerPixel::Two),
            (Indexed, Eight, BytesPerPixel::One),
            (GrayscaleAlpha, Eight, BytesPerPixel::Two),
            (GrayscaleAlpha, Sixteen, BytesPerPixel::Four),
            (Rgb, Eight, BytesPerPixel::Three),
            (Rgb, Sixteen, BytesPerPixel::Six),
            (Rgba, Eight, BytesPerPixel::Four),
            (Rgba, Sixteen, BytesPerPixel::Eight),
        ];
        for (ct, bd, want) in cases {
            assert_eq!(BytesPerPixel::from_color_and_depth(ct, bd), want);
        }
    }

    #[test]
    fn row_geometry_matches_the_spec_formula() {
        let h = Ihdr::parse(&ihdr(7, 1, 1, 0, 0)).expect("valid");
        assert_eq!(h.row_stride(), 1);
        assert_eq!(h.raw_row_length_for_width(7), 2);
        let h = Ihdr::parse(&ihdr(9, 1, 4, 3, 0)).expect("valid");
        assert_eq!(h.row_stride(), 5);
        let h = Ihdr::parse(&ihdr(3, 1, 16, 6, 0)).expect("valid");
        assert_eq!(h.row_stride(), 24);
        assert_eq!(
            ColorType::Rgba.raw_row_length_from_width(BitDepth::Sixteen, 3),
            25
        );
    }

    #[test]
    fn expected_raw_bytes_non_interlaced() {
        let h = Ihdr::parse(&ihdr(5, 3, 8, 2, 0)).expect("valid");
        assert_eq!(h.expected_raw_bytes(), Some(3 * (1 + 15)));
    }

    #[test]
    fn expected_raw_bytes_interlaced_1x1_counts_only_pass_1() {
        let h = Ihdr::parse(&ihdr(1, 1, 8, 0, 1)).expect("valid");
        assert_eq!(h.expected_raw_bytes(), Some(2));
    }

    #[test]
    fn expected_raw_bytes_interlaced_height_one_skips_odd_passes() {
        // height == 1 => passes 3, 5 and 7 (y_offset > 0) are empty.
        let h = Ihdr::parse(&ihdr(8, 1, 8, 0, 1)).expect("valid");
        // pass 1: w=1, pass 2: w=1, pass 4: w=2, pass 6: w=4 (each 1 row)
        assert_eq!(
            h.expected_raw_bytes(),
            Some((1 + 1) + (1 + 1) + (1 + 2) + (1 + 4))
        );
    }

    #[test]
    fn expected_raw_bytes_interlaced_width_four_skips_pass_2() {
        let h = Ihdr::parse(&ihdr(4, 8, 8, 0, 1)).expect("valid");
        // pass 2 has x_offset 4 => empty for width 4.
        let mut total = 0u64;
        for pass in 0..7 {
            let (pw, ph) = crate::interlace::pass_dimensions(4, 8, pass);
            if pw == 0 || ph == 0 {
                continue;
            }
            total += (1 + u64::from(pw)) * u64::from(ph);
        }
        assert_eq!(h.expected_raw_bytes(), Some(total));
        assert_eq!(crate::interlace::pass_dimensions(4, 8, 1).0, 0);
    }

    #[test]
    fn ihdr_round_trips_through_bytes() {
        let h = Ihdr::parse(&ihdr(640, 480, 16, 6, 1)).expect("valid");
        assert_eq!(Ihdr::parse(&h.to_bytes()).expect("round trip"), h);
        assert_eq!(h.bits_per_pixel(), 64);
    }
}

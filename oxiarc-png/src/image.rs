//! The native in-memory image container.

use crate::error::DecodingError;
use crate::header::{BitDepth, ColorType};
use crate::info::Info;

/// A decoded image and everything its file said about it.
///
/// `data` is row-major and packed exactly as PNG stores samples: most
/// significant bit first for sub-byte depths, **big-endian** for 16-bit
/// samples. That matches both the `png` crate and the `image` crate, so a
/// buffer can be handed to either without a byte swap.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Image {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Colour type of `data`, after any requested transformations.
    pub color_type: ColorType,
    /// Bit depth of `data`, after any requested transformations.
    pub bit_depth: BitDepth,
    /// The pixels. Exactly `line_size() * height` bytes long.
    pub data: Vec<u8>,
    /// Everything read from the file's chunks.
    pub info: Info<'static>,
}

impl Image {
    /// The number of bytes one row occupies.
    #[must_use]
    pub fn line_size(&self) -> usize {
        self.color_type
            .checked_raw_row_length(self.bit_depth, self.width)
            .map_or(0, |n| n - 1)
    }

    /// The number of bytes the whole image occupies.
    #[must_use]
    pub fn buffer_size(&self) -> usize {
        self.line_size().saturating_mul(self.height as usize)
    }

    /// The image's palette, three bytes per entry, when it has one.
    #[must_use]
    pub fn palette(&self) -> Option<&[u8]> {
        self.info.palette.as_deref()
    }

    /// The image's transparency chunk, normalised the way [`Info::trns`]
    /// describes.
    ///
    /// [`Info::trns`]: crate::info::Info::trns
    #[must_use]
    pub fn transparency(&self) -> Option<&[u8]> {
        self.info.trns.as_deref()
    }

    /// Read the `index`-th sample of a row, whatever the bit depth.
    fn sample(&self, row: &[u8], index: usize) -> u16 {
        match self.bit_depth {
            BitDepth::Sixteen => {
                let i = index * 2;
                u16::from_be_bytes([
                    row.get(i).copied().unwrap_or(0),
                    row.get(i + 1).copied().unwrap_or(0),
                ])
            }
            BitDepth::Eight => u16::from(row.get(index).copied().unwrap_or(0)),
            depth => {
                let bits = depth as u8;
                let bit = index * usize::from(bits);
                let byte = bit / 8;
                let shift = 8 - bits - (bit % 8) as u8;
                let mask = (1u16 << bits) as u8 - 1;
                u16::from((row.get(byte).copied().unwrap_or(0) >> shift) & mask)
            }
        }
    }

    /// Scale a sample from the image's bit depth up to a full 16-bit range.
    fn scale_to_16(&self, value: u16) -> u16 {
        match self.bit_depth {
            BitDepth::Sixteen => value,
            BitDepth::Eight => value * 257,
            depth => {
                let max = (1u32 << (depth as u8)) - 1;
                ((u32::from(value) * 65535) / max) as u16
            }
        }
    }

    /// Convert to non-premultiplied 16-bit RGBA, native endian.
    ///
    /// This is the canonical conversion; [`Image::to_rgba8`] is its high-byte
    /// truncation, which is exact for 8-bit and sub-byte sources.
    ///
    /// # Errors
    ///
    /// An indexed image with no palette, or an image whose expanded form does
    /// not fit in memory: this conversion needs eight bytes per pixel, so both
    /// the palette check and the size check happen before the allocation.
    pub fn to_rgba16(&self) -> Result<Vec<u16>, DecodingError> {
        let width = self.width as usize;
        let height = self.height as usize;
        let line = self.line_size();
        let palette = self.info.palette.as_deref();
        if self.color_type == ColorType::Indexed && palette.is_none() {
            return Err(crate::error::FormatErrorKind::PaletteRequired.into());
        }
        let count = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .filter(|n| n.saturating_mul(2) <= isize::MAX as usize)
            .ok_or(DecodingError::LimitsExceeded)?;
        let mut out = vec![0u16; count];
        let trns = self.info.trns.as_deref();
        let samples = self.color_type.samples();
        for y in 0..height {
            let start = y.saturating_mul(line);
            let row = start
                .checked_add(line)
                .and_then(|end| self.data.get(start..end))
                .unwrap_or(&[]);
            for x in 0..width {
                let dst = (y * width + x) * 4;
                match self.color_type {
                    ColorType::Grayscale => {
                        let raw = self.sample(row, x);
                        let v = self.scale_to_16(raw);
                        let a = match trns {
                            Some(key) if self.trns_gray_matches(key, raw) => 0,
                            _ => u16::MAX,
                        };
                        out[dst] = v;
                        out[dst + 1] = v;
                        out[dst + 2] = v;
                        out[dst + 3] = a;
                    }
                    ColorType::GrayscaleAlpha => {
                        let v = self.scale_to_16(self.sample(row, x * samples));
                        let a = self.scale_to_16(self.sample(row, x * samples + 1));
                        out[dst] = v;
                        out[dst + 1] = v;
                        out[dst + 2] = v;
                        out[dst + 3] = a;
                    }
                    ColorType::Rgb => {
                        let raw = [
                            self.sample(row, x * 3),
                            self.sample(row, x * 3 + 1),
                            self.sample(row, x * 3 + 2),
                        ];
                        for c in 0..3 {
                            out[dst + c] = self.scale_to_16(raw[c]);
                        }
                        out[dst + 3] = match trns {
                            Some(key) if self.trns_rgb_matches(key, raw) => 0,
                            _ => u16::MAX,
                        };
                    }
                    ColorType::Rgba => {
                        for c in 0..4 {
                            out[dst + c] = self.scale_to_16(self.sample(row, x * 4 + c));
                        }
                    }
                    ColorType::Indexed => {
                        let index = self.sample(row, x) as usize;
                        let palette = palette.unwrap_or(&[]);
                        let entry = palette.get(index * 3..index * 3 + 3);
                        let (r, g, b) = match entry {
                            Some(e) => (e[0], e[1], e[2]),
                            None => (0, 0, 0),
                        };
                        let a = match trns.and_then(|t| t.get(index)) {
                            Some(a) => *a,
                            None => 0xFF,
                        };
                        let a = if entry.is_none() { 0xFF } else { a };
                        out[dst] = u16::from(r) * 257;
                        out[dst + 1] = u16::from(g) * 257;
                        out[dst + 2] = u16::from(b) * 257;
                        out[dst + 3] = u16::from(a) * 257;
                    }
                }
            }
        }
        Ok(out)
    }

    fn trns_gray_matches(&self, key: &[u8], raw: u16) -> bool {
        match self.bit_depth {
            BitDepth::Sixteen => key.len() >= 2 && u16::from_be_bytes([key[0], key[1]]) == raw,
            _ => key.first().is_some_and(|k| u16::from(*k) == raw),
        }
    }

    fn trns_rgb_matches(&self, key: &[u8], raw: [u16; 3]) -> bool {
        match self.bit_depth {
            BitDepth::Sixteen => {
                key.len() >= 6
                    && (0..3).all(|i| u16::from_be_bytes([key[i * 2], key[i * 2 + 1]]) == raw[i])
            }
            _ => key.len() >= 3 && (0..3).all(|i| u16::from(key[i]) == raw[i]),
        }
    }

    /// Convert to non-premultiplied 8-bit RGBA.
    ///
    /// ```no_run
    /// let image = oxiarc_png::decode(&std::fs::read("image.png")?)?;
    /// let rgba = image.to_rgba8()?;
    /// assert_eq!(rgba.len(), image.width as usize * image.height as usize * 4);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn to_rgba8(&self) -> Result<Vec<u8>, DecodingError> {
        Ok(self
            .to_rgba16()?
            .into_iter()
            .map(|v| (v >> 8) as u8)
            .collect())
    }

    /// Convert to 8-bit RGB, discarding any alpha channel.
    pub fn to_rgb8(&self) -> Result<Vec<u8>, DecodingError> {
        let rgba = self.to_rgba8()?;
        Ok(rgba
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect())
    }

    /// Convert to 8-bit grayscale using the ITU-R BT.601 luma weights.
    pub fn to_luma8(&self) -> Result<Vec<u8>, DecodingError> {
        let rgba = self.to_rgba8()?;
        Ok(rgba
            .chunks_exact(4)
            .map(|p| {
                let y = 299 * u32::from(p[0]) + 587 * u32::from(p[1]) + 114 * u32::from(p[2]);
                (y / 1000) as u8
            })
            .collect())
    }

    /// Build an image from an RGBA8 buffer.
    ///
    /// # Errors
    ///
    /// The buffer length must be exactly `width * height * 4`.
    pub fn from_rgba8(width: u32, height: u32, rgba: Vec<u8>) -> Result<Image, DecodingError> {
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or(DecodingError::LimitsExceeded)?;
        if rgba.len() != expected {
            return Err(crate::error::ParameterErrorKind::ImageBufferSize {
                expected,
                actual: rgba.len(),
            }
            .into());
        }
        let mut info = Info::with_size(width, height);
        info.color_type = ColorType::Rgba;
        info.bit_depth = BitDepth::Eight;
        Ok(Image {
            width,
            height,
            color_type: ColorType::Rgba,
            bit_depth: BitDepth::Eight,
            data: rgba,
            info,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn image(color_type: ColorType, bit_depth: BitDepth, data: Vec<u8>, w: u32, h: u32) -> Image {
        let mut info = Info::with_size(w, h);
        info.color_type = color_type;
        info.bit_depth = bit_depth;
        Image {
            width: w,
            height: h,
            color_type,
            bit_depth,
            data,
            info,
        }
    }

    #[test]
    fn line_and_buffer_sizes() {
        let img = image(ColorType::Rgb, BitDepth::Eight, vec![0; 18], 3, 2);
        assert_eq!(img.line_size(), 9);
        assert_eq!(img.buffer_size(), 18);
        let img = image(ColorType::Grayscale, BitDepth::One, vec![0; 2], 5, 2);
        assert_eq!(img.line_size(), 1);
        assert_eq!(img.buffer_size(), 2);
    }

    #[test]
    fn gray8_to_rgba8_replicates_channels() {
        let img = image(
            ColorType::Grayscale,
            BitDepth::Eight,
            vec![0, 128, 255],
            3,
            1,
        );
        assert_eq!(
            img.to_rgba8().expect("convert"),
            vec![0, 0, 0, 255, 128, 128, 128, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn sub_byte_gray_scales_by_replication() {
        let img = image(
            ColorType::Grayscale,
            BitDepth::Two,
            vec![0b00_01_10_11],
            4,
            1,
        );
        let rgba = img.to_rgba8().expect("convert");
        assert_eq!(
            rgba.chunks_exact(4).map(|p| p[0]).collect::<Vec<_>>(),
            vec![0, 85, 170, 255]
        );
    }

    #[test]
    fn sixteen_bit_survives_to_rgba16_and_truncates_to_rgba8() {
        let img = image(
            ColorType::Rgb,
            BitDepth::Sixteen,
            vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC],
            1,
            1,
        );
        assert_eq!(
            img.to_rgba16().expect("convert"),
            vec![0x1234, 0x5678, 0x9ABC, 0xFFFF]
        );
        assert_eq!(
            img.to_rgba8().expect("convert"),
            vec![0x12, 0x56, 0x9A, 0xFF]
        );
    }

    #[test]
    fn grayscale_trns_key_makes_pixels_transparent() {
        let mut img = image(ColorType::Grayscale, BitDepth::Eight, vec![7, 8], 2, 1);
        img.info.trns = Some(Cow::Owned(vec![7]));
        let rgba = img.to_rgba8().expect("convert");
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[7], 255);
    }

    #[test]
    fn rgb_trns_key_matches_all_three_channels() {
        let mut img = image(
            ColorType::Rgb,
            BitDepth::Eight,
            vec![1, 2, 3, 1, 2, 4],
            2,
            1,
        );
        img.info.trns = Some(Cow::Owned(vec![1, 2, 3]));
        let rgba = img.to_rgba8().expect("convert");
        assert_eq!(rgba[3], 0);
        assert_eq!(rgba[7], 255);
    }

    #[test]
    fn indexed_uses_the_palette_and_falls_back_to_opaque_black() {
        let mut img = image(ColorType::Indexed, BitDepth::Eight, vec![0, 1, 9], 3, 1);
        img.info.palette = Some(Cow::Owned(vec![10, 20, 30, 40, 50, 60]));
        img.info.trns = Some(Cow::Owned(vec![0x80]));
        let rgba = img.to_rgba8().expect("convert");
        assert_eq!(&rgba[0..4], &[10, 20, 30, 0x80]);
        assert_eq!(&rgba[4..8], &[40, 50, 60, 255]);
        assert_eq!(&rgba[8..12], &[0, 0, 0, 255]);
    }

    #[test]
    fn indexed_without_palette_is_an_error() {
        let img = image(ColorType::Indexed, BitDepth::Eight, vec![0], 1, 1);
        assert!(img.to_rgba16().is_err());
    }

    #[test]
    fn rgb_and_luma_conversions() {
        let img = image(ColorType::Rgb, BitDepth::Eight, vec![255, 0, 0], 1, 1);
        assert_eq!(img.to_rgb8().expect("convert"), vec![255, 0, 0]);
        assert_eq!(img.to_luma8().expect("convert"), vec![76]);
    }

    #[test]
    fn palette_and_transparency_accessors() {
        let mut img = image(ColorType::Indexed, BitDepth::Eight, vec![0], 1, 1);
        assert!(img.palette().is_none());
        assert!(img.transparency().is_none());
        img.info.palette = Some(Cow::Owned(vec![1, 2, 3]));
        img.info.trns = Some(Cow::Owned(vec![9]));
        assert_eq!(img.palette(), Some(&[1u8, 2, 3][..]));
        assert_eq!(img.transparency(), Some(&[9u8][..]));
    }

    #[test]
    fn from_rgba8_checks_the_length() {
        assert!(Image::from_rgba8(2, 2, vec![0; 16]).is_ok());
        assert!(Image::from_rgba8(2, 2, vec![0; 15]).is_err());
    }
}

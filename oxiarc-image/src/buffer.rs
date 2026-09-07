//! [`ImageBuffer`]: a width/height/samples container generic over pixel
//! type, and the ten `image`-shaped type aliases built on it.
//!
//! # Deviation from `image`: one concrete container, not two type parameters
//!
//! `image::ImageBuffer<P, Container>` is generic over *both* the pixel type
//! and the backing storage (`Vec<S>`, `&[S]`, a memory-mapped slice, ...).
//! This crate only ever needs the owned case, so `Container` is dropped: the
//! backing storage here is always `Vec<P::Subpixel>`.
//!
//! # Deviation from `image`: [`ImageBuffer::get_pixel`] returns an owned pixel
//!
//! See the [`crate::color`] module docs: this crate is
//! `#![forbid(unsafe_code)]`, so there is no safe zero-copy cast from
//! `&[Subpixel]` to `&Pixel`. [`ImageBuffer::pixel_slice`] is the zero-copy
//! escape hatch for a caller that needs a reference into the buffer rather
//! than a copy, and [`ImageBuffer::get_pixel_mut_channels`] is the escape
//! hatch for in-place mutation (there is no `get_pixel_mut() -> &mut P`
//! either, for the same reason).

use crate::color::{Luma, LumaA, Pixel, Primitive, Rgb, Rgba};

/// A width/height grid of pixels of type `P`, backed by one flat `Vec` of
/// samples.
///
/// See the module docs for the two ways this differs from `image`'s own
/// `ImageBuffer`.
#[derive(Clone, Debug, PartialEq)]
pub struct ImageBuffer<P: Pixel> {
    width: u32,
    height: u32,
    data: Vec<P::Subpixel>,
}

impl<P: Pixel> ImageBuffer<P> {
    /// A black (all-zero-sample) image of the given size.
    #[must_use]
    pub fn new(width: u32, height: u32) -> Self {
        let len = pixel_buffer_len::<P>(width, height);
        Self {
            width,
            height,
            data: vec![P::Subpixel::DEFAULT_MIN_VALUE; len],
        }
    }

    /// An image of the given size, every pixel set to `pixel`.
    #[must_use]
    pub fn from_pixel(width: u32, height: u32, pixel: P) -> Self {
        let mut buf = Self::new(width, height);
        for chunk in buf.data.chunks_exact_mut(P::CHANNEL_COUNT as usize) {
            chunk.copy_from_slice(pixel.channels());
        }
        buf
    }

    /// Build an image by calling `f(x, y)` for every pixel, row-major.
    ///
    /// ```
    /// use oxiarc_image::{ImageBuffer, Luma};
    /// let buf: ImageBuffer<Luma<u8>> = ImageBuffer::from_fn(2, 2, |x, y| Luma::new((x + y) as u8));
    /// assert_eq!(buf.get_pixel(1, 1), Luma::new(2));
    /// ```
    #[must_use]
    pub fn from_fn<F: FnMut(u32, u32) -> P>(width: u32, height: u32, mut f: F) -> Self {
        let mut buf = Self::new(width, height);
        for y in 0..height {
            for x in 0..width {
                buf.put_pixel(x, y, f(x, y));
            }
        }
        buf
    }

    /// Wrap an existing sample buffer, or `None` if its length does not
    /// match `width * height * P::CHANNEL_COUNT`.
    ///
    /// ```
    /// use oxiarc_image::{ImageBuffer, Rgb};
    /// let buf: Option<ImageBuffer<Rgb<u8>>> = ImageBuffer::from_raw(1, 1, vec![1, 2, 3]);
    /// assert!(buf.is_some());
    /// let too_short: Option<ImageBuffer<Rgb<u8>>> = ImageBuffer::from_raw(1, 1, vec![1, 2]);
    /// assert!(too_short.is_none());
    /// ```
    #[must_use]
    pub fn from_raw(width: u32, height: u32, data: Vec<P::Subpixel>) -> Option<Self> {
        if data.len() == pixel_buffer_len::<P>(width, height) {
            Some(Self {
                width,
                height,
                data,
            })
        } else {
            None
        }
    }

    /// Consume the buffer, returning its flat sample storage.
    #[must_use]
    pub fn into_raw(self) -> Vec<P::Subpixel> {
        self.data
    }

    /// The flat sample storage.
    #[must_use]
    pub fn as_raw(&self) -> &[P::Subpixel] {
        &self.data
    }

    /// The flat sample storage, mutably.
    #[must_use]
    pub fn as_raw_mut(&mut self) -> &mut [P::Subpixel] {
        &mut self.data
    }

    /// `(width, height)`.
    #[must_use]
    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    fn sample_index(&self, x: u32, y: u32) -> usize {
        assert!(
            x < self.width && y < self.height,
            "pixel index ({x}, {y}) out of bounds for a {}x{} image",
            self.width,
            self.height
        );
        (y as usize * self.width as usize + x as usize) * P::CHANNEL_COUNT as usize
    }

    /// The pixel at `(x, y)`, copied out of the buffer.
    ///
    /// # Panics
    /// If `x >= width` or `y >= height`, matching `image::ImageBuffer`'s own
    /// contract.
    #[must_use]
    pub fn get_pixel(&self, x: u32, y: u32) -> P {
        let i = self.sample_index(x, y);
        P::from_slice(&self.data[i..i + P::CHANNEL_COUNT as usize])
    }

    /// [`Self::get_pixel`], returning `None` instead of panicking when
    /// `(x, y)` is out of bounds.
    #[must_use]
    pub fn get_pixel_checked(&self, x: u32, y: u32) -> Option<P> {
        if x < self.width && y < self.height {
            Some(self.get_pixel(x, y))
        } else {
            None
        }
    }

    /// A zero-copy view of one pixel's samples, the escape hatch for a
    /// caller that would otherwise want `image`'s `&P`-returning
    /// `get_pixel`. See the module docs.
    #[must_use]
    pub fn pixel_slice(&self, x: u32, y: u32) -> &[P::Subpixel] {
        let i = self.sample_index(x, y);
        &self.data[i..i + P::CHANNEL_COUNT as usize]
    }

    /// A mutable, zero-copy view of one pixel's samples: the escape hatch
    /// for in-place mutation in place of `image`'s `&mut P`-returning
    /// `get_pixel_mut`. See the module docs.
    pub fn get_pixel_mut_channels(&mut self, x: u32, y: u32) -> &mut [P::Subpixel] {
        let i = self.sample_index(x, y);
        &mut self.data[i..i + P::CHANNEL_COUNT as usize]
    }

    /// Overwrite the pixel at `(x, y)`.
    ///
    /// # Panics
    /// If `x >= width` or `y >= height`.
    pub fn put_pixel(&mut self, x: u32, y: u32, pixel: P) {
        let i = self.sample_index(x, y);
        self.data[i..i + P::CHANNEL_COUNT as usize].copy_from_slice(pixel.channels());
    }

    /// Wrap a sample vector the caller has already sized to
    /// `width * height * P::CHANNEL_COUNT`.
    ///
    /// Crate-internal fast path for [`crate::DynamicImage`]'s colour
    /// conversions, which build their output vector from a source buffer
    /// whose own invariant already fixes the length exactly — re-deriving
    /// the product only to hand back an [`Option`] every caller would have
    /// to unwrap adds nothing. A `debug_assert!` pins the invariant in test
    /// builds, and the `resize` keeps the struct invariant total even in a
    /// release build where that assertion is compiled out (it is a no-op
    /// whenever the length is already right, which is always).
    pub(crate) fn from_raw_sized(width: u32, height: u32, mut data: Vec<P::Subpixel>) -> Self {
        let len = pixel_buffer_len::<P>(width, height);
        debug_assert_eq!(
            data.len(),
            len,
            "a conversion produced {} samples for a {width}x{height} image that needs {len}",
            data.len()
        );
        data.resize(len, P::Subpixel::DEFAULT_MIN_VALUE);
        Self {
            width,
            height,
            data,
        }
    }

    /// Every pixel, row-major, copied out of the buffer.
    #[must_use]
    pub fn pixels(&self) -> Pixels<'_, P> {
        Pixels {
            chunks: self.data.chunks_exact(P::CHANNEL_COUNT as usize),
            _pixel: std::marker::PhantomData,
        }
    }
}

/// `width * height * P::CHANNEL_COUNT` as a `usize`, saturating rather than
/// overflowing (an overflowing product simply cannot match any real `Vec`'s
/// length, so [`ImageBuffer::from_raw`] correctly returns `None`).
fn pixel_buffer_len<P: Pixel>(width: u32, height: u32) -> usize {
    (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(P::CHANNEL_COUNT as usize)
}

/// Row-major iterator over a [`ImageBuffer`]'s pixels, yielding owned
/// values. See the [`crate::buffer`] module docs for why this yields `P`
/// rather than `&P`.
#[derive(Clone)]
pub struct Pixels<'a, P: Pixel> {
    chunks: std::slice::ChunksExact<'a, P::Subpixel>,
    _pixel: std::marker::PhantomData<P>,
}

impl<P: Pixel> Iterator for Pixels<'_, P> {
    type Item = P;

    fn next(&mut self) -> Option<P> {
        self.chunks.next().map(P::from_slice)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.chunks.size_hint()
    }
}

impl<P: Pixel> ExactSizeIterator for Pixels<'_, P> {}

/// 8-bit RGB image buffer.
pub type RgbImage = ImageBuffer<Rgb<u8>>;
/// 8-bit RGBA image buffer.
pub type RgbaImage = ImageBuffer<Rgba<u8>>;
/// 8-bit grayscale image buffer.
pub type GrayImage = ImageBuffer<Luma<u8>>;
/// 8-bit grayscale-with-alpha image buffer.
pub type GrayAlphaImage = ImageBuffer<LumaA<u8>>;
/// 16-bit RGB image buffer.
pub type Rgb16Image = ImageBuffer<Rgb<u16>>;
/// 16-bit RGBA image buffer.
pub type Rgba16Image = ImageBuffer<Rgba<u16>>;
/// 16-bit grayscale image buffer.
pub type Gray16Image = ImageBuffer<Luma<u16>>;
/// 16-bit grayscale-with-alpha image buffer.
pub type GrayAlpha16Image = ImageBuffer<LumaA<u16>>;
/// 32-bit float RGB image buffer.
pub type Rgb32FImage = ImageBuffer<Rgb<f32>>;
/// 32-bit float RGBA image buffer.
pub type Rgba32FImage = ImageBuffer<Rgba<f32>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_black() {
        let buf: RgbImage = ImageBuffer::new(2, 2);
        assert_eq!(buf.dimensions(), (2, 2));
        assert_eq!(buf.as_raw(), &[0u8; 12]);
    }

    #[test]
    fn from_pixel_fills_every_pixel() {
        let buf: RgbaImage = ImageBuffer::from_pixel(2, 1, Rgba::new(1, 2, 3, 4));
        assert_eq!(buf.as_raw(), &[1, 2, 3, 4, 1, 2, 3, 4]);
    }

    #[test]
    fn get_and_put_pixel_round_trip() {
        let mut buf: GrayImage = ImageBuffer::new(3, 3);
        buf.put_pixel(1, 2, Luma::new(200));
        assert_eq!(buf.get_pixel(1, 2), Luma::new(200));
        assert_eq!(buf.get_pixel(0, 0), Luma::new(0));
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn get_pixel_panics_out_of_bounds() {
        let buf: GrayImage = ImageBuffer::new(2, 2);
        let _ = buf.get_pixel(5, 0);
    }

    #[test]
    fn get_pixel_checked_is_the_safe_alternative() {
        let buf: GrayImage = ImageBuffer::new(2, 2);
        assert!(buf.get_pixel_checked(5, 0).is_none());
        assert!(buf.get_pixel_checked(1, 1).is_some());
    }

    #[test]
    fn from_raw_validates_length() {
        assert!(RgbImage::from_raw(2, 2, vec![0; 12]).is_some());
        assert!(RgbImage::from_raw(2, 2, vec![0; 11]).is_none());
        let buf = RgbImage::from_raw(2, 1, vec![1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(buf.into_raw(), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn from_fn_matches_manual_construction() {
        let buf: RgbImage = ImageBuffer::from_fn(2, 2, |x, y| Rgb::new(x as u8, y as u8, 0));
        assert_eq!(buf.get_pixel(1, 0), Rgb::new(1, 0, 0));
        assert_eq!(buf.get_pixel(0, 1), Rgb::new(0, 1, 0));
    }

    #[test]
    fn pixels_iterates_row_major_and_is_exact_sized() {
        let buf: GrayImage = ImageBuffer::from_fn(2, 2, |x, y| Luma::new((y * 2 + x) as u8));
        let values: Vec<u8> = buf.pixels().map(|p| p.0[0]).collect();
        assert_eq!(values, vec![0, 1, 2, 3]);
        assert_eq!(buf.pixels().len(), 4);
    }

    #[test]
    fn pixel_slice_and_mut_channels_are_zero_copy_views() {
        let mut buf: RgbImage = ImageBuffer::from_pixel(1, 1, Rgb::new(1, 2, 3));
        assert_eq!(buf.pixel_slice(0, 0), &[1, 2, 3]);
        buf.get_pixel_mut_channels(0, 0)[1] = 99;
        assert_eq!(buf.get_pixel(0, 0), Rgb::new(1, 99, 3));
    }

    #[test]
    fn huge_dimensions_do_not_overflow_from_raw_len_check() {
        let huge = u32::MAX;
        let buf: Option<RgbaImage> = ImageBuffer::from_raw(huge, huge, vec![0; 4]);
        assert!(buf.is_none());
    }
}

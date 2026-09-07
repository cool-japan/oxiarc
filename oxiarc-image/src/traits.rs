//! [`ImageDecoder`] / [`ImageEncoder`]: the two traits every
//! `codecs::{png,jpeg,tiff}` type implements.
//!
//! Trimmed from `image` 0.25's traits of the same name: no
//! `icc_profile`/`exif_metadata`/`xmp_metadata`/`orientation`/`set_limits`
//! default methods, and [`ImageDecoder`] is not object-safe (no
//! `read_image_boxed`/`Box<dyn ImageDecoder>` support), since nothing in
//! this crate needs to store a decoder behind a trait object —
//! [`crate::DynamicImage::from_decoder`] and
//! [`crate::ImageReader::decode`] dispatch on [`crate::ImageFormat`] with a
//! plain three-arm `match` instead. A decoder's own metadata accessors
//! (`exif()`, `icc_profile()`, ...) remain on the concrete
//! `codecs::*::*Decoder` types, unchanged from the crate they wrap.

use crate::color::{ColorType, ExtendedColorType};
use crate::error::ImageResult;

/// A pull decoder for one image format.
pub trait ImageDecoder {
    /// `(width, height)` in pixels.
    fn dimensions(&self) -> (u32, u32);

    /// The colour type [`Self::read_image`] will produce.
    fn color_type(&self) -> ColorType;

    /// The exact length [`Self::read_image`]'s `buf` must have:
    /// `width * height * color_type.bytes_per_pixel()`, saturating rather
    /// than overflowing.
    fn total_bytes(&self) -> u64 {
        let (w, h) = self.dimensions();
        u64::from(w)
            .saturating_mul(u64::from(h))
            .saturating_mul(u64::from(self.color_type().bytes_per_pixel()))
    }

    /// Decode the whole image into `buf`, native-endian.
    ///
    /// # Errors
    /// Any decode failure.
    ///
    /// # Panics
    /// Implementations panic if `buf.len() != self.total_bytes()`, matching
    /// `image::ImageDecoder::read_image`'s own contract.
    fn read_image(self, buf: &mut [u8]) -> ImageResult<()>
    where
        Self: Sized;
}

/// A push encoder for one image format.
pub trait ImageEncoder {
    /// Encode `buf` (`width * height` pixels of `color_type`, native-endian,
    /// row-major, no padding) to this encoder's sink.
    ///
    /// # Errors
    /// `color_type` is not one this format/encoder configuration can
    /// represent, `buf`'s length does not match `width`/`height`/
    /// `color_type`, or the underlying writer fails.
    fn write_image(
        self,
        buf: &[u8],
        width: u32,
        height: u32,
        color_type: ExtendedColorType,
    ) -> ImageResult<()>;
}

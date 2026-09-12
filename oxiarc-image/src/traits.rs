//! [`ImageDecoder`] / [`ImageEncoder`]: the two traits every
//! `codecs::{png,jpeg,tiff}` type implements.
//!
//! Trimmed from `image` 0.25's traits of the same name: no
//! `icc_profile`/`exif_metadata`/`xmp_metadata`/`orientation`/`set_limits`
//! default methods, and [`ImageDecoder`] is not object-safe (no
//! `read_image_boxed`/`Box<dyn ImageDecoder>` support), since nothing in
//! this crate needs to store a decoder behind a trait object — a caller
//! holding a concrete decoder uses [`crate::DynamicImage::from_decoder`]
//! directly instead, and [`crate::ImageReader::decode`] dispatches on
//! [`crate::ImageFormat`] with a plain three-arm `match`. A decoder's own
//! metadata accessors (`exif()`, `icc_profile()`, ...) remain on the
//! concrete `codecs::*::*Decoder` types, unchanged from the crate they
//! wrap.

use crate::color::{ColorType, ExtendedColorType};
use crate::error::{ImageError, ImageResult, ParameterError, ParameterErrorKind};

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
    /// `buf.len()` must be exactly [`Self::total_bytes`].
    ///
    /// # Deviation from `image`
    /// `image::ImageDecoder::read_image` *asserts* the buffer length and
    /// panics on a mismatch. Every implementation in this crate returns
    /// [`crate::ImageError::Parameter`] instead, in both directions (too
    /// short **and** too long — a caller who over-allocates gets an error
    /// rather than a silently half-filled buffer). A wrong buffer length is
    /// a caller mistake either way; making it an ordinary `Err` keeps the
    /// whole decode surface of this crate panic-free, which is the property
    /// `tests/adversarial.rs` pins for untrusted input.
    ///
    /// # Errors
    /// `buf.len() != self.total_bytes()`, or any decode failure.
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

/// The [`ImageDecoder::read_image`] buffer-length precondition, shared by
/// all three `codecs::*` decoders so that the check — and the error it
/// produces — cannot drift apart between formats.
///
/// # Errors
/// [`ImageError::Parameter`] when `buf.len()` is not exactly `expected`.
pub(crate) fn check_read_buffer(buf: &[u8], expected: u64) -> ImageResult<()> {
    if buf.len() as u64 == expected {
        Ok(())
    } else {
        Err(ImageError::Parameter(ParameterError::from_kind(
            ParameterErrorKind::Generic(format!(
                "read_image needs a buffer of exactly {expected} bytes, got {}",
                buf.len()
            )),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_read_buffer_check_accepts_only_the_exact_length() {
        assert!(check_read_buffer(&[0u8; 12], 12).is_ok());
        assert!(matches!(
            check_read_buffer(&[0u8; 11], 12),
            Err(ImageError::Parameter(_))
        ));
        assert!(matches!(
            check_read_buffer(&[0u8; 13], 12),
            Err(ImageError::Parameter(_))
        ));
        assert!(check_read_buffer(&[], 0).is_ok());
    }

    #[test]
    fn the_read_buffer_check_message_names_both_lengths() {
        let err = check_read_buffer(&[0u8; 3], 12).expect_err("length mismatch");
        let text = err.to_string();
        assert!(
            text.contains("12"),
            "message should name the expected length: {text}"
        );
        assert!(
            text.contains('3'),
            "message should name the actual length: {text}"
        );
    }
}

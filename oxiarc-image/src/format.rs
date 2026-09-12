//! Image container formats: the [`ImageFormat`] enumeration, extension and
//! MIME-type lookup, and magic-byte sniffing.
//!
//! Shaped after `image` 0.25's `ImageFormat` so that `match` statements and
//! extension tables written against `image` keep compiling unchanged: every
//! variant the real crate has is present here too, not only the three this
//! crate can actually decode. [`ImageFormat::can_decode`] /
//! [`ImageFormat::can_encode`] are the honest, additive way to ask "does
//! `oxiarc-image` actually support this one", and every entry point that
//! dispatches on format ([`crate::open`], [`crate::load_from_memory`],
//! [`ImageReader`](crate::ImageReader)) returns
//! [`ImageError::Unsupported`] by name for the eleven it does not.

use std::ffi::OsStr;
use std::path::Path;

use crate::error::{ImageError, ImageFormatHint, ImageResult};

/// An enumeration of image container formats.
///
/// Only [`ImageFormat::Png`], [`ImageFormat::Jpeg`] and [`ImageFormat::Tiff`]
/// are actually decoded or encoded by this crate ([`ImageFormat::can_decode`]
/// / [`ImageFormat::can_encode`]); the rest of the variants exist so that
/// code written against `image::ImageFormat` — `match` arms, extension
/// tables, `format == ImageFormat::Gif` checks — keeps compiling after the
/// `png = { package = "oxiarc-image" }` rename, and fails at the format
/// dispatch with a named [`ImageError::Unsupported`] rather than at
/// `cargo build`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[non_exhaustive]
pub enum ImageFormat {
    /// An image in PNG format. Decoded and encoded.
    Png,
    /// An image in JPEG format. Decoded and encoded.
    Jpeg,
    /// An image in TIFF format. Decoded and encoded.
    Tiff,
    /// An image in GIF format. Not implemented by this crate.
    Gif,
    /// An image in WebP format. Not implemented by this crate.
    WebP,
    /// An image in general PNM format. Not implemented by this crate.
    Pnm,
    /// An image in TGA format. Not implemented by this crate.
    Tga,
    /// An image in DDS format. Not implemented by this crate.
    Dds,
    /// An image in BMP format. Not implemented by this crate.
    Bmp,
    /// An image in ICO format. Not implemented by this crate.
    Ico,
    /// An image in Radiance HDR format. Not implemented by this crate.
    Hdr,
    /// An image in OpenEXR format. Not implemented by this crate.
    OpenExr,
    /// An image in farbfeld format. Not implemented by this crate.
    Farbfeld,
    /// An image in AVIF format. Not implemented by this crate.
    Avif,
    /// An image in QOI format. Not implemented by this crate.
    Qoi,
}

/// One (signature, mask, format) row of the magic-byte sniff table.
/// An empty mask means an exact-prefix match.
type MagicRow = (&'static [u8], &'static [u8], ImageFormat);

/// Ordered the same way `image` orders it: formats with a short, generic
/// prefix (like the two-byte PNM codes) come after formats with a long,
/// specific one, so a specific match always wins first.
static MAGIC_BYTES: &[MagicRow] = &[
    (b"\x89PNG\r\n\x1a\n", b"", ImageFormat::Png),
    (&[0xFF, 0xD8, 0xFF], b"", ImageFormat::Jpeg),
    (b"MM\x00*", b"", ImageFormat::Tiff),
    (b"II*\x00", b"", ImageFormat::Tiff),
    // BigTIFF: version word 43 (0x2B) instead of classic TIFF's 42 (0x2A).
    (b"MM\x00+", b"", ImageFormat::Tiff),
    (b"II+\x00", b"", ImageFormat::Tiff),
    (b"GIF89a", b"", ImageFormat::Gif),
    (b"GIF87a", b"", ImageFormat::Gif),
    (
        b"RIFF\0\0\0\0WEBP",
        b"\xFF\xFF\xFF\xFF\0\0\0\0",
        ImageFormat::WebP,
    ),
    (b"DDS ", b"", ImageFormat::Dds),
    (b"BM", b"", ImageFormat::Bmp),
    (&[0, 0, 1, 0], b"", ImageFormat::Ico),
    (b"#?RADIANCE", b"", ImageFormat::Hdr),
    // The mask is deliberately *shorter* than the signature: bytes past its
    // end are matched with an implicit `0xFF` (see `guess_format_impl`), so
    // this reads "the first two bytes of the box size must be zero, the next
    // two are anything, and bytes 4..12 must be `ftypavif`" -- `image`
    // 0.25's own row, byte for byte. A mask padded out to the signature's
    // full length with zeros would instead demand `byte & 0 == b'f'` at
    // offset 4, which no input can satisfy, and the row would never match
    // anything at all.
    (b"\0\0\0\0ftypavif", b"\xFF\xFF\0\0", ImageFormat::Avif),
    (&[0x76, 0x2f, 0x31, 0x01], b"", ImageFormat::OpenExr),
    (b"qoif", b"", ImageFormat::Qoi),
    (b"P1", b"", ImageFormat::Pnm),
    (b"P2", b"", ImageFormat::Pnm),
    (b"P3", b"", ImageFormat::Pnm),
    (b"P4", b"", ImageFormat::Pnm),
    (b"P5", b"", ImageFormat::Pnm),
    (b"P6", b"", ImageFormat::Pnm),
    (b"P7", b"", ImageFormat::Pnm),
    (b"farbfeld", b"", ImageFormat::Farbfeld),
];

impl ImageFormat {
    /// Whether [`crate::open`], [`crate::load_from_memory`] and
    /// [`ImageReader`](crate::ImageReader) actually decode this format.
    ///
    /// ```
    /// use oxiarc_image::ImageFormat;
    /// assert!(ImageFormat::Png.can_decode());
    /// assert!(!ImageFormat::Gif.can_decode());
    /// ```
    #[must_use]
    pub const fn can_decode(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Tiff)
    }

    /// Whether [`crate::DynamicImage::save`] / `write_to` actually encode
    /// this format. Identical to [`Self::can_decode`] for every format this
    /// crate touches.
    #[must_use]
    pub const fn can_encode(self) -> bool {
        self.can_decode()
    }

    /// The format named by a path's extension, matched case-insensitively.
    ///
    /// ```
    /// use oxiarc_image::ImageFormat;
    /// let format = ImageFormat::from_extension("jpg");
    /// assert_eq!(format, Some(ImageFormat::Jpeg));
    /// ```
    #[must_use]
    pub fn from_extension<S: AsRef<OsStr>>(ext: S) -> Option<Self> {
        fn inner(ext: &OsStr) -> Option<ImageFormat> {
            let ext = ext.to_str()?.to_ascii_lowercase();
            Some(match ext.as_str() {
                "png" | "apng" => ImageFormat::Png,
                "jpg" | "jpeg" | "jfif" => ImageFormat::Jpeg,
                "tif" | "tiff" => ImageFormat::Tiff,
                "gif" => ImageFormat::Gif,
                "webp" => ImageFormat::WebP,
                "tga" => ImageFormat::Tga,
                "dds" => ImageFormat::Dds,
                "bmp" => ImageFormat::Bmp,
                "ico" => ImageFormat::Ico,
                "hdr" => ImageFormat::Hdr,
                "exr" => ImageFormat::OpenExr,
                "pbm" | "pam" | "ppm" | "pgm" | "pnm" => ImageFormat::Pnm,
                "ff" => ImageFormat::Farbfeld,
                "avif" => ImageFormat::Avif,
                "qoi" => ImageFormat::Qoi,
                _ => return None,
            })
        }
        inner(ext.as_ref())
    }

    /// The format named by a path's extension.
    ///
    /// # Errors
    /// [`ImageError::Unsupported`] when the path has no extension, or one
    /// [`Self::from_extension`] does not recognise.
    ///
    /// ```
    /// use oxiarc_image::ImageFormat;
    /// assert_eq!(ImageFormat::from_path("photo.png").expect("recognised"), ImageFormat::Png);
    /// ```
    pub fn from_path<P: AsRef<Path>>(path: P) -> ImageResult<Self> {
        let path = path.as_ref();
        let ext = path.extension();
        ext.and_then(Self::from_extension).ok_or_else(|| {
            let hint = match ext {
                None => ImageFormatHint::Unknown,
                Some(os) => ImageFormatHint::PathExtension(os.into()),
            };
            ImageError::Unsupported(hint.into())
        })
    }

    /// The format named by an `image/*` MIME type.
    #[must_use]
    pub fn from_mime_type<M: AsRef<str>>(mime_type: M) -> Option<Self> {
        Some(match mime_type.as_ref() {
            "image/png" => Self::Png,
            "image/jpeg" => Self::Jpeg,
            "image/tiff" => Self::Tiff,
            "image/gif" => Self::Gif,
            "image/webp" => Self::WebP,
            "image/x-targa" | "image/x-tga" => Self::Tga,
            "image/vnd-ms.dds" => Self::Dds,
            "image/bmp" => Self::Bmp,
            "image/x-icon" | "image/vnd.microsoft.icon" => Self::Ico,
            "image/vnd.radiance" => Self::Hdr,
            "image/x-exr" => Self::OpenExr,
            "image/x-portable-bitmap"
            | "image/x-portable-graymap"
            | "image/x-portable-pixmap"
            | "image/x-portable-anymap" => Self::Pnm,
            "image/avif" => Self::Avif,
            "image/x-qoi" => Self::Qoi,
            _ => return None,
        })
    }

    /// The canonical `image/*` MIME type for this format, or
    /// `"application/octet-stream"` if none is registered.
    #[must_use]
    pub const fn to_mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Tiff => "image/tiff",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
            Self::Tga => "image/x-targa",
            Self::Dds => "image/vnd-ms.dds",
            Self::Bmp => "image/bmp",
            Self::Ico => "image/x-icon",
            Self::Hdr => "image/vnd.radiance",
            Self::OpenExr => "image/x-exr",
            Self::Pnm => "image/x-portable-anymap",
            Self::Avif => "image/avif",
            Self::Qoi => "image/x-qoi",
            Self::Farbfeld => "application/octet-stream",
        }
    }

    /// One representative filename extension for this format.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Tiff => "tiff",
            Self::Gif => "gif",
            Self::WebP => "webp",
            Self::Tga => "tga",
            Self::Dds => "dds",
            Self::Bmp => "bmp",
            Self::Ico => "ico",
            Self::Hdr => "hdr",
            Self::OpenExr => "exr",
            Self::Pnm => "pnm",
            Self::Farbfeld => "ff",
            Self::Avif => "avif",
            Self::Qoi => "qoi",
        }
    }
}

/// Guess a byte buffer's image format from its leading magic bytes.
///
/// TGA is not sniffable (it has no signature) and is never returned here,
/// matching `image::guess_format`.
///
/// # Errors
/// [`ImageError::Unsupported`] wrapping [`ImageFormatHint::Unknown`] when no
/// entry in the sniff table matches.
///
/// ```
/// use oxiarc_image::guess_format;
/// let png_sig = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
/// assert_eq!(guess_format(&png_sig).expect("sniff"), oxiarc_image::ImageFormat::Png);
/// assert!(guess_format(b"not an image").is_err());
/// ```
pub fn guess_format(buffer: &[u8]) -> ImageResult<ImageFormat> {
    guess_format_impl(buffer)
        .ok_or_else(|| ImageError::Unsupported(ImageFormatHint::Unknown.into()))
}

/// The infallible core of [`guess_format`]; used by [`crate::ImageReader`],
/// which must not error just because the format could not be guessed.
#[must_use]
pub(crate) fn guess_format_impl(buffer: &[u8]) -> Option<ImageFormat> {
    for &(signature, mask, format) in MAGIC_BYTES {
        if mask.is_empty() {
            if buffer.starts_with(signature) {
                return Some(format);
            }
        } else if buffer.len() >= signature.len()
            && buffer
                .iter()
                .zip(signature)
                .zip(mask.iter().chain(std::iter::repeat(&0xFF)))
                .all(|((&byte, &sig), &mask)| byte & mask == sig)
        {
            return Some(format);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_round_trips_the_three_supported_formats() {
        assert_eq!(ImageFormat::from_extension("png"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_extension("APNG"), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::from_extension("jpg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension("jpeg"), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::from_extension("tif"), Some(ImageFormat::Tiff));
        assert_eq!(ImageFormat::from_extension("tiff"), Some(ImageFormat::Tiff));
        assert_eq!(ImageFormat::from_extension("xyz"), None);
    }

    #[test]
    fn extension_also_recognises_formats_this_crate_cannot_decode() {
        assert_eq!(ImageFormat::from_extension("gif"), Some(ImageFormat::Gif));
        assert_eq!(ImageFormat::from_extension("webp"), Some(ImageFormat::WebP));
        assert!(!ImageFormat::Gif.can_decode());
    }

    #[test]
    fn can_decode_is_exactly_the_three_codecs() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
            assert!(format.can_decode());
            assert!(format.can_encode());
        }
        for format in [
            ImageFormat::Gif,
            ImageFormat::WebP,
            ImageFormat::Bmp,
            ImageFormat::Ico,
            ImageFormat::Avif,
        ] {
            assert!(!format.can_decode());
            assert!(!format.can_encode());
        }
    }

    #[test]
    fn from_path_errors_with_a_useful_hint() {
        let err = ImageFormat::from_path("photo.xyz").unwrap_err();
        assert!(matches!(err, ImageError::Unsupported(_)));
        assert!(ImageFormat::from_path("no_extension").is_err());
    }

    #[test]
    fn mime_type_round_trip() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
            let mime = format.to_mime_type();
            assert_eq!(ImageFormat::from_mime_type(mime), Some(format));
        }
    }

    #[test]
    fn guess_format_sniffs_png_jpeg_tiff() {
        assert_eq!(
            guess_format(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap(),
            ImageFormat::Png
        );
        assert_eq!(
            guess_format(&[0xFF, 0xD8, 0xFF, 0xE0]).unwrap(),
            ImageFormat::Jpeg
        );
        assert_eq!(
            guess_format(b"II*\x00\x08\x00\x00\x00").unwrap(),
            ImageFormat::Tiff
        );
        assert_eq!(
            guess_format(b"MM\x00*\x00\x00\x00\x08").unwrap(),
            ImageFormat::Tiff
        );
    }

    #[test]
    fn guess_format_sniffs_bigtiff_both_byte_orders() {
        assert_eq!(
            guess_format(b"II+\x00\x08\x00\x00\x00").unwrap(),
            ImageFormat::Tiff
        );
        assert_eq!(
            guess_format(b"MM\x00+\x00\x08\x00\x00").unwrap(),
            ImageFormat::Tiff
        );
    }

    #[test]
    fn guess_format_recognises_unsupported_formats_by_name() {
        assert_eq!(guess_format(b"GIF89a").unwrap(), ImageFormat::Gif);
        assert_eq!(guess_format(b"BM\x00\x00").unwrap(), ImageFormat::Bmp);
    }

    /// The masked rows (`WebP`, `Avif`) are the ones a wrong mask length
    /// silently kills: `byte & mask == signature` can never hold at an
    /// offset where the mask byte is `0` but the signature byte is not, so a
    /// mask padded to the signature's length turns the whole row into dead
    /// code that reports nothing and fails no test. Both directions are
    /// pinned here: the real header must match, and near-misses must not.
    #[test]
    fn guess_format_sniffs_the_masked_rows_webp_and_avif() {
        let mut avif = Vec::new();
        avif.extend_from_slice(&[0x00, 0x00, 0x00, 0x20]); // box size: don't care
        avif.extend_from_slice(b"ftypavif");
        assert_eq!(guess_format(&avif).expect("sniff avif"), ImageFormat::Avif);

        let mut webp = Vec::new();
        webp.extend_from_slice(b"RIFF");
        webp.extend_from_slice(&1234u32.to_le_bytes()); // file size: don't care
        webp.extend_from_slice(b"WEBP");
        assert_eq!(guess_format(&webp).expect("sniff webp"), ImageFormat::WebP);
    }

    #[test]
    fn guess_format_does_not_over_match_the_masked_rows() {
        // All-zero bytes share the AVIF row's two leading zeros but carry no
        // `ftypavif` brand: the mask must still reject them.
        assert!(guess_format(&[0u8; 32]).is_err());
        // The right brand at the wrong offset is not AVIF either.
        let mut shifted = vec![0u8; 3];
        shifted.extend_from_slice(b"ftypavif");
        shifted.extend_from_slice(&[0u8; 8]);
        assert!(guess_format(&shifted).is_err());
        // `RIFF` with a non-`WEBP` form type is some other RIFF file.
        let mut riff = Vec::new();
        riff.extend_from_slice(b"RIFF");
        riff.extend_from_slice(&8u32.to_le_bytes());
        riff.extend_from_slice(b"WAVE");
        assert!(guess_format(&riff).is_err());
    }

    /// A buffer shorter than a masked row's signature must not match it,
    /// and must not panic either.
    #[test]
    fn guess_format_handles_buffers_shorter_than_every_signature() {
        let full: &[u8] = b"\0\0\0\x20ftypavif";
        for n in 0..full.len() {
            assert!(
                guess_format(&full[..n]).is_err(),
                "a {n}-byte prefix of an AVIF header must not sniff as AVIF"
            );
        }
        assert_eq!(
            guess_format(full).expect("the whole header does sniff"),
            ImageFormat::Avif
        );
    }

    #[test]
    fn guess_format_rejects_garbage() {
        assert!(guess_format(b"not an image at all").is_err());
        assert!(guess_format(&[]).is_err());
    }
}

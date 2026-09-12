//! [`ImageReader`] and the top-level `open`/`load_from_memory*` free
//! functions.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use crate::codecs;
use crate::dynamic::DynamicImage;
use crate::error::{ImageError, ImageFormatHint, ImageResult, UnsupportedError};
use crate::format::{ImageFormat, guess_format_impl};
use crate::traits::ImageDecoder;

fn unsupported_format(format: ImageFormat) -> ImageError {
    ImageError::Unsupported(UnsupportedError::from_format_and_kind(
        format.into(),
        crate::error::UnsupportedErrorKind::Format(format.into()),
    ))
}

/// A multi-format image reader: wraps a source, figures out (or is told)
/// its format, and dispatches to the matching `codecs::*` decoder.
///
/// Only [`ImageFormat::can_decode`] formats (PNG, JPEG, TIFF) actually
/// decode; [`Self::decode`]/[`Self::into_dimensions`] on any other
/// recognised-but-unimplemented format return
/// [`crate::ImageError::Unsupported`] by name.
///
/// # Deviation from `image`: no `Box<dyn ImageDecoder>`
/// `image::ImageReader::into_decoder` returns a boxed, object-safe
/// decoder; this crate has no such method; [`Self::decode`] and
/// [`Self::into_dimensions`] dispatch on [`ImageFormat`] with a plain
/// three-arm `match` instead; see [`crate::traits`] module docs.
pub struct ImageReader<R: BufRead + Seek> {
    inner: R,
    format: Option<ImageFormat>,
}

impl<R: BufRead + Seek> ImageReader<R> {
    /// A reader with no format set yet.
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            format: None,
        }
    }

    /// A reader with the format already known.
    pub fn with_format(inner: R, format: ImageFormat) -> Self {
        Self {
            inner,
            format: Some(format),
        }
    }

    /// The currently-known format, if any.
    #[must_use]
    pub fn format(&self) -> Option<ImageFormat> {
        self.format
    }

    /// Supply the format explicitly.
    pub fn set_format(&mut self, format: ImageFormat) {
        self.format = Some(format);
    }

    /// Forget the current format.
    pub fn clear_format(&mut self) {
        self.format = None;
    }

    /// Unwrap the underlying reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Sniff the magic bytes at the *current* stream position and adopt
    /// that format, leaving the format unchanged if none matches. Rewinds
    /// back to the starting position either way.
    ///
    /// # Errors
    /// An I/O error seeking or reading; never a failure to guess (that
    /// just leaves the format as it was, matching `image::ImageReader`).
    pub fn with_guessed_format(mut self) -> io::Result<Self> {
        let mut start = [0u8; 16];
        let cur = self.inner.stream_position()?;
        let n = io::copy(
            &mut (&mut self.inner).take(16),
            &mut Cursor::new(&mut start[..]),
        )?;
        self.inner.seek(SeekFrom::Start(cur))?;
        if let Some(format) = guess_format_impl(&start[..n as usize]) {
            self.format = Some(format);
        }
        Ok(self)
    }

    fn require_format(&self) -> ImageResult<ImageFormat> {
        self.format
            .ok_or_else(|| ImageError::Unsupported(ImageFormatHint::Unknown.into()))
    }

    /// `(width, height)`, without decoding pixel data.
    ///
    /// # Errors
    /// No format is known, the format is not one this crate decodes, or
    /// the header is malformed.
    pub fn into_dimensions(self) -> ImageResult<(u32, u32)> {
        match self.require_format()? {
            ImageFormat::Png => Ok(codecs::png::PngDecoder::new(self.inner)?.dimensions()),
            ImageFormat::Jpeg => Ok(codecs::jpeg::JpegDecoder::new(self.inner)?.dimensions()),
            ImageFormat::Tiff => Ok(codecs::tiff::TiffDecoder::new(self.inner)?.dimensions()),
            other => Err(unsupported_format(other)),
        }
    }

    /// Decode the whole image.
    ///
    /// # Errors
    /// No format is known, the format is not one this crate decodes, or a
    /// decode failure.
    pub fn decode(self) -> ImageResult<DynamicImage> {
        match self.require_format()? {
            ImageFormat::Png => {
                DynamicImage::from_decoder(codecs::png::PngDecoder::new(self.inner)?)
            }
            ImageFormat::Jpeg => {
                DynamicImage::from_decoder(codecs::jpeg::JpegDecoder::new(self.inner)?)
            }
            ImageFormat::Tiff => {
                DynamicImage::from_decoder(codecs::tiff::TiffDecoder::new(self.inner)?)
            }
            other => Err(unsupported_format(other)),
        }
    }
}

impl ImageReader<BufReader<File>> {
    /// Open a file, guessing the format from its path extension (not its
    /// content — chain [`Self::with_guessed_format`] for that).
    ///
    /// # Errors
    /// The file cannot be opened. A missing or unrecognised extension is
    /// **not** an error here, matching `image::ImageReader::open`; it
    /// surfaces later, from [`Self::decode`]/[`Self::into_dimensions`].
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let path = path.as_ref();
        let format = path.extension().and_then(ImageFormat::from_extension);
        Ok(Self {
            inner: BufReader::new(File::open(path)?),
            format,
        })
    }
}

/// Open and decode an image file, guessing the format from its path
/// extension.
///
/// # Errors
/// The file cannot be opened, its extension names an unrecognised or
/// unsupported format, or the decode fails.
///
/// ```
/// let path = std::env::temp_dir().join("oxiarc_image_open_doctest.png");
/// # {
/// #     use oxiarc_image::{DynamicImage, ImageBuffer, Rgb};
/// #     let buf: ImageBuffer<Rgb<u8>> = ImageBuffer::new(2, 2);
/// #     DynamicImage::ImageRgb8(buf).save(&path).expect("seed file");
/// # }
/// let image = oxiarc_image::open(&path)?;
/// assert_eq!(image.dimensions(), (2, 2));
/// # std::fs::remove_file(&path).ok();
/// # Ok::<(), oxiarc_image::ImageError>(())
/// ```
pub fn open<P: AsRef<Path>>(path: P) -> ImageResult<DynamicImage> {
    ImageReader::open(path)?.decode()
}

/// `(width, height)` of an image file, without decoding pixel data.
///
/// # Errors
/// As [`open`].
pub fn image_dimensions<P: AsRef<Path>>(path: P) -> ImageResult<(u32, u32)> {
    ImageReader::open(path)?.into_dimensions()
}

/// Decode an image already in memory, sniffing its format from the leading
/// bytes.
///
/// # Errors
/// The bytes are not one of PNG/JPEG/TIFF, or the decode fails.
pub fn load_from_memory(bytes: &[u8]) -> ImageResult<DynamicImage> {
    let format = crate::format::guess_format(bytes)?;
    load_from_memory_with_format(bytes, format)
}

/// Decode an image already in memory, in an explicitly given format.
///
/// # Errors
/// `format` is not one this crate decodes, or the decode fails.
pub fn load_from_memory_with_format(
    bytes: &[u8],
    format: ImageFormat,
) -> ImageResult<DynamicImage> {
    ImageReader::with_format(Cursor::new(bytes), format).decode()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ImageBuffer, Rgb};

    fn sample_png() -> Vec<u8> {
        let buf: ImageBuffer<Rgb<u8>> =
            ImageBuffer::from_fn(3, 2, |x, y| Rgb::new(x as u8, y as u8, 0));
        let mut out = Vec::new();
        DynamicImage::ImageRgb8(buf)
            .write_to(Cursor::new(&mut out), ImageFormat::Png)
            .expect("encode");
        out
    }

    #[test]
    fn load_from_memory_sniffs_and_decodes() {
        let image = load_from_memory(&sample_png()).expect("decode");
        assert_eq!(image.dimensions(), (3, 2));
        assert_eq!(image.color(), ColorTypeAlias::Rgb8);
    }

    // Local alias purely so the test above reads naturally without a second
    // `use` line pulling in the whole `color` module.
    use crate::ColorType as ColorTypeAlias;

    #[test]
    fn load_from_memory_with_format_skips_sniffing() {
        let bytes = sample_png();
        let image = load_from_memory_with_format(&bytes, ImageFormat::Png).expect("decode");
        assert_eq!(image.dimensions(), (3, 2));
    }

    #[test]
    fn load_from_memory_rejects_garbage() {
        assert!(load_from_memory(b"not an image").is_err());
    }

    #[test]
    fn image_reader_new_then_with_guessed_format() {
        let bytes = sample_png();
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .expect("sniff");
        assert_eq!(reader.format(), Some(ImageFormat::Png));
        let dims = reader.into_dimensions().expect("dimensions");
        assert_eq!(dims, (3, 2));
    }

    #[test]
    fn image_reader_with_no_format_set_is_a_named_error() {
        let reader = ImageReader::new(Cursor::new(sample_png()));
        let err = reader.decode().unwrap_err();
        assert!(matches!(err, ImageError::Unsupported(_)));
    }

    #[test]
    fn image_reader_open_reads_the_extension_not_the_content() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "oxiarc_image_reader_test_{}.png",
            std::process::id()
        ));
        std::fs::write(&path, sample_png()).expect("write fixture");
        let reader = ImageReader::open(&path).expect("open");
        assert_eq!(reader.format(), Some(ImageFormat::Png));
        let image = reader.decode().expect("decode");
        assert_eq!(image.dimensions(), (3, 2));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_format_and_clear_format() {
        let mut reader = ImageReader::new(Cursor::new(sample_png()));
        assert_eq!(reader.format(), None);
        reader.set_format(ImageFormat::Png);
        assert_eq!(reader.format(), Some(ImageFormat::Png));
        reader.clear_format();
        assert_eq!(reader.format(), None);
    }

    #[test]
    fn decoding_an_unsupported_but_recognised_format_is_a_named_error() {
        let reader = ImageReader::with_format(Cursor::new(b"GIF89a".to_vec()), ImageFormat::Gif);
        let err = reader.decode().unwrap_err();
        assert!(matches!(err, ImageError::Unsupported(_)));
    }
}

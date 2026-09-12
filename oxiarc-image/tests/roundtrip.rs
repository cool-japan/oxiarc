//! Round trips through every format this crate decodes/encodes (PNG, JPEG,
//! TIFF), covering every pixel type each format can carry.
//!
//! PNG and TIFF are both lossless and both carry all eight integer
//! [`ColorType`]s exactly, so those two formats assert byte-for-byte
//! equality after an encode/decode round trip. TIFF additionally carries
//! the two 32-bit float types. PNG has no float sample format at all
//! ([`ExtendedColorType::Rgb32F`]/`Rgba32F` are a named
//! [`ImageError::Unsupported`] there, asserted below rather than assumed).
//! JPEG is always lossy (DCT + chroma subsampling) and has no alpha
//! channel, so its round trips assert closeness, not equality, and its
//! non-`Luma`/`Rgb` inputs assert the documented narrowing instead of a
//! round trip at all -- see `jpeg_encode_narrows_colour_types_without_a_plain_rgb_or_luma_decode_path`.

use oxiarc_image::{
    ColorType, DynamicImage, ExtendedColorType, ImageBuffer, ImageError, ImageFormat, Luma, LumaA,
    Rgb, Rgba,
};
use std::io::Cursor;

fn encode_decode(image: &DynamicImage, format: ImageFormat) -> DynamicImage {
    let mut bytes = Vec::new();
    image
        .write_to(Cursor::new(&mut bytes), format)
        .unwrap_or_else(|e| panic!("encode to {format:?} failed: {e}"));
    oxiarc_image::load_from_memory_with_format(&bytes, format)
        .unwrap_or_else(|e| panic!("decode from {format:?} failed: {e}"))
}

/// Every [`DynamicImage`] variant this crate defines carries `PartialEq`
/// buffers; the enum itself does not derive `PartialEq` (real `image` does
/// not either), so this is the exhaustive-per-variant comparison every
/// round-trip test below shares. `DynamicImage` is `#[non_exhaustive]`, so
/// (unlike a match written inside the crate itself) this integration test
/// -- a separate crate, as far as that attribute is concerned -- must carry
/// a wildcard arm even though these ten are the only variants that exist.
fn assert_same_pixels(original: &DynamicImage, decoded: &DynamicImage) {
    match (original, decoded) {
        (DynamicImage::ImageLuma8(a), DynamicImage::ImageLuma8(b)) => assert_eq!(a, b),
        (DynamicImage::ImageLumaA8(a), DynamicImage::ImageLumaA8(b)) => assert_eq!(a, b),
        (DynamicImage::ImageRgb8(a), DynamicImage::ImageRgb8(b)) => assert_eq!(a, b),
        (DynamicImage::ImageRgba8(a), DynamicImage::ImageRgba8(b)) => assert_eq!(a, b),
        (DynamicImage::ImageLuma16(a), DynamicImage::ImageLuma16(b)) => assert_eq!(a, b),
        (DynamicImage::ImageLumaA16(a), DynamicImage::ImageLumaA16(b)) => assert_eq!(a, b),
        (DynamicImage::ImageRgb16(a), DynamicImage::ImageRgb16(b)) => assert_eq!(a, b),
        (DynamicImage::ImageRgba16(a), DynamicImage::ImageRgba16(b)) => assert_eq!(a, b),
        (DynamicImage::ImageRgb32F(a), DynamicImage::ImageRgb32F(b)) => assert_eq!(a, b),
        (DynamicImage::ImageRgba32F(a), DynamicImage::ImageRgba32F(b)) => assert_eq!(a, b),
        (a, b) => panic!(
            "colour type changed across round trip: {:?} -> {:?}",
            a.color(),
            b.color()
        ),
    }
}

fn sample_luma8() -> DynamicImage {
    DynamicImage::ImageLuma8(ImageBuffer::from_fn(3, 2, |x, y| {
        Luma::new((x * 40 + y * 7) as u8)
    }))
}

fn sample_luma_alpha8() -> DynamicImage {
    DynamicImage::ImageLumaA8(ImageBuffer::from_fn(3, 2, |x, y| {
        LumaA::new((x * 40) as u8, (255 - y * 30) as u8)
    }))
}

fn sample_rgb8() -> DynamicImage {
    DynamicImage::ImageRgb8(ImageBuffer::from_fn(3, 2, |x, y| {
        Rgb::new((x * 40) as u8, (y * 60) as u8, 128)
    }))
}

fn sample_rgba8() -> DynamicImage {
    DynamicImage::ImageRgba8(ImageBuffer::from_fn(3, 2, |x, y| {
        Rgba::new((x * 40) as u8, (y * 60) as u8, 128, (200 + x) as u8)
    }))
}

fn sample_luma16() -> DynamicImage {
    DynamicImage::ImageLuma16(ImageBuffer::from_fn(3, 2, |x, y| {
        Luma::new((x * 4000 + y * 777).min(u16::MAX as u32) as u16)
    }))
}

fn sample_luma_alpha16() -> DynamicImage {
    DynamicImage::ImageLumaA16(ImageBuffer::from_fn(3, 2, |x, y| {
        LumaA::new((x as u16) * 4000, 60000 - (y as u16) * 1000)
    }))
}

fn sample_rgb16() -> DynamicImage {
    DynamicImage::ImageRgb16(ImageBuffer::from_fn(3, 2, |x, y| {
        Rgb::new((x as u16) * 4000, (y as u16) * 6000, 0x1234)
    }))
}

fn sample_rgba16() -> DynamicImage {
    DynamicImage::ImageRgba16(ImageBuffer::from_fn(3, 2, |x, y| {
        Rgba::new((x as u16) * 4000, (y as u16) * 6000, 0x1234, 0xABCD)
    }))
}

fn sample_rgb32f() -> DynamicImage {
    DynamicImage::ImageRgb32F(ImageBuffer::from_fn(3, 2, |x, y| {
        Rgb::new(x as f32 * 0.5, y as f32 * 0.25, 0.75)
    }))
}

fn sample_rgba32f() -> DynamicImage {
    DynamicImage::ImageRgba32F(ImageBuffer::from_fn(3, 2, |x, y| {
        Rgba::new(x as f32 * 0.5, y as f32 * 0.25, 0.75, 1.0 - x as f32 * 0.1)
    }))
}

/// Generates one `#[test]` per `(name, format, sample builder)` row that
/// round-trips a sample image through `encode_decode` and asserts the
/// decoded image is pixel-identical, dimension-identical and
/// colour-type-identical to the original.
macro_rules! lossless_round_trip {
    ($name:ident, $format:expr, $sample:expr) => {
        #[test]
        fn $name() {
            let original = $sample();
            let decoded = encode_decode(&original, $format);
            assert_eq!(decoded.dimensions(), original.dimensions());
            assert_eq!(decoded.color(), original.color());
            assert_same_pixels(&original, &decoded);
        }
    };
}

mod png {
    use super::*;

    lossless_round_trip!(luma8, ImageFormat::Png, sample_luma8);
    lossless_round_trip!(luma_alpha8, ImageFormat::Png, sample_luma_alpha8);
    lossless_round_trip!(rgb8, ImageFormat::Png, sample_rgb8);
    lossless_round_trip!(rgba8, ImageFormat::Png, sample_rgba8);
    lossless_round_trip!(luma16, ImageFormat::Png, sample_luma16);
    lossless_round_trip!(luma_alpha16, ImageFormat::Png, sample_luma_alpha16);
    lossless_round_trip!(rgb16, ImageFormat::Png, sample_rgb16);
    lossless_round_trip!(rgba16, ImageFormat::Png, sample_rgba16);

    /// PNG has no floating-point sample format (Table 11.1 lists only
    /// integer bit depths); this crate reports that as a named
    /// `Unsupported` rather than silently narrowing to 8/16-bit integer.
    #[test]
    fn rgb32f_is_a_named_unsupported_error() {
        let err = sample_rgb32f()
            .write_to(Cursor::new(Vec::new()), ImageFormat::Png)
            .expect_err("PNG has no float sample format");
        assert!(matches!(err, ImageError::Unsupported(_)));
    }

    #[test]
    fn rgba32f_is_a_named_unsupported_error() {
        let err = sample_rgba32f()
            .write_to(Cursor::new(Vec::new()), ImageFormat::Png)
            .expect_err("PNG has no float sample format");
        assert!(matches!(err, ImageError::Unsupported(_)));
    }
}

mod tiff {
    use super::*;

    lossless_round_trip!(luma8, ImageFormat::Tiff, sample_luma8);
    lossless_round_trip!(luma_alpha8, ImageFormat::Tiff, sample_luma_alpha8);
    lossless_round_trip!(rgb8, ImageFormat::Tiff, sample_rgb8);
    lossless_round_trip!(rgba8, ImageFormat::Tiff, sample_rgba8);
    lossless_round_trip!(luma16, ImageFormat::Tiff, sample_luma16);
    lossless_round_trip!(luma_alpha16, ImageFormat::Tiff, sample_luma_alpha16);
    lossless_round_trip!(rgb16, ImageFormat::Tiff, sample_rgb16);
    lossless_round_trip!(rgba16, ImageFormat::Tiff, sample_rgba16);
    lossless_round_trip!(rgb32f, ImageFormat::Tiff, sample_rgb32f);
    lossless_round_trip!(rgba32f, ImageFormat::Tiff, sample_rgba32f);

    /// `ExtendedColorType::Cmyk8`/`Cmyk16` have no matching [`ColorType`] (no
    /// `DynamicImage` variant can hold four un-composited ink channels), so
    /// they are reachable only through [`oxiarc_image::codecs::tiff::TiffEncoder`]
    /// directly, never through `DynamicImage::write_to`. Encoding must
    /// still succeed, and the file must still decode -- through the
    /// generic `read_image_rgba8` fallback, to `Rgba8` -- rather than
    /// round-tripping byte-for-byte as CMYK.
    #[test]
    fn cmyk8_encodes_and_still_decodes_as_rgba8_through_the_fallback() {
        use oxiarc_image::codecs::tiff::TiffEncoder;
        use oxiarc_image::traits::ImageEncoder;

        let pixels: Vec<u8> = vec![0, 255, 0, 0, 255, 0, 255, 0]; // 2 CMYK pixels
        let mut out = Vec::new();
        TiffEncoder::new(Cursor::new(&mut out))
            .write_image(&pixels, 2, 1, ExtendedColorType::Cmyk8)
            .expect("cmyk8 must encode");

        let decoded =
            oxiarc_image::load_from_memory_with_format(&out, ImageFormat::Tiff).expect("decode");
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.color(), ColorType::Rgba8);
    }
}

mod jpeg {
    use super::*;

    /// JPEG is lossy at any quality; a mid-tone flat-colour source keeps
    /// the DCT/quantisation error small and predictable enough to bound
    /// tightly without the test depending on `oxiarc-jpeg`'s exact
    /// internals.
    fn flat_rgb(r: u8, g: u8, b: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(ImageBuffer::from_pixel(16, 16, Rgb::new(r, g, b)))
    }

    fn flat_luma(l: u8) -> DynamicImage {
        DynamicImage::ImageLuma8(ImageBuffer::from_pixel(16, 16, Luma::new(l)))
    }

    #[test]
    fn rgb8_round_trips_within_jpeg_tolerance() {
        let original = flat_rgb(180, 90, 40);
        let decoded = encode_decode(&original, ImageFormat::Jpeg);
        assert_eq!(decoded.dimensions(), (16, 16));
        assert_eq!(decoded.color(), ColorType::Rgb8);
        let DynamicImage::ImageRgb8(buf) = &decoded else {
            panic!("JPEG must decode to Rgb8, got {:?}", decoded.color());
        };
        for p in buf.pixels() {
            assert!(p.0[0].abs_diff(180) <= 6, "r drifted too far: {p:?}");
            assert!(p.0[1].abs_diff(90) <= 6, "g drifted too far: {p:?}");
            assert!(p.0[2].abs_diff(40) <= 6, "b drifted too far: {p:?}");
        }
    }

    #[test]
    fn luma8_round_trips_within_jpeg_tolerance() {
        let original = flat_luma(210);
        let decoded = encode_decode(&original, ImageFormat::Jpeg);
        assert_eq!(decoded.color(), ColorType::L8);
        let DynamicImage::ImageLuma8(buf) = &decoded else {
            panic!("JPEG must decode to Luma8, got {:?}", decoded.color());
        };
        for p in buf.pixels() {
            assert!(p.0[0].abs_diff(210) <= 4, "luma drifted too far: {p:?}");
        }
    }

    /// `DynamicImage::write_to(.., ImageFormat::Jpeg)` narrows any coloured
    /// source through `to_rgb8()` and any grayscale source through
    /// `to_luma8()` before it ever reaches the encoder (documented on
    /// `DynamicImage::write_to`), so an `Rgba8`/16-bit source round trips
    /// to `Rgb8`/`L8`, never back to its own colour type -- unlike PNG/TIFF
    /// above, this is *not* a bug, it is `image`'s own JPEG behaviour too
    /// (JPEG has no alpha channel to carry one home in).
    #[test]
    fn write_to_jpeg_narrows_rgba_and_reports_the_narrowed_colour() {
        let decoded = encode_decode(&sample_rgba8(), ImageFormat::Jpeg);
        assert_eq!(decoded.color(), ColorType::Rgb8);
    }

    #[test]
    fn write_to_jpeg_narrows_sixteen_bit_to_eight_bit() {
        let decoded = encode_decode(&sample_rgb16(), ImageFormat::Jpeg);
        assert_eq!(decoded.color(), ColorType::Rgb8);
    }

    /// Below the codec boundary (`codecs::jpeg`, not `DynamicImage`), a
    /// caller can still ask for CMYK/BGR(A) input colours directly --
    /// `oxiarc_jpeg::InputColor` supports them -- and the encode succeeds.
    /// Decoding that file back through this crate's `JpegDecoder` is a
    /// named `Unsupported`, though (see `codecs::jpeg`'s module docs):
    /// [`ColorType`] has no CMYK variant, matching `image` 0.25's own
    /// decoder, which also has no CMYK JPEG support. This is the
    /// "narrowing, not equality" contract for JPEG's non-`Luma`/`Rgb`
    /// inputs.
    #[test]
    fn jpeg_encode_narrows_colour_types_without_a_plain_rgb_or_luma_decode_path() {
        use oxiarc_image::codecs::jpeg::{JpegDecoder, JpegEncoder};
        use oxiarc_image::traits::ImageEncoder;

        let pixels = vec![0u8, 255, 0, 0, 255, 0, 255, 0]; // 2 CMYK pixels
        let mut out = Vec::new();
        JpegEncoder::new(&mut out)
            .write_image(&pixels, 2, 1, ExtendedColorType::Cmyk8)
            .expect("cmyk8 jpeg must encode");
        // `JpegDecoder` does not implement `Debug` (it wraps a live reader),
        // so `unwrap_err`/`expect_err` -- both of which require the `Ok`
        // side to be `Debug` -- cannot be used here; match instead.
        let result = JpegDecoder::new(Cursor::new(out));
        assert!(
            matches!(result, Err(ImageError::Unsupported(_))),
            "a CMYK JPEG must fail to decode as a named Unsupported error"
        );
    }
}

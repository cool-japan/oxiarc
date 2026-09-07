//! Format detection: magic-byte sniffing (content-based) versus extension
//! guessing (path-based), and where the two entry points that use each
//! (`open`/`ImageReader::open` vs. `load_from_memory`/`guess_format`)
//! agree and disagree.

use oxiarc_image::{ColorType, DynamicImage, ImageBuffer, ImageError, ImageFormat, Luma, Rgb};
use std::io::Cursor;

fn encoded(format: ImageFormat) -> Vec<u8> {
    let image = match format {
        ImageFormat::Jpeg => {
            DynamicImage::ImageLuma8(ImageBuffer::from_pixel(4, 4, Luma::new(128)))
        }
        _ => DynamicImage::ImageRgb8(ImageBuffer::from_fn(4, 4, |x, y| {
            Rgb::new((x * 60) as u8, (y * 60) as u8, 10)
        })),
    };
    let mut out = Vec::new();
    image
        .write_to(Cursor::new(&mut out), format)
        .unwrap_or_else(|e| panic!("fixture encode to {format:?} failed: {e}"));
    out
}

#[test]
fn guess_format_recognises_all_three_real_encodings() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
        let bytes = encoded(format);
        assert_eq!(
            oxiarc_image::guess_format(&bytes).expect("sniff"),
            format,
            "failed to sniff a real {format:?} encoding"
        );
    }
}

#[test]
fn load_from_memory_auto_detects_all_three_formats() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
        let bytes = encoded(format);
        let image = oxiarc_image::load_from_memory(&bytes)
            .unwrap_or_else(|e| panic!("auto-detect+decode {format:?} failed: {e}"));
        assert_eq!(image.dimensions(), (4, 4));
    }
}

#[test]
fn image_reader_with_guessed_format_matches_content_not_a_prior_guess() {
    use oxiarc_image::ImageReader;

    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
        let bytes = encoded(format);
        // Deliberately start from no format at all, then sniff.
        let reader = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .expect("sniff should not fail on well-formed input");
        assert_eq!(reader.format(), Some(format));
        let image = reader.decode().expect("decode after sniffing");
        assert_eq!(image.dimensions(), (4, 4));
    }
}

#[test]
fn tiff_sniffs_correctly_regardless_of_byte_order() {
    // Both byte-order signatures are real TIFF; this crate's own encoder
    // always picks one (native/little on most build machines), but the
    // *sniffer* must recognise both magic prefixes independent of what this
    // machine's encoder happens to produce -- see `format::guess_format`'s
    // own unit tests for the byte-level assertion. Here: confirm the
    // encoder's actual output round-trips through `guess_format` too, i.e.
    // this is not merely a synthetic-prefix test.
    let bytes = encoded(ImageFormat::Tiff);
    assert!(bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*"));
    assert_eq!(
        oxiarc_image::guess_format(&bytes).expect("sniff"),
        ImageFormat::Tiff
    );
}

#[test]
fn guess_format_on_unrelated_bytes_is_a_named_unsupported_error() {
    let err = oxiarc_image::guess_format(b"this is not an image, just prose")
        .expect_err("prose is not a recognised image format");
    assert!(matches!(err, ImageError::Unsupported(_)));
}

#[test]
fn from_extension_is_case_insensitive_for_all_three_formats() {
    for (ext, format) in [
        ("PNG", ImageFormat::Png),
        ("Png", ImageFormat::Png),
        ("JPG", ImageFormat::Jpeg),
        ("JPEG", ImageFormat::Jpeg),
        ("TIF", ImageFormat::Tiff),
        ("TIFF", ImageFormat::Tiff),
    ] {
        assert_eq!(ImageFormat::from_extension(ext), Some(format), "ext {ext}");
    }
}

/// `open`/`ImageReader::open` trust the *path extension*; `load_from_memory`
/// trusts the *content*. A file whose extension lies about its content is
/// exactly where the two disagree -- proving neither one secretly falls
/// back to the other.
#[test]
fn extension_and_content_based_detection_disagree_on_a_mislabeled_file() {
    let jpeg_bytes = encoded(ImageFormat::Jpeg);

    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "oxiarc_image_sniffing_test_mislabeled_{}.png",
        std::process::id()
    ));
    std::fs::write(&path, &jpeg_bytes).expect("write JPEG bytes under a .png name");

    // Path-extension-based: `open` believes the `.png` extension, tries the
    // PNG decoder on JPEG bytes, and fails with a decode error rather than
    // silently succeeding or panicking.
    let by_extension = oxiarc_image::open(&path).expect_err("PNG decoder must reject JPEG bytes");
    assert!(matches!(by_extension, ImageError::Decoding(_)));

    // Content-based: reading the same bytes through `load_from_memory`
    // (which never looks at the discarded path) sniffs JPEG correctly and
    // decodes.
    let by_content = oxiarc_image::load_from_memory(&jpeg_bytes).expect("decode by content");
    assert_eq!(by_content.color(), ColorType::L8);

    std::fs::remove_file(&path).ok();
}

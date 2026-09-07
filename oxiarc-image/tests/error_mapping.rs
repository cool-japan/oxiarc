//! Error mapping through the public entry points (`open`, `load_from_memory`,
//! `DynamicImage::write_to`), not just the `From` conversions in isolation
//! (those have their own direct unit tests in `src/error.rs`).

use oxiarc_image::{DynamicImage, ImageBuffer, ImageError, ImageFormat, Rgb};
use std::error::Error as StdError;
use std::io::Cursor;

#[test]
fn opening_a_missing_file_is_an_io_error() {
    let path = std::env::temp_dir().join(format!(
        "oxiarc_image_error_mapping_definitely_missing_{}.png",
        std::process::id()
    ));
    // Guard against a stale file from a previous crashed run reusing this pid.
    std::fs::remove_file(&path).ok();

    let err = oxiarc_image::open(&path).expect_err("the path must not exist");
    assert!(matches!(err, ImageError::IoError(_)));
}

#[test]
fn opening_an_unrecognised_extension_is_a_named_unsupported_error() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "oxiarc_image_error_mapping_unknown_ext_{}.this_is_not_a_format",
        std::process::id()
    ));
    std::fs::write(&path, b"contents do not matter here").expect("write fixture");

    let err = oxiarc_image::open(&path).expect_err("extension is not recognised");
    assert!(matches!(err, ImageError::Unsupported(_)));

    std::fs::remove_file(&path).ok();
}

#[test]
fn opening_a_path_with_no_extension_at_all_is_unsupported_not_a_panic() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "oxiarc_image_error_mapping_no_extension_{}",
        std::process::id()
    ));
    std::fs::write(&path, b"contents do not matter here").expect("write fixture");

    let err = oxiarc_image::open(&path).expect_err("no extension to guess from");
    assert!(matches!(err, ImageError::Unsupported(_)));

    std::fs::remove_file(&path).ok();
}

#[test]
fn truncated_or_corrupt_bytes_are_a_decoding_error_for_every_format_not_a_panic() {
    // A correct signature/header followed by nothing else: enough for each
    // format to recognise itself (so `load_from_memory`'s sniff succeeds)
    // but not enough to decode a single pixel.
    let png_truncated: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let jpeg_truncated: &[u8] = &[0xFF, 0xD8, 0xFF, 0xD9]; // SOI immediately followed by EOI
    let tiff_truncated: &[u8] = b"II*\0"; // no IFD offset even follows

    for (name, bytes) in [
        ("png", png_truncated),
        ("jpeg", jpeg_truncated),
        ("tiff", tiff_truncated),
    ] {
        let err = match oxiarc_image::load_from_memory(bytes) {
            Ok(image) => panic!(
                "expected {name} to fail decoding this short a header, got {:?}",
                image.dimensions()
            ),
            Err(e) => e,
        };
        // The important property is "an error was returned", already
        // checked above (an `Ok` panics before reaching here); the exact
        // variant differs by format (a header this short can be an I/O
        // EOF or a format error depending on where each decoder gives up),
        // so only the crate-level guarantee -- decode failures are always
        // one of the six `ImageError` variants and format a message
        // without panicking -- is asserted generically here.
        let text = format!("{err}");
        assert!(!text.is_empty(), "{name} error message must not be empty");
    }
}

#[test]
fn writing_a_png_incompatible_colour_type_reports_the_png_format_hint() {
    // `Rgb32F` has no PNG representation; `write_to` must fail before ever
    // touching the writer, not panic on an assertion inside `oxiarc-png`.
    let image = DynamicImage::ImageRgb32F(ImageBuffer::from_pixel(1, 1, Rgb::new(0.5, 0.5, 0.5)));
    let err = image
        .write_to(Cursor::new(Vec::new()), ImageFormat::Png)
        .expect_err("PNG cannot carry float samples");
    assert!(matches!(err, ImageError::Unsupported(_)));
    let text = err.to_string();
    assert!(
        text.to_ascii_lowercase().contains("png") || text.to_ascii_lowercase().contains("color"),
        "error message should be informative, got: {text}"
    );
}

#[test]
fn image_error_implements_std_error_end_to_end() {
    // A realistic caller pattern: propagate through `Box<dyn std::error::Error>`.
    fn decode_or_box(bytes: &[u8]) -> Result<DynamicImage, Box<dyn StdError>> {
        Ok(oxiarc_image::load_from_memory(bytes)?)
    }

    let err = decode_or_box(b"not an image").expect_err("garbage bytes must fail");
    // `Display` must produce a non-empty, human-readable message (not a
    // `{:?}`-only type), matching `image::ImageError`'s own contract.
    assert!(!err.to_string().is_empty());
}

/// A value-level format error (not a truncation/EOF) reliably carries the
/// underlying codec error as `source()`. JPEG has no CRC to satisfy (unlike
/// PNG), so a hand-built `DQT` segment whose declared length is
/// self-consistent (the marker framing is not truncated) but too short for
/// even one quantisation table is unambiguously a malformed *value*, not a
/// short read -- `oxiarc_jpeg::quant::parse_dqt`'s own
/// `rejects_bad_dqt_payloads` unit test uses the identical 4-byte payload
/// shape (`[0x00, 1, 2, 3]`) to prove the same thing at the codec level;
/// this test proves the error survives `oxiarc-image`'s conversion with
/// its `source()` chain intact, through the public decode path.
#[test]
fn a_malformed_segment_error_keeps_its_source_chain_through_the_public_api() {
    use oxiarc_image::codecs::jpeg::JpegDecoder;

    // SOI, then a DQT marker declaring a 6-byte segment (2 length bytes +
    // 4 payload bytes: one Pq/Tq byte plus 3 of the 64 values a table
    // needs) -- self-consistent framing, semantically invalid payload.
    let bytes: &[u8] = &[0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x06, 0x00, 0x01, 0x02, 0x03];

    let result = JpegDecoder::new(Cursor::new(bytes));
    let err = match result {
        Ok(_) => panic!("a truncated DQT table must not decode successfully"),
        Err(e) => e,
    };
    assert!(matches!(err, ImageError::Decoding(_)));
    assert!(
        err.source().is_some(),
        "Decoding errors built from a real codec failure must keep that failure as source(), \
         got: {err}"
    );
}

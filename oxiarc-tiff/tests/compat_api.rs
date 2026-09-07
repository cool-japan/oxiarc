//! Pins the `compat` module's shape against the exact call sequence
//! `image-0.25.10/src/codecs/tiff.rs` performs (tiff-design.md section 1.3),
//! reproduced here line-by-line-equivalent rather than as a paraphrase, so a
//! shape break fails a **compile**, not a runtime assertion, wherever
//! possible.
//!
//! Structured as one function per numbered `image` call site; each doc
//! comment names the line range in the real file this reproduces.

#![cfg(feature = "compat")]

use oxiarc_tiff::compat::decoder::{
    BufferLayoutPreference, ChunkType, Decoder, DecodingResult, Limits, TiffCodingUnit,
};
use oxiarc_tiff::compat::encoder::{
    Compression, TiffEncoder,
    colortype::{Gray8, Gray16, RGB8, RGB16, RGB32Float, RGBA8, RGBA16, RGBA32Float},
};
use oxiarc_tiff::compat::tags::Tag;
use oxiarc_tiff::compat::{ColorType, TiffError, TiffFormatError, TiffResult};
use std::io::Cursor;

fn sample_tiff_bytes() -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    let pixels: Vec<u8> = (0..16u32).map(|i| i as u8).collect();
    TiffEncoder::new(&mut buffer)
        .expect("encoder")
        .write_image::<Gray8>(4, 4, &pixels)
        .expect("write");
    buffer.into_inner()
}

/// `codecs/tiff.rs:61-99` -- `tiff::ColorType`'s exhaustive match, the
/// pattern `image` uses to translate into its own `image::ExtendedColorType`.
/// No wildcard arm: this stops compiling the moment `ColorType` gains an
/// eleventh variant or `#[non_exhaustive]`.
fn describe_color_type(ty: ColorType) -> &'static str {
    match ty {
        ColorType::Gray(_) => "L",
        ColorType::GrayA(_) => "La",
        ColorType::RGB(_) => "Rgb",
        ColorType::RGBA(_) => "Rgba",
        ColorType::CMYK(_) => "Cmyk",
        ColorType::Palette(_) => "Palette",
        ColorType::YCbCr(_) => "Rgb", // image upsamples YCbCr to RGB itself
        ColorType::CMYKA(_) => "Cmyk",
        ColorType::Lab(_) => "Lab",
        ColorType::Multiband { num_samples, .. } => {
            if num_samples == 1 {
                "L"
            } else {
                "Unknown"
            }
        }
    }
}

/// `codecs/tiff.rs:391-447` -- the exhaustive `DecodingResult` match with no
/// wildcard. Reproduced as a real conversion (into byte length), not a
/// no-op match, so an accidental variant reordering that changed a payload
/// type would still be caught by the assertions in
/// `decoder_matches_the_image_call_sequence` below.
fn decoding_result_byte_len(result: &DecodingResult) -> usize {
    match result {
        DecodingResult::U8(v) => v.len(),
        DecodingResult::U16(v) => v.len() * 2,
        DecodingResult::U32(v) => v.len() * 4,
        DecodingResult::U64(v) => v.len() * 8,
        DecodingResult::F16(v) => v.len() * 2,
        DecodingResult::F32(v) => v.len() * 4,
        DecodingResult::F64(v) => v.len() * 8,
        DecodingResult::I8(v) => v.len(),
        DecodingResult::I16(v) => v.len() * 2,
        DecodingResult::I32(v) => v.len() * 4,
        DecodingResult::I64(v) => v.len() * 8,
    }
}

/// `codecs/tiff.rs:237-274` -- the exhaustive `TiffError` match, performed
/// **twice** in the real file (once for the decode path's `TiffResult`,
/// once for a second call site). Reproduced twice here too
/// (this function and [`describe_error_second_site`]), each with its own
/// independent exhaustive match and no wildcard.
fn describe_error(err: &TiffError) -> &'static str {
    match err {
        TiffError::IoError(_) => "io",
        TiffError::FormatError(_) => "format",
        TiffError::IntSizeError => "int_size",
        TiffError::UsageError(_) => "usage",
        TiffError::UnsupportedError(_) => "unsupported",
        TiffError::LimitsExceeded => "limits",
    }
}

/// The second exhaustive `TiffError` match site, per `codecs/tiff.rs:237-274`.
fn describe_error_second_site(err: &TiffError) -> bool {
    match err {
        TiffError::IoError(_)
        | TiffError::FormatError(_)
        | TiffError::IntSizeError
        | TiffError::UsageError(_)
        | TiffError::UnsupportedError(_) => false,
        TiffError::LimitsExceeded => true,
    }
}

/// `codecs/tiff.rs:12-13, 61-99, 127, 185-186, 313, 326, 334, 341-347,
/// 358-367, 375-381, 391-447` -- the decode-side call sequence, in order.
#[test]
fn decoder_matches_the_image_call_sequence() {
    let bytes = sample_tiff_bytes();

    // 358-367: `Limits::default()`, fields written, `Decoder::new` + `with_limits`.
    let limits = Limits {
        decoding_buffer_size: 64 * 1024 * 1024,
        intermediate_buffer_size: 32 * 1024 * 1024,
        ifd_value_size: 512 * 1024,
    };
    let mut decoder: Decoder<Cursor<Vec<u8>>> = Decoder::new(Cursor::new(bytes))
        .expect("Decoder::new")
        .with_limits(limits);

    // "dimensions()"
    let dims = decoder.dimensions().expect("dimensions");
    assert_eq!(dims, (4, 4));

    // 61-99: "colortype()" -> exhaustive match, no wildcard.
    let color_type = decoder.colortype().expect("colortype");
    assert_eq!(describe_color_type(color_type), "L");

    // 127: `get_chunk_type()` used as a `BufferLayoutPreference`-adjacent
    // fact (planarity), and `ChunkType` itself.
    let chunk_type = decoder.get_chunk_type().expect("chunk type");
    assert_eq!(chunk_type, ChunkType::Strip);

    // 375-381 / 1467 / 1501: `read_image_to_buffer(&mut DecodingResult)`.
    let mut result = DecodingResult::U8(Vec::new());
    let layout: BufferLayoutPreference = decoder
        .read_image_to_buffer(&mut result)
        .expect("read_image_to_buffer");
    assert_eq!(layout.planes, 1);
    assert_eq!(layout.plane_stride, None);
    assert_eq!(layout.complete_len, layout.len);
    assert_eq!(layout.len, 16);

    // 391-447: exhaustive `DecodingResult` match, no wildcard.
    assert_eq!(decoding_result_byte_len(&result), 16);
    let DecodingResult::U8(pixels) = &result else {
        panic!("expected U8");
    };
    assert_eq!(pixels, &(0..16u32).map(|i| i as u8).collect::<Vec<u8>>());

    // 313: `get_tag_u8_vec(Tag::IccProfile)` (ICC passthrough; absent here,
    // so an empty vec -- exactly what `image` treats as "no profile").
    let icc = decoder.get_tag_u8_vec(Tag::IccProfile).expect("icc tag");
    assert!(icc.is_empty());

    // 326 / 334: XMP via `find_tag`, `RequiredTagNotFound` pattern.
    let xmp = decoder.find_tag(Tag::Xmp).expect("find xmp");
    assert_eq!(xmp, None);
    let missing_required = decoder.get_tag(Tag::Xmp);
    assert!(matches!(
        missing_required,
        Err(TiffError::FormatError(
            TiffFormatError::RequiredTagNotFound(Tag::Xmp)
        )) | Err(TiffError::FormatError(TiffFormatError::Other(_)))
    ));

    // 341-347: `find_tag(Tag::Orientation)?.and_then(|v| v.into_u16())`.
    let orientation = decoder
        .find_tag(Tag::Orientation)
        .expect("find orientation")
        .and_then(|v| v.first_u16());
    assert_eq!(orientation, None); // never written by `sample_tiff_bytes`

    // Both exhaustive `TiffError` match sites compile and run.
    let synthetic = TiffError::LimitsExceeded;
    assert_eq!(describe_error(&synthetic), "limits");
    assert!(describe_error_second_site(&synthetic));

    // strip_count / more_images / next_image / seek_to_image, per this
    // track's own deliverable list (not in `image`'s call path, but in the
    // contract this module is graded against).
    assert_eq!(decoder.strip_count().expect("strip count"), 1);
    assert!(!decoder.more_images());
    let err = decoder.next_image().expect_err("no next image");
    assert!(matches!(err, TiffError::FormatError(_)));
    assert!(decoder.seek_to_image(0).is_ok());
    assert!(matches!(
        decoder.seek_to_image(1),
        Err(TiffError::FormatError(_) | TiffError::UsageError(_))
    ));
}

/// `codecs/tiff.rs:522-551, 584-602` -- the encode-side call sequence:
/// `TiffEncoder`, `new_image::<C>`, `img_encoder.encoder().write_tag(...)`,
/// `write_data`, and the eight colour-type markers `image` names by
/// identifier (`colortype::{Gray8, Gray16, RGB8, RGB16, RGBA8, RGBA16,
/// RGB32Float, RGBA32Float}`).
#[test]
fn encoder_matches_the_image_call_sequence() {
    fn round_trip<C>(width: u32, height: u32, data: &[C::Inner]) -> TiffResult<()>
    where
        C: oxiarc_tiff::compat::encoder::colortype::ColorType,
    {
        let mut buffer = Cursor::new(Vec::new());
        // `Uncompressed` (rather than `Lzw`/`Deflate`) so this test needs no
        // codec feature -- it exists to pin the compat *shape*, not to
        // exercise a codec, and `--features compat` alone (no codec at all)
        // is a real, supported build (critique.md section 6.6).
        let mut encoder =
            TiffEncoder::new(&mut buffer)?.with_compression(Compression::Uncompressed);
        let mut img_encoder = encoder.new_image::<C>(width, height)?;
        img_encoder
            .encoder()
            .write_tag_u8_vec(Tag::IccProfile, vec![0xAA, 0xBB])?;
        img_encoder.write_data(data)?;
        let bytes = buffer.into_inner();

        let mut decoder = Decoder::new(Cursor::new(bytes))?;
        assert_eq!(decoder.dimensions()?, (width, height));
        assert_eq!(decoder.get_tag_u8_vec(Tag::IccProfile)?, vec![0xAA, 0xBB]);
        Ok(())
    }

    round_trip::<Gray8>(2, 2, &[1u8, 2, 3, 4]).expect("Gray8");
    round_trip::<Gray16>(2, 2, &[1u16, 2, 3, 4]).expect("Gray16");
    round_trip::<RGB8>(2, 1, &[1u8, 2, 3, 4, 5, 6]).expect("RGB8");
    round_trip::<RGB16>(2, 1, &[1u16, 2, 3, 4, 5, 6]).expect("RGB16");
    round_trip::<RGBA8>(1, 1, &[1u8, 2, 3, 4]).expect("RGBA8");
    round_trip::<RGBA16>(1, 1, &[1u16, 2, 3, 4]).expect("RGBA16");
    round_trip::<RGB32Float>(1, 1, &[0.5f32, 0.25, 0.75]).expect("RGB32Float");
    round_trip::<RGBA32Float>(1, 1, &[0.5f32, 0.25, 0.75, 1.0]).expect("RGBA32Float");
}

/// `TiffCodingUnit` shape (critique.md section 2.11's export list): present
/// and usable, even though `image` itself never touches it.
#[test]
fn tiff_coding_unit_exists_and_is_usable() {
    assert_eq!(TiffCodingUnit::Strip(2).index(), 2);
    assert_eq!(TiffCodingUnit::Tile(5).index(), 5);
}

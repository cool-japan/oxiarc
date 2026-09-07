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
    assert_eq!(layout.complete_len, layout.len);
    // 16 Gray8 pixels = 16 bytes; the Gray16 case in
    // `buffer_layout_lengths_are_byte_counts_not_sample_counts` is the one
    // that can tell bytes from samples apart.
    assert_eq!(layout.len, 16);
    assert_eq!(layout.plane_stride, std::num::NonZeroUsize::new(16));
    assert_eq!(layout.row_stride, std::num::NonZeroUsize::new(4));

    // 391-447: exhaustive `DecodingResult` match, no wildcard.
    assert_eq!(decoding_result_byte_len(&result), 16);
    let DecodingResult::U8(pixels) = &result else {
        panic!("expected U8");
    };
    assert_eq!(pixels, &(0..16u32).map(|i| i as u8).collect::<Vec<u8>>());

    // 313: `Ok(decoder.get_tag_u8_vec(Tag::IccProfile).ok())` -- the ICC
    // passthrough. The tag is absent here, and `image` reads *absence* off
    // the `Err`, so this must error rather than succeed with an empty vec:
    // `Ok(vec![])` would make `.ok()` yield `Some(vec![])` and attach a
    // zero-length ICC profile to every file that has none.
    let icc_err = decoder
        .get_tag_u8_vec(Tag::IccProfile)
        .expect_err("an absent ICC tag must be an error, not an empty vec");
    assert!(matches!(
        icc_err,
        TiffError::FormatError(TiffFormatError::RequiredTagNotFound(Tag::IccProfile))
    ));
    assert_eq!(decoder.get_tag_u8_vec(Tag::IccProfile).ok(), None);

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

    // 341-347: `find_tag(Tag::Orientation)?.and_then(|v| v.into_u16().ok())`
    // -- `into_u16` verbatim, not this crate's own `first_u16` spelling.
    let orientation = decoder
        .find_tag(Tag::Orientation)
        .expect("find orientation")
        .and_then(|v| v.into_u16().ok());
    assert_eq!(orientation, None); // never written by `sample_tiff_bytes`

    // 50: the very first call `image` makes against a fresh decoder.
    let sample_formats: Option<Vec<u16>> = decoder
        .find_tag_unsigned_vec::<u16>(Tag::SampleFormat)
        .expect("find_tag_unsigned_vec");
    assert_eq!(sample_formats, Some(vec![1]));
    assert_eq!(
        decoder
            .find_tag_unsigned::<u32>(Tag::ImageWidth)
            .expect("find_tag_unsigned"),
        Some(4)
    );
    assert_eq!(
        decoder
            .get_tag_unsigned::<u32>(Tag::ImageLength)
            .expect("get_tag_unsigned"),
        4
    );
    assert!(
        decoder
            .find_tag_unsigned_vec::<u16>(Tag::Unknown(64999))
            .expect("absent tag is Ok(None)")
            .is_none()
    );

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

        // The page must declare the numeric format its Rust sample type
        // implies -- without this the eight markers below all passed while
        // three of them wrote `Uint` over IEEE-754 bytes (see
        // `every_colour_marker_declares_its_own_sample_format`).
        let mut probe = oxiarc_tiff::Decoder::new(Cursor::new(bytes.clone()))?;
        let formats: Vec<u16> = probe
            .find_tag(oxiarc_tiff::Tag::SampleFormat)?
            .and_then(|v| v.as_u64_vec())
            .unwrap_or_default()
            .into_iter()
            .map(|v| v as u16)
            .collect();
        assert!(
            !formats.is_empty() && formats.iter().all(|f| *f == C::SAMPLE_FORMAT.to_u16()),
            "SampleFormat {formats:?} does not match the marker's own"
        );

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

/// **Regression (TIFF-verify V1).** Every length in
/// [`BufferLayoutPreference`] is a *byte* count, as upstream documents them
/// ("Number of bytes of data when reading all planes") and as upstream's own
/// `DecodingResult::resize_to` consumes them (`extent_for_bytes(complete_len)`).
///
/// Reported in samples instead, a ported caller sizing a buffer from
/// `complete_len` gets half the bytes it needs for `U16`, a quarter for
/// `U32`, an eighth for `U64` -- with no compile error and no runtime error,
/// because the documented guard
/// `as_buffer(0).as_bytes().len() < layout.complete_len` then compares a byte
/// count against a sample count and passes trivially.
///
/// Gray8 alone cannot catch this (one sample *is* one byte), which is why the
/// pre-existing call-sequence test did not.
#[test]
fn buffer_layout_lengths_are_byte_counts_not_sample_counts() {
    for (width, height, bytes_per_sample, encode) in
        [(4u32, 4u32, 1usize, 0u8), (4, 4, 2, 1), (3, 5, 4, 2)]
    {
        let mut buffer = Cursor::new(Vec::new());
        let samples = (width * height) as usize;
        let mut encoder = TiffEncoder::new(&mut buffer).expect("encoder");
        match encode {
            0 => encoder
                .write_image::<Gray8>(width, height, &vec![7u8; samples])
                .expect("gray8"),
            1 => encoder
                .write_image::<Gray16>(width, height, &vec![7u16; samples])
                .expect("gray16"),
            _ => encoder
                .write_image::<oxiarc_tiff::compat::encoder::colortype::Gray32Float>(
                    width,
                    height,
                    &vec![0.5f32; samples],
                )
                .expect("gray32f"),
        }
        let bytes = buffer.into_inner();

        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        let mut result = DecodingResult::U8(Vec::new());
        let layout = decoder
            .read_image_to_buffer(&mut result)
            .expect("read_image_to_buffer");

        let expected_bytes = samples * bytes_per_sample;
        assert_eq!(
            layout.complete_len, expected_bytes,
            "complete_len must be bytes for {bytes_per_sample}-byte samples"
        );
        assert_eq!(layout.len, expected_bytes);
        assert_eq!(
            layout.row_stride,
            std::num::NonZeroUsize::new(width as usize * bytes_per_sample)
        );
        assert_eq!(
            layout.plane_stride,
            std::num::NonZeroUsize::new(expected_bytes)
        );
        assert_eq!(layout.planes, 1);

        // The exact guard upstream documents on `read_image_to_buffer`, and
        // that `image` 0.25.10 runs at `codecs/tiff.rs:379`: it must not fire
        // for a complete decode, and it is only meaningful because both sides
        // are now byte counts.
        let mut owned = result;
        assert_eq!(owned.as_buffer(0).byte_len(), layout.complete_len);
        assert!(owned.as_buffer(0).to_bytes().len() >= layout.complete_len);
        assert_eq!(
            owned.as_buffer(0).byte_len(),
            owned.len() * layout.sample_type.expect("sample type").byte_width()
        );
    }
}

/// **Regression (TIFF-verify V2).** A planar (`PlanarConfiguration = 2`)
/// file is interleaved back to chunky by this crate's own decoder, so
/// `planes` really is 1 and a caller never has to run `image`'s
/// `interleave_planes` path. Pins the fact the byte-count fix above depends
/// on: `complete_len` covers the *whole* image, not one plane of it.
#[test]
fn a_planar_file_still_reports_one_interleaved_plane() {
    use oxiarc_tiff::{ColorType as NativeColorType, Encoder as NativeEncoder, ImageSpec};

    // Written through the native API: the compat encoder cannot select
    // planar, and this is about what the *decoder* reports.
    let pixels: Vec<u8> = (0..12u32).map(|i| i as u8).collect(); // 2x2 RGB
    let spec = ImageSpec::new(2, 2, NativeColorType::Rgb(8))
        .with_planar(oxiarc_tiff::tags::PlanarConfiguration::Planar);
    let mut buffer = Cursor::new(Vec::new());
    NativeEncoder::new(&mut buffer)
        .expect("native encoder")
        .write_image(&spec, &pixels)
        .expect("write planar");
    let bytes = buffer.into_inner();

    // Non-vacuity: the fixture really is planar, so the interleaving
    // assertion below has something to prove.
    let mut probe = oxiarc_tiff::Decoder::new(Cursor::new(bytes.clone())).expect("probe");
    assert_eq!(
        probe
            .find_tag(oxiarc_tiff::Tag::PlanarConfiguration)
            .expect("planar tag")
            .and_then(|v| v.first_u16()),
        Some(2)
    );

    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    let mut result = DecodingResult::U8(Vec::new());
    let layout = decoder
        .read_image_to_buffer(&mut result)
        .expect("read planar");
    assert_eq!(layout.planes, 1);
    assert_eq!(layout.complete_len, 12);
    assert_eq!(layout.row_stride, std::num::NonZeroUsize::new(6));
    let DecodingResult::U8(decoded) = &result else {
        panic!("expected U8");
    };
    // Interleaved, i.e. the original chunky order -- not R,R,R,R,G,G,...
    assert_eq!(decoded, &pixels);
}

/// **Regression (TIFF-verify V3).** The `into_*` conversions upstream
/// callers write by name exist on the tag-value type, and report
/// `InvalidTypeForTag` (never a silent default) when the field type does not
/// match.
#[test]
fn value_into_conversions_match_the_upstream_names_and_failure_mode() {
    use oxiarc_tiff::compat::tags::ValueBuffer;

    assert_eq!(
        ValueBuffer::Short(vec![3, 4]).into_u16().expect("into_u16"),
        3
    );
    assert_eq!(
        ValueBuffer::Long(vec![70000]).into_u32().expect("into_u32"),
        70000
    );
    assert_eq!(
        ValueBuffer::Undefined(vec![1, 2, 3])
            .into_u8_vec()
            .expect("into_u8_vec"),
        vec![1, 2, 3]
    );
    assert_eq!(
        ValueBuffer::Ascii("hi".to_string())
            .into_string()
            .expect("into_string"),
        "hi"
    );
    assert_eq!(
        ValueBuffer::Short(vec![1, 2])
            .into_u16_vec()
            .expect("into_u16_vec"),
        vec![1u16, 2]
    );

    // Wrong field type -> the upstream error, not `Ok(default)`.
    let err = ValueBuffer::Ascii("nope".to_string())
        .into_u8_vec()
        .expect_err("ascii is not byte-shaped");
    assert!(matches!(
        err,
        TiffError::FormatError(TiffFormatError::InvalidTypeForTag { .. })
    ));
    // Too wide for the requested width -> an error, never a truncation.
    let err = ValueBuffer::Long(vec![70000])
        .into_u16()
        .expect_err("70000 > u16::MAX");
    assert!(matches!(
        err,
        TiffError::FormatError(TiffFormatError::InvalidTypeForTag { .. })
    ));
    // Empty -> an error, never a silent zero.
    let err = ValueBuffer::Short(Vec::new())
        .into_u16()
        .expect_err("empty");
    assert!(matches!(
        err,
        TiffError::FormatError(TiffFormatError::InvalidTypeForTag { .. })
    ));
}

/// **Regression (TIFF-verify V4).** `ImageEncoder`'s three resolution
/// setters are independent upstream, so setting one alone must not
/// materialise its siblings. Before this fix, `resolution_unit(..)` alone
/// wrote `XResolution = 0/1` and `YResolution = 0/1` into the file -- a
/// *wrong* resolution rather than an absent one -- and `x_resolution(..)`
/// alone wrote `YResolution = 0/1`.
#[test]
fn resolution_setters_do_not_invent_their_siblings() {
    use oxiarc_tiff::Decoder as NativeDecoder;
    use oxiarc_tiff::compat::tags::ResolutionUnit;
    use oxiarc_tiff::ifd::Rational;

    fn tags_of(bytes: Vec<u8>) -> (Option<Vec<f64>>, Option<Vec<f64>>, Option<u16>) {
        let mut decoder = NativeDecoder::new(Cursor::new(bytes)).expect("decoder");
        let x = decoder
            .find_tag(oxiarc_tiff::Tag::XResolution)
            .expect("x")
            .and_then(|v| v.as_f64_vec());
        let y = decoder
            .find_tag(oxiarc_tiff::Tag::YResolution)
            .expect("y")
            .and_then(|v| v.as_f64_vec());
        let unit = decoder
            .find_tag(oxiarc_tiff::Tag::ResolutionUnit)
            .expect("unit")
            .and_then(|v| v.first_u16());
        (x, y, unit)
    }

    fn write(
        f: impl for<'a> FnOnce(
            oxiarc_tiff::compat::encoder::ImageEncoder<'a, &'a mut Cursor<Vec<u8>>, Gray8>,
        ) -> oxiarc_tiff::compat::encoder::ImageEncoder<
            'a,
            &'a mut Cursor<Vec<u8>>,
            Gray8,
        >,
    ) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut encoder = TiffEncoder::new(&mut buffer).expect("encoder");
            let page = encoder.new_image::<Gray8>(2, 2).expect("new_image");
            f(page).write_data(&[1u8, 2, 3, 4]).expect("write_data");
        }
        buffer.into_inner()
    }

    // Nothing set: no resolution tags at all.
    let (x, y, unit) = tags_of(write(|page| page));
    assert_eq!((x, y, unit), (None, None, None));

    // Unit alone: the unit tag, and *only* the unit tag.
    let (x, y, unit) = tags_of(write(|page| {
        page.resolution_unit(ResolutionUnit::Centimeter)
    }));
    assert_eq!(
        x, None,
        "a lone resolution_unit must not invent XResolution"
    );
    assert_eq!(
        y, None,
        "a lone resolution_unit must not invent YResolution"
    );
    assert_eq!(unit, Some(3));

    // One axis alone: square pixels, never a 0/1 filler on the other axis.
    let (x, y, unit) = tags_of(write(|page| {
        page.x_resolution(Rational { num: 300, den: 1 })
    }));
    assert_eq!(x, Some(vec![300.0]));
    assert_eq!(y, Some(vec![300.0]), "the sibling axis must not be 0");
    assert_eq!(unit, Some(2), "TIFF 6.0's default ResolutionUnit is Inch");

    // Both axes plus a unit: exactly what was asked for.
    let (x, y, unit) = tags_of(write(|page| {
        page.resolution(Rational { num: 72, den: 1 }, Rational { num: 144, den: 1 })
            .resolution_unit(ResolutionUnit::Centimeter)
    }));
    assert_eq!(x, Some(vec![72.0]));
    assert_eq!(y, Some(vec![144.0]));
    assert_eq!(unit, Some(3));
}

/// **Regression (TIFF-verify V7).** Every colour-type marker writes the
/// `SampleFormat` (339) its Rust sample type calls for.
///
/// `ImageSpec::new` defaults every channel to `Uint`, and the compat encoder
/// never overrode it -- so a `Gray32Float`/`RGB32Float`/`RGBA32Float` page
/// (the three `image` 0.25.10 writes for `Rgb32F`/`Rgba32F`,
/// `codecs/tiff.rs:601-602`) was tagged `SampleFormat = 1` and read back as
/// `u32` by every reader, and a `GrayI16` page round-tripped `-5` as
/// `65531`. Nothing errored anywhere: the bytes were right and the *label*
/// was wrong, which is the worst shape a bug can have.
///
/// Upstream carries `const SAMPLE_FORMAT: &'static [SampleFormat]` on each
/// marker for exactly this reason (`tiff-0.11.3/src/encoder/colortype.rs:38`).
#[test]
fn every_colour_marker_declares_its_own_sample_format() {
    use oxiarc_tiff::compat::encoder::colortype::{
        CMYK8, CMYK32Float, CMYKA8, Gray32Float, Gray64Float, GrayI8, GrayI16, GrayI32, GrayI64,
        RGB64Float, RGBA64Float, YCbCr8,
    };
    use oxiarc_tiff::compat::tags::SampleFormat;

    fn sample_format_of(bytes: Vec<u8>) -> Vec<u16> {
        let mut decoder = oxiarc_tiff::Decoder::new(Cursor::new(bytes)).expect("decoder");
        decoder
            .find_tag(oxiarc_tiff::Tag::SampleFormat)
            .expect("tag")
            .and_then(|v| v.as_u64_vec())
            .unwrap_or_default()
            .into_iter()
            .map(|v| v as u16)
            .collect()
    }

    macro_rules! check {
        ($marker:ty, $w:expr, $h:expr, $data:expr, $want:expr, $channels:expr) => {{
            let mut buffer = Cursor::new(Vec::new());
            TiffEncoder::new(&mut buffer)
                .expect("encoder")
                .write_image::<$marker>($w, $h, $data)
                .expect("write");
            let bytes = buffer.into_inner();
            assert_eq!(
                sample_format_of(bytes),
                vec![$want.to_u16(); $channels],
                "wrong SampleFormat for {}",
                stringify!($marker)
            );
        }};
    }

    check!(Gray8, 1, 1, &[1u8], SampleFormat::Uint, 1);
    check!(GrayI8, 1, 1, &[-1i8], SampleFormat::Int, 1);
    check!(Gray16, 1, 1, &[1u16], SampleFormat::Uint, 1);
    check!(GrayI16, 1, 1, &[-1i16], SampleFormat::Int, 1);
    check!(GrayI32, 1, 1, &[-1i32], SampleFormat::Int, 1);
    check!(GrayI64, 1, 1, &[-1i64], SampleFormat::Int, 1);
    check!(Gray32Float, 1, 1, &[0.5f32], SampleFormat::IEEEFP, 1);
    check!(Gray64Float, 1, 1, &[0.5f64], SampleFormat::IEEEFP, 1);
    check!(RGB8, 1, 1, &[1u8, 2, 3], SampleFormat::Uint, 3);
    check!(
        RGB32Float,
        1,
        1,
        &[0.5f32, 0.25, 0.75],
        SampleFormat::IEEEFP,
        3
    );
    check!(
        RGB64Float,
        1,
        1,
        &[0.5f64, 0.25, 0.75],
        SampleFormat::IEEEFP,
        3
    );
    check!(
        RGBA32Float,
        1,
        1,
        &[0.5f32, 0.25, 0.75, 1.0],
        SampleFormat::IEEEFP,
        4
    );
    check!(
        RGBA64Float,
        1,
        1,
        &[0.5f64, 0.25, 0.75, 1.0],
        SampleFormat::IEEEFP,
        4
    );
    check!(CMYK8, 1, 1, &[1u8, 2, 3, 4], SampleFormat::Uint, 4);
    // Channel counts other than 1/3/4: `with_sample_format` fills one entry
    // per channel from the spec's `samples_per_pixel`, so a five-channel
    // marker must produce five entries, not four.
    check!(CMYKA8, 1, 1, &[1u8, 2, 3, 4, 5], SampleFormat::Uint, 5);
    check!(
        CMYK32Float,
        1,
        1,
        &[0.1f32, 0.2, 0.3, 0.4],
        SampleFormat::IEEEFP,
        4
    );
    check!(YCbCr8, 1, 1, &[1u8, 2, 3], SampleFormat::Uint, 3);
}

/// **Regression (TIFF-verify V7, the observable half).** The values survive
/// the round trip with their *type*, not merely their bit pattern.
#[test]
fn signed_and_float_markers_round_trip_as_signed_and_float() {
    use oxiarc_tiff::compat::encoder::colortype::{Gray32Float, GrayI16};

    let mut buffer = Cursor::new(Vec::new());
    TiffEncoder::new(&mut buffer)
        .expect("encoder")
        .write_image::<GrayI16>(3, 1, &[-5i16, 0, 32767])
        .expect("write");
    let mut decoder = Decoder::new(Cursor::new(buffer.into_inner())).expect("decoder");
    let DecodingResult::I16(values) = decoder.read_image().expect("read") else {
        panic!("a GrayI16 page must decode as I16, not U16");
    };
    assert_eq!(values, vec![-5i16, 0, 32767]);

    let mut buffer = Cursor::new(Vec::new());
    TiffEncoder::new(&mut buffer)
        .expect("encoder")
        .write_image::<Gray32Float>(3, 1, &[0.5f32, -1.25, 1e10])
        .expect("write");
    let mut decoder = Decoder::new(Cursor::new(buffer.into_inner())).expect("decoder");
    let DecodingResult::F32(values) = decoder.read_image().expect("read") else {
        panic!("a Gray32Float page must decode as F32, not U32");
    };
    assert_eq!(values, vec![0.5f32, -1.25, 1e10]);
}

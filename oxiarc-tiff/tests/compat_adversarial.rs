//! Malformed input through the `compat` facade.
//!
//! `adversarial.rs` and `corrupt_no_panic.rs` drive the **native** reader over
//! hostile files. The `compat` module is a second public entry point into the
//! same pipeline, with its own argument and result translation on both sides
//! (index widths narrowed to `u32`, caller-supplied buffers, a cached
//! `more_images`, and the `into_*` value conversions), so it needs its own
//! sweep: a translation layer is exactly where a `try_from` becomes an
//! `as`-cast, an absent value becomes a default, and a `Result` becomes a
//! panic.
//!
//! Every test here pins the same three properties the native suites do -- no
//! panic, no hang, no silently-wrong success -- plus the two that are specific
//! to this layer: an error must keep its *shape* across the boundary, and a
//! caller loop driven by `more_images()` must terminate.

#![cfg(feature = "compat")]

mod support;

use oxiarc_tiff::compat::decoder::{Decoder, DecodingResult, Limits};
use oxiarc_tiff::compat::tags::Tag;
use oxiarc_tiff::compat::{TiffError, TiffFormatError};
use std::io::Cursor;
use std::time::{Duration, Instant};
use support::{NextIfd, gray8, ramp};

/// Tight guards, so an unbounded allocation fails a test instead of swapping.
fn guarded() -> Limits {
    Limits {
        decoding_buffer_size: 4 * 1024 * 1024,
        ifd_value_size: 64 * 1024,
        intermediate_buffer_size: 4 * 1024 * 1024,
    }
}

/// Every read entry point the compat `Decoder` exposes, driven once. Returns
/// `true` when the file decoded a whole image, so a caller can assert the
/// sweep was not vacuous.
fn exercise(bytes: Vec<u8>) -> bool {
    let Ok(mut decoder) = Decoder::new(Cursor::new(bytes)).map(|d| d.with_limits(guarded())) else {
        return false;
    };
    let _ = decoder.byte_order();
    let _ = decoder.ifd_pointer();
    let _ = decoder.dimensions();
    let _ = decoder.colortype();
    let _ = decoder.get_chunk_type();
    let _ = decoder.chunk_dimensions();
    let _ = decoder.tag_iter();
    let _ = decoder.get_tag(Tag::ImageWidth);
    let _ = decoder.find_tag(Tag::Xmp);
    let _ = decoder.get_tag_u8_vec(Tag::IccProfile);
    let _ = decoder.get_tag_ascii_string(Tag::ImageDescription);
    let _ = decoder.find_tag_unsigned::<u32>(Tag::ImageWidth);
    let _ = decoder.find_tag_unsigned_vec::<u16>(Tag::SampleFormat);
    let _ = decoder.get_tag_unsigned::<u16>(Tag::BitsPerSample);

    if let Ok(count) = decoder.strip_count() {
        // Index 0, the last valid index, and one past the end -- the three
        // places a `u32`/`u64` narrowing can go wrong.
        for index in [0, count.saturating_sub(1), count] {
            let _ = decoder.chunk_data_dimensions(index);
            let _ = decoder.read_chunk(index);
            let mut scratch = vec![0u8; 64];
            let _ = decoder.read_chunk_bytes(index, &mut scratch);
        }
    }

    let mut result = DecodingResult::U8(Vec::new());
    let _ = decoder.read_image_to_buffer(&mut result);
    let mut small = [0u8; 3];
    let _ = decoder.read_image_bytes(&mut small);
    let complete = decoder.read_image().is_ok();

    // A caller's page loop, exactly as upstream documents it -- bounded here
    // only so a *failure* to terminate is a test failure rather than a hang.
    let mut pages = 0;
    while decoder.more_images() && pages < 64 {
        if decoder.next_image().is_err() {
            break;
        }
        pages += 1;
        let _ = decoder.dimensions();
        let _ = decoder.read_image();
    }
    assert!(pages < 64, "the more_images() loop did not terminate");
    complete
}

#[test]
fn every_prefix_of_a_valid_file_survives_the_whole_compat_surface() {
    let bytes = gray8(8, 8, &ramp(64)).build();
    let started = Instant::now();
    let mut complete = 0;
    for cut in 0..=bytes.len() {
        if exercise(bytes.get(..cut).unwrap_or_default().to_vec()) {
            complete += 1;
        }
    }
    assert!(
        complete > 0,
        "no prefix decoded completely -- the sweep would be vacuous"
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the truncation sweep took {:?}",
        started.elapsed()
    );
}

#[test]
fn single_byte_substitutions_never_panic_through_compat() {
    let bytes = gray8(4, 4, &ramp(16)).build();
    for index in 0..bytes.len() {
        for replacement in [0x00u8, 0x7F, 0xFF] {
            let mut corrupted = bytes.clone();
            if let Some(slot) = corrupted.get_mut(index) {
                *slot = replacement;
            }
            exercise(corrupted);
        }
    }
}

#[test]
fn hostile_declared_lengths_are_refused_through_compat_limits() {
    let started = Instant::now();
    for (tag, values) in [(279u16, vec![u64::MAX]), (273, vec![u64::MAX - 8])] {
        let mut tiff = gray8(8, 8, &ramp(64));
        tiff.long8(tag, &values);
        let mut decoder = Decoder::new(Cursor::new(tiff.build()))
            .expect("header")
            .with_limits(guarded());
        let err = decoder
            .read_image()
            .expect_err("a hostile length must fail");
        assert!(
            matches!(
                err,
                TiffError::LimitsExceeded
                    | TiffError::FormatError(_)
                    | TiffError::IntSizeError
                    | TiffError::IoError(_)
            ),
            "unexpected error shape: {err:?}"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a hostile length allocated before it was refused ({:?})",
        started.elapsed()
    );
}

/// **Regression (TIFF-verify V5).** `more_images()` answers "is there a
/// pointer to another IFD", like upstream's `next_ifd.is_some()`.
///
/// It previously answered "does the rest of the chain walk cleanly", by
/// calling the native `Decoder::more_images()` (which extends and validates
/// the chain) and mapping its *error* to `false`. A file whose next-IFD
/// pointer is non-zero but leads to a cyclic or unreadable chain therefore
/// reported "no more images", so a caller's `while decoder.more_images()`
/// loop stopped silently with the defect reported nowhere -- the page-list
/// equivalent of a truncated decode. Now the claim is surfaced and
/// [`next_image`] is what fails.
#[test]
fn a_corrupt_next_ifd_pointer_is_reported_not_swallowed() {
    // A self-looping chain: the native walker detects the cycle *while
    // extending*, which is exactly the error the old cache swallowed.
    let bytes = gray8(4, 4, &ramp(16)).next_ifd(NextIfd::SelfLoop).build();

    let decoder = Decoder::new(Cursor::new(bytes.clone())).expect("header");
    assert!(
        decoder.more_images(),
        "a non-zero next-IFD pointer means another page is claimed to exist, \
         even when following it fails"
    );

    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("header");
    let err = decoder
        .next_image()
        .expect_err("the chain it points into is cyclic");
    assert!(
        matches!(
            err,
            TiffError::FormatError(_) | TiffError::IoError(_) | TiffError::LimitsExceeded
        ),
        "unexpected error shape: {err:?}"
    );

    // An out-of-file pointer is claimed too, and reading that page must fail
    // rather than yield an empty or stale image.
    let far = gray8(4, 4, &ramp(16))
        .next_ifd(NextIfd::At(0xFFFF_0000))
        .build();
    let mut decoder = Decoder::new(Cursor::new(far)).expect("header");
    assert!(decoder.more_images());
    let reached = decoder.next_image().is_ok();
    if reached {
        assert!(
            decoder.dimensions().is_err() && decoder.read_image().is_err(),
            "a page at an out-of-file offset must not decode"
        );
    }

    // The honest end of a chain still reports `false`, so the fix did not
    // simply hard-code `true`.
    let ended = Decoder::new(Cursor::new(
        gray8(4, 4, &ramp(16)).next_ifd(NextIfd::None).build(),
    ))
    .expect("header");
    assert!(!ended.more_images());
}

/// **Regression (TIFF-verify V6).** `read_image_bytes` writes straight into
/// the caller's buffer through the native byte path.
///
/// It previously decoded to a `DecodingResult`, serialised *that* into a
/// second whole-image `Vec`, and copied -- so the peak allocation was twice
/// the image, above a `decoding_buffer_size` the caller had explicitly
/// capped. This pins the observable half of that: the bytes are right, an
/// exactly-sized buffer is accepted, and a short one is a named usage error
/// rather than a partial write.
#[test]
fn read_image_bytes_fills_the_callers_buffer_exactly() {
    let pixels = ramp(64);
    let bytes = gray8(8, 8, &pixels).build();
    let mut decoder = Decoder::new(Cursor::new(bytes.clone())).expect("header");

    let mut exact = vec![0u8; 64];
    decoder.read_image_bytes(&mut exact).expect("exact buffer");
    assert_eq!(exact, pixels);

    let mut roomy = vec![0xEEu8; 80];
    decoder.read_image_bytes(&mut roomy).expect("roomy buffer");
    assert_eq!(roomy.get(..64), Some(pixels.as_slice()));
    assert_eq!(
        roomy.get(64..),
        Some(&[0xEEu8; 16][..]),
        "bytes past the image must be left alone"
    );

    let mut short = vec![0xEEu8; 63];
    let err = decoder
        .read_image_bytes(&mut short)
        .expect_err("a short buffer must be refused");
    assert!(matches!(err, TiffError::UsageError(_)), "{err:?}");
    assert_eq!(
        short,
        vec![0xEEu8; 63],
        "a refused read must not partially write"
    );
}

/// A tag whose field type cannot satisfy the requested conversion reports
/// `InvalidTypeForTag` from a *real file*, not just from a hand-built
/// `ValueBuffer`.
#[test]
fn a_wrongly_typed_tag_reports_invalid_type_rather_than_a_default() {
    let mut tiff = gray8(4, 4, &ramp(16));
    // `ImageDescription` (270) is ASCII by the spec; make it SHORT instead.
    tiff.short(270, &[7, 8]);
    // `XmlPacket` (700) is BYTE by the spec; make it ASCII instead.
    tiff.ascii(700, "not bytes");
    let mut decoder = Decoder::new(Cursor::new(tiff.build())).expect("header");

    let err = decoder
        .get_tag_ascii_string(Tag::ImageDescription)
        .expect_err("a SHORT-typed ImageDescription is not a string");
    assert!(
        matches!(
            err,
            TiffError::FormatError(TiffFormatError::InvalidTypeForTag { found: 3 })
        ),
        "{err:?}"
    );

    let err = decoder
        .get_tag_u8_vec(Tag::Xmp)
        .expect_err("an ASCII-typed XMP packet is not a byte string");
    assert!(
        matches!(
            err,
            TiffError::FormatError(TiffFormatError::InvalidTypeForTag { found: 2 })
        ),
        "{err:?}"
    );

    // ... and the well-typed reads next to them still succeed, so the guard
    // is not simply refusing everything.
    assert_eq!(
        decoder
            .find_tag_unsigned_vec::<u16>(Tag::ImageDescription)
            .expect("short values are unsigned-readable"),
        Some(vec![7u16, 8])
    );
}

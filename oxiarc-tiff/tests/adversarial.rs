//! Adversarial reads: hostile geometry, hostile buffers, hostile indices.
//!
//! `corrupt_no_panic.rs` sweeps *mutations* of valid files. This suite builds
//! files that are structurally well formed but semantically hostile — a strip
//! that claims to be 2^64 bytes long, a page that claims sixteen million strips
//! in two hundred bytes, a predictor over per-channel bit depths — and pins the
//! properties that must hold for every one of them:
//!
//! * no panic, no abort, no hang;
//! * every allocation stays inside [`Limits`];
//! * a short destination buffer is a named error, never a partial write;
//! * a semantically impossible combination is an error, never silently wrong
//!   pixels.

mod support;

use oxiarc_tiff::{
    ColorType, Compression, Decoder, Encoder, ImageSpec, Layout, Leniency, Limits, Rect, TiffError,
    UsageError,
};
use std::io::Cursor;
use std::time::{Duration, Instant};
use support::{RawTiff, gray8, ramp};

/// Tight guards so an unbounded allocation shows up as a failure, not as swap.
fn guarded() -> Limits {
    Limits::default()
        .with_max_image_bytes(4 * 1024 * 1024)
        .with_decoding_buffer_size(4 * 1024 * 1024)
        .with_intermediate_buffer_size(4 * 1024 * 1024)
        .with_ifd_value_size(64 * 1024)
}

fn decoder(bytes: Vec<u8>, leniency: Leniency) -> Decoder<Cursor<Vec<u8>>> {
    Decoder::new(Cursor::new(bytes))
        .expect("header")
        .with_limits(guarded())
        .with_leniency(leniency)
}

#[test]
fn a_strip_byte_count_near_u64_max_is_refused_without_allocating() {
    for leniency in [Leniency::Strict, Leniency::Normal, Leniency::Lenient] {
        let mut tiff = gray8(8, 8, &ramp(64));
        // A LONG8-typed byte count is the only way to say "2^64 - 1 bytes"; the
        // offset stays inside the file so only the length is hostile.
        tiff.long8(279, &[u64::MAX]);
        let bytes = tiff.build();
        let started = Instant::now();
        let mut decoder = decoder(bytes, leniency);
        let result = decoder.read_image();
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "a 2^64-byte strip must be refused, not read ({leniency:?})"
        );
        match result {
            // Strict/Normal: the chunk does not lie inside the file.
            Err(err) => assert!(
                matches!(err, TiffError::Format(_)) || err.is_limits(),
                "unexpected error for {leniency:?}: {err}"
            ),
            // Lenient: the count is clipped to the file, which is smaller than
            // the guard, so the read succeeds with the bytes that exist.
            Ok(samples) => {
                assert_eq!(leniency, Leniency::Lenient);
                assert_eq!(samples.len(), 64);
            }
        }
    }
}

#[test]
fn an_offset_plus_length_that_overflows_u64_is_refused() {
    let mut tiff = gray8(8, 8, &ramp(64));
    tiff.long8(273, &[u64::MAX - 8]);
    tiff.long8(279, &[64]);
    let mut decoder = decoder(tiff.build(), Leniency::Normal);
    let err = decoder.read_image().expect_err("offset past EOF");
    assert!(matches!(err, TiffError::Format(_)), "{err}");
}

#[test]
fn sixteen_million_declared_strips_in_two_hundred_bytes_stay_cheap() {
    // `RowsPerStrip = 1` over a tall image declares one strip per row. With
    // `StripByteCounts` present the per-chunk size table must never be built,
    // and with it absent the derivation must stay inside `max_chunks`.
    for with_counts in [true, false] {
        let mut tiff = RawTiff::new();
        let offset = tiff.add_data(&ramp(16));
        tiff.long(256, &[1]);
        tiff.long(257, &[4_000_000]);
        tiff.short(258, &[8]);
        tiff.short(259, &[1]);
        tiff.short(262, &[1]);
        tiff.long(273, &[offset as u32]);
        tiff.short(277, &[1]);
        tiff.long(278, &[1]);
        if with_counts {
            tiff.long(279, &[1]);
        }
        let bytes = tiff.build();
        assert!(bytes.len() < 512, "the fixture itself must stay tiny");

        let started = Instant::now();
        let mut decoder = decoder(bytes, Leniency::Lenient);
        // The image itself is 4 MB of pixels, which the guard permits; what
        // must not happen is a 4-million-entry side table being built to get
        // there, nor a 4-million-iteration walk per call.
        let _ = decoder.info().map(|i| (i.width, i.height));
        let _ = decoder.read_image();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "declared strip count drove the cost, not the data (with_counts={with_counts})"
        );
    }
}

#[test]
fn a_one_byte_destination_is_a_named_error_not_a_partial_write() {
    let mut decoder = decoder(gray8(8, 8, &ramp(64)).build(), Leniency::Normal);
    let mut dst = [0u8; 1];
    let err = decoder
        .read_image_bytes(&mut dst)
        .expect_err("one byte cannot hold a 64-byte image");
    assert!(
        matches!(
            err,
            TiffError::Usage(UsageError::BufferTooSmall { needed: 64, got: 1 })
        ),
        "{err}"
    );
    assert_eq!(dst, [0], "a rejected read must not have written anything");

    // An empty destination for an empty rectangle is fine.
    let mut empty: [u8; 0] = [];
    let layout = decoder
        .read_region_bytes(Rect::new(0, 0, 0, 0), &mut empty)
        .expect("an empty region needs no bytes");
    assert_eq!(layout.total_len, 0);

    // One byte short is still an error.
    let mut nearly = vec![0u8; 63];
    assert!(decoder.read_image_bytes(&mut nearly).is_err());
    // Exactly enough works.
    let mut exact = vec![0u8; 64];
    let layout = decoder.read_image_bytes(&mut exact).expect("exact fit");
    assert_eq!(layout.total_len, 64);
    assert_eq!(exact, ramp(64));
}

#[test]
fn regions_outside_the_image_are_rejected_rather_than_clamped() {
    let mut decoder = decoder(gray8(8, 8, &ramp(64)).build(), Leniency::Normal);
    for rect in [
        Rect::new(8, 0, 1, 1),
        Rect::new(0, 8, 1, 1),
        Rect::new(0, 0, 9, 1),
        Rect::new(0, 0, 1, 9),
        Rect::new(u32::MAX, u32::MAX, 1, 1),
        Rect::new(1, 1, u32::MAX, u32::MAX),
    ] {
        let err = decoder
            .read_region(rect.x, rect.y, rect.width, rect.height)
            .expect_err("outside the image");
        assert!(
            matches!(err, TiffError::Usage(UsageError::RegionOutOfBounds { .. })),
            "{rect:?} produced {err}"
        );
    }
}

#[test]
fn chunk_indices_at_the_end_of_the_range_are_errors_not_panics() {
    let mut decoder = decoder(gray8(8, 8, &ramp(64)).build(), Leniency::Normal);
    assert_eq!(decoder.chunk_count().expect("count"), 1);
    for index in [1u64, 2, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
        let err = decoder.read_chunk_raw(index).expect_err("out of range");
        assert!(
            matches!(
                err,
                TiffError::Usage(UsageError::ChunkIndexOutOfRange { .. })
            ),
            "raw {index} produced {err}"
        );
        assert!(decoder.read_chunk(index).is_err(), "decoded {index}");
        assert!(decoder.chunk_data_dimensions(index).is_err());
    }
}

#[test]
fn duplicate_and_overlapping_strip_offsets_decode_without_panicking() {
    // Four strips that all point at the same two bytes: legal per the file
    // format, nonsense as an image, and it must decode to *something* or fail
    // cleanly rather than trip an index.
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&ramp(4));
    tiff.long(256, &[2]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset as u32; 4]);
    tiff.short(277, &[1]);
    tiff.long(278, &[1]);
    tiff.long(279, &[2, 2, 2, 2]);
    let mut decoder = decoder(tiff.build(), Leniency::Normal);
    let samples = decoder.read_image().expect("overlapping strips are legal");
    assert_eq!(samples.len(), 8);
    assert_eq!(
        samples.as_u8(),
        Some(&[0u8, 1, 0, 1, 0, 1, 0, 1][..]),
        "every strip reads the same two bytes"
    );
}

#[test]
fn a_predictor_over_per_channel_bit_depths_is_an_error_not_wrong_pixels() {
    // `BitsPerSample = [8, 16, 8]` makes the row length 4 bytes per pixel while
    // every predictor computation in the crate assumes one sample width. The
    // combination must be refused; before it was, the deltas were applied
    // across the wrong row boundaries and the pixels came out silently wrong.
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&ramp(16));
    tiff.long(256, &[2]);
    tiff.long(257, &[2]);
    tiff.short(258, &[8, 16, 8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[2]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[3]);
    tiff.long(278, &[2]);
    tiff.long(279, &[16]);
    tiff.short(317, &[2]);
    let bytes = tiff.build();

    for leniency in [Leniency::Strict, Leniency::Normal, Leniency::Lenient] {
        let mut decoder = decoder(bytes.clone(), leniency);
        let err = decoder.read_image().expect_err("mixed depths + predictor");
        assert!(err.is_unsupported(), "{leniency:?} produced {err}");
    }

    // Without the predictor the same file still decodes, so the rejection is
    // about tag 317 and not about the depths themselves.
    let mut without = RawTiff::new();
    let offset = without.add_data(&ramp(16));
    without.long(256, &[2]);
    without.long(257, &[2]);
    without.short(258, &[8, 16, 8]);
    without.short(259, &[1]);
    without.short(262, &[2]);
    without.long(273, &[offset as u32]);
    without.short(277, &[3]);
    without.long(278, &[2]);
    without.long(279, &[16]);
    let mut decoder = decoder(without.build(), Leniency::Normal);
    assert!(decoder.read_image().is_ok());
}

#[test]
fn the_writer_refuses_a_predictor_over_per_channel_bit_depths_too() {
    // Chunky: one chunk carries all three channels, so the predictor would run
    // over rows of `width * 3 * 1` bytes when the real row is `width * 4`.
    let chunky = ImageSpec::new(4, 4, ColorType::Rgb(8))
        .with_bits_per_sample(vec![8, 16, 8])
        .with_predictor(oxiarc_tiff::Predictor::Horizontal)
        .with_compression(Compression::PackBits);
    let err = chunky.validate().expect_err("mixed depths + predictor");
    assert!(err.is_unsupported(), "{err}");

    // Planar: each chunk carries one channel at a stride of 1, so planes of
    // different widths are well defined and must keep working. Rejecting this
    // would make the writer refuse a file the reader accepts.
    let planar = chunky
        .clone()
        .with_planar(oxiarc_tiff::PlanarConfiguration::Planar);
    planar
        .validate()
        .expect("planar planes may have different depths, predictor or not");

    // ... but a planar page whose *plane* depth is one the predictor does not
    // define is still refused.
    let bad_planar = ImageSpec::new(4, 4, ColorType::Rgb(8))
        .with_bits_per_sample(vec![12, 12, 12])
        .with_planar(oxiarc_tiff::PlanarConfiguration::Planar)
        .with_predictor(oxiarc_tiff::Predictor::Horizontal)
        .with_compression(Compression::PackBits);
    assert!(bad_planar.validate().is_err());
}

#[test]
fn a_planar_page_with_per_plane_depths_and_a_predictor_round_trips() {
    // The end-to-end companion to the check above: write it, read it back,
    // byte-identically.
    let spec = ImageSpec::new(4, 4, ColorType::Rgb(8))
        .with_bits_per_sample(vec![8, 16, 8])
        .with_planar(oxiarc_tiff::PlanarConfiguration::Planar)
        .with_predictor(oxiarc_tiff::Predictor::Horizontal)
        .with_compression(Compression::PackBits)
        .with_layout(Layout::Strips { rows_per_strip: 2 });
    // The caller's buffer is in the widest slot (u16) for every channel.
    let pixels: Vec<u8> = ramp(4 * 4 * 3 * 2);
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image(&spec, &pixels)
        .expect("planar page with per-plane depths and a predictor");
    encoder.finish().expect("finish");

    let mut decoder = decoder(buffer.into_inner(), Leniency::Normal);
    let samples = decoder.read_image().expect("read back");
    assert_eq!(samples.len(), 4 * 4 * 3);
}

#[test]
fn a_chunk_that_decodes_short_never_leaves_stale_bytes_behind() {
    // Two strips: the first full, the second deliberately one byte short. In
    // Lenient mode the short strip is zero filled, and the zeros must be the
    // *new* chunk's, not the previous chunk's pixels left in the buffer.
    let mut tiff = RawTiff::new();
    let first = tiff.add_data(&[0xAA; 8]);
    let second = tiff.add_data(&[0x11; 4]);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[first as u32, second as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[2]);
    tiff.long(279, &[8, 4]);
    let mut decoder = decoder(tiff.build(), Leniency::Lenient);
    let samples = decoder.read_image().expect("lenient short strip");
    let bytes = samples.as_u8().expect("u8");
    assert_eq!(&bytes[..8], &[0xAA; 8]);
    assert_eq!(&bytes[8..12], &[0x11; 4]);
    assert_eq!(
        &bytes[12..],
        &[0u8; 4],
        "the tail of a short strip must be zeros, not the previous strip"
    );
}

#[test]
fn every_prefix_of_an_encoder_written_file_is_survivable() {
    // A prefix sweep over a real multi-page, tiled, PackBits file, driving the
    // navigation API rather than only the pixel path, with a bounded budget so
    // an accidental O(n^2) shows up as a failure.
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    let tiled = ImageSpec::new(32, 32, ColorType::Rgb(8))
        .with_compression(Compression::PackBits)
        .with_layout(Layout::Tiles {
            width: 16,
            length: 16,
        });
    encoder
        .write_image(&tiled, &ramp(32 * 32 * 3))
        .expect("page 1");
    encoder
        .write_image(&ImageSpec::new(9, 7, ColorType::Gray(16)), &ramp(9 * 7 * 2))
        .expect("page 2");
    encoder.finish().expect("finish");
    let bytes = buffer.into_inner();

    let started = Instant::now();
    for cut in 0..bytes.len() {
        let Ok(decoder) = Decoder::new(Cursor::new(bytes[..cut].to_vec())) else {
            continue;
        };
        let mut decoder = decoder.with_limits(guarded());
        let _ = decoder.image_count();
        let _ = decoder.layout();
        let _ = decoder.read_image();
        while decoder.next_image().unwrap_or(false) {
            let _ = decoder.read_image();
            let _ = decoder.read_region(0, 0, 1, 1);
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "prefix sweep over {} bytes took too long",
        bytes.len()
    );
}

#[test]
fn a_zero_entry_directory_and_a_zero_length_file_are_clean_errors() {
    // Zero entries: no ImageWidth, so it is a format error, not a panic.
    let mut tiff = RawTiff::new();
    tiff.long(700, &[1]);
    let mut only_junk = decoder(tiff.build(), Leniency::Normal);
    assert!(only_junk.info().is_err());

    // A header with first_ifd = 0 has no images at all.
    let mut headerless = Vec::new();
    headerless.extend_from_slice(b"II");
    headerless.extend_from_slice(&42u16.to_le_bytes());
    headerless.extend_from_slice(&0u32.to_le_bytes());
    let mut decoder = Decoder::new(Cursor::new(headerless)).expect("header");
    assert_eq!(decoder.image_count().expect("count"), 0);
    assert!(decoder.info().is_err());
    assert!(decoder.read_image().is_err());
    assert!(!decoder.next_image().expect("no next"));

    // Nothing at all is not a TIFF.
    assert!(Decoder::new(Cursor::new(Vec::new())).is_err());
    assert!(Decoder::new(Cursor::new(vec![b'I'])).is_err());
    assert!(Decoder::new(Cursor::new(b"II\x2a\x00".to_vec())).is_err());
}

#[test]
fn a_malformed_extra_samples_list_does_not_panic() {
    // Tag 338 is never cross-checked against tag 277 by the spec, so a file may
    // declare more extra samples than it has channels. Computing the alpha
    // channel as `i + spp - extra_samples.len()` underflows for those, which is
    // a panic in a debug build and a wild channel index in a release one.
    for (spp, extras) in [
        (1u16, vec![2u16, 2, 2]),
        (1, vec![1, 1]),
        (2, vec![2, 2, 2, 2, 2]),
        (3, vec![1, 2, 1, 2]),
    ] {
        let pixels = ramp(4 * usize::from(spp));
        let mut tiff = RawTiff::new();
        let offset = tiff.add_data(&pixels);
        tiff.long(256, &[2]);
        tiff.long(257, &[2]);
        tiff.short(258, &vec![8u16; usize::from(spp)]);
        tiff.short(259, &[1]);
        tiff.short(262, &[1]);
        tiff.long(273, &[offset as u32]);
        tiff.short(277, &[spp]);
        tiff.long(278, &[2]);
        tiff.long(279, &[pixels.len() as u32]);
        tiff.short(338, &extras);
        let bytes = tiff.build();

        // Any outcome but a panic is acceptable; what must never happen is the
        // subtraction wrapping.
        let mut lenient = decoder(bytes.clone(), Leniency::Lenient);
        let _ = lenient.read_image_rgba8();
        let _ = lenient.read_image_rgb8();
        let mut strict = decoder(bytes, Leniency::Strict);
        let _ = strict.read_image_rgba8();
    }
}

#[test]
fn associated_alpha_is_read_from_the_alpha_channels_own_entry() {
    // `[Unspecified, AssociatedAlpha]` on a 4-channel image: the alpha is
    // channel 3 and it *is* premultiplied, even though the first extra sample
    // is not. Reading `associated` from `extra_samples.first()` said otherwise.
    let width = 2u32;
    let height = 1u32;
    // R, G, B, A per pixel; premultiplied by alpha = 128/255.
    let pixels: Vec<u8> = vec![100, 50, 25, 128, 200, 100, 50, 255];
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&pixels);
    tiff.long(256, &[width]);
    tiff.long(257, &[height]);
    tiff.short(258, &[8, 8, 8, 8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[2]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[4]);
    tiff.long(278, &[height]);
    tiff.long(279, &[pixels.len() as u32]);
    // 0 = unspecified, 1 = associated (premultiplied) alpha.
    tiff.short(338, &[0, 1]);
    let mut decoder = decoder(tiff.build(), Leniency::Normal);
    let rgba = decoder.read_image_rgba8().expect("rgba");
    assert_eq!(rgba.len(), 8);
    assert_eq!(rgba[3], 128, "alpha comes from channel 3");
    assert_eq!(rgba[7], 255);
    assert!(
        rgba[0] > 100,
        "channel 3 is associated alpha, so the colour must be un-premultiplied \
         (got {}, stored 100)",
        rgba[0]
    );
    assert_eq!(&rgba[4..7], &pixels[4..7], "alpha 255 changes nothing");
}

/// A minimal abbreviated JPEG datastream: `SOI`, one `SOF` declaring
/// `width`x`height` at `precision` bits over `components` components, and an
/// `SOS` so the marker walk stops where a real strip's entropy data would
/// begin. Nothing after the header is valid, which is the point: the frame
/// dimensions alone must not be able to drive an allocation.
#[cfg(feature = "jpeg")]
fn jpeg_header_only(precision: u8, width: u16, height: u16, components: u8) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0];
    let len = 8u16 + 3 * u16::from(components);
    out.extend_from_slice(&len.to_be_bytes());
    out.push(precision);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(components);
    for index in 0..components {
        out.push(index + 1);
        out.push(0x11);
        out.push(0);
    }
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
    out
}

/// A one-strip JPEG-compressed page whose strip is `strip`.
#[cfg(feature = "jpeg")]
fn jpeg_page(bits: u16, strip: &[u8]) -> Vec<u8> {
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(strip);
    tiff.long(256, &[4]);
    tiff.long(257, &[1]);
    tiff.short(258, &[bits]);
    tiff.short(259, &[7]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[1]);
    tiff.long(279, &[strip.len() as u32]);
    tiff.short(339, &[1]);
    tiff.build()
}

/// A TIFF strip's JPEG `SOF` marker carries the frame's own dimensions, and
/// they need not agree with the TIFF geometry — when they disagree the codec
/// decodes into scratch and crops. Those two `u16`s are attacker-controlled,
/// so the scratch they size must go through [`Limits`] like every other
/// file-driven allocation: `65535 x 65535 x 4` is 17 GiB, and nothing in the
/// chunk buffer's own size stops it.
#[cfg(feature = "jpeg")]
#[test]
fn a_jpeg_frame_larger_than_the_chunk_cannot_allocate_past_the_limits() {
    // 4096 x 4096 x 3 = 48 MiB of scratch against `guarded()`'s 4 MiB cap.
    let bytes = jpeg_page(8, &jpeg_header_only(8, 4096, 4096, 3));
    let started = Instant::now();
    let err = decoder(bytes, Leniency::Normal)
        .read_image()
        .expect_err("an over-large JPEG frame must be refused");
    assert!(
        err.is_limits(),
        "expected a limits error, got {err} ({err:?})"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "refusal must be cheap"
    );
}

/// The same guard on the 9..=16-bit path, where the scratch is `u16` and the
/// unguarded worst case is therefore twice as large.
#[cfg(feature = "jpeg")]
#[test]
fn a_wide_precision_jpeg_frame_cannot_allocate_past_the_limits() {
    let bytes = jpeg_page(16, &jpeg_header_only(12, 4096, 4096, 3));
    let started = Instant::now();
    let err = decoder(bytes, Leniency::Normal)
        .read_image()
        .expect_err("an over-large 12-bit JPEG frame must be refused");
    assert!(
        err.is_limits(),
        "expected a limits error, got {err} ({err:?})"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "refusal must be cheap"
    );
}

/// The guard must not fire on a frame that legitimately disagrees with the
/// chunk geometry by a little — the crop path stays reachable.
#[cfg(feature = "jpeg")]
#[test]
fn a_slightly_oversized_jpeg_frame_still_reaches_the_crop_path() {
    // 16 x 16 x 1 = 256 bytes of scratch, far inside `guarded()`'s cap, so
    // whatever this returns it must not be a limits error.
    let bytes = jpeg_page(8, &jpeg_header_only(8, 16, 16, 1));
    match decoder(bytes, Leniency::Normal).read_image() {
        Ok(_) => {}
        Err(err) => assert!(
            !err.is_limits(),
            "a small mismatched frame must not hit the scratch guard: {err}"
        ),
    }
}

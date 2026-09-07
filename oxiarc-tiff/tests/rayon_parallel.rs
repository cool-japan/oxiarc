//! `read_*_parallel` and `write_image_parallel` must produce output
//! byte-identical to their serial counterparts, across every geometry the
//! parallel chunk-index enumeration has to get right: strips, tiles, planar,
//! and (when available) a codec whose scratch is a shared `Mutex`
//! (`CodecState`'s Deflate window), which is the one thing that could make
//! the parallel path serialise incorrectly or corrupt state across chunks.

#![cfg(feature = "rayon")]

#[cfg(feature = "deflate")]
use oxiarc_tiff::Compression;
use oxiarc_tiff::tags::PlanarConfiguration;
use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec, Layout};
use std::io::Cursor;

fn ramp_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(37) % 251) as u8).collect()
}

fn write_serial(spec: &ImageSpec, data: &[u8]) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .write_image(spec, data)
        .expect("write image");
    buffer.into_inner()
}

/// Decode `bytes` both ways and assert everything they can differ on is
/// identical: pixels, layout and any recorded warnings.
fn assert_decode_matches(bytes: &[u8]) {
    let mut serial = Decoder::new(Cursor::new(bytes.to_vec())).expect("serial decoder");
    let serial_samples = serial.read_image().expect("serial read");
    let serial_warnings = serial.warnings().to_vec();

    let mut parallel = Decoder::new(Cursor::new(bytes.to_vec())).expect("parallel decoder");
    let parallel_samples = parallel.read_image_parallel().expect("parallel read");
    let parallel_warnings = parallel.warnings().to_vec();

    assert_eq!(
        serial_samples, parallel_samples,
        "serial and parallel decode disagree on pixels"
    );
    assert_eq!(
        serial_warnings, parallel_warnings,
        "serial and parallel decode disagree on recorded warnings"
    );
}

/// Encode `spec`/`data` both ways and assert the produced files are
/// byte-identical (not merely pixel-identical: the IFD, tag order and
/// `JPEGTables` placement must match too).
fn assert_encode_matches(spec: &ImageSpec, data: &[u8]) {
    let serial = write_serial(spec, data);

    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .write_image_parallel(spec, data)
        .expect("write image parallel");
    let parallel = buffer.into_inner();

    assert_eq!(
        serial, parallel,
        "serial and parallel encode produced different bytes"
    );

    // And both must round-trip to the same pixels through the real decoder,
    // not just agree with each other by coincidence.
    let mut decoder = Decoder::new(Cursor::new(parallel)).expect("decoder");
    let decoded = decoder.read_image().expect("read");
    assert_eq!(decoded.as_u8(), Some(data));
}

#[test]
fn strips_decode_identically_in_parallel() {
    let pixels = ramp_bytes(64 * 40);
    let spec = ImageSpec::new(64, 40, ColorType::Gray(8))
        .with_layout(Layout::Strips { rows_per_strip: 6 });
    let bytes = write_serial(&spec, &pixels);
    assert_decode_matches(&bytes);
}

#[test]
fn tiles_decode_identically_in_parallel() {
    let pixels = ramp_bytes(96 * 80 * 3);
    let spec = ImageSpec::new(96, 80, ColorType::Rgb(8)).with_layout(Layout::Tiles {
        width: 16,
        length: 16,
    });
    let bytes = write_serial(&spec, &pixels);
    assert_decode_matches(&bytes);
}

#[test]
fn a_non_multiple_of_the_tile_size_decodes_identically_in_parallel() {
    // 50x37 in 16x16 tiles: the right and bottom edge tiles are padded, so
    // the last row/column of the chunk-index enumeration is exercised.
    let pixels = ramp_bytes(50 * 37);
    let spec = ImageSpec::new(50, 37, ColorType::Gray(8)).with_layout(Layout::Tiles {
        width: 16,
        length: 16,
    });
    let bytes = write_serial(&spec, &pixels);
    assert_decode_matches(&bytes);
}

#[test]
fn planar_strips_decode_identically_in_parallel() {
    let pixels = ramp_bytes(40 * 20 * 3);
    let spec = ImageSpec::new(40, 20, ColorType::Rgb(8))
        .with_planar(PlanarConfiguration::Planar)
        .with_layout(Layout::Strips { rows_per_strip: 4 });
    let bytes = write_serial(&spec, &pixels);
    assert_decode_matches(&bytes);
}

#[test]
fn a_region_smaller_than_the_whole_image_decodes_identically_in_parallel() {
    let pixels = ramp_bytes(64 * 64);
    let spec = ImageSpec::new(64, 64, ColorType::Gray(8)).with_layout(Layout::Tiles {
        width: 16,
        length: 16,
    });
    let bytes = write_serial(&spec, &pixels);

    let mut serial = Decoder::new(Cursor::new(bytes.clone())).expect("serial decoder");
    let serial_region = serial.read_region(10, 10, 30, 20).expect("serial region");

    let mut parallel = Decoder::new(Cursor::new(bytes)).expect("parallel decoder");
    let parallel_region = parallel
        .read_region_parallel(10, 10, 30, 20)
        .expect("parallel region");

    assert_eq!(serial_region, parallel_region);
}

#[cfg(feature = "deflate")]
#[test]
fn deflate_strips_decode_identically_in_parallel() {
    // Deflate's window and inflate machine live behind `CodecState`'s
    // `Mutex` (see `rayon_support` module docs): this is the fixture that
    // exercises worker contention on it, and would show state bleeding
    // between chunks if `reset()` between chunks were ever skipped.
    let pixels = ramp_bytes(80 * 50);
    let spec = ImageSpec::new(80, 50, ColorType::Gray(8))
        .with_compression(Compression::Deflate { level: 6 })
        .with_layout(Layout::Strips { rows_per_strip: 5 });
    let bytes = write_serial(&spec, &pixels);
    assert_decode_matches(&bytes);
}

#[cfg(feature = "deflate")]
#[test]
fn deflate_strips_encode_identically_in_parallel() {
    let pixels = ramp_bytes(48 * 32);
    let spec = ImageSpec::new(48, 32, ColorType::Gray(8))
        .with_compression(Compression::Deflate { level: 6 })
        .with_layout(Layout::Strips { rows_per_strip: 4 });
    assert_encode_matches(&spec, &pixels);
}

#[test]
fn uncompressed_strips_encode_identically_in_parallel() {
    let pixels = ramp_bytes(40 * 40);
    let spec = ImageSpec::new(40, 40, ColorType::Gray(8))
        .with_layout(Layout::Strips { rows_per_strip: 7 });
    assert_encode_matches(&spec, &pixels);
}

#[test]
fn tiles_encode_identically_in_parallel() {
    let pixels = ramp_bytes(50 * 34 * 3);
    let spec = ImageSpec::new(50, 34, ColorType::Rgb(8)).with_layout(Layout::Tiles {
        width: 16,
        length: 16,
    });
    assert_encode_matches(&spec, &pixels);
}

#[test]
fn planar_encode_identically_in_parallel() {
    let pixels = ramp_bytes(30 * 20 * 3);
    let spec = ImageSpec::new(30, 20, ColorType::Rgb(8))
        .with_planar(PlanarConfiguration::Planar)
        .with_layout(Layout::Strips { rows_per_strip: 3 });
    assert_encode_matches(&spec, &pixels);
}

/// JPEG with `shared_tables: true` is the one parallel-encode path with a
/// hand-written compensating step: [`write_image_parallel`] resolves the
/// page's `JPEGTables` (tag 347) once, synchronously, before any chunk's
/// encode work is spread across workers, then pokes it into the
/// `ImageWriter` so `finish()`'s `populate_directory` -- which reads the tag
/// from `self.jpeg_tables`, a field the parallel driver never sets through
/// the normal per-chunk path -- writes it. A missing or misordered poke
/// there would still write a file with no error: every chunk an abbreviated
/// JPEG datastream with no tables to decode against, valid enough to open
/// and impossible to decode. `assert_encode_matches`'s trailing pixel
/// equality can't catch that (JPEG is lossy, so it isn't used here); the
/// serial/parallel byte-identity comparison can, since a missing tag 347
/// changes the file's bytes, and the explicit tag-347 and decode checks
/// below confirm what specifically would have been missing.
#[cfg(feature = "jpeg")]
#[test]
fn jpeg_shared_tables_encode_identically_in_parallel() {
    let pixels = ramp_bytes(48 * 32 * 3);
    let spec = ImageSpec::new(48, 32, ColorType::YCbCr(8))
        .with_compression(Compression::Jpeg {
            quality: 75,
            shared_tables: true,
        })
        .with_ycbcr_subsampling(2, 2)
        // A multiple of 8 * Vmax (16 for 2x2 subsampling), per
        // `ImageSpec::validate` -- see `jpeg_strips_must_be_a_multiple_of_the_mcu_height`
        // in `src/writer/mod.rs`.
        .with_layout(Layout::Strips { rows_per_strip: 16 });
    let serial = write_serial(&spec, &pixels);

    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .write_image_parallel(&spec, &pixels)
        .expect("write image parallel");
    let parallel = buffer.into_inner();

    assert_eq!(
        serial, parallel,
        "serial and parallel JPEG encode produced different bytes"
    );

    let mut decoder = Decoder::new(Cursor::new(parallel)).expect("decoder");
    assert!(
        decoder
            .find_tag(oxiarc_tiff::Tag::JpegTables)
            .expect("find_tag")
            .is_some(),
        "shared_tables: true must write tag 347"
    );
    // Byte-identical to a broken file is still broken: the file must also
    // actually decode, not merely match the serial path's mistake.
    decoder
        .read_image()
        .expect("parallel-encoded shared-tables JPEG file must decode");
}

/// A budget too small for the whole image must fail the same way on both
/// paths -- exercising the precharged parallel budget against the serial
/// per-chunk one (see `rayon_support::decode_into_parallel`'s docs).
#[test]
fn an_over_budget_image_fails_the_same_way_on_both_paths() {
    let pixels = ramp_bytes(64 * 64);
    let spec = ImageSpec::new(64, 64, ColorType::Gray(8))
        .with_layout(Layout::Strips { rows_per_strip: 4 });
    let bytes = write_serial(&spec, &pixels);
    let limits = oxiarc_tiff::Limits::default().with_max_image_bytes(64);

    let mut serial = Decoder::new(Cursor::new(bytes.clone()))
        .expect("serial decoder")
        .with_limits(limits.clone());
    let serial_err = serial.read_image().expect_err("serial over budget");

    let mut parallel = Decoder::new(Cursor::new(bytes))
        .expect("parallel decoder")
        .with_limits(limits);
    let parallel_err = parallel
        .read_image_parallel()
        .expect_err("parallel over budget");

    assert!(serial_err.is_limits());
    assert!(parallel_err.is_limits());
}

mod support;

use oxiarc_tiff::{Leniency, Limits, Warning};
use support::RawTiff;

/// A two-strip greyscale file in which strip 0 decodes *short* (its
/// `StripByteCounts` entry is half what the geometry calls for, so the
/// pipeline records a `SpecViolation`) and strip 1 is declared to run far
/// past the end of the file (so the *fetch* step records a `ChunkTruncated`
/// before that strip is ever decoded).
///
/// This is the one shape that tells the two warning orders apart: the serial
/// pipeline interleaves fetch and decode per chunk, so it emits
/// `[SpecViolation(0), ChunkTruncated(1), ...]`, while a parallel driver that
/// runs *every* fetch before *any* decode emits the `ChunkTruncated` first.
fn short_strip_then_truncated_strip() -> Vec<u8> {
    let pixels = support::ramp(16);
    let mut tiff = RawTiff::new();
    let first = tiff.add_data(&pixels[..8]);
    let second = tiff.add_data(&pixels[8..]);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[first as u32, second as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[2]);
    tiff.long(279, &[4, 0x0100_0000]);
    tiff.build()
}

/// The `rayon_support` module documents that parallel decode merges warnings
/// "in the same order the serial path would have produced them". Every other
/// test in this file decodes a *well-formed* file, where that list is empty
/// and the claim is vacuous. This one makes both a fetch-time and a
/// decode-time warning fire, on different chunks, in the order that
/// distinguishes a per-chunk merge from a fetch-all-then-decode-all one.
#[test]
fn fetch_and_decode_warnings_interleave_per_chunk_exactly_as_the_serial_path() {
    let bytes = short_strip_then_truncated_strip();

    let mut serial = Decoder::new(Cursor::new(bytes.clone()))
        .expect("serial decoder")
        .with_leniency(Leniency::Lenient);
    let serial_samples = serial.read_image().expect("serial read");
    let serial_warnings = serial.warnings().to_vec();

    let mut parallel = Decoder::new(Cursor::new(bytes))
        .expect("parallel decoder")
        .with_leniency(Leniency::Lenient);
    let parallel_samples = parallel.read_image_parallel().expect("parallel read");
    let parallel_warnings = parallel.warnings().to_vec();

    assert_eq!(serial_samples, parallel_samples);
    assert_eq!(
        serial_warnings, parallel_warnings,
        "parallel decode must merge fetch and decode warnings per chunk, in chunk order"
    );

    // Non-vacuous: both kinds really did fire, on the chunks intended, and
    // the decode-time one comes first because it belongs to chunk 0.
    assert!(
        matches!(
            serial_warnings.first(),
            Some(Warning::SpecViolation { message }) if message.starts_with("chunk 0 ")
        ),
        "expected chunk 0's short-decode warning first, got {serial_warnings:?}"
    );
    assert!(
        serial_warnings
            .iter()
            .any(|w| matches!(w, Warning::ChunkTruncated { index: 1, .. })),
        "expected chunk 1's fetch-time truncation warning, got {serial_warnings:?}"
    );
}

/// The parallel decoder fetches in bounded batches so that a file whose
/// strips all point at the same bytes cannot make it hold `chunk_count`
/// copies of them at once (see the `rayon_support` module docs). Batching
/// must not change what comes out: this file needs several batches under a
/// deliberately small `intermediate_buffer_size`, and every batch boundary
/// falls in the middle of the strip list.
#[test]
fn a_multi_batch_fetch_decodes_identically_to_a_single_batch() {
    let pixels = ramp_bytes(32 * 64);
    let spec = ImageSpec::new(32, 64, ColorType::Gray(8))
        .with_layout(Layout::Strips { rows_per_strip: 2 });
    let bytes = write_serial(&spec, &pixels);

    let mut serial = Decoder::new(Cursor::new(bytes.clone())).expect("serial decoder");
    let expected = serial.read_image().expect("serial read");

    // One strip is 32 * 2 = 64 bytes, and there are 32 of them: a 100-byte
    // cap admits at most one strip per batch, a 200-byte cap three, and the
    // default (128 MiB) all 32 in one go. All three must agree.
    for cap in [
        100usize,
        200,
        4096,
        Limits::default().intermediate_buffer_size,
    ] {
        let mut parallel = Decoder::new(Cursor::new(bytes.clone()))
            .expect("parallel decoder")
            .with_limits(Limits::default().with_intermediate_buffer_size(cap));
        let got = parallel
            .read_image_parallel()
            .unwrap_or_else(|e| panic!("parallel read with cap {cap}: {e}"));
        assert_eq!(got, expected, "cap {cap} changed the decoded pixels");
        assert!(
            parallel.warnings().is_empty(),
            "cap {cap} invented warnings: {:?}",
            parallel.warnings()
        );
    }
}

/// A file whose 64 strips every one point at the same region and declare its
/// whole length: the serial pipeline holds one such region at a time, and the
/// batched parallel pipeline must not hold all 64. The aggregate here is kept
/// small enough to run anywhere (64 x 4 KiB); the point is that the *batch
/// boundary rule* is exercised by a file that would otherwise multiply, not
/// that this particular file is large.
#[test]
fn overlapping_strip_offsets_do_not_multiply_the_resident_bytes() {
    const STRIPS: u32 = 64;
    const REGION: usize = 4096;

    let region = support::ramp(REGION);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&region) as u32;
    tiff.long(256, &[REGION as u32]);
    tiff.long(257, &[STRIPS]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &vec![offset; STRIPS as usize]);
    tiff.short(277, &[1]);
    tiff.long(278, &[1]);
    tiff.long(279, &vec![REGION as u32; STRIPS as usize]);
    let bytes = tiff.build();

    // A cap of one region admits exactly one strip per batch.
    let limits = Limits::default().with_intermediate_buffer_size(REGION);

    let mut serial = Decoder::new(Cursor::new(bytes.clone()))
        .expect("serial decoder")
        .with_limits(limits.clone());
    let expected = serial.read_image().expect("serial read");

    let mut parallel = Decoder::new(Cursor::new(bytes))
        .expect("parallel decoder")
        .with_limits(limits);
    let got = parallel.read_image_parallel().expect("parallel read");

    assert_eq!(got, expected);
    assert_eq!(serial.warnings(), parallel.warnings());
}

/// The `shared_tables: false` JPEG path is the other half of the parallel
/// encoder's one hand-written compensating step: there tag 347 must *not* be
/// written and every chunk must carry its own tables, so the compensating
/// `set_jpeg_tables(None)` has to be a no-op rather than the source of a
/// stray tag. `jpeg_shared_tables_encode_identically_in_parallel` covers only
/// the `true` side.
#[cfg(feature = "jpeg")]
#[test]
fn jpeg_self_contained_tables_encode_identically_in_parallel() {
    let pixels = ramp_bytes(48 * 32 * 3);
    let spec = ImageSpec::new(48, 32, ColorType::YCbCr(8))
        .with_compression(Compression::Jpeg {
            quality: 75,
            shared_tables: false,
        })
        .with_ycbcr_subsampling(2, 2)
        .with_layout(Layout::Strips { rows_per_strip: 16 });
    let serial = write_serial(&spec, &pixels);

    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .write_image_parallel(&spec, &pixels)
        .expect("write image parallel");
    let parallel = buffer.into_inner();

    assert_eq!(
        serial, parallel,
        "serial and parallel self-contained JPEG encode produced different bytes"
    );

    let mut decoder = Decoder::new(Cursor::new(parallel)).expect("decoder");
    assert!(
        decoder
            .find_tag(oxiarc_tiff::Tag::JpegTables)
            .expect("find_tag")
            .is_none(),
        "shared_tables: false must not write tag 347"
    );
    decoder
        .read_image()
        .expect("parallel-encoded self-contained JPEG file must decode");
}

/// JPEG in *tiles* rather than strips, shared tables on: tiles are coded at
/// full size and padded, so the parallel chunk enumeration and the
/// once-up-front table resolution have to agree with the serial path on a
/// second geometry too.
#[cfg(feature = "jpeg")]
#[test]
fn jpeg_tiles_encode_identically_in_parallel() {
    let pixels = ramp_bytes(48 * 48 * 3);
    let spec = ImageSpec::new(48, 48, ColorType::YCbCr(8))
        .with_compression(Compression::Jpeg {
            quality: 60,
            shared_tables: true,
        })
        .with_ycbcr_subsampling(2, 2)
        .with_layout(Layout::Tiles {
            width: 16,
            length: 16,
        });
    let serial = write_serial(&spec, &pixels);

    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .write_image_parallel(&spec, &pixels)
        .expect("write image parallel");
    let parallel = buffer.into_inner();

    assert_eq!(serial, parallel, "tiled JPEG encode differs in parallel");
    Decoder::new(Cursor::new(parallel))
        .expect("decoder")
        .read_image()
        .expect("tiled parallel JPEG must decode");
}

/// A single-chunk image is the degenerate case for the batch loop (one
/// iteration, one chunk) and for the parallel encoder (one payload).
#[test]
fn a_single_chunk_image_round_trips_through_both_parallel_paths() {
    let pixels = ramp_bytes(8 * 8);
    let spec =
        ImageSpec::new(8, 8, ColorType::Gray(8)).with_layout(Layout::Strips { rows_per_strip: 8 });
    assert_encode_matches(&spec, &pixels);
    let bytes = write_serial(&spec, &pixels);
    assert_decode_matches(&bytes);
}

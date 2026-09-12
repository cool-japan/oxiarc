//! End-to-end round trips through the real [`Encoder`]/[`Writer`], then back
//! through the decoder.
//!
//! This is the one place in the whole test suite that actually *runs*:
//!
//! - the interlaced encode path (`set_interlaced(true)` followed by
//!   `write_image_data`) -- everywhere else, `extract_pass_row` is exercised
//!   only directly against `expand_pass`, never through the encoder;
//! - the indexed-colour + `set_palette` path -- the one real-world call site
//!   in the design survey that writes a palette (`fop-pdf-renderer`'s image
//!   writer);
//! - 16-bit-depth encoding;
//! - sub-byte (1/2/4-bit) depth encoding, interlaced and not.
//!
//! `png-design.md` names Adam7 combined with sub-byte depths as the specific
//! shape "where every in-house implementation failed" (risk 5); this file
//! exists so that claim is backed by a passing test rather than by careful
//! reading of `extract_pass_row` alone.
//!
//! [`Encoder`]: oxiarc_png::Encoder
//! [`Writer`]: oxiarc_png::Writer

mod common;

use common::Rng;
use oxiarc_png::header::Ihdr;
use oxiarc_png::{BitDepth, ColorType, EncodingError, EncodingFormatErrorKind, Filter, Interlace};

/// Every legal `(colour type, bit depth)` pair (Table 11.1).
const COMBINATIONS: &[(ColorType, BitDepth)] = &[
    (ColorType::Grayscale, BitDepth::One),
    (ColorType::Grayscale, BitDepth::Two),
    (ColorType::Grayscale, BitDepth::Four),
    (ColorType::Grayscale, BitDepth::Eight),
    (ColorType::Grayscale, BitDepth::Sixteen),
    (ColorType::Rgb, BitDepth::Eight),
    (ColorType::Rgb, BitDepth::Sixteen),
    (ColorType::Indexed, BitDepth::One),
    (ColorType::Indexed, BitDepth::Two),
    (ColorType::Indexed, BitDepth::Four),
    (ColorType::Indexed, BitDepth::Eight),
    (ColorType::GrayscaleAlpha, BitDepth::Eight),
    (ColorType::GrayscaleAlpha, BitDepth::Sixteen),
    (ColorType::Rgba, BitDepth::Eight),
    (ColorType::Rgba, BitDepth::Sixteen),
];

const FILTERS: &[Filter] = &[
    Filter::NoFilter,
    Filter::Sub,
    Filter::Up,
    Filter::Avg,
    Filter::Paeth,
    Filter::Adaptive,
    Filter::MinEntropy,
];

/// A 256-entry palette with no repeated triples, so an off-by-one index
/// error cannot hide behind a duplicate entry.
fn palette_256() -> Vec<u8> {
    (0..256u16)
        .flat_map(|i| {
            [
                i as u8,
                (i.wrapping_mul(7)) as u8,
                (i.wrapping_mul(13)) as u8,
            ]
        })
        .collect()
}

/// Compare two packed rows, ignoring the undefined padding bits after the
/// last real sample in a sub-byte-depth row -- neither the encoder nor the
/// decoder is required to preserve those (same trick `tests/props.rs` uses
/// for the decode-only matrix).
fn rows_match(a: &[u8], b: &[u8], bits: usize) -> bool {
    let full = bits / 8;
    if a[..full] != b[..full] {
        return false;
    }
    if bits % 8 == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - bits % 8);
    a[full] & mask == b[full] & mask
}

#[allow(clippy::too_many_arguments)]
fn roundtrip_one(
    color_type: ColorType,
    bit_depth: BitDepth,
    width: u32,
    height: u32,
    interlaced: bool,
    filter: Filter,
    seed: u64,
) {
    let ihdr = Ihdr {
        width,
        height,
        bit_depth,
        color_type,
        interlace: if interlaced {
            Interlace::Adam7
        } else {
            Interlace::None
        },
    };
    let stride = ihdr.row_stride();
    let mut rng = Rng::new(seed);
    let samples = rng.bytes(stride * height as usize);

    let mut out = Vec::new();
    let mut enc = oxiarc_png::Encoder::new(&mut out, width, height);
    enc.set_color(color_type);
    enc.set_depth(bit_depth);
    enc.set_interlaced(interlaced);
    enc.set_filter(filter);
    if color_type == ColorType::Indexed {
        enc.set_palette(palette_256());
    }
    let label = format!(
        "{color_type:?}/{bit_depth:?} {width}x{height} interlaced={interlaced} filter={filter:?} seed={seed}"
    );
    let mut w = enc
        .write_header()
        .unwrap_or_else(|e| panic!("write_header {label}: {e}"));
    w.write_image_data(&samples)
        .unwrap_or_else(|e| panic!("write_image_data {label}: {e}"));
    w.finish().unwrap_or_else(|e| panic!("finish {label}: {e}"));

    let image = oxiarc_png::decode(&out).unwrap_or_else(|e| panic!("decode {label}: {e}"));
    assert_eq!(image.width, width, "{label}: width");
    assert_eq!(image.height, height, "{label}: height");
    assert_eq!(image.color_type, color_type, "{label}: color_type");
    assert_eq!(image.bit_depth, bit_depth, "{label}: bit_depth");
    assert_eq!(image.data.len(), samples.len(), "{label}: data length");

    let bits = width as usize * color_type.samples() * usize::from(bit_depth as u8);
    for y in 0..height as usize {
        let a = &image.data[y * stride..(y + 1) * stride];
        let b = &samples[y * stride..(y + 1) * stride];
        assert!(
            rows_match(a, b, bits),
            "{label}: row {y} mismatch\n  got:      {a:?}\n  expected: {b:?}"
        );
    }

    if color_type == ColorType::Indexed {
        let plte = image
            .info
            .palette
            .as_deref()
            .unwrap_or_else(|| panic!("{label}: PLTE did not round-trip"));
        assert_eq!(plte, palette_256().as_slice(), "{label}: palette bytes");
    }
}

/// Every legal `(colour type, bit depth)` pair, non-interlaced and Adam7,
/// across sizes chosen to hit the Adam7 edge cases `png-design.md` §8.15
/// calls out: 1x1 and `height == 1` (several Adam7 passes are empty),
/// `width < 5` (pass 4/6 are empty too), and a couple of "ordinary" sizes.
#[test]
fn every_combination_round_trips_through_the_real_encoder() {
    let sizes: &[(u32, u32)] = &[
        (1, 1),
        (3, 1),
        (1, 3),
        (2, 2),
        (5, 1),
        (1, 5),
        (4, 4),
        (7, 7),
        (9, 5),
        (16, 13),
    ];
    let mut seed = 1u64;
    for &(color_type, bit_depth) in COMBINATIONS {
        for interlace in [false, true] {
            for &(w, h) in sizes {
                roundtrip_one(
                    color_type,
                    bit_depth,
                    w,
                    h,
                    interlace,
                    Filter::Adaptive,
                    seed,
                );
                seed += 1;
            }
        }
    }
}

/// Every filter strategy, on three representative bit-packings (8-bit RGBA,
/// 1-bit grayscale, 4-bit indexed), both scanline orders.
#[test]
fn every_filter_strategy_round_trips() {
    let mut seed = 10_000u64;
    for &filter in FILTERS {
        for interlace in [false, true] {
            roundtrip_one(
                ColorType::Rgba,
                BitDepth::Eight,
                11,
                7,
                interlace,
                filter,
                seed,
            );
            roundtrip_one(
                ColorType::Grayscale,
                BitDepth::One,
                13,
                5,
                interlace,
                filter,
                seed + 1,
            );
            roundtrip_one(
                ColorType::Indexed,
                BitDepth::Four,
                9,
                6,
                interlace,
                filter,
                seed + 2,
            );
            seed += 10;
        }
    }
}

/// `BitDepth::Sixteen` on every colour type that allows it (all but
/// `Indexed`, which Table 11.1 caps at 8 bits).
#[test]
fn sixteen_bit_depth_round_trips_on_every_applicable_colour_type() {
    let colours = [
        ColorType::Grayscale,
        ColorType::Rgb,
        ColorType::GrayscaleAlpha,
        ColorType::Rgba,
    ];
    let mut seed = 77_000u64;
    for color_type in colours {
        for interlace in [false, true] {
            roundtrip_one(
                color_type,
                BitDepth::Sixteen,
                6,
                4,
                interlace,
                Filter::Adaptive,
                seed,
            );
            seed += 1;
        }
    }
}

/// Writing `Indexed` image data without ever calling `set_palette` is
/// rejected at `write_image_data`, not silently accepted with a garbage
/// palette -- `write_header` itself succeeds (it has nothing to validate
/// yet), matching `png` 0.18's own deferred check.
#[test]
fn indexed_without_a_palette_is_rejected_at_write_image_data() {
    let mut out = Vec::new();
    let mut enc = oxiarc_png::Encoder::new(&mut out, 2, 2);
    enc.set_color(ColorType::Indexed);
    enc.set_depth(BitDepth::Eight);
    let mut w = enc
        .write_header()
        .expect("write_header has nothing palette-related to check yet");
    let err = w
        .write_image_data(&[0, 0, 0, 0])
        .expect_err("no palette was ever set");
    assert!(matches!(
        err,
        EncodingError::Format(ref f) if matches!(f.kind(), EncodingFormatErrorKind::NoPalette)
    ));
}

/// The same rejection applies to `stream_writer`, which shares
/// `validate_new_image` with `write_image_data`.
#[test]
fn indexed_without_a_palette_is_rejected_at_stream_writer() {
    let mut out = Vec::new();
    let mut enc = oxiarc_png::Encoder::new(&mut out, 2, 2);
    enc.set_color(ColorType::Indexed);
    enc.set_depth(BitDepth::Eight);
    let mut w = enc.write_header().expect("write_header");
    let err = match w.stream_writer() {
        Ok(_) => panic!("no palette was ever set"),
        Err(e) => e,
    };
    assert!(matches!(
        err,
        EncodingError::Format(ref f) if matches!(f.kind(), EncodingFormatErrorKind::NoPalette)
    ));
}

/// The `parallel` feature's row-band filtering path
/// (`encoder::parallel::should_parallelize` triggers at >= 64
/// transmission-order rows) must actually run, end to end, through the
/// real `Encoder`/`Writer` and round-trip correctly -- not only in the
/// isolated `filter_rows_parallel`-vs-hand-rolled-serial-loop unit test in
/// `src/encoder/parallel.rs`, which proves filtering output matches but
/// never calls `write_image_data` at all. Covers both the non-interlaced
/// and interlaced dispatch branches inside `write_rows_parallel`.
#[cfg(feature = "parallel")]
#[test]
fn parallel_feature_round_trips_a_frame_large_enough_to_trigger_it() {
    for interlace in [false, true] {
        // 97x130 is comfortably past the 64-row `should_parallelize`
        // threshold even counted the interlaced way (summed over all seven
        // Adam7 passes, which is sparser than the plain height).
        roundtrip_one(
            ColorType::Rgba,
            BitDepth::Eight,
            97,
            130,
            interlace,
            Filter::Adaptive,
            0xFEED_0000 + u64::from(interlace),
        );
    }
}

/// Filtered bytes well past `encoder::zlib`'s 32 KiB `DEFLATE_BATCH_SIZE`
/// (input-side batching added to close a ~3x measured encode-speed gap;
/// see that module's doc) must still round-trip pixel-exact through the
/// real `Encoder`/`Writer` and decoder -- the unit test in
/// `src/encoder/zlib.rs` proves the compressed *stream* decompresses
/// correctly in isolation; this proves the whole `Writer::write_image_data`
/// path (filtering, chunk framing, IEND) agrees end to end.
#[test]
fn a_frame_spanning_many_deflate_input_batches_round_trips_exactly() {
    roundtrip_one(
        ColorType::Rgba,
        BitDepth::Eight,
        512,
        256,
        false,
        Filter::Adaptive,
        0xBA7C_4000,
    );
}

/// `Encoder::with_info` is the one route that installs a `FrameControl`
/// without going through `Writer::set_frame_dimension`/`set_frame_position`,
/// which do validate. Before this check it accepted an empty rectangle,
/// whose row stride is zero, and `write_image_data`'s row loop then hit
/// `chunks_exact(0)` -- a hard panic ("chunk size must be non-zero") from
/// entirely safe public API, on both the serial and (with `--features
/// parallel`, at 64+ rows) the parallel row path.
///
/// The rectangle now has to satisfy the same rule everywhere it can be set.
#[test]
fn with_info_rejects_an_empty_frame_rectangle() {
    for (fw, fh, expected) in [(0u32, 4u32, "width"), (4, 0, "height")] {
        // 128 rows so this is also above `MIN_ROWS_FOR_PARALLEL_FILTER`:
        // with `--features parallel` the same call would otherwise reach
        // `write_rows_parallel`'s own `chunks_exact`, a second panic site.
        let mut info = oxiarc_png::Info::with_size(64, 128);
        info.color_type = ColorType::Grayscale;
        info.bit_depth = BitDepth::Eight;
        info.animation_control = Some(oxiarc_png::AnimationControl {
            num_frames: 1,
            num_plays: 0,
        });
        info.frame_control = Some(oxiarc_png::FrameControl {
            sequence_number: 0,
            width: fw,
            height: fh,
            ..Default::default()
        });
        let err = oxiarc_png::Encoder::with_info(Vec::new(), info)
            .err()
            .unwrap_or_else(|| panic!("{fw}x{fh} frame must be rejected"));
        match err {
            EncodingError::Format(f) => {
                let text = f.to_string();
                assert!(
                    text.contains(expected) && text.contains("zero"),
                    "{fw}x{fh}: expected a zero-{expected} error, got {text:?}"
                );
                assert!(matches!(
                    f.kind(),
                    EncodingFormatErrorKind::ZeroWidth | EncodingFormatErrorKind::ZeroHeight
                ));
            }
            other => panic!("expected a format error, got {other:?}"),
        }
    }
}

/// The same route also used to accept a rectangle *outside* the canvas,
/// which is not a panic but is just as wrong: it writes an `fcTL` this
/// crate's own decoder then rejects with `BadSubFrameBounds`, i.e. the
/// encoder silently produced a file it cannot read back.
#[test]
fn with_info_rejects_an_out_of_canvas_frame_rectangle() {
    for (x, y, w, h) in [(0u32, 0u32, 99u32, 4u32), (0, 0, 4, 99), (3, 0, 4, 4)] {
        let mut info = oxiarc_png::Info::with_size(4, 4);
        info.color_type = ColorType::Grayscale;
        info.bit_depth = BitDepth::Eight;
        info.animation_control = Some(oxiarc_png::AnimationControl {
            num_frames: 1,
            num_plays: 0,
        });
        info.frame_control = Some(oxiarc_png::FrameControl {
            sequence_number: 0,
            width: w,
            height: h,
            x_offset: x,
            y_offset: y,
            ..Default::default()
        });
        let err = oxiarc_png::Encoder::with_info(Vec::new(), info)
            .err()
            .unwrap_or_else(|| panic!("{w}x{h}+{x}+{y} must be rejected"));
        match err {
            EncodingError::Format(f) => assert!(
                matches!(f.kind(), EncodingFormatErrorKind::OutOfBounds),
                "{w}x{h}+{x}+{y}: expected OutOfBounds, got {f}"
            ),
            other => panic!("expected a format error, got {other:?}"),
        }
    }
}

/// A legal `with_info` animation still round-trips, so the new validation
/// rejects only what it should: the first frame covers the canvas (as an
/// `fcTL` preceding `IDAT` must), the second is a sub-rectangle.
#[test]
fn with_info_still_accepts_a_legal_animation() {
    let mut info = oxiarc_png::Info::with_size(8, 8);
    info.color_type = ColorType::Rgba;
    info.bit_depth = BitDepth::Eight;
    info.animation_control = Some(oxiarc_png::AnimationControl {
        num_frames: 2,
        num_plays: 0,
    });
    info.frame_control = Some(oxiarc_png::FrameControl {
        sequence_number: 0,
        width: 8,
        height: 8,
        ..Default::default()
    });
    let mut buf = Vec::new();
    let enc = oxiarc_png::Encoder::with_info(&mut buf, info).expect("legal rectangle");
    let mut w = enc.write_header().expect("header");
    w.write_image_data(&Rng::new(3).bytes(8 * 8 * 4))
        .expect("frame 0");
    w.set_frame_dimension(4, 4).expect("shrink");
    w.set_frame_position(2, 2).expect("move");
    w.write_image_data(&Rng::new(4).bytes(4 * 4 * 4))
        .expect("frame 1");
    w.finish().expect("finish");

    let mut dec = oxiarc_png::ApngDecoder::new(&buf[..]).expect("apng decoder");
    for i in 0..2 {
        let frame = dec
            .next_composed()
            .unwrap_or_else(|e| panic!("frame {i}: {e}"))
            .unwrap_or_else(|| panic!("frame {i} missing"));
        assert_eq!((frame.width, frame.height), (8, 8));
    }
    assert!(dec.next_composed().expect("end").is_none());
}

/// An `fcTL` that precedes `IDAT` describes the default image and must
/// cover the whole canvas at the origin (PNG 3rd Edition; `png-design`
/// §8.14). The encoder used to write `fcTL(4x4 at 2,2)` + `IDAT` for the
/// first frame of an animation without complaint -- a file this crate's own
/// decoder then rejects with `BadSubFrameBounds`, i.e. silently
/// unreadable output. `png` 0.18 has the identical hole (its
/// `set_frame_position` even carries a `// ??? TODO ??? - The next frame is
/// the default image` note); this crate deliberately diverges rather than
/// reproduce a bug that makes files nothing can read.
#[test]
fn a_sub_canvas_first_animation_frame_is_rejected_not_silently_written() {
    let mut buf = Vec::new();
    let mut enc = oxiarc_png::Encoder::new(&mut buf, 8, 8);
    enc.set_color(ColorType::Rgba);
    enc.set_depth(BitDepth::Eight);
    enc.set_animated(2, 0).expect("animated");
    let mut w = enc.write_header().expect("header");
    w.set_frame_dimension(4, 4).expect("shrink");
    w.set_frame_position(2, 2).expect("move");
    let err = w
        .write_image_data(&Rng::new(5).bytes(4 * 4 * 4))
        .expect_err("a sub-canvas fcTL before IDAT must be refused");
    match err {
        EncodingError::Format(f) => assert!(
            matches!(f.kind(), EncodingFormatErrorKind::OutOfBounds),
            "expected OutOfBounds, got {f}"
        ),
        other => panic!("expected a format error, got {other:?}"),
    }
}

/// `set_sep_def_img` writes the default image with **no** `fcTL` at all, so
/// nothing in the file tells a decoder that the rectangle shrank: an
/// undersized default image just leaves the `IDAT` stream short of what
/// `IHDR` demands, and the decoder walks off the end of the data
/// (`InvalidRowFilter`) instead of reporting a bounds problem. That makes
/// this the *quieter* half of the same defect as
/// `a_sub_canvas_first_animation_frame_is_rejected_not_silently_written`,
/// and it is why the rule keys on "this frame becomes `IDAT`" rather than
/// on "an `fcTL` is being written".
///
/// The default image must therefore be the whole canvas; the animation
/// frames that follow it may be smaller, and this checks both halves.
#[test]
fn a_separate_default_image_must_still_cover_the_canvas() {
    let build = |def_w: u32, def_h: u32| -> Result<Vec<u8>, EncodingError> {
        let mut buf = Vec::new();
        let mut enc = oxiarc_png::Encoder::new(&mut buf, 8, 8);
        enc.set_color(ColorType::Rgba);
        enc.set_depth(BitDepth::Eight);
        enc.set_animated(1, 0)?;
        enc.set_sep_def_img(true)?;
        let mut w = enc.write_header()?;
        if (def_w, def_h) != (8, 8) {
            w.set_frame_dimension(def_w, def_h)?;
            w.set_frame_position(2, 2)?;
        }
        w.write_image_data(&Rng::new(6).bytes((def_w * def_h * 4) as usize))?;
        // The animation frame after it may be a sub-rectangle.
        w.reset_frame_position()?;
        w.set_frame_dimension(4, 4)?;
        w.set_frame_position(2, 2)?;
        w.write_image_data(&Rng::new(7).bytes(4 * 4 * 4))?;
        w.finish()?;
        Ok(buf)
    };

    let err = build(4, 4).expect_err("an undersized default image must be refused");
    match err {
        EncodingError::Format(f) => assert!(
            matches!(f.kind(), EncodingFormatErrorKind::OutOfBounds),
            "expected OutOfBounds, got {f}"
        ),
        other => panic!("expected a format error, got {other:?}"),
    }

    let buf = build(8, 8).expect("a full-canvas default image is legal");
    let mut dec = oxiarc_png::ApngDecoder::new(&buf[..]).expect("apng decoder");
    let frame = dec
        .next_composed()
        .expect("compose")
        .expect("one animation frame");
    assert_eq!((frame.width, frame.height), (8, 8));
    assert!(dec.next_composed().expect("end").is_none());
}

/// `StreamWriter` fed **one byte at a time** must produce exactly what
/// `write_image_data` produces for the same frame, on every legal
/// `(colour type, bit depth)` pair — including the sub-byte depths, where a
/// scanline's last byte is partially padded, and 16-bit, where one sample
/// spans two `write` calls.
///
/// Before this, `StreamWriter` was exercised only on 8-bit grayscale and
/// RGBA (three tests inside `src/encoder/stream.rs`) plus one compat-surface
/// call: the whole sub-byte and 16-bit half of the row buffer's arithmetic
/// had no streaming coverage at all.
#[test]
fn stream_writer_one_byte_at_a_time_matches_write_image_data_everywhere() {
    use std::io::Write;

    for (seed, &(color_type, bit_depth)) in (500_000u64..).zip(COMBINATIONS.iter()) {
        let (w, h) = (9u32, 5u32);
        let ihdr = Ihdr {
            width: w,
            height: h,
            bit_depth,
            color_type,
            interlace: Interlace::None,
        };
        let pixels = Rng::new(seed).bytes(ihdr.row_stride() * h as usize);
        let label = format!("{color_type:?}/{bit_depth:?}");

        let build = |stream: bool| -> Vec<u8> {
            let mut out = Vec::new();
            let mut enc = oxiarc_png::Encoder::new(&mut out, w, h);
            enc.set_color(color_type);
            enc.set_depth(bit_depth);
            if color_type == ColorType::Indexed {
                enc.set_palette(palette_256());
            }
            let mut writer = enc
                .write_header()
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            if stream {
                // Same flush granularity as `write_image_data`'s default, so
                // the two paths' chunk boundaries coincide and the bytes are
                // directly comparable (see `encoder::stream`'s module doc).
                let mut sw = writer
                    .stream_writer_with_size(64 * 1024)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                for byte in &pixels {
                    sw.write_all(&[*byte])
                        .unwrap_or_else(|e| panic!("{label}: {e}"));
                }
                sw.finish().unwrap_or_else(|e| panic!("{label}: {e}"));
            } else {
                writer
                    .write_image_data(&pixels)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
            }
            writer.finish().unwrap_or_else(|e| panic!("{label}: {e}"));
            out
        };

        let direct = build(false);
        let streamed = build(true);
        assert_eq!(direct, streamed, "{label}: streamed bytes differ");
        let decoded = oxiarc_png::decode(&streamed).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert_eq!(decoded.data, pixels, "{label}: pixels differ");
    }
}

/// `StreamWriter` on an **animation** frame: the second frame's payload has
/// to become `fdAT` chunks carrying the shared sequence counter, not `IDAT`.
/// Nothing tested this path before — every `StreamWriter` test wrote a
/// still image, so `push_complete_row`/`finish_mut`'s `ChunkKind::Fdat`
/// branch and its `frame_control.sequence_number` write-back were dead as
/// far as the test suite was concerned.
#[test]
fn stream_writer_writes_an_animation_frame_as_fdat() {
    use std::io::Write;

    let (w, h) = (8u32, 8u32);
    let mut out = Vec::new();
    let mut enc = oxiarc_png::Encoder::new(&mut out, w, h);
    enc.set_color(ColorType::Rgba);
    enc.set_depth(BitDepth::Eight);
    enc.set_animated(2, 0).expect("animated");
    let mut writer = enc.write_header().expect("header");

    let frame0 = Rng::new(11).bytes((w * h * 4) as usize);
    writer.write_image_data(&frame0).expect("frame 0");

    let frame1 = Rng::new(12).bytes((w * h * 4) as usize);
    {
        let mut sw = writer.stream_writer().expect("stream writer");
        for chunk in frame1.chunks(7) {
            sw.write_all(chunk).expect("write");
        }
        sw.finish().expect("finish stream");
    }
    writer.finish().expect("finish");

    let kinds: Vec<oxiarc_png::chunk::ChunkType> = oxiarc_png::chunk::ChunkIter::new(&out)
        .map(|c| c.expect("chunk").0)
        .collect();
    assert!(
        kinds.contains(&oxiarc_png::chunk::fdAT),
        "the streamed animation frame must be fdAT, got {kinds:?}"
    );

    // The sequence counter must be consecutive across fcTL and fdAT, which
    // is what `ApngDecoder` checks while composing.
    let mut dec = oxiarc_png::ApngDecoder::new(&out[..]).expect("apng decoder");
    for i in 0..2 {
        let frame = dec
            .next_composed()
            .unwrap_or_else(|e| panic!("frame {i}: {e}"))
            .unwrap_or_else(|| panic!("frame {i} missing"));
        assert_eq!((frame.width, frame.height), (w, h));
    }
    assert!(dec.next_composed().expect("end").is_none());
}

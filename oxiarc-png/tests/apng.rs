//! APNG control-chunk parsing and multi-frame decoding.

mod common;

use common::{ApngBuilder, PngBuilder, Rng, encode_fctl};
use oxiarc_png::chunk::{self, SIGNATURE, write_chunk};
use oxiarc_png::{BitDepth, BlendOp, ColorType, DisposeOp, FrameControl};

fn rgba(seed: u64, pixels: usize) -> Vec<u8> {
    Rng::new(seed).bytes(pixels * 4)
}

#[test]
fn a_two_frame_animation_parses_and_decodes() {
    let frame0 = rgba(1, 4 * 4);
    let frame1 = rgba(2, 2 * 2);
    let png = ApngBuilder::new(4, 4)
        .frame(0, 0, 4, 4, &frame0)
        .frame(1, 1, 2, 2, &frame1)
        .build();

    let info = oxiarc_png::peek_info(&png).expect("peek");
    let control = info.animation_control.expect("acTL");
    assert_eq!(control.num_frames, 2);
    assert!(info.is_animated());

    let mut reader = oxiarc_png::Decoder::new(&png[..])
        .read_info()
        .expect("read_info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    let out = reader.next_frame(&mut buf).expect("frame 0");
    assert_eq!((out.width, out.height), (4, 4));
    assert_eq!(buf, frame0);

    // The second frame is a 2x2 sub-frame carried in an fdAT chunk.
    let mut buf = vec![0u8; 2 * 2 * 4];
    let out = reader.next_frame(&mut buf).expect("frame 1");
    assert_eq!((out.width, out.height), (2, 2));
    assert_eq!(buf, frame1);
    let fctl = reader.info().frame_control.expect("fcTL");
    assert_eq!((fctl.x_offset, fctl.y_offset), (1, 1));
    assert_eq!(fctl.delay(), std::time::Duration::from_millis(100));
}

/// `Reader::finish_frame` probes one byte past the end of each frame's zlib
/// stream and, under `strict`, propagates whatever that probe reports. For an
/// animation that probe steps into the *next* frame's `fcTL`/`fdAT`, so strict
/// mode exercises a path a still image never reaches: a well-formed animation
/// must still decode cleanly.
#[test]
fn strict_mode_decodes_a_well_formed_animation() {
    let frame0 = rgba(1, 4 * 4);
    let frame1 = rgba(2, 2 * 2);
    let png = ApngBuilder::new(4, 4)
        .frame(0, 0, 4, 4, &frame0)
        .frame(1, 1, 2, 2, &frame1)
        .build();

    let mut options = oxiarc_png::DecodeOptions::default();
    options.set_strict(true);
    let mut reader = oxiarc_png::Decoder::new_with_options(&png[..], options)
        .read_info()
        .expect("strict read_info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("strict frame 0");
    assert_eq!(buf, frame0);
    let mut buf = vec![0u8; 2 * 2 * 4];
    reader.next_frame(&mut buf).expect("strict frame 1");
    assert_eq!(buf, frame1, "strict mode must not disturb the frame bytes");
    reader.finish().expect("strict finish");
}

/// Phase 8 decision 13: the inflater's own output cap is cleared at every
/// frame boundary, so a thousand-frame animation would otherwise get a
/// thousand times the per-stream budget. The file-level running total is what
/// closes that, and this checks it is actually wired into the decode path
/// rather than merely existing.
#[test]
fn the_animation_byte_budget_is_a_file_level_total() {
    let frame0 = rgba(1, 4 * 4);
    let frame1 = rgba(2, 2 * 2);
    let png = ApngBuilder::new(4, 4)
        .frame(0, 0, 4, 4, &frame0)
        .frame(1, 1, 2, 2, &frame1)
        .build();

    // Raw bytes: frame 0 is 4 rows of (1 filter + 16 samples) = 68, frame 1 is
    // 2 rows of (1 + 8) = 18. Neither frame alone reaches 86.
    let total = 4 * (1 + 4 * 4) + 2 * (1 + 2 * 4);
    assert_eq!(total, 86);

    let decode_frames = |budget: u64| -> Result<(), oxiarc_png::DecodingError> {
        let mut options = oxiarc_png::DecodeOptions::default();
        options.set_limits(oxiarc_png::DecodeLimits::default().with_max_total_frame_bytes(budget));
        let mut reader = oxiarc_png::Decoder::new_with_options(&png[..], options).read_info()?;
        let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
        reader.next_frame(&mut buf)?;
        let mut buf = vec![0u8; 2 * 2 * 4];
        reader.next_frame(&mut buf)?;
        Ok(())
    };

    // A budget that covers frame 0 but not both frames stops at frame 1, which
    // proves the total survives the per-frame inflater reset.
    assert!(
        matches!(
            decode_frames(total - 1),
            Err(oxiarc_png::DecodingError::LimitsExceeded)
        ),
        "the second frame must be charged to the same budget as the first"
    );
    // The inverse, so the test cannot pass by firing spuriously.
    decode_frames(total).expect("a budget covering both frames decodes both");
}

#[test]
fn a_sequence_gap_is_rejected() {
    let frame0 = rgba(1, 4 * 4);
    let frame1 = rgba(2, 2 * 2);
    let png = ApngBuilder::new(4, 4)
        .frame(0, 0, 4, 4, &frame0)
        .frame(1, 1, 2, 2, &frame1)
        // fcTL 0, fcTL 1, fdAT 2 becomes fcTL 0, fcTL 1, fdAT 3.
        .sequence_override(vec![0, 1, 3])
        .build();
    let mut reader = oxiarc_png::Decoder::new(&png[..])
        .read_info()
        .expect("read_info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("frame 0");
    let mut buf = vec![0u8; 2 * 2 * 4];
    assert!(reader.next_frame(&mut buf).is_err(), "sequence gap");
}

#[test]
fn an_fdat_before_any_fctl_is_rejected() {
    let samples = rgba(3, 2 * 2);
    let raw = {
        let mut raw = Vec::new();
        for y in 0..2 {
            raw.push(0u8);
            raw.extend_from_slice(&samples[y * 8..(y + 1) * 8]);
        }
        raw
    };
    let compressed = oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib");
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 2;
    ihdr[7] = 2;
    ihdr[8] = 8;
    ihdr[9] = ColorType::Rgba as u8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    let mut actl = Vec::new();
    actl.extend_from_slice(&2u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    write_chunk(&mut png, chunk::acTL, &actl).expect("actl");
    write_chunk(&mut png, chunk::IDAT, &compressed).expect("idat");
    let mut fdat = 0u32.to_be_bytes().to_vec();
    fdat.extend_from_slice(&compressed);
    write_chunk(&mut png, chunk::fdAT, &fdat).expect("fdat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");

    let mut reader = oxiarc_png::Decoder::new(&png[..])
        .read_info()
        .expect("read_info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("frame 0");
    assert!(reader.next_frame(&mut buf).is_err(), "fdAT without fcTL");
}

#[test]
fn an_out_of_bounds_sub_frame_is_rejected() {
    let frame0 = rgba(1, 4 * 4);
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 4;
    ihdr[7] = 4;
    ihdr[8] = 8;
    ihdr[9] = ColorType::Rgba as u8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    let mut actl = Vec::new();
    actl.extend_from_slice(&2u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    write_chunk(&mut png, chunk::acTL, &actl).expect("actl");
    let mut raw = Vec::new();
    for y in 0..4 {
        raw.push(0u8);
        raw.extend_from_slice(&frame0[y * 16..(y + 1) * 16]);
    }
    let compressed = oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib");
    write_chunk(&mut png, chunk::IDAT, &compressed).expect("idat");
    let bad = FrameControl {
        sequence_number: 0,
        width: 4,
        height: 4,
        x_offset: 2,
        y_offset: 0,
        delay_num: 1,
        delay_den: 10,
        dispose_op: DisposeOp::None,
        blend_op: BlendOp::Source,
    };
    write_chunk(&mut png, chunk::fcTL, &encode_fctl(&bad)).expect("fctl");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");

    let mut reader = oxiarc_png::Decoder::new(&png[..])
        .read_info()
        .expect("read_info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
    reader.next_frame(&mut buf).expect("frame 0");
    assert!(
        reader.next_frame(&mut buf).is_err(),
        "sub-frame out of bounds"
    );
}

#[test]
fn an_fctl_before_idat_must_cover_the_whole_canvas() {
    let frame0 = rgba(1, 4 * 4);
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 4;
    ihdr[7] = 4;
    ihdr[8] = 8;
    ihdr[9] = ColorType::Rgba as u8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    let mut actl = Vec::new();
    actl.extend_from_slice(&1u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    write_chunk(&mut png, chunk::acTL, &actl).expect("actl");
    let bad = FrameControl {
        sequence_number: 0,
        width: 2,
        height: 2,
        x_offset: 0,
        y_offset: 0,
        delay_num: 1,
        delay_den: 10,
        dispose_op: DisposeOp::None,
        blend_op: BlendOp::Source,
    };
    write_chunk(&mut png, chunk::fcTL, &encode_fctl(&bad)).expect("fctl");
    let mut raw = Vec::new();
    for y in 0..4 {
        raw.push(0u8);
        raw.extend_from_slice(&frame0[y * 16..(y + 1) * 16]);
    }
    write_chunk(
        &mut png,
        chunk::IDAT,
        &oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib"),
    )
    .expect("idat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");
    assert!(oxiarc_png::decode(&png).is_err());
}

#[test]
fn zero_frames_is_rejected() {
    let samples = vec![0u8; 4];
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 1;
    ihdr[7] = 1;
    ihdr[8] = 8;
    ihdr[9] = ColorType::Rgba as u8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    write_chunk(&mut png, chunk::acTL, &[0, 0, 0, 0, 0, 0, 0, 0]).expect("actl");
    let mut raw = vec![0u8];
    raw.extend_from_slice(&samples);
    write_chunk(
        &mut png,
        chunk::IDAT,
        &oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib"),
    )
    .expect("idat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");
    assert!(oxiarc_png::decode(&png).is_err());
}

#[test]
fn a_still_image_reports_no_animation() {
    let png = PngBuilder::new(2, 2, ColorType::Grayscale, BitDepth::Eight)
        .build_from_samples(&[1, 2, 3, 4]);
    let mut reader = oxiarc_png::Decoder::new(&png[..])
        .read_info()
        .expect("read_info");
    assert!(!reader.info().is_animated());
    let mut buf = vec![0u8; 4];
    reader.next_frame(&mut buf).expect("frame");
    assert!(reader.next_frame_info().is_err(), "not an animation");
}

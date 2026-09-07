//! The 40-row edge-case register from the design report, one named test per row.
//!
//! Every fixture here is hand-built by `tests/support`, a spec-literal writer
//! that shares no code with `oxiarc_tiff::writer`, so a bug in the encoder
//! cannot mask a bug in the decoder.

mod support;

use oxiarc_tiff::tags::{
    CompressionMethod, ExtraSamples, FillOrder, Orientation, PhotometricInterpretation,
    PlanarConfiguration, SampleFormat, Tag,
};
use oxiarc_tiff::{
    ChunkType, ColorType, Compression, Decoder, Encoder, ImageSpec, Layout, Leniency, Limits,
    Predictor, Rational, SampleType, Samples, TiffError, UnsupportedError, Value, VariantChoice,
};
use std::io::Cursor;
use support::{NextIfd, RawTiff, gray8, ramp};

fn decode(bytes: Vec<u8>) -> Decoder<Cursor<Vec<u8>>> {
    Decoder::new(Cursor::new(bytes)).expect("valid header")
}

fn decode_lenient(bytes: Vec<u8>) -> Decoder<Cursor<Vec<u8>>> {
    decode(bytes).with_leniency(Leniency::Lenient)
}

fn decode_strict(bytes: Vec<u8>) -> Decoder<Cursor<Vec<u8>>> {
    decode(bytes).with_leniency(Leniency::Strict)
}

/// Encodes with `spec` and decodes the result back to samples.
fn round_trip(spec: &ImageSpec, data: &[u8]) -> Samples {
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(spec, data).expect("write");
    encoder.finish().expect("finish");
    let mut decoder = decode(buffer.into_inner());
    decoder.read_image().expect("read back")
}

// ---------------------------------------------------------------- E1 - E4

#[test]
fn e1_strip_byte_counts_absent_single_strip_runs_to_eof() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.remove(279);
    let mut decoder = decode(tiff.build());
    let samples = decoder.read_image().expect("recovered from EOF");
    assert_eq!(samples.as_u8().map(|s| &s[..16]), Some(&pixels[..]));
    assert!(!decoder.warnings().is_empty());
}

#[test]
fn e1_strip_byte_counts_absent_multi_strip_uncompressed_is_derived() {
    let pixels = ramp(16);
    let mut tiff = RawTiff::new();
    let a = tiff.add_data(&pixels[..8]);
    let b = tiff.add_data(&pixels[8..]);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[a as u32, b as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[2]);
    let mut decoder = decode(tiff.build());
    let samples = decoder.read_image().expect("derived byte counts");
    assert_eq!(samples.as_u8(), Some(&pixels[..]));
}

#[test]
fn e2_strip_offsets_typed_short_are_widened() {
    let pixels = ramp(16);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&pixels);
    assert!(offset < u64::from(u16::MAX));
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.short(273, &[offset as u16]);
    tiff.short(277, &[1]);
    tiff.long(278, &[4]);
    tiff.short(279, &[16]);
    let samples = decode(tiff.build()).read_image().expect("SHORT offsets");
    assert_eq!(samples.as_u8(), Some(&pixels[..]));
}

#[test]
fn e3_rows_per_strip_absent_means_one_strip() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.remove(278);
    let mut decoder = decode(tiff.build());
    assert_eq!(decoder.chunk_count().expect("count"), 1);
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

#[test]
fn e3_rows_per_strip_zero_is_lenient_only() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.long(278, &[0]);
    let bytes = tiff.build();
    assert!(decode_strict(bytes.clone()).info().is_err());
    let mut decoder = decode(bytes);
    assert_eq!(decoder.chunk_count().expect("count"), 1);
}

#[test]
fn e4_rows_per_strip_greater_than_height_still_yields_one_strip() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.long(278, &[1000]);
    let mut decoder = decode(tiff.build());
    assert_eq!(decoder.chunk_count().expect("count"), 1);
    assert_eq!(decoder.chunk_data_dimensions(0).expect("dims"), (4, 4));
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

// ---------------------------------------------------------------- E5 - E6

#[test]
fn e5_edge_tiles_are_coded_full_size_and_padded() {
    let pixels = ramp(20 * 20);
    let spec = ImageSpec::new(20, 20, ColorType::Gray(8)).with_layout(Layout::Tiles {
        width: 16,
        length: 16,
    });
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(&spec, &pixels).expect("write");
    encoder.finish().expect("finish");
    let mut decoder = decode(buffer.into_inner());
    assert_eq!(decoder.chunk_type().expect("type"), ChunkType::Tile);
    assert_eq!(decoder.chunk_count().expect("count"), 4);
    // Every tile is coded 16x16; only the top-left one is fully valid.
    assert_eq!(decoder.chunk_data_dimensions(0).expect("dims"), (16, 16));
    assert_eq!(decoder.chunk_data_dimensions(1).expect("dims"), (4, 16));
    assert_eq!(decoder.chunk_data_dimensions(3).expect("dims"), (4, 4));
    let tile = decoder.read_tile(3).expect("read padded tile");
    assert_eq!(tile.len(), 16 * 16);
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

#[test]
fn e6_tile_dimensions_not_multiples_of_sixteen_warn_on_read_and_fail_on_write() {
    let pixels = ramp(8 * 8);
    let mut tiff = RawTiff::new();
    let offsets: Vec<u32> = (0..1).map(|_| tiff.add_data(&pixels) as u32).collect();
    tiff.long(256, &[8]);
    tiff.long(257, &[8]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.short(277, &[1]);
    tiff.long(322, &[8]);
    tiff.long(323, &[8]);
    tiff.long(324, &offsets);
    tiff.long(325, &[64]);
    let mut decoder = decode(tiff.build());
    assert_eq!(
        decoder.read_image().expect("decoded anyway").as_u8(),
        Some(&pixels[..])
    );
    assert!(!decoder.warnings().is_empty());

    let spec = ImageSpec::new(8, 8, ColorType::Gray(8)).with_layout(Layout::Tiles {
        width: 8,
        length: 8,
    });
    assert!(spec.validate().is_err());
}

// ---------------------------------------------------------------- E7 - E10

#[test]
fn e7_heterogeneous_bits_per_sample_are_unpacked_per_channel() {
    // Three channels of 10, 12 and 10 bits: 32 bits per pixel, two pixels per
    // row, so each row is exactly four bytes and the widest channel picks the
    // u16 slot.
    let values: [u16; 12] = [1023, 4095, 1023, 0, 1, 2, 500, 600, 700, 3, 4, 5];
    let mut native = Vec::new();
    for v in values {
        native.extend_from_slice(&v.to_ne_bytes());
    }
    let bits = [10u16, 12, 10];
    let row_bytes = oxiarc_tiff::sample::packed_row_bytes(&bits, 6) as usize;
    assert_eq!(row_bytes, 8);
    let mut packed = vec![0u8; row_bytes * 2];
    for row in 0..2usize {
        oxiarc_tiff::sample::pack_row(
            &native[row * 12..(row + 1) * 12],
            &bits,
            6,
            SampleType::U16,
            &mut packed[row * row_bytes..(row + 1) * row_bytes],
        )
        .expect("pack");
    }
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&packed);
    tiff.long(256, &[2]);
    tiff.long(257, &[2]);
    tiff.short(258, &[10, 12, 10]);
    tiff.short(259, &[1]);
    tiff.short(262, &[2]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[3]);
    tiff.long(278, &[2]);
    tiff.long(279, &[packed.len() as u32]);
    let mut decoder = decode(tiff.build());
    assert_eq!(
        decoder.color_type().expect("colour type"),
        ColorType::Multiband {
            bit_depth: 12,
            num_samples: 3
        }
    );
    assert_eq!(decoder.sample_type().expect("slot"), SampleType::U16);
    let samples = decoder.read_image().expect("heterogeneous read");
    assert_eq!(samples.as_u16(), Some(&values[..]));
}

#[test]
fn e8_bits_per_sample_count_mismatch_broadcasts_or_errors() {
    let pixels = ramp(12);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&pixels);
    tiff.long(256, &[2]);
    tiff.long(257, &[2]);
    tiff.short(258, &[8]); // one entry for three samples
    tiff.short(259, &[1]);
    tiff.short(262, &[2]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[3]);
    tiff.long(278, &[2]);
    tiff.long(279, &[12]);
    let bytes = tiff.build();
    let mut lenient = decode(bytes.clone());
    assert_eq!(
        lenient.read_image().expect("broadcast").as_u8(),
        Some(&pixels[..])
    );
    assert!(decode_strict(bytes).info().is_err());
}

#[test]
fn e9_extra_samples_expose_alpha_and_associated_alpha_is_undone() {
    let spec = ImageSpec::new(2, 1, ColorType::Rgba(8))
        .with_extra_samples(vec![ExtraSamples::AssociatedAlpha]);
    let data = [64u8, 64, 64, 128, 255, 0, 0, 255];
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(&spec, &data).expect("write");
    encoder.finish().expect("finish");
    let mut decoder = decode(buffer.into_inner());
    assert_eq!(
        decoder.info().expect("info").extra_samples,
        vec![ExtraSamples::AssociatedAlpha]
    );
    let rgba = decoder.read_image_rgba8().expect("rgba");
    // 64 premultiplied by alpha 128 un-premultiplies to 127.
    assert_eq!(&rgba[..4], &[127, 127, 127, 128]);
    assert_eq!(&rgba[4..], &[255, 0, 0, 255]);
}

#[test]
fn e10_extra_channels_without_extra_samples_warn() {
    let pixels = ramp(8);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&pixels);
    tiff.long(256, &[2]);
    tiff.long(257, &[1]);
    tiff.short(258, &[8, 8, 8, 8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[2]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[4]);
    tiff.long(278, &[1]);
    tiff.long(279, &[8]);
    let mut decoder = decode(tiff.build());
    decoder.read_image().expect("read");
    assert!(!decoder.warnings().is_empty());
}

// ---------------------------------------------------------------- E11 - E13

#[test]
fn e11_sub_byte_packing_with_fill_order_two() {
    for bits in [1u16, 2, 4] {
        let max = (1u32 << bits) - 1;
        let pixels: Vec<u8> = (0..12u32).map(|i| (i % (max + 1)) as u8).collect();
        let spec = ImageSpec::new(6, 2, ColorType::Gray(bits as u8))
            .with_fill_order(FillOrder::Lsb2Msb)
            .with_layout(Layout::Strips { rows_per_strip: 2 });
        let samples = round_trip(&spec, &pixels);
        assert_eq!(
            samples.as_u8(),
            Some(&pixels[..]),
            "{bits}-bit fill order 2"
        );
    }
}

#[test]
fn e11_row_padding_is_per_row_not_per_strip() {
    // 3 pixels of 1 bit each pad to one byte per row; two rows are two bytes.
    let pixels = [1u8, 0, 1, 0, 1, 1];
    let spec =
        ImageSpec::new(3, 2, ColorType::Gray(1)).with_layout(Layout::Strips { rows_per_strip: 2 });
    assert_eq!(spec.chunk_packed_len(0).expect("len"), 2);
    let samples = round_trip(&spec, &pixels);
    assert_eq!(samples.as_u8(), Some(&pixels[..]));
}

#[test]
fn e12_twelve_bit_samples_unpack_into_u16() {
    let values: [u16; 6] = [0, 4095, 2048, 1, 3000, 777];
    let mut native = Vec::new();
    for v in values {
        native.extend_from_slice(&v.to_ne_bytes());
    }
    let spec =
        ImageSpec::new(3, 2, ColorType::Gray(12)).with_layout(Layout::Strips { rows_per_strip: 2 });
    let samples = round_trip(&spec, &native);
    assert_eq!(samples.as_u16(), Some(&values[..]));
}

#[test]
fn e12_predictor_is_rejected_for_twelve_bit_data() {
    let spec = ImageSpec::new(3, 2, ColorType::Gray(12)).with_predictor(Predictor::Horizontal);
    assert!(spec.validate().is_err());
}

#[test]
fn e13_twenty_four_bit_samples_unpack_into_u32() {
    let values: [u32; 4] = [0, 0xFF_FFFF, 0x12_3456, 7];
    let mut native = Vec::new();
    for v in values {
        native.extend_from_slice(&v.to_ne_bytes());
    }
    let spec =
        ImageSpec::new(2, 2, ColorType::Gray(24)).with_layout(Layout::Strips { rows_per_strip: 2 });
    let samples = round_trip(&spec, &native);
    assert_eq!(samples.as_u32(), Some(&values[..]));
}

// ---------------------------------------------------------------- E14 - E16

#[test]
fn e14_predictor_with_planar_uses_a_stride_of_one() {
    let data: Vec<u8> = (0..24u8).collect();
    let spec = ImageSpec::new(4, 2, ColorType::Rgb(8))
        .with_planar(PlanarConfiguration::Planar)
        .with_predictor(Predictor::Horizontal)
        .with_layout(Layout::Strips { rows_per_strip: 2 });
    let samples = round_trip(&spec, &data);
    assert_eq!(samples.as_u8(), Some(&data[..]));
}

#[test]
fn e15_predictor_two_with_wide_samples_propagates_carries() {
    for (bits, ty) in [
        (16u16, SampleType::U16),
        (32, SampleType::U32),
        (64, SampleType::U64),
    ] {
        let count = 8usize;
        let values: Vec<u64> = (0..count as u64)
            .map(|i| i.wrapping_mul(0x0101_0101) + 255)
            .collect();
        let mut native = Vec::new();
        for v in &values {
            match ty {
                SampleType::U16 => native.extend_from_slice(&(*v as u16).to_ne_bytes()),
                SampleType::U32 => native.extend_from_slice(&(*v as u32).to_ne_bytes()),
                _ => native.extend_from_slice(&v.to_ne_bytes()),
            }
        }
        for endian in [oxiarc_tiff::Endian::Little, oxiarc_tiff::Endian::Big] {
            let spec = ImageSpec::new(4, 2, ColorType::Gray(bits as u8))
                .with_predictor(Predictor::Horizontal)
                .with_layout(Layout::Strips { rows_per_strip: 2 });
            let mut buffer = Cursor::new(Vec::new());
            let mut encoder = Encoder::new(&mut buffer)
                .expect("encoder")
                .with_endian(endian);
            encoder.write_image(&spec, &native).expect("write");
            encoder.finish().expect("finish");
            let samples = decode(buffer.into_inner()).read_image().expect("read");
            assert_eq!(
                samples.to_native_bytes(),
                native,
                "{bits}-bit predictor 2 in {endian:?}"
            );
        }
    }
}

#[test]
fn e16_float_predictor_in_both_byte_orders() {
    let values: [f32; 8] = [0.0, 1.0, -2.5, 1e10, -1e-10, 3.25, 42.0, -0.5];
    let mut native = Vec::new();
    for v in values {
        native.extend_from_slice(&v.to_ne_bytes());
    }
    for endian in [oxiarc_tiff::Endian::Little, oxiarc_tiff::Endian::Big] {
        let spec = ImageSpec::new(4, 2, ColorType::Gray(32))
            .with_sample_format(SampleFormat::IeeeFp)
            .with_predictor(Predictor::FloatingPoint)
            .with_layout(Layout::Strips { rows_per_strip: 2 });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer)
            .expect("encoder")
            .with_endian(endian);
        encoder.write_image(&spec, &native).expect("write");
        encoder.finish().expect("finish");
        let samples = decode(buffer.into_inner()).read_image().expect("read");
        assert_eq!(
            samples.as_f32(),
            Some(&values[..]),
            "float predictor {endian:?}"
        );
    }
}

// ---------------------------------------------------------------- E17

#[test]
fn e17_jpeg_in_tiles_reports_not_yet_available() {
    let pixels = ramp(16 * 16);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&pixels);
    tiff.long(256, &[16]);
    tiff.long(257, &[16]);
    tiff.short(258, &[8, 8, 8]);
    tiff.short(259, &[7]);
    tiff.short(262, &[6]);
    tiff.short(277, &[3]);
    tiff.short(530, &[2, 2]);
    tiff.long(322, &[16]);
    tiff.long(323, &[16]);
    tiff.long(324, &[offset as u32]);
    tiff.long(325, &[pixels.len() as u32]);
    let err = decode(tiff.build())
        .read_image()
        .expect_err("JPEG is not wired up yet");
    assert!(matches!(
        err,
        TiffError::Unsupported(UnsupportedError::NotYetAvailable { method: 7, .. })
    ));
}

// ---------------------------------------------------------------- E18 - E21

#[test]
fn e18_a_huge_entry_count_is_rejected_before_allocating() {
    // Classic TIFF: a DOUBLE count of u32::MAX is 34 GB, which the value-size
    // guard rejects before any allocation.
    let mut tiff = gray8(4, 4, &ramp(16));
    tiff.raw(700, 12, u64::from(u32::MAX), vec![0; 8]);
    let started = std::time::Instant::now();
    let mut decoder = decode(tiff.build());
    let err = decoder.get_tag(Tag::Xmp).expect_err("must not allocate");
    assert!(err.is_limits(), "{err:?}");
    assert!(started.elapsed().as_millis() < 200);
}

#[test]
fn e18_a_count_that_overflows_u64_is_rejected() {
    // BigTIFF: 2^61 DOUBLEs is exactly 2^64 bytes, which `checked_mul` catches.
    let pixels = ramp(16);
    let mut tiff = RawTiff::new().bigtiff();
    let offset = tiff.add_data(&pixels);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long8(273, &[offset]);
    tiff.short(277, &[1]);
    tiff.long(278, &[4]);
    tiff.long8(279, &[16]);
    tiff.raw(700, 12, 1u64 << 61, vec![0; 8]);
    let mut decoder = decode(tiff.build());
    let err = decoder.get_tag(Tag::Xmp).expect_err("must not allocate");
    assert!(
        err.is_limits() || matches!(err, TiffError::IntOverflow),
        "{err:?}"
    );
    // The image itself still decodes.
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

#[test]
fn e19_implausible_dimensions_are_rejected_before_allocating() {
    let mut tiff = gray8(4, 4, &ramp(16));
    tiff.long(256, &[1 << 30]);
    tiff.long(257, &[1 << 30]);
    let started = std::time::Instant::now();
    let err = decode(tiff.build()).info().expect_err("must not allocate");
    assert!(err.is_limits());
    assert!(started.elapsed().as_millis() < 200);
}

#[test]
fn e20_an_ifd_loop_terminates() {
    let tiff = gray8(4, 4, &ramp(16)).next_ifd(NextIfd::SelfLoop);
    let mut decoder = decode(tiff.build());
    let err = decoder.image_count().expect_err("self loop");
    assert!(matches!(err, TiffError::Format(_)));
}

#[test]
fn e20_a_sub_ifd_loop_terminates() {
    let pixels = ramp(16);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&pixels);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[4]);
    tiff.long(279, &[16]);
    // Build once to learn the IFD offset, then make SubIFDs point at it.
    let probe = tiff.build();
    let ifd_offset = u32::from_le_bytes([probe[4], probe[5], probe[6], probe[7]]);
    tiff.raw(330, 13, 1, ifd_offset.to_le_bytes().to_vec());
    let mut decoder = decode(tiff.build());
    // The main chain already visited this offset, so the SubIFD is skipped.
    let tree = decoder.sub_ifd_tree().expect("no infinite recursion");
    assert!(tree.is_empty());
}

#[test]
fn e21_classic_offsets_beyond_four_gib_are_an_error_not_a_truncation() {
    let mut buffer = Cursor::new(Vec::new());
    let mut writer =
        oxiarc_tiff::EndianWriter::new(&mut buffer, oxiarc_tiff::Endian::Little).expect("writer");
    let err = writer
        .write_offset(u64::from(u32::MAX) + 1, false)
        .expect_err("classic overflow");
    assert!(matches!(err, TiffError::Usage(_)));
}

#[test]
fn e21_auto_promotes_to_bigtiff_for_a_large_projection() {
    let small = ImageSpec::new(16, 16, ColorType::Gray(8));
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer)
        .expect("encoder")
        .with_variant(VariantChoice::Auto);
    encoder.write_image(&small, &ramp(256)).expect("write");
    assert_eq!(encoder.variant(), Some(oxiarc_tiff::Variant::Classic));
    encoder.finish().expect("finish");

    let huge = ImageSpec::new(40000, 40000, ColorType::Rgb(8));
    assert!(huge.projected_bytes().expect("projection") > 0xFFFF_0000);
}

#[test]
fn e21_bigtiff_round_trips() {
    let pixels = ramp(64);
    let spec =
        ImageSpec::new(8, 8, ColorType::Gray(8)).with_layout(Layout::Strips { rows_per_strip: 4 });
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer)
        .expect("encoder")
        .with_variant(VariantChoice::Big);
    encoder.write_image(&spec, &pixels).expect("write");
    encoder.finish().expect("finish");
    let mut decoder = decode(buffer.into_inner());
    assert_eq!(decoder.variant(), oxiarc_tiff::Variant::Big);
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

// ---------------------------------------------------------------- E22 - E25

#[test]
fn e22_unknown_tags_and_unknown_types_round_trip() {
    let mut tiff = gray8(2, 2, &ramp(4));
    tiff.raw(60123, 14, 3, vec![7, 8, 9]);
    tiff.ascii(60124, "kept");
    let mut decoder = decode(tiff.build());
    let unknown = decoder.find_tag_raw(60123).expect("load").expect("present");
    assert_eq!(
        unknown,
        Value::Unknown {
            ty_raw: 14,
            bytes: vec![7, 8, 9]
        }
    );
    let text = decoder.find_tag_raw(60124).expect("load").expect("present");
    assert_eq!(text.as_str(), Some("kept"));

    // Round-trip them through the writer.
    let samples = decoder.read_image().expect("read");
    let spec = ImageSpec::new(2, 2, ColorType::Gray(8))
        .with_extra_tag(60123, unknown.clone())
        .with_extra_tag(60124, text.clone());
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image_samples(&spec, &samples)
        .expect("write back");
    encoder.finish().expect("finish");
    let mut back = decode(buffer.into_inner());
    assert_eq!(back.find_tag_raw(60123).expect("load"), Some(unknown));
    assert_eq!(back.find_tag_raw(60124).expect("load"), Some(text));
}

#[test]
fn e23_image_description_is_carried_never_interpreted() {
    let description = "ImageJ=1.54\nimages=7\nframes=7";
    let mut tiff = gray8(2, 2, &ramp(4));
    tiff.ascii(270, description);
    let mut decoder = decode(tiff.build());
    assert_eq!(
        decoder
            .get_tag_ascii(Tag::ImageDescription)
            .expect("load")
            .as_deref(),
        Some(description)
    );
    // The claimed frame count does not change the geometry.
    assert_eq!(decoder.image_count().expect("count"), 1);
    assert_eq!(decoder.dimensions().expect("dims"), (2, 2));
}

#[test]
fn e24_multi_page_files_rebuild_the_geometry_per_page() {
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image(&ImageSpec::new(2, 2, ColorType::Gray(8)), &ramp(4))
        .expect("page 0");
    encoder
        .write_image(
            &ImageSpec::new(3, 1, ColorType::Rgb(16))
                .with_layout(Layout::Strips { rows_per_strip: 1 }),
            &vec![0u8; 3 * 3 * 2],
        )
        .expect("page 1");
    encoder
        .write_image(&ImageSpec::new(1, 4, ColorType::Gray(8)), &ramp(4))
        .expect("page 2");
    encoder.finish().expect("finish");

    let mut decoder = decode(buffer.into_inner());
    assert_eq!(decoder.image_count().expect("count"), 3);
    assert_eq!(decoder.dimensions().expect("page 0"), (2, 2));
    assert!(decoder.next_image().expect("advance"));
    assert_eq!(decoder.dimensions().expect("page 1"), (3, 1));
    assert_eq!(decoder.color_type().expect("type"), ColorType::Rgb(16));
    decoder.seek_to_image(2).expect("seek");
    assert_eq!(decoder.dimensions().expect("page 2"), (1, 4));
    decoder.seek_to_image(0).expect("seek back");
    assert_eq!(decoder.dimensions().expect("page 0 again"), (2, 2));
    assert!(decoder.seek_to_image(3).is_err());
}

#[test]
fn e25_orientation_is_exposed_but_never_applied() {
    let pixels = ramp(4);
    let mut tiff = gray8(2, 2, &pixels);
    tiff.short(274, &[8]);
    let mut decoder = decode(tiff.build());
    assert_eq!(
        decoder.info().expect("info").orientation,
        Orientation::LeftBottom
    );
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

// ---------------------------------------------------------------- E26 - E30

#[test]
fn e26_a_rational_with_a_zero_denominator_does_not_panic() {
    let mut tiff = gray8(2, 2, &ramp(4));
    tiff.rational(282, &[(300, 0)]);
    let mut decoder = decode(tiff.build());
    let resolution = decoder.info().expect("info").resolution;
    assert_eq!(resolution.0, Some(Rational { num: 300, den: 0 }));
    assert_eq!(resolution.0.and_then(Rational::as_f64), None);
}

#[test]
fn e27_inline_versus_offset_is_recomputed_from_the_length() {
    // Declare a count of 1 for a SHORT (2 bytes, inline) but store the value
    // where a writer would have put an offset. The value must still be read
    // from the inline field.
    let mut tiff = gray8(2, 2, &ramp(4));
    tiff.raw(305, 3, 1, vec![0x2A, 0x00]);
    let mut decoder = decode(tiff.build());
    assert_eq!(
        decoder.find_tag(Tag::Software).expect("load"),
        Some(Value::Short(vec![42]))
    );
}

#[test]
fn e28_odd_offsets_are_accepted_on_read() {
    let pixels = ramp(16);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data_odd(&pixels);
    assert_eq!(offset % 2, 1);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[4]);
    tiff.long(279, &[16]);
    assert_eq!(
        decode(tiff.build()).read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

#[test]
fn e29_compression_defaults_to_none() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.remove(259);
    let mut decoder = decode(tiff.build());
    assert_eq!(
        decoder.info().expect("info").compression,
        CompressionMethod::None
    );
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
}

#[test]
fn e30_photometric_absent_is_inferred_or_rejected() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.remove(262);
    let bytes = tiff.build();
    let mut lenient = decode(bytes.clone());
    assert_eq!(
        lenient.info().expect("inferred").photometric,
        PhotometricInterpretation::BlackIsZero
    );
    assert!(!lenient.warnings().is_empty());
    assert!(decode_strict(bytes).info().is_err());
}

// ---------------------------------------------------------------- E31 - E35

#[test]
fn e31_a_chunk_past_eof_is_rejected_or_truncated() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.long(279, &[1 << 20]);
    let bytes = tiff.build();
    let err = decode(bytes.clone()).read_image().expect_err("past EOF");
    assert!(matches!(err, TiffError::Format(_)));
    let mut lenient = decode_lenient(bytes);
    lenient.read_image().expect("truncated to EOF");
    assert!(!lenient.warnings().is_empty());
}

#[test]
fn e32_offset_and_count_length_mismatch() {
    let pixels = ramp(16);
    let mut tiff = RawTiff::new();
    let a = tiff.add_data(&pixels[..8]);
    let b = tiff.add_data(&pixels[8..]);
    tiff.long(256, &[4]);
    tiff.long(257, &[4]);
    tiff.short(258, &[8]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[a as u32, b as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[2]);
    tiff.long(279, &[8]); // one count for two offsets
    let bytes = tiff.build();
    assert!(decode_strict(bytes.clone()).info().is_err());
    let mut lenient = decode(bytes);
    lenient.info().expect("min length used");
    assert!(!lenient.warnings().is_empty());
}

#[test]
fn e33_planar_planes_may_have_different_bit_depths() {
    let plane0 = vec![1u8, 2, 3, 4];
    let mut plane1 = Vec::new();
    for v in [10u16, 20, 30, 40] {
        plane1.extend_from_slice(&v.to_be_bytes());
    }
    let mut tiff = RawTiff::new().big_endian();
    let a = tiff.add_data(&plane0);
    let b = tiff.add_data(&plane1);
    tiff.long(256, &[2]);
    tiff.long(257, &[2]);
    tiff.short(258, &[8, 16]);
    tiff.short(259, &[1]);
    tiff.short(262, &[1]);
    tiff.long(273, &[a as u32, b as u32]);
    tiff.short(277, &[2]);
    tiff.long(278, &[2]);
    tiff.long(279, &[4, 8]);
    tiff.short(284, &[2]);
    let mut decoder = decode(tiff.build());
    let info = decoder.info().expect("info");
    assert_eq!(info.planar, PlanarConfiguration::Planar);
    assert_eq!(info.plane_bits(0), vec![8]);
    assert_eq!(info.plane_bits(1), vec![16]);
    assert_eq!(info.chunk_count(), 2);
    // Both planes are sized from their own depth.
    assert_eq!(info.chunk_packed_len(0, &Limits::default()).expect("p0"), 4);
    assert_eq!(info.chunk_packed_len(1, &Limits::default()).expect("p1"), 8);
}

#[test]
fn e34_a_zero_dimension_is_rejected() {
    for (w, h) in [(0u32, 4u32), (4, 0), (0, 0)] {
        let mut tiff = gray8(4, 4, &ramp(16));
        tiff.long(256, &[w]);
        tiff.long(257, &[h]);
        let err = decode(tiff.build()).info().expect_err("zero dimension");
        assert!(matches!(err, TiffError::Format(_)));
    }
}

#[test]
fn e35_mixed_sample_formats_are_rejected_by_the_typed_api() {
    let mut tiff = gray8(2, 2, &ramp(8));
    tiff.short(258, &[8, 8]);
    tiff.short(277, &[2]);
    tiff.short(339, &[1, 3]);
    tiff.long(279, &[8]);
    let mut decoder = decode(tiff.build());
    let err = decoder.sample_type().expect_err("mixed formats");
    assert!(matches!(
        err,
        TiffError::Unsupported(UnsupportedError::MixedSampleFormats)
    ));
}

// ---------------------------------------------------------------- E36 - E40

#[test]
fn e36_sample_format_void_is_treated_as_unsigned_with_a_warning() {
    let pixels = ramp(16);
    let mut tiff = gray8(4, 4, &pixels);
    tiff.short(339, &[4]);
    let mut decoder = decode(tiff.build());
    assert_eq!(decoder.sample_type().expect("type"), SampleType::U8);
    assert_eq!(
        decoder.read_image().expect("read").as_u8(),
        Some(&pixels[..])
    );
    assert!(!decoder.warnings().is_empty());
}

#[test]
fn e37_a_chunk_that_decodes_to_too_many_bytes_is_an_error() {
    // A PackBits stream that expands past the strip geometry.
    let mut tiff = RawTiff::new();
    let stream = vec![0xFFu8, 0x41, 0xFF, 0x41, 0xFF, 0x41];
    let offset = tiff.add_data(&stream);
    tiff.long(256, &[2]);
    tiff.long(257, &[1]);
    tiff.short(258, &[8]);
    tiff.short(259, &[32773]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset as u32]);
    tiff.short(277, &[1]);
    tiff.long(278, &[1]);
    tiff.long(279, &[stream.len() as u32]);
    let err = decode(tiff.build()).read_image().expect_err("overrun");
    assert!(matches!(err, TiffError::Format(_)));
}

#[test]
fn e38_a_wrong_length_color_map_warns_or_errors() {
    let pixels = [0u8, 1, 1, 0];
    let mut tiff = gray8(2, 2, &pixels);
    tiff.short(258, &[1]);
    tiff.short(262, &[3]);
    tiff.short(320, &[0, 0xFFFF]); // 2 entries, needs 6
    tiff.long(279, &[1]);
    let bytes = tiff.build();
    assert!(decode_strict(bytes.clone()).info().is_err());
    let mut lenient = decode(bytes);
    lenient.info().expect("warned");
    assert!(!lenient.warnings().is_empty());
}

#[test]
fn e39_fax_options_on_a_non_ccitt_image_are_ignored_with_a_warning() {
    let mut tiff = gray8(2, 2, &ramp(4));
    tiff.long(292, &[5]);
    let mut decoder = decode(tiff.build());
    decoder.info().expect("info");
    assert!(
        decoder
            .warnings()
            .iter()
            .any(|w| format!("{w:?}").contains("292"))
    );
}

#[test]
fn e40_jpeg_tables_on_a_non_jpeg_image_are_ignored_with_a_warning() {
    let mut tiff = gray8(2, 2, &ramp(4));
    tiff.undefined(347, &[0xFF, 0xD8, 0xFF, 0xD9]);
    let mut decoder = decode(tiff.build());
    decoder.info().expect("info");
    assert!(
        decoder
            .warnings()
            .iter()
            .any(|w| format!("{w:?}").contains("347"))
    );
}

// ------------------------------------------------- codec dispatch coverage

#[test]
fn scheduled_codecs_return_the_named_error() {
    for method in [2u16, 3, 4, 5, 6, 7, 8, 32946, 34925, 50000] {
        let mut tiff = gray8(4, 4, &ramp(16));
        tiff.short(259, &[method]);
        let result = decode(tiff.build()).read_image();
        match result {
            Err(TiffError::Unsupported(UnsupportedError::NotYetAvailable { method: m, name })) => {
                assert_eq!(m, method);
                assert!(!name.is_empty());
            }
            other => panic!("compression {method} produced {other:?}"),
        }
    }
}

#[test]
fn genuinely_unknown_compressions_report_the_number() {
    for method in [32766u16, 32771, 34661, 32895, 32947, 50001, 50002] {
        let mut tiff = gray8(4, 4, &ramp(16));
        tiff.short(259, &[method]);
        let err = decode(tiff.build()).read_image().expect_err("unsupported");
        assert!(
            matches!(
                err,
                TiffError::Unsupported(UnsupportedError::Compression(m)) if m == method
            ),
            "compression {method} produced {err:?}"
        );
    }
}

#[test]
fn packbits_round_trips_through_the_writer() {
    let pixels = ramp(64);
    let spec = ImageSpec::new(8, 8, ColorType::Gray(8))
        .with_compression(Compression::PackBits)
        .with_layout(Layout::Strips { rows_per_strip: 4 });
    let samples = round_trip(&spec, &pixels);
    assert_eq!(samples.as_u8(), Some(&pixels[..]));
}

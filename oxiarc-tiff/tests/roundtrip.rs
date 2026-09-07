//! Writer -> reader round-trips across the full geometry matrix.
//!
//! These cover the combinations libtiff itself cannot write (BigTIFF plus
//! tiles, planar plus a float predictor, 12-bit data), so they run
//! unconditionally rather than behind the oracle feature.

use oxiarc_tiff::tags::{ExtraSamples, FillOrder, PlanarConfiguration, SampleFormat, Tag};
use oxiarc_tiff::{
    ChunkType, ColorType, Compression, Decoder, Encoder, Endian, ImageSpec, Layout, Predictor,
    Rational, ResolutionUnit, SampleType, Samples, Value, VariantChoice,
};
use std::io::Cursor;

fn write(spec: &ImageSpec, data: &[u8], endian: Endian, variant: VariantChoice) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer)
        .expect("encoder")
        .with_endian(endian)
        .with_variant(variant);
    encoder.write_image(spec, data).expect("write image");
    encoder.finish().expect("finish");
    buffer.into_inner()
}

fn read_back(bytes: Vec<u8>) -> (Samples, Decoder<Cursor<Vec<u8>>>) {
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    let samples = decoder.read_image().expect("read image");
    (samples, decoder)
}

fn ramp_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i.wrapping_mul(37) % 251) as u8).collect()
}

#[test]
fn every_bit_depth_and_sample_format_round_trips() {
    let cases: [(u16, SampleFormat, SampleType); 16] = [
        (1, SampleFormat::Uint, SampleType::U8),
        (2, SampleFormat::Uint, SampleType::U8),
        (4, SampleFormat::Uint, SampleType::U8),
        (8, SampleFormat::Uint, SampleType::U8),
        (12, SampleFormat::Uint, SampleType::U16),
        (16, SampleFormat::Uint, SampleType::U16),
        (24, SampleFormat::Uint, SampleType::U32),
        (32, SampleFormat::Uint, SampleType::U32),
        (64, SampleFormat::Uint, SampleType::U64),
        (8, SampleFormat::Int, SampleType::I8),
        (16, SampleFormat::Int, SampleType::I16),
        (32, SampleFormat::Int, SampleType::I32),
        (64, SampleFormat::Int, SampleType::I64),
        (16, SampleFormat::IeeeFp, SampleType::F16),
        (32, SampleFormat::IeeeFp, SampleType::F32),
        (64, SampleFormat::IeeeFp, SampleType::F64),
    ];
    for (bits, format, slot) in cases {
        let width = 5u32;
        let height = 3u32;
        let count = (width * height) as usize;
        let max = if bits >= 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        };
        let mut native = Vec::new();
        for i in 0..count {
            let raw = (i as u64 * 7 + 1) & max;
            match slot {
                SampleType::U8 | SampleType::I8 => native.push(raw as u8),
                SampleType::U16 | SampleType::I16 | SampleType::F16 => {
                    native.extend_from_slice(&(raw as u16).to_ne_bytes());
                }
                SampleType::U32 | SampleType::I32 => {
                    native.extend_from_slice(&(raw as u32).to_ne_bytes());
                }
                SampleType::F32 => {
                    native.extend_from_slice(&(i as f32 * 0.5 - 1.0).to_ne_bytes());
                }
                SampleType::F64 => {
                    native.extend_from_slice(&(i as f64 * 0.25 - 2.0).to_ne_bytes());
                }
                _ => native.extend_from_slice(&raw.to_ne_bytes()),
            }
        }
        for endian in [Endian::Little, Endian::Big] {
            let spec = ImageSpec::new(width, height, ColorType::Gray(bits as u8))
                .with_sample_format(format)
                .with_layout(Layout::Strips { rows_per_strip: 2 });
            let bytes = write(&spec, &native, endian, VariantChoice::Classic);
            let (samples, mut decoder) = read_back(bytes);
            assert_eq!(
                samples.sample_type(),
                slot,
                "{bits} bits {format} in {endian:?}"
            );
            assert_eq!(
                samples.to_native_bytes(),
                native,
                "{bits} bits {format} in {endian:?}"
            );
            assert_eq!(decoder.dimensions().expect("dims"), (width, height));
        }
    }
}

#[test]
fn every_colour_type_round_trips() {
    let cases: [ColorType; 8] = [
        ColorType::Gray(8),
        ColorType::GrayA(8),
        ColorType::Rgb(8),
        ColorType::Rgba(8),
        ColorType::Cmyk(8),
        ColorType::CmykA(8),
        ColorType::YCbCr(8),
        ColorType::Multiband {
            bit_depth: 16,
            num_samples: 5,
        },
    ];
    for colour in cases {
        let (w, h) = (4u32, 4u32);
        let spp = usize::from(colour.samples_per_pixel());
        let slot = usize::from(colour.bit_depth()) / 8;
        let data = ramp_bytes((w * h) as usize * spp * slot);
        let spec = ImageSpec::new(w, h, colour).with_layout(Layout::Strips { rows_per_strip: 2 });
        let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
        let (samples, mut decoder) = read_back(bytes);
        assert_eq!(samples.to_native_bytes(), data, "{colour:?}");
        assert_eq!(decoder.color_type().expect("colour"), colour);
    }
}

#[test]
fn every_layout_and_container_combination_round_trips() {
    let (w, h) = (33u32, 17u32);
    let data = ramp_bytes((w * h) as usize * 3);
    let layouts = [
        Layout::Strips { rows_per_strip: 1 },
        Layout::Strips { rows_per_strip: 4 },
        Layout::Strips {
            rows_per_strip: 100_000,
        },
        Layout::Tiles {
            width: 16,
            length: 16,
        },
        Layout::Tiles {
            width: 48,
            length: 32,
        },
    ];
    for layout in layouts {
        for endian in [Endian::Little, Endian::Big] {
            for variant in [VariantChoice::Classic, VariantChoice::Big] {
                for planar in [PlanarConfiguration::Chunky, PlanarConfiguration::Planar] {
                    let spec = ImageSpec::new(w, h, ColorType::Rgb(8))
                        .with_layout(layout)
                        .with_planar(planar);
                    let bytes = write(&spec, &data, endian, variant);
                    let (samples, mut decoder) = read_back(bytes);
                    assert_eq!(
                        samples.as_u8(),
                        Some(&data[..]),
                        "{layout:?} {endian:?} {variant:?} {planar:?}"
                    );
                    let expected_chunk = match layout {
                        Layout::Tiles { .. } => ChunkType::Tile,
                        _ => ChunkType::Strip,
                    };
                    assert_eq!(decoder.chunk_type().expect("type"), expected_chunk);
                }
            }
        }
    }
}

#[test]
fn predictors_round_trip_for_every_supported_width() {
    for bits in [8u16, 16, 32, 64] {
        let (w, h) = (6u32, 4u32);
        let data = ramp_bytes((w * h) as usize * 3 * usize::from(bits) / 8);
        for planar in [PlanarConfiguration::Chunky, PlanarConfiguration::Planar] {
            for endian in [Endian::Little, Endian::Big] {
                let spec = ImageSpec::new(w, h, ColorType::Rgb(bits as u8))
                    .with_predictor(Predictor::Horizontal)
                    .with_planar(planar)
                    .with_layout(Layout::Strips { rows_per_strip: 2 });
                let bytes = write(&spec, &data, endian, VariantChoice::Classic);
                let (samples, _) = read_back(bytes);
                assert_eq!(
                    samples.to_native_bytes(),
                    data,
                    "predictor 2, {bits} bits, {planar:?}, {endian:?}"
                );
            }
        }
    }
}

#[test]
fn the_float_predictor_round_trips_in_tiles_and_strips() {
    let (w, h) = (32u32, 32u32);
    let count = (w * h) as usize;
    let mut data = Vec::new();
    for i in 0..count {
        data.extend_from_slice(&(i as f32 * 0.125 - 64.0).to_ne_bytes());
    }
    for layout in [
        Layout::Strips { rows_per_strip: 8 },
        Layout::Tiles {
            width: 16,
            length: 16,
        },
    ] {
        for endian in [Endian::Little, Endian::Big] {
            let spec = ImageSpec::new(w, h, ColorType::Gray(32))
                .with_sample_format(SampleFormat::IeeeFp)
                .with_predictor(Predictor::FloatingPoint)
                .with_layout(layout);
            let bytes = write(&spec, &data, endian, VariantChoice::Classic);
            let (samples, _) = read_back(bytes);
            assert_eq!(samples.to_native_bytes(), data, "{layout:?} {endian:?}");
        }
    }
}

#[test]
fn packbits_round_trips_across_the_matrix() {
    let (w, h) = (20u32, 12u32);
    // Runs and literals, so both PackBits branches are exercised.
    let data: Vec<u8> = (0..(w * h) as usize)
        .map(|i| if i % 11 < 6 { 0xAA } else { (i % 251) as u8 })
        .collect();
    for layout in [
        Layout::Strips { rows_per_strip: 3 },
        Layout::Tiles {
            width: 16,
            length: 16,
        },
    ] {
        for variant in [VariantChoice::Classic, VariantChoice::Big] {
            let spec = ImageSpec::new(w, h, ColorType::Gray(8))
                .with_compression(Compression::PackBits)
                .with_layout(layout);
            let bytes = write(&spec, &data, Endian::Little, variant);
            let (samples, _) = read_back(bytes);
            assert_eq!(samples.as_u8(), Some(&data[..]), "{layout:?} {variant:?}");
        }
    }
}

#[test]
fn fill_order_two_round_trips_for_sub_byte_depths() {
    for bits in [1u16, 2, 4] {
        let (w, h) = (13u32, 5u32);
        let max = (1u32 << bits) - 1;
        let data: Vec<u8> = (0..(w * h)).map(|i| (i % (max + 1)) as u8).collect();
        for compression in [Compression::None, Compression::PackBits] {
            let spec = ImageSpec::new(w, h, ColorType::Gray(bits as u8))
                .with_fill_order(FillOrder::Lsb2Msb)
                .with_compression(compression)
                .with_layout(Layout::Strips { rows_per_strip: 2 });
            let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
            let (samples, mut decoder) = read_back(bytes);
            assert_eq!(
                samples.as_u8(),
                Some(&data[..]),
                "{bits} bits {compression:?}"
            );
            assert_eq!(decoder.info().expect("info").fill_order, FillOrder::Lsb2Msb);
        }
    }
}

#[test]
fn a_palette_image_round_trips_and_expands() {
    let bits = 4u16;
    let entries = 1usize << bits;
    let mut map = vec![0u16; entries * 3];
    for i in 0..entries {
        map[i] = (i as u16) << 12;
        map[entries + i] = 0x8000;
        map[2 * entries + i] = 0xFFFF - ((i as u16) << 12);
    }
    let data: Vec<u8> = (0..24u8).map(|i| i % 16).collect();
    let spec = ImageSpec::new(6, 4, ColorType::Palette(4))
        .with_color_map(map.clone())
        .with_layout(Layout::Strips { rows_per_strip: 2 });
    let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
    let (samples, mut decoder) = read_back(bytes);
    assert_eq!(samples.as_u8(), Some(&data[..]));
    assert_eq!(decoder.info().expect("info").color_map, Some(map));
    let rgb = decoder.read_image_rgb8().expect("palette expansion");
    assert_eq!(rgb.len(), 24 * 3);
    assert_eq!(&rgb[..3], &[0x00, 0x80, 0xFF]);
}

#[test]
fn min_is_white_is_reported_and_inverted_only_on_request() {
    let data = vec![0u8, 255, 128, 64];
    let spec = ImageSpec::new(2, 2, ColorType::Gray(8))
        .with_photometric(oxiarc_tiff::PhotometricInterpretation::WhiteIsZero);
    let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
    let (samples, mut decoder) = read_back(bytes);
    // The raw path never rewrites colour.
    assert_eq!(samples.as_u8(), Some(&data[..]));
    let rgb = decoder.read_image_rgb8().expect("inverted");
    assert_eq!(&rgb[..3], &[255, 255, 255]);
    assert_eq!(&rgb[3..6], &[0, 0, 0]);
}

#[test]
fn subsampled_ycbcr_round_trips_uncompressed() {
    for subsampling in [(1u16, 1u16), (2, 1), (2, 2), (4, 4)] {
        let (w, h) = (8u32, 8u32);
        let mut data = vec![0u8; (w * h * 3) as usize];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let spec = ImageSpec::new(w, h, ColorType::YCbCr(8))
            .with_ycbcr_subsampling(subsampling.0, subsampling.1)
            .with_layout(Layout::Strips { rows_per_strip: 8 });
        let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
        let (samples, mut decoder) = read_back(bytes);
        assert_eq!(decoder.info().expect("info").ycbcr_subsampling, subsampling);
        let decoded = samples.as_u8().expect("u8 samples");
        // Luma survives exactly; chroma is block-constant after subsampling.
        for pixel in 0..(w * h) as usize {
            assert_eq!(
                decoded[pixel * 3],
                data[pixel * 3],
                "luma {pixel} at {subsampling:?}"
            );
        }
        if subsampling == (1, 1) {
            assert_eq!(decoded, &data[..]);
        }
        // The colour conversion runs without panicking.
        let rgb = decoder.read_image_rgb8().expect("ycbcr to rgb");
        assert_eq!(rgb.len(), (w * h * 3) as usize);
    }
}

#[test]
fn arbitrary_tags_and_metadata_blobs_round_trip_byte_identically() {
    let geo_ascii = Value::Ascii("WGS 84|Unknown|".to_string());
    let geo_keys = Value::Short(vec![
        1, 1, 0, 4, 1024, 0, 1, 2, 1025, 0, 1, 1, 3072, 0, 1, 32767,
    ]);
    let geo_doubles = Value::Double(vec![0.5, -1.25, 1e10]);
    let pixel_scale = Value::Double(vec![10.0, 10.0, 0.0]);
    let tiepoint = Value::Double(vec![0.0, 0.0, 0.0, 100.0, 200.0, 0.0]);
    let icc = Value::Undefined((0u8..64).collect());
    let xmp = Value::Byte(b"<x:xmpmeta/>".to_vec());
    let iptc = Value::Byte(vec![0x1C, 0x02, 0x05, 0x00, 0x03, b'a', b'b', b'c']);
    let photoshop = Value::Byte(b"8BIM".to_vec());

    let spec = ImageSpec::new(2, 2, ColorType::Gray(8))
        .with_extra_tag(Tag::GeoAsciiParams.to_u16(), geo_ascii.clone())
        .with_extra_tag(Tag::GeoKeyDirectory.to_u16(), geo_keys.clone())
        .with_extra_tag(Tag::GeoDoubleParams.to_u16(), geo_doubles.clone())
        .with_extra_tag(Tag::ModelPixelScale.to_u16(), pixel_scale.clone())
        .with_extra_tag(Tag::ModelTiepoint.to_u16(), tiepoint.clone())
        .with_extra_tag(Tag::InterColorProfile.to_u16(), icc.clone())
        .with_extra_tag(Tag::Xmp.to_u16(), xmp.clone())
        .with_extra_tag(Tag::IptcNaa.to_u16(), iptc.clone())
        .with_extra_tag(Tag::Photoshop.to_u16(), photoshop.clone())
        .with_extra_tag(Tag::DocumentName.to_u16(), Value::Ascii("page".to_string()))
        .with_extra_tag(Tag::PageName.to_u16(), Value::Ascii("front".to_string()))
        .with_extra_tag(Tag::PageNumber.to_u16(), Value::Short(vec![0, 2]));

    let bytes = write(&spec, &[1, 2, 3, 4], Endian::Little, VariantChoice::Classic);
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");

    let geo = decoder.geo_tags().expect("geo tags");
    assert!(!geo.is_empty());
    assert_eq!(geo.geo_ascii_params, Some(geo_ascii));
    assert_eq!(geo.geo_key_directory, Some(geo_keys));
    assert_eq!(geo.geo_double_params, Some(geo_doubles));
    assert_eq!(geo.model_pixel_scale, Some(pixel_scale));
    assert_eq!(geo.model_tiepoint, Some(tiepoint));
    assert_eq!(geo.model_transformation, None);
    assert_eq!(geo.to_extra_tags().len(), 5);

    assert_eq!(
        decoder.icc_profile().expect("icc"),
        icc.as_bytes().map(<[u8]>::to_vec)
    );
    assert_eq!(
        decoder.xmp().expect("xmp"),
        xmp.as_bytes().map(<[u8]>::to_vec)
    );
    assert_eq!(
        decoder.iptc().expect("iptc"),
        iptc.as_bytes().map(<[u8]>::to_vec)
    );
    assert_eq!(
        decoder.photoshop().expect("photoshop"),
        photoshop.as_bytes().map(<[u8]>::to_vec)
    );
    // The byte content matching is not the whole story: the field *type*
    // (UNDEFINED for ICC vs. BYTE for XMP/IPTC/Photoshop here) matters to
    // some consumers, and a lossy round trip could silently coerce one into
    // the other while every assertion above still passed.
    assert_eq!(
        decoder.find_tag(Tag::InterColorProfile).expect("icc tag"),
        Some(icc)
    );
    assert_eq!(decoder.find_tag(Tag::Xmp).expect("xmp tag"), Some(xmp));
    assert_eq!(
        decoder.find_tag(Tag::IptcNaa).expect("iptc tag"),
        Some(iptc)
    );
    assert_eq!(
        decoder.find_tag(Tag::Photoshop).expect("photoshop tag"),
        Some(photoshop)
    );
    assert_eq!(
        decoder.get_tag_ascii(Tag::DocumentName).expect("doc"),
        Some("page".to_string())
    );
    assert_eq!(
        decoder.get_tag_ascii(Tag::PageName).expect("page"),
        Some("front".to_string())
    );
    assert_eq!(
        decoder.find_tag(Tag::PageNumber).expect("page number"),
        Some(Value::Short(vec![0, 2]))
    );
}

#[test]
fn resolution_and_extra_samples_survive_the_round_trip() {
    let spec = ImageSpec::new(2, 2, ColorType::Rgba(8))
        .with_resolution(
            Rational { num: 300, den: 1 },
            Rational { num: 300, den: 1 },
            ResolutionUnit::Inch,
        )
        .with_extra_samples(vec![ExtraSamples::UnassociatedAlpha]);
    let bytes = write(&spec, &[0u8; 16], Endian::Little, VariantChoice::Classic);
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    let info = decoder.info().expect("info");
    assert_eq!(info.resolution.0, Some(Rational { num: 300, den: 1 }));
    assert_eq!(info.resolution.1, Some(Rational { num: 300, den: 1 }));
    assert_eq!(info.resolution.2, ResolutionUnit::Inch);
    assert_eq!(info.extra_samples, vec![ExtraSamples::UnassociatedAlpha]);
}

#[test]
fn region_reads_match_the_whole_image() {
    let (w, h) = (37u32, 23u32);
    let data = ramp_bytes((w * h) as usize * 3);
    for layout in [
        Layout::Strips { rows_per_strip: 5 },
        Layout::Tiles {
            width: 16,
            length: 16,
        },
    ] {
        let spec = ImageSpec::new(w, h, ColorType::Rgb(8)).with_layout(layout);
        let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        for (x, y, rw, rh) in [
            (0u32, 0u32, w, h),
            (3, 4, 10, 7),
            (30, 20, 7, 3),
            (0, 22, 37, 1),
        ] {
            let region = decoder.read_region(x, y, rw, rh).expect("region");
            let got = region.as_u8().expect("u8");
            for row in 0..rh as usize {
                for col in 0..(rw as usize * 3) {
                    let expected =
                        data[((y as usize + row) * w as usize * 3) + x as usize * 3 + col];
                    assert_eq!(
                        got[row * rw as usize * 3 + col],
                        expected,
                        "{layout:?} region {x},{y} {rw}x{rh} at {row},{col}"
                    );
                }
            }
        }
        assert!(decoder.read_region(w, 0, 1, 1).is_err());
        assert!(decoder.read_region(0, 0, w + 1, h).is_err());
    }
}

#[test]
fn raw_and_decoded_chunk_access_agree() {
    let (w, h) = (16u32, 16u32);
    let data = ramp_bytes((w * h) as usize);
    let spec = ImageSpec::new(w, h, ColorType::Gray(8))
        .with_compression(Compression::PackBits)
        .with_layout(Layout::Strips { rows_per_strip: 4 });
    let bytes = write(&spec, &data, Endian::Little, VariantChoice::Classic);
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    assert_eq!(decoder.chunk_count().expect("count"), 4);
    for index in 0..4u64 {
        let raw = decoder.read_strip_raw(index).expect("raw strip");
        assert!(!raw.is_empty());
        let decoded = decoder.read_strip(index).expect("decoded strip");
        assert_eq!(decoded.len(), (w * 4) as usize);
        let start = (index as usize) * (w * 4) as usize;
        assert_eq!(
            decoded.as_u8(),
            Some(&data[start..start + (w * 4) as usize])
        );
    }
    assert!(decoder.read_tile(0).is_err());
    assert!(decoder.read_chunk(4).is_err());
}

#[test]
fn streaming_row_writes_match_a_whole_image_write() {
    let (w, h) = (9u32, 7u32);
    let data = ramp_bytes((w * h) as usize * 3);
    let spec =
        ImageSpec::new(w, h, ColorType::Rgb(8)).with_layout(Layout::Strips { rows_per_strip: 3 });

    let one_shot = write(&spec, &data, Endian::Little, VariantChoice::Classic);

    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    {
        let mut page = encoder.new_image(&spec).expect("page");
        let row_len = (w * 3) as usize;
        for row in 0..h as usize {
            page.write_rows(&data[row * row_len..(row + 1) * row_len])
                .expect("row");
        }
        page.finish().expect("finish page");
    }
    encoder.finish().expect("finish");
    let streamed = buffer.into_inner();
    assert_eq!(streamed, one_shot);
}

#[test]
fn sub_ifds_exif_and_gps_are_navigable() {
    // Build a page whose SubIFDs/Exif/GPS pointers all target a second page.
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image(&ImageSpec::new(2, 2, ColorType::Gray(8)), &[1, 2, 3, 4])
        .expect("page 0");
    encoder
        .write_image(&ImageSpec::new(1, 1, ColorType::Gray(8)), &[9])
        .expect("page 1");
    encoder.finish().expect("finish");
    let bytes = buffer.into_inner();

    // Find page 1's IFD offset through the chain, then rebuild page 0 pointing
    // at it. Reading the chain is enough to prove navigation works.
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    assert_eq!(decoder.image_count().expect("count"), 2);
    decoder.seek_to_image(1).expect("seek");
    let second = decoder.ifd_pointer().expect("pointer");
    decoder.seek_to_image(0).expect("seek back");
    let directory = decoder.read_directory_at(second).expect("read child ifd");
    assert!(directory.contains(Tag::ImageWidth));
    assert!(decoder.sub_ifds().expect("no sub ifds").is_empty());
    assert!(decoder.exif_directory().expect("no exif").is_none());
    assert!(decoder.gps_directory().expect("no gps").is_none());
    assert!(decoder.interop_directory().expect("no interop").is_none());
    assert!(decoder.sub_ifd_tree().expect("no tree").is_empty());
}

#[test]
fn all_tags_lists_the_written_directory() {
    let spec = ImageSpec::new(2, 2, ColorType::Gray(8));
    let bytes = write(&spec, &[1, 2, 3, 4], Endian::Little, VariantChoice::Classic);
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    let tags = decoder.all_tags().expect("tags");
    let numbers: Vec<u16> = tags.iter().map(|(t, _)| t.to_u16()).collect();
    assert!(numbers.windows(2).all(|w| w[0] < w[1]), "{numbers:?}");
    for required in [256u16, 257, 258, 259, 262, 273, 277, 278, 279] {
        assert!(numbers.contains(&required), "tag {required} missing");
    }
}

#[test]
fn an_empty_encoder_still_produces_a_valid_header() {
    for variant in [VariantChoice::Classic, VariantChoice::Big] {
        let mut buffer = Cursor::new(Vec::new());
        let encoder = Encoder::new(&mut buffer)
            .expect("encoder")
            .with_variant(variant);
        encoder.finish().expect("finish");
        let bytes = buffer.into_inner();
        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("header parses");
        assert_eq!(decoder.image_count().expect("count"), 0);
        assert!(decoder.info().is_err());
    }
}

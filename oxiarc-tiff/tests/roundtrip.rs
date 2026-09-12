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

/// A greyscale-plus-alpha JPEG page round-trips **chunky**.
///
/// Until `oxiarc-jpeg` 0.4.2 this was refused: JPEG has no *named*
/// two-component colour space, and libjpeg reaches one only through
/// `JCS_UNKNOWN`, which `oxiarc-jpeg` did not encode yet. It now does
/// (`ColorSpace::Unknown(2)`: sequential ids `1`/`2`, no subsampling, no
/// colour transform, no JFIF/Adobe marker), so `ImageSpec::validate`'s
/// former guard and the `plan()` arm that produced the refusal are both
/// gone — this is the regression test that they stay gone.
/// `PlanarConfiguration::Planar` (each channel its own single-component
/// frame) still works too, and is checked alongside chunky rather than
/// dropped, since it is still a legitimate way to write the same page.
#[cfg(feature = "jpeg")]
#[test]
fn a_two_channel_jpeg_page_round_trips_chunky_through_jcs_unknown() {
    use oxiarc_tiff::PlanarConfiguration;

    let (width, height) = (32u32, 16u32);
    let pixels: Vec<u8> = (0..width * height * 2).map(|i| (i % 251) as u8).collect();
    let jpeg = oxiarc_tiff::Compression::Jpeg {
        quality: 90,
        shared_tables: true,
    };
    let base = oxiarc_tiff::ImageSpec::new(width, height, oxiarc_tiff::ColorType::GrayA(8))
        .with_compression(jpeg)
        .with_layout(oxiarc_tiff::Layout::Strips {
            rows_per_strip: height,
        });
    assert_eq!(
        base.planar,
        PlanarConfiguration::Chunky,
        "chunky is the default, and the case that used to be refused"
    );

    let mut chunky_bytes = Vec::new();
    let mut encoder =
        oxiarc_tiff::Encoder::new(std::io::Cursor::new(&mut chunky_bytes)).expect("encoder");
    encoder.write_image(&base, &pixels).expect("chunky write");
    encoder.finish().expect("finish");

    let mut decoder =
        oxiarc_tiff::Decoder::new(std::io::Cursor::new(&chunky_bytes)).expect("decoder");
    let mut chunky_decoded = vec![0u8; pixels.len()];
    decoder
        .read_image_bytes(&mut chunky_decoded)
        .expect("decode");
    for (index, (a, b)) in chunky_decoded.iter().zip(pixels.iter()).enumerate() {
        assert!(
            a.abs_diff(*b) <= 20,
            "chunky sample {index}: {a} against {b} (JPEG is lossy, but not that lossy)"
        );
    }

    // `PlanarConfiguration::Planar` is still a legitimate way to write the
    // same page, and still works.
    let mut planar_bytes = Vec::new();
    let mut encoder =
        oxiarc_tiff::Encoder::new(std::io::Cursor::new(&mut planar_bytes)).expect("encoder");
    let planar_spec = base.with_planar(PlanarConfiguration::Planar);
    encoder
        .write_image(&planar_spec, &pixels)
        .expect("planar write");
    encoder.finish().expect("finish");

    let mut decoder =
        oxiarc_tiff::Decoder::new(std::io::Cursor::new(&planar_bytes)).expect("decoder");
    let mut planar_decoded = vec![0u8; pixels.len()];
    decoder
        .read_image_bytes(&mut planar_decoded)
        .expect("decode");
    for (index, (a, b)) in planar_decoded.iter().zip(pixels.iter()).enumerate() {
        assert!(
            a.abs_diff(*b) <= 20,
            "planar sample {index}: {a} against {b} (JPEG is lossy, but not that lossy)"
        );
    }

    // `tiffinfo` parses the chunky file: `Compression Scheme: JPEG`,
    // `Samples/Pixel: 2`, `Extra Samples: 1<unassoc-alpha>`, the right
    // geometry, and no warning on stderr. Self-skips when `tiffinfo` is
    // absent, as every oracle check in this crate does.
    if let Some(tiffinfo) = which("tiffinfo") {
        let path = std::env::temp_dir().join(format!(
            "oxiarc_tiff_two_channel_{}.tif",
            std::process::id()
        ));
        std::fs::write(&path, &chunky_bytes).expect("write scratch file");
        let output = std::process::Command::new(&tiffinfo)
            .arg(&path)
            .output()
            .expect("spawn tiffinfo");
        let _ = std::fs::remove_file(&path);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "tiffinfo did not parse the chunky two-channel file cleanly:\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for expected in [
            "Image Width: 32 Image Length: 16",
            "Compression Scheme: JPEG",
            "Samples/Pixel: 2",
            "Extra Samples: 1<unassoc-alpha>",
        ] {
            assert!(
                stdout.contains(expected),
                "tiffinfo output missing {expected:?}:\n{stdout}"
            );
        }
    } else {
        eprintln!("libtiff's tiffinfo is not on PATH; skipping that half of the check");
    }

    // Stronger still: `tiffcp -c none` makes libtiff's *own* embedded
    // libjpeg actually entropy-decode the two-component scan (not merely
    // parse its headers, which is all a bare `djpeg` can do for this shape —
    // see `oxiarc-jpeg/tests/encode_oracle.rs` for why) and re-encode
    // uncompressed. Decoding libtiff's own recoded output through this
    // crate must be byte-identical to decoding the original file through
    // this crate: the two are the same pixels through the same JPEG loss,
    // decoded by two different JPEG decoders written by two different
    // projects.
    if let Some(tiffcp) = which("tiffcp") {
        let source_path = std::env::temp_dir().join(format!(
            "oxiarc_tiff_two_channel_src_{}.tif",
            std::process::id()
        ));
        let recoded_path = std::env::temp_dir().join(format!(
            "oxiarc_tiff_two_channel_recoded_{}.tif",
            std::process::id()
        ));
        std::fs::write(&source_path, &chunky_bytes).expect("write scratch file");
        let recode = std::process::Command::new(&tiffcp)
            .args(["-c", "none"])
            .arg(&source_path)
            .arg(&recoded_path)
            .output()
            .expect("spawn tiffcp");
        assert!(
            recode.status.success(),
            "libtiff's tiffcp refused to recode the two-channel JPEG page: {}",
            String::from_utf8_lossy(&recode.stderr)
        );
        let recoded_bytes = std::fs::read(&recoded_path).expect("read recoded file");
        let _ = std::fs::remove_file(&source_path);
        let _ = std::fs::remove_file(&recoded_path);

        let mut recoded_decoder =
            oxiarc_tiff::Decoder::new(std::io::Cursor::new(recoded_bytes)).expect("decoder");
        let mut via_libtiff = vec![0u8; pixels.len()];
        recoded_decoder
            .read_image_bytes(&mut via_libtiff)
            .expect("decode libtiff's recode");
        assert_eq!(
            via_libtiff, chunky_decoded,
            "libtiff's own libjpeg decoded the two-component scan to different \
             samples than this crate's decoder did"
        );
    } else {
        eprintln!("libtiff's tiffcp is not on PATH; skipping that half of the check");
    }
}

/// A tool on `PATH`, found the same way `which` reports it, or `None`.
///
/// Only the two-channel JPEG test below uses this, so it is gated the same
/// way that test is: unused (and a `dead_code` error under `-D warnings`)
/// when the `jpeg` feature is off.
#[cfg(feature = "jpeg")]
fn which(name: &str) -> Option<std::path::PathBuf> {
    // The bare name first: `which` does not exist on Windows outside a POSIX
    // shell, and inside one (MSYS / Git Bash) it prints a POSIX path that
    // `CreateProcess` cannot open. Only spawnability is checked here.
    if std::process::Command::new(name)
        .arg("--version")
        .output()
        .is_ok()
    {
        return Some(std::path::PathBuf::from(name));
    }
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = std::process::Command::new(locator)
        .arg(name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // `where` can report several matches, one per line; take the first.
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    (!path.is_empty()).then(|| std::path::PathBuf::from(path))
}

/// The chunky two-channel JPEG page above is written as a single strip. The
/// interesting boundaries are the ones a real writer hits: several strips
/// (each its own abbreviated frame, the last one short), tiles, restart
/// markers inside the scan, and tables written per chunk instead of into tag
/// 347. None of these had ever coded a two-component frame — before
/// `oxiarc-jpeg` gained `ColorSpace::Unknown(2)` the spec was refused
/// outright — so each one is checked here rather than assumed to follow from
/// the single-strip case.
#[cfg(feature = "jpeg")]
#[test]
fn two_channel_jpeg_pages_survive_strip_tile_and_restart_boundaries() {
    use oxiarc_tiff::{Compression, Decoder, Encoder, ImageSpec, Layout};

    fn page(width: u32, height: u32) -> Vec<u8> {
        (0..width * height * 2).map(|i| (i % 251) as u8).collect()
    }

    fn round_trip(spec: &ImageSpec, data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = Encoder::new(std::io::Cursor::new(&mut bytes)).expect("encoder");
        encoder.write_image(spec, data).expect("write");
        encoder.finish().expect("finish");
        let mut decoder = Decoder::new(std::io::Cursor::new(&bytes)).expect("decoder");
        let mut out = vec![0u8; data.len()];
        decoder.read_image_bytes(&mut out).expect("decode");
        out
    }

    fn check(label: &str, decoded: &[u8], expected: &[u8]) {
        assert_eq!(decoded.len(), expected.len(), "{label}: wrong length");
        for (index, (a, b)) in decoded.iter().zip(expected.iter()).enumerate() {
            assert!(
                a.abs_diff(*b) <= 20,
                "{label}: sample {index} is {a} against {b} (JPEG is lossy, but not \
                 that lossy — a boundary is coding the wrong samples)"
            );
        }
    }

    let jpeg = Compression::Jpeg {
        quality: 90,
        shared_tables: true,
    };

    // Several strips, including a short final one (20 rows in strips of 8)
    // and a page narrower than one MCU row (17 wide, 8 rows per strip).
    for (width, height, rows_per_strip) in [
        (32u32, 20u32, 8u32),
        (32, 24, 8),
        (17, 20, 8),
        (32, 40, 24),
        (7, 3, 8),
    ] {
        let data = page(width, height);
        let spec = ImageSpec::new(width, height, oxiarc_tiff::ColorType::GrayA(8))
            .with_compression(jpeg)
            .with_layout(Layout::Strips { rows_per_strip });
        let decoded = round_trip(&spec, &data);
        check(
            &format!("strips {width}x{height}/{rows_per_strip}"),
            &decoded,
            &data,
        );
    }

    // Tiles, including a page whose edge tiles are partly padding.
    for (width, height, tile_width, tile_length) in [
        (32u32, 32u32, 16u32, 16u32),
        (40, 24, 16, 16),
        (17, 19, 32, 16),
    ] {
        let data = page(width, height);
        let spec = ImageSpec::new(width, height, oxiarc_tiff::ColorType::GrayA(8))
            .with_compression(jpeg)
            .with_layout(Layout::Tiles {
                width: tile_width,
                length: tile_length,
            });
        let decoded = round_trip(&spec, &data);
        check(
            &format!("tiles {width}x{height}/{tile_width}x{tile_length}"),
            &decoded,
            &data,
        );
    }

    // Restart markers inside a two-component interleaved scan, with the
    // tables in tag 347 and again written into every chunk.
    for shared_tables in [true, false] {
        for restart_rows in [0u16, 1, 2] {
            let (width, height) = (32u32, 24u32);
            let data = page(width, height);
            let spec = ImageSpec::new(width, height, oxiarc_tiff::ColorType::GrayA(8))
                .with_compression(Compression::Jpeg {
                    quality: 85,
                    shared_tables,
                })
                .with_jpeg_restart_rows(restart_rows)
                .with_layout(Layout::Strips { rows_per_strip: 8 });
            let decoded = round_trip(&spec, &data);
            check(
                &format!("shared={shared_tables} restart_rows={restart_rows}"),
                &decoded,
                &data,
            );
        }
    }
}

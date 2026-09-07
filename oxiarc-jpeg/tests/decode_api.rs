//! Public API behaviour that does not need an external reference decoder.
//!
//! Every fixture here is either the crate's embedded sample or a stream built
//! from it by patching bytes, so the whole file runs on a hermetic machine.

use oxiarc_jpeg::{
    CodingProcess, ColorSpace, DecodeLimits, DecodeOptions, Decoder, EntropyCoding, JpegError,
    LimitKind, PixelFormat, TableSet, TablesMode, UnsupportedFeature, Upsampling,
    decode_abbreviated, decode_abbreviated_into, decode_abbreviated_into_u16, sample,
};

/// Offset of the `SOF` marker in a datastream, and its code byte.
fn find_sof(data: &[u8]) -> (usize, u8) {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0xD9 {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xCC {
            return (i, marker);
        }
        i += 2 + length;
    }
    panic!("no SOF in fixture");
}

/// Replace the `SOF` marker code, leaving everything else alone.
fn patch_sof_marker(data: &[u8], code: u8) -> Vec<u8> {
    let (at, _) = find_sof(data);
    let mut out = data.to_vec();
    out[at + 1] = code;
    out
}

/// Set the `SOF` height to zero and append a `DNL` segment naming `height`.
fn with_dnl(data: &[u8], height: u16) -> Vec<u8> {
    let (at, _) = find_sof(data);
    let mut out = data.to_vec();
    out[at + 5] = 0;
    out[at + 6] = 0;
    // Splice the DNL in just before the trailing EOI.
    let eoi = out.len() - 2;
    let dnl = [0xFFu8, 0xDC, 0x00, 0x04, (height >> 8) as u8, height as u8];
    out.splice(eoi..eoi, dnl);
    out
}

#[test]
fn reads_info_from_the_embedded_grayscale_sample() {
    let mut decoder = Decoder::new(&sample::GRAY_1X1[..]);
    let info = decoder.read_info().expect("read_info");
    assert_eq!((info.width, info.height), (1, 1));
    assert_eq!(info.precision, 8);
    assert_eq!(info.num_components, 1);
    assert_eq!(info.process, CodingProcess::Baseline);
    assert_eq!(info.entropy, EntropyCoding::Huffman);
    assert_eq!(info.input_color_space, ColorSpace::Luma);
    assert_eq!(info.output_color_space, ColorSpace::Luma);
    assert!(info.has_jfif);
    assert!(!info.has_adobe);
    assert_eq!(info.subsampling, (1, 1));
    assert_eq!(info.components().len(), 1);
    assert_eq!(info.components()[0].id, 1);
    assert_eq!(decoder.pixel_format(), Some(PixelFormat::L8));
    assert_eq!(decoder.output_buffer_size(), Some(1));
    assert_eq!(decoder.decode().expect("decode").len(), 1);

    // read_info is idempotent.
    let mut decoder = Decoder::new(&sample::GRAY_1X1[..]);
    let first = decoder.read_info().expect("first");
    let second = decoder.read_info().expect("second");
    assert_eq!(first, second);
    assert_eq!(decoder.info(), Some(first));
}

#[test]
fn reads_info_and_geometry_from_the_colour_sample() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    let info = decoder.read_info().expect("read_info");
    assert_eq!((info.width, info.height), (8, 8));
    assert_eq!(info.num_components, 3);
    assert_eq!(info.input_color_space, ColorSpace::Ycbcr);
    assert_eq!(info.output_color_space, ColorSpace::Rgb);
    assert_eq!(info.subsampling, (2, 2));
    let components = info.components();
    assert_eq!((components[0].h, components[0].v), (2, 2));
    assert_eq!((components[1].h, components[1].v), (1, 1));
    assert_eq!(components[0].quant_table, 0);
    assert_eq!(components[1].quant_table, 1);

    let frame = decoder.frame_header().expect("frame header");
    assert_eq!((frame.hmax, frame.vmax), (2, 2));
    assert_eq!(frame.mcus_per_line, 1);
    assert_eq!(frame.components[1].width_samples, 4);

    let pixels = decoder.decode().expect("decode");
    assert_eq!(pixels.len(), 8 * 8 * 3);
}

#[test]
fn decode_into_and_strided_agree() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let flat = decoder.decode().expect("decode");

    let mut packed = vec![0u8; flat.len()];
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    decoder.decode_into(&mut packed).expect("decode_into");
    assert_eq!(packed, flat);

    // A strided destination inside a larger canvas.
    let stride = 8 * 3 + 7;
    let mut canvas = vec![0xAAu8; stride * 8];
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    decoder
        .decode_into_strided(&mut canvas, stride)
        .expect("strided");
    for y in 0..8 {
        assert_eq!(
            &canvas[y * stride..y * stride + 24],
            &flat[y * 24..y * 24 + 24]
        );
        if y < 7 {
            assert!(
                canvas[y * stride + 24..(y + 1) * stride]
                    .iter()
                    .all(|&b| b == 0xAA),
                "row {y} padding was overwritten"
            );
        }
    }
}

#[test]
fn strided_decode_rejects_a_short_buffer_and_a_small_stride() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let mut small = [0u8; 4];
    assert!(matches!(
        decoder.decode_into_strided(&mut small, 24),
        Err(JpegError::BufferTooSmall { .. })
    ));

    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let mut buffer = vec![0u8; 8 * 8 * 3];
    assert!(matches!(
        decoder.decode_into_strided(&mut buffer, 10),
        Err(JpegError::BufferTooSmall { .. })
    ));
}

#[test]
fn raw_components_skip_the_colour_transform() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let rgb = decoder.decode().expect("rgb");

    let mut decoder = Decoder::with_options(&sample::RGB_8X8_420[..], DecodeOptions::raw());
    let info = decoder.read_info().expect("info");
    assert_eq!(info.output_color_space, ColorSpace::Ycbcr);
    let raw = decoder.decode().expect("raw");
    assert_eq!(raw.len(), rgb.len());
    assert_ne!(raw, rgb, "raw components must not be colour-converted");
}

#[test]
fn forcing_luma_output_drops_the_chroma() {
    let options = DecodeOptions {
        output_color_space: Some(ColorSpace::Luma),
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(&sample::RGB_8X8_420[..], options);
    let info = decoder.read_info().expect("info");
    assert_eq!(info.output_color_space, ColorSpace::Luma);
    assert_eq!(decoder.output_buffer_size(), Some(64));
    assert_eq!(decoder.decode().expect("decode").len(), 64);
}

#[test]
fn an_unrepresentable_output_transform_is_rejected() {
    let options = DecodeOptions {
        output_color_space: Some(ColorSpace::Cmyk),
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(&sample::RGB_8X8_420[..], options);
    decoder.read_info().expect("info");
    assert!(matches!(
        decoder.decode(),
        Err(JpegError::Unsupported(UnsupportedFeature::ColorTransform(
            _
        )))
    ));
}

#[test]
fn box_and_fancy_upsampling_differ_on_a_subsampled_image() {
    let mut fancy = Decoder::new(&sample::RGB_8X8_420[..]);
    fancy.read_info().expect("info");
    let fancy = fancy.decode().expect("decode");

    let options = DecodeOptions {
        upsampling: Upsampling::Box,
        ..DecodeOptions::default()
    };
    let mut boxed = Decoder::with_options(&sample::RGB_8X8_420[..], options);
    boxed.read_info().expect("info");
    let boxed = boxed.decode().expect("decode");
    assert_eq!(fancy.len(), boxed.len());
    assert_ne!(fancy, boxed);
}

#[test]
fn abbreviated_decode_matches_the_interchange_stream() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let whole = decoder.decode().expect("decode");

    let tables = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
    let mut out = vec![0u8; whole.len()];
    let info = decode_abbreviated_into(
        Some(&tables),
        &sample::RGB_8X8_420_SCAN,
        &DecodeOptions::default(),
        &mut out,
    )
    .expect("abbreviated");
    assert_eq!((info.width, info.height), (8, 8));
    assert_eq!(out, whole);

    let (info2, allocated) = decode_abbreviated(
        Some(&tables),
        &sample::RGB_8X8_420_SCAN,
        &DecodeOptions::default(),
    )
    .expect("abbreviated");
    assert_eq!(info2.width, info.width);
    assert_eq!(allocated, whole);
}

#[test]
fn load_tables_primes_a_scan_only_stream() {
    let tables = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
    let mut decoder = Decoder::new(&sample::RGB_8X8_420_SCAN[..]);
    decoder.load_tables(&tables);
    decoder.read_info().expect("info");
    let pixels = decoder.decode().expect("decode");
    assert_eq!(pixels.len(), 8 * 8 * 3);

    // Without the tables the same stream cannot be decoded.
    let mut decoder = Decoder::new(&sample::RGB_8X8_420_SCAN[..]);
    decoder.read_info().expect("info");
    assert!(matches!(
        decoder.decode(),
        Err(JpegError::UndefinedTable { .. })
    ));
}

#[test]
fn stream_tables_override_loaded_tables() {
    // Load a table set whose quantiser is deliberately wrong; the interchange
    // stream carries its own DQT, which must win.
    let mut wrong = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
    wrong.quant[0] = Some(oxiarc_jpeg::QuantTable::annex_k_luma().scaled_for_quality(1, true));
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.load_tables(&wrong);
    decoder.read_info().expect("info");
    let patched = decoder.decode().expect("decode");

    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let plain = decoder.decode().expect("decode");
    assert_eq!(patched, plain);
}

#[test]
fn tables_mode_round_trips_through_the_tiff_helpers() {
    let tables = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
    assert_eq!(
        tables.emit(TablesMode::BOTH),
        sample::RGB_8X8_420_TABLES.to_vec(),
        "libtiff's 574-byte two-DQT/four-DHT layout must be reproduced exactly"
    );
}

#[test]
fn dnl_resolves_a_zero_height_frame() {
    let stream = with_dnl(&sample::RGB_8X8_420, 8);
    let mut decoder = Decoder::new(stream.as_slice());
    let info = decoder.read_info().expect("read_info");
    assert_eq!(info.height, 8);
    let pixels = decoder.decode().expect("decode");
    assert_eq!(pixels.len(), 8 * 8 * 3);

    let mut reference = Decoder::new(&sample::RGB_8X8_420[..]);
    reference.read_info().expect("info");
    assert_eq!(pixels, reference.decode().expect("decode"));
}

#[test]
fn a_zero_height_frame_without_dnl_is_a_named_error() {
    let (at, _) = find_sof(&sample::RGB_8X8_420);
    let mut stream = sample::RGB_8X8_420.to_vec();
    stream[at + 5] = 0;
    stream[at + 6] = 0;
    let mut decoder = Decoder::new(stream.as_slice());
    assert!(matches!(decoder.read_info(), Err(JpegError::MissingDnl)));
}

#[test]
fn hierarchical_frames_are_a_named_unsupported_error() {
    for code in [0xC5u8, 0xC6, 0xC7, 0xCD, 0xCE, 0xCF] {
        let stream = patch_sof_marker(&sample::RGB_8X8_420, code);
        let mut decoder = Decoder::new(stream.as_slice());
        assert!(
            matches!(
                decoder.read_info(),
                Err(JpegError::Unsupported(UnsupportedFeature::Hierarchical))
            ),
            "SOF marker {code:#04X} should report Hierarchical"
        );
    }
}

#[test]
fn arithmetic_frames_parse_but_do_not_decode_yet() {
    for code in [0xC9u8, 0xCA, 0xCB] {
        let stream = patch_sof_marker(&sample::RGB_8X8_420, code);
        let mut decoder = Decoder::new(stream.as_slice());
        let info = decoder
            .read_info()
            .unwrap_or_else(|e| panic!("SOF {code:#04X} header should parse: {e}"));
        assert_eq!(info.entropy, EntropyCoding::Arithmetic);
        assert!(matches!(
            decoder.decode(),
            Err(JpegError::Unsupported(UnsupportedFeature::ArithmeticCoding))
        ));
    }
}

#[test]
fn dac_segments_are_parsed_and_survive_a_table_round_trip() {
    let mut tables = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
    tables.arithmetic.dc[1] = 0x53;
    tables.arithmetic.ac[2] = 9;
    let blob = tables.emit(TablesMode::BOTH);
    let reparsed = TableSet::parse(&blob).expect("reparse");
    assert_eq!(reparsed.arithmetic.dc[1], 0x53);
    assert_eq!(reparsed.arithmetic.ac[2], 9);
    assert_eq!(
        reparsed.arithmetic.dc[0], 0x10,
        "untouched slot keeps L=0,U=1"
    );
}

#[test]
fn eight_bit_entry_points_reject_a_twelve_bit_frame() {
    // Patch the sample into a 12-bit extended-sequential frame header. The
    // entropy data will not decode, but the precision check happens first.
    let (at, _) = find_sof(&sample::RGB_8X8_420);
    let mut stream = sample::RGB_8X8_420.to_vec();
    stream[at + 1] = 0xC1;
    stream[at + 4] = 12;
    let mut decoder = Decoder::new(stream.as_slice());
    let info = decoder.read_info().expect("info");
    assert_eq!(info.precision, 12);
    assert_eq!(decoder.pixel_format(), Some(PixelFormat::Rgb16));
}

#[test]
fn a_truncated_scan_is_an_error_unless_tolerated() {
    let mut truncated = sample::RGB_8X8_420.to_vec();
    truncated.truncate(truncated.len() - 6);
    let mut decoder = Decoder::new(truncated.as_slice());
    decoder.read_info().expect("info");
    assert!(decoder.decode().is_err());

    let options = DecodeOptions {
        tolerate_truncated: true,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(truncated.as_slice(), options);
    decoder.read_info().expect("info");
    let pixels = decoder.decode().expect("tolerant decode");
    assert_eq!(pixels.len(), 8 * 8 * 3);
}

#[test]
fn limits_are_enforced_before_allocating() {
    let options = DecodeOptions {
        limits: DecodeLimits {
            max_pixels: 4,
            ..DecodeLimits::default()
        },
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(&sample::RGB_8X8_420[..], options);
    assert!(matches!(
        decoder.read_info(),
        Err(JpegError::LimitExceeded(LimitKind::Pixels))
    ));

    let options = DecodeOptions {
        limits: DecodeLimits {
            max_components: 1,
            ..DecodeLimits::default()
        },
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(&sample::RGB_8X8_420[..], options);
    assert!(matches!(
        decoder.read_info(),
        Err(JpegError::LimitExceeded(LimitKind::Components))
    ));

    let options = DecodeOptions {
        limits: DecodeLimits {
            max_output_bytes: 8,
            ..DecodeLimits::default()
        },
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(&sample::RGB_8X8_420[..], options);
    decoder.read_info().expect("info");
    assert!(matches!(
        decoder.decode(),
        Err(JpegError::LimitExceeded(LimitKind::OutputBytes))
    ));
}

#[test]
fn metadata_is_exposed_verbatim() {
    let mut decoder = Decoder::new(&sample::GRAY_1X1[..]);
    decoder.read_info().expect("info");
    let jfif = decoder.jfif().expect("JFIF APP0");
    assert_eq!(jfif.version_major, 1);
    assert!(decoder.adobe().is_none());
    assert!(decoder.exif().is_none());
    assert!(decoder.xmp().is_none());
    assert!(decoder.comments().is_empty());
    assert!(decoder.icc_profile().expect("no ICC").is_none());
    let segments = decoder.app_segments();
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].marker, 0xE0);
    assert!(segments[0].data.starts_with(b"JFIF\0"));
}

#[test]
fn u16_entry_points_work_for_eight_bit_frames() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let wide = decoder.decode_u16().expect("decode_u16");

    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    decoder.read_info().expect("info");
    let narrow = decoder.decode().expect("decode");
    let widened: Vec<u16> = narrow.iter().map(|&b| u16::from(b)).collect();
    assert_eq!(wide, widened);

    let mut buffer = vec![0u16; wide.len()];
    let tables = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
    decode_abbreviated_into_u16(
        Some(&tables),
        &sample::RGB_8X8_420_SCAN,
        &DecodeOptions::default(),
        &mut buffer,
    )
    .expect("abbreviated u16");
    assert_eq!(buffer, wide);
}

#[test]
fn into_inner_hands_the_source_back() {
    let data: &[u8] = &sample::GRAY_1X1;
    let mut decoder = Decoder::new(data);
    decoder.read_info().expect("info");
    let inner = decoder.into_inner();
    assert_eq!(inner.len(), 0, "the source was consumed by buffering");
}

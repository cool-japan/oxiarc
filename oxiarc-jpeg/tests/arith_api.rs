//! Arithmetic coding tests that need no external tools.
//!
//! The `SOF9`/`SOF10` halves are pinned byte-for-byte against libjpeg in
//! `arith_oracle.rs`; these cover what no reference implementation can check:
//! `SOF11` (libjpeg-turbo refuses `-lossless` together with `-arithmetic`,
//! and its `jdarith.c` has no lossless path at all), custom `DAC`
//! conditioning, and the API surface.
#![cfg(feature = "arithmetic")]

use oxiarc_jpeg::{
    ArithmeticConditioning, CodingProcess, DecodeOptions, Decoder, EncodeOptions, EncodeProcess,
    EntropyCoding, InputColor, RestartInterval, Subsampling, TableSet, TablesMode,
    decode_abbreviated_into, encode_to_vec_with_options, encode_u16_to_vec_with_options,
};

/// A source image with flat regions, hard edges and a gradient.
fn source(width: usize, height: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            let values = [
                ((x * 7 + y * 13) % 256) as u8,
                if (x / 5 + y / 3) % 2 == 0 { 255 } else { 32 },
                ((x * 3 + y * 11) % 256) as u8,
            ];
            for c in 0..channels {
                out.push(values[c % 3]);
            }
        }
    }
    out
}

fn wide_source(width: usize, height: usize, channels: usize, maxval: u16) -> Vec<u16> {
    let mut out = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            for c in 0..channels {
                let value = ((x * 37 + y * 101 + c * 613) % (usize::from(maxval) + 1)) as u16;
                out.push(value);
            }
        }
    }
    out
}

/// Lossless arithmetic (`SOF11`) must reproduce every sample exactly, for
/// every predictor and at several precisions.
#[test]
fn lossless_arithmetic_round_trips_exactly() {
    for predictor in 1u8..=7 {
        for &(width, height) in &[(1usize, 1usize), (16, 9), (37, 23)] {
            let pixels = source(width, height, 3);
            let options = EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor,
                    point_transform: 0,
                },
                entropy: EntropyCoding::Arithmetic,
                ..Default::default()
            };
            let jpeg = encode_to_vec_with_options(
                &pixels,
                width as u16,
                height as u16,
                InputColor::Rgb,
                &options,
            )
            .expect("encode");
            assert_eq!(
                jpeg[jpeg
                    .windows(2)
                    .position(|w| w[0] == 0xFF && w[1] == 0xCB)
                    .expect("SOF11 marker")
                    + 1],
                0xCB
            );
            let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
            assert_eq!(
                decoded, pixels,
                "predictor {predictor} at {width}x{height} is not lossless"
            );
        }
    }
}

/// Lossless arithmetic at 12 and 16 bits, with a point transform.
#[test]
fn wide_lossless_arithmetic_round_trips() {
    for &(precision, maxval) in &[(12u8, 4095u16), (16, 65535)] {
        for &point_transform in &[0u8, 1, 3] {
            let pixels = wide_source(23, 17, 1, maxval);
            let options = EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 4,
                    point_transform,
                },
                precision,
                entropy: EntropyCoding::Arithmetic,
                ..Default::default()
            };
            let jpeg = encode_u16_to_vec_with_options(&pixels, 23, 17, InputColor::Luma, &options)
                .expect("encode");
            let decoded = Decoder::new(&jpeg[..]).decode_u16().expect("decode");
            let expected: Vec<u16> = pixels
                .iter()
                .map(|&v| (v >> point_transform) << point_transform)
                .collect();
            assert_eq!(
                decoded, expected,
                "precision {precision} Pt {point_transform}"
            );
        }
    }
}

/// Restart intervals in a lossless arithmetic scan reset the statistics, the
/// conditioning and the prediction together; a mismatch between the two sides
/// shows up as corruption after the first interval.
#[test]
fn lossless_arithmetic_restarts_round_trip() {
    for &interval in &[
        RestartInterval::McuRows(1),
        RestartInterval::McuRows(2),
        RestartInterval::Mcus(9),
    ] {
        let pixels = source(24, 12, 3);
        let options = EncodeOptions {
            process: EncodeProcess::Lossless {
                predictor: 2,
                point_transform: 0,
            },
            restart_interval: interval,
            entropy: EntropyCoding::Arithmetic,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 24, 12, InputColor::Rgb, &options).expect("encode");
        assert!(
            jpeg.windows(2)
                .any(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1])),
            "no restart markers were written"
        );
        let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
        assert_eq!(decoded, pixels, "{interval:?}");
    }
}

/// Encode one image twice — once with each entropy coder — and decode both.
///
/// Entropy coding is downstream of the DCT and the quantiser, so the two
/// streams carry *identical* coefficients and must decode to identical
/// pixels. That is a far sharper assertion than any tolerance: it holds the
/// arithmetic path to the Huffman path, which `encode_oracle.rs` already
/// holds to `cjpeg` byte for byte.
fn assert_entropy_coders_agree(
    pixels: &[u8],
    width: u16,
    height: u16,
    color: InputColor,
    options: &EncodeOptions,
    label: &str,
) {
    let mut huffman = options.clone();
    huffman.entropy = EntropyCoding::Huffman;
    let mut arithmetic = options.clone();
    arithmetic.entropy = EntropyCoding::Arithmetic;

    let huffman_jpeg =
        encode_to_vec_with_options(pixels, width, height, color, &huffman).expect("huffman encode");
    let arithmetic_jpeg = encode_to_vec_with_options(pixels, width, height, color, &arithmetic)
        .expect("arithmetic encode");
    assert!(
        arithmetic_jpeg.len() != huffman_jpeg.len() || arithmetic_jpeg != huffman_jpeg,
        "{label}: the two coders produced the same bytes, so nothing was tested"
    );

    let from_huffman = Decoder::new(&huffman_jpeg[..]).decode().expect("decode");
    let from_arithmetic = Decoder::new(&arithmetic_jpeg[..]).decode().expect("decode");
    assert_eq!(
        from_arithmetic, from_huffman,
        "{label}: the arithmetic stream decoded differently from its Huffman twin"
    );
}

/// Sequential and progressive arithmetic, at restart intervals and at 4:2:0,
/// decode exactly as their Huffman twins do.
#[test]
fn dct_arithmetic_agrees_with_its_huffman_twin() {
    for &process in &[EncodeProcess::Sequential, EncodeProcess::Progressive] {
        for &subsampling in &[Subsampling::S444, Subsampling::S420, Subsampling::S422] {
            for &restart in &[RestartInterval::None, RestartInterval::McuRows(1)] {
                for &(width, height) in &[(40usize, 24usize), (17, 19), (1, 1)] {
                    let pixels = source(width, height, 3);
                    let options = EncodeOptions {
                        quality: 95,
                        subsampling,
                        process,
                        restart_interval: restart,
                        ..Default::default()
                    };
                    assert_entropy_coders_agree(
                        &pixels,
                        width as u16,
                        height as u16,
                        InputColor::Rgb,
                        &options,
                        &format!("{process:?} {subsampling:?} {restart:?} {width}x{height}"),
                    );
                }
            }
        }
    }
}

/// A non-default `DAC` must survive the round trip: the encoder writes it, the
/// decoder reads it, and both condition on it. Encoding with one conditioning
/// and decoding with the parser's default would corrupt the image.
#[test]
fn custom_conditioning_round_trips() {
    let pixels = source(32, 16, 3);
    let mut conditioning = ArithmeticConditioning::default();
    conditioning.dc[0] = 0x53; // L = 3, U = 5
    conditioning.dc[1] = 0x21; // L = 1, U = 2
    conditioning.ac[0] = 9;
    conditioning.ac[1] = 3;
    let options = EncodeOptions {
        quality: 90,
        entropy: EntropyCoding::Arithmetic,
        arithmetic: conditioning,
        ..Default::default()
    };
    let jpeg =
        encode_to_vec_with_options(&pixels, 32, 16, InputColor::Rgb, &options).expect("encode");

    // The DAC really carries the custom values.
    let dac = jpeg
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] == 0xCC)
        .expect("DAC segment");
    assert_eq!(
        &jpeg[dac + 4..dac + 12],
        &[0x00, 0x53, 0x10, 9, 0x01, 0x21, 0x11, 3]
    );

    let mut decoder = Decoder::new(&jpeg[..]);
    let info = decoder.read_info().expect("read_info");
    assert_eq!(info.entropy, EntropyCoding::Arithmetic);
    assert_eq!(info.process, CodingProcess::ExtendedSequential);
    let decoded = decoder.decode().expect("decode");

    // Conditioning changes how the coefficients are coded, never what they
    // are, so the pixels must equal those of the default-conditioning stream
    // exactly.
    let default_options = EncodeOptions {
        quality: 90,
        entropy: EntropyCoding::Arithmetic,
        ..Default::default()
    };
    let default_jpeg =
        encode_to_vec_with_options(&pixels, 32, 16, InputColor::Rgb, &default_options)
            .expect("encode");
    assert_ne!(
        jpeg, default_jpeg,
        "the custom conditioning did not reach the wire"
    );
    let default_decoded = Decoder::new(&default_jpeg[..]).decode().expect("decode");
    assert_eq!(decoded, default_decoded);
}

/// Lossless arithmetic conditioning is two-dimensional (T.81 H.1.2.3), so a
/// custom `L`/`U` changes far more state than in a DCT frame. It must still
/// round-trip exactly.
#[test]
fn custom_conditioning_round_trips_losslessly() {
    let pixels = source(29, 13, 1);
    let mut conditioning = ArithmeticConditioning::default();
    conditioning.dc[0] = 0x42; // L = 2, U = 4
    let options = EncodeOptions {
        process: EncodeProcess::Lossless {
            predictor: 5,
            point_transform: 0,
        },
        entropy: EntropyCoding::Arithmetic,
        arithmetic: conditioning,
        ..Default::default()
    };
    let jpeg =
        encode_to_vec_with_options(&pixels, 29, 13, InputColor::Luma, &options).expect("encode");
    assert!(
        jpeg.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xCC),
        "a lossless arithmetic frame must carry its conditioning"
    );
    let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
    assert_eq!(decoded, pixels);
}

/// The abbreviated (TIFF `JPEGTables`) split works for arithmetic frames too:
/// the tables half carries `DQT` and `DAC`, the strip half carries neither.
#[test]
fn abbreviated_arithmetic_strips_decode_against_their_tables() {
    let pixels = source(16, 16, 3);
    let mut options = EncodeOptions::tiff_strip(75);
    options.entropy = EntropyCoding::Arithmetic;
    let tables = oxiarc_jpeg::table_set(&options, InputColor::Rgb).expect("table_set");
    let blob = tables.emit(TablesMode::BOTH);
    assert!(
        blob.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xDB),
        "the tables blob must carry DQT"
    );

    let mut strip = Vec::new();
    let mut encoder = oxiarc_jpeg::Encoder::with_options(&mut strip, options);
    encoder
        .encode_scan_only(&pixels, 16, 16, InputColor::Rgb)
        .expect("encode_scan_only");
    encoder.finish().expect("finish");
    assert!(
        !strip.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xDB),
        "a scan-only stream must not carry tables"
    );

    let parsed = TableSet::parse(&blob).expect("parse");
    let mut out = vec![0u8; 16 * 16 * 3];
    let info = decode_abbreviated_into(Some(&parsed), &strip, &DecodeOptions::raw(), &mut out)
        .expect("decode_abbreviated_into");
    assert_eq!(info.num_components, 3);
    assert_eq!(info.entropy, EntropyCoding::Arithmetic);
}

/// Every `SOF` an arithmetic frame can carry, and the process each maps to.
#[test]
fn arithmetic_frames_carry_the_right_sof_marker() {
    let pixels = source(8, 8, 1);
    let cases: [(EncodeProcess, u8, CodingProcess); 3] = [
        (
            EncodeProcess::Sequential,
            0xC9,
            CodingProcess::ExtendedSequential,
        ),
        (EncodeProcess::Progressive, 0xCA, CodingProcess::Progressive),
        (
            EncodeProcess::Lossless {
                predictor: 1,
                point_transform: 0,
            },
            0xCB,
            CodingProcess::Lossless,
        ),
    ];
    for &(process, marker, expected) in &cases {
        let options = EncodeOptions {
            process,
            entropy: EntropyCoding::Arithmetic,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 8, 8, InputColor::Luma, &options).expect("encode");
        assert!(
            jpeg.windows(2).any(|w| w[0] == 0xFF && w[1] == marker),
            "{process:?} did not write {marker:#04x}"
        );
        assert!(
            !jpeg.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xC4),
            "an arithmetic frame must carry no DHT"
        );
        let info = Decoder::new(&jpeg[..]).read_info().expect("read_info");
        assert_eq!(info.process, expected);
        assert_eq!(info.entropy, EntropyCoding::Arithmetic);
    }
}

/// `optimize_huffman` is meaningless for an arithmetic frame and is ignored
/// rather than rejected, exactly as libjpeg ignores `-optimize -arithmetic`.
#[test]
fn optimize_huffman_is_ignored_for_arithmetic_frames() {
    let pixels = source(16, 16, 3);
    let options = EncodeOptions {
        optimize_huffman: true,
        entropy: EntropyCoding::Arithmetic,
        ..Default::default()
    };
    let jpeg =
        encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options).expect("encode");
    assert!(!jpeg.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xC4));
    // And it stays abbreviatable, which `optimize_huffman` would have blocked.
    let mut abbreviated = EncodeOptions::tiff_strip(75);
    abbreviated.entropy = EntropyCoding::Arithmetic;
    abbreviated.optimize_huffman = true;
    assert!(oxiarc_jpeg::table_set(&abbreviated, InputColor::Rgb).is_ok());
}

/// Corrupt the entropy data of an arithmetic stream every which way.
///
/// The QM decoder has two failure modes T.81 leaves undefined — a magnitude
/// chain past `2^15` and a spectral index past `Se` — and both must surface
/// as [`oxiarc_jpeg::JpegError::InvalidArithmeticCode`] rather than as a
/// panic or as silence. The test asserts the fault path is actually reached,
/// so it cannot pass by never triggering anything.
#[test]
fn corrupt_arithmetic_scans_fault_rather_than_panic() {
    use oxiarc_jpeg::JpegError;

    let pixels = source(32, 24, 3);
    let mut faults = 0usize;
    let mut decoded = 0usize;
    for &process in &[
        EncodeProcess::Sequential,
        EncodeProcess::Progressive,
        EncodeProcess::Lossless {
            predictor: 1,
            point_transform: 0,
        },
    ] {
        let options = EncodeOptions {
            quality: 85,
            process,
            entropy: EntropyCoding::Arithmetic,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 32, 24, InputColor::Rgb, &options).expect("encode");
        // Find the first entropy byte: everything after the first SOS.
        let sos = jpeg
            .windows(2)
            .position(|w| w[0] == 0xFF && w[1] == 0xDA)
            .expect("SOS");
        let length = usize::from(u16::from_be_bytes([jpeg[sos + 2], jpeg[sos + 3]]));
        let entropy = sos + 2 + length;

        for offset in entropy..jpeg.len() {
            for mask in [0x01u8, 0x40, 0xFF] {
                let mut corrupt = jpeg.clone();
                corrupt[offset] ^= mask;
                let mut decoder = Decoder::new(corrupt.as_slice());
                if decoder.read_info().is_err() {
                    continue;
                }
                match decoder.decode() {
                    Ok(pixels) => {
                        assert_eq!(pixels.len(), 32 * 24 * 3);
                        decoded += 1;
                    }
                    Err(JpegError::InvalidArithmeticCode { .. }) => faults += 1,
                    Err(_) => {}
                }
            }
        }
    }
    assert!(decoded > 0, "no corrupt stream decoded at all");
    assert!(
        faults > 0,
        "no corruption reached the arithmetic fault path, so it is untested"
    );
}

/// A sixteen-bit lossless frame whose neighbouring samples are 0 and 65535
/// produces differences of 65535, which T.81 H.1.2.1 codes **modulo 2^16**.
///
/// Without that reduction the magnitude-category chain runs one step past
/// `X15` and off the end of the 158-bin statistics area. This is the fixture
/// that found it.
#[test]
fn sixteen_bit_lossless_extremes_round_trip() {
    let mut pixels = Vec::with_capacity(32 * 8);
    for y in 0..8 {
        for x in 0..32 {
            pixels.push(if (x + y) % 2 == 0 { 0u16 } else { 65535 });
        }
    }
    for predictor in 1u8..=7 {
        let options = EncodeOptions {
            process: EncodeProcess::Lossless {
                predictor,
                point_transform: 0,
            },
            precision: 16,
            entropy: EntropyCoding::Arithmetic,
            ..Default::default()
        };
        let jpeg = encode_u16_to_vec_with_options(&pixels, 32, 8, InputColor::Luma, &options)
            .expect("encode");
        let decoded = Decoder::new(&jpeg[..]).decode_u16().expect("decode");
        assert_eq!(decoded, pixels, "predictor {predictor}");
    }
}

/// The builder-style setters reach the same options the struct literal does.
#[test]
fn the_encoder_setters_reach_the_arithmetic_options() {
    let pixels = source(16, 16, 3);
    let mut conditioning = ArithmeticConditioning::default();
    conditioning.dc[0] = 0x31;

    let mut bytes = Vec::new();
    let mut encoder = oxiarc_jpeg::Encoder::new(&mut bytes);
    encoder
        .set_quality(88)
        .set_entropy(EntropyCoding::Arithmetic)
        .set_arithmetic_conditioning(conditioning);
    encoder
        .encode(&pixels, 16, 16, InputColor::Rgb)
        .expect("encode");
    encoder.finish().expect("finish");

    assert!(bytes.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xC9));
    let dac = bytes
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] == 0xCC)
        .expect("DAC");
    assert_eq!(&bytes[dac + 4..dac + 6], &[0x00, 0x31]);
    let info = Decoder::new(&bytes[..]).read_info().expect("read_info");
    assert_eq!(info.entropy, EntropyCoding::Arithmetic);
}

/// A sixteen-bit lossless OJPEG strip needs the wide entry point; the
/// eight-bit one refuses rather than truncating.
#[test]
fn wide_ojpeg_strips_decode_through_the_u16_entry_point() {
    use oxiarc_jpeg::tiff::{OJpegGeometry, OJpegTags, decode_ojpeg_into, decode_ojpeg_into_u16};

    let pixels: Vec<u16> = (0..16 * 8).map(|i| ((i * 517) % 65536) as u16).collect();
    let options = EncodeOptions {
        process: EncodeProcess::Lossless {
            predictor: 1,
            point_transform: 0,
        },
        precision: 16,
        write_jfif: oxiarc_jpeg::MarkerPolicy::Never,
        write_adobe: oxiarc_jpeg::MarkerPolicy::Never,
        ..Default::default()
    };
    let jpeg =
        encode_u16_to_vec_with_options(&pixels, 16, 8, InputColor::Luma, &options).expect("encode");

    let tags = OJpegTags {
        jpeg_proc: Some(14),
        interchange: Some(&jpeg),
        ..Default::default()
    };
    let geometry = OJpegGeometry {
        width: 16,
        height: 8,
        bits_per_sample: 16,
        samples_per_pixel: 1,
        ..Default::default()
    };
    let mut wide = vec![0u16; 16 * 8];
    decode_ojpeg_into_u16(&tags, &geometry, &[], &DecodeOptions::raw(), &mut wide)
        .expect("decode_ojpeg_into_u16");
    assert_eq!(wide, pixels);

    let mut narrow = vec![0u8; 16 * 8];
    assert!(decode_ojpeg_into(&tags, &geometry, &[], &DecodeOptions::raw(), &mut narrow).is_err());
}

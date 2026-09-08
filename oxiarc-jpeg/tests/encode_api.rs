//! Encoder API, round-trip and no-panic coverage that needs no external tool.

use oxiarc_jpeg::{
    CodingProcess, ColorSpace, ComponentIds, DecodeOptions, Decoder, Downsampling, EncodeOptions,
    EncodeProcess, Encoder, EntropyCoding, InputColor, JpegError, MarkerPolicy, QuantTable,
    QuantTableSource, RestartInterval, ScanSpec, Subsampling, TableSet, TablesMode,
    decode_abbreviated_into, decode_abbreviated_into_u16, encode_to_vec,
    encode_to_vec_with_options, encode_u16_to_vec_with_options, table_set,
};
use proptest::prelude::*;

/// A source with flat areas, hard edges and a gradient.
fn source(width: usize, height: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            let values = [
                ((x * 7 + y * 13) % 256) as u8,
                if (x / 5 + y / 3) % 2 == 0 { 240 } else { 24 },
                ((x * 3 + y * 11) % 256) as u8,
            ];
            for c in 0..channels {
                out.push(values[c % 3]);
            }
        }
    }
    out
}

fn mean_squared_error(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
        .sum::<f64>()
        / a.len() as f64
}

const AWKWARD: [(u16, u16); 12] = [
    (1, 1),
    (1, 2),
    (2, 1),
    (1, 17),
    (17, 1),
    (3, 3),
    (7, 9),
    (8, 8),
    (9, 8),
    (16, 17),
    (17, 16),
    (33, 31),
];

/// Every process, colour space and awkward size must encode, decode and
/// report the size it was given.
#[test]
fn every_shape_and_process_survives_a_round_trip() {
    let processes = [
        EncodeProcess::Sequential,
        EncodeProcess::Progressive,
        EncodeProcess::Lossless {
            predictor: 1,
            point_transform: 0,
        },
    ];
    let colours = [
        (InputColor::Luma, 1usize),
        (InputColor::LumaAlpha, 2),
        (InputColor::Rgb, 3),
        (InputColor::Bgr, 3),
        (InputColor::Rgba, 4),
        (InputColor::Cmyk, 4),
        // Pre-converted inputs, whose geometry is distinctive: `Ycck` gives
        // K the luma sampling factors, so K is a full-size component and
        // takes the plane-reuse path rather than the decimation path.
        (InputColor::Ycbcr, 3),
        (InputColor::Ycck, 4),
    ];
    for &process in &processes {
        for &(color, channels) in &colours {
            for &(width, height) in &AWKWARD {
                let pixels = source(usize::from(width), usize::from(height), channels);
                let options = EncodeOptions {
                    process,
                    quality: 80,
                    ..Default::default()
                };
                let jpeg = encode_to_vec_with_options(&pixels, width, height, color, &options)
                    .unwrap_or_else(|e| panic!("{color:?} {process:?} {width}x{height}: {e}"));
                let mut decoder = Decoder::new(&jpeg[..]);
                let info = decoder.read_info().expect("info");
                assert_eq!((info.width, info.height), (width, height));
                let decoded = decoder.decode().expect("decode");
                assert_eq!(decoded.len(), decoder.output_buffer_size().unwrap_or(0));
            }
        }
    }
}

/// A two-component frame ([`ColorSpace::Unknown(2)`], libjpeg's `JCS_UNKNOWN`
/// shape) round-trips through this crate's own decoder, keeping both
/// channels rather than dropping the second as the default `Luma` target
/// does for [`InputColor::LumaAlpha`].
#[test]
fn two_component_frames_round_trip_and_keep_both_channels() {
    for &(width, height) in &AWKWARD {
        let pixels = source(usize::from(width), usize::from(height), 2);
        let options = EncodeOptions {
            quality: 90,
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
                .unwrap_or_else(|e| panic!("{width}x{height}: {e}"));
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        assert_eq!(info.num_components, 2);
        assert_eq!(info.output_color_space, ColorSpace::Unknown(2));
        let decoded = decoder.decode().expect("decode");
        assert_eq!(decoded.len(), pixels.len());
        let error = mean_squared_error(&decoded, &pixels);
        assert!(
            error < 80.0,
            "{width}x{height}: MSE {error} too high for quality 90"
        );
    }
}

/// The default decode and an explicit [`DecodeOptions::raw`] decode must
/// agree byte for byte: a two-component frame's `output_color_space` is
/// already the same as its `input_color_space` (nothing to transform), so
/// asking for raw components changes nothing.
#[test]
fn two_component_default_decode_matches_raw_components_decode() {
    let pixels = source(9, 7, 2);
    let options = EncodeOptions {
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let jpeg =
        encode_to_vec_with_options(&pixels, 9, 7, InputColor::LumaAlpha, &options).expect("encode");

    let mut default_decoder = Decoder::new(&jpeg[..]);
    default_decoder.read_info().expect("info");
    let default_decoded = default_decoder.decode().expect("decode");

    let mut raw_decoder = Decoder::with_options(&jpeg[..], DecodeOptions::raw());
    raw_decoder.read_info().expect("info");
    let raw_decoded = raw_decoder.decode().expect("decode");

    assert_eq!(default_decoded, raw_decoded);
}

/// libjpeg writes no `JFIF` or Adobe marker for `JCS_UNKNOWN`, every
/// component sits on quantisation and Huffman slot 0, and the identifiers are
/// sequential (`1`, `2`) rather than letters — the whole shape TIFF's
/// `Compression = 7` strips need for a greyscale-plus-alpha page.
#[test]
fn two_component_frames_write_no_metadata_and_share_one_table_slot() {
    let pixels = source(6, 5, 2);
    let options = EncodeOptions {
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let jpeg =
        encode_to_vec_with_options(&pixels, 6, 5, InputColor::LumaAlpha, &options).expect("encode");

    let mut decoder = Decoder::new(&jpeg[..]);
    let info = decoder.read_info().expect("info");
    assert!(!info.has_jfif, "no JFIF marker for JCS_UNKNOWN");
    assert!(!info.has_adobe, "no Adobe marker for JCS_UNKNOWN");
    let frame = decoder.frame_header().expect("frame header");
    let ids: Vec<u8> = frame.components.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![1, 2]);
    assert!(frame.components.iter().all(|c| (c.h, c.v) == (1, 1)));
    assert!(frame.components.iter().all(|c| c.quant_table == 0));

    let tables = table_set(&options, InputColor::LumaAlpha).expect("table_set");
    assert!(tables.quant[0].is_some(), "slot 0 must be referenced");
    assert!(
        tables.quant[1..].iter().all(Option::is_none),
        "only one quantisation table"
    );
    assert!(
        tables.dc_huffman[1..].iter().all(Option::is_none),
        "only one DC Huffman table"
    );
    assert!(
        tables.ac_huffman[1..].iter().all(Option::is_none),
        "only one AC Huffman table"
    );
}

/// Only exactly two components reach `ColorSpace::Unknown`; every other
/// count is a named error, both through the public encode entry point and
/// regardless of which `InputColor` claims to supply it.
#[test]
fn unknown_colour_space_is_refused_except_at_two_components() {
    for count in [0u8, 1, 3, 4, 5, 6, 255] {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Unknown(count)),
            ..Default::default()
        };
        let error = encode_to_vec_with_options(&[0u8; 32], 4, 4, InputColor::LumaAlpha, &options)
            .expect_err(&format!("Unknown({count}) should be refused"));
        assert!(
            matches!(error, JpegError::InvalidEncodeParameter { .. }),
            "Unknown({count}): unexpected error {error}"
        );
    }
    // Even at the one accepted count, only `LumaAlpha` can reach it: `Luma`
    // has one channel and nothing to put in the second component.
    let options = EncodeOptions {
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    assert!(encode_to_vec_with_options(&[0u8; 16], 4, 4, InputColor::Luma, &options).is_err());
}

/// `component_template` is the only place that used to reject
/// `ColorSpace::Unknown` outright; nothing downstream of it — the
/// progressive scan-script builder, the lossless predictor, arithmetic
/// coding — special-cases component count the way it did. All three
/// processes, in both entropy codings, must therefore already accept a
/// two-component frame with no further change: lossless exactly (no
/// quantisation), the two lossy processes within ordinary JPEG error.
#[test]
fn two_component_frames_survive_every_process_and_entropy_coding() {
    let width = 17u16;
    let height = 19u16;
    let pixels = source(usize::from(width), usize::from(height), 2);
    let processes = [
        EncodeProcess::Sequential,
        EncodeProcess::Progressive,
        EncodeProcess::Lossless {
            predictor: 1,
            point_transform: 0,
        },
    ];
    let entropies: &[EntropyCoding] = &[
        EntropyCoding::Huffman,
        #[cfg(feature = "arithmetic")]
        EntropyCoding::Arithmetic,
    ];
    for &process in &processes {
        for &entropy in entropies {
            let options = EncodeOptions {
                process,
                entropy,
                jpeg_color_space: Some(ColorSpace::Unknown(2)),
                ..Default::default()
            };
            let jpeg =
                encode_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
                    .unwrap_or_else(|e| panic!("{process:?}/{entropy:?}: {e}"));
            let mut decoder = Decoder::new(&jpeg[..]);
            let info = decoder.read_info().expect("info");
            assert_eq!(info.num_components, 2);
            let decoded = decoder.decode().expect("decode");
            assert_eq!(decoded.len(), pixels.len());
            let max_diff = decoded
                .iter()
                .zip(pixels.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(255);
            if matches!(process, EncodeProcess::Lossless { .. }) {
                assert_eq!(
                    max_diff, 0,
                    "{process:?}/{entropy:?}: lossless must be exact"
                );
            } else {
                assert!(
                    max_diff < 64,
                    "{process:?}/{entropy:?}: max_diff {max_diff} too high"
                );
            }
        }
    }
}

/// [`Encoder::encode_planar`] interleaves its planes into the same buffer
/// shape [`encode_to_vec_with_options`] takes, so it must reach
/// `ColorSpace::Unknown(2)` exactly as the chunky entry point does — the two
/// paths are compared byte for byte, the same way `planar_input_matches_interleaved_input`
/// already checks it for RGB.
#[test]
fn two_component_frames_via_encode_planar_match_the_interleaved_path() {
    let width = 12u16;
    let height = 9u16;
    let count = usize::from(width) * usize::from(height);
    let gray: Vec<u8> = (0..count).map(|i| (i % 251) as u8).collect();
    let alpha: Vec<u8> = (0..count).map(|i| (i % 97) as u8).collect();
    let mut interleaved = Vec::with_capacity(count * 2);
    for index in 0..count {
        interleaved.extend_from_slice(&[gray[index], alpha[index]]);
    }

    let options = EncodeOptions {
        quality: 80,
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let expected =
        encode_to_vec_with_options(&interleaved, width, height, InputColor::LumaAlpha, &options)
            .expect("chunky encode");

    let mut out = Vec::new();
    let mut encoder = Encoder::with_options(&mut out, options);
    encoder
        .encode_planar(&[&gray, &alpha], width, height, InputColor::LumaAlpha)
        .expect("planar encode");
    encoder.finish().expect("finish");

    assert_eq!(out, expected);
}

/// Every subsampling ratio must survive every awkward size.
#[test]
fn every_subsampling_ratio_survives_every_shape() {
    let ratios = [
        Subsampling::S444,
        Subsampling::S422,
        Subsampling::S440,
        Subsampling::S420,
        Subsampling::S411,
        Subsampling::Custom([(2, 4), (1, 1), (1, 1), (1, 1)]),
        Subsampling::Custom([(4, 4), (1, 1), (2, 2), (1, 1)]),
    ];
    for &subsampling in &ratios {
        for &(width, height) in &AWKWARD {
            let pixels = source(usize::from(width), usize::from(height), 3);
            let options = EncodeOptions {
                subsampling,
                ..Default::default()
            };
            let jpeg =
                encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
                    .unwrap_or_else(|e| panic!("{subsampling:?} {width}x{height}: {e}"));
            let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
            assert_eq!(decoded.len(), usize::from(width) * usize::from(height) * 3);
        }
    }
}

/// A high-quality 4:4:4 encode must be close to the source, and quality must
/// be monotone in file size.
#[test]
fn quality_controls_fidelity_and_size() {
    let pixels = source(64, 64, 3);
    let mut previous = 0usize;
    for &quality in &[20u8, 50, 75, 90, 100] {
        let options = EncodeOptions {
            quality,
            subsampling: Subsampling::S444,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 64, 64, InputColor::Rgb, &options).expect("encode");
        assert!(
            jpeg.len() > previous,
            "quality {quality} did not grow the file"
        );
        previous = jpeg.len();
    }

    // Fidelity is measured on a smooth source: the pattern above is a
    // deliberately hostile one (a 240/24 checkerboard at the block scale),
    // and its ringing at quality 90 is what `cjpeg` produces too — our
    // output is byte-identical to it, so the error is the source's, not the
    // encoder's.
    let smooth: Vec<u8> = (0..64 * 64 * 3)
        .map(|i| {
            let pixel = i / 3;
            let (x, y) = (pixel % 64, pixel / 64);
            ((x * 2 + y) % 256) as u8
        })
        .collect();
    for &(quality, bound) in &[(90u8, 4.0f64), (100, 0.5)] {
        let options = EncodeOptions {
            quality,
            subsampling: Subsampling::S444,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&smooth, 64, 64, InputColor::Rgb, &options).expect("encode");
        let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
        let error = mean_squared_error(&decoded, &smooth);
        assert!(error < bound, "quality {quality} error {error}");
    }
}

/// Lossless coding must be exact at every predictor, every point transform
/// that fits, and a spread of precisions.
#[test]
fn lossless_is_exact_at_every_precision() {
    for &precision in &[2u8, 4, 8, 12, 16] {
        let maxval = (1u32 << precision) - 1;
        let count = 23 * 17;
        let pixels: Vec<u16> = (0..count)
            .map(|i| ((i * 37) as u32 % (maxval + 1)) as u16)
            .collect();
        for predictor in 1..=7u8 {
            let options = EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor,
                    point_transform: 0,
                },
                precision,
                ..Default::default()
            };
            let jpeg = encode_u16_to_vec_with_options(&pixels, 23, 17, InputColor::Luma, &options)
                .expect("encode");
            let mut decoder = Decoder::new(&jpeg[..]);
            let info = decoder.read_info().expect("info");
            assert_eq!(info.precision, precision);
            assert_eq!(info.process, CodingProcess::Lossless);
            let decoded = decoder.decode_u16().expect("decode");
            assert_eq!(decoded, pixels, "P{precision} psv{predictor}");
        }
    }
}

/// A point transform drops low bits, so the round trip is exact only after
/// the same shift; that is what `Pt` means.
#[test]
fn a_lossless_point_transform_drops_exactly_its_low_bits() {
    let pixels: Vec<u16> = (0..(16 * 16)).map(|i| ((i * 41) % 256) as u16).collect();
    for point_transform in 0..=3u8 {
        let options = EncodeOptions {
            process: EncodeProcess::Lossless {
                predictor: 4,
                point_transform,
            },
            precision: 8,
            ..Default::default()
        };
        let jpeg = encode_u16_to_vec_with_options(&pixels, 16, 16, InputColor::Luma, &options)
            .expect("encode");
        let decoded = Decoder::new(&jpeg[..]).decode_u16().expect("decode");
        for (index, (&got, &want)) in decoded.iter().zip(pixels.iter()).enumerate() {
            assert_eq!(
                got >> point_transform,
                want >> point_transform,
                "Pt {point_transform} sample {index}"
            );
        }
    }
}

/// Restart markers must appear where the options ask for them, cycle
/// `RST0..RST7` and survive a decode.
#[test]
fn restart_markers_cycle_and_decode() {
    let pixels = source(80, 48, 3);
    for restart in [
        RestartInterval::Mcus(1),
        RestartInterval::Mcus(2),
        RestartInterval::Mcus(7),
        RestartInterval::McuRows(1),
        RestartInterval::McuRows(2),
    ] {
        let options = EncodeOptions {
            restart_interval: restart,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 80, 48, InputColor::Rgb, &options).expect("encode");
        let mut seen = Vec::new();
        let mut index = 0usize;
        while index + 1 < jpeg.len() {
            if jpeg[index] == 0xFF && (0xD0..=0xD7).contains(&jpeg[index + 1]) {
                seen.push(jpeg[index + 1] - 0xD0);
            }
            index += 1;
        }
        assert!(!seen.is_empty(), "{restart:?} produced no restart markers");
        for (position, &code) in seen.iter().enumerate() {
            assert_eq!(code as usize, position % 8, "{restart:?} broke the cycle");
        }
        Decoder::new(&jpeg[..]).decode().expect("decode");
    }
}

/// A progressive frame with a custom script must decode to the same pixels as
/// the sequential frame built from the same coefficients.
#[test]
fn a_custom_progressive_script_decodes_like_the_default_one() {
    let pixels = source(40, 24, 3);
    let script = vec![
        ScanSpec::dc(vec![0, 1, 2], 0, 0),
        ScanSpec::ac(0, 1, 63, 0, 0),
        ScanSpec::ac(1, 1, 63, 0, 0),
        ScanSpec::ac(2, 1, 63, 0, 0),
    ];
    let custom = EncodeOptions {
        process: EncodeProcess::Progressive,
        progressive_script: Some(script),
        quality: 85,
        ..Default::default()
    };
    let sequential = EncodeOptions {
        quality: 85,
        optimize_huffman: true,
        ..Default::default()
    };
    let a = encode_to_vec_with_options(&pixels, 40, 24, InputColor::Rgb, &custom).expect("a");
    let b = encode_to_vec_with_options(&pixels, 40, 24, InputColor::Rgb, &sequential).expect("b");
    let da = Decoder::new(&a[..]).decode().expect("decode a");
    let db = Decoder::new(&b[..]).decode().expect("decode b");
    assert_eq!(da, db, "a single-pass progressive script is lossless-equal");
}

/// Every rejected setting must be an error, never a panic.
#[test]
fn invalid_settings_are_errors() {
    let pixels = vec![0u8; 64 * 3];
    let cases: Vec<(&str, EncodeOptions)> = vec![
        (
            "precision 10",
            EncodeOptions {
                precision: 10,
                ..Default::default()
            },
        ),
        (
            "baseline with 16-bit quantisers",
            EncodeOptions {
                force_baseline: true,
                quant_tables: QuantTableSource::Custom(Box::new([
                    Some(QuantTable::from_natural([300; 64])),
                    None,
                    None,
                    None,
                ])),
                ..Default::default()
            },
        ),
        (
            "predictor 0",
            EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 0,
                    point_transform: 0,
                },
                ..Default::default()
            },
        ),
        (
            "point transform too large",
            EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 1,
                    point_transform: 8,
                },
                precision: 8,
                ..Default::default()
            },
        ),
        (
            "non-dividing sampling factors",
            EncodeOptions {
                subsampling: Subsampling::Custom([(3, 1), (2, 1), (1, 1), (1, 1)]),
                ..Default::default()
            },
        ),
        (
            "cmyk ids on a three-component frame",
            EncodeOptions {
                component_ids: ComponentIds::Cmyk,
                ..Default::default()
            },
        ),
        (
            "an empty progressive script",
            EncodeOptions {
                process: EncodeProcess::Progressive,
                progressive_script: Some(Vec::new()),
                ..Default::default()
            },
        ),
        (
            "an AC scan before any DC scan",
            EncodeOptions {
                process: EncodeProcess::Progressive,
                progressive_script: Some(vec![ScanSpec::ac(0, 1, 63, 0, 0)]),
                ..Default::default()
            },
        ),
        (
            "an unreachable colour conversion",
            EncodeOptions {
                jpeg_color_space: Some(ColorSpace::Cmyk),
                ..Default::default()
            },
        ),
    ];
    for (label, options) in cases {
        let result = encode_to_vec_with_options(&pixels, 8, 8, InputColor::Rgb, &options);
        assert!(result.is_err(), "{label} should be rejected");
    }

    assert!(matches!(
        encode_to_vec(&[0u8; 5], 2, 1, InputColor::Rgb, 75),
        Err(JpegError::BufferTooSmall { .. })
    ));
    assert!(encode_to_vec(&[0u8; 3], 0, 1, InputColor::Rgb, 75).is_err());
    assert!(encode_to_vec(&[0u8; 3], 1, 0, InputColor::Rgb, 75).is_err());
}

/// The abbreviated pair must decode against each other, at every colour space
/// TIFF uses, and the tables must be reusable across strips.
#[test]
fn tiff_abbreviated_mode_round_trips_for_every_photometric() {
    let cases = [
        (InputColor::Rgb, None, 3usize),
        (InputColor::Rgb, Some(ColorSpace::Rgb), 3),
        (InputColor::Luma, None, 1),
        (InputColor::Cmyk, None, 4),
        (InputColor::Cmyk, Some(ColorSpace::Ycck), 4),
        (InputColor::Ycck, None, 4),
    ];
    for &(input, colour, channels) in &cases {
        let options = EncodeOptions {
            jpeg_color_space: colour,
            ..EncodeOptions::tiff_strip(75)
        };
        let tables = table_set(&options, input).expect("tables");
        let blob = tables.emit(TablesMode::BOTH);
        let parsed = TableSet::parse(&blob).expect("parse");
        assert_eq!(parsed, tables, "JPEGTables must round-trip");

        // Three strips of different heights, as a TIFF's last strip is short.
        for &height in &[16u16, 16, 5] {
            let width = 32u16;
            let pixels = source(usize::from(width), usize::from(height), channels);
            let mut strip = Vec::new();
            let mut encoder = Encoder::with_options(&mut strip, options.clone());
            encoder
                .encode_scan_only(&pixels, width, height, input)
                .expect("strip");
            encoder.finish().expect("finish");
            assert_eq!(&strip[..4], &[0xFF, 0xD8, 0xFF, 0xC0]);

            let mut out = vec![0u8; usize::from(width) * usize::from(height) * channels];
            let info =
                decode_abbreviated_into(Some(&parsed), &strip, &DecodeOptions::raw(), &mut out)
                    .expect("decode");
            assert_eq!((info.width, info.height), (width, height));
            assert_eq!(usize::from(info.num_components), channels);
        }
    }
}

/// A twelve-bit abbreviated strip must go through the `u16` entry points.
#[test]
fn twelve_bit_abbreviated_strips_decode() {
    let pixels: Vec<u16> = (0..(32 * 16)).map(|i| ((i * 37) % 4096) as u16).collect();
    let options = EncodeOptions {
        precision: 12,
        optimize_huffman: false,
        ..EncodeOptions::tiff_strip(75)
    };
    // Twelve bits force generated tables, so they cannot be shared; the strip
    // has to be self-contained.
    assert!(table_set(&options, InputColor::Luma).is_err());
    let jpeg = encode_u16_to_vec_with_options(&pixels, 32, 16, InputColor::Luma, &options)
        .expect("encode");
    let mut out = vec![0u16; 32 * 16];
    let info =
        decode_abbreviated_into_u16(None, &jpeg, &DecodeOptions::raw(), &mut out).expect("decode");
    assert_eq!(info.precision, 12);
}

/// The marker policies must do what they say.
#[test]
fn marker_policies_control_app0_and_app14() {
    let pixels = vec![128u8; 8 * 8 * 3];
    let combinations = [
        (MarkerPolicy::Auto, MarkerPolicy::Auto, true, false),
        (MarkerPolicy::Never, MarkerPolicy::Auto, false, false),
        (MarkerPolicy::Auto, MarkerPolicy::Always, true, true),
        (MarkerPolicy::Never, MarkerPolicy::Never, false, false),
    ];
    for &(jfif, adobe, expect_jfif, expect_adobe) in &combinations {
        let options = EncodeOptions {
            write_jfif: jfif,
            write_adobe: adobe,
            ..Default::default()
        };
        let jpeg =
            encode_to_vec_with_options(&pixels, 8, 8, InputColor::Rgb, &options).expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        assert_eq!(info.has_jfif, expect_jfif, "{jfif:?}");
        assert_eq!(info.has_adobe, expect_adobe, "{adobe:?}");
        if expect_adobe {
            assert_eq!(decoder.adobe().map(|a| a.transform), Some(1), "YCbCr");
        }
    }
}

/// A CMYK frame written with an Adobe marker must come back inverted, and one
/// without must not. This is the rule TIFF depends on.
#[test]
fn the_adobe_marker_drives_cmyk_inversion() {
    let pixels = vec![40u8; 8 * 8 * 4];
    let without = EncodeOptions {
        write_adobe: MarkerPolicy::Never,
        ..Default::default()
    };
    let jpeg =
        encode_to_vec_with_options(&pixels, 8, 8, InputColor::Cmyk, &without).expect("encode");
    let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
    assert!(
        decoded.iter().all(|&v| v.abs_diff(40) <= 2),
        "no Adobe marker means no inversion"
    );

    let with = EncodeOptions::default();
    let jpeg = encode_to_vec_with_options(&pixels, 8, 8, InputColor::Cmyk, &with).expect("encode");
    let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
    assert!(
        decoded.iter().all(|&v| v.abs_diff(215) <= 2),
        "an Adobe marker turns the inversion on"
    );
}

/// Downsampling modes must all produce decodable output.
#[test]
fn every_downsampling_mode_decodes() {
    let pixels = source(37, 21, 3);
    for downsampling in [
        Downsampling::Box,
        Downsampling::Smooth(1),
        Downsampling::Smooth(50),
        Downsampling::Smooth(100),
    ] {
        for subsampling in [Subsampling::S444, Subsampling::S420, Subsampling::S411] {
            let options = EncodeOptions {
                downsampling,
                subsampling,
                ..Default::default()
            };
            let jpeg = encode_to_vec_with_options(&pixels, 37, 21, InputColor::Rgb, &options)
                .expect("encode");
            let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
            assert_eq!(decoded.len(), 37 * 21 * 3);
        }
    }
}

/// Every byte we write must be decodable, and the decode must agree with a
/// second decode of the same bytes: a cheap determinism check.
#[test]
fn encoding_is_deterministic() {
    let pixels = source(29, 31, 3);
    for &process in &[EncodeProcess::Sequential, EncodeProcess::Progressive] {
        let options = EncodeOptions {
            process,
            ..Default::default()
        };
        let first =
            encode_to_vec_with_options(&pixels, 29, 31, InputColor::Rgb, &options).expect("a");
        let second =
            encode_to_vec_with_options(&pixels, 29, 31, InputColor::Rgb, &options).expect("b");
        assert_eq!(first, second, "{process:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any image of any small size must encode and decode to the right shape.
    #[test]
    fn arbitrary_images_round_trip(
        width in 1u16..=24,
        height in 1u16..=24,
        quality in 1u8..=100,
        channel in 0usize..3,
        bytes in prop::collection::vec(any::<u8>(), 1..(24 * 24 * 4)),
    ) {
        let colours = [InputColor::Luma, InputColor::Rgb, InputColor::Cmyk];
        let color = colours[channel];
        let count = usize::from(width) * usize::from(height) * color.channels();
        let mut pixels = bytes;
        pixels.resize(count, 0);

        let options = EncodeOptions { quality, ..Default::default() };
        let jpeg = encode_to_vec_with_options(&pixels, width, height, color, &options)
            .expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        prop_assert_eq!((info.width, info.height), (width, height));
        let decoded = decoder.decode().expect("decode");
        prop_assert_eq!(decoded.len(), count);
    }

    /// Lossless coding must be exact for any input at all.
    #[test]
    fn arbitrary_images_are_lossless_when_asked(
        width in 1u16..=20,
        height in 1u16..=20,
        predictor in 1u8..=7,
        bytes in prop::collection::vec(any::<u8>(), 1..(20 * 20 * 3)),
    ) {
        let count = usize::from(width) * usize::from(height) * 3;
        let mut pixels = bytes;
        pixels.resize(count, 0);
        let options = EncodeOptions {
            process: EncodeProcess::Lossless { predictor, point_transform: 0 },
            ..Default::default()
        };
        let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
            .expect("encode");
        let decoded = Decoder::new(&jpeg[..]).decode().expect("decode");
        prop_assert_eq!(decoded, pixels);
    }

    /// Progressive and sequential encodings of the same coefficients must
    /// decode to the same pixels.
    #[test]
    fn progressive_and_sequential_agree(
        width in 1u16..=18,
        height in 1u16..=18,
        bytes in prop::collection::vec(any::<u8>(), 1..(18 * 18 * 3)),
    ) {
        let count = usize::from(width) * usize::from(height) * 3;
        let mut pixels = bytes;
        pixels.resize(count, 0);
        let script = vec![
            ScanSpec::dc(vec![0, 1, 2], 0, 0),
            ScanSpec::ac(0, 1, 63, 0, 0),
            ScanSpec::ac(1, 1, 63, 0, 0),
            ScanSpec::ac(2, 1, 63, 0, 0),
        ];
        let progressive = EncodeOptions {
            process: EncodeProcess::Progressive,
            progressive_script: Some(script),
            ..Default::default()
        };
        let sequential = EncodeOptions::default();
        let a = encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &progressive)
            .expect("a");
        let b = encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &sequential)
            .expect("b");
        prop_assert_eq!(
            Decoder::new(&a[..]).decode().expect("decode a"),
            Decoder::new(&b[..]).decode().expect("decode b")
        );
    }
}

/// A caller-supplied table parsed from a `Pq = 1` `DQT` keeps that flag even
/// when every value fits in a byte, and `emit_dqt` honours it — so the frame
/// header must honour it too. T.81 B.2.4.1 forbids `Pq = 1` at eight-bit
/// precision, and a `SOF0` paired with a 16-bit `DQT` is exactly that.
#[test]
fn a_sixteen_bit_dqt_flag_forces_an_extended_frame_header() {
    // A tables-only stream whose single DQT is `Pq = 1` with small values.
    let mut blob = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x83, 0x10];
    for _ in 0..64 {
        blob.extend_from_slice(&16u16.to_be_bytes());
    }
    blob.extend_from_slice(&[0xFF, 0xD9]);
    let parsed = TableSet::parse(&blob).expect("parse");
    let table = parsed.quant[0].expect("table 0");
    assert_eq!(
        table.precision_flag(),
        1,
        "the Pq flag must survive parsing"
    );
    assert_eq!(
        table.required_precision(),
        0,
        "…while the values do not need it"
    );

    let source = QuantTableSource::Custom(Box::new([Some(table), None, None, None]));
    let pixels = vec![0u8; 16 * 16];
    let jpeg = encode_to_vec_with_options(
        &pixels,
        16,
        16,
        InputColor::Luma,
        &EncodeOptions {
            quant_tables: source.clone(),
            force_baseline: false,
            ..Default::default()
        },
    )
    .expect("encode");
    let dqt = jpeg
        .windows(2)
        .position(|pair| pair == [0xFF, 0xDB])
        .expect("DQT");
    assert_eq!(jpeg[dqt + 4] >> 4, 1, "the DQT is written with Pq = 1");
    assert!(
        jpeg.windows(2).any(|pair| pair == [0xFF, 0xC1]),
        "so the frame must be SOF1, not SOF0"
    );
    assert!(!jpeg.windows(2).any(|pair| pair == [0xFF, 0xC0]));

    // And `force_baseline` must reject it rather than downgrade it.
    let refused = encode_to_vec_with_options(
        &pixels,
        16,
        16,
        InputColor::Luma,
        &EncodeOptions {
            quant_tables: source,
            force_baseline: true,
            ..Default::default()
        },
    );
    assert!(
        matches!(
            refused,
            Err(JpegError::InvalidEncodeParameter {
                parameter: "quant_tables",
                ..
            })
        ),
        "force_baseline must refuse a Pq = 1 table, got {refused:?}"
    );
}

/// `encode_scan_only` may not emit `DHT` segments, because an abbreviated
/// stream promises its tables live in the container. Progressive, twelve-bit
/// and lossless frames all turn table generation on *after* the options are
/// read, so the guard has to test the plan and not the options.
#[test]
fn abbreviated_scans_refuse_every_process_that_generates_tables() {
    let pixels = vec![0u8; 32 * 16 * 3];
    let cases: [(&str, EncodeOptions); 4] = [
        (
            "explicitly optimized",
            EncodeOptions {
                optimize_huffman: true,
                ..EncodeOptions::tiff_strip(75)
            },
        ),
        (
            "progressive",
            EncodeOptions {
                process: EncodeProcess::Progressive,
                ..EncodeOptions::tiff_strip(75)
            },
        ),
        (
            "twelve-bit",
            EncodeOptions {
                precision: 12,
                ..EncodeOptions::tiff_strip(75)
            },
        ),
        (
            "lossless",
            EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 1,
                    point_transform: 0,
                },
                ..EncodeOptions::tiff_strip(75)
            },
        ),
    ];
    for (name, options) in cases {
        // Matching the variant, not just `is_err()`: a bare error check would
        // stay green if one of these configurations were rejected for some
        // unrelated reason, and would then prove nothing about the guard.
        // `table_set` names the option the caller set — `optimize_huffman`
        // when they asked for it outright, `process` when the process
        // implies it — so the invariant to assert is the variant and the
        // reason, not one particular parameter name.
        let refused = table_set(&options, InputColor::Rgb);
        let Err(JpegError::InvalidEncodeParameter { parameter, reason }) = refused else {
            panic!("{name}: table_set must refuse with InvalidEncodeParameter, got {refused:?}");
        };
        assert!(
            matches!(parameter, "optimize_huffman" | "process")
                && (reason.contains("Huffman") || reason.contains("generated tables")),
            "{name}: table_set refused for the wrong reason: {parameter} / {reason}"
        );
        let mut strip = Vec::new();
        let mut encoder = Encoder::with_options(&mut strip, options);
        let result = encoder.encode_scan_only(&pixels, 32, 16, InputColor::Rgb);
        assert!(
            matches!(
                result,
                Err(JpegError::InvalidEncodeParameter {
                    parameter: "optimize_huffman",
                    ..
                })
            ),
            "{name}: encode_scan_only must refuse for the same reason, got {result:?}"
        );
        assert!(
            !strip.windows(2).any(|pair| pair == [0xFF, 0xC4]),
            "{name}: no DHT may reach an abbreviated stream"
        );
    }
}

/// `Subsampling::Custom` is the only way a two-component frame ever leaves
/// `1x1`, and it reaches it through the same `build_plan` arm every other
/// colour space uses. The plan-level check lives in `encoder/plan.rs`; this
/// is the end-to-end half — the frame really codes, the decoder really
/// upsamples the decimated component back to full resolution, and a smooth
/// source survives the round trip within the decimation's own error.
#[test]
fn two_component_frames_survive_custom_subsampling_end_to_end() {
    let (width, height) = (32usize, 24usize);
    let mut gradient = Vec::with_capacity(width * height * 2);
    for y in 0..height {
        for x in 0..width {
            gradient.push((x * 4) as u8);
            gradient.push((y * 4) as u8);
        }
    }
    for factors in [
        [(1u8, 1u8), (1, 1), (1, 1), (1, 1)],
        [(2, 1), (1, 1), (1, 1), (1, 1)],
        [(1, 2), (1, 1), (1, 1), (1, 1)],
        [(2, 2), (1, 1), (1, 1), (1, 1)],
        // Both components decimated equally is not decimation at all: hmax
        // and vmax rise with them, so every component stays full size.
        [(2, 2), (2, 2), (1, 1), (1, 1)],
        // The *second* component full-rate and the first decimated is just
        // as legal here as the other way round, since neither is "luma".
        [(1, 1), (2, 2), (1, 1), (1, 1)],
    ] {
        let options = EncodeOptions {
            quality: 95,
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            subsampling: Subsampling::Custom(factors),
            ..Default::default()
        };
        let jpeg = encode_to_vec_with_options(
            &gradient,
            width as u16,
            height as u16,
            InputColor::LumaAlpha,
            &options,
        )
        .unwrap_or_else(|e| panic!("{factors:?}: {e}"));

        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("info");
        assert_eq!(info.num_components, 2, "{factors:?}");
        let frame = decoder.frame_header().expect("frame header");
        let coded: Vec<(u8, u8)> = frame.components.iter().map(|c| (c.h, c.v)).collect();
        assert_eq!(coded, vec![factors[0], factors[1]], "{factors:?}");

        let decoded = decoder.decode().expect("decode");
        assert_eq!(decoded.len(), gradient.len(), "{factors:?}");
        let max_diff = decoded
            .iter()
            .zip(gradient.iter())
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(255);
        assert!(
            max_diff <= 8,
            "{factors:?}: a linear ramp came back with max_diff {max_diff}, which is \
             more than decimating and re-interpolating it can explain"
        );
    }
}

/// Restart intervals at two components, including a frame large enough for
/// the `rayon` feature's parallel scan splitter to engage (it needs at least
/// 256 MCUs and a non-zero interval, which no other two-component test
/// reaches). The split is byte-identical to the serial coder's by
/// construction, so the check here is that the frame codes and decodes at
/// all — verified separately to be bit-identical with and without the
/// feature.
#[test]
fn two_component_frames_survive_every_restart_interval() {
    let cases: [(u16, u16, RestartInterval); 8] = [
        (37, 23, RestartInterval::None),
        (37, 23, RestartInterval::Mcus(1)),
        (37, 23, RestartInterval::Mcus(3)),
        (37, 23, RestartInterval::McuRows(1)),
        (37, 23, RestartInterval::McuRows(2)),
        (37, 23, RestartInterval::Mcus(u16::MAX)),
        // 32x16 = 512 MCUs: past the parallel splitter's threshold.
        (256, 128, RestartInterval::Mcus(8)),
        (256, 128, RestartInterval::McuRows(1)),
    ];
    for (width, height, restart) in cases {
        for process in [EncodeProcess::Sequential, EncodeProcess::Progressive] {
            let pixels = source(usize::from(width), usize::from(height), 2);
            let options = EncodeOptions {
                quality: 90,
                process,
                restart_interval: restart,
                jpeg_color_space: Some(ColorSpace::Unknown(2)),
                ..Default::default()
            };
            let jpeg =
                encode_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
                    .unwrap_or_else(|e| panic!("{restart:?}/{process:?}: {e}"));
            let mut decoder = Decoder::new(&jpeg[..]);
            let info = decoder.read_info().expect("info");
            assert_eq!(info.num_components, 2);
            let decoded = decoder.decode().expect("decode");
            assert_eq!(decoded.len(), pixels.len());
            let max_diff = decoded
                .iter()
                .zip(pixels.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(255);
            assert!(
                max_diff < 64,
                "{width}x{height} {restart:?}/{process:?}: max_diff {max_diff}"
            );
        }
    }
}

/// Twelve-bit two-component frames: `SOF1` with a two-component template,
/// generated Huffman tables (Annex K's stop short of the categories a
/// twelve-bit sample reaches) and the `u16` sample path through
/// `convert_rows_two`.
#[test]
fn two_component_frames_round_trip_at_twelve_bits() {
    let (width, height) = (17u16, 19u16);
    let count = usize::from(width) * usize::from(height) * 2;
    let pixels: Vec<u16> = (0..count).map(|i| ((i * 53) % 4096) as u16).collect();
    let options = EncodeOptions {
        precision: 12,
        quality: 95,
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let jpeg =
        encode_u16_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
            .expect("twelve-bit encode");
    let mut decoder = Decoder::new(&jpeg[..]);
    let info = decoder.read_info().expect("info");
    assert_eq!(info.num_components, 2);
    assert_eq!(info.precision, 12);
    assert_eq!(info.output_color_space, ColorSpace::Unknown(2));
    let decoded = decoder.decode_u16().expect("decode");
    assert_eq!(decoded.len(), pixels.len());
}

/// [`Encoder::encode_planar`] takes one slice per *component*, and a
/// two-component frame needs exactly two. Handing it one, three or none must
/// be a named error rather than a panic or a frame built from whatever the
/// slice list happened to contain.
#[test]
fn encode_planar_refuses_a_wrong_plane_count_at_two_components() {
    let gray = [7u8; 12 * 9];
    let alpha = [9u8; 12 * 9];
    let wrong: [Vec<&[u8]>; 3] = [
        vec![&gray[..]],
        vec![&gray[..], &alpha[..], &gray[..]],
        Vec::new(),
    ];
    for planes in wrong {
        let options = EncodeOptions {
            jpeg_color_space: Some(ColorSpace::Unknown(2)),
            ..Default::default()
        };
        let mut out = Vec::new();
        let mut encoder = Encoder::with_options(&mut out, options);
        let error = encoder
            .encode_planar(&planes, 12, 9, InputColor::LumaAlpha)
            .expect_err("a wrong plane count must be refused");
        assert!(
            matches!(
                error,
                JpegError::InvalidEncodeParameter { .. } | JpegError::BufferTooSmall { .. }
            ),
            "{} planes: unexpected error {error}",
            planes.len()
        );
    }
}

/// Now that a two-component frame can be *written*, a decode of one can also
/// be asked for an `output_color_space` this crate has no transform for.
/// Every such request must come back as a named error: never a panic, and
/// never a short or over-long output buffer that a caller sized from
/// [`ImageInfo::output_components`].
#[test]
fn two_component_decode_refuses_transforms_it_cannot_perform() {
    let (width, height) = (24u16, 16u16);
    let pixels = source(usize::from(width), usize::from(height), 2);
    let options = EncodeOptions {
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
        .expect("encode");

    for target in [
        ColorSpace::Luma,
        ColorSpace::Ycbcr,
        ColorSpace::Rgb,
        ColorSpace::Cmyk,
        ColorSpace::Ycck,
        ColorSpace::Unknown(0),
        ColorSpace::Unknown(3),
        ColorSpace::Unknown(255),
    ] {
        let decode_options = DecodeOptions {
            output_color_space: Some(target),
            ..Default::default()
        };
        let mut decoder = Decoder::with_options(&jpeg[..], decode_options);
        decoder.read_info().expect("info");
        let error = decoder
            .decode()
            .expect_err("no transform exists out of a two-component frame");
        assert!(
            matches!(error, JpegError::Unsupported(_)),
            "{target:?}: unexpected error {error}"
        );
    }

    // The identity is the one that works, and it produces exactly two
    // samples per pixel.
    let decode_options = DecodeOptions {
        output_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let mut decoder = Decoder::with_options(&jpeg[..], decode_options);
    let info = decoder.read_info().expect("info");
    assert_eq!(info.output_components(), 2);
    assert_eq!(decoder.decode().expect("decode").len(), pixels.len());
}

/// A truncated or corrupted two-component stream must always come back as an
/// error, never a panic. Every prefix of a real frame, and every single-bit
/// and high-bit flip across its header region, is tried.
#[test]
fn two_component_streams_never_panic_when_damaged() {
    let (width, height) = (24u16, 20u16);
    let pixels = source(usize::from(width), usize::from(height), 2);
    let options = EncodeOptions {
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
        .expect("encode");

    for cut in 0..jpeg.len() {
        let mut decoder = Decoder::new(&jpeg[..cut]);
        if decoder.read_info().is_ok() {
            let _ = decoder.decode();
        }
    }
    for offset in 0..jpeg.len().min(256) {
        for delta in [1u8, 0x80, 0xff] {
            let mut corrupt = jpeg.clone();
            corrupt[offset] ^= delta;
            let mut decoder = Decoder::new(&corrupt[..]);
            if decoder.read_info().is_ok() {
                let _ = decoder.decode();
            }
        }
    }
}

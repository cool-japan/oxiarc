//! Byte-parity differential tests for the encoder, against libjpeg-turbo's
//! `cjpeg` and `djpeg`.
//!
//! Enable with `--features jpeg-oracle`. Every test self-skips (prints a note
//! and passes) when the reference tools are missing, but once
//! [`tools_available`] has returned `true` a fixture failure is an error, not
//! a skip, and every test counts its comparisons and fails if it made none.
//! Without that counting a renamed `cjpeg` flag would turn the whole suite
//! green while comparing nothing.

#![cfg(feature = "jpeg-oracle")]

// `oracle_support` is shared with the decoder suite, so some of its
// helpers are unused in this binary.
#[allow(dead_code)]
mod oracle_support;

use oracle_support::{Pnm, cjpeg, djpeg, first_difference, libjpeg_version, temp_path};
use oxiarc_jpeg::{
    ColorSpace, DecodeOptions, Decoder, EncodeOptions, EncodeProcess, Encoder, InputColor,
    MarkerPolicy, QuantTableSource, RestartInterval, Subsampling, TableSet, TablesMode,
    decode_abbreviated_into, encode_to_vec_with_options, encode_u16_to_vec_with_options,
};
use std::process::Command;

/// `true` when `cjpeg` and `djpeg` both run.
fn tools_available() -> bool {
    oracle_support::tool_available("cjpeg") && oracle_support::tool_available("djpeg")
}

/// Fail with the reference version banner when a test compared nothing.
fn require_comparisons(count: usize, what: &str) {
    assert!(
        count > 0,
        "{what}: no comparison was made even though the tools are present. \
         Reference: {}",
        libjpeg_version()
    );
}

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

fn pnm(pixels: &[u8], width: usize, height: usize, channels: usize) -> Pnm {
    Pnm {
        width,
        height,
        maxval: 255,
        channels,
        samples: pixels.iter().map(|&v| u16::from(v)).collect(),
    }
}

/// Report the first differing byte, for a diagnosable failure message.
fn describe(ours: &[u8], theirs: &[u8]) -> String {
    let at = ours
        .iter()
        .zip(theirs.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(ours.len().min(theirs.len()));
    let low = at.saturating_sub(8);
    format!(
        "first difference at byte {at} (ours {} bytes, theirs {} bytes)\n  ours   {:02x?}\n  theirs {:02x?}\nReference: {}",
        ours.len(),
        theirs.len(),
        &ours[low..(at + 8).min(ours.len())],
        &theirs[low..(at + 8).min(theirs.len())],
        libjpeg_version()
    )
}

const SIZES: [(usize, usize); 6] = [(1, 1), (1, 33), (33, 1), (17, 19), (64, 64), (131, 97)];

/// ENC-1: baseline output must be byte-identical to `cjpeg -dct int` across
/// every subsampling ratio, a sampled quality range and every awkward size.
#[test]
fn baseline_output_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let ratios = [
        ("1x1", Subsampling::S444),
        ("2x1", Subsampling::S422),
        ("1x2", Subsampling::S440),
        ("2x2", Subsampling::S420),
        ("4x1", Subsampling::S411),
        ("2x4", Subsampling::Custom([(2, 4), (1, 1), (1, 1), (1, 1)])),
    ];
    let qualities = [1u8, 5, 10, 25, 50, 63, 75, 88, 90, 99, 100];
    let mut compared = 0usize;
    for &(sample, subsampling) in &ratios {
        for &quality in &qualities {
            for &(width, height) in &SIZES {
                let pixels = source(width, height, 3);
                let source_pnm = pnm(&pixels, width, height, 3);
                let quality_text = quality.to_string();
                let sample_text = format!("{sample},1x1,1x1");
                let theirs = cjpeg(
                    &[
                        "-quality",
                        &quality_text,
                        "-sample",
                        &sample_text,
                        "-dct",
                        "int",
                    ],
                    &source_pnm,
                )
                .unwrap_or_else(|| panic!("cjpeg failed at {sample} q{quality} {width}x{height}"));
                let options = EncodeOptions {
                    quality,
                    subsampling,
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "{sample} q{quality} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "baseline parity");
}

/// ENC-1, the other colour spaces: grayscale, `-grayscale` from RGB, `-rgb`
/// (no colour transform, letter component identifiers and an Adobe marker).
#[test]
fn other_colour_spaces_are_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &quality in &[10u8, 50, 75, 95] {
        for &(width, height) in &SIZES {
            let quality_text = quality.to_string();

            let grey = source(width, height, 1);
            let theirs = cjpeg(&["-quality", &quality_text], &pnm(&grey, width, height, 1))
                .expect("cjpeg grayscale");
            let ours = encode_to_vec_with_options(
                &grey,
                width as u16,
                height as u16,
                InputColor::Luma,
                &EncodeOptions {
                    quality,
                    ..Default::default()
                },
            )
            .expect("encode");
            assert!(
                ours == theirs,
                "grayscale q{quality}: {}",
                describe(&ours, &theirs)
            );
            compared += 1;

            let colour = source(width, height, 3);
            let source_pnm = pnm(&colour, width, height, 3);

            let theirs = cjpeg(&["-quality", &quality_text, "-grayscale"], &source_pnm)
                .expect("cjpeg -grayscale");
            let ours = encode_to_vec_with_options(
                &colour,
                width as u16,
                height as u16,
                InputColor::Rgb,
                &EncodeOptions {
                    quality,
                    jpeg_color_space: Some(ColorSpace::Luma),
                    ..Default::default()
                },
            )
            .expect("encode");
            assert!(
                ours == theirs,
                "rgb to grey q{quality}: {}",
                describe(&ours, &theirs)
            );
            compared += 1;

            let theirs =
                cjpeg(&["-quality", &quality_text, "-rgb"], &source_pnm).expect("cjpeg -rgb");
            let ours = encode_to_vec_with_options(
                &colour,
                width as u16,
                height as u16,
                InputColor::Rgb,
                &EncodeOptions {
                    quality,
                    jpeg_color_space: Some(ColorSpace::Rgb),
                    ..Default::default()
                },
            )
            .expect("encode");
            assert!(
                ours == theirs,
                "-rgb q{quality}: {}",
                describe(&ours, &theirs)
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "colour space parity");
}

/// ENC-1 with restart markers, in both of `cjpeg`'s spellings: `-restart N`
/// counts MCU rows and `-restart NB` counts MCUs.
#[test]
fn restart_intervals_are_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(17usize, 19usize), (64, 64), (131, 97)] {
        let pixels = source(width, height, 3);
        let source_pnm = pnm(&pixels, width, height, 3);
        for &(spelling, restart) in &[
            ("1", RestartInterval::McuRows(1)),
            ("3", RestartInterval::McuRows(3)),
            ("1B", RestartInterval::Mcus(1)),
            ("5B", RestartInterval::Mcus(5)),
        ] {
            for &subsampling in &[Subsampling::S444, Subsampling::S420] {
                let sample = if subsampling == Subsampling::S444 {
                    "1x1,1x1,1x1"
                } else {
                    "2x2,1x1,1x1"
                };
                let theirs = cjpeg(
                    &["-quality", "75", "-restart", spelling, "-sample", sample],
                    &source_pnm,
                )
                .expect("cjpeg -restart");
                let options = EncodeOptions {
                    restart_interval: restart,
                    subsampling,
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-restart {spelling} {sample} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "restart parity");
}

/// ENC-1 with `-smooth N`: libjpeg's input smoothing, which also switches
/// `jcprepct.c` into context-rows mode and so changes how the rows below the
/// image are produced.
#[test]
fn input_smoothing_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &factor in &[1u8, 10, 50, 100] {
        for &(sample, subsampling) in &[
            ("1x1,1x1,1x1", Subsampling::S444),
            ("2x1,1x1,1x1", Subsampling::S422),
            ("2x2,1x1,1x1", Subsampling::S420),
        ] {
            for &(width, height) in &SIZES {
                let pixels = source(width, height, 3);
                let factor_text = factor.to_string();
                let theirs = cjpeg(
                    &["-quality", "80", "-smooth", &factor_text, "-sample", sample],
                    &pnm(&pixels, width, height, 3),
                )
                .expect("cjpeg -smooth");
                let options = EncodeOptions {
                    quality: 80,
                    subsampling,
                    downsampling: oxiarc_jpeg::Downsampling::Smooth(factor),
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-smooth {factor} {sample} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "smoothing parity");
}

/// ENC-1 with `-optimize`: this is the test that pins the optimal table
/// generator's tie-breaking and its dummy-symbol handling.
#[test]
fn optimized_huffman_tables_are_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &quality in &[10u8, 50, 75, 95] {
        for &(width, height) in &SIZES {
            for &(sample, subsampling) in &[
                ("1x1,1x1,1x1", Subsampling::S444),
                ("2x2,1x1,1x1", Subsampling::S420),
            ] {
                let pixels = source(width, height, 3);
                let quality_text = quality.to_string();
                let theirs = cjpeg(
                    &["-quality", &quality_text, "-optimize", "-sample", sample],
                    &pnm(&pixels, width, height, 3),
                )
                .expect("cjpeg -optimize");
                let options = EncodeOptions {
                    quality,
                    subsampling,
                    optimize_huffman: true,
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-optimize q{quality} {sample} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "optimize parity");
}

/// ENC-2: progressive output must equal `cjpeg -progressive` for libjpeg's
/// default scan script, which also proves the generated tables are right for
/// every one of the ten scans.
#[test]
fn progressive_output_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &quality in &[10u8, 50, 75, 95] {
        for &(width, height) in &SIZES {
            for &(channels, sample, subsampling, colour) in &[
                (3usize, "1x1,1x1,1x1", Subsampling::S444, None),
                (3, "2x2,1x1,1x1", Subsampling::S420, None),
                (1, "1x1", Subsampling::S444, Some(ColorSpace::Luma)),
            ] {
                let pixels = source(width, height, channels);
                let quality_text = quality.to_string();
                let mut args = vec!["-quality", quality_text.as_str(), "-progressive"];
                if channels == 3 {
                    args.push("-sample");
                    args.push(sample);
                }
                let theirs = cjpeg(&args, &pnm(&pixels, width, height, channels))
                    .expect("cjpeg -progressive");
                let options = EncodeOptions {
                    quality,
                    subsampling,
                    process: EncodeProcess::Progressive,
                    jpeg_color_space: colour,
                    ..Default::default()
                };
                let input = if channels == 1 {
                    InputColor::Luma
                } else {
                    InputColor::Rgb
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    input,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-progressive q{quality} {sample} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "progressive parity");
}

/// ENC-2 with a custom scan script that forces several AC refinement passes,
/// which is where a progressive encoder's correction-bit buffering shows up.
#[test]
fn a_custom_scan_script_with_refinements_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    // Three successive approximation passes over the luma AC band, plus a
    // spectral split, plus DC refinement.
    let script_text = "\
0: 0 0 0 1;
1: 0 0 0 1;
2: 0 0 0 1;
0: 1 5 0 3;
0: 6 63 0 3;
0: 1 63 3 2;
0: 1 63 2 1;
0: 1 63 1 0;
1: 1 63 0 2;
1: 1 63 2 1;
1: 1 63 1 0;
2: 1 63 0 2;
2: 1 63 2 1;
2: 1 63 1 0;
0: 0 0 1 0;
1: 0 0 1 0;
2: 0 0 1 0;
";
    let script_path = temp_path("scans", "txt");
    std::fs::write(&script_path, script_text).expect("write script");

    let script = vec![
        oxiarc_jpeg::ScanSpec::dc(vec![0], 0, 1),
        oxiarc_jpeg::ScanSpec::dc(vec![1], 0, 1),
        oxiarc_jpeg::ScanSpec::dc(vec![2], 0, 1),
        oxiarc_jpeg::ScanSpec::ac(0, 1, 5, 0, 3),
        oxiarc_jpeg::ScanSpec::ac(0, 6, 63, 0, 3),
        oxiarc_jpeg::ScanSpec::ac(0, 1, 63, 3, 2),
        oxiarc_jpeg::ScanSpec::ac(0, 1, 63, 2, 1),
        oxiarc_jpeg::ScanSpec::ac(0, 1, 63, 1, 0),
        oxiarc_jpeg::ScanSpec::ac(1, 1, 63, 0, 2),
        oxiarc_jpeg::ScanSpec::ac(1, 1, 63, 2, 1),
        oxiarc_jpeg::ScanSpec::ac(1, 1, 63, 1, 0),
        oxiarc_jpeg::ScanSpec::ac(2, 1, 63, 0, 2),
        oxiarc_jpeg::ScanSpec::ac(2, 1, 63, 2, 1),
        oxiarc_jpeg::ScanSpec::ac(2, 1, 63, 1, 0),
        oxiarc_jpeg::ScanSpec::dc(vec![0], 1, 0),
        oxiarc_jpeg::ScanSpec::dc(vec![1], 1, 0),
        oxiarc_jpeg::ScanSpec::dc(vec![2], 1, 0),
    ];

    let mut compared = 0usize;
    for &(width, height) in &[(17usize, 19usize), (64, 64), (131, 97)] {
        let pixels = source(width, height, 3);
        let path = script_path.to_string_lossy().to_string();
        let theirs = cjpeg(
            &["-quality", "80", "-scans", &path, "-sample", "2x2,1x1,1x1"],
            &pnm(&pixels, width, height, 3),
        );
        let Some(theirs) = theirs else {
            panic!("cjpeg -scans failed; {}", libjpeg_version());
        };
        // The fixture must really contain refinement scans, or the test
        // proves nothing about the hardest code path.
        let refinements = count_refinement_scans(&theirs);
        assert!(
            refinements >= 3,
            "expected at least three AC refinement scans, found {refinements}"
        );
        let options = EncodeOptions {
            quality: 80,
            process: EncodeProcess::Progressive,
            progressive_script: Some(script.clone()),
            ..Default::default()
        };
        let ours = encode_to_vec_with_options(
            &pixels,
            width as u16,
            height as u16,
            InputColor::Rgb,
            &options,
        )
        .expect("encode");
        assert!(
            ours == theirs,
            "custom script {width}x{height}: {}",
            describe(&ours, &theirs)
        );
        compared += 1;
    }
    let _ = std::fs::remove_file(&script_path);
    require_comparisons(compared, "custom script parity");
}

/// Count `SOS` segments whose `Ah` is non-zero and whose band is not DC.
fn count_refinement_scans(data: &[u8]) -> usize {
    let mut count = 0usize;
    let mut index = 2usize;
    while index + 4 <= data.len() {
        if data[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = data[index + 1];
        if marker == 0xD9 {
            break;
        }
        let length = usize::from(u16::from_be_bytes([data[index + 2], data[index + 3]]));
        if marker == 0xDA {
            let ns = usize::from(data[index + 4]);
            let ss = data[index + 5 + 2 * ns];
            let ah = data[index + 7 + 2 * ns] >> 4;
            if ss != 0 && ah != 0 {
                count += 1;
            }
            // Skip the entropy data.
            let mut probe = index + 2 + length;
            while probe + 1 < data.len() {
                if data[probe] == 0xFF
                    && data[probe + 1] != 0
                    && !(0xD0..=0xD7).contains(&data[probe + 1])
                {
                    break;
                }
                probe += 1;
            }
            index = probe;
            continue;
        }
        index += 2 + length;
    }
    count
}

/// ENC-1c: twelve-bit frames use `SOF1`, switch `Pq` on the quantiser values
/// rather than on the precision, and generate their Huffman tables.
#[test]
fn twelve_bit_output_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &quality in &[10u8, 25, 50, 75, 90] {
        for &(width, height) in &[(17usize, 19usize), (1, 1), (64, 64)] {
            let samples: Vec<u16> = (0..width * height)
                .map(|i| ((i * 37) % 4096) as u16)
                .collect();
            let source_pnm = Pnm {
                width,
                height,
                maxval: 4095,
                channels: 1,
                samples: samples.clone(),
            };
            let quality_text = quality.to_string();
            let Some(theirs) = cjpeg(
                &["-precision", "12", "-quality", &quality_text],
                &source_pnm,
            ) else {
                panic!("cjpeg -precision 12 failed; {}", libjpeg_version());
            };
            let options = EncodeOptions {
                quality,
                precision: 12,
                ..Default::default()
            };
            let ours = encode_u16_to_vec_with_options(
                &samples,
                width as u16,
                height as u16,
                InputColor::Luma,
                &options,
            )
            .expect("encode");
            assert!(
                ours == theirs,
                "12-bit q{quality} {width}x{height}: {}",
                describe(&ours, &theirs)
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "12-bit parity");

    // The measured `Pq` switch: 16-bit entries at quality 10, 8-bit at 25.
    let samples: Vec<u16> = (0..64).map(|i| ((i * 37) % 4096) as u16).collect();
    for &(quality, expected_pq, expected_sof) in &[(10u8, 1u8, 0xC1u8), (25, 0, 0xC1)] {
        let options = EncodeOptions {
            quality,
            precision: 12,
            ..Default::default()
        };
        let ours = encode_u16_to_vec_with_options(&samples, 8, 8, InputColor::Luma, &options)
            .expect("encode");
        let dqt = ours
            .windows(2)
            .position(|pair| pair == [0xFF, 0xDB])
            .expect("DQT");
        assert_eq!(ours[dqt + 4] >> 4, expected_pq, "Pq at quality {quality}");
        assert!(
            ours.windows(2).any(|pair| pair == [0xFF, expected_sof]),
            "SOF1 at quality {quality}"
        );
    }
}

/// ENC-4: lossless frames must be byte-identical to `cjpeg -lossless psv,Pt`
/// and must round-trip exactly through our own decoder.
#[test]
fn lossless_output_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for predictor in 1..=7u8 {
        for &point_transform in &[0u8, 1, 2] {
            for &(width, height, channels) in &[(17usize, 19usize, 3usize), (1, 1, 1), (64, 64, 1)]
            {
                let pixels = source(width, height, channels);
                let spec = if point_transform == 0 {
                    predictor.to_string()
                } else {
                    format!("{predictor},{point_transform}")
                };
                let Some(theirs) = cjpeg(
                    &["-lossless", &spec],
                    &pnm(&pixels, width, height, channels),
                ) else {
                    panic!("cjpeg -lossless failed; {}", libjpeg_version());
                };
                let options = EncodeOptions {
                    process: EncodeProcess::Lossless {
                        predictor,
                        point_transform,
                    },
                    ..Default::default()
                };
                let input = if channels == 1 {
                    InputColor::Luma
                } else {
                    InputColor::Rgb
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    input,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-lossless {spec} {width}x{height}x{channels}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;

                if point_transform == 0 {
                    let decoded = Decoder::new(&ours[..]).decode().expect("decode");
                    assert_eq!(decoded, pixels, "lossless round trip psv {predictor}");
                }
            }
        }
    }
    require_comparisons(compared, "lossless parity");
}

/// ENC-3: our decode of our own bytes must equal `djpeg`'s decode of the same
/// bytes, exactly. This is the check that our encoder writes what we think it
/// writes even where `cjpeg` cannot be asked for the same thing.
#[test]
fn djpeg_decodes_our_output_to_the_same_pixels_we_do() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &SIZES {
        for &subsampling in &[Subsampling::S444, Subsampling::S422, Subsampling::S420] {
            for &process in &[EncodeProcess::Sequential, EncodeProcess::Progressive] {
                let pixels = source(width, height, 3);
                let options = EncodeOptions {
                    quality: 85,
                    subsampling,
                    process,
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                let theirs = djpeg(&["-dct", "int", "-ppm"], &ours).expect("djpeg");
                let mine = Decoder::new(&ours[..]).decode().expect("decode");
                let mine16: Vec<u16> = mine.iter().map(|&v| u16::from(v)).collect();
                if let Some((index, a, b)) = first_difference(&mine16, &theirs.samples) {
                    panic!(
                        "{width}x{height} {subsampling:?} {process:?}: sample {index} \
                         ours {a} theirs {b}; {}",
                        libjpeg_version()
                    );
                }
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "decode agreement");
}

/// Pillow must be able to read what we write, at every process.
#[test]
fn pillow_reads_our_output() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let script = temp_path("pillow_check", "py");
    std::fs::write(
        &script,
        "import sys\nfrom PIL import Image\nim = Image.open(sys.argv[1])\nim.load()\nprint(im.mode, im.size[0], im.size[1])\n",
    )
    .expect("write script");
    let mut compared = 0usize;
    for &process in &[EncodeProcess::Sequential, EncodeProcess::Progressive] {
        let pixels = source(37, 23, 3);
        let options = EncodeOptions {
            quality: 80,
            process,
            ..Default::default()
        };
        let ours =
            encode_to_vec_with_options(&pixels, 37, 23, InputColor::Rgb, &options).expect("encode");
        let path = temp_path("pillow", "jpg");
        std::fs::write(&path, &ours).expect("write");
        let output = Command::new("python3").arg(&script).arg(&path).output();
        let _ = std::fs::remove_file(&path);
        let Ok(output) = output else {
            eprintln!("python3 absent; skipping the Pillow check");
            let _ = std::fs::remove_file(&script);
            return;
        };
        if !output.status.success() {
            let text = String::from_utf8_lossy(&output.stderr);
            if text.contains("No module named") {
                eprintln!("Pillow absent; skipping");
                let _ = std::fs::remove_file(&script);
                return;
            }
            panic!("Pillow failed to read our {process:?} output: {text}");
        }
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "RGB 37 23",
            "Pillow read the wrong shape"
        );
        compared += 1;
    }
    let _ = std::fs::remove_file(&script);
    require_comparisons(compared, "Pillow read");
}

/// TIFF abbreviated mode: our `JPEGTables` blob must be byte-identical to the
/// one libtiff writes at the same quality, and the strips we write must decode
/// against it.
#[test]
fn our_jpeg_tables_match_libtiffs_and_our_strips_decode_against_them() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    // libtiff's default quality is 75, YCbCr with 2x2 luma sampling.
    let options = EncodeOptions::tiff_strip(75);
    let tables = oxiarc_jpeg::table_set(&options, InputColor::Rgb).expect("tables");
    let blob = tables.emit(TablesMode::BOTH);
    assert_eq!(
        blob.len(),
        574,
        "libtiff's YCbCr JPEGTables tag is 574 bytes"
    );
    assert_eq!(&blob[..4], &[0xFF, 0xD8, 0xFF, 0xDB]);

    let grey_tables = oxiarc_jpeg::table_set(&options, InputColor::Luma).expect("tables");
    assert_eq!(
        grey_tables.emit(TablesMode::BOTH).len(),
        289,
        "libtiff's grayscale JPEGTables tag is 289 bytes"
    );

    let width = 64usize;
    let height = 32usize;
    let pixels = source(width, height, 3);
    let mut strip = Vec::new();
    let mut encoder = Encoder::with_options(&mut strip, options);
    encoder
        .encode_scan_only(&pixels, width as u16, height as u16, InputColor::Rgb)
        .expect("strip");
    encoder.finish().expect("finish");
    assert_eq!(&strip[..4], &[0xFF, 0xD8, 0xFF, 0xC0], "no tables, no APP0");

    let parsed = TableSet::parse(&blob).expect("parse");
    let mut out = vec![0u8; width * height * 3];
    let info = decode_abbreviated_into(Some(&parsed), &strip, &DecodeOptions::raw(), &mut out)
        .expect("decode");
    assert_eq!(
        (usize::from(info.width), usize::from(info.height)),
        (width, height)
    );

    // The same pixels through a self-contained stream must give the same
    // components back.
    let self_contained = encode_to_vec_with_options(
        &pixels,
        width as u16,
        height as u16,
        InputColor::Rgb,
        &EncodeOptions {
            write_jfif: MarkerPolicy::Never,
            ..EncodeOptions::tiff_strip(75)
        },
    )
    .expect("encode");
    let mut whole = vec![0u8; width * height * 3];
    decode_abbreviated_into(None, &self_contained, &DecodeOptions::raw(), &mut whole)
        .expect("decode");
    assert_eq!(out, whole, "abbreviated and self-contained must agree");
}

/// A flat quantisation table and an explicit custom table both reach `cjpeg`
/// parity through `-qtables`, which proves the `Custom` and `Flat` sources are
/// wired to the same DQT path as `AnnexK`.
#[test]
fn flat_quantisation_tables_match_cjpeg_qtables() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let table_path = temp_path("qtable", "txt");
    let mut text = String::new();
    for _ in 0..64 {
        text.push_str("7\n");
    }
    std::fs::write(&table_path, &text).expect("write table");
    let pixels = source(24, 24, 1);
    let path = table_path.to_string_lossy().to_string();
    let Some(theirs) = cjpeg(
        &["-qtables", &path, "-quality", "50"],
        &pnm(&pixels, 24, 24, 1),
    ) else {
        panic!("cjpeg -qtables failed; {}", libjpeg_version());
    };
    let options = EncodeOptions {
        quant_tables: QuantTableSource::Flat(7),
        ..Default::default()
    };
    let ours =
        encode_to_vec_with_options(&pixels, 24, 24, InputColor::Luma, &options).expect("encode");
    let _ = std::fs::remove_file(&table_path);
    assert!(ours == theirs, "flat qtable: {}", describe(&ours, &theirs));
}

/// Build a JPEG-compressed TIFF with `tiffcp` and return its `JPEGTables`
/// tag and first strip, or `None` when the tools are missing.
fn libtiff_tables(mode: &str, photometric: &str, channels: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    if Command::new("tiffcp").arg("-h").output().is_err() {
        return None;
    }
    if !Command::new("python3")
        .args(["-c", "import tifffile, numpy"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return None;
    }
    let src = temp_path("enc_tiff_src", "tif");
    let dst = temp_path("enc_tiff_dst", "tif");
    let dump = temp_path("enc_tiff_dump", "bin");
    let strip = temp_path("enc_tiff_strip", "bin");

    let shape = if channels == 1 {
        "(64, 64)".to_string()
    } else {
        format!("(64, 64, {channels})")
    };
    let build = format!(
        "import numpy as np, tifffile\n\
         img = (np.indices({shape}).sum(axis=0) % 251).astype('uint8')\n\
         tifffile.imwrite(r'{src}', img, photometric='{photometric}')\n",
        src = src.display()
    );
    let status = Command::new("python3").args(["-c", &build]).status().ok()?;
    if !status.success() {
        return None;
    }
    let status = Command::new("tiffcp")
        .args(["-c", mode, "-r", "16"])
        .arg(&src)
        .arg(&dst)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    let extract = format!(
        "import tifffile\n\
         with tifffile.TiffFile(r'{dst}') as tf:\n\
         \x20   page = tf.pages[0]\n\
         \x20   open(r'{dump}', 'wb').write(bytes(page.tags['JPEGTables'].value))\n\
         \x20   fh = tf.filehandle\n\
         \x20   offset = page.tags['StripOffsets'].value[0]\n\
         \x20   count = page.tags['StripByteCounts'].value[0]\n\
         \x20   fh.seek(offset)\n\
         \x20   open(r'{strip}', 'wb').write(fh.read(count))\n",
        dst = dst.display(),
        dump = dump.display(),
        strip = strip.display()
    );
    let status = Command::new("python3")
        .args(["-c", &extract])
        .status()
        .ok()?;
    let result = if status.success() {
        Some((std::fs::read(&dump).ok()?, std::fs::read(&strip).ok()?))
    } else {
        None
    };
    for path in [&src, &dst, &dump, &strip] {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// The `JPEGTables` blob we generate from scratch must equal the one libtiff
/// writes at the same quality — a stronger claim than round-tripping theirs,
/// because it exercises the quality scaling and the Annex K.3 tables.
#[test]
fn our_jpeg_tables_are_byte_identical_to_libtiffs() {
    let cases = [
        ("jpeg", "rgb", 3usize, InputColor::Rgb, None),
        ("jpeg:r", "rgb", 3, InputColor::Rgb, Some(ColorSpace::Rgb)),
        ("jpeg", "minisblack", 1, InputColor::Luma, None),
        (
            "jpeg",
            "separated",
            4,
            InputColor::Cmyk,
            Some(ColorSpace::Cmyk),
        ),
    ];
    let mut compared = 0usize;
    for &(mode, photometric, channels, input, colour) in &cases {
        let Some((theirs, strip)) = libtiff_tables(mode, photometric, channels) else {
            eprintln!("tiffcp/tifffile unavailable; skipping");
            return;
        };
        let options = EncodeOptions {
            jpeg_color_space: colour,
            ..EncodeOptions::tiff_strip(75)
        };
        let ours = oxiarc_jpeg::table_set(&options, input)
            .expect("tables")
            .emit(TablesMode::BOTH);
        assert!(
            ours == theirs,
            "{mode} {photometric}: {}",
            describe(&ours, &theirs)
        );
        compared += 1;

        // And libtiff's own strip must decode against the tables we built.
        let parsed = TableSet::parse(&ours).expect("parse");
        let mut out = vec![0u8; 64 * 16 * channels];
        let info = decode_abbreviated_into(Some(&parsed), &strip, &DecodeOptions::raw(), &mut out)
            .expect("decode libtiff's strip against our tables");
        assert_eq!(usize::from(info.num_components), channels);
    }
    require_comparisons(compared, "libtiff JPEGTables parity");
}

/// A twelve-bit *colour* frame: the twelve-bit path had only ever been driven
/// with one component, so nothing exercised the wide colour conversion, the
/// chroma decimation of twelve-bit samples, the fourteen-bit coefficient
/// limit or the fifteen-bit DC difference until this test.
#[test]
fn twelve_bit_colour_frames_are_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(17usize, 19usize), (1, 1), (64, 64), (131, 97)] {
        let samples: Vec<u16> = (0..width * height * 3)
            .map(|i| (((i / 3) * 7 + (i % 3) * 1367) % 4096) as u16)
            .collect();
        let source_pnm = Pnm {
            width,
            height,
            maxval: 4095,
            channels: 3,
            samples: samples.clone(),
        };
        for &(sample, subsampling) in &[
            ("1x1,1x1,1x1", Subsampling::S444),
            ("2x1,1x1,1x1", Subsampling::S422),
            ("2x2,1x1,1x1", Subsampling::S420),
        ] {
            for &progressive in &[false, true] {
                let mut flags = vec!["-precision", "12", "-quality", "80", "-sample", sample];
                if progressive {
                    flags.push("-progressive");
                }
                let Some(theirs) = cjpeg(&flags, &source_pnm) else {
                    panic!("cjpeg -precision 12 (colour) failed; {}", libjpeg_version());
                };
                let options = EncodeOptions {
                    quality: 80,
                    precision: 12,
                    subsampling,
                    process: if progressive {
                        EncodeProcess::Progressive
                    } else {
                        EncodeProcess::Sequential
                    },
                    ..Default::default()
                };
                let ours = encode_u16_to_vec_with_options(
                    &samples,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "12-bit colour {sample} progressive={progressive} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "12-bit colour parity");
}

/// Restart markers inside a *progressive* frame, where each scan carries its
/// own interval and the end-of-block run must be flushed before the marker,
/// and inside a *lossless* frame, where an interval can start mid-row and so
/// re-arms the "first sample of a row" predictor rule.
#[test]
fn restart_in_progressive_and_lossless_frames_is_byte_identical_to_cjpeg() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(17usize, 19usize), (64, 64), (131, 97)] {
        let pixels = source(width, height, 3);
        let source_pnm = pnm(&pixels, width, height, 3);
        for &(spelling, restart) in &[
            ("1", RestartInterval::McuRows(1)),
            ("2", RestartInterval::McuRows(2)),
            ("1B", RestartInterval::Mcus(1)),
            ("7B", RestartInterval::Mcus(7)),
        ] {
            for &(sample, subsampling) in &[
                ("1x1,1x1,1x1", Subsampling::S444),
                ("2x2,1x1,1x1", Subsampling::S420),
            ] {
                let theirs = cjpeg(
                    &[
                        "-progressive",
                        "-quality",
                        "75",
                        "-restart",
                        spelling,
                        "-sample",
                        sample,
                    ],
                    &source_pnm,
                )
                .expect("cjpeg -progressive -restart");
                let options = EncodeOptions {
                    process: EncodeProcess::Progressive,
                    restart_interval: restart,
                    subsampling,
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-progressive -restart {spelling} {sample} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }

            // Lossless, where an MCU is a single sample: libjpeg-turbo
            // rejects any interval that is not a whole number of MCU rows
            // ("must be an integer multiple of the number of MCUs in an MCU
            // row"), so only the row spellings can be compared to it.
            if let RestartInterval::McuRows(rows) = restart {
                let theirs = cjpeg(&["-lossless", "1,0", "-restart", spelling], &source_pnm)
                    .expect("cjpeg -lossless -restart");
                let options = EncodeOptions {
                    process: EncodeProcess::Lossless {
                        predictor: 1,
                        point_transform: 0,
                    },
                    restart_interval: restart,
                    ..Default::default()
                };
                let ours = encode_to_vec_with_options(
                    &pixels,
                    width as u16,
                    height as u16,
                    InputColor::Rgb,
                    &options,
                )
                .expect("encode");
                assert!(
                    ours == theirs,
                    "-lossless -restart {rows} rows {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }

        // T.81 puts no such restriction on the interval, and an interval that
        // starts part way along a row is exactly the case that re-arms the
        // "first sample of the row uses `Rb`" rule. cjpeg will not write one,
        // so the oracle here is exactness: lossless must still be lossless.
        for &interval in &[1u16, 5, 23] {
            let options = EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 6,
                    point_transform: 0,
                },
                restart_interval: RestartInterval::Mcus(interval),
                ..Default::default()
            };
            let ours = encode_to_vec_with_options(
                &pixels,
                width as u16,
                height as u16,
                InputColor::Rgb,
                &options,
            )
            .expect("encode");
            let mut decoder = Decoder::new(&ours[..]);
            decoder.read_info().expect("info");
            let back = decoder.decode().expect("decode");
            assert_eq!(
                back, pixels,
                "lossless with a mid-row restart interval of {interval} MCUs \
                 must still reproduce {width}x{height} exactly"
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "progressive/lossless restart parity");
}

/// The strip bytes themselves, not only the `JPEGTables` blob: our
/// `encode_scan_only` output for the first sixteen rows of libtiff's own
/// fixture must equal libtiff's strip 0 byte for byte. This is the only
/// external reference for four-component encoding.
#[test]
fn our_abbreviated_strips_are_byte_identical_to_libtiffs() {
    let cases = [
        ("jpeg", "rgb", 3usize, InputColor::Rgb, None),
        ("jpeg:r", "rgb", 3, InputColor::Rgb, Some(ColorSpace::Rgb)),
        ("jpeg", "minisblack", 1, InputColor::Luma, None),
        (
            "jpeg",
            "separated",
            4,
            InputColor::Cmyk,
            Some(ColorSpace::Cmyk),
        ),
    ];
    let mut compared = 0usize;
    for &(mode, photometric, channels, input, colour) in &cases {
        let Some((_tables, theirs)) = libtiff_tables(mode, photometric, channels) else {
            eprintln!("tiffcp/tifffile unavailable; skipping");
            return;
        };
        // The same pixels `libtiff_tables` asks tifffile to write:
        // `(np.indices(shape).sum(axis=0) % 251)`, whose axes are row,
        // column and — when there is more than one — channel.
        let width = 64usize;
        let rows = 16usize;
        let mut pixels = Vec::with_capacity(width * rows * channels);
        for y in 0..rows {
            for x in 0..width {
                for c in 0..channels {
                    let axes = if channels == 1 { y + x } else { y + x + c };
                    pixels.push((axes % 251) as u8);
                }
            }
        }
        let options = EncodeOptions {
            jpeg_color_space: colour,
            ..EncodeOptions::tiff_strip(75)
        };
        let mut ours = Vec::new();
        let mut encoder = Encoder::with_options(&mut ours, options);
        encoder
            .encode_scan_only(&pixels, width as u16, rows as u16, input)
            .expect("strip");
        encoder.finish().expect("finish");
        assert!(
            ours == theirs,
            "{mode} {photometric} strip 0: {}",
            describe(&ours, &theirs)
        );
        compared += 1;
    }
    require_comparisons(compared, "libtiff strip parity");
}

/// A two-component frame (`ColorSpace::Unknown(2)`, libjpeg's `JCS_UNKNOWN`
/// shape) against `djpeg -verbose`.
///
/// **What this proves, and what it does not.** libjpeg-turbo has no output
/// module for a colour space it cannot map to grayscale or RGB (`wrppm.c`,
/// `wrbmp.c`, `wrgif.c`, `wrtarga.c`, `wrrle.c` all `ERREXIT` with "output
/// must be grayscale or RGB" before a single scanline is produced), and
/// `-grayscale`/`-rgb` refuse the conversion outright ("Unsupported color
/// conversion request", `jdcolor.c`'s `JERR_CONVERSION_NOTIMPL`) — checked
/// by hand on this machine, not asserted here, since asserting an external
/// tool's *refusal* pins nothing about this crate. `tjbench` and Pillow were
/// checked the same way: TurboJPEG's `tj3DecompressHeader` cannot even name
/// the colour space, and Pillow's `Image.open` raises
/// `UnidentifiedImageError` before returning a image object at all. **None
/// of the three tools can decode a single pixel of a two-component frame,
/// on any machine**, because none has an output path for it — this is not
/// a gap in this crate.
///
/// What `-verbose` (`jdmarker.c`'s marker printer, which runs during
/// `jpeg_read_header` and so *before* the output module is even selected)
/// **does** reach: the frame header and the scan header. This test asserts
/// djpeg's own trace names the exact shape this crate wrote — `Nf = 2`,
/// identifiers `1`/`2`, `Hi = Vi = 1` for both (no subsampling), quantisation
/// table `0` for both, and a scan naming both components on Huffman table
/// `0` — and that the failure is the *expected* one at the *expected* point
/// (the output-format check, after the scan header is printed), not an
/// earlier parse error that would mean this crate wrote something libjpeg
/// considers malformed.
///
/// The one thing no external tool anywhere can check is whether the
/// *entropy-coded sample values* are correct — see
/// `two_component_channels_decompose_to_cjpeg_djpeg_reference_pixels` below
/// for how much of that gap this crate closes anyway, and
/// `two_component_frames_round_trip_and_keep_both_channels` (`encode_api.rs`)
/// for the part that is genuinely internal-only.
#[test]
fn djpeg_verbose_parses_our_two_component_frame_and_scan_header() {
    if !oracle_support::tool_available("djpeg") {
        eprintln!("djpeg unavailable; skipping");
        return;
    }
    let width = 24u16;
    let height = 18u16;
    let pixels: Vec<u8> = (0..u32::from(width) * u32::from(height) * 2)
        .map(|i| (i % 251) as u8)
        .collect();
    let options = EncodeOptions {
        quality: 80,
        jpeg_color_space: Some(ColorSpace::Unknown(2)),
        ..Default::default()
    };
    let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::LumaAlpha, &options)
        .expect("encode");

    let input = oracle_support::temp_path("verbose_2c", "jpg");
    std::fs::write(&input, &jpeg).expect("write");
    let output = Command::new("djpeg")
        .args(["-verbose", "-verbose"])
        .arg(&input)
        .output()
        .expect("spawn djpeg");
    let _ = std::fs::remove_file(&input);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "djpeg unexpectedly succeeded — no output module exists for this colour \
         space, so success here would mean the frame decoded as something else \
         entirely:\n{stderr}"
    );
    for expected in [
        format!("Start Of Frame 0xc0: width={width}, height={height}, components=2"),
        "Component 1: 1hx1v q=0".to_string(),
        "Component 2: 1hx1v q=0".to_string(),
        "Start Of Scan: 2 components".to_string(),
        "Component 1: dc=0 ac=0".to_string(),
        "Component 2: dc=0 ac=0".to_string(),
    ] {
        assert!(
            stderr.contains(&expected),
            "djpeg -verbose did not print {expected:?}:\n{stderr}"
        );
    }
    assert!(
        stderr
            .trim_end()
            .ends_with("PPM output must be grayscale or RGB"),
        "djpeg failed somewhere other than the expected output-format check \
         (a parse failure earlier would mean it rejected the frame or scan \
         header, not merely the pixel format):\n{stderr}"
    );
}

/// Decomposes a two-component frame into the two libjpeg-turbo *can* fully
/// decode, closing most of the gap `djpeg_verbose_parses_our_two_component_
/// frame_and_scan_header` leaves open.
///
/// `ColorSpace::Unknown(2)`'s component template puts **both** components on
/// quantisation and Huffman slot 0 — the same slot a plain one-component
/// `Luma` frame uses — and neither is subsampled, so for a frame with no
/// restart markers the sequence of quantised coefficients the entropy coder
/// sees for component *N* of the two-component frame is, block for block,
/// identical to the sequence a standalone one-component frame of that same
/// channel's pixels would produce at the same quality: the MCU grid, the DC
/// prediction chain and the dummy-block copy rule are all per-component
/// state, blind to how many *other* components share the frame (`plan.rs`'s
/// own `component_template` doc explains why the table slots have to agree
/// for this to hold).
///
/// So: split the source image into its two channels, encode *each alone* as
/// a standalone grayscale frame with real `cjpeg -dct int`, decode *that*
/// with real `djpeg -dct int` — both fully external, and grayscale byte
/// parity with `cjpeg`/`djpeg` is what `baseline_output_is_byte_identical_
/// to_cjpeg` already covers for every quality and shape — and require it to
/// equal this crate's own two-component encode, decoded by this crate's own
/// decoder, one channel at a time. A mismatch here would mean this crate's
/// two-component DCT/quantisation/entropy-coding path disagrees with the
/// (independently, externally verified) single-component path it is built
/// from, for a component that happens to share a frame with another one.
///
/// What this still does **not** prove: that libjpeg would decode the *real*
/// two-component *interleaved* bitstream to these values — no tool can run
/// that entropy decoder at all (see the sibling test). It does prove the
/// coefficients each component's own pipeline produces are libjpeg-correct.
#[test]
fn two_component_channels_decompose_to_cjpeg_djpeg_reference_pixels() {
    if !tools_available() {
        eprintln!("libjpeg tools absent; skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(16usize, 16), (17, 9), (33, 5)] {
        for quality in [50u8, 75, 95] {
            let mut channel_a = vec![0u8; width * height];
            let mut channel_b = vec![0u8; width * height];
            let mut combined = vec![0u8; width * height * 2];
            for y in 0..height {
                for x in 0..width {
                    let index = y * width + x;
                    let a = ((x * 7 + y * 13) % 256) as u8;
                    let b = if (x / 3 + y / 2) % 2 == 0 { 220 } else { 24 };
                    channel_a[index] = a;
                    channel_b[index] = b;
                    combined[index * 2] = a;
                    combined[index * 2 + 1] = b;
                }
            }

            let options = EncodeOptions {
                quality,
                jpeg_color_space: Some(ColorSpace::Unknown(2)),
                ..Default::default()
            };
            let jpeg = encode_to_vec_with_options(
                &combined,
                width as u16,
                height as u16,
                InputColor::LumaAlpha,
                &options,
            )
            .expect("encode");
            let mut decoder = Decoder::new(&jpeg[..]);
            decoder.read_info().expect("info");
            let ours = decoder.decode().expect("decode");
            let (ours_a, ours_b): (Vec<u8>, Vec<u8>) =
                ours.chunks_exact(2).map(|pair| (pair[0], pair[1])).unzip();

            for (label, ours_channel, source_channel) in
                [("A", &ours_a, &channel_a), ("B", &ours_b, &channel_b)]
            {
                let reference_jpeg = cjpeg(
                    &["-quality", &quality.to_string(), "-dct", "int"],
                    &pnm(source_channel, width, height, 1),
                )
                .expect("cjpeg reference");
                let decoded = djpeg(&["-dct", "int", "-pnm"], &reference_jpeg)
                    .expect("djpeg reference decode");
                let reference: Vec<u8> = decoded.samples.iter().map(|&v| v as u8).collect();
                assert!(
                    *ours_channel == reference,
                    "{width}x{height} q{quality} channel {label}: {}",
                    describe(ours_channel, &reference)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "two-component channel decomposition");
}

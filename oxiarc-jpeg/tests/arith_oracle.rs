//! Differential tests for arithmetic entropy coding (`SOF9`, `SOF10`).
//!
//! `cjpeg -arithmetic` and `djpeg` are the references. Two invariants are
//! asserted, both byte-exact:
//!
//! * **decode**: our decode of an arithmetic stream equals `djpeg -dct int`'s,
//!   and equals our own decode of the Huffman twin `cjpeg` writes from the
//!   same source — the twin is the sharper of the two, because `-arithmetic`
//!   only changes the entropy coder, so every coefficient is identical by
//!   construction and any difference is in the QM coder alone;
//! * **encode**: our arithmetic stream is byte-identical to `cjpeg`'s.
//!
//! Gated behind `jpeg-oracle`; every test self-skips when the tools are
//! missing, and counts its comparisons so it cannot pass vacuously.
#![cfg(all(feature = "jpeg-oracle", feature = "arithmetic"))]

#[allow(dead_code)]
mod oracle_support;

use oracle_support::{
    cjpeg, djpeg, djpeg_env, first_difference, libjpeg_version, synthetic, tool_available,
};
use oxiarc_jpeg::{
    Decoder, EncodeOptions, EncodeProcess, EntropyCoding, InputColor, RestartInterval, Subsampling,
    encode_to_vec_with_options,
};

/// `true` when both reference binaries are usable.
fn oracle_ready() -> bool {
    tool_available("cjpeg") && tool_available("djpeg")
}

/// `true` when the local `cjpeg` can write arithmetic streams at all.
fn arithmetic_supported() -> bool {
    cjpeg(&["-quality", "75", "-arithmetic"], &synthetic(8, 8, 3, 255)).is_some()
}

/// Decode with this crate at 8-bit precision.
fn decode_u8(jpeg: &[u8]) -> Vec<u16> {
    let mut decoder = Decoder::new(jpeg);
    decoder.read_info().expect("read_info");
    decoder
        .decode()
        .expect("decode")
        .into_iter()
        .map(u16::from)
        .collect()
}

/// Decode with this crate at any precision.
fn decode_wide(jpeg: &[u8]) -> Vec<u16> {
    let mut decoder = Decoder::new(jpeg);
    decoder.read_info().expect("read_info");
    decoder.decode_u16().expect("decode_u16")
}

fn assert_identical(label: &str, ours: &[u16], theirs: &[u16]) {
    if let Some((index, a, b)) = first_difference(ours, theirs) {
        panic!(
            "{label}: sample {index} differs (ours {a}, reference {b}); \
             {} samples vs {}; reference: {}",
            ours.len(),
            theirs.len(),
            libjpeg_version()
        );
    }
}

fn require_comparisons(compared: usize, what: &str) {
    assert!(
        compared > 0,
        "{what}: cjpeg produced no fixture, so the test would pass vacuously ({})",
        libjpeg_version()
    );
}

/// The `SOF` marker code of a datastream, so a test can prove the fixture
/// really is the process it claims to exercise.
fn sof_marker(jpeg: &[u8]) -> Option<u8> {
    let mut i = 2usize;
    while i + 3 < jpeg.len() {
        if jpeg[i] != 0xFF {
            return None;
        }
        let code = jpeg[i + 1];
        if (0xC0..=0xCF).contains(&code) && code != 0xC4 && code != 0xC8 && code != 0xCC {
            return Some(code);
        }
        if code == 0xD8 || code == 0x01 || (0xD0..=0xD7).contains(&code) {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        i += 2 + length;
    }
    None
}

/// Sequential arithmetic (`SOF9`) at every subsampling ratio and quality.
#[test]
fn sequential_arithmetic_decode_matches_djpeg() {
    if !oracle_ready() {
        println!("cjpeg/djpeg not on PATH: skipping");
        return;
    }
    if !arithmetic_supported() {
        println!("this cjpeg cannot write arithmetic streams: skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(16usize, 16usize), (17, 19), (131, 97), (1, 1)] {
        for &sample in &["1x1", "2x1", "2x2", "1x2", "4x1"] {
            for &quality in &[10u8, 50, 75, 95] {
                let source = synthetic(width, height, 3, 255);
                let q = quality.to_string();
                let Some(jpeg) = cjpeg(
                    &[
                        "-quality",
                        &q,
                        "-sample",
                        sample,
                        "-arithmetic",
                        "-dct",
                        "int",
                    ],
                    &source,
                ) else {
                    continue;
                };
                assert_eq!(
                    sof_marker(&jpeg),
                    Some(0xC9),
                    "fixture is not SOF9: {sample} q{quality}"
                );
                let reference = djpeg(&["-dct", "int", "-ppm"], &jpeg).expect("djpeg");
                let ours = decode_u8(&jpeg);
                assert_identical(
                    &format!("sequential arithmetic {width}x{height} {sample} q{quality}"),
                    &ours,
                    &reference.samples,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "sequential arithmetic decode");
}

/// The sharper form of the same check: the arithmetic stream and the Huffman
/// stream `cjpeg` writes from the same source carry identical coefficients,
/// so our two decodes must agree sample for sample. A mismatch is in the QM
/// coder or its conditioning and nowhere else.
#[test]
fn arithmetic_and_huffman_twins_decode_identically() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(16usize, 16usize), (64, 64), (37, 23)] {
        for &sample in &["1x1", "2x2", "2x1"] {
            for &quality in &[25u8, 75, 90] {
                let source = synthetic(width, height, 3, 255);
                let q = quality.to_string();
                let args = ["-quality", &q, "-sample", sample, "-dct", "int"];
                let Some(huffman) = cjpeg(&args, &source) else {
                    continue;
                };
                let mut arith_args = args.to_vec();
                arith_args.push("-arithmetic");
                let Some(arithmetic) = cjpeg(&arith_args, &source) else {
                    continue;
                };
                let ours_huffman = decode_u8(&huffman);
                let ours_arithmetic = decode_u8(&arithmetic);
                assert_identical(
                    &format!("twin {width}x{height} {sample} q{quality}"),
                    &ours_arithmetic,
                    &ours_huffman,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "arithmetic/Huffman twin decode");
}

/// Restart intervals reset the statistics, the predictions and the coder
/// registers; a wrong reset shows up as noise after the first interval.
#[test]
fn arithmetic_restart_intervals_decode() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let mut compared = 0usize;
    for &restart in &["1", "2", "5", "1B"] {
        for &sample in &["1x1", "2x2"] {
            let source = synthetic(64, 48, 3, 255);
            let Some(jpeg) = cjpeg(
                &[
                    "-quality",
                    "80",
                    "-sample",
                    sample,
                    "-restart",
                    restart,
                    "-arithmetic",
                    "-dct",
                    "int",
                ],
                &source,
            ) else {
                continue;
            };
            // `-restart N` counts MCU *rows*, so a large N over a short
            // image legitimately produces no markers at all; that fixture
            // proves nothing and is skipped rather than asserted on.
            if !jpeg
                .windows(2)
                .any(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
            {
                continue;
            }
            let reference = djpeg(&["-dct", "int", "-ppm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg);
            assert_identical(
                &format!("arithmetic restart {restart} {sample}"),
                &ours,
                &reference.samples,
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "arithmetic restart decode");
}

/// Progressive arithmetic (`SOF10`): the default ten-scan script exercises DC
/// first and refinement, AC first, AC refinement and every band split.
#[test]
fn progressive_arithmetic_decode_matches_djpeg() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(16usize, 16usize), (64, 64), (37, 23)] {
        for &sample in &["1x1", "2x2", "2x1"] {
            for &quality in &[40u8, 85] {
                let source = synthetic(width, height, 3, 255);
                let q = quality.to_string();
                let Some(jpeg) = cjpeg(
                    &[
                        "-quality",
                        &q,
                        "-sample",
                        sample,
                        "-progressive",
                        "-arithmetic",
                        "-dct",
                        "int",
                    ],
                    &source,
                ) else {
                    continue;
                };
                assert_eq!(sof_marker(&jpeg), Some(0xCA), "fixture is not SOF10");
                let reference = djpeg(&["-dct", "int", "-ppm"], &jpeg).expect("djpeg");
                let ours = decode_u8(&jpeg);
                assert_identical(
                    &format!("progressive arithmetic {width}x{height} {sample} q{quality}"),
                    &ours,
                    &reference.samples,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "progressive arithmetic decode");
}

/// Grayscale and 12-bit arithmetic, which take different paths through the
/// magnitude chains (12-bit coefficients reach higher categories).
#[test]
fn grayscale_and_twelve_bit_arithmetic_decode() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let mut compared = 0usize;
    for &quality in &[30u8, 75, 95] {
        let q = quality.to_string();
        let gray = synthetic(45, 33, 1, 255);
        if let Some(jpeg) = cjpeg(&["-quality", &q, "-grayscale", "-arithmetic"], &gray) {
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            assert_identical(
                &format!("grayscale arithmetic q{quality}"),
                &decode_u8(&jpeg),
                &reference.samples,
            );
            compared += 1;
        }
        let wide = synthetic(24, 20, 3, 4095);
        if let Some(jpeg) = cjpeg(&["-quality", &q, "-precision", "12", "-arithmetic"], &wide) {
            let reference = djpeg(&["-dct", "int", "-ppm"], &jpeg).expect("djpeg");
            assert_identical(
                &format!("12-bit arithmetic q{quality}"),
                &decode_wide(&jpeg),
                &reference.samples,
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "grayscale/12-bit arithmetic decode");
}

// ── encode parity ─────────────────────────────────────────────────────────

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

fn pnm(pixels: &[u8], width: usize, height: usize, channels: usize) -> oracle_support::Pnm {
    oracle_support::Pnm {
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

/// Sequential arithmetic output must be byte-identical to
/// `cjpeg -arithmetic -dct int`, which pins the QM coder, the conditioning,
/// the `DAC` segment and the flush procedure all at once.
#[test]
fn sequential_arithmetic_encode_is_byte_identical_to_cjpeg() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let ratios = [
        ("1x1", Subsampling::S444),
        ("2x1", Subsampling::S422),
        ("1x2", Subsampling::S440),
        ("2x2", Subsampling::S420),
        ("4x1", Subsampling::S411),
    ];
    let mut compared = 0usize;
    for &(sample, subsampling) in &ratios {
        for &quality in &[1u8, 25, 50, 75, 90, 100] {
            for &(width, height) in &[(1usize, 1usize), (17, 19), (64, 64), (131, 97)] {
                let pixels = source(width, height, 3);
                let quality_text = quality.to_string();
                let sample_text = format!("{sample},1x1,1x1");
                let Some(theirs) = cjpeg(
                    &[
                        "-quality",
                        &quality_text,
                        "-sample",
                        &sample_text,
                        "-arithmetic",
                        "-dct",
                        "int",
                    ],
                    &pnm(&pixels, width, height, 3),
                ) else {
                    continue;
                };
                let options = EncodeOptions {
                    quality,
                    subsampling,
                    entropy: EntropyCoding::Arithmetic,
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
                    "arithmetic {sample} q{quality} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "sequential arithmetic encode parity");
}

/// Restart intervals, grayscale and twelve-bit arithmetic encoding.
#[test]
fn arithmetic_encode_variants_are_byte_identical_to_cjpeg() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let mut compared = 0usize;
    let (width, height) = (64usize, 48usize);
    let pixels = source(width, height, 3);

    for &(flag, interval) in &[
        ("1", RestartInterval::McuRows(1)),
        ("3", RestartInterval::McuRows(3)),
        ("1B", RestartInterval::Mcus(1)),
        ("7B", RestartInterval::Mcus(7)),
    ] {
        let Some(theirs) = cjpeg(
            &[
                "-quality",
                "80",
                "-sample",
                "2x2,1x1,1x1",
                "-restart",
                flag,
                "-arithmetic",
                "-dct",
                "int",
            ],
            &pnm(&pixels, width, height, 3),
        ) else {
            continue;
        };
        let options = EncodeOptions {
            quality: 80,
            subsampling: Subsampling::S420,
            restart_interval: interval,
            entropy: EntropyCoding::Arithmetic,
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
            "arithmetic restart {flag}: {}",
            describe(&ours, &theirs)
        );
        compared += 1;
    }

    // Grayscale.
    let gray = source(45, 33, 1);
    if let Some(theirs) = cjpeg(
        &["-quality", "70", "-grayscale", "-arithmetic", "-dct", "int"],
        &pnm(&gray, 45, 33, 1),
    ) {
        let options = EncodeOptions {
            quality: 70,
            entropy: EntropyCoding::Arithmetic,
            ..Default::default()
        };
        let ours =
            encode_to_vec_with_options(&gray, 45, 33, InputColor::Luma, &options).expect("encode");
        assert!(
            ours == theirs,
            "grayscale arithmetic: {}",
            describe(&ours, &theirs)
        );
        compared += 1;
    }

    // Twelve-bit: the magnitude chains reach categories the eight-bit path
    // never visits.
    let wide: Vec<u16> = source(24, 20, 3)
        .into_iter()
        .map(|v| u16::from(v) * 16 + 7)
        .collect();
    let wide_pnm = oracle_support::Pnm {
        width: 24,
        height: 20,
        maxval: 4095,
        channels: 3,
        samples: wide.clone(),
    };
    if let Some(theirs) = cjpeg(
        &[
            "-quality",
            "85",
            "-precision",
            "12",
            "-sample",
            "2x2,1x1,1x1",
            "-arithmetic",
            "-dct",
            "int",
        ],
        &wide_pnm,
    ) {
        let options = EncodeOptions {
            quality: 85,
            precision: 12,
            subsampling: Subsampling::S420,
            entropy: EntropyCoding::Arithmetic,
            ..Default::default()
        };
        let ours =
            oxiarc_jpeg::encode_u16_to_vec_with_options(&wide, 24, 20, InputColor::Rgb, &options)
                .expect("encode");
        assert!(
            ours == theirs,
            "12-bit arithmetic: {}",
            describe(&ours, &theirs)
        );
        compared += 1;
    }

    require_comparisons(compared, "arithmetic encode variants");
}

/// Progressive arithmetic output, with libjpeg's default scan script.
#[test]
fn progressive_arithmetic_encode_is_byte_identical_to_cjpeg() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let mut compared = 0usize;
    for &(width, height) in &[(17usize, 19usize), (64, 64), (131, 97)] {
        for &sample in &["1x1", "2x2", "2x1"] {
            for &quality in &[35u8, 75, 95] {
                let pixels = source(width, height, 3);
                let quality_text = quality.to_string();
                let sample_text = format!("{sample},1x1,1x1");
                let Some(theirs) = cjpeg(
                    &[
                        "-quality",
                        &quality_text,
                        "-sample",
                        &sample_text,
                        "-progressive",
                        "-arithmetic",
                        "-dct",
                        "int",
                    ],
                    &pnm(&pixels, width, height, 3),
                ) else {
                    continue;
                };
                let subsampling = match sample {
                    "1x1" => Subsampling::S444,
                    "2x1" => Subsampling::S422,
                    _ => Subsampling::S420,
                };
                let options = EncodeOptions {
                    quality,
                    subsampling,
                    process: EncodeProcess::Progressive,
                    entropy: EntropyCoding::Arithmetic,
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
                    "progressive arithmetic {sample} q{quality} {width}x{height}: {}",
                    describe(&ours, &theirs)
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "progressive arithmetic encode parity");
}

/// A restart-interval stream large enough that the `rayon` feature splits it.
///
/// The parallel encoder's claim is byte identity with the serial one; this
/// test makes the stronger claim, byte identity with **libjpeg**, at a size
/// above the splitting threshold. It is in this file rather than in
/// `parallel.rs` because it needs the oracle, and it runs whether or not the
/// feature is enabled — which is the point, since the two configurations must
/// produce the same bytes.
#[test]
fn large_restart_streams_match_cjpeg_in_both_scheduling_modes() {
    if !oracle_ready() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let (width, height) = (320usize, 256usize);
    let pixels = source(width, height, 3);
    let mut compared = 0usize;

    for &(flag, interval) in &[
        ("1", RestartInterval::McuRows(1)),
        ("2", RestartInterval::McuRows(2)),
        ("5B", RestartInterval::Mcus(5)),
    ] {
        for arithmetic in [false, true] {
            if arithmetic && !arithmetic_supported() {
                continue;
            }
            let mut args = vec![
                "-quality",
                "85",
                "-sample",
                "2x2,1x1,1x1",
                "-restart",
                flag,
                "-dct",
                "int",
            ];
            if arithmetic {
                args.push("-arithmetic");
            }
            let Some(theirs) = cjpeg(&args, &pnm(&pixels, width, height, 3)) else {
                continue;
            };
            let options = EncodeOptions {
                quality: 85,
                subsampling: Subsampling::S420,
                restart_interval: interval,
                entropy: if arithmetic {
                    EntropyCoding::Arithmetic
                } else {
                    EntropyCoding::Huffman
                },
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
                "restart {flag} arithmetic={arithmetic}: {}",
                describe(&ours, &theirs)
            );

            // And the decode of that same stream, which is where the parallel
            // decoder engages.
            let reference = djpeg(&["-dct", "int", "-ppm"], &theirs).expect("djpeg");
            assert_identical(
                &format!("large restart {flag} arithmetic={arithmetic}"),
                &decode_u8(&theirs),
                &reference.samples,
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "large restart parity");
}

/// Non-interleaved **sequential** arithmetic scans, one per component.
///
/// `cjpeg -scans` accepts a sequential script, and it is the only way to reach
/// the non-interleaved sequential path — the default encoder always
/// interleaves. Each scan resets its own statistics areas, so a decoder that
/// carried state across scans would fail here and nowhere else.
#[test]
fn non_interleaved_sequential_arithmetic_scans_decode() {
    if !oracle_ready() || !arithmetic_supported() {
        println!("reference tools unavailable: skipping");
        return;
    }
    let path = oracle_support::temp_path("arith_seq_scans", "txt");
    std::fs::write(&path, "0: 0 63 0 0;\n1: 0 63 0 0;\n2: 0 63 0 0;\n").expect("write scan script");
    let script = path.to_string_lossy().to_string();

    let mut compared = 0usize;
    for &(width, height, sample) in &[(64usize, 48usize, "2x2"), (37, 23, "1x1")] {
        let source = synthetic(width, height, 3, 255);
        let Some(jpeg) = cjpeg(
            &[
                "-quality",
                "80",
                "-scans",
                &script,
                "-sample",
                sample,
                "-arithmetic",
                "-dct",
                "int",
            ],
            &source,
        ) else {
            continue;
        };
        assert_eq!(
            sof_marker(&jpeg),
            Some(0xC9),
            "the script must stay sequential"
        );
        assert_eq!(
            jpeg.windows(2)
                .filter(|w| w[0] == 0xFF && w[1] == 0xDA)
                .count(),
            3,
            "the fixture must really carry three scans"
        );
        let reference = djpeg(&["-dct", "int", "-ppm"], &jpeg).expect("djpeg");
        assert_identical(
            &format!("non-interleaved arithmetic {width}x{height} {sample}"),
            &decode_u8(&jpeg),
            &reference.samples,
        );
        compared += 1;
    }
    let _ = std::fs::remove_file(&path);
    require_comparisons(compared, "non-interleaved arithmetic scans");
}

/// Every decode-parity claim in this file rests on `djpeg` producing the same
/// samples whichever kernels it picks. `jpeg_oracle.rs` asserts that once over
/// the Huffman corpus; this is the same assertion over an *arithmetic* stream,
/// so the fixtures added here are covered by it too — libjpeg-turbo's SIMD
/// kernels sit in the IDCT and the upsampler, which the arithmetic path
/// reaches through a different decoder front end.
#[test]
fn djpeg_kernels_agree_on_arithmetic_streams() {
    if !oracle_ready() {
        println!("cjpeg/djpeg not on PATH: skipping");
        return;
    }
    if !arithmetic_supported() {
        println!("this cjpeg cannot write arithmetic streams: skipping");
        return;
    }
    let pixels = synthetic(131, 97, 3, 255);
    let mut compared = 0usize;
    for &sample in &["1x1", "2x2"] {
        let Some(jpeg) = cjpeg(
            &[
                "-quality",
                "80",
                "-sample",
                sample,
                "-arithmetic",
                "-dct",
                "int",
            ],
            &pixels,
        ) else {
            continue;
        };
        let args = &["-dct", "int", "-ppm"][..];
        let simd = djpeg(args, &jpeg).expect("djpeg simd");
        let portable = djpeg_env(args, &jpeg, &[("JSIMD_FORCENONE", "1")]).expect("djpeg portable");
        assert_eq!(
            simd.samples,
            portable.samples,
            "djpeg SIMD and C kernels disagree on an arithmetic {sample} stream ({})",
            libjpeg_version()
        );
        // And ours equals both, which is what makes the parity claim
        // kernel-independent rather than a coincidence of this build.
        assert_identical(
            &format!("arithmetic {sample} against the portable kernels"),
            &decode_u8(&jpeg),
            &portable.samples,
        );
        compared += 1;
    }
    require_comparisons(compared, "arithmetic JSIMD parity");
}

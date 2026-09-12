//! Differential tests for `DecodeOptions::scale` against libjpeg-turbo's
//! `djpeg -dct int -scale M/8`.
//!
//! Byte identity is required, and checked, for `M` in `{1, 2, 4, 8}` — the
//! four values libjpeg itself reconstructs with a dedicated fixed-point
//! kernel this crate ports exactly (`jpeg_idct_1x1`/`_2x2`/`_4x4`, and the
//! unscaled `jpeg_idct_islow` at `8`). The other twelve values (`{3, 5, 6, 7,
//! 9..=16}`) are checked to a numeric tolerance instead — see
//! [`oxiarc_jpeg::Scale`]'s rustdoc for why, and this file's
//! `general_kernel_m_values_are_within_tolerance_of_djpeg` for the measured
//! error this suite records.
//!
//! Gated behind the `jpeg-oracle` feature; every test self-skips (prints a
//! note and passes) when `cjpeg`/`djpeg` are not on `PATH`, and every test
//! that drives `cjpeg` counts its comparisons so a renamed flag cannot make
//! the suite pass vacuously (the same `require_comparisons` discipline
//! `tests/jpeg_oracle.rs` uses).
#![cfg(feature = "jpeg-oracle")]

mod oracle_support;

use oracle_support::{
    Pnm, cjpeg, djpeg, first_difference, libjpeg_version, synthetic, tool_available,
};
use oxiarc_jpeg::{DecodeOptions, Decoder, Scale};

/// `true` when both reference binaries are usable.
fn oracle_ready() -> bool {
    tool_available("cjpeg") && tool_available("djpeg")
}

/// Fail rather than pass vacuously when `cjpeg` produced no fixture at all.
fn require_comparisons(compared: usize, what: &str) {
    assert!(
        compared > 0,
        "{what}: cjpeg produced no fixture, so the test would pass vacuously ({})",
        libjpeg_version()
    );
}

/// Decode `jpeg` at `scale`, returning `(width, height, samples)`.
fn decode_scaled(jpeg: &[u8], scale: Scale) -> (usize, usize, Vec<u16>) {
    let options = DecodeOptions {
        scale,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(jpeg, options);
    let info = decoder.read_info().expect("read_info");
    let samples = decoder
        .decode()
        .expect("decode")
        .into_iter()
        .map(u16::from)
        .collect();
    (
        info.scaled_width as usize,
        info.scaled_height as usize,
        samples,
    )
}

/// Assert byte identity, including dimensions: a scaled decode that gets the
/// `ceil(dim * M / 8)` rounding wrong would otherwise compare a truncated
/// prefix against `djpeg`'s full output and could pass by accident.
fn assert_identical(label: &str, ours: &(usize, usize, Vec<u16>), theirs: &Pnm) {
    let (width, height, samples) = ours;
    assert_eq!(
        (*width, *height),
        (theirs.width, theirs.height),
        "{label}: dimensions differ (reference: {})",
        libjpeg_version()
    );
    if let Some((index, a, b)) = first_difference(samples, &theirs.samples) {
        panic!(
            "{label}: sample {index} differs (ours {a}, djpeg {b}); reference: {}",
            libjpeg_version()
        );
    }
}

/// Three shapes: one 8x8-aligned, one with dimensions that are not a
/// multiple of 8 on either axis (exercises `ceil()` rounding and a partial
/// last MCU at every scale), and one small enough that a 1/8-scale decode
/// still has more than one output row/column.
fn sources() -> Vec<(&'static str, Pnm)> {
    vec![
        ("aligned_64x64", synthetic(64, 64, 3, 255)),
        ("odd_37x29", synthetic(37, 29, 3, 255)),
        ("small_odd_18x11", synthetic(18, 11, 3, 255)),
    ]
}

/// 4:4:4, 4:2:0 and 4:2:2 — the three ratios whose per-component
/// `_DCT_scaled_size` bump behaves differently at reduced scale (see
/// `planes.rs`'s `component_output_size`): 4:4:4 never bumps (nothing is
/// subsampled to begin with), 4:2:0 bumps on both axes, 4:2:2 bumps on
/// neither (the bump loop requires *both* axes to clear the divisibility
/// check, and 4:2:2 only qualifies on one).
const SAMPLING_RATIOS: [&str; 3] = ["1x1", "2x2", "2x1"];

fn scale_for(numerator: u8) -> Scale {
    Scale::new(numerator).expect("1..=16")
}

#[test]
fn baseline_scale_1_2_4_8_is_byte_identical_to_djpeg() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in SAMPLING_RATIOS {
            let Some(jpeg) = cjpeg(&["-quality", "80", "-sample", sampling], &source) else {
                continue;
            };
            for numerator in [1u8, 2, 4, 8] {
                let scale_arg = format!("{numerator}/8");
                let reference =
                    djpeg(&["-dct", "int", "-scale", &scale_arg, "-pnm"], &jpeg).expect("djpeg");
                let ours = decode_scaled(&jpeg, scale_for(numerator));
                assert_identical(
                    &format!("{name} baseline {sampling} scale {numerator}/8"),
                    &ours,
                    &reference,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "baseline scale 1/2/4/8");
}

#[test]
fn progressive_scale_1_2_4_8_is_byte_identical_to_djpeg() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in SAMPLING_RATIOS {
            let Some(jpeg) = cjpeg(
                &["-quality", "80", "-sample", sampling, "-progressive"],
                &source,
            ) else {
                continue;
            };
            for numerator in [1u8, 2, 4, 8] {
                let scale_arg = format!("{numerator}/8");
                let reference =
                    djpeg(&["-dct", "int", "-scale", &scale_arg, "-pnm"], &jpeg).expect("djpeg");
                let ours = decode_scaled(&jpeg, scale_for(numerator));
                assert_identical(
                    &format!("{name} progressive {sampling} scale {numerator}/8"),
                    &ours,
                    &reference,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "progressive scale 1/2/4/8");
}

/// `M` in `{1, 2, 4, 8}` alone does not exercise every branch of
/// `component_output_size`'s doubling loop: `4:2:0` bumps chroma from `2` to
/// `4` (still an exact kernel) at `M=2`, but never needs the *general*
/// kernel via the bump — this test forces that by picking `M` values in the
/// tolerance range specifically over 4:2:0/4:2:2 sources, so a subsampled
/// component really does route through `idct_general_into` (confirmed by
/// the peak-error figures printed below staying tiny rather than showing an
/// upsampling-ratio mismatch, which would blow the bound completely, not
/// marginally).
#[test]
fn general_kernel_m_values_are_within_tolerance_of_djpeg() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    let mut peak_error = 0i32;
    let mut sum_sq_error = 0f64;
    let mut sample_count = 0u64;
    for (name, source) in sources() {
        for sampling in SAMPLING_RATIOS {
            for progressive in [false, true] {
                let mut args = vec!["-quality", "80", "-sample", sampling];
                if progressive {
                    args.push("-progressive");
                }
                let Some(jpeg) = cjpeg(&args, &source) else {
                    continue;
                };
                for numerator in [3u8, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15, 16] {
                    let scale_arg = format!("{numerator}/8");
                    let Some(reference) =
                        djpeg(&["-dct", "int", "-scale", &scale_arg, "-pnm"], &jpeg)
                    else {
                        continue;
                    };
                    let (width, height, samples) = decode_scaled(&jpeg, scale_for(numerator));
                    assert_eq!(
                        (width, height),
                        (reference.width, reference.height),
                        "{name} {sampling} prog={progressive} scale {numerator}/8: dimensions"
                    );
                    for (&ours, &theirs) in samples.iter().zip(reference.samples.iter()) {
                        let delta = i32::from(ours) - i32::from(theirs);
                        peak_error = peak_error.max(delta.abs());
                        sum_sq_error += f64::from(delta) * f64::from(delta);
                        sample_count += 1;
                    }
                    compared += 1;
                }
            }
        }
    }
    require_comparisons(compared, "general kernel tolerance");
    let mse = sum_sq_error / sample_count.max(1) as f64;
    eprintln!(
        "general kernel vs djpeg over {sample_count} samples, {compared} configurations: \
         peak error {peak_error}, MSE {mse:.4}"
    );
    // A resampling reconstruction that disagrees on which kernel to run
    // (wrong upsample ratio, wrong DC normalisation, a transposed pass, or —
    // this bound's own history — summing over the wrong frequency range for
    // `M < 8`) produces errors of tens to hundreds, not a handful of LSBs.
    // Measured before `idct_general_into` truncated to the lowest `n`
    // frequencies for `n < 8` (matching `jidctint.c`'s own `n < 8` family
    // instead of resampling all 8, which is what `jidctred.c`'s `n` in
    // `{1,2,4}` does): peak error 134, MSE 55.5, over this same matrix.
    // After: peak error 3, MSE 0.05 — the residual is `f64` direct
    // summation vs libjpeg's 13-bit fixed-point fast algorithm, not an
    // algorithmic difference, which is exactly what this bound exists to
    // tell apart from a real regression.
    assert!(
        peak_error <= 6,
        "peak error {peak_error} exceeds the tolerance bound (reference: {})",
        libjpeg_version()
    );
    assert!(
        mse <= 0.5,
        "MSE {mse:.4} exceeds the tolerance bound (reference: {})",
        libjpeg_version()
    );
}

/// Decode `jpeg` at `scale` through the wide (`u16`) entry point, for a
/// twelve-bit frame.
fn decode_scaled_wide(jpeg: &[u8], scale: Scale) -> (usize, usize, Vec<u16>) {
    let options = DecodeOptions {
        scale,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(jpeg, options);
    let info = decoder.read_info().expect("read_info");
    let samples = decoder.decode_u16().expect("decode_u16");
    (
        info.scaled_width as usize,
        info.scaled_height as usize,
        samples,
    )
}

/// Twelve-bit byte parity at every exact scale.
///
/// The ported `jidctred.c` kernels take `PASS1_BITS = 1` above eight-bit
/// samples (libjpeg drops a bit of intermediate precision there), which
/// changes five shift amounts across `idct_1x1_into` / `idct_2x2_into` /
/// `idct_4x4_into`. None of them were exercised against `djpeg` by the rest
/// of this file, whose fixtures are all eight-bit: a wrong `pass1_bits`
/// branch would produce a systematically dark or bright twelve-bit image
/// with every other test in this suite still green. libjpeg-turbo 3.x's
/// `cjpeg -precision 12` and `djpeg -scale` handle these directly (`djpeg`
/// emits a `maxval 4095` PGM/PPM, asserted below so the comparison cannot
/// silently degrade to eight-bit), so this is a true byte-parity check.
#[test]
fn twelve_bit_scale_1_2_4_8_is_byte_identical_to_djpeg() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    let gray = synthetic(37, 23, 1, 4095);
    let colour = synthetic(34, 26, 3, 4095);
    let cases: [(&str, &Pnm, &[&str]); 3] = [
        ("gray_12bit", &gray, &["-precision", "12", "-quality", "85"]),
        (
            "colour_12bit_2x2",
            &colour,
            &["-precision", "12", "-quality", "85", "-sample", "2x2"],
        ),
        (
            "colour_12bit_1x1_progressive",
            &colour,
            &[
                "-precision",
                "12",
                "-quality",
                "85",
                "-sample",
                "1x1",
                "-progressive",
            ],
        ),
    ];
    for (name, source, args) in cases {
        let Some(jpeg) = cjpeg(args, source) else {
            eprintln!("skipping {name}: cjpeg -precision 12 unsupported");
            continue;
        };
        for numerator in [1u8, 2, 4, 8] {
            let scale_arg = format!("{numerator}/8");
            let Some(reference) = djpeg(&["-dct", "int", "-scale", &scale_arg, "-pnm"], &jpeg)
            else {
                continue;
            };
            assert_eq!(
                reference.maxval, 4095,
                "{name}: djpeg did not emit a twelve-bit image, so this comparison would \
                 be against eight-bit samples"
            );
            let ours = decode_scaled_wide(&jpeg, scale_for(numerator));
            assert_identical(&format!("{name} scale {numerator}/8"), &ours, &reference);
            compared += 1;
        }
    }
    require_comparisons(compared, "twelve-bit scale 1/2/4/8");
}

/// The same byte-parity requirement over a stream that carries restart
/// markers, on an image large enough that the `rayon` feature actually splits
/// it into bands.
///
/// The fixtures above are 64x64 or smaller and carry no `DRI`, so the
/// parallel band planner declines every one of them and the parallel merge
/// was never exercised at a reduced scale by this suite at all. It had a real
/// defect: bands were placed at `mcu_row * Vi * 8` rather than at
/// `mcu_row * Vi * _DCT_scaled_size`, which is invisible at `M = 8` and
/// scrambles everything below the first band at every other `M`.
/// `320x256` with `cjpeg -restart 1` (a marker every MCU row) clears the
/// planner's `MINIMUM_UNITS` threshold; grayscale is included because a
/// one-component frame takes the non-interleaved geometry, whose units are
/// blocks rather than MCUs.
#[test]
fn restart_marker_streams_are_byte_identical_to_djpeg_at_every_exact_scale() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    let colour = synthetic(320, 256, 3, 255);
    let gray = synthetic(320, 256, 1, 255);
    let cases: [(&str, &Pnm, &[&str]); 4] = [
        (
            "colour_2x2",
            &colour,
            &["-quality", "80", "-sample", "2x2", "-restart", "1"],
        ),
        (
            "colour_2x1",
            &colour,
            &["-quality", "80", "-sample", "2x1", "-restart", "2"],
        ),
        (
            "colour_1x1",
            &colour,
            &["-quality", "80", "-sample", "1x1", "-restart", "1"],
        ),
        (
            "gray",
            &gray,
            &["-quality", "80", "-grayscale", "-restart", "1"],
        ),
    ];
    for (name, source, args) in cases {
        let Some(jpeg) = cjpeg(args, source) else {
            continue;
        };
        for numerator in [1u8, 2, 4, 8] {
            let scale_arg = format!("{numerator}/8");
            let reference =
                djpeg(&["-dct", "int", "-scale", &scale_arg, "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_scaled(&jpeg, scale_for(numerator));
            assert_identical(
                &format!("{name} restart scale {numerator}/8"),
                &ours,
                &reference,
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "restart-marker scale 1/2/4/8");
}

#[test]
fn scaled_dimensions_match_djpeg_for_every_numerator() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        let Some(jpeg) = cjpeg(&["-quality", "80", "-sample", "2x2"], &source) else {
            continue;
        };
        for numerator in 1u8..=16 {
            let scale_arg = format!("{numerator}/8");
            let Some(reference) = djpeg(&["-dct", "int", "-scale", &scale_arg, "-pnm"], &jpeg)
            else {
                continue;
            };
            let mut decoder = Decoder::with_options(
                jpeg.as_slice(),
                DecodeOptions {
                    scale: scale_for(numerator),
                    ..DecodeOptions::default()
                },
            );
            let info = decoder.read_info().expect("read_info");
            assert_eq!(
                (info.scaled_width as usize, info.scaled_height as usize),
                (reference.width, reference.height),
                "{name} scale {numerator}/8: read_info dimensions before decode()"
            );
            assert_eq!(
                decoder.output_buffer_size(),
                Some(reference.width * reference.height * 3),
                "{name} scale {numerator}/8: output_buffer_size"
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "scaled dimensions");
}

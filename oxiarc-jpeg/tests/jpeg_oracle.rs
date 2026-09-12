//! Differential decode tests against libjpeg-turbo's `cjpeg` and `djpeg`.
//!
//! The invariant is **byte identity**, not a tolerance: this crate reproduces
//! `jpeg_idct_islow`, libjpeg's three fancy upsamplers and its fixed-point
//! YCbCr conversion exactly, so any difference from `djpeg -dct int` is a bug.
//! `-dct fast` and `-dct float` are deliberately never used — they are not
//! bit-reproducible even between builds of libjpeg.
//!
//! Gated behind the `jpeg-oracle` feature. Every test self-skips (prints a
//! note and passes) when `cjpeg`/`djpeg` are not on `PATH`.
#![cfg(feature = "jpeg-oracle")]

mod oracle_support;

use oracle_support::{
    Pnm, cjpeg, djpeg, djpeg_env, first_difference, libjpeg_version, synthetic, tool_available,
};
use oxiarc_jpeg::{DecodeOptions, Decoder, Upsampling};

/// `true` when both reference binaries are usable.
fn oracle_ready() -> bool {
    tool_available("cjpeg") && tool_available("djpeg")
}

/// Decode `jpeg` with this crate at 8-bit precision.
fn decode_u8(jpeg: &[u8], upsampling: Upsampling) -> Vec<u16> {
    let options = DecodeOptions {
        upsampling,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(jpeg, options);
    decoder.read_info().expect("read_info");
    decoder
        .decode()
        .expect("decode")
        .into_iter()
        .map(u16::from)
        .collect()
}

/// Decode `jpeg` with this crate at any precision.
fn decode_wide(jpeg: &[u8]) -> Vec<u16> {
    let mut decoder = Decoder::new(jpeg);
    decoder.read_info().expect("read_info");
    decoder.decode_u16().expect("decode_u16")
}

/// Assert byte identity between our decode and `djpeg`'s.
fn assert_identical(label: &str, ours: &[u16], theirs: &Pnm) {
    if let Some((index, a, b)) = first_difference(ours, &theirs.samples) {
        panic!(
            "{label}: sample {index} differs (ours {a}, djpeg {b}); \
             {} samples vs {}; reference: {}",
            ours.len(),
            theirs.samples.len(),
            libjpeg_version()
        );
    }
}

/// Fail rather than pass vacuously when `cjpeg` produced no fixture at all.
///
/// Every parity test drives `cjpeg` and `continue`s past an option the local
/// build rejects. Without this check a renamed `cjpeg` flag would turn the
/// whole suite green while comparing nothing at all.
fn require_comparisons(compared: usize, what: &str) {
    assert!(
        compared > 0,
        "{what}: cjpeg produced no fixture, so the test would pass vacuously ({})",
        libjpeg_version()
    );
}

/// Walk the `SOS` headers of `jpeg`, yielding `(Ns, AhAl)` per scan.
///
/// Entropy-coded data is skipped by scanning forward to the next non-`RST`
/// marker, which is enough structure to tell what a scan script produced.
fn scan_headers(jpeg: &[u8]) -> Vec<(usize, u8)> {
    let mut out = Vec::new();
    let mut i = 2usize;
    while i + 4 <= jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        if marker != 0xDA {
            i += 2 + length;
            continue;
        }
        let ns = usize::from(jpeg[i + 4]);
        let tail = i + 5 + 2 * ns;
        if tail + 2 < jpeg.len() {
            out.push((ns, jpeg[tail + 2]));
        }
        let mut j = i + 2 + length;
        while j + 1 < jpeg.len() {
            if jpeg[j] == 0xFF && jpeg[j + 1] != 0 && !(0xD0..=0xD7).contains(&jpeg[j + 1]) {
                break;
            }
            j += 1;
        }
        i = j;
    }
    out
}

/// Count `SOS` segments carrying exactly one component, i.e. non-interleaved
/// scans. Used to prove a scan script actually took effect.
fn single_component_scan_count(jpeg: &[u8]) -> usize {
    scan_headers(jpeg)
        .iter()
        .filter(|&&(ns, _)| ns == 1)
        .count()
}

/// Count `SOS` segments with a non-zero `Ah`, i.e. successive-approximation
/// refinement scans. Used to prove the progressive fixtures are not trivial.
fn refinement_scan_count(jpeg: &[u8]) -> usize {
    scan_headers(jpeg)
        .iter()
        .filter(|&&(_, ah_al)| ah_al >> 4 != 0)
        .count()
}

/// The source images the whole suite runs over.
fn sources() -> Vec<(&'static str, Pnm)> {
    vec![
        ("tiny_1x1", synthetic(1, 1, 3, 255)),
        ("row_17x1", synthetic(17, 1, 3, 255)),
        ("column_1x19", synthetic(1, 19, 3, 255)),
        ("odd_17x19", synthetic(17, 19, 3, 255)),
        ("mcu_edge_131x97", synthetic(131, 97, 3, 255)),
        ("aligned_64x64", synthetic(64, 64, 3, 255)),
    ]
}

#[test]
fn dec1_grayscale_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        let gray = synthetic(source.width, source.height, 1, 255);
        for quality in ["10", "50", "75", "90", "100"] {
            let Some(jpeg) = cjpeg(&["-quality", quality, "-grayscale"], &gray) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(&format!("{name} gray q{quality}"), &ours, &reference);
            compared += 1;
        }
    }
    require_comparisons(compared, "grayscale");
}

#[test]
fn dec1_444_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for quality in ["10", "50", "75", "90", "100"] {
            let Some(jpeg) = cjpeg(&["-quality", quality, "-sample", "1x1"], &source) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(&format!("{name} 4:4:4 q{quality}"), &ours, &reference);
            compared += 1;
        }
    }
    require_comparisons(compared, "4:4:4");
}

#[test]
fn dec2_subsampled_fancy_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in ["2x1", "1x2", "2x2", "4x1", "1x4", "2x4", "4x2"] {
            let Some(jpeg) = cjpeg(&["-quality", "80", "-sample", sampling], &source) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(&format!("{name} fancy {sampling}"), &ours, &reference);
            compared += 1;
        }
    }
    require_comparisons(compared, "fancy upsampling");
}

#[test]
fn dec2_subsampled_box_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in ["2x1", "1x2", "2x2", "4x1"] {
            let Some(jpeg) = cjpeg(&["-quality", "80", "-sample", sampling], &source) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-nosmooth", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Box);
            assert_identical(&format!("{name} box {sampling}"), &ours, &reference);
            compared += 1;
        }
    }
    require_comparisons(compared, "box upsampling");
}

#[test]
fn progressive_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in ["1x1", "2x1", "2x2"] {
            let Some(jpeg) = cjpeg(
                &["-quality", "85", "-progressive", "-sample", sampling],
                &source,
            ) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(&format!("{name} progressive {sampling}"), &ours, &reference);
            compared += 1;
        }
    }
    require_comparisons(compared, "progressive");
}

/// A custom scan script that forces several AC refinement passes per band —
/// the corner of Annex G that decoders get wrong.
#[test]
fn progressive_ac_refinement_script_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    // Two successive-approximation refinement passes for the luma AC band and
    // one for each chroma band, plus a DC refinement pass.
    let script = "0: 0 0 0 1;\n1: 0 0 0 0;\n2: 0 0 0 0;\n\
                  0: 1 5 0 2;\n0: 6 63 0 2;\n1: 1 63 0 1;\n2: 1 63 0 1;\n\
                  0: 1 63 2 1;\n1: 1 63 1 0;\n2: 1 63 1 0;\n\
                  0: 1 63 1 0;\n0: 0 0 1 0;\n";
    let path = oracle_support::temp_path("scans", "txt");
    std::fs::write(&path, script).expect("write scan script");
    let script_path = path.to_string_lossy().to_string();

    let mut compared = 0;
    for (name, source) in sources() {
        let Some(jpeg) = cjpeg(
            &["-quality", "85", "-scans", &script_path, "-sample", "2x2"],
            &source,
        ) else {
            continue;
        };
        assert!(
            refinement_scan_count(&jpeg) >= 3,
            "{name}: the script must produce refinement scans, found {}",
            refinement_scan_count(&jpeg)
        );
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        let ours = decode_u8(&jpeg, Upsampling::Fancy);
        assert_identical(&format!("{name} ac-refine script"), &ours, &reference);
        compared += 1;
    }
    let _ = std::fs::remove_file(&path);
    require_comparisons(compared, "ac-refinement scan script");
}

#[test]
fn restart_intervals_are_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for restart in ["1", "2", "1B"] {
            for sampling in ["1x1", "2x2"] {
                let Some(jpeg) = cjpeg(
                    &["-quality", "80", "-restart", restart, "-sample", sampling],
                    &source,
                ) else {
                    continue;
                };
                let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
                let ours = decode_u8(&jpeg, Upsampling::Fancy);
                assert_identical(
                    &format!("{name} restart {restart} {sampling}"),
                    &ours,
                    &reference,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "restart intervals");
}

#[test]
fn optimized_huffman_tables_are_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        let Some(jpeg) = cjpeg(&["-quality", "70", "-optimize", "-sample", "2x2"], &source) else {
            continue;
        };
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        let ours = decode_u8(&jpeg, Upsampling::Fancy);
        assert_identical(&format!("{name} optimized"), &ours, &reference);
        compared += 1;
    }
    require_comparisons(compared, "optimized huffman");
}

#[test]
fn twelve_bit_frames_are_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let source = synthetic(37, 23, 1, 4095);
    for quality in ["10", "25", "50", "90"] {
        let Some(jpeg) = cjpeg(&["-precision", "12", "-quality", quality], &source) else {
            eprintln!("skipping 12-bit: cjpeg -precision unsupported");
            return;
        };
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        assert_eq!(reference.maxval, 4095, "djpeg should emit a 12-bit PGM");
        let ours = decode_wide(&jpeg);
        assert_identical(&format!("12-bit q{quality}"), &ours, &reference);
    }
}

#[test]
fn lossless_frames_round_trip_exactly() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let source = synthetic(29, 17, 1, 255);
    let mut any = false;
    for psv in ["1", "2", "3", "4", "5", "6", "7"] {
        let Some(jpeg) = cjpeg(&["-lossless", psv], &source) else {
            continue;
        };
        any = true;
        let ours = decode_wide(&jpeg);
        assert_eq!(
            ours,
            source.samples,
            "lossless psv {psv} must reproduce the source exactly ({})",
            libjpeg_version()
        );
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        assert_identical(&format!("lossless psv {psv}"), &ours, &reference);
    }
    if !any {
        eprintln!("skipping lossless: cjpeg -lossless unsupported");
    }
}

#[test]
fn lossless_point_transform_and_colour_round_trip() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let colour = synthetic(23, 11, 3, 255);
    for psv in ["1", "4", "7"] {
        let Some(jpeg) = cjpeg(&["-lossless", psv], &colour) else {
            continue;
        };
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        let ours = decode_wide(&jpeg);
        assert_identical(&format!("lossless rgb psv {psv}"), &ours, &reference);
    }

    let wide = synthetic(19, 13, 1, 65535);
    for spec in ["1,1", "4,2"] {
        let Some(jpeg) = cjpeg(&["-precision", "16", "-lossless", spec], &wide) else {
            continue;
        };
        let ours = decode_wide(&jpeg);
        // A point transform discards low bits, so compare against what the
        // transform preserves rather than the raw source.
        let shift: u32 = spec
            .split(',')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let expected: Vec<u16> = wide
            .samples
            .iter()
            .map(|&s| (s >> shift) << shift)
            .collect();
        assert_eq!(ours, expected, "16-bit lossless with Pt {shift}");
    }
}

/// A multi-scan **sequential** stream, i.e. one non-interleaved scan per
/// component. `cjpeg -scans` accepts a sequential script, and this is the only
/// way the non-interleaved sequential path gets exercised: the default
/// baseline encoder always interleaves.
#[test]
fn non_interleaved_sequential_scans_are_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let path = oracle_support::temp_path("seq_scans", "txt");
    std::fs::write(&path, "0: 0 63 0 0;\n1: 0 63 0 0;\n2: 0 63 0 0;\n").expect("write scan script");
    let script = path.to_string_lossy().to_string();

    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in ["1x1", "2x2"] {
            let Some(jpeg) = cjpeg(
                &["-quality", "80", "-sample", sampling, "-scans", &script],
                &source,
            ) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(
                &format!("{name} sequential non-interleaved {sampling}"),
                &ours,
                &reference,
            );
            compared += 1;
        }
    }
    let _ = std::fs::remove_file(&path);
    require_comparisons(compared, "non-interleaved sequential scans");
}

/// Lossless frames with restart intervals: `Ra` prediction resumes at the
/// start of every interval, which is the part of Annex H that is easiest to
/// get wrong.
#[test]
fn lossless_with_restart_intervals_round_trips() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let source = synthetic(37, 23, 1, 255);
    let mut compared = 0;
    for psv in ["1", "4", "7"] {
        for restart in ["1", "2", "1B"] {
            let Some(jpeg) = cjpeg(&["-lossless", psv, "-restart", restart], &source) else {
                continue;
            };
            let ours = decode_wide(&jpeg);
            assert_eq!(
                ours,
                source.samples,
                "lossless psv {psv} restart {restart} must be exact ({})",
                libjpeg_version()
            );
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            assert_identical(
                &format!("lossless psv {psv} restart {restart}"),
                &ours,
                &reference,
            );
            compared += 1;
        }
    }
    if compared == 0 {
        eprintln!("skipping lossless restarts: cjpeg -lossless unsupported");
    }
}

/// A lossless colour frame is written as one **interleaved** three-component
/// scan with `'R'`, `'G'`, `'B'` identifiers, so this covers `Nf > 1`
/// interleaving in Annex H.
#[test]
fn lossless_interleaved_colour_scans_round_trip() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        let Some(jpeg) = cjpeg(&["-lossless", "1"], &source) else {
            continue;
        };
        let ours = decode_wide(&jpeg);
        assert_eq!(
            ours,
            source.samples,
            "{name}: interleaved lossless must be exact ({})",
            libjpeg_version()
        );
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        assert_identical(&format!("{name} lossless interleaved"), &ours, &reference);
        compared += 1;
    }
    if compared == 0 {
        eprintln!("skipping interleaved lossless: cjpeg -lossless unsupported");
    }
}

/// Lossless colour with **one scan per component**: three `Ns = 1` scans over
/// an `Nf = 3` frame. This is the Annex H path where `row_units`, the
/// per-interval first-row prediction rule and the single-component extent all
/// interact, and it is reachable only through a scan script — `cjpeg` writes
/// colour lossless as one interleaved scan otherwise.
#[test]
fn lossless_non_interleaved_colour_scans_round_trip() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    // In a lossless script `Ss` is the predictor selector and `Al` the point
    // transform, so this asks for predictor 1 on every component separately.
    let path = oracle_support::temp_path("lossless_scans", "txt");
    std::fs::write(&path, "0: 1 0 0 0;\n1: 1 0 0 0;\n2: 1 0 0 0;\n").expect("write scan script");
    let script = path.to_string_lossy().to_string();

    let mut compared = 0;
    for (name, source) in sources() {
        let Some(jpeg) = cjpeg(&["-lossless", "1", "-scans", &script], &source) else {
            continue;
        };
        assert_eq!(
            single_component_scan_count(&jpeg),
            3,
            "{name}: the script must produce three single-component scans ({})",
            libjpeg_version()
        );
        let ours = decode_wide(&jpeg);
        assert_eq!(
            ours,
            source.samples,
            "{name}: non-interleaved lossless must be exact ({})",
            libjpeg_version()
        );
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        assert_identical(
            &format!("{name} lossless non-interleaved"),
            &ours,
            &reference,
        );
        compared += 1;
    }
    let _ = std::fs::remove_file(&path);
    if compared == 0 {
        eprintln!("skipping non-interleaved lossless: cjpeg -lossless -scans unsupported");
    }
}

/// Progressive frames at 12-bit precision exercise the `PASS1_BITS = 1` IDCT
/// path together with the Annex G coefficient buffer.
#[test]
fn twelve_bit_progressive_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let source = synthetic(37, 23, 1, 4095);
    let mut compared = 0;
    for quality in ["30", "60", "90"] {
        let Some(jpeg) = cjpeg(
            &["-precision", "12", "-progressive", "-quality", quality],
            &source,
        ) else {
            continue;
        };
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        let ours = decode_wide(&jpeg);
        assert_identical(&format!("12-bit progressive q{quality}"), &ours, &reference);
        compared += 1;
    }
    if compared == 0 {
        eprintln!("skipping 12-bit progressive: cjpeg -precision unsupported");
    }
}

/// The byte-parity invariants are only meaningful if `djpeg`'s own output does
/// not depend on which kernel it selects. `JSIMD_FORCENONE=1` forces the
/// portable C paths; the two must agree.
#[test]
fn djpeg_simd_and_c_paths_agree() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let source = synthetic(131, 97, 3, 255);
    for sampling in ["1x1", "2x1", "2x2"] {
        for extra in [
            &["-dct", "int", "-pnm"][..],
            &["-dct", "int", "-nosmooth", "-pnm"][..],
        ] {
            let Some(jpeg) = cjpeg(&["-quality", "80", "-sample", sampling], &source) else {
                continue;
            };
            let simd = djpeg(extra, &jpeg).expect("djpeg simd");
            let portable =
                djpeg_env(extra, &jpeg, &[("JSIMD_FORCENONE", "1")]).expect("djpeg portable");
            assert_eq!(
                simd.samples,
                portable.samples,
                "djpeg SIMD and C kernels disagree for {sampling} {extra:?} ({})",
                libjpeg_version()
            );
        }
    }
}

/// Pillow is a second opinion, not the primary gate: its libjpeg build can
/// differ in upsampler details, so the comparison is a tolerance.
#[test]
fn pillow_cross_check_within_one_lsb() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let pillow = std::process::Command::new("python3")
        .args(["-c", "import PIL"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !pillow {
        eprintln!("skipping: Pillow unavailable");
        return;
    }

    let source = synthetic(64, 48, 3, 255);
    let Some(jpeg) = cjpeg(&["-quality", "85", "-sample", "2x2"], &source) else {
        return;
    };
    let jpeg_path = oracle_support::temp_path("pillow_in", "jpg");
    let raw_path = oracle_support::temp_path("pillow_out", "bin");
    std::fs::write(&jpeg_path, &jpeg).expect("write jpeg");
    let script = "import sys\nfrom PIL import Image\nim = Image.open(sys.argv[1]).convert('RGB')\nopen(sys.argv[2], 'wb').write(im.tobytes())\n";
    let status = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&jpeg_path)
        .arg(&raw_path)
        .status()
        .expect("run python3");
    assert!(status.success(), "Pillow decode failed");
    let theirs = std::fs::read(&raw_path).expect("read Pillow output");
    let ours = decode_u8(&jpeg, Upsampling::Fancy);
    assert_eq!(ours.len(), theirs.len());
    let mut peak = 0i32;
    let mut squared = 0f64;
    for (a, b) in ours.iter().zip(theirs.iter()) {
        let diff = i32::from(*a) - i32::from(*b);
        peak = peak.max(diff.abs());
        squared += f64::from(diff * diff);
    }
    let mse = squared / ours.len() as f64;
    assert!(peak <= 1, "peak difference vs Pillow is {peak}");
    assert!(mse <= 0.06, "MSE vs Pillow is {mse}");
    let _ = std::fs::remove_file(&jpeg_path);
    let _ = std::fs::remove_file(&raw_path);
}

/// Restart markers inside a *progressive* scan. `DRI` resets the DC
/// predictors, the bit alignment and the EOB run at once, and a progressive
/// frame exercises all three across ten scans rather than one.
#[test]
fn progressive_restart_intervals_are_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for restart in ["1", "2"] {
            for sampling in ["1x1", "2x2"] {
                let Some(jpeg) = cjpeg(
                    &[
                        "-quality",
                        "80",
                        "-progressive",
                        "-restart",
                        restart,
                        "-sample",
                        sampling,
                    ],
                    &source,
                ) else {
                    continue;
                };
                let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
                let ours = decode_u8(&jpeg, Upsampling::Fancy);
                assert_identical(
                    &format!("{name} progressive restart {restart} {sampling}"),
                    &ours,
                    &reference,
                );
                compared += 1;
            }
        }
    }
    require_comparisons(compared, "progressive restart intervals");
}

/// Expansion factors that are neither 1 nor 2 fall through libjpeg's fancy
/// kernels to `int_upsample`, its plain replicator. Both upsampling modes are
/// checked because `-nosmooth` selects a different libjpeg path for the 2x
/// cases and must not for these.
#[test]
fn odd_sampling_factors_are_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in ["3x1", "1x3", "3x2", "2x3"] {
            let Some(jpeg) = cjpeg(&["-quality", "80", "-sample", sampling], &source) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(&format!("{name} sample {sampling}"), &ours, &reference);
            let reference_box = djpeg(&["-dct", "int", "-nosmooth", "-pnm"], &jpeg).expect("djpeg");
            let ours_box = decode_u8(&jpeg, Upsampling::Box);
            assert_identical(
                &format!("{name} sample {sampling} box"),
                &ours_box,
                &reference_box,
            );
            compared += 2;
        }
    }
    require_comparisons(compared, "odd sampling factors");
}

/// `-sample H1xV1,H2xV2,H3xV3` gives Cb and Cr *different* sampling, so one
/// image drives two different upsamplers at once — the case a per-frame
/// "which kernel do we use" decision gets wrong.
#[test]
fn asymmetric_per_component_sampling_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, source) in sources() {
        for sampling in [
            "2x2,1x1,2x1",
            "2x2,2x1,1x1",
            "2x2,1x2,1x1",
            "2x2,1x1,1x2",
            "2x2,2x2,1x1",
            "4x1,2x1,1x1",
        ] {
            let Some(jpeg) = cjpeg(&["-quality", "82", "-sample", sampling], &source) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(&format!("{name} asymmetric {sampling}"), &ours, &reference);
            let reference_box = djpeg(&["-dct", "int", "-nosmooth", "-pnm"], &jpeg).expect("djpeg");
            let ours_box = decode_u8(&jpeg, Upsampling::Box);
            assert_identical(
                &format!("{name} asymmetric {sampling} box"),
                &ours_box,
                &reference_box,
            );
            compared += 2;
        }
        let gray = synthetic(source.width, source.height, 1, 255);
        for extra in [
            &["-progressive"][..],
            &["-progressive", "-restart", "1"][..],
        ] {
            let mut args = vec!["-quality", "82"];
            args.extend_from_slice(extra);
            let Some(jpeg) = cjpeg(&args, &gray) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            let ours = decode_u8(&jpeg, Upsampling::Fancy);
            assert_identical(
                &format!("{name} progressive grayscale {extra:?}"),
                &ours,
                &reference,
            );
            compared += 1;
        }
    }
    require_comparisons(compared, "asymmetric sampling");
}

/// 12-bit *colour*: the fancy upsamplers and the fixed-point YCbCr conversion
/// both key off `MAXJSAMPLE`/`CENTERJSAMPLE`, and every other 12-bit test in
/// this file is grayscale, which exercises neither.
#[test]
fn twelve_bit_colour_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0;
    for (name, (width, height)) in [("small", (37usize, 29usize)), ("mcu_edge", (131, 97))] {
        let source = synthetic(width, height, 3, 4095);
        for sampling in ["1x1", "2x1", "2x2"] {
            let Some(jpeg) = cjpeg(
                &["-precision", "12", "-quality", "85", "-sample", sampling],
                &source,
            ) else {
                continue;
            };
            let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
            assert_eq!(reference.maxval, 4095, "djpeg should emit a 12-bit PPM");
            let ours = decode_wide(&jpeg);
            assert_identical(
                &format!("{name} 12-bit colour {sampling}"),
                &ours,
                &reference,
            );
            compared += 1;
        }
    }
    if compared == 0 {
        eprintln!("skipping 12-bit colour: cjpeg -precision unsupported");
    }
}

/// 12-bit combined with the options that change the entropy layer rather than
/// the sample range: restart markers, an odd sampling factor, and encoder-side
/// optimized Huffman tables.
#[test]
fn twelve_bit_with_restarts_and_odd_sampling_is_byte_identical() {
    if !oracle_ready() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let source = synthetic(59, 41, 3, 4095);
    let mut compared = 0;
    for args in [
        &[
            "-precision",
            "12",
            "-quality",
            "80",
            "-restart",
            "1",
            "-sample",
            "2x2",
        ][..],
        &["-precision", "12", "-quality", "80", "-sample", "3x1"][..],
        &[
            "-precision",
            "12",
            "-quality",
            "80",
            "-optimize",
            "-sample",
            "2x1",
        ][..],
    ] {
        let Some(jpeg) = cjpeg(args, &source) else {
            continue;
        };
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        let ours = decode_wide(&jpeg);
        assert_identical(&format!("12-bit {args:?}"), &ours, &reference);
        compared += 1;
    }
    if compared == 0 {
        eprintln!("skipping 12-bit variants: cjpeg -precision unsupported");
    }
}

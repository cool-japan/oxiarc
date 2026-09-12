//! `DecodeOptions::scale` behaviour that needs no external reference decoder
//! — the hermetic companion to `tests/scale_oracle.rs`. Every fixture here is
//! built with this crate's own encoder, so the whole file runs on a machine
//! with no `cjpeg`/`djpeg` on `PATH`.

use oxiarc_jpeg::{
    DecodeOptions, Decoder, EncodeOptions, EncodeProcess, InputColor, JpegError, RestartInterval,
    Scale, Subsampling, TableSet, TablesMode, decode_abbreviated_into, encode_to_vec_with_options,
    table_set,
};

/// A synthetic gradient, RGB, interleaved.
fn synthetic_rgb(width: u16, height: u16) -> Vec<u8> {
    let mut pixels = Vec::with_capacity(usize::from(width) * usize::from(height) * 3);
    for y in 0..height {
        for x in 0..width {
            pixels.push(((u32::from(x) * 7 + u32::from(y) * 3) % 256) as u8);
            pixels.push(((u32::from(x) * 11 + u32::from(y) * 5) % 256) as u8);
            pixels.push(((u32::from(x) + u32::from(y) * 13) % 256) as u8);
        }
    }
    pixels
}

fn encode(width: u16, height: u16, subsampling: Subsampling, process: EncodeProcess) -> Vec<u8> {
    encode_with_restarts(width, height, subsampling, process, RestartInterval::None)
}

fn encode_with_restarts(
    width: u16,
    height: u16,
    subsampling: Subsampling,
    process: EncodeProcess,
    restart_interval: RestartInterval,
) -> Vec<u8> {
    let pixels = synthetic_rgb(width, height);
    let options = EncodeOptions {
        subsampling,
        process,
        restart_interval,
        ..EncodeOptions::default()
    };
    encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options).expect("encode")
}

fn decode_at(jpeg: &[u8], scale: Scale) -> (u32, u32, Vec<u8>) {
    let options = DecodeOptions {
        scale,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(jpeg, options);
    let info = decoder.read_info().expect("read_info");
    let samples = decoder.decode().expect("decode");
    (info.scaled_width, info.scaled_height, samples)
}

/// `ceil(dim * M / 8)`, computed independently of `scaled_dim` (the crate's
/// own private helper) so this is a real cross-check, not a restatement.
fn expected_dim(dim: u16, numerator: u8) -> u32 {
    let product = u32::from(dim) * u32::from(numerator);
    product.div_ceil(8)
}

/// Odd dimensions (not a multiple of 8 on either axis, and — for the
/// smallest cases — not even a whole MCU), 4:2:0 subsampled so the
/// per-component `_DCT_scaled_size` bump loop in `planes.rs` really runs,
/// at every `M` in `1..=16`.
#[test]
fn scaled_dimensions_follow_the_ceil_formula_for_odd_sizes() {
    for &(w, h) in &[(37u16, 29u16), (41, 31), (9, 7), (1, 1), (255, 129)] {
        let jpeg = encode(w, h, Subsampling::S420, EncodeProcess::Sequential);
        for numerator in 1u8..=16 {
            let scale = Scale::new(numerator).expect("1..=16");
            let (scaled_w, scaled_h, samples) = decode_at(&jpeg, scale);
            let want_w = expected_dim(w, numerator);
            let want_h = expected_dim(h, numerator);
            assert_eq!(
                (scaled_w, scaled_h),
                (want_w, want_h),
                "{w}x{h} M={numerator}/8"
            );
            assert_eq!(
                samples.len(),
                want_w as usize * want_h as usize * 3,
                "{w}x{h} M={numerator}/8: decode() length"
            );
        }
    }
}

/// The same cross-check for `Decoder::output_buffer_size` and
/// `Decoder::decode_into_strided`, which take a different path through the
/// crate than plain `decode()`.
#[test]
fn output_buffer_size_and_strided_decode_agree_with_scaled_dimensions() {
    let jpeg = encode(37, 29, Subsampling::S420, EncodeProcess::Sequential);
    for numerator in [1u8, 3, 4, 7, 8, 11, 16] {
        let options = DecodeOptions {
            scale: Scale::new(numerator).expect("1..=16"),
            ..DecodeOptions::default()
        };
        let mut decoder = Decoder::with_options(jpeg.as_slice(), options);
        decoder.read_info().expect("read_info");
        let want_w = expected_dim(37, numerator) as usize;
        let want_h = expected_dim(29, numerator) as usize;
        assert_eq!(
            decoder.output_buffer_size(),
            Some(want_w * want_h * 3),
            "M={numerator}/8"
        );
        // A stride wider than the row, so the padding-preservation contract
        // is exercised too: only the first `want_w * 3` samples of each row
        // are written.
        let stride = want_w * 3 + 6;
        let mut out = vec![0xAAu8; stride * want_h.max(1)];
        decoder
            .decode_into_strided(&mut out, stride)
            .unwrap_or_else(|e| panic!("M={numerator}/8: {e}"));
        for row in 0..want_h {
            let padding = &out[row * stride + want_w * 3..row * stride + stride];
            assert!(
                padding.iter().all(|&b| b == 0xAA),
                "M={numerator}/8 row {row}: padding was touched"
            );
        }
    }
}

/// [`Scale::FULL`] must change nothing at all: every sample `Decoder::new`
/// (no options) produces must equal what an explicit
/// `DecodeOptions { scale: Scale::FULL, .. }` produces, for both a 4:4:4 and
/// a 4:2:0 fixture. This is the regression guard for the claim in this
/// track's handoff that the whole geometry change is a no-op at the
/// default scale.
#[test]
fn full_scale_is_byte_identical_to_no_scale_option_at_all() {
    for subsampling in [Subsampling::S444, Subsampling::S420, Subsampling::S422] {
        let jpeg = encode(35, 23, subsampling, EncodeProcess::Sequential);
        let mut plain = Decoder::new(jpeg.as_slice());
        let plain_out = plain.decode().expect("decode");

        let mut scaled = Decoder::with_options(
            jpeg.as_slice(),
            DecodeOptions {
                scale: Scale::FULL,
                ..DecodeOptions::default()
            },
        );
        let scaled_out = scaled.decode().expect("decode");
        assert_eq!(plain_out, scaled_out, "{subsampling:?}");
    }
}

/// T.81 lossless has no DCT to scale; `DecodeOptions::scale`'s rustdoc
/// documents that it is silently ignored there, matching libjpeg's own
/// hardwired "no scaling" rule. This pins that a lossless frame really does
/// decode at its native size regardless of what `scale` asks for, rather
/// than merely trusting the rustdoc.
#[test]
fn scale_is_ignored_for_lossless_frames() {
    let jpeg = encode(
        24,
        18,
        Subsampling::S444,
        EncodeProcess::Lossless {
            predictor: 1,
            point_transform: 0,
        },
    );
    let native = {
        let mut decoder = Decoder::new(jpeg.as_slice());
        let info = decoder.read_info().expect("read_info");
        (info.width, info.height, decoder.decode().expect("decode"))
    };
    for numerator in [1u8, 2, 4, 7, 11, 16] {
        let options = DecodeOptions {
            scale: Scale::new(numerator).expect("1..=16"),
            ..DecodeOptions::default()
        };
        let mut decoder = Decoder::with_options(jpeg.as_slice(), options);
        let info = decoder.read_info().expect("read_info");
        assert_eq!(
            (info.scaled_width, info.scaled_height),
            (u32::from(native.0), u32::from(native.1)),
            "M={numerator}/8: lossless scaled dims must equal the native size"
        );
        let out = decoder.decode().expect("decode");
        assert_eq!(out, native.2, "M={numerator}/8: lossless samples changed");
    }
}

#[test]
fn scale_new_rejects_zero_and_above_sixteen() {
    assert!(matches!(
        Scale::new(0),
        Err(JpegError::InvalidDecodeParameter {
            parameter: "scale",
            ..
        })
    ));
    assert!(matches!(
        Scale::new(17),
        Err(JpegError::InvalidDecodeParameter {
            parameter: "scale",
            ..
        })
    ));
    assert!(Scale::new(1).is_ok());
    assert!(Scale::new(16).is_ok());
}

#[test]
fn scale_constants_match_their_documented_fractions() {
    assert_eq!(Scale::FULL.numerator(), 8);
    assert_eq!(Scale::ONE_HALF.numerator(), 4);
    assert_eq!(Scale::ONE_QUARTER.numerator(), 2);
    assert_eq!(Scale::ONE_EIGHTH.numerator(), 1);
    for scale in [
        Scale::FULL,
        Scale::ONE_HALF,
        Scale::ONE_QUARTER,
        Scale::ONE_EIGHTH,
    ] {
        assert_eq!(scale.denominator(), 8);
    }
    assert!(Scale::FULL.is_full());
    assert!(!Scale::ONE_HALF.is_full());
    assert_eq!(Scale::default(), Scale::FULL);
}

/// Every `M` in `1..=16`, over both a progressive and a sequential frame,
/// simply must not panic — the hermetic half of the no-panic guarantee the
/// oracle-gated suite cannot provide on a machine without `djpeg`.
#[test]
fn every_numerator_decodes_without_panicking_on_both_processes() {
    for process in [EncodeProcess::Sequential, EncodeProcess::Progressive] {
        for subsampling in [Subsampling::S444, Subsampling::S420, Subsampling::S422] {
            let jpeg = encode(19, 13, subsampling, process);
            for numerator in 1u8..=16 {
                let (_, _, samples) = decode_at(&jpeg, Scale::new(numerator).expect("1..=16"));
                assert!(
                    !samples.is_empty(),
                    "{process:?} {subsampling:?} M={numerator}"
                );
            }
        }
    }
}

/// Restart markers must not change a single decoded sample, at any scale.
///
/// A restart-marker stream is the only input the `rayon` feature decodes in
/// parallel bands, and the crate's own feature table promises the output is
/// byte-identical with and without it. This is the public-API regression
/// guard for a real defect where it was not: the band merge placed each
/// band's rows at `mcu_row * Vi * 8` instead of
/// `mcu_row * Vi * _DCT_scaled_size`, so with `rayon` on, everything below
/// the first band landed in the wrong plane rows at every scale but
/// `Scale::FULL` (measured before the fix: 2686 of 3072 output bytes wrong at
/// `M = 1`). `256x256` is large enough to clear the planner's
/// `MINIMUM_UNITS` threshold and split into several bands; the check is
/// meaningful with or without `rayon`, since the serial path must produce the
/// same answer either way.
#[test]
fn restart_markers_do_not_change_a_scaled_decode() {
    for subsampling in [Subsampling::S444, Subsampling::S420, Subsampling::S422] {
        let with_restarts = encode_with_restarts(
            256,
            256,
            subsampling,
            EncodeProcess::Sequential,
            RestartInterval::Mcus(16),
        );
        let without = encode(256, 256, subsampling, EncodeProcess::Sequential);
        for numerator in 1u8..=16 {
            let scale = Scale::new(numerator).expect("1..=16");
            let (rw, rh, restarted) = decode_at(&with_restarts, scale);
            let (pw, ph, plain) = decode_at(&without, scale);
            assert_eq!(
                (rw, rh),
                (pw, ph),
                "{subsampling:?} M={numerator}/8: dimensions"
            );
            let differing = restarted
                .iter()
                .zip(plain.iter())
                .filter(|(a, b)| a != b)
                .count();
            assert_eq!(
                differing,
                0,
                "{subsampling:?} M={numerator}/8: {differing} of {} samples differ between a \
                 restart-marker stream and the same image without markers",
                plain.len()
            );
        }
    }
}

/// `DecodeOptions::raw_components` (the TIFF pipeline's mode: no colour
/// transform, planes handed through interleaved) must size and upsample from
/// the *scaled* geometry too, at every `M`.
///
/// Not a restatement of `scaled_dimensions_follow_the_ceil_formula_for_odd_sizes`:
/// that one goes through the YCbCr-to-RGB path, which resolves its component
/// count from the colour transform rather than from the frame, and never
/// reaches `OutputPlan`'s `Mode::Passthrough` arm.
#[test]
fn raw_component_decodes_follow_the_scaled_geometry() {
    for subsampling in [Subsampling::S444, Subsampling::S420, Subsampling::S422] {
        let jpeg = encode(37, 29, subsampling, EncodeProcess::Sequential);
        for numerator in 1u8..=16 {
            let options = DecodeOptions {
                scale: Scale::new(numerator).expect("1..=16"),
                raw_components: true,
                ..DecodeOptions::default()
            };
            let mut decoder = Decoder::with_options(jpeg.as_slice(), options);
            let info = decoder.read_info().expect("read_info");
            let want = info.scaled_width as usize * info.scaled_height as usize * 3;
            assert_eq!(
                decoder.output_buffer_size(),
                Some(want),
                "{subsampling:?} raw M={numerator}/8"
            );
            let samples = decoder.decode().expect("decode");
            assert_eq!(samples.len(), want, "{subsampling:?} raw M={numerator}/8");
        }
    }
}

/// `decode_into_u16_strided` — the wide, sub-rectangle entry point — sizes
/// its bounds check from the scaled geometry, leaves the inter-row padding
/// alone, and rejects a buffer one sample short instead of writing a
/// truncated image.
#[test]
fn wide_strided_decode_is_bounded_by_the_scaled_geometry() {
    let jpeg = encode(35, 23, Subsampling::S420, EncodeProcess::Sequential);
    for numerator in [1u8, 3, 5, 8, 13, 16] {
        let scale = Scale::new(numerator).expect("1..=16");
        let options = DecodeOptions {
            scale,
            ..DecodeOptions::default()
        };
        let mut decoder = Decoder::with_options(jpeg.as_slice(), options.clone());
        let info = decoder.read_info().expect("read_info");
        let row = info.scaled_width as usize * 3;
        let rows = info.scaled_height as usize;
        let stride = row + 5;
        let mut out = vec![0x7Fu16; stride * rows];
        decoder
            .decode_into_u16_strided(&mut out, stride)
            .unwrap_or_else(|e| panic!("M={numerator}/8: {e}"));
        for y in 0..rows {
            assert!(
                out[y * stride + row..(y + 1) * stride]
                    .iter()
                    .all(|&v| v == 0x7F),
                "M={numerator}/8 row {y}: padding was touched"
            );
        }

        // `stride * (rows - 1) + row` is exactly enough — the last row needs
        // only its own samples — so one less must be refused.
        let need = stride * (rows - 1) + row;
        let mut short = vec![0u16; need - 1];
        let mut decoder = Decoder::with_options(jpeg.as_slice(), options);
        assert!(
            matches!(
                decoder.decode_into_u16_strided(&mut short, stride),
                Err(JpegError::BufferTooSmall { .. })
            ),
            "M={numerator}/8: a buffer one sample short was accepted"
        );
    }
}

/// The abbreviated (TIFF `JPEGTables`) entry point threads `scale` through
/// as well: `decode_abbreviated_into` sizes its destination from the scaled
/// geometry and reports it back in the `ImageInfo` it returns.
#[test]
fn abbreviated_decode_threads_the_scale_through() {
    let options = EncodeOptions::default();
    let tables = table_set(&options, InputColor::Rgb).expect("table_set");
    let blob = tables.emit(TablesMode::default());
    let parsed = TableSet::parse(&blob).expect("parse");
    let scan =
        encode_to_vec_with_options(&synthetic_rgb(24, 16), 24, 16, InputColor::Rgb, &options)
            .expect("encode");

    for numerator in [1u8, 2, 3, 4, 8, 11, 16] {
        let decode_options = DecodeOptions {
            scale: Scale::new(numerator).expect("1..=16"),
            raw_components: true,
            ..DecodeOptions::default()
        };
        let want_w = expected_dim(24, numerator) as usize;
        let want_h = expected_dim(16, numerator) as usize;
        let mut out = vec![0u8; want_w * want_h * 3];
        let info = decode_abbreviated_into(Some(&parsed), &scan, &decode_options, &mut out)
            .unwrap_or_else(|e| panic!("M={numerator}/8: {e}"));
        assert_eq!(
            (info.scaled_width as usize, info.scaled_height as usize),
            (want_w, want_h),
            "M={numerator}/8"
        );
    }
}

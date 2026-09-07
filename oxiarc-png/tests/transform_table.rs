//! The output colour-type table, pinned row by row.
//!
//! `image` 0.25 matches exhaustively on the colour type a PNG decoder reports
//! and errors on any surviving `Indexed` or sub-byte output, so a single
//! divergence from the `png` crate's table silently breaks every downstream
//! consumer. Every row below is written out by hand rather than derived, so a
//! change to the implementation cannot quietly change the expectation too.

mod common;

use common::PngBuilder;
use oxiarc_png::decoder::output_color_type;
use oxiarc_png::{BitDepth, ColorType, Transformations};

use BitDepth::{Eight, Four, One, Sixteen, Two};
use ColorType::{Grayscale, GrayscaleAlpha, Indexed, Rgb, Rgba};

/// `(input colour type, input depth, has tRNS, output colour type, output depth)`
/// under `Transformations::EXPAND`.
const EXPAND: &[(ColorType, BitDepth, bool, ColorType, BitDepth)] = &[
    (Grayscale, One, false, Grayscale, Eight),
    (Grayscale, Two, false, Grayscale, Eight),
    (Grayscale, Four, false, Grayscale, Eight),
    (Grayscale, Eight, false, Grayscale, Eight),
    (Grayscale, Sixteen, false, Grayscale, Sixteen),
    (Grayscale, One, true, GrayscaleAlpha, Eight),
    (Grayscale, Two, true, GrayscaleAlpha, Eight),
    (Grayscale, Four, true, GrayscaleAlpha, Eight),
    (Grayscale, Eight, true, GrayscaleAlpha, Eight),
    (Grayscale, Sixteen, true, GrayscaleAlpha, Sixteen),
    (Rgb, Eight, false, Rgb, Eight),
    (Rgb, Sixteen, false, Rgb, Sixteen),
    (Rgb, Eight, true, Rgba, Eight),
    (Rgb, Sixteen, true, Rgba, Sixteen),
    (Indexed, One, false, Rgb, Eight),
    (Indexed, Two, false, Rgb, Eight),
    (Indexed, Four, false, Rgb, Eight),
    (Indexed, Eight, false, Rgb, Eight),
    (Indexed, One, true, Rgba, Eight),
    (Indexed, Two, true, Rgba, Eight),
    (Indexed, Four, true, Rgba, Eight),
    (Indexed, Eight, true, Rgba, Eight),
    (GrayscaleAlpha, Eight, false, GrayscaleAlpha, Eight),
    (GrayscaleAlpha, Sixteen, false, GrayscaleAlpha, Sixteen),
    (Rgba, Eight, false, Rgba, Eight),
    (Rgba, Sixteen, false, Rgba, Sixteen),
];

/// The same under `Transformations::EXPAND | Transformations::ALPHA`, where
/// the alpha channel is forced whether or not a `tRNS` chunk is present.
const EXPAND_ALPHA: &[(ColorType, BitDepth, bool, ColorType, BitDepth)] = &[
    (Grayscale, One, false, GrayscaleAlpha, Eight),
    (Grayscale, Two, false, GrayscaleAlpha, Eight),
    (Grayscale, Four, false, GrayscaleAlpha, Eight),
    (Grayscale, Eight, false, GrayscaleAlpha, Eight),
    (Grayscale, Sixteen, false, GrayscaleAlpha, Sixteen),
    (Rgb, Eight, false, Rgba, Eight),
    (Rgb, Sixteen, false, Rgba, Sixteen),
    (Indexed, One, false, Rgba, Eight),
    (Indexed, Four, false, Rgba, Eight),
    (Indexed, Eight, false, Rgba, Eight),
    (GrayscaleAlpha, Eight, false, GrayscaleAlpha, Eight),
    (Rgba, Sixteen, false, Rgba, Sixteen),
];

/// `Transformations::STRIP_16` alone changes the depth and nothing else.
const STRIP_ONLY: &[(ColorType, BitDepth, bool, ColorType, BitDepth)] = &[
    (Grayscale, One, false, Grayscale, One),
    (Grayscale, Four, true, Grayscale, Four),
    (Grayscale, Sixteen, false, Grayscale, Eight),
    (Rgb, Sixteen, true, Rgb, Eight),
    (Rgb, Eight, false, Rgb, Eight),
    (Indexed, Four, false, Indexed, Four),
    (Indexed, Eight, true, Indexed, Eight),
    (GrayscaleAlpha, Sixteen, false, GrayscaleAlpha, Eight),
    (Rgba, Sixteen, false, Rgba, Eight),
];

/// `Transformations::normalize_to_color8()` is `EXPAND | STRIP_16`.
const NORMALIZE: &[(ColorType, BitDepth, bool, ColorType, BitDepth)] = &[
    (Grayscale, One, false, Grayscale, Eight),
    (Grayscale, Sixteen, false, Grayscale, Eight),
    (Grayscale, Sixteen, true, GrayscaleAlpha, Eight),
    (Rgb, Sixteen, false, Rgb, Eight),
    (Rgb, Sixteen, true, Rgba, Eight),
    (Indexed, One, false, Rgb, Eight),
    (Indexed, Eight, true, Rgba, Eight),
    (GrayscaleAlpha, Sixteen, false, GrayscaleAlpha, Eight),
    (Rgba, Sixteen, false, Rgba, Eight),
];

fn check(
    rows: &[(ColorType, BitDepth, bool, ColorType, BitDepth)],
    transform: Transformations,
    label: &str,
) {
    for &(ct, depth, trns, want_ct, want_depth) in rows {
        assert_eq!(
            output_color_type(ct, depth, trns, transform),
            (want_ct, want_depth),
            "{label}: {ct:?}/{depth:?} trns={trns}"
        );
    }
}

#[test]
fn expand_table() {
    check(EXPAND, Transformations::EXPAND, "EXPAND");
}

#[test]
fn expand_alpha_table() {
    check(
        EXPAND_ALPHA,
        Transformations::EXPAND | Transformations::ALPHA,
        "EXPAND|ALPHA",
    );
    // ALPHA on its own implies EXPAND.
    check(EXPAND_ALPHA, Transformations::ALPHA, "ALPHA");
}

#[test]
fn strip16_table() {
    check(STRIP_ONLY, Transformations::STRIP_16, "STRIP_16");
}

#[test]
fn normalize_table() {
    check(
        NORMALIZE,
        Transformations::normalize_to_color8(),
        "normalize",
    );
}

#[test]
fn identity_is_the_input_type() {
    for &(ct, depth, trns, _, _) in EXPAND {
        assert_eq!(
            output_color_type(ct, depth, trns, Transformations::IDENTITY),
            (ct, depth)
        );
    }
}

#[test]
fn normalize_never_leaves_indexed_or_sub_byte_output() {
    // This is precisely the invariant `image` 0.25 relies on.
    for &(ct, depth, trns, _, _) in EXPAND {
        let (out_ct, out_depth) =
            output_color_type(ct, depth, trns, Transformations::normalize_to_color8());
        assert_ne!(out_ct, Indexed, "{ct:?}/{depth:?} trns={trns}");
        assert_eq!(out_depth, Eight, "{ct:?}/{depth:?} trns={trns}");
    }
}

/// The table is only useful if a real decode agrees with it.
#[test]
fn a_real_decode_reports_and_produces_the_tabulated_shape() {
    for &(ct, depth, trns, want_ct, want_depth) in EXPAND {
        let mut builder = PngBuilder::new(4, 2, ct, depth);
        if ct == Indexed {
            let palette: Vec<u8> = (0..256u16)
                .flat_map(|i| [i as u8, (i * 3) as u8, (i * 5) as u8])
                .collect();
            builder = builder.chunk(oxiarc_png::chunk::PLTE, &palette);
        }
        if trns {
            let payload: Vec<u8> = match ct {
                Grayscale => vec![0, 1],
                Rgb => vec![0, 1, 0, 2, 0, 3],
                Indexed => vec![0x80; 4],
                _ => continue,
            };
            builder = builder.chunk(oxiarc_png::chunk::tRNS, &payload);
        }
        let stride = builder.row_stride(4);
        let samples = vec![0x55u8; stride * 2];
        let png = builder.build_from_samples(&samples);

        let mut decoder = oxiarc_png::Decoder::new(&png[..]);
        decoder.set_transformations(Transformations::EXPAND);
        let mut reader = decoder.read_info().expect("read_info");
        assert_eq!(
            reader.output_color_type(),
            (want_ct, want_depth),
            "{ct:?}/{depth:?} trns={trns}"
        );
        let size = reader.output_buffer_size().expect("size");
        let expected_line = want_ct.samples() * usize::from(want_depth as u8) * 4;
        assert_eq!(size, expected_line.div_ceil(8) * 2);
        let mut buf = vec![0u8; size];
        let info = reader.next_frame(&mut buf).expect("next_frame");
        assert_eq!((info.color_type, info.bit_depth), (want_ct, want_depth));
        assert_eq!(info.line_size * info.height as usize, size);
    }
}

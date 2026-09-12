//! Exact-byte characterisation of every [`DynamicImage`] colour conversion.
//!
//! Every `to_*`/`into_*` method is pinned here against a hand-checked golden
//! table, one row per `DynamicImage` variant, so that a rewrite of the
//! conversion internals (for speed, say) has to prove it changed nothing —
//! "the round trip still works" is far too weak a gate for code whose whole
//! job is producing exact sample values.
//!
//! The two-pixel fixtures deliberately mix saturated, zero, mid-range and
//! (for the float variants) out-of-`[0, 1]` samples, so a clamp or a
//! rounding-mode change cannot slip through.

use oxiarc_image::{DynamicImage, ImageBuffer};

/// One golden row: the fixture, plus the exact samples every conversion
/// must produce from it.
struct Row {
    name: &'static str,
    image: DynamicImage,
    rgba16: &'static [u16],
    rgba8: &'static [u8],
    rgb16: &'static [u16],
    rgb8: &'static [u8],
    luma8: &'static [u8],
    luma16: &'static [u16],
    luma_alpha8: &'static [u8],
    luma_alpha16: &'static [u16],
}

fn golden() -> Vec<Row> {
    vec![
        Row {
            name: "ImageLuma8",
            image: DynamicImage::ImageLuma8(
                ImageBuffer::from_raw(2, 1, vec![0u8, 200]).expect("l8 fixture"),
            ),
            rgba16: &[0, 0, 0, 65535, 51400, 51400, 51400, 65535],
            rgba8: &[0, 0, 0, 255, 200, 200, 200, 255],
            rgb16: &[0, 0, 0, 51400, 51400, 51400],
            rgb8: &[0, 0, 0, 200, 200, 200],
            luma8: &[0, 200],
            luma16: &[0, 51400],
            luma_alpha8: &[0, 255, 200, 255],
            luma_alpha16: &[0, 65535, 51400, 65535],
        },
        Row {
            name: "ImageLumaA8",
            image: DynamicImage::ImageLumaA8(
                ImageBuffer::from_raw(2, 1, vec![10u8, 128, 250, 0]).expect("la8 fixture"),
            ),
            rgba16: &[2570, 2570, 2570, 32896, 64250, 64250, 64250, 0],
            rgba8: &[10, 10, 10, 128, 250, 250, 250, 0],
            rgb16: &[2570, 2570, 2570, 64250, 64250, 64250],
            rgb8: &[10, 10, 10, 250, 250, 250],
            luma8: &[10, 250],
            luma16: &[2570, 64250],
            luma_alpha8: &[10, 128, 250, 0],
            luma_alpha16: &[2570, 32896, 64250, 0],
        },
        Row {
            name: "ImageRgb8",
            image: DynamicImage::ImageRgb8(
                ImageBuffer::from_raw(2, 1, vec![255u8, 0, 0, 1, 2, 3]).expect("rgb8 fixture"),
            ),
            rgba16: &[65535, 0, 0, 65535, 257, 514, 771, 65535],
            rgba8: &[255, 0, 0, 255, 1, 2, 3, 255],
            rgb16: &[65535, 0, 0, 257, 514, 771],
            rgb8: &[255, 0, 0, 1, 2, 3],
            luma8: &[54, 1],
            luma16: &[13932, 477],
            luma_alpha8: &[54, 255, 1, 255],
            luma_alpha16: &[13932, 65535, 477, 65535],
        },
        Row {
            name: "ImageRgba8",
            image: DynamicImage::ImageRgba8(
                ImageBuffer::from_raw(2, 1, vec![255u8, 0, 0, 128, 4, 5, 6, 255])
                    .expect("rgba8 fixture"),
            ),
            rgba16: &[65535, 0, 0, 32896, 1028, 1285, 1542, 65535],
            rgba8: &[255, 0, 0, 128, 4, 5, 6, 255],
            rgb16: &[65535, 0, 0, 1028, 1285, 1542],
            rgb8: &[255, 0, 0, 4, 5, 6],
            luma8: &[54, 4],
            luma16: &[13932, 1248],
            luma_alpha8: &[54, 128, 4, 255],
            luma_alpha16: &[13932, 32896, 1248, 65535],
        },
        Row {
            name: "ImageLuma16",
            image: DynamicImage::ImageLuma16(
                ImageBuffer::from_raw(2, 1, vec![0u16, 40000]).expect("l16 fixture"),
            ),
            rgba16: &[0, 0, 0, 65535, 40000, 40000, 40000, 65535],
            rgba8: &[0, 0, 0, 255, 156, 156, 156, 255],
            rgb16: &[0, 0, 0, 40000, 40000, 40000],
            rgb8: &[0, 0, 0, 156, 156, 156],
            luma8: &[0, 156],
            luma16: &[0, 40000],
            luma_alpha8: &[0, 255, 156, 255],
            luma_alpha16: &[0, 65535, 40000, 65535],
        },
        Row {
            name: "ImageLumaA16",
            image: DynamicImage::ImageLumaA16(
                ImageBuffer::from_raw(2, 1, vec![1000u16, 65535, 65535, 0]).expect("la16 fixture"),
            ),
            rgba16: &[1000, 1000, 1000, 65535, 65535, 65535, 65535, 0],
            rgba8: &[4, 4, 4, 255, 255, 255, 255, 0],
            rgb16: &[1000, 1000, 1000, 65535, 65535, 65535],
            rgb8: &[4, 4, 4, 255, 255, 255],
            luma8: &[4, 255],
            luma16: &[1000, 65535],
            luma_alpha8: &[4, 255, 255, 0],
            luma_alpha16: &[1000, 65535, 65535, 0],
        },
        Row {
            name: "ImageRgb16",
            image: DynamicImage::ImageRgb16(
                ImageBuffer::from_raw(2, 1, vec![65535u16, 0, 257, 1, 2, 3])
                    .expect("rgb16 fixture"),
            ),
            rgba16: &[65535, 0, 257, 65535, 1, 2, 3, 65535],
            rgba8: &[255, 0, 1, 255, 0, 0, 0, 255],
            rgb16: &[65535, 0, 257, 1, 2, 3],
            rgb8: &[255, 0, 1, 0, 0, 0],
            luma8: &[54, 0],
            luma16: &[13951, 1],
            luma_alpha8: &[54, 255, 0, 255],
            luma_alpha16: &[13951, 65535, 1, 65535],
        },
        Row {
            name: "ImageRgba16",
            image: DynamicImage::ImageRgba16(
                ImageBuffer::from_raw(2, 1, vec![65535u16, 0, 257, 32768, 4, 5, 6, 65535])
                    .expect("rgba16 fixture"),
            ),
            rgba16: &[65535, 0, 257, 32768, 4, 5, 6, 65535],
            rgba8: &[255, 0, 1, 128, 0, 0, 0, 255],
            rgb16: &[65535, 0, 257, 4, 5, 6],
            rgb8: &[255, 0, 1, 0, 0, 0],
            luma8: &[54, 0],
            luma16: &[13951, 4],
            luma_alpha8: &[54, 128, 0, 255],
            luma_alpha16: &[13951, 32768, 4, 65535],
        },
        Row {
            name: "ImageRgb32F",
            image: DynamicImage::ImageRgb32F(
                ImageBuffer::from_raw(2, 1, vec![1.0f32, 0.0, 0.25, 0.5, 2.0, -1.0])
                    .expect("rgb32f fixture"),
            ),
            rgba16: &[65535, 0, 16384, 65535, 32768, 65535, 0, 65535],
            rgba8: &[255, 0, 64, 255, 128, 255, 0, 255],
            rgb16: &[65535, 0, 16384, 32768, 65535, 0],
            rgb8: &[255, 0, 64, 128, 255, 0],
            luma8: &[58, 209],
            luma16: &[15115, 53837],
            luma_alpha8: &[58, 255, 209, 255],
            luma_alpha16: &[15115, 65535, 53837, 65535],
        },
        Row {
            name: "ImageRgba32F",
            image: DynamicImage::ImageRgba32F(
                ImageBuffer::from_raw(2, 1, vec![1.0f32, 0.0, 0.25, 0.5, 0.5, 0.75, 1.0, 1.0])
                    .expect("rgba32f fixture"),
            ),
            rgba16: &[65535, 0, 16384, 32768, 32768, 49151, 65535, 65535],
            rgba8: &[255, 0, 64, 128, 128, 191, 255, 255],
            rgb16: &[65535, 0, 16384, 32768, 49151, 65535],
            rgb8: &[255, 0, 64, 128, 191, 255],
            luma8: &[58, 182],
            luma16: &[15115, 46850],
            luma_alpha8: &[58, 128, 182, 255],
            luma_alpha16: &[15115, 32768, 46850, 65535],
        },
    ]
}

#[test]
fn every_conversion_produces_exactly_the_golden_samples() {
    let mut mismatches: Vec<String> = Vec::new();
    for row in golden() {
        let name = row.name;
        check_u16(
            &mut mismatches,
            name,
            "to_rgba16",
            &row.image.to_rgba16().into_raw(),
            row.rgba16,
        );
        check_u8(
            &mut mismatches,
            name,
            "to_rgba8",
            &row.image.to_rgba8().into_raw(),
            row.rgba8,
        );
        check_u16(
            &mut mismatches,
            name,
            "to_rgb16",
            &row.image.to_rgb16().into_raw(),
            row.rgb16,
        );
        check_u8(
            &mut mismatches,
            name,
            "to_rgb8",
            &row.image.to_rgb8().into_raw(),
            row.rgb8,
        );
        check_u8(
            &mut mismatches,
            name,
            "to_luma8",
            &row.image.to_luma8().into_raw(),
            row.luma8,
        );
        check_u16(
            &mut mismatches,
            name,
            "to_luma16",
            &row.image.to_luma16().into_raw(),
            row.luma16,
        );
        check_u8(
            &mut mismatches,
            name,
            "to_luma_alpha8",
            &row.image.to_luma_alpha8().into_raw(),
            row.luma_alpha8,
        );
        check_u16(
            &mut mismatches,
            name,
            "to_luma_alpha16",
            &row.image.to_luma_alpha16().into_raw(),
            row.luma_alpha16,
        );
    }
    assert!(
        mismatches.is_empty(),
        "{} conversion mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn check_u8(out: &mut Vec<String>, name: &str, method: &str, got: &[u8], want: &[u8]) {
    if got != want {
        out.push(format!("{name}::{method}: got {got:?}, want {want:?}"));
    }
}

fn check_u16(out: &mut Vec<String>, name: &str, method: &str, got: &[u16], want: &[u16]) {
    if got != want {
        out.push(format!("{name}::{method}: got {got:?}, want {want:?}"));
    }
}

/// The four `into_*` methods must agree byte-for-byte with their `to_*`
/// twins — the only difference is whether the source buffer is reused.
#[test]
fn into_conversions_agree_with_the_borrowing_ones() {
    for row in golden() {
        let name = row.name;
        assert_eq!(
            row.image.clone().into_rgba8().as_raw(),
            row.rgba8,
            "{name} into_rgba8"
        );
        assert_eq!(
            row.image.clone().into_rgb8().as_raw(),
            row.rgb8,
            "{name} into_rgb8"
        );
        assert_eq!(
            row.image.clone().into_luma8().as_raw(),
            row.luma8,
            "{name} into_luma8"
        );
        assert_eq!(
            row.image.clone().into_rgba16().as_raw(),
            row.rgba16,
            "{name} into_rgba16"
        );
    }
}

/// Every conversion must preserve the source dimensions exactly, including
/// the degenerate zero-width/zero-height cases a decoder can legitimately
/// produce for an empty image.
#[test]
fn conversions_preserve_dimensions_including_empty_images() {
    for (w, h) in [(2u32, 1u32), (1, 1), (0, 5), (5, 0), (0, 0)] {
        let image = DynamicImage::ImageRgba8(
            ImageBuffer::from_raw(w, h, vec![0u8; (w as usize) * (h as usize) * 4])
                .expect("fixture"),
        );
        assert_eq!(image.to_rgba16().dimensions(), (w, h));
        assert_eq!(image.to_rgba8().dimensions(), (w, h));
        assert_eq!(image.to_rgb16().dimensions(), (w, h));
        assert_eq!(image.to_rgb8().dimensions(), (w, h));
        assert_eq!(image.to_luma8().dimensions(), (w, h));
        assert_eq!(image.to_luma16().dimensions(), (w, h));
        assert_eq!(image.to_luma_alpha8().dimensions(), (w, h));
        assert_eq!(image.to_luma_alpha16().dimensions(), (w, h));
    }
}

/// Both luma coefficient sets — the sRGB / Rec. 709 one this crate now uses
/// and the BT.601 one it used before — sum to exactly their own divisor, so
/// for a pixel whose three channels are equal the weighted sum *is* that
/// channel. That is why switching the weights moved no JPEG output byte:
/// `DynamicImage::write_to(.., ImageFormat::Jpeg)` routes only *grayscale*
/// sources through `to_luma8` (coloured ones go through `to_rgb8`), and a
/// grayscale source is exactly the equal-channel case.
///
/// Pinned here so the blast radius of any future weight change stays a fact
/// rather than an argument.
#[test]
fn a_grayscale_source_is_luma_weight_independent() {
    for v in 0..=u8::MAX {
        let luma =
            DynamicImage::ImageLuma8(ImageBuffer::from_raw(1, 1, vec![v]).expect("luma8 fixture"));
        assert_eq!(luma.to_luma8().into_raw(), vec![v], "Luma8 {v}");

        let luma_alpha = DynamicImage::ImageLumaA8(
            ImageBuffer::from_raw(1, 1, vec![v, 128]).expect("lumaa8 fixture"),
        );
        assert_eq!(luma_alpha.to_luma8().into_raw(), vec![v], "LumaA8 {v}");

        // And the equal-channel RGB case, which is what a grayscale source
        // becomes on its way through `to_rgb8`.
        let grey_rgb = DynamicImage::ImageRgb8(
            ImageBuffer::from_raw(1, 1, vec![v, v, v]).expect("rgb8 fixture"),
        );
        assert_eq!(grey_rgb.to_luma8().into_raw(), vec![v], "Rgb8 grey {v}");
    }

    for v in [0u16, 1, 257, 1000, 32768, 40000, 65534, 65535] {
        let luma = DynamicImage::ImageLuma16(
            ImageBuffer::from_raw(1, 1, vec![v]).expect("luma16 fixture"),
        );
        assert_eq!(luma.to_luma16().into_raw(), vec![v], "Luma16 {v}");
        let grey_rgb = DynamicImage::ImageRgb16(
            ImageBuffer::from_raw(1, 1, vec![v, v, v]).expect("rgb16 fixture"),
        );
        assert_eq!(grey_rgb.to_luma16().into_raw(), vec![v], "Rgb16 grey {v}");
    }
}

/// A grayscale image encoded to JPEG must produce byte-identical output to
/// what it produced under the previous luma weights. The test above proves
/// that algebraically at the sample level; this one proves the encoder path
/// really is the grayscale one, by checking that the JPEG round trip of a
/// grayscale source comes back as `L8` (not `Rgb8`) and close to the source.
#[test]
fn write_to_jpeg_routes_a_grayscale_source_through_the_luma_path() {
    use oxiarc_image::{ColorType, ImageFormat};
    let source = DynamicImage::ImageLuma8(
        ImageBuffer::from_raw(8, 8, vec![137u8; 64]).expect("luma8 fixture"),
    );
    let mut out = Vec::new();
    source
        .write_to(std::io::Cursor::new(&mut out), ImageFormat::Jpeg)
        .expect("encode");
    let decoded = oxiarc_image::load_from_memory(&out).expect("decode");
    assert_eq!(decoded.color(), ColorType::L8);
    for &v in decoded.to_luma8().as_raw() {
        assert!(
            v.abs_diff(137) <= 4,
            "grayscale JPEG round trip drifted to {v}"
        );
    }
}

/// `to_rgba8` on a float source narrows in two steps (`f32 -> u16 -> u8`)
/// because that is what routing through `to_rgba16` did before the
/// conversions were rewritten, and the golden table above pins that
/// behaviour. `image` 0.25 narrows a float straight to `u8`
/// (`(v.clamp(0, 1) * 255).round()`). This test settles whether the two ever
/// disagree, rather than leaving it an open question in the deviations list.
#[test]
fn two_step_float_narrowing_agrees_with_the_single_step_form() {
    fn single_step(v: f32) -> u8 {
        (f64::from(v.clamp(0.0, 1.0)) * 255.0).round() as u8
    }

    let mut samples: Vec<f32> = Vec::new();
    // Every 8-bit level, exactly.
    samples.extend((0..=255u32).map(|k| k as f32 / 255.0));
    // A stride through the 16-bit levels, including both endpoints.
    samples.extend((0..=65535u32).step_by(37).map(|m| m as f32 / 65535.0));
    samples.push(1.0);
    // Deterministic pseudo-random values in [0, 1], plus the out-of-range
    // and non-finite cases both forms have to clamp.
    let mut state = 0x1234_5678_9ABC_DEF0u64;
    for _ in 0..20_000 {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let bits = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as u32;
        samples.push(bits as f32 / f32::from(u16::MAX));
    }
    samples.extend([-1.0f32, 2.0, f32::MIN, f32::MAX]);

    let mut disagreements = 0usize;
    let mut first: Option<(f32, u8, u8)> = None;
    for &v in &samples {
        let image = DynamicImage::ImageRgb32F(
            ImageBuffer::from_raw(1, 1, vec![v, v, v]).expect("rgb32f fixture"),
        );
        let two_step = image.to_rgba8().into_raw()[0];
        let one_step = single_step(v);
        if two_step != one_step {
            disagreements += 1;
            if first.is_none() {
                first = Some((v, two_step, one_step));
            }
        }
    }
    assert_eq!(
        disagreements,
        0,
        "float narrowing forms disagree on {}/{} samples, first {:?}",
        disagreements,
        samples.len(),
        first
    );
}

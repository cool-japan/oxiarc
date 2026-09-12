//! Property tests.

mod common;

use common::PngBuilder;
use oxiarc_png::filter::{RowFilter, apply_filter, unfilter};
use oxiarc_png::header::Ihdr;
use oxiarc_png::{
    ApngEncoder, BitDepth, BlendOp, BytesPerPixel, ColorType, DisposeOp, Encoder, FrameControl,
    Interlace,
};
use proptest::prelude::*;

const COMBINATIONS: &[(ColorType, BitDepth)] = &[
    (ColorType::Grayscale, BitDepth::One),
    (ColorType::Grayscale, BitDepth::Two),
    (ColorType::Grayscale, BitDepth::Four),
    (ColorType::Grayscale, BitDepth::Eight),
    (ColorType::Grayscale, BitDepth::Sixteen),
    (ColorType::Rgb, BitDepth::Eight),
    (ColorType::Rgb, BitDepth::Sixteen),
    (ColorType::Indexed, BitDepth::One),
    (ColorType::Indexed, BitDepth::Two),
    (ColorType::Indexed, BitDepth::Four),
    (ColorType::Indexed, BitDepth::Eight),
    (ColorType::GrayscaleAlpha, BitDepth::Eight),
    (ColorType::GrayscaleAlpha, BitDepth::Sixteen),
    (ColorType::Rgba, BitDepth::Eight),
    (ColorType::Rgba, BitDepth::Sixteen),
];

const BPPS: &[BytesPerPixel] = &[
    BytesPerPixel::One,
    BytesPerPixel::Two,
    BytesPerPixel::Three,
    BytesPerPixel::Four,
    BytesPerPixel::Six,
    BytesPerPixel::Eight,
];

/// Compare two packed rows, ignoring the undefined padding bits at the end.
fn rows_match(a: &[u8], b: &[u8], bits: usize) -> bool {
    let full = bits / 8;
    if a[..full] != b[..full] {
        return false;
    }
    if bits % 8 == 0 {
        return true;
    }
    let mask = 0xFFu8 << (8 - bits % 8);
    a[full] & mask == b[full] & mask
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Every legal image shape survives a build-and-decode round trip.
    #[test]
    fn prop_round_trip(
        combo in 0usize..COMBINATIONS.len(),
        width in 1u32..40,
        height in 1u32..40,
        interlace in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let (color_type, bit_depth) = COMBINATIONS[combo];
        let mut builder = PngBuilder::new(width, height, color_type, bit_depth)
            .interlaced(interlace);
        if color_type == ColorType::Indexed {
            let palette: Vec<u8> = (0..256u16)
                .flat_map(|i| [i as u8, (i * 3) as u8, (i * 5) as u8])
                .collect();
            builder = builder.chunk(oxiarc_png::chunk::PLTE, &palette);
        }
        let stride = builder.row_stride(width);
        let mut rng = common::Rng::new(seed);
        let samples = rng.bytes(stride * height as usize);
        let png = builder.build_from_samples(&samples);
        let image = oxiarc_png::decode(&png).expect("decode");
        let bits = width as usize * color_type.samples() * usize::from(bit_depth as u8);
        for y in 0..height as usize {
            prop_assert!(rows_match(
                &image.data[y * stride..(y + 1) * stride],
                &samples[y * stride..(y + 1) * stride],
                bits,
            ));
        }
    }

    /// Filtering and unfiltering are exact inverses for every filter and stride.
    #[test]
    fn prop_filter_inverse(
        raw in proptest::collection::vec(any::<u8>(), 0..200),
        previous in proptest::collection::vec(any::<u8>(), 0..200),
        bpp_index in 0usize..BPPS.len(),
        filter_index in 0usize..5,
        use_previous in any::<bool>(),
    ) {
        let bpp = BPPS[bpp_index];
        let filter = RowFilter::ALL[filter_index];
        let previous: &[u8] = if use_previous && previous.len() == raw.len() {
            &previous
        } else {
            &[]
        };
        let mut filtered = apply_filter(filter, bpp, previous, &raw);
        unfilter(filter, bpp, previous, &mut filtered);
        prop_assert_eq!(filtered, raw);
    }

    /// Feeding the file in arbitrary pieces gives the same image.
    #[test]
    fn prop_chunked_feed_equivalence(
        width in 1u32..24,
        height in 1u32..24,
        interlace in any::<bool>(),
        piece in 1usize..64,
        idat_split in 1usize..64,
        seed in any::<u64>(),
    ) {
        let builder = PngBuilder::new(width, height, ColorType::Rgba, BitDepth::Eight)
            .interlaced(interlace)
            .idat_split(idat_split);
        let stride = builder.row_stride(width);
        let mut rng = common::Rng::new(seed);
        let samples = rng.bytes(stride * height as usize);
        let png = builder.build_from_samples(&samples);
        let whole = oxiarc_png::decode(&png).expect("whole");
        let piecewise =
            oxiarc_png::decode_reader(common::ChunkedReader::new(&png, piece)).expect("pieces");
        prop_assert_eq!(whole.data, piecewise.data);
    }

    /// The a-priori raw byte count equals what an encoder actually produces.
    #[test]
    fn prop_expected_raw_matches_the_stream(
        combo in 0usize..COMBINATIONS.len(),
        width in 1u32..64,
        height in 1u32..64,
        interlace in any::<bool>(),
    ) {
        let (color_type, bit_depth) = COMBINATIONS[combo];
        let builder = PngBuilder::new(width, height, color_type, bit_depth)
            .interlaced(interlace);
        let stride = builder.row_stride(width);
        let samples = vec![0u8; stride * height as usize];
        let raw = builder.filter_samples(&samples);
        prop_assert_eq!(raw.len(), builder.expected_raw());

        let ihdr = Ihdr {
            width,
            height,
            bit_depth,
            color_type,
            interlace: if interlace { Interlace::Adam7 } else { Interlace::None },
        };
        prop_assert_eq!(ihdr.expected_raw_bytes(), Some(raw.len() as u64));
    }

    /// Garbage never panics, whatever it looks like.
    #[test]
    fn prop_arbitrary_input_never_panics(
        data in proptest::collection::vec(any::<u8>(), 0..512),
        with_signature in any::<bool>(),
    ) {
        let mut input = Vec::new();
        if with_signature {
            input.extend_from_slice(&oxiarc_png::chunk::SIGNATURE);
        }
        input.extend_from_slice(&data);
        let _ = oxiarc_png::decode(&input);
        let _ = oxiarc_png::peek_info(&input);
    }

    /// Every legal image shape survives a build-through-the-*real*-encoder,
    /// decode round trip -- the property-test counterpart to
    /// `tests/encoder_roundtrip.rs`'s deterministic matrix, so the encoder
    /// gets the same randomized-shape coverage the decoder already has via
    /// `prop_round_trip` above.
    #[test]
    fn prop_encode_decode_roundtrip(
        combo in 0usize..COMBINATIONS.len(),
        width in 1u32..40,
        height in 1u32..40,
        interlace in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let (color_type, bit_depth) = COMBINATIONS[combo];
        let ihdr = Ihdr {
            width,
            height,
            bit_depth,
            color_type,
            interlace: if interlace { Interlace::Adam7 } else { Interlace::None },
        };
        let stride = ihdr.row_stride();
        let mut rng = common::Rng::new(seed);
        let samples = rng.bytes(stride * height as usize);

        let mut out = Vec::new();
        let mut enc = Encoder::new(&mut out, width, height);
        enc.set_color(color_type);
        enc.set_depth(bit_depth);
        enc.set_interlaced(interlace);
        if color_type == ColorType::Indexed {
            let palette: Vec<u8> = (0..256u16)
                .flat_map(|i| [i as u8, (i * 3) as u8, (i * 5) as u8])
                .collect();
            enc.set_palette(palette);
        }
        let mut w = enc.write_header().expect("write_header");
        w.write_image_data(&samples).expect("write_image_data");
        w.finish().expect("finish");

        let image = oxiarc_png::decode(&out).expect("decode");
        prop_assert_eq!(image.width, width);
        prop_assert_eq!(image.height, height);
        prop_assert_eq!(image.color_type, color_type);
        prop_assert_eq!(image.bit_depth, bit_depth);

        let bits = width as usize * color_type.samples() * usize::from(bit_depth as u8);
        for y in 0..height as usize {
            prop_assert!(rows_match(
                &image.data[y * stride..(y + 1) * stride],
                &samples[y * stride..(y + 1) * stride],
                bits,
            ));
        }
    }

    /// A random sequence of APNG frames -- random dimensions, offsets,
    /// dispose/blend operators and pixel bytes -- never panics the
    /// compositor and always yields a canvas of exactly the file's `IHDR`
    /// size, whatever the dispose/blend combination.
    #[test]
    fn prop_apng_compose_never_panics_and_canvas_is_always_full_size(
        canvas_w in 1u32..12,
        canvas_h in 1u32..12,
        frame_count in 1usize..5,
        seed in any::<u64>(),
    ) {
        let mut rng = common::Rng::new(seed);
        let mut buf = Vec::new();
        let mut enc = ApngEncoder::new(&mut buf, canvas_w, canvas_h, frame_count as u32, 1)
            .expect("new");
        for i in 0..frame_count {
            // Every frame covers the whole canvas: this property is about
            // dispose/blend/pixel-content robustness, not region-bounds
            // validation (already covered by `apng.rs`'s own unit tests).
            let dispose = match rng.next_u8() % 3 {
                0 => DisposeOp::None,
                1 => DisposeOp::Background,
                _ => DisposeOp::Previous,
            };
            let blend = if rng.next_u8() % 2 == 0 {
                BlendOp::Source
            } else {
                BlendOp::Over
            };
            let ctl = FrameControl {
                width: canvas_w,
                height: canvas_h,
                dispose_op: dispose,
                blend_op: blend,
                delay_num: 1,
                delay_den: 100,
                ..FrameControl::default()
            };
            let pixels = rng.bytes(canvas_w as usize * canvas_h as usize * 4);
            enc.write_frame(&ctl, &pixels)
                .unwrap_or_else(|e| panic!("frame {i}: {e}"));
        }
        enc.finish().expect("finish");

        let mut dec = oxiarc_png::ApngDecoder::new(&buf[..]).expect("open");
        let mut composed = 0usize;
        while let Some(frame) = dec.next_composed().expect("compose") {
            prop_assert_eq!(frame.width, canvas_w);
            prop_assert_eq!(frame.height, canvas_h);
            prop_assert_eq!(frame.canvas.len(), canvas_w as usize * canvas_h as usize * 4);
            composed += 1;
        }
        prop_assert_eq!(composed, frame_count);
    }
}

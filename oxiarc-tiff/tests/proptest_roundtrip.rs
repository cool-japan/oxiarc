//! Property-based writer -> reader round-trips.
//!
//! `proptest` generates the geometry; every lossless configuration must come
//! back bit-identical, and every generated file must parse without panicking.

use oxiarc_tiff::tags::{FillOrder, PlanarConfiguration, SampleFormat};
use oxiarc_tiff::{
    ColorType, Compression, Decoder, Encoder, Endian, ImageSpec, Layout, Predictor, SampleType,
    VariantChoice,
};
use proptest::prelude::*;
use std::io::Cursor;

/// The bit depths the writer supports for a uniform-depth image.
const DEPTHS: [u16; 8] = [1, 2, 4, 8, 12, 16, 24, 32];

fn slot_for(bits: u16, format: SampleFormat) -> SampleType {
    SampleType::resolve(bits, format).unwrap_or(SampleType::U8)
}

/// Builds a deterministic native-endian sample buffer for the given geometry.
fn make_data(count: usize, bits: u16, format: SampleFormat, seed: u64) -> Vec<u8> {
    let slot = slot_for(bits, format);
    let max = if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let mut out = Vec::with_capacity(count * slot.byte_width());
    for i in 0..count {
        let raw = (seed.wrapping_mul(i as u64 + 1) ^ (i as u64) << 3) & max;
        match slot {
            SampleType::U8 => out.push(raw as u8),
            SampleType::I8 => out.push(raw as u8),
            SampleType::U16 | SampleType::I16 | SampleType::F16 => {
                out.extend_from_slice(&(raw as u16).to_ne_bytes());
            }
            SampleType::U32 | SampleType::I32 => {
                out.extend_from_slice(&(raw as u32).to_ne_bytes());
            }
            SampleType::F32 => {
                out.extend_from_slice(&((raw % 1000) as f32 * 0.5).to_ne_bytes());
            }
            SampleType::F64 => {
                out.extend_from_slice(&((raw % 1000) as f64 * 0.25).to_ne_bytes());
            }
            SampleType::U64 | SampleType::I64 => out.extend_from_slice(&raw.to_ne_bytes()),
        }
    }
    out
}

proptest! {
    // No failure-persistence file: the test directory has no crate root, and a
    // regression corpus must never be written into the repository.
    #![proptest_config(ProptestConfig {
        cases: 192,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn uncompressed_and_packbits_round_trip_bit_exactly(
        width in 1u32..=40,
        height in 1u32..=40,
        spp in 1usize..=5,
        depth_index in 0usize..DEPTHS.len(),
        rows_per_strip in 1u32..=40,
        packbits in any::<bool>(),
        big_endian in any::<bool>(),
        bigtiff in any::<bool>(),
        planar in any::<bool>(),
        lsb_first in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let bits = DEPTHS[depth_index];
        let colour = ColorType::Multiband {
            bit_depth: bits as u8,
            num_samples: spp as u16,
        };
        let data = make_data((width * height) as usize * spp, bits, SampleFormat::Uint, seed);
        let mut spec = ImageSpec::new(width, height, colour)
            .with_layout(Layout::Strips { rows_per_strip })
            .with_compression(if packbits {
                Compression::PackBits
            } else {
                Compression::None
            });
        if planar {
            spec = spec.with_planar(PlanarConfiguration::Planar);
        }
        if lsb_first && bits < 8 {
            spec = spec.with_fill_order(FillOrder::Lsb2Msb);
        }
        prop_assume!(spec.validate().is_ok());

        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer)
            .expect("encoder")
            .with_endian(if big_endian { Endian::Big } else { Endian::Little })
            .with_variant(if bigtiff { VariantChoice::Big } else { VariantChoice::Classic });
        encoder.write_image(&spec, &data).expect("write");
        encoder.finish().expect("finish");

        let mut decoder = Decoder::new(Cursor::new(buffer.into_inner())).expect("decoder");
        prop_assert_eq!(decoder.dimensions().expect("dims"), (width, height));
        let samples = decoder.read_image().expect("read");
        prop_assert_eq!(samples.to_native_bytes(), data);
    }

    #[test]
    fn tiled_images_round_trip_bit_exactly(
        width in 1u32..=64,
        height in 1u32..=64,
        tile_units_w in 1u32..=3,
        tile_units_h in 1u32..=3,
        spp in 1usize..=4,
        packbits in any::<bool>(),
        planar in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let data = make_data((width * height) as usize * spp, 8, SampleFormat::Uint, seed);
        let colour = ColorType::Multiband {
            bit_depth: 8,
            num_samples: spp as u16,
        };
        let mut spec = ImageSpec::new(width, height, colour)
            .with_layout(Layout::Tiles {
                width: tile_units_w * 16,
                length: tile_units_h * 16,
            })
            .with_compression(if packbits {
                Compression::PackBits
            } else {
                Compression::None
            });
        if planar {
            spec = spec.with_planar(PlanarConfiguration::Planar);
        }
        prop_assume!(spec.validate().is_ok());

        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(&spec, &data).expect("write");
        encoder.finish().expect("finish");

        let mut decoder = Decoder::new(Cursor::new(buffer.into_inner())).expect("decoder");
        let samples = decoder.read_image().expect("read");
        prop_assert_eq!(samples.to_native_bytes(), data);
    }

    #[test]
    fn predictors_round_trip_bit_exactly(
        width in 1u32..=24,
        height in 1u32..=24,
        spp in 1usize..=4,
        wide in any::<bool>(),
        float in any::<bool>(),
        planar in any::<bool>(),
        big_endian in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let (bits, format, predictor) = if float {
            (32u16, SampleFormat::IeeeFp, Predictor::FloatingPoint)
        } else if wide {
            (16u16, SampleFormat::Uint, Predictor::Horizontal)
        } else {
            (8u16, SampleFormat::Uint, Predictor::Horizontal)
        };
        let data = make_data((width * height) as usize * spp, bits, format, seed);
        let colour = ColorType::Multiband {
            bit_depth: bits as u8,
            num_samples: spp as u16,
        };
        let mut spec = ImageSpec::new(width, height, colour)
            .with_sample_format(format)
            .with_predictor(predictor)
            .with_layout(Layout::Strips { rows_per_strip: height.min(4).max(1) });
        if planar {
            spec = spec.with_planar(PlanarConfiguration::Planar);
        }
        prop_assume!(spec.validate().is_ok());

        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer)
            .expect("encoder")
            .with_endian(if big_endian { Endian::Big } else { Endian::Little });
        encoder.write_image(&spec, &data).expect("write");
        encoder.finish().expect("finish");

        let mut decoder = Decoder::new(Cursor::new(buffer.into_inner())).expect("decoder");
        let samples = decoder.read_image().expect("read");
        prop_assert_eq!(samples.to_native_bytes(), data);
    }

    #[test]
    fn regions_agree_with_the_whole_image(
        width in 1u32..=32,
        height in 1u32..=32,
        x_frac in 0u32..1024,
        y_frac in 0u32..1024,
        w_frac in 0u32..1024,
        h_frac in 0u32..1024,
        tiled in any::<bool>(),
        seed in any::<u64>(),
    ) {
        // Derive the window from fractions so every generated case is inside
        // the image; `prop_assume` would reject almost everything here.
        let x = x_frac % width;
        let y = y_frac % height;
        let w = 1 + w_frac % (width - x);
        let h = 1 + h_frac % (height - y);
        let data = make_data((width * height) as usize, 8, SampleFormat::Uint, seed);
        let spec = ImageSpec::new(width, height, ColorType::Gray(8)).with_layout(if tiled {
            Layout::Tiles { width: 16, length: 16 }
        } else {
            Layout::Strips { rows_per_strip: 3 }
        });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(&spec, &data).expect("write");
        encoder.finish().expect("finish");

        let mut decoder = Decoder::new(Cursor::new(buffer.into_inner())).expect("decoder");
        let region = decoder.read_region(x, y, w, h).expect("region");
        let got = region.as_u8().expect("u8");
        for row in 0..h as usize {
            for col in 0..w as usize {
                let expected = data[(y as usize + row) * width as usize + x as usize + col];
                prop_assert_eq!(got[row * w as usize + col], expected);
            }
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        if let Ok(decoder) = Decoder::new(Cursor::new(bytes)) {
            let mut decoder = decoder;
            let _ = decoder.image_count();
            let _ = decoder.info().map(|i| (i.width, i.height));
            let _ = decoder.read_image();
            let _ = decoder.all_tags();
        }
    }
}

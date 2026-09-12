//! End-to-end decoding of hand-built files.

mod common;

use common::{PngBuilder, Rng};
use oxiarc_png::{BitDepth, ColorType, Transformations};

/// Every legal (colour type, bit depth) pair.
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

fn palette() -> Vec<u8> {
    (0..256u16)
        .flat_map(|i| {
            [
                (i % 256) as u8,
                ((i * 7) % 256) as u8,
                ((i * 13) % 256) as u8,
            ]
        })
        .collect()
}

#[test]
fn every_combination_round_trips_through_the_decoder() {
    for &(color_type, bit_depth) in COMBINATIONS {
        for interlace in [false, true] {
            for (w, h) in [(1u32, 1u32), (3, 1), (1, 3), (7, 7), (9, 5), (16, 4)] {
                let mut builder =
                    PngBuilder::new(w, h, color_type, bit_depth).interlaced(interlace);
                if color_type == ColorType::Indexed {
                    builder = builder.chunk(oxiarc_png::chunk::PLTE, &palette());
                }
                let stride = builder.row_stride(w);
                let mut rng = Rng::new(0xABCD_1234 ^ u64::from(w * 31 + h));
                let samples = rng.bytes(stride * h as usize);
                let png = builder.build_from_samples(&samples);
                let image = oxiarc_png::decode(&png).unwrap_or_else(|e| {
                    panic!("{color_type:?}/{bit_depth:?} {w}x{h} interlace={interlace}: {e}")
                });
                assert_eq!(image.width, w);
                assert_eq!(image.height, h);
                assert_eq!(image.color_type, color_type);
                assert_eq!(image.bit_depth, bit_depth);
                assert_eq!(
                    image.data.len(),
                    stride * h as usize,
                    "{color_type:?}/{bit_depth:?} {w}x{h} interlace={interlace}"
                );
                // Sub-byte rows have undefined padding bits; compare only the
                // bits the image actually defines.
                let bits = w as usize * color_type.samples() * usize::from(bit_depth as u8);
                for y in 0..h as usize {
                    let got = &image.data[y * stride..(y + 1) * stride];
                    let want = &samples[y * stride..(y + 1) * stride];
                    let full = bits / 8;
                    assert_eq!(
                        &got[..full],
                        &want[..full],
                        "row {y} of {color_type:?}/{bit_depth:?} {w}x{h} interlace={interlace}"
                    );
                    if bits % 8 != 0 {
                        let mask = 0xFFu8 << (8 - bits % 8);
                        assert_eq!(got[full] & mask, want[full] & mask, "padding row {y}");
                    }
                }
            }
        }
    }
}

#[test]
fn split_idat_chunks_decode_identically() {
    let mut rng = Rng::new(7);
    let samples = rng.bytes(16 * 16 * 3);
    let builder = PngBuilder::new(16, 16, ColorType::Rgb, BitDepth::Eight);
    let whole = oxiarc_png::decode(&builder.build_from_samples(&samples)).expect("whole");
    for split in [1usize, 2, 3, 17, 64] {
        let png = PngBuilder::new(16, 16, ColorType::Rgb, BitDepth::Eight)
            .idat_split(split)
            .build_from_samples(&samples);
        let image = oxiarc_png::decode(&png).unwrap_or_else(|e| panic!("split {split}: {e}"));
        assert_eq!(image.data, whole.data, "split {split}");
    }
}

#[test]
fn zero_length_idat_chunks_are_legal() {
    let samples = vec![9u8; 4 * 4];
    let builder = PngBuilder::new(4, 4, ColorType::Grayscale, BitDepth::Eight);
    let raw = builder.filter_samples(&samples);
    let compressed = oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib");
    let mut png = oxiarc_png::chunk::SIGNATURE.to_vec();
    let mut ihdr = [0u8; 13];
    ihdr[3] = 4;
    ihdr[7] = 4;
    ihdr[8] = 8;
    oxiarc_png::chunk::write_chunk(&mut png, oxiarc_png::chunk::IHDR, &ihdr).expect("ihdr");
    oxiarc_png::chunk::write_chunk(&mut png, oxiarc_png::chunk::IDAT, &[]).expect("empty");
    oxiarc_png::chunk::write_chunk(&mut png, oxiarc_png::chunk::IDAT, &compressed).expect("idat");
    oxiarc_png::chunk::write_chunk(&mut png, oxiarc_png::chunk::IDAT, &[]).expect("empty");
    oxiarc_png::chunk::write_chunk(&mut png, oxiarc_png::chunk::IEND, &[]).expect("iend");
    let image = oxiarc_png::decode(&png).expect("decode");
    assert_eq!(image.data, samples);
}

#[test]
fn transformations_expand_palette_and_sub_byte_gray() {
    // 4-bit palette, EXPAND -> RGB8.
    let png = PngBuilder::new(2, 1, ColorType::Indexed, BitDepth::Four)
        .chunk(oxiarc_png::chunk::PLTE, &[1, 2, 3, 4, 5, 6])
        .build_from_samples(&[0x01]);
    let image =
        oxiarc_png::decode_with(&png, Default::default(), Transformations::EXPAND).expect("decode");
    assert_eq!(image.color_type, ColorType::Rgb);
    assert_eq!(image.bit_depth, BitDepth::Eight);
    assert_eq!(image.data, vec![1, 2, 3, 4, 5, 6]);

    // 2-bit grayscale, EXPAND -> Gray8 scaled by replication.
    let png = PngBuilder::new(4, 1, ColorType::Grayscale, BitDepth::Two)
        .build_from_samples(&[0b00_01_10_11]);
    let image =
        oxiarc_png::decode_with(&png, Default::default(), Transformations::EXPAND).expect("decode");
    assert_eq!(image.color_type, ColorType::Grayscale);
    assert_eq!(image.data, vec![0, 85, 170, 255]);
}

#[test]
fn strip16_and_alpha_transformations() {
    let png = PngBuilder::new(1, 1, ColorType::Rgb, BitDepth::Sixteen)
        .build_from_samples(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC]);
    let image = oxiarc_png::decode_with(
        &png,
        Default::default(),
        Transformations::EXPAND | Transformations::ALPHA | Transformations::STRIP_16,
    )
    .expect("decode");
    assert_eq!(image.color_type, ColorType::Rgba);
    assert_eq!(image.bit_depth, BitDepth::Eight);
    assert_eq!(image.data, vec![0x12, 0x56, 0x9A, 0xFF]);
}

#[test]
fn peek_info_reads_metadata_without_image_data() {
    let png = PngBuilder::new(5, 3, ColorType::Rgb, BitDepth::Eight)
        .chunk(oxiarc_png::chunk::gAMA, &45455u32.to_be_bytes())
        .build_from_samples(&[0u8; 5 * 3 * 3]);
    let info = oxiarc_png::peek_info(&png).expect("peek");
    assert_eq!(info.size(), (5, 3));
    assert_eq!(info.color_type, ColorType::Rgb);
    assert!(oxiarc_png::is_png(&png));
}

#[test]
fn row_by_row_matches_whole_frame() {
    let mut rng = Rng::new(99);
    let samples = rng.bytes(11 * 7 * 4);
    for interlace in [false, true] {
        let png = PngBuilder::new(11, 7, ColorType::Rgba, BitDepth::Eight)
            .interlaced(interlace)
            .build_from_samples(&samples);
        let whole = oxiarc_png::decode(&png).expect("whole");
        let mut reader = oxiarc_png::Decoder::new(&png[..])
            .read_info()
            .expect("read_info");
        let mut rows = Vec::new();
        while let Some(row) = reader.next_interlaced_row().expect("row") {
            rows.push((*row.interlace(), row.data().to_vec()));
        }
        if interlace {
            let mut canvas = vec![0u8; whole.data.len()];
            for (info, data) in &rows {
                if let oxiarc_png::InterlaceInfo::Adam7(a) = info {
                    oxiarc_png::expand_interlaced_row(&mut canvas, 11 * 4, data, a, 32);
                }
            }
            assert_eq!(canvas, whole.data);
        } else {
            let flat: Vec<u8> = rows.iter().flat_map(|(_, d)| d.clone()).collect();
            assert_eq!(flat, whole.data);
        }
    }
}

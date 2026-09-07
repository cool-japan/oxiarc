//! Hostile and malformed input: no panics, no unbounded work, no unbounded
//! allocation, and limits that fire before anything is allocated.

mod common;

use common::{PngBuilder, Rng};
use oxiarc_png::chunk::{self, SIGNATURE, write_chunk};
use oxiarc_png::{
    BitDepth, ColorType, DecodeLimits, DecodeOptions, DecodingError, Transformations,
};

/// A representative set of files to mutate.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = Rng::new(0xC0FFEE);
    let palette: Vec<u8> = (0..256u16)
        .flat_map(|i| [i as u8, (i * 3) as u8, (i * 7) as u8])
        .collect();
    vec![
        (
            "gray8",
            PngBuilder::new(8, 8, ColorType::Grayscale, BitDepth::Eight)
                .build_from_samples(&rng.bytes(64)),
        ),
        (
            "rgba8_interlaced",
            PngBuilder::new(7, 5, ColorType::Rgba, BitDepth::Eight)
                .interlaced(true)
                .build_from_samples(&rng.bytes(7 * 5 * 4)),
        ),
        (
            "palette4",
            PngBuilder::new(9, 3, ColorType::Indexed, BitDepth::Four)
                .chunk(chunk::PLTE, &palette)
                .build_from_samples(&rng.bytes(5 * 3)),
        ),
        (
            "rgb16",
            PngBuilder::new(4, 4, ColorType::Rgb, BitDepth::Sixteen)
                .build_from_samples(&rng.bytes(4 * 4 * 6)),
        ),
        (
            "split_idat",
            PngBuilder::new(6, 6, ColorType::Rgb, BitDepth::Eight)
                .idat_split(3)
                .build_from_samples(&rng.bytes(6 * 6 * 3)),
        ),
    ]
}

/// Decoding must terminate with either an image or an error, never a panic.
fn decode_is_total(data: &[u8]) {
    let _ = oxiarc_png::decode(data);
    let _ = oxiarc_png::decode_with(
        data,
        DecodeOptions::default(),
        Transformations::EXPAND | Transformations::ALPHA,
    );
    let mut strict = DecodeOptions::default();
    strict.set_strict(true);
    let _ = oxiarc_png::decode_with(data, strict, Transformations::IDENTITY);
    let _ = oxiarc_png::peek_info(data);
}

#[test]
fn truncation_at_every_offset_never_panics() {
    for (name, png) in corpus() {
        for cut in 0..png.len() {
            decode_is_total(&png[..cut]);
        }
        // And the full file still decodes.
        oxiarc_png::decode(&png).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn single_byte_mutations_never_panic() {
    for (_, png) in corpus() {
        for offset in 0..png.len() {
            for patch in [0x00u8, 0xFF, png[offset] ^ 0x40] {
                let mut mutated = png.clone();
                mutated[offset] = patch;
                decode_is_total(&mutated);
            }
        }
    }
}

#[test]
fn arbitrary_garbage_never_panics() {
    let mut rng = Rng::new(1234);
    for len in [0usize, 1, 8, 9, 33, 200] {
        for _ in 0..40 {
            let mut data = SIGNATURE.to_vec();
            data.extend_from_slice(&rng.bytes(len));
            decode_is_total(&data);
            decode_is_total(&rng.bytes(len));
        }
    }
}

#[test]
fn dimension_limits_fire_before_allocation() {
    let png = PngBuilder::new(64, 64, ColorType::Rgba, BitDepth::Eight)
        .build_from_samples(&vec![0u8; 64 * 64 * 4]);
    let mut options = DecodeOptions::default();
    options.set_limits(DecodeLimits::default().with_max_width(32));
    assert!(matches!(
        oxiarc_png::decode_with(&png, options, Transformations::IDENTITY),
        Err(DecodingError::LimitsExceeded)
    ));

    let mut options = DecodeOptions::default();
    options.set_limits(DecodeLimits::default().with_max_pixels(100));
    assert!(matches!(
        oxiarc_png::decode_with(&png, options, Transformations::IDENTITY),
        Err(DecodingError::LimitsExceeded)
    ));
}

#[test]
fn a_giant_header_cannot_talk_decode_into_a_giant_allocation() {
    // 32768 x 32768 RGBA8 is exactly the default `max_pixels` of 2^30, so the
    // dimension check passes and only the allocation check stands between a
    // sixty-byte file and a four-gigabyte `Vec`.
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[0..4].copy_from_slice(&32768u32.to_be_bytes());
    ihdr[4..8].copy_from_slice(&32768u32.to_be_bytes());
    ihdr[8] = 8;
    ihdr[9] = ColorType::Rgba as u8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    // A short but structurally valid IDAT, so `read_info` reaches image data.
    let idat = oxiarc_deflate::zlib_compress(&[0u8; 4], 6).expect("zlib");
    write_chunk(&mut png, chunk::IDAT, &idat).expect("idat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");
    assert!(png.len() < 128, "the fixture must stay tiny");

    // The default limits reject it outright.
    assert!(
        matches!(oxiarc_png::decode(&png), Err(DecodingError::LimitsExceeded)),
        "decode must refuse to allocate 4 GiB for a 60-byte file"
    );

    // The row-by-row path is deliberately exempt: it allocates two scanlines,
    // so `read_info` succeeds and only the frame-sized accessor complains.
    let reader = oxiarc_png::Decoder::new(&png[..])
        .read_info()
        .expect("streaming a huge image is allowed");
    assert_eq!(reader.output_buffer_size(), Some(32768 * 32768 * 4));
    assert!(matches!(
        reader.checked_output_buffer_size(),
        Err(DecodingError::LimitsExceeded)
    ));

    // Raising the limit past the frame size lets the same call through.
    let mut options = DecodeOptions::default();
    options.set_limits(DecodeLimits::default().with_max_alloc_bytes(1 << 33));
    let mut decoder = oxiarc_png::Decoder::new_with_options(&png[..], options);
    decoder.set_transformations(Transformations::IDENTITY);
    let reader = decoder.read_info().expect("read_info");
    assert_eq!(
        reader.checked_output_buffer_size().expect("allowed"),
        32768 * 32768 * 4
    );
}

#[test]
fn the_memory_budget_is_honoured() {
    let png = PngBuilder::new(64, 64, ColorType::Rgba, BitDepth::Eight)
        .build_from_samples(&vec![0u8; 64 * 64 * 4]);
    let decoder = oxiarc_png::Decoder::new_with_limits(&png[..], oxiarc_png::Limits { bytes: 8 });
    assert!(matches!(
        decoder.read_info().map(|_| ()),
        Err(DecodingError::LimitsExceeded)
    ));
}

#[test]
fn a_compression_bomb_cannot_expand_past_the_header() {
    // A 4x4 grayscale image whose IDAT decompresses to 8 MiB.
    let mut raw = Vec::new();
    for _ in 0..4 {
        raw.push(0u8);
        raw.extend_from_slice(&[0u8; 4]);
    }
    raw.extend_from_slice(&vec![0u8; 8 * 1024 * 1024]);
    let compressed = oxiarc_deflate::zlib_compress(&raw, 9).expect("zlib");
    assert!(compressed.len() < 20_000, "the fixture must be a real bomb");
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 4;
    ihdr[7] = 4;
    ihdr[8] = 8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    write_chunk(&mut png, chunk::IDAT, &compressed).expect("idat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");

    let image = oxiarc_png::decode(&png).expect("lenient decode");
    assert_eq!(image.data.len(), 16, "only the declared image is produced");

    let mut strict = DecodeOptions::default();
    strict.set_strict(true);
    assert!(
        oxiarc_png::decode_with(&png, strict, Transformations::IDENTITY).is_err(),
        "strict mode rejects extra image data"
    );
}

#[test]
fn a_truncated_image_data_stream_is_an_error_not_a_short_image() {
    let samples = vec![7u8; 32 * 32];
    let png =
        PngBuilder::new(32, 32, ColorType::Grayscale, BitDepth::Eight).build_from_samples(&samples);
    // Drop the second half of the IDAT payload but keep a well-formed file.
    let idat_at = png
        .windows(4)
        .position(|w| w == b"IDAT")
        .expect("IDAT present");
    let len = u32::from_be_bytes([
        png[idat_at - 4],
        png[idat_at - 3],
        png[idat_at - 2],
        png[idat_at - 1],
    ]) as usize;
    let keep = len / 2;
    let mut short = png[..idat_at - 4].to_vec();
    let payload = png[idat_at + 4..idat_at + 4 + keep].to_vec();
    write_chunk(&mut short, chunk::IDAT, &payload).expect("idat");
    write_chunk(&mut short, chunk::IEND, &[]).expect("iend");
    assert!(oxiarc_png::decode(&short).is_err());
}

#[test]
fn text_and_iccp_bombs_are_capped() {
    let bomb = oxiarc_deflate::zlib_compress(&vec![b'a'; 4 * 1024 * 1024], 9).expect("zlib");
    let mut payload = b"Comment\0".to_vec();
    payload.push(0);
    payload.extend_from_slice(&bomb);
    let png = PngBuilder::new(2, 2, ColorType::Grayscale, BitDepth::Eight)
        .chunk(chunk::zTXt, &payload)
        .build_from_samples(&[1, 2, 3, 4]);
    let mut options = DecodeOptions::default();
    options.set_limits(DecodeLimits::default().with_max_text_bytes(1024));
    assert!(oxiarc_png::decode_with(&png, options, Transformations::IDENTITY).is_err());
    // The default 2 MiB limit also rejects a 4 MiB expansion.
    assert!(oxiarc_png::decode(&png).is_err());

    let mut profile = b"ICC\0".to_vec();
    profile.push(0);
    profile.extend_from_slice(&bomb);
    let png = PngBuilder::new(2, 2, ColorType::Grayscale, BitDepth::Eight)
        .chunk(chunk::iCCP, &profile)
        .build_from_samples(&[1, 2, 3, 4]);
    let mut options = DecodeOptions::default();
    options.set_limits(DecodeLimits::default().with_max_iccp_bytes(4096));
    assert!(oxiarc_png::decode_with(&png, options, Transformations::IDENTITY).is_err());
    // The default 16 MiB limit accepts it.
    let image = oxiarc_png::decode(&png).expect("default limits");
    assert_eq!(
        image.info.icc_profile.as_deref().map(<[u8]>::len),
        Some(4 * 1024 * 1024)
    );
}

#[test]
fn unknown_chunk_retention_is_capped() {
    let big = vec![0u8; 64 * 1024];
    let png = PngBuilder::new(2, 2, ColorType::Grayscale, BitDepth::Eight)
        .chunk(chunk::ChunkType(*b"prVt"), &big)
        .build_from_samples(&[1, 2, 3, 4]);
    let image = oxiarc_png::decode(&png).expect("default");
    assert_eq!(image.info.unknown_chunks.len(), 1);

    let mut options = DecodeOptions::default();
    options.set_limits(DecodeLimits::default().with_max_unknown_chunk_bytes(1024));
    let image = oxiarc_png::decode_with(&png, options, Transformations::IDENTITY)
        .expect("skipped, not fatal");
    assert!(image.info.unknown_chunks.is_empty());
}

#[test]
fn the_idat_zlib_header_must_satisfy_the_png_rules() {
    let raw = [0u8, 1, 2];
    for (label, header) in [
        ("CM != 8", [0x79u8, 0x9b]),
        ("CINFO > 7", [0x88, 0x1d]),
        ("FDICT set", [0x78, 0xbb]),
        ("bad check bits", [0x78, 0x9d]),
    ] {
        let mut idat = header.to_vec();
        idat.extend_from_slice(&oxiarc_deflate::deflate(&raw, 6).expect("deflate"));
        idat.extend_from_slice(&[0, 0, 0, 0]);
        let mut png = SIGNATURE.to_vec();
        let mut ihdr = vec![0u8; 13];
        ihdr[3] = 2;
        ihdr[7] = 1;
        ihdr[8] = 8;
        write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
        write_chunk(&mut png, chunk::IDAT, &idat).expect("idat");
        write_chunk(&mut png, chunk::IEND, &[]).expect("iend");
        assert!(
            oxiarc_png::decode(&png).is_err(),
            "{label} must be rejected"
        );
    }
}

#[test]
fn a_palette_index_without_an_entry_renders_as_opaque_black() {
    let png = PngBuilder::new(3, 1, ColorType::Indexed, BitDepth::Eight)
        .chunk(chunk::PLTE, &[10, 20, 30, 40, 50, 60])
        .build_from_samples(&[0, 1, 9]);
    let image = oxiarc_png::decode_with(&png, DecodeOptions::default(), Transformations::EXPAND)
        .expect("lenient");
    assert_eq!(image.data, vec![10, 20, 30, 40, 50, 60, 0, 0, 0]);

    let mut strict = DecodeOptions::default();
    strict.set_strict_palette_indices(true);
    assert!(oxiarc_png::decode_with(&png, strict, Transformations::EXPAND).is_err());
}

#[test]
fn an_oversized_indexed_trns_is_ignored() {
    let png = PngBuilder::new(2, 1, ColorType::Indexed, BitDepth::Eight)
        .chunk(chunk::PLTE, &[10, 20, 30, 40, 50, 60])
        .chunk(chunk::tRNS, &[1, 2, 3, 4, 5])
        .build_from_samples(&[0, 1]);
    let image = oxiarc_png::decode_with(&png, DecodeOptions::default(), Transformations::EXPAND)
        .expect("decode");
    // The tRNS chunk was discarded, so the output stays RGB.
    assert_eq!(image.color_type, ColorType::Rgb);
    assert!(image.info.trns.is_none());
}

#[test]
fn an_apple_cgbi_file_decodes_with_its_channels_swapped_back() {
    // BGRA source bytes; the decoder must hand back RGBA.
    let bgra = [40u8, 30, 20, 10, 80, 70, 60, 50];
    let mut raw = vec![0u8];
    raw.extend_from_slice(&bgra);
    let mut png = SIGNATURE.to_vec();
    write_chunk(&mut png, chunk::CgBI, &[0x50, 0x00, 0x20, 0x06]).expect("cgbi");
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 2;
    ihdr[7] = 1;
    ihdr[8] = 8;
    ihdr[9] = ColorType::Rgba as u8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    write_chunk(
        &mut png,
        chunk::IDAT,
        &oxiarc_deflate::deflate(&raw, 6).expect("deflate"),
    )
    .expect("idat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");

    let image = oxiarc_png::decode(&png).expect("cgbi decode");
    assert_eq!(image.data, vec![20, 30, 40, 10, 60, 70, 80, 50]);
    let cgbi = image.info.cgbi.expect("flag");
    assert!(cgbi.premultiplied_alpha);

    let mut strict = DecodeOptions::default();
    strict.set_strict(true);
    assert!(oxiarc_png::decode_with(&png, strict, Transformations::IDENTITY).is_err());
}

#[test]
fn an_invalid_row_filter_byte_is_rejected() {
    let mut raw = vec![9u8]; // filter type 9 does not exist
    raw.extend_from_slice(&[1, 2]);
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = vec![0u8; 13];
    ihdr[3] = 2;
    ihdr[7] = 1;
    ihdr[8] = 8;
    write_chunk(&mut png, chunk::IHDR, &ihdr).expect("ihdr");
    write_chunk(
        &mut png,
        chunk::IDAT,
        &oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib"),
    )
    .expect("idat");
    write_chunk(&mut png, chunk::IEND, &[]).expect("iend");
    assert!(oxiarc_png::decode(&png).is_err());
}

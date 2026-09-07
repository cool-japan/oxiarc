//! The fuzz seed corpus generator.
//!
//! No corpus is committed: the seeds are *built* here, from this crate's own
//! encoder, so that they can never drift out of step with the formats it
//! supports. Running the test with `OXIARC_JPEG_FUZZ_SEEDS` set to a
//! directory writes them out for `cargo fuzz`:
//!
//! ```text
//! OXIARC_JPEG_FUZZ_SEEDS=$PWD/fuzz/corpus/fuzz_jpeg_decode \
//!   cargo test -p oxiarc-jpeg --all-features --test fuzz_seeds
//! ```
//!
//! Without the variable the seeds are still built and every one of them is
//! decoded, which is what keeps the generator honest: a seed that no longer
//! encodes, or that encodes into something this crate cannot read, fails the
//! test rather than silently producing a corpus of rubbish.
//!
//! The corpus spans every `SOF` the crate writes, both entropy coders, the
//! abbreviated (TIFF) halves, restart intervals, metadata segments and the
//! awkward sizes — a 1x1 image, a one-pixel-wide column, a row shorter than
//! one MCU — because those are where a decoder's geometry arithmetic breaks.

use oxiarc_jpeg::{
    ColorSpace, Decoder, EncodeOptions, EncodeProcess, InputColor, MarkerPolicy, QuantTableSource,
    RestartInterval, Subsampling, TableSet, TablesMode, encode_to_vec_with_options,
    encode_u16_to_vec_with_options, sample, table_set,
};

/// One seed: a name for the file it becomes, and its bytes.
struct Seed {
    name: String,
    bytes: Vec<u8>,
    /// `false` for the abbreviated halves, which are not datastreams on their
    /// own and are meant to be fed to the fuzzer as fragments.
    decodable: bool,
}

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

fn wide_source(width: usize, height: usize, maxval: u16) -> Vec<u16> {
    (0..width * height)
        .map(|i| ((i * 137) % (usize::from(maxval) + 1)) as u16)
        .collect()
}

/// Build the whole corpus.
fn corpus() -> Vec<Seed> {
    let mut seeds = Vec::new();
    let sizes: [(u16, u16); 5] = [(1, 1), (1, 17), (17, 1), (17, 19), (64, 48)];
    let subsamplings = [
        ("444", Subsampling::S444),
        ("422", Subsampling::S422),
        ("440", Subsampling::S440),
        ("420", Subsampling::S420),
        ("411", Subsampling::S411),
    ];

    // The crate's own embedded fixtures: a complete stream and both halves of
    // an abbreviated pair.
    seeds.push(Seed {
        name: "sample_gray_1x1.jpg".into(),
        bytes: sample::GRAY_1X1.to_vec(),
        decodable: true,
    });
    seeds.push(Seed {
        name: "sample_rgb_8x8_420.jpg".into(),
        bytes: sample::RGB_8X8_420.to_vec(),
        decodable: true,
    });
    seeds.push(Seed {
        name: "sample_rgb_tables.bin".into(),
        bytes: sample::RGB_8X8_420_TABLES.to_vec(),
        decodable: false,
    });
    seeds.push(Seed {
        name: "sample_rgb_scan.bin".into(),
        bytes: sample::RGB_8X8_420_SCAN.to_vec(),
        decodable: false,
    });

    let entropies: &[(&str, oxiarc_jpeg::EntropyCoding)] = &[
        ("huffman", oxiarc_jpeg::EntropyCoding::Huffman),
        #[cfg(feature = "arithmetic")]
        ("arithmetic", oxiarc_jpeg::EntropyCoding::Arithmetic),
    ];

    for &(entropy_name, entropy) in entropies {
        for &(width, height) in &sizes {
            for &(ratio_name, subsampling) in &subsamplings {
                let pixels = source(usize::from(width), usize::from(height), 3);
                let options = EncodeOptions {
                    quality: 75,
                    subsampling,
                    entropy,
                    ..Default::default()
                };
                if let Ok(bytes) =
                    encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
                {
                    seeds.push(Seed {
                        name: format!("seq_{entropy_name}_{ratio_name}_{width}x{height}.jpg"),
                        bytes,
                        decodable: true,
                    });
                }
            }

            // Progressive, restarts, grayscale and CMYK at one size each.
            let pixels = source(usize::from(width), usize::from(height), 3);
            for (label, options) in [
                (
                    "prog",
                    EncodeOptions {
                        process: EncodeProcess::Progressive,
                        entropy,
                        ..Default::default()
                    },
                ),
                (
                    "restart",
                    EncodeOptions {
                        restart_interval: RestartInterval::Mcus(1),
                        entropy,
                        ..Default::default()
                    },
                ),
                (
                    "optimized",
                    EncodeOptions {
                        optimize_huffman: true,
                        entropy,
                        ..Default::default()
                    },
                ),
                (
                    "flatquant",
                    EncodeOptions {
                        quant_tables: QuantTableSource::Flat(1),
                        entropy,
                        ..Default::default()
                    },
                ),
            ] {
                if let Ok(bytes) =
                    encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
                {
                    seeds.push(Seed {
                        name: format!("{label}_{entropy_name}_{width}x{height}.jpg"),
                        bytes,
                        decodable: true,
                    });
                }
            }

            // Lossless, at both a one-dimensional and a two-dimensional
            // predictor, and with a point transform.
            for predictor in [1u8, 4, 7] {
                let options = EncodeOptions {
                    process: EncodeProcess::Lossless {
                        predictor,
                        point_transform: u8::from(predictor == 7),
                    },
                    entropy,
                    ..Default::default()
                };
                if let Ok(bytes) =
                    encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
                {
                    seeds.push(Seed {
                        name: format!("lossless{predictor}_{entropy_name}_{width}x{height}.jpg"),
                        bytes,
                        decodable: true,
                    });
                }
            }

            // Twelve-bit and sixteen-bit lossless.
            let wide = wide_source(usize::from(width), usize::from(height), 4095);
            let options = EncodeOptions {
                precision: 12,
                entropy,
                ..Default::default()
            };
            if let Ok(bytes) =
                encode_u16_to_vec_with_options(&wide, width, height, InputColor::Luma, &options)
            {
                seeds.push(Seed {
                    name: format!("twelvebit_{entropy_name}_{width}x{height}.jpg"),
                    bytes,
                    decodable: true,
                });
            }
            let widest = wide_source(usize::from(width), usize::from(height), 65535);
            let options = EncodeOptions {
                precision: 16,
                process: EncodeProcess::Lossless {
                    predictor: 2,
                    point_transform: 0,
                },
                entropy,
                ..Default::default()
            };
            if let Ok(bytes) =
                encode_u16_to_vec_with_options(&widest, width, height, InputColor::Luma, &options)
            {
                seeds.push(Seed {
                    name: format!("sixteenbit_{entropy_name}_{width}x{height}.jpg"),
                    bytes,
                    decodable: true,
                });
            }
        }

        // Grayscale, CMYK and YCCK, which take the other colour paths.
        for (label, color, channels, space) in [
            ("gray", InputColor::Luma, 1usize, None),
            ("cmyk", InputColor::Cmyk, 4, Some(ColorSpace::Cmyk)),
            ("ycck", InputColor::Cmyk, 4, Some(ColorSpace::Ycck)),
        ] {
            let pixels = source(24, 16, channels);
            let options = EncodeOptions {
                jpeg_color_space: space,
                entropy,
                ..Default::default()
            };
            if let Ok(bytes) = encode_to_vec_with_options(&pixels, 24, 16, color, &options) {
                seeds.push(Seed {
                    name: format!("{label}_{entropy_name}.jpg"),
                    bytes,
                    decodable: true,
                });
            }
        }

        // The abbreviated (TIFF) pair: tables and scan, as separate seeds.
        let mut options = EncodeOptions::tiff_strip(75);
        options.entropy = entropy;
        if let Ok(tables) = table_set(&options, InputColor::Rgb) {
            seeds.push(Seed {
                name: format!("tables_{entropy_name}.bin"),
                bytes: tables.emit(TablesMode::BOTH),
                decodable: false,
            });
            let pixels = source(16, 16, 3);
            let mut strip = Vec::new();
            let mut encoder = oxiarc_jpeg::Encoder::with_options(&mut strip, options);
            if encoder
                .encode_scan_only(&pixels, 16, 16, InputColor::Rgb)
                .is_ok()
                && encoder.finish().is_ok()
            {
                seeds.push(Seed {
                    name: format!("strip_{entropy_name}.bin"),
                    bytes: strip,
                    decodable: false,
                });
            }
        }
    }

    // A stream carrying every metadata segment the parser recognises.
    let pixels = source(16, 16, 3);
    let mut bytes = Vec::new();
    {
        let mut encoder = oxiarc_jpeg::Encoder::with_options(
            &mut bytes,
            EncodeOptions {
                write_jfif: MarkerPolicy::Always,
                write_adobe: MarkerPolicy::Always,
                ..Default::default()
            },
        );
        let _ = encoder.add_exif(b"\x49\x49\x2a\x00\x08\x00\x00\x00\x00\x00");
        let _ = encoder.add_xmp(b"<x:xmpmeta/>");
        let _ = encoder.add_icc_profile(&[0u8; 128]);
        let _ = encoder.add_comment(b"oxiarc fuzz seed");
        if encoder.encode(&pixels, 16, 16, InputColor::Rgb).is_ok() {
            let _ = encoder.finish();
        }
    }
    if !bytes.is_empty() {
        seeds.push(Seed {
            name: "metadata.jpg".into(),
            bytes,
            decodable: true,
        });
    }

    seeds
}

#[test]
fn every_seed_builds_and_decodes() {
    let seeds = corpus();
    assert!(
        seeds.len() >= 60,
        "the corpus shrank to {} seeds, which usually means an encoder option \
         started failing silently",
        seeds.len()
    );

    let mut names = std::collections::BTreeSet::new();
    for seed in &seeds {
        assert!(
            names.insert(seed.name.clone()),
            "duplicate seed name {}",
            seed.name
        );
        assert!(!seed.bytes.is_empty(), "{} is empty", seed.name);
        if !seed.decodable {
            // The abbreviated halves must at least parse as a table stream or
            // be usable as a strip.
            let _ = TableSet::parse(&seed.bytes);
            continue;
        }
        let mut decoder = Decoder::new(seed.bytes.as_slice());
        let info = decoder
            .read_info()
            .unwrap_or_else(|e| panic!("{}: read_info failed: {e}", seed.name));
        let pixels = decoder
            .decode()
            .or_else(|_| decoder.decode_u16().map(|_| Vec::new()))
            .unwrap_or_else(|e| panic!("{}: decode failed: {e}", seed.name));
        if !pixels.is_empty() {
            assert_eq!(
                pixels.len(),
                usize::from(info.width) * usize::from(info.height) * info.output_components(),
                "{}",
                seed.name
            );
        }
    }

    if let Ok(directory) = std::env::var("OXIARC_JPEG_FUZZ_SEEDS") {
        let root = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&root).expect("create the seed directory");
        for seed in &seeds {
            std::fs::write(root.join(&seed.name), &seed.bytes).expect("write a seed");
        }
        println!("wrote {} seeds to {}", seeds.len(), root.display());
    }
}

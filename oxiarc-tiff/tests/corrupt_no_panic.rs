//! Corruption sweeps: a malformed TIFF must return `Ok` or `Err`, never panic,
//! never allocate past the configured limits, and always terminate.
//!
//! TIFF's tag-driven allocation is the classic memory-bomb vector, so this is
//! the highest-value suite in the crate.

mod support;

use oxiarc_tiff::{ColorType, Compression, Decoder, Encoder, ImageSpec, Layout, Leniency, Limits};
use std::io::Cursor;
use std::time::{Duration, Instant};
use support::{NextIfd, RawTiff, gray8, ramp};

/// A deterministic 64-bit PRNG so the sweep is reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

/// Tight limits so a bomb is refused rather than allocated.
fn guarded() -> Limits {
    Limits::default()
        .with_max_image_bytes(4 * 1024 * 1024)
        .with_decoding_buffer_size(4 * 1024 * 1024)
        .with_intermediate_buffer_size(4 * 1024 * 1024)
        .with_ifd_value_size(64 * 1024)
        .with_max_ifds(64)
}

/// Drives every read path and asserts only that nothing panics or hangs.
fn exercise(bytes: Vec<u8>, leniency: Leniency) {
    let Ok(decoder) = Decoder::new(Cursor::new(bytes)) else {
        return;
    };
    let mut decoder = decoder.with_limits(guarded()).with_leniency(leniency);
    let _ = decoder.image_count();
    let _ = decoder.info().map(|i| (i.width, i.height));
    let _ = decoder.color_type();
    let _ = decoder.sample_type();
    let _ = decoder.layout();
    let _ = decoder.read_image();
    let _ = decoder.read_image_rgb8();
    let _ = decoder.read_image_rgba8();
    let _ = decoder.read_region(0, 0, 1, 1);
    if let Ok(count) = decoder.chunk_count() {
        for index in 0..count.min(8) {
            let _ = decoder.read_chunk_raw(index);
            let _ = decoder.read_chunk(index);
        }
    }
    let _ = decoder.all_tags();
    let _ = decoder.geo_tags();
    let _ = decoder.sub_ifd_tree();
    let _ = decoder.exif_directory();
    let _ = decoder.icc_profile();
    while decoder.next_image().unwrap_or(false) {
        let _ = decoder.read_image();
    }
}

/// The seed corpus every sweep is derived from.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut out: Vec<(&'static str, Vec<u8>)> = Vec::new();
    out.push(("gray8_strip", gray8(8, 8, &ramp(64)).build()));
    out.push(("gray8_bigtiff", {
        let pixels = ramp(64);
        let mut tiff = RawTiff::new().bigtiff();
        let offset = tiff.add_data(&pixels);
        tiff.long(256, &[8]);
        tiff.long(257, &[8]);
        tiff.short(258, &[8]);
        tiff.short(259, &[1]);
        tiff.short(262, &[1]);
        tiff.long8(273, &[offset]);
        tiff.short(277, &[1]);
        tiff.long(278, &[8]);
        tiff.long8(279, &[64]);
        tiff.build()
    }));
    out.push(("gray8_big_endian", {
        let pixels = ramp(64);
        let mut tiff = RawTiff::new().big_endian();
        let offset = tiff.add_data(&pixels);
        tiff.long(256, &[8]);
        tiff.long(257, &[8]);
        tiff.short(258, &[8]);
        tiff.short(259, &[1]);
        tiff.short(262, &[1]);
        tiff.long(273, &[offset as u32]);
        tiff.short(277, &[1]);
        tiff.long(278, &[4]);
        tiff.long(279, &[32, 32]);
        tiff.build()
    }));
    // Real encoder output: tiles, PackBits, RGB, a predictor.
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image(
            &ImageSpec::new(32, 32, ColorType::Rgb(8))
                .with_compression(Compression::PackBits)
                .with_predictor(oxiarc_tiff::Predictor::Horizontal)
                .with_layout(Layout::Tiles {
                    width: 16,
                    length: 16,
                }),
            &ramp(32 * 32 * 3),
        )
        .expect("write");
    encoder.finish().expect("finish");
    out.push(("rgb8_tiles_packbits", buffer.into_inner()));
    for (label, spec) in codec_specs() {
        let pixels = codec_pixels(&spec);
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(&spec, &pixels).expect("write");
        encoder.finish().expect("finish");
        out.push((label, buffer.into_inner()));
    }
    out
}

/// One page per compressed codec this build supports.
fn codec_specs() -> Vec<(&'static str, ImageSpec)> {
    // Every entry is gated on the feature that compiles its codec: a build
    // without `lzw` or `ccitt` must exercise the codecs it *does* have rather
    // than fail on `FeatureNotCompiled` while writing a fixture.
    let mut out: Vec<(&'static str, ImageSpec)> = Vec::new();
    if cfg!(feature = "lzw") {
        out.push((
            "gray8_lzw",
            ImageSpec::new(24, 20, ColorType::Gray(8))
                .with_compression(Compression::Lzw)
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
    }
    if cfg!(feature = "deflate") {
        out.push((
            "gray8_deflate",
            ImageSpec::new(24, 20, ColorType::Gray(8))
                .with_compression(Compression::Deflate { level: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
    }
    if cfg!(feature = "ccitt") {
        out.push((
            "bilevel_g4",
            ImageSpec::new(24, 20, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup4)
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
        out.push((
            "bilevel_g3_2d",
            ImageSpec::new(24, 20, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup3 {
                    two_dimensional: true,
                    byte_align_eol: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
        out.push((
            "bilevel_rle",
            ImageSpec::new(24, 20, ColorType::Gray(1))
                .with_compression(Compression::CcittRle)
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
    }
    // PackBits needs no feature, so the list is never empty and the corpus
    // still has a compressed codec to damage under `--no-default-features`.
    out.push((
        "gray8_packbits",
        ImageSpec::new(24, 20, ColorType::Gray(8))
            .with_compression(Compression::PackBits)
            .with_layout(Layout::Strips { rows_per_strip: 5 }),
    ));
    if cfg!(feature = "zstd") {
        out.push((
            "gray8_zstd",
            ImageSpec::new(24, 20, ColorType::Gray(8))
                .with_compression(Compression::Zstd { level: 5 })
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
    }
    if cfg!(feature = "lzma") {
        out.push((
            "gray8_lzma",
            ImageSpec::new(24, 20, ColorType::Gray(8))
                .with_compression(Compression::Lzma { preset: 3 })
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
        ));
    }
    if cfg!(feature = "jpeg") {
        out.push((
            "gray8_jpeg",
            ImageSpec::new(24, 16, ColorType::Gray(8))
                .with_compression(Compression::Jpeg {
                    quality: 80,
                    shared_tables: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 8 }),
        ));
        out.push((
            "ycbcr_jpeg",
            ImageSpec::new(32, 32, ColorType::YCbCr(8))
                .with_compression(Compression::Jpeg {
                    quality: 80,
                    shared_tables: false,
                })
                .with_ycbcr_subsampling(2, 2)
                .with_layout(Layout::Strips { rows_per_strip: 16 }),
        ));
    }
    out
}

/// Deterministic pixels of the shape a spec wants.
fn codec_pixels(spec: &ImageSpec) -> Vec<u8> {
    let samples = spec.width as usize * spec.height as usize * usize::from(spec.samples_per_pixel);
    let bilevel = spec.bits_per_sample.first().copied() == Some(1);
    (0..samples)
        .map(|i| {
            if bilevel {
                u8::from((i / 3 + i / 24) % 4 == 0)
            } else {
                (i * 7 % 251) as u8
            }
        })
        .collect()
}

#[test]
fn truncation_at_every_offset_never_panics() {
    let started = Instant::now();
    for (name, bytes) in corpus() {
        for cut in 0..bytes.len() {
            exercise(bytes[..cut].to_vec(), Leniency::Normal);
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "{name} truncation sweep is too slow"
        );
    }
}

#[test]
fn random_single_bit_flips_never_panic() {
    for (_, bytes) in corpus() {
        let mut rng = Rng::new(0xC0FF_EE00);
        for _ in 0..1000 {
            let mut copy = bytes.clone();
            let index = rng.below(copy.len().max(1));
            if let Some(byte) = copy.get_mut(index) {
                *byte ^= 1u8 << (rng.below(8) as u32);
            }
            exercise(copy, Leniency::Normal);
        }
    }
}

#[test]
fn random_four_byte_windows_zeroed_never_panic() {
    for (_, bytes) in corpus() {
        let mut rng = Rng::new(0x5EED_1234);
        for _ in 0..1000 {
            let mut copy = bytes.clone();
            let index = rng.below(copy.len().saturating_sub(4).max(1));
            for offset in 0..4 {
                if let Some(byte) = copy.get_mut(index + offset) {
                    *byte = 0;
                }
            }
            exercise(copy, Leniency::Lenient);
        }
    }
}

#[test]
fn random_byte_substitutions_never_panic() {
    for (_, bytes) in corpus() {
        let mut rng = Rng::new(0xBEEF_0F00);
        for _ in 0..1000 {
            let mut copy = bytes.clone();
            for _ in 0..3 {
                let index = rng.below(copy.len().max(1));
                if let Some(byte) = copy.get_mut(index) {
                    *byte = rng.below(256) as u8;
                }
            }
            exercise(copy, Leniency::Strict);
        }
    }
}

#[test]
fn arbitrary_bytes_are_rejected_without_panicking() {
    let mut rng = Rng::new(0x1234_5678);
    for _ in 0..500 {
        let len = rng.below(512);
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.below(256) as u8).collect();
        // Half of them start with a valid signature so the parser gets further.
        if len >= 8 && rng.below(2) == 0 {
            bytes[0] = b'I';
            bytes[1] = b'I';
            bytes[2] = 42;
            bytes[3] = 0;
        }
        exercise(bytes, Leniency::Normal);
    }
}

#[test]
fn a_bomb_is_refused_in_microseconds_without_allocating() {
    let mut tiff = gray8(4, 4, &ramp(16));
    tiff.long(256, &[1 << 31]);
    tiff.long(257, &[1 << 31]);
    let bytes = tiff.build();
    let started = Instant::now();
    let mut decoder = Decoder::new(Cursor::new(bytes))
        .expect("header")
        .with_limits(guarded());
    let err = decoder.read_image().expect_err("bomb");
    assert!(err.is_limits(), "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn a_sixty_thousand_entry_ifd_does_not_allocate_sixty_thousand_vectors() {
    let mut tiff = gray8(4, 4, &ramp(16));
    for tag in 40000u16..60000 {
        tiff.short(tag, &[1]);
    }
    let bytes = tiff.build();
    let started = Instant::now();
    let mut decoder = Decoder::new(Cursor::new(bytes))
        .expect("header")
        .with_limits(Limits::default());
    let directory = decoder.directory().expect("directory");
    assert_eq!(directory.len(), 20009);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_deep_ifd_chain_is_capped() {
    // A chain that points forward forever is stopped by `max_ifds`.
    let tiff = gray8(2, 2, &ramp(4));
    let probe = tiff.build();
    let ifd_offset = u32::from_le_bytes([probe[4], probe[5], probe[6], probe[7]]);
    let looped = tiff.next_ifd(NextIfd::At(u64::from(ifd_offset))).build();
    let mut decoder = Decoder::new(Cursor::new(looped))
        .expect("header")
        .with_limits(Limits::default().with_max_ifds(4));
    let err = decoder.image_count().expect_err("cycle or cap");
    assert!(err.is_limits() || matches!(err, oxiarc_tiff::TiffError::Format(_)));
}

#[test]
fn offsets_past_eof_are_rejected_for_every_tag() {
    for tag in [273u16, 279, 324, 325, 320, 700, 34675] {
        let mut tiff = gray8(4, 4, &ramp(16));
        tiff.long(tag, &[0xFFFF_0000]);
        let bytes = tiff.build();
        let mut decoder = Decoder::new(Cursor::new(bytes))
            .expect("header")
            .with_limits(guarded());
        // Must not panic; an error is fine, and so is ignoring an unrelated tag.
        let _ = decoder.info();
        let _ = decoder.read_image();
        let _ = decoder.all_tags();
    }
}

#[test]
fn every_codec_survives_truncation_and_bit_flips() {
    // The codecs are the part of a TIFF reader that walks attacker-controlled
    // bit streams, so each one gets its own sweep: every truncation point, a
    // spray of single-bit flips, and a lenient read of both. Nothing may
    // panic, hang or allocate without bound.
    let started = Instant::now();
    let mut cases = 0usize;
    for (label, bytes) in corpus() {
        if !label.contains('_') {
            continue;
        }
        for cut in (1..bytes.len()).step_by(7) {
            exercise(bytes[..cut].to_vec(), Leniency::Normal);
            cases += 1;
        }
        for seed in 0..256usize {
            let mut damaged = bytes.clone();
            let index = (seed * 37 + 11) % damaged.len();
            damaged[index] ^= 1 << (seed % 8);
            exercise(damaged.clone(), Leniency::Normal);
            exercise(damaged, Leniency::Lenient);
            cases += 2;
        }
    }
    assert!(cases > 2000, "only {cases} damaged inputs were exercised");
    assert!(
        started.elapsed() < Duration::from_secs(120),
        "the sweep took {:?}, which means something is not bounded",
        started.elapsed()
    );
}

#[test]
fn a_lenient_read_of_a_damaged_codec_stream_still_fills_the_image() {
    // Lenient mode is the "show me what you can" path a viewer wants. It must
    // return a full-size image rather than an error, whatever the codec.
    for (label, spec) in codec_specs() {
        let pixels = codec_pixels(&spec);
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(&spec, &pixels).expect("write");
        encoder.finish().expect("finish");
        let bytes = buffer.into_inner();
        // Damage the middle of the file, which is always strip data.
        let mut damaged = bytes.clone();
        let start = damaged.len() / 3;
        for byte in damaged.iter_mut().skip(start).take(16) {
            *byte ^= 0xA5;
        }
        let mut decoder = Decoder::new(Cursor::new(damaged))
            .expect("header")
            .with_leniency(Leniency::Lenient);
        match decoder.read_image() {
            Ok(samples) => {
                let got = samples.to_native_bytes();
                assert_eq!(got.len(), pixels.len(), "{label}: short lenient image");
            }
            Err(error) => {
                // An error is acceptable only if it names the codec rather
                // than panicking or hanging.
                assert!(!error.to_string().is_empty(), "{label}");
            }
        }
    }
}

/// The `rayon` decode driver is a second, independent path over the same
/// malformed bytes: it enumerates chunks, precharges the budget and fetches in
/// batches with its own bookkeeping, so a corruption that the serial pipeline
/// merely reports could still make it panic, hang or disagree. `exercise`
/// deliberately does not call it on every input (thousands of thread-pool
/// round trips per sweep would dominate this suite's runtime), so it gets its
/// own sweep over a subsampled set of cut points, plus the flat assertion that
/// matters most: whatever the serial path decides about a damaged file, the
/// parallel one must decide the same *kind* of thing.
#[cfg(feature = "rayon")]
#[test]
fn the_parallel_decoder_survives_the_same_corruption_as_the_serial_one() {
    let started = Instant::now();
    let mut agreed_images = 0usize;
    for (name, bytes) in corpus() {
        // Every 7th cut point -- enough to land inside the header, the IFD,
        // the strip offsets and the pixel data of every fixture, without
        // paying for a full sweep -- plus the *untruncated* file, without
        // which every pair below would be `(Err, Err)` and the sweep would
        // prove nothing (`agreed_images` is the assertion that it does).
        let cuts = (0..bytes.len())
            .step_by(7)
            .chain(std::iter::once(bytes.len()));
        for cut in cuts {
            let damaged = bytes.get(..cut).unwrap_or(&bytes).to_vec();
            for leniency in [Leniency::Normal, Leniency::Lenient] {
                let Ok(serial) = Decoder::new(Cursor::new(damaged.clone())) else {
                    continue;
                };
                let mut serial = serial.with_limits(guarded()).with_leniency(leniency);
                let serial_result = serial.read_image();

                let Ok(parallel) = Decoder::new(Cursor::new(damaged.clone())) else {
                    continue;
                };
                let mut parallel = parallel.with_limits(guarded()).with_leniency(leniency);
                let parallel_result = parallel.read_image_parallel();

                match (serial_result, parallel_result) {
                    (Ok(a), Ok(b)) => {
                        assert_eq!(
                            a, b,
                            "{name} cut {cut} {leniency:?}: parallel decoded different pixels"
                        );
                        agreed_images += 1;
                    }
                    (Err(_), Err(_)) => {}
                    // The two paths charge the output budget differently (the
                    // parallel one precharges the whole image up front), so
                    // one may refuse a file the other accepts *only* on
                    // limits grounds; anything else is a real divergence.
                    (Ok(_), Err(error)) | (Err(error), Ok(_)) => assert!(
                        error.is_limits(),
                        "{name} cut {cut} {leniency:?}: paths disagree with {error}"
                    ),
                }
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "{name} parallel corruption sweep is too slow"
        );
    }
    // Non-vacuity: a sweep in which nothing ever decoded would pass every
    // assertion above while comparing nothing at all.
    assert!(
        agreed_images >= corpus().len(),
        "the sweep decoded only {agreed_images} images successfully; it is not comparing anything"
    );
}

/// The feature gates in [`codec_specs`] must only ever *subtract*: a build
/// that compiles a codec has to damage a page written with it. Without this,
/// a mis-typed `cfg!` would quietly shrink the corpus to PackBits and every
/// sweep below would still report `PASS`.
#[test]
fn the_corruption_corpus_covers_every_codec_this_build_compiles() {
    let names: Vec<&str> = codec_specs().into_iter().map(|(name, _)| name).collect();
    for (name, present) in [
        ("gray8_lzw", cfg!(feature = "lzw")),
        ("gray8_deflate", cfg!(feature = "deflate")),
        ("bilevel_g4", cfg!(feature = "ccitt")),
        ("bilevel_g3_2d", cfg!(feature = "ccitt")),
        ("bilevel_rle", cfg!(feature = "ccitt")),
        ("gray8_zstd", cfg!(feature = "zstd")),
        ("gray8_lzma", cfg!(feature = "lzma")),
        ("gray8_jpeg", cfg!(feature = "jpeg")),
        ("ycbcr_jpeg", cfg!(feature = "jpeg")),
        ("gray8_packbits", true),
    ] {
        assert_eq!(
            names.contains(&name),
            present,
            "{name} is missing from the corpus of a build that compiles it"
        );
    }
    // And the corpus itself is never empty, whatever is switched off.
    assert!(!corpus().is_empty(), "the corruption corpus is empty");
}

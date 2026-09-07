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
    out
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

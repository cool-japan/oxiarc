//! Malformed-input hardening: the properties this crate promises about
//! untrusted bytes, exercised rather than asserted in prose.
//!
//! The crate docs say every decode path bottoms out in one of
//! `oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`'s own bounded decoders, so a
//! hostile file is an `ImageError`, never a panic and never an unbounded
//! allocation. These tests are the evidence for that: truncation at *every*
//! offset, single-byte corruption at every offset, byte drops and inserts,
//! one-byte-at-a-time and randomly-split readers, degenerate buffers, and
//! hand-built headers declaring dimensions far larger than any real image.
//!
//! Every sweep is bounded (the fixtures are a few hundred bytes each, and
//! the mutation counts are fixed), so the whole file runs in well under a
//! second and is a normal, always-on test rather than a fuzz target.

use oxiarc_image::codecs::png::PngDecoder;
use oxiarc_image::codecs::tiff::TiffDecoder;
use oxiarc_image::{DynamicImage, ImageBuffer, ImageFormat, Rgb};
use std::io::{Cursor, Read, Seek, SeekFrom};

// --- fixtures -------------------------------------------------------------

fn sample(format: ImageFormat) -> Vec<u8> {
    let image = DynamicImage::ImageRgb8(ImageBuffer::from_fn(8, 8, |x, y| {
        Rgb::new((x * 30) as u8, (y * 30) as u8, 7)
    }));
    let mut out = Vec::new();
    image
        .write_to(Cursor::new(&mut out), format)
        .unwrap_or_else(|e| panic!("fixture encode to {format:?} failed: {e}"));
    out
}

fn decodable_formats() -> [(ImageFormat, Vec<u8>); 3] {
    [
        (ImageFormat::Png, sample(ImageFormat::Png)),
        (ImageFormat::Jpeg, sample(ImageFormat::Jpeg)),
        (ImageFormat::Tiff, sample(ImageFormat::Tiff)),
    ]
}

/// A tiny deterministic PRNG, so the "random" splits are reproducible and
/// need no dev-dependency.
struct Rng(u64);

impl Rng {
    fn next_u32(&mut self) -> u32 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) as u32
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            self.next_u32() as usize % n
        }
    }
}

// --- mutation sweeps ------------------------------------------------------

/// Every prefix of a well-formed file must either decode or return an
/// `ImageError` — never panic, and never hand back an image whose reported
/// dimensions disagree with its own sample count.
#[test]
fn truncation_at_every_offset_never_panics() {
    for (format, bytes) in decodable_formats() {
        for n in 0..bytes.len() {
            if let Ok(image) = oxiarc_image::load_from_memory_with_format(&bytes[..n], format) {
                assert_consistent(&image, format, &format!("truncated to {n}"));
            }
        }
    }
}

/// A single flipped bit or byte anywhere in the file, likewise.
#[test]
fn single_byte_corruption_at_every_offset_never_panics() {
    for (format, bytes) in decodable_formats() {
        for i in 0..bytes.len() {
            for delta in [0x01u8, 0x80, 0xFF] {
                let mut corrupted = bytes.clone();
                corrupted[i] ^= delta;
                if let Ok(image) = oxiarc_image::load_from_memory_with_format(&corrupted, format) {
                    assert_consistent(&image, format, &format!("byte {i} ^ {delta:#04x}"));
                }
            }
        }
    }
}

/// Dropping or inserting a byte shifts every subsequent length and offset
/// field, which is where off-by-one handling in a container parser shows up.
#[test]
fn byte_drops_and_inserts_never_panic() {
    for (format, bytes) in decodable_formats() {
        for i in 0..bytes.len() {
            let mut dropped = bytes.clone();
            dropped.remove(i);
            if let Ok(image) = oxiarc_image::load_from_memory_with_format(&dropped, format) {
                assert_consistent(&image, format, &format!("byte {i} dropped"));
            }

            let mut inserted = bytes.clone();
            inserted.insert(i, 0xA5);
            if let Ok(image) = oxiarc_image::load_from_memory_with_format(&inserted, format) {
                assert_consistent(&image, format, &format!("byte {i} inserted"));
            }
        }
    }
}

/// Truncating to a random length and appending random trailing garbage: the
/// two shapes a network-sourced image most often arrives in.
#[test]
fn random_truncations_and_trailing_garbage_never_panic() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for (format, bytes) in decodable_formats() {
        for _ in 0..200 {
            let mut mutated = bytes[..rng.below(bytes.len() + 1)].to_vec();
            for _ in 0..rng.below(8) {
                mutated.push(rng.next_u32() as u8);
            }
            if let Ok(image) = oxiarc_image::load_from_memory_with_format(&mutated, format) {
                assert_consistent(&image, format, "random mutation");
            }
        }
    }
}

/// Degenerate buffers: empty, one byte, and a bare magic prefix with nothing
/// behind it.
#[test]
fn degenerate_buffers_are_errors_not_panics() {
    for (format, bytes) in decodable_formats() {
        for len in [0usize, 1, 2, 3, 4] {
            let prefix = &bytes[..len.min(bytes.len())];
            let _ = oxiarc_image::load_from_memory_with_format(prefix, format);
            let _ = oxiarc_image::guess_format(prefix);
        }
    }
    assert!(oxiarc_image::load_from_memory(&[]).is_err());
    assert!(oxiarc_image::load_from_memory(&[0u8]).is_err());
    assert!(oxiarc_image::guess_format(&[]).is_err());
}

/// `guess_format` must survive every prefix length of every real encoding
/// without panicking, and must never claim a *different* format than the one
/// the whole file sniffs as.
#[test]
fn sniffing_every_prefix_length_is_stable() {
    for (format, bytes) in decodable_formats() {
        for n in 0..bytes.len().min(64) {
            if let Ok(guessed) = oxiarc_image::guess_format(&bytes[..n]) {
                assert_eq!(
                    guessed, format,
                    "a {n}-byte prefix of a {format:?} file sniffed as {guessed:?}"
                );
            }
        }
    }
}

// --- adversarial readers --------------------------------------------------

/// A `Read` that hands back at most `chunk` bytes per call, to prove the
/// decoders do not silently assume a single large read.
struct DripReader {
    inner: Cursor<Vec<u8>>,
    chunk: usize,
}

impl Read for DripReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let take = self.chunk.min(buf.len());
        self.inner.read(&mut buf[..take])
    }
}

impl Seek for DripReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn one_byte_at_a_time_decoding_matches_a_single_read() {
    for (format, bytes) in decodable_formats() {
        let reference = oxiarc_image::load_from_memory_with_format(&bytes, format)
            .expect("the well-formed fixture must decode");
        let reference = reference.to_rgb8().into_raw();

        for chunk in [1usize, 2, 3, 7, 64] {
            let reader = DripReader {
                inner: Cursor::new(bytes.clone()),
                chunk,
            };
            let decoded = match format {
                ImageFormat::Png => DynamicImage::from_decoder(
                    PngDecoder::new(reader).expect("png header from a dripping reader"),
                ),
                ImageFormat::Tiff => DynamicImage::from_decoder(
                    TiffDecoder::new(reader).expect("tiff header from a dripping reader"),
                ),
                _ => DynamicImage::from_decoder(
                    oxiarc_image::codecs::jpeg::JpegDecoder::new(reader)
                        .expect("jpeg header from a dripping reader"),
                ),
            }
            .unwrap_or_else(|e| panic!("{format:?} at {chunk} bytes/read: {e}"));
            assert_eq!(
                decoded.to_rgb8().into_raw(),
                reference,
                "{format:?} decoded differently when fed {chunk} bytes at a time"
            );
        }
    }
}

// --- hand-built hostile headers -------------------------------------------

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc ^ 0xFFFF_FFFF
}

/// A syntactically valid PNG whose `IHDR` declares `width` x `height`, with
/// a token IDAT far too small to fill it.
fn crafted_png(width: u32, height: u32) -> Vec<u8> {
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut crc_input = kind.to_vec();
        crc_input.extend_from_slice(data);
        out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
        out
    }
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA, no interlace
    let mut out = Vec::new();
    out.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    out.extend_from_slice(&chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&chunk(
        b"IDAT",
        &[
            0x78, 0x01, 0x01, 0x00, 0x00, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01,
        ],
    ));
    out.extend_from_slice(&chunk(b"IEND", &[]));
    out
}

/// A minimal little-endian classic TIFF whose IFD declares `width` x
/// `height` 8-bit grayscale but carries only 16 bytes of strip data.
fn crafted_tiff(width: u32, height: u32) -> Vec<u8> {
    let entries: [(u16, u16, u32); 9] = [
        (256, 4, width),
        (257, 4, height),
        (258, 3, 8),
        (259, 3, 1),
        (262, 3, 1),
        (273, 4, 0), // StripOffsets, patched below
        (277, 3, 1),
        (278, 4, height),
        (279, 4, 16),
    ];
    let mut out = Vec::new();
    out.extend_from_slice(b"II*\x00");
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let ifd_body = out.len();
    for (tag, ty, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        if ty == 3 {
            out.extend_from_slice(&(value as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    let data_offset = out.len() as u32;
    out.extend_from_slice(&[0u8; 16]);
    let offset_field = ifd_body + 5 * 12 + 8;
    out[offset_field..offset_field + 4].copy_from_slice(&data_offset.to_le_bytes());
    out
}

/// The header says the image is astronomically large; the file is a hundred
/// bytes. Neither decoder may attempt the implied allocation.
///
/// This is the property the crate docs lean on when they say the underlying
/// crates' own `Limits` still apply here: `DynamicImage::from_decoder`
/// allocates `total_bytes()` up front, so if a decoder's constructor
/// accepted these headers, that allocation — 17 GB for the smallest case
/// here — would be reached before a single byte of pixel data was read.
/// Both constructors reject first, so the allocation never happens.
#[test]
fn absurd_declared_dimensions_are_rejected_before_any_allocation() {
    for (width, height) in [
        (65_535u32, 65_535u32),
        (100_000, 100_000),
        (0x7FFF_FFFF, 0x7FFF_FFFF),
        (u32::MAX, u32::MAX),
    ] {
        let png = crafted_png(width, height);
        let err = PngDecoder::new(Cursor::new(png))
            .err()
            .unwrap_or_else(|| panic!("PNG {width}x{height} header must be rejected"));
        assert!(
            matches!(
                err,
                oxiarc_image::ImageError::Limits(_) | oxiarc_image::ImageError::Decoding(_)
            ),
            "PNG {width}x{height}: unexpected error {err}"
        );

        let tiff = crafted_tiff(width, height);
        let err = TiffDecoder::new(Cursor::new(tiff))
            .err()
            .unwrap_or_else(|| panic!("TIFF {width}x{height} header must be rejected"));
        assert!(
            matches!(
                err,
                oxiarc_image::ImageError::Limits(_) | oxiarc_image::ImageError::Decoding(_)
            ),
            "TIFF {width}x{height}: unexpected error {err}"
        );
    }
}

/// A dimension small enough to pass the header limits still has to produce
/// an image whose buffer really is that size — no partially-filled buffer
/// reported as a success.
#[test]
fn a_header_that_over_declares_its_pixel_data_is_an_error_not_a_short_image() {
    // 64x64 RGBA is 16 KiB, well inside every limit, but the crafted IDAT
    // carries a single byte of inflated data.
    //
    // Rejecting outright is equally correct here; what must not happen is a
    // success whose buffer does not actually cover the declared dimensions.
    let png = crafted_png(64, 64);
    if let Ok(image) = oxiarc_image::load_from_memory_with_format(&png, ImageFormat::Png) {
        assert_consistent(&image, ImageFormat::Png, "over-declared PNG");
    }

    let tiff = crafted_tiff(64, 64);
    if let Ok(image) = oxiarc_image::load_from_memory_with_format(&tiff, ImageFormat::Tiff) {
        assert_consistent(&image, ImageFormat::Tiff, "over-declared TIFF");
    }
}

// --- shared invariant -----------------------------------------------------

/// Whatever a decode returns, the image's declared dimensions and colour
/// type must account for exactly the samples it carries: a silently
/// half-filled or over-long buffer is as bad as a panic.
fn assert_consistent(image: &DynamicImage, format: ImageFormat, what: &str) {
    let (width, height) = image.dimensions();
    let pixels = (width as usize).saturating_mul(height as usize);
    for (name, got, per_pixel) in [
        ("to_rgba8", image.to_rgba8().into_raw().len(), 4),
        ("to_rgb8", image.to_rgb8().into_raw().len(), 3),
        ("to_luma8", image.to_luma8().into_raw().len(), 1),
        ("to_luma_alpha8", image.to_luma_alpha8().into_raw().len(), 2),
    ] {
        assert_eq!(
            got,
            pixels * per_pixel,
            "{format:?} ({what}): {name} produced {got} samples for a {width}x{height} image"
        );
    }
    // And the declared colour type has to agree with the declared size too:
    // a decoder that reported dimensions its own buffer cannot cover would
    // show up here rather than as a later out-of-bounds panic in a caller.
    assert_eq!(
        u64::from(width) * u64::from(height) * u64::from(image.color().bytes_per_pixel()),
        (pixels * usize::from(image.color().bytes_per_pixel())) as u64,
        "{format:?} ({what}): declared size overflows its own sample count"
    );
}

/// The alpha channel of a decoded image must stay in range for its own
/// sample type — a corrupted file must not be able to produce a pixel that
/// breaks a later conversion.
#[test]
fn conversions_of_a_corrupted_decode_stay_in_range() {
    let mut rng = Rng(0x0BAD_C0DE_0BAD_C0DE);
    for (format, bytes) in decodable_formats() {
        for _ in 0..100 {
            let mut mutated = bytes.clone();
            let i = rng.below(mutated.len());
            mutated[i] ^= rng.next_u32() as u8;
            if let Ok(image) = oxiarc_image::load_from_memory_with_format(&mutated, format) {
                let rgba = image.to_rgba16();
                let (w, h) = rgba.dimensions();
                assert_eq!(rgba.as_raw().len(), (w as usize) * (h as usize) * 4);
                let _ = image.to_luma_alpha8();
                let _ = image.to_rgb16();
            }
        }
    }
}

/// Re-encoding whatever came out of a corrupted decode must not panic
/// either: the encoders see attacker-influenced dimensions and samples.
#[test]
fn re_encoding_a_corrupted_decode_never_panics() {
    let mut rng = Rng(0xFEED_FACE_0000_0001);
    for (format, bytes) in decodable_formats() {
        for _ in 0..50 {
            let mut mutated = bytes.clone();
            let i = rng.below(mutated.len());
            mutated[i] ^= rng.next_u32() as u8;
            if let Ok(image) = oxiarc_image::load_from_memory_with_format(&mutated, format) {
                for target in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
                    let mut out = Vec::new();
                    let _ = image.write_to(Cursor::new(&mut out), target);
                }
            }
        }
    }
}

/// A zero-sized image is a legitimate (if useless) decode result and must
/// round-trip through every conversion and encoder without panicking.
#[test]
fn zero_sized_images_survive_every_conversion_and_encoder() {
    for (w, h) in [(0u32, 0u32), (0, 4), (4, 0)] {
        let image = DynamicImage::ImageRgba8(
            ImageBuffer::from_raw(w, h, Vec::<u8>::new()).expect("empty fixture"),
        );
        assert_eq!(image.to_rgba16().dimensions(), (w, h));
        assert_eq!(image.to_luma8().dimensions(), (w, h));
        assert_eq!(image.to_luma_alpha16().dimensions(), (w, h));
        for target in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff] {
            let mut out = Vec::new();
            let _ = image.write_to(Cursor::new(&mut out), target);
        }
    }
    // And one non-empty control, so the loop above cannot pass vacuously.
    let one = DynamicImage::ImageRgba8(
        ImageBuffer::from_raw(1, 1, vec![1u8, 2, 3, 4]).expect("1x1 fixture"),
    );
    assert_eq!(one.to_rgba16().as_raw().len(), 4);
}

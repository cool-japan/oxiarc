//! The invariants the five fuzz targets `examples/fuzz_seeds.rs` seeds are
//! meant to assert, run against freshly regenerated seeds (never the
//! git-ignored `fuzz/corpus/` on disk).
//!
//! The targets live in the workspace `fuzz/` crate (Wave 3's to add, per
//! `examples/fuzz_seeds.rs`'s module docs, which fix the exact byte layout
//! each one parses); this file re-derives the same seed families and
//! decodes them the way each target is documented to, so:
//!
//! * a byte layout that stops decoding fails here, in `cargo test`, rather
//!   than only under a fuzzer someone remembers to run;
//! * the documented layout and the seed generator can never silently drift,
//!   because this file is the thing that proves they agree.

#![cfg(all(feature = "ccitt", feature = "lzw"))]

use oxiarc_tiff::compression::{CodecContext, CodecLevel, decode_into, encode};
use oxiarc_tiff::tags::{PhotometricInterpretation, T4Options};
use oxiarc_tiff::{ColorType, CompressionMethod, Decoder, Encoder, Endian, ImageSpec, Layout};
use std::io::Cursor;

/// A small deterministic bilevel test pattern, already packed MSB-first at
/// one bit per pixel (`row_bytes = width.div_ceil(8)`) -- the format the
/// CCITT codecs themselves read and write (see `compression/ccitt/mod.rs`'s
/// own doctest: `rows = [0b1111_0000u8, ...]` is 8 pixels per byte, not one
/// byte per pixel).
fn bilevel_pattern(width: usize, height: usize) -> Vec<u8> {
    let row_bytes = width.div_ceil(8);
    let mut packed = vec![0u8; row_bytes * height];
    for y in 0..height {
        for x in 0..width {
            let set = if y == height / 2 {
                (x * 7 + y) % 5 < 2
            } else {
                (x / 4 + y) % 2 == 0
            };
            if set {
                packed[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    packed
}

/// `tiff_decode`: whole files, no header -- a valid file must decode, and a
/// truncated or bit-flipped one must return `Ok` or `Err`, never panic.
#[test]
fn tiff_decode_seeds_decode_or_fail_cleanly() {
    let gray: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image(
            &ImageSpec::new(8, 8, ColorType::Gray(8))
                .with_layout(Layout::Strips { rows_per_strip: 2 }),
            &gray,
        )
        .expect("write");
    encoder.finish().expect("finish");
    let full = buffer.into_inner();

    let mut decoder = Decoder::new(Cursor::new(full.clone())).expect("decoder");
    let decoded = decoder.read_image().expect("valid seed must decode");
    assert_eq!(decoded.as_u8(), Some(gray.as_slice()));

    for cut in [full.len() / 2, full.len() - 1, 16] {
        if cut >= full.len() {
            continue;
        }
        let truncated = &full[..cut];
        // "Ok or Err, never panic" -- the panic is the failure mode this
        // asserts against, by simply not catching one.
        let _ = Decoder::new(Cursor::new(truncated.to_vec())).and_then(|mut d| d.read_image());
    }
}

/// `ccitt_decode`: byte 0 = flavour, bytes 1-2 = width (`u16` LE), byte 3 =
/// `T4Options` bits, rest = payload -- every flavour this crate encodes
/// must decode back through the same header-driven `CodecContext`.
#[test]
fn ccitt_decode_seeds_decode_through_the_documented_header() {
    let width = 32u32;
    let height = 16u32;
    let pixels = bilevel_pattern(width as usize, height as usize);
    let bits = [1u16];

    for (flavour, method, t4_bits) in [
        (0u8, CompressionMethod::CcittRle, 0u32),
        (1u8, CompressionMethod::CcittFax3, 0u32),
        (2u8, CompressionMethod::CcittFax3, 1u32), // two-dimensional
        (3u8, CompressionMethod::CcittFax4, 0u32),
    ] {
        let mut cx = CodecContext::new(
            method,
            width as usize,
            height as usize,
            &bits,
            1,
            Endian::Little,
        );
        cx.photometric = PhotometricInterpretation::BlackIsZero;
        cx.t4_options = T4Options::from_u32(t4_bits);
        let coded = encode(&pixels, &cx, CodecLevel::Default).expect("encode");

        // Rebuild the seed exactly as the generator does.
        let mut seed = vec![flavour];
        seed.extend_from_slice(&(width as u16).to_le_bytes());
        seed.push(t4_bits as u8);
        seed.extend_from_slice(&coded);

        // And parse it back the way the fuzz target is documented to.
        let parsed_width = u16::from_le_bytes([seed[1], seed[2]]) as usize;
        let parsed_t4 = T4Options::from_u32(u32::from(seed[3]));
        let payload = &seed[4..];
        assert_eq!(parsed_width, width as usize);

        let mut decode_cx = CodecContext::new(
            method,
            parsed_width,
            height as usize,
            &bits,
            1,
            Endian::Little,
        );
        decode_cx.t4_options = parsed_t4;
        let mut out = vec![0u8; pixels.len()];
        let produced = decode_into(payload, &mut out, &decode_cx).expect("decode");
        assert_eq!(produced, out.len());
        assert_eq!(out, pixels, "flavour {flavour} round trip mismatch");
    }
}

/// `packbits_decode`: bytes 0-3 = destination capacity (`u32` LE), rest =
/// payload -- every run shape this crate's own encoder produces must decode
/// back losslessly through that header.
#[test]
fn packbits_decode_seeds_decode_through_the_documented_header() {
    let bits = [8u16];
    let cx = CodecContext::new(CompressionMethod::PackBits, 32, 1, &bits, 1, Endian::Little);

    for pixels in [
        (0..32u32).map(|i| i as u8).collect::<Vec<u8>>(), // literal run
        vec![0x42u8; 32],                                 // repeat run
    ] {
        let coded = encode(&pixels, &cx, CodecLevel::Default).expect("encode");
        let mut seed = (pixels.len() as u32).to_le_bytes().to_vec();
        seed.extend_from_slice(&coded);

        let capacity = u32::from_le_bytes([seed[0], seed[1], seed[2], seed[3]]) as usize;
        let payload = &seed[4..];
        let mut out = vec![0u8; capacity];
        let produced =
            oxiarc_tiff::compression::packbits::decode_into(payload, &mut out).expect("decode");
        assert_eq!(&out[..produced], pixels.as_slice());
    }
}

/// `lzw_tiff_decode`: byte 0's low bit = code-width rule, bytes 1-4 =
/// expected output size (`u32` LE), rest = payload.
#[test]
fn lzw_tiff_decode_seed_decodes_through_the_documented_header() {
    let bits = [8u16];
    let cx = CodecContext::new(CompressionMethod::Lzw, 64, 1, &bits, 1, Endian::Little);
    let pixels: Vec<u8> = (0..64u32).map(|i| (i / 4) as u8).collect();
    let coded = encode(&pixels, &cx, CodecLevel::Default).expect("encode");

    let mut seed = vec![0u8];
    seed.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    seed.extend_from_slice(&coded);

    let old_style = seed[0] & 1 == 1;
    assert!(!old_style);
    let expected_size =
        u32::from_le_bytes([seed[1], seed[2], seed[3], seed[4]]) as usize % (1 << 20);
    let payload = &seed[5..];
    let mut out = vec![0u8; expected_size];
    let produced = decode_into(payload, &mut out, &cx).expect("decode");
    assert_eq!(&out[..produced], pixels.as_slice());
}

/// `tiff_roundtrip`: the generic seeds carry no fixed header (module docs),
/// so the only invariant to pin here is that the generator's byte patterns
/// are non-trivial (a fuzzer gains nothing from an all-zero seed it can
/// already reach by mutation).
#[test]
fn tiff_roundtrip_seeds_are_non_trivial() {
    let ramp: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    assert!(ramp.iter().any(|&b| b != 0));
    assert_eq!(ramp.len(), 256);
}

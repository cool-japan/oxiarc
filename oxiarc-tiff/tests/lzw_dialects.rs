//! The three LZW dialects a TIFF strip can be written in, end to end.
//!
//! `compression::lzw`'s unit tests cover the codec in isolation; these drive
//! whole files through [`Decoder`], so the sniff, the per-image cache and the
//! chunk pipeline are all in the loop. Every fixture is built here — either by
//! `oxiarc-lzw` with an explicit [`LzwConfig`], or bit by bit — because the
//! writer only ever emits the standard dialect.
#![cfg(feature = "lzw")]

mod support;

use oxiarc_lzw::LzwConfig;
use oxiarc_tiff::compression::lzw::is_compat_lsb;
use oxiarc_tiff::compression::{CodecContext, CodecState, decode_into};
use oxiarc_tiff::{CompressionMethod, Decoder, Endian};
use std::io::Cursor;
use support::RawTiff;

/// Eight rows of sixteen pixels, in four two-row strips.
const WIDTH: u32 = 16;
const HEIGHT: u32 = 8;
const ROWS_PER_STRIP: u32 = 2;

/// Deterministic pixels with enough repetition for the code table to grow.
fn pixels() -> Vec<u8> {
    (0..(WIDTH * HEIGHT) as usize)
        .map(|i| ((i / 3) % 17 * 15) as u8)
        .collect()
}

/// A four-strip greyscale TIFF whose strips are coded with `config`.
fn lzw_tiff(config: LzwConfig) -> (Vec<u8>, Vec<u8>) {
    let data = pixels();
    let per_strip = (WIDTH * ROWS_PER_STRIP) as usize;
    let mut tiff = RawTiff::new();
    let mut offsets = Vec::new();
    let mut counts = Vec::new();
    for chunk in data.chunks(per_strip) {
        let coded = oxiarc_lzw::compress(chunk, config).expect("lzw encode");
        counts.push(coded.len() as u32);
        offsets.push(tiff.add_data(&coded) as u32);
    }
    tiff.long(256, &[WIDTH]);
    tiff.long(257, &[HEIGHT]);
    tiff.short(258, &[8]);
    tiff.short(259, &[5]);
    tiff.short(262, &[1]);
    tiff.long(273, &offsets);
    tiff.short(277, &[1]);
    tiff.long(278, &[ROWS_PER_STRIP]);
    tiff.long(279, &counts);
    (tiff.build(), data)
}

#[test]
fn a_compat_lsb_file_decodes_through_the_whole_pipeline() {
    let (file, want) = lzw_tiff(LzwConfig::TIFF_COMPAT_LSB);
    let mut decoder = Decoder::new(Cursor::new(&file)).expect("open");
    let mut got = vec![0u8; want.len()];
    decoder.read_image_bytes(&mut got).expect("decode");
    assert_eq!(got, want, "every strip must round trip");
    // The dialect really was the LSB one: prove it from the decoder's own
    // per-image state, not merely from the decode having succeeded.
    let state = decoder.info().expect("info").codec_state.clone();
    assert!(
        state.lzw_is_compat_lsb(),
        "the image must have settled on the compat dialect"
    );
}

#[test]
fn every_strip_of_a_compat_file_carries_the_sniffable_prefix() {
    // The cache short-circuits the sniff after strip 0, so this is what makes
    // "sniff per image" safe here: all four strips are the same dialect.
    let (file, _) = lzw_tiff(LzwConfig::TIFF_COMPAT_LSB);
    let mut decoder = Decoder::new(Cursor::new(&file)).expect("open");
    let count = decoder.chunk_count().expect("chunk count");
    assert_eq!(count, 4, "the fixture must be multi-strip");
    let mut sniffed = 0;
    for index in 0..count {
        let raw = decoder.read_chunk_raw(index).expect("raw strip");
        assert!(is_compat_lsb(&raw), "strip {index}: {:02x?}", &raw[..2]);
        sniffed += 1;
    }
    assert_eq!(sniffed, 4);
}

#[test]
fn a_standard_file_still_decodes_and_is_not_sniffed_as_compat() {
    let (file, want) = lzw_tiff(LzwConfig::TIFF);
    let mut decoder = Decoder::new(Cursor::new(&file)).expect("open");
    let mut got = vec![0u8; want.len()];
    decoder.read_image_bytes(&mut got).expect("decode");
    assert_eq!(got, want);
    let state = decoder.info().expect("info").codec_state.clone();
    assert!(!state.lzw_is_compat_lsb());
    for index in 0..decoder.chunk_count().expect("chunk count") {
        let raw = decoder.read_chunk_raw(index).expect("raw strip");
        assert!(!is_compat_lsb(&raw), "strip {index}");
    }
}

#[test]
fn an_old_style_msb_file_still_decodes_by_retry() {
    // The third dialect: MSB packing with the late code-width change. It has
    // no sniff, so this exercises the retry path the compat branch must not
    // have disturbed.
    let (file, want) = lzw_tiff(LzwConfig::TIFF_OLD_STYLE);
    let mut decoder = Decoder::new(Cursor::new(&file)).expect("open");
    let mut got = vec![0u8; want.len()];
    decoder.read_image_bytes(&mut got).expect("decode");
    assert_eq!(got, want);
}

#[test]
fn a_compat_file_is_not_readable_as_a_standard_one() {
    // Non-vacuity: without the sniff this file would fail or decode wrong,
    // so the passing test above is really testing the new code path.
    let (file, want) = lzw_tiff(LzwConfig::TIFF_COMPAT_LSB);
    let mut standard = vec![0u8; want.len()];
    let per_strip = (WIDTH * ROWS_PER_STRIP) as usize;
    let mut agreed = 0usize;
    let mut decoder = Decoder::new(Cursor::new(&file)).expect("open");
    for index in 0..4usize {
        let raw = decoder.read_chunk_raw(index as u64).expect("raw strip");
        let range = index * per_strip..(index + 1) * per_strip;
        let slot = &mut standard[range.clone()];
        if let Ok(written) = oxiarc_lzw::decompress_tiff_into(&raw, slot) {
            if written == per_strip && slot == &want[range] {
                agreed += 1;
            }
        }
    }
    assert_eq!(
        agreed, 0,
        "the standard rule must not reproduce any compat strip"
    );
}

/// Packs nine-bit codes into a strip, least significant bit of each code
/// first, which is how libtiff's `LZWDecodeCompat` writers packed them.
///
/// Hand-written rather than taken from `oxiarc_lzw::compress`: a fixture the
/// encoder produced could only prove that the encoder and the decoder share a
/// convention, not that the convention is libtiff's.
fn pack_lsb(codes: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut held = 0u8;
    for code in codes {
        accumulator |= u32::from(*code) << held;
        held += 9;
        while held >= 8 {
            out.push((accumulator & 0xFF) as u8);
            accumulator >>= 8;
            held -= 8;
        }
    }
    if held > 0 {
        out.push((accumulator & 0xFF) as u8);
    }
    out
}

/// The same codes packed most significant bit first: the standard dialect.
fn pack_msb(codes: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut held = 0u8;
    for code in codes {
        accumulator = (accumulator << 9) | u32::from(*code);
        held += 9;
        while held >= 8 {
            held -= 8;
            out.push(((accumulator >> held) & 0xFF) as u8);
        }
    }
    if held > 0 {
        out.push(((accumulator << (8 - held)) & 0xFF) as u8);
    }
    out
}

/// A code sequence that stays inside nine bits, so it says nothing about the
/// early-versus-late width change and everything about the bit order.
///
/// `Clear(256)`, `1`, `1` (which adds 258 = `11`), `258`, `2`, `2` (adding
/// 261 = `22`), `261`, `EOI(257)` — eight bytes of output.
const HAND_CODES: [u16; 8] = [256, 1, 1, 258, 2, 2, 261, 257];
/// What [`HAND_CODES`] expands to.
const HAND_BYTES: [u8; 8] = [1, 1, 1, 1, 2, 2, 2, 2];

fn codec_context<'a>(state: Option<&'a CodecState>, bits: &'a [u16]) -> CodecContext<'a> {
    let mut cx = CodecContext::new(CompressionMethod::Lzw, 8, 1, bits, 1, Endian::Little);
    cx.state = state;
    cx
}

#[test]
fn a_hand_packed_compat_strip_carries_libtiffs_own_two_byte_signature() {
    let strip = pack_lsb(&HAND_CODES);
    // libtiff's `LZWPreDecode` test, transcribed: the `ClearCode` packed
    // least-significant-bit first puts nothing in the first byte and its
    // ninth bit in bit 0 of the second.
    assert_eq!(strip.first(), Some(&0x00), "{strip:02x?}");
    assert_eq!(strip.get(1).map(|byte| byte & 1), Some(1), "{strip:02x?}");
    assert!(is_compat_lsb(&strip));

    // The same codes the other way round open with `0x80`, which is what
    // makes the test decisive rather than merely true.
    let standard = pack_msb(&HAND_CODES);
    assert_eq!(standard.first(), Some(&0x80), "{standard:02x?}");
    assert!(!is_compat_lsb(&standard));
}

#[test]
fn hand_packed_strips_of_both_bit_orders_decode_to_the_same_pixels() {
    let bits = [8u16];
    for (label, strip) in [
        ("compat", pack_lsb(&HAND_CODES)),
        ("standard", pack_msb(&HAND_CODES)),
    ] {
        let state = CodecState::new();
        let cx = codec_context(Some(&state), &bits);
        let mut out = [0u8; 8];
        assert_eq!(
            decode_into(&strip, &mut out, &cx).expect(label),
            HAND_BYTES.len(),
            "{label}"
        );
        assert_eq!(out, HAND_BYTES, "{label}");
        assert_eq!(
            state.lzw_is_compat_lsb(),
            label == "compat",
            "{label}: the image must settle on the right dialect"
        );
    }
}

#[test]
fn a_hand_packed_compat_strip_decodes_through_the_whole_pipeline() {
    let strip = pack_lsb(&HAND_CODES);
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&strip) as u32;
    tiff.long(256, &[8]);
    tiff.long(257, &[1]);
    tiff.short(258, &[8]);
    tiff.short(259, &[5]);
    tiff.short(262, &[1]);
    tiff.long(273, &[offset]);
    tiff.short(277, &[1]);
    tiff.long(278, &[1]);
    tiff.long(279, &[strip.len() as u32]);
    let file = tiff.build();

    let mut decoder = Decoder::new(Cursor::new(&file)).expect("open");
    let mut got = [0u8; 8];
    decoder.read_image_bytes(&mut got).expect("decode");
    assert_eq!(got, HAND_BYTES);
    assert!(
        decoder
            .info()
            .expect("info")
            .codec_state
            .lzw_is_compat_lsb()
    );
}

#[test]
fn the_dialect_is_cached_per_image_and_not_re_sniffed_per_strip() {
    // Non-vacuity for the cache. Once a strip has proved the image is compat,
    // a *standard* strip handed to the same state must be decoded with the
    // compat rule — and therefore not reproduce its own bytes. If the codec
    // re-sniffed per strip this would succeed, and the cache would be doing
    // nothing.
    let bits = [8u16];
    let state = CodecState::new();
    let cx = codec_context(Some(&state), &bits);

    let compat = pack_lsb(&HAND_CODES);
    let mut out = [0u8; 8];
    decode_into(&compat, &mut out, &cx).expect("the strip that decides");
    assert_eq!(out, HAND_BYTES);
    assert!(state.lzw_is_compat_lsb(), "the sniff must have fired");

    let standard = pack_msb(&HAND_CODES);
    let mut out = [0u8; 8];
    let decoded = decode_into(&standard, &mut out, &cx);
    assert!(
        decoded.is_err() || out != HAND_BYTES,
        "a standard strip must not decode correctly under the cached compat \
         rule; the cache is being ignored"
    );

    // And the same strip through a *fresh* state decodes perfectly, so the
    // failure above is the cache and not the strip.
    let fresh = CodecState::new();
    let cx = codec_context(Some(&fresh), &bits);
    let mut out = [0u8; 8];
    decode_into(&standard, &mut out, &cx).expect("a fresh image");
    assert_eq!(out, HAND_BYTES);
    assert!(!fresh.lzw_is_compat_lsb());
}

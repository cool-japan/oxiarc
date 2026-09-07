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
use oxiarc_tiff::Decoder;
use oxiarc_tiff::compression::lzw::is_compat_lsb;
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

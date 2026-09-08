//! T.4 §4.2.1.3.2 uncompressed mode, end to end through the writer and reader.
//!
//! The codec's own tests (in `compression::ccitt`) cover the code table row by
//! row and one hand-assembled segment at a time. These drive whole TIFF files:
//! the option tags, the encoder's per-row choice and the reader's decode all
//! in the loop.
//!
//! Uncompressed mode is the one thing this crate writes that libtiff cannot
//! read back — `Fax3Decode2D` reports `Uncompressed data (not supported)` —
//! so the interoperability requirement is weaker than for every other codec
//! and is stated as such: `tiffinfo` must *parse* the file and report the
//! option bit. The oracle half lives in `tiff_oracle_codecs.rs`.
#![cfg(feature = "ccitt")]

mod support;

use oxiarc_tiff::{
    ColorType, Compression, Decoder, Encoder, ImageSpec, Layout, PhotometricInterpretation,
    Samples, Tag, Value,
};
use std::io::Cursor;
use support::RawTiff;

/// A page whose rows alternate between long runs (where fax coding wins) and
/// dithered noise (where uncompressed mode does), so one file exercises both
/// arms of the encoder's per-row choice.
fn mixed_page(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; (width * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let value = if y % 2 == 0 {
                // Long runs: two blocks of black.
                x < width / 4 || (x > width / 2 && x < width * 3 / 4)
            } else {
                // Dither: isolated pixels, the case fax coding expands.
                (x + y) % 2 == 0 || (x * 7 + y * 3) % 11 == 0
            };
            out[(y * width + x) as usize] = u8::from(value);
        }
    }
    out
}

/// Every pixel isolated: the case uncompressed mode exists for.
fn dithered_page(width: u32, height: u32) -> Vec<u8> {
    (0..width * height)
        .map(|index| u8::from((index % 2 == 0) ^ ((index / width) % 2 == 0)))
        .collect()
}

fn write(spec: &ImageSpec, pixels: &[u8]) -> Vec<u8> {
    let mut buffer = Vec::new();
    let mut encoder = Encoder::new(Cursor::new(&mut buffer)).expect("encoder");
    encoder.write_image(spec, pixels).expect("write");
    encoder.finish().expect("finish");
    buffer
}

fn read(file: &[u8], len: usize) -> Vec<u8> {
    let mut decoder = Decoder::new(Cursor::new(file)).expect("decoder");
    let mut out = vec![0u8; len];
    decoder.read_image_bytes(&mut out).expect("decode");
    out
}

fn spec(width: u32, height: u32, compression: Compression, allow: bool) -> ImageSpec {
    ImageSpec::new(width, height, ColorType::Gray(1))
        .with_photometric(PhotometricInterpretation::WhiteIsZero)
        .with_compression(compression)
        .with_layout(Layout::Strips {
            rows_per_strip: height,
        })
        .with_ccitt_uncompressed(allow)
}

fn tag(file: &[u8], which: Tag) -> Option<u32> {
    let mut decoder = Decoder::new(Cursor::new(file)).expect("decoder");
    match decoder.find_tag(which).expect("find tag") {
        Some(Value::Long(values)) => values.first().copied(),
        Some(Value::Short(values)) => values.first().copied().map(u32::from),
        _ => None,
    }
}

const G3_2D: Compression = Compression::CcittGroup3 {
    two_dimensional: true,
    byte_align_eol: false,
};
const G3_1D: Compression = Compression::CcittGroup3 {
    two_dimensional: false,
    byte_align_eol: false,
};

#[test]
fn every_dialect_round_trips_bit_exactly_with_the_mode_allowed() {
    let (width, height) = (211u32, 17u32);
    for pixels in [mixed_page(width, height), dithered_page(width, height)] {
        for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
            let file = write(&spec(width, height, compression, true), &pixels);
            let got = read(&file, pixels.len());
            assert_eq!(
                got, pixels,
                "{compression:?} at {width}x{height} did not round trip"
            );
        }
    }
}

#[test]
fn a_dithered_page_is_smaller_when_the_mode_is_allowed() {
    let (width, height) = (512u32, 64u32);
    let pixels = dithered_page(width, height);
    for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
        let without = write(&spec(width, height, compression, false), &pixels);
        let with = write(&spec(width, height, compression, true), &pixels);
        assert!(
            with.len() < without.len(),
            "{compression:?}: {} bytes with the mode, {} without",
            with.len(),
            without.len()
        );
        // Not marginally smaller: the whole point is that fax coding expands
        // this data and uncompressed mode does not.
        assert!(
            with.len() * 2 < without.len(),
            "{compression:?}: expected roughly a halving, got {} vs {}",
            with.len(),
            without.len()
        );
    }
}

#[test]
fn a_page_of_long_runs_is_byte_identical_whether_or_not_the_mode_is_allowed() {
    // The per-row choice must never take the mode when it loses, so allowing
    // it cannot change a page that has no use for it — apart from the option
    // tag, which is why the strips are compared rather than the files.
    let (width, height) = (256u32, 32u32);
    let pixels: Vec<u8> = (0..width * height)
        .map(|index| u8::from((index / 37) % 3 == 0))
        .collect();
    for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
        let without = write(&spec(width, height, compression, false), &pixels);
        let with = write(&spec(width, height, compression, true), &pixels);
        let strip = |file: &[u8]| {
            let mut decoder = Decoder::new(Cursor::new(file.to_vec())).expect("decoder");
            decoder.read_chunk_raw(0).expect("raw strip")
        };
        assert_eq!(
            strip(&with),
            strip(&without),
            "{compression:?}: the mode was taken on a page it cannot help"
        );
    }
}

#[test]
fn the_option_tag_bit_is_written_in_the_tag_the_codec_owns() {
    let (width, height) = (64u32, 8u32);
    let pixels = dithered_page(width, height);

    // Group 3: bit 1 of `T4Options` (292), alongside the 2D bit.
    let file = write(&spec(width, height, G3_2D, true), &pixels);
    let t4 = tag(&file, Tag::T4Options).expect("T4Options");
    assert_eq!(t4 & 2, 2, "T4Options bit 1 must be set: {t4:#b}");
    assert_eq!(t4 & 1, 1, "the 2D bit must survive: {t4:#b}");
    assert_eq!(tag(&file, Tag::T6Options), None, "no T6Options on Group 3");

    // Group 4: bit 1 of `T6Options` (293), and no `T4Options` at all.
    let file = write(
        &spec(width, height, Compression::CcittGroup4, true),
        &pixels,
    );
    assert_eq!(tag(&file, Tag::T6Options), Some(2));
    assert_eq!(tag(&file, Tag::T4Options), None, "no T4Options on Group 4");

    // Off by default, in both tags.
    let file = write(&spec(width, height, G3_2D, false), &pixels);
    assert_eq!(tag(&file, Tag::T4Options).expect("T4Options") & 2, 0);
    let file = write(
        &spec(width, height, Compression::CcittGroup4, false),
        &pixels,
    );
    assert_eq!(tag(&file, Tag::T6Options), None);
}

#[test]
fn a_file_that_uses_the_mode_without_declaring_it_still_decodes() {
    // The entrance code is unambiguous, so a reader that refused the data
    // because tag 292 was left out would be strictly worse than one that did
    // not. Built by writing with the option on and then clearing the tag.
    let (width, height) = (128u32, 16u32);
    let pixels = dithered_page(width, height);
    let file = write(&spec(width, height, G3_2D, true), &pixels);
    let mut decoder = Decoder::new(Cursor::new(file.clone())).expect("decoder");
    let strip = decoder.read_chunk_raw(0).expect("raw strip");

    // Re-file that exact strip with `T4Options` claiming 2D only.
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(&strip) as u32;
    tiff.long(256, &[width]);
    tiff.long(257, &[height]);
    tiff.short(258, &[1]);
    tiff.short(259, &[3]);
    tiff.short(262, &[0]);
    tiff.long(273, &[offset]);
    tiff.short(277, &[1]);
    tiff.long(278, &[height]);
    tiff.long(279, &[strip.len() as u32]);
    tiff.long(292, &[1]);
    let stripped = tiff.build();

    assert_eq!(
        tag(&stripped, Tag::T4Options).expect("T4Options") & 2,
        0,
        "the fixture must not declare the mode"
    );
    assert_eq!(read(&stripped, pixels.len()), pixels);
}

#[test]
fn the_mode_survives_fill_order_two() {
    // The fax codecs consume `FillOrder` themselves, so an uncompressed
    // segment's bits are reversed with everything else.
    let (width, height) = (192u32, 12u32);
    let pixels = dithered_page(width, height);
    for compression in [G3_2D, Compression::CcittGroup4] {
        let file = write(
            &spec(width, height, compression, true)
                .with_fill_order(oxiarc_tiff::FillOrder::Lsb2Msb),
            &pixels,
        );
        assert_eq!(read(&file, pixels.len()), pixels, "{compression:?}");
    }
}

#[test]
fn a_page_wider_than_one_strip_keeps_its_rows_aligned() {
    // Several strips, each entering and leaving the mode on its own, so a
    // segment that ran to the end of a strip cannot bleed into the next.
    let (width, height) = (100u32, 40u32);
    let pixels = mixed_page(width, height);
    for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
        let file = write(
            &ImageSpec::new(width, height, ColorType::Gray(1))
                .with_photometric(PhotometricInterpretation::WhiteIsZero)
                .with_compression(compression)
                .with_layout(Layout::Strips { rows_per_strip: 7 })
                .with_ccitt_uncompressed(true),
            &pixels,
        );
        assert_eq!(read(&file, pixels.len()), pixels, "{compression:?}");
    }
}

#[test]
fn the_samples_api_agrees_with_the_byte_api() {
    let (width, height) = (64u32, 9u32);
    let pixels = dithered_page(width, height);
    let file = write(
        &spec(width, height, Compression::CcittGroup4, true),
        &pixels,
    );
    let mut decoder = Decoder::new(Cursor::new(file)).expect("decoder");
    match decoder.read_image().expect("decode") {
        Samples::U8(values) => assert_eq!(values, pixels),
        other => panic!("unexpected sample type {other:?}"),
    }
}

/// A deterministic pseudo-random bilevel page at a chosen black density.
fn random_page(width: u32, height: u32, seed: u64, density: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..width * height)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            u8::from((state >> 33) as u32 % 100 < density)
        })
        .collect()
}

#[test]
fn a_sweep_of_widths_densities_and_dialects_round_trips() {
    // The encoder's per-row choice flips with density, so this crosses the
    // boundary between the two arms many times in one test — including the
    // widths where a row ends inside a code word and the densities where
    // whole rows are one colour.
    let mut checked = 0usize;
    for width in [1u32, 2, 5, 8, 9, 15, 16, 17, 31, 64, 100, 127, 128, 333] {
        for density in [0u32, 1, 15, 50, 85, 99, 100] {
            let height = 5u32;
            let pixels = random_page(width, height, u64::from(width * 31 + density), density);
            for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
                let file = write(&spec(width, height, compression, true), &pixels);
                let got = read(&file, pixels.len());
                assert_eq!(
                    got, pixels,
                    "{compression:?} {width}x{height} at {density}% black"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 14 * 7 * 3, "the sweep must actually have run");
}

#[test]
fn an_all_white_and_an_all_black_page_round_trip_with_the_mode_allowed() {
    // The two rows the escape rule is most likely to get wrong: one that is
    // nothing but `000001` words, and one that is nothing but `1` words.
    for fill in [0u8, 1u8] {
        let (width, height) = (137u32, 4u32);
        let pixels = vec![fill; (width * height) as usize];
        for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
            let file = write(&spec(width, height, compression, true), &pixels);
            assert_eq!(read(&file, pixels.len()), pixels, "{compression:?} {fill}");
        }
    }
}

#[test]
fn hostile_streams_in_the_mode_neither_panic_nor_hang() {
    // The entrance code used to be a hard error, so arbitrary bytes never
    // reached the segment decoder. Now they do: every word must either make
    // progress, hit the row edge, or end the segment, so a stream of noise
    // behind an entrance code has to terminate.
    let (width, height) = (64u32, 8u32);
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 32) as u8
    };
    for case in 0..400 {
        let mut strip = Vec::with_capacity(64);
        // The two-dimensional entrance code, then noise.
        strip.push(0b0000_0011);
        strip.push(0b1100_0000 | (next() & 0x3F));
        for _ in 0..(8 + case % 48) {
            strip.push(next());
        }
        for compression in [3u16, 4] {
            let mut tiff = RawTiff::new();
            let offset = tiff.add_data(&strip) as u32;
            tiff.long(256, &[width]);
            tiff.long(257, &[height]);
            tiff.short(258, &[1]);
            tiff.short(259, &[compression]);
            tiff.short(262, &[0]);
            tiff.long(273, &[offset]);
            tiff.short(277, &[1]);
            tiff.long(278, &[height]);
            tiff.long(279, &[strip.len() as u32]);
            tiff.long(292, &[3]);
            tiff.long(293, &[2]);
            let file = tiff.build();
            // Both leniencies: strict must report, lenient must fill.
            for leniency in [
                oxiarc_tiff::Leniency::Strict,
                oxiarc_tiff::Leniency::Normal,
                oxiarc_tiff::Leniency::Lenient,
            ] {
                let mut decoder = Decoder::new(Cursor::new(file.clone()))
                    .expect("open")
                    .with_leniency(leniency);
                let mut out = vec![0u8; (width * height) as usize];
                // Any answer is acceptable; not returning is not.
                let _ = decoder.read_image_bytes(&mut out);
            }
        }
    }
}

/// Every dialect, with rows that flip between "uncompressed mode wins" and
/// "Huffman coding wins" in every combination.
///
/// The property this exists for is the *reference line*: a two-dimensionally
/// coded row is coded against the changing elements of the row above, so a
/// row the encoder wrote in uncompressed mode has to decode to exactly the
/// changing-element list the encoder used as the next row's reference. A
/// segment that recorded one element too many — or cancelled one it should
/// have kept — would leave the following ordinary row decoding against a
/// reference line the encoder never had, and only a page that mixes both
/// kinds of row can catch it.
#[test]
fn rows_of_both_kinds_in_one_page_round_trip_in_every_dialect() {
    let mut state = 0x243F_6A88_85A3_08D3u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 32) as u32
    };
    let mut checked = 0usize;
    for _ in 0..24 {
        let width = 1 + (next() % 200);
        let height = 1 + (next() % 12);
        let mut pixels = vec![0u8; (width * height) as usize];
        for y in 0..height {
            // A row is dithered (uncompressed mode wins), long-run (Huffman
            // wins) or noisy (either may win), independently of its
            // neighbours.
            let kind = next() % 3;
            for x in 0..width {
                let black = match kind {
                    0 => (x + y) % 2 == 0,
                    1 => x < width / 3 || (x > width / 2 && x < width * 2 / 3),
                    _ => (next() % 7) < 3,
                };
                pixels[(y * width + x) as usize] = u8::from(black);
            }
        }
        for compression in [
            G3_1D,
            G3_2D,
            Compression::CcittGroup3 {
                two_dimensional: true,
                byte_align_eol: true,
            },
            Compression::CcittGroup4,
            Compression::CcittRle,
        ] {
            for allow in [false, true] {
                let file = write(&spec(width, height, compression, allow), &pixels);
                assert_eq!(
                    read(&file, pixels.len()),
                    pixels,
                    "{compression:?} allow={allow} {width}x{height}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 24 * 5 * 2, "the sweep must actually have run");
}

/// Widths where a row's last uncompressed-mode word, its exit code and the
/// chunk's own zero padding all land in the same byte.
#[test]
fn narrow_pages_round_trip_with_the_mode_allowed() {
    let mut checked = 0usize;
    for width in 1..=17u32 {
        for height in [1u32, 2, 3, 8] {
            let pixels: Vec<u8> = (0..width * height)
                .map(|index| u8::from(index % 2 == 0))
                .collect();
            for compression in [G3_1D, G3_2D, Compression::CcittGroup4] {
                for allow in [false, true] {
                    let file = write(&spec(width, height, compression, allow), &pixels);
                    assert_eq!(
                        read(&file, pixels.len()),
                        pixels,
                        "{compression:?} allow={allow} {width}x{height}"
                    );
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 17 * 4 * 3 * 2, "the sweep must actually have run");
}

/// The strip table of a single-strip little-endian classic file.
fn single_strip(file: &[u8]) -> Vec<u8> {
    let ifd = u32::from_le_bytes([file[4], file[5], file[6], file[7]]) as usize;
    let count = usize::from(u16::from_le_bytes([file[ifd], file[ifd + 1]]));
    let (mut offset, mut length) = (0usize, 0usize);
    for index in 0..count {
        let base = ifd + 2 + index * 12;
        let which = u16::from_le_bytes([file[base], file[base + 1]]);
        let kind = u16::from_le_bytes([file[base + 2], file[base + 3]]);
        let value = if kind == 3 {
            usize::from(u16::from_le_bytes([file[base + 8], file[base + 9]]))
        } else {
            u32::from_le_bytes([
                file[base + 8],
                file[base + 9],
                file[base + 10],
                file[base + 11],
            ]) as usize
        };
        match which {
            273 => offset = value,
            279 => length = value,
            _ => {}
        }
    }
    assert!(
        length > 0 && offset + length <= file.len(),
        "bad strip table"
    );
    file[offset..offset + length].to_vec()
}

/// Rebuilds a file around one hand-damaged strip and decodes it three ways.
fn decode_damaged(strip: &[u8], width: u32, height: u32, method: u16) {
    let mut tiff = RawTiff::new();
    let offset = tiff.add_data(strip) as u32;
    tiff.long(256, &[width]);
    tiff.long(257, &[height]);
    tiff.short(258, &[1]);
    tiff.short(259, &[method]);
    tiff.short(262, &[0]);
    tiff.long(273, &[offset]);
    tiff.short(277, &[1]);
    tiff.long(278, &[height]);
    tiff.long(279, &[strip.len() as u32]);
    if method == 3 {
        tiff.long(292, &[3]);
    } else {
        tiff.long(293, &[2]);
    }
    let file = tiff.build();
    for leniency in [
        oxiarc_tiff::Leniency::Strict,
        oxiarc_tiff::Leniency::Normal,
        oxiarc_tiff::Leniency::Lenient,
    ] {
        let Ok(decoder) = Decoder::new(Cursor::new(file.clone())) else {
            return;
        };
        let mut decoder = decoder.with_leniency(leniency);
        let mut out = vec![0u8; (width * height) as usize];
        // Any answer is acceptable; not returning, or panicking, is not.
        let _ = decoder.read_image_bytes(&mut out);
    }
}

/// A real uncompressed-mode strip, truncated at **every** byte offset and
/// then flipped at every bit, decoded under every leniency.
///
/// The entrance code used to be a hard error, so no crafted bytes reached the
/// segment decoder at all; now every code word has to either make progress,
/// hit the row edge or leave the mode, and the exit paths (`Invalid`,
/// `Truncated`, `Overrun`) have to be reachable without a panic or a spin.
#[test]
fn a_real_strip_truncated_and_flipped_everywhere_neither_panics_nor_hangs() {
    let (width, height) = (72u32, 9u32);
    let pixels: Vec<u8> = (0..width * height)
        .map(|index| u8::from((index % 2 == 0) ^ ((index / width) % 3 == 0)))
        .collect();
    let mut probes = 0usize;
    for (compression, method) in [(G3_1D, 3u16), (G3_2D, 3), (Compression::CcittGroup4, 4)] {
        let file = write(&spec(width, height, compression, true), &pixels);
        let strip = single_strip(&file);
        assert!(
            strip.len() > 16,
            "{compression:?}: strip too small to sweep"
        );
        for cut in 0..strip.len() {
            decode_damaged(&strip[..cut], width, height, method);
            probes += 1;
        }
        for bit in 0..(strip.len() * 8) {
            let mut flipped = strip.clone();
            flipped[bit / 8] ^= 0x80 >> (bit % 8);
            decode_damaged(&flipped, width, height, method);
            probes += 1;
        }
        // And a few insertions, which shift every following code word.
        for at in [0usize, 1, strip.len() / 2, strip.len() - 1] {
            let mut grown = strip.clone();
            grown.insert(at, 0x5A);
            decode_damaged(&grown, width, height, method);
            probes += 1;
        }
    }
    assert!(probes > 2500, "only {probes} hostile streams were decoded");
}

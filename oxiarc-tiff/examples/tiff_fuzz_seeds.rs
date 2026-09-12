//! Seed-corpus generator for `oxiarc-tiff`'s five planned fuzz targets
//! (tiff-design.md section 5.2 item 8): `tiff_decode`, `tiff_roundtrip`,
//! `ccitt_decode`, `packbits_decode`, `lzw_tiff_decode`.
//!
//! Mirrors `oxiarc-http/examples/http_fuzz_seeds.rs`'s shape exactly (that crate
//! wrote its generator before its own fuzz targets landed too -- Wave 3 owns
//! `fuzz/`, the seeds are ready for whenever it adds the `[[bin]]` entries
//! below).
//!
//! ```text
//! # write every corpus under fuzz/corpus/<target>/
//! cargo run -p oxiarc-tiff --example tiff_fuzz_seeds --features all-codecs -- fuzz/corpus
//!
//! # or into a scratch directory (the default is the system temp dir)
//! cargo run -p oxiarc-tiff --example tiff_fuzz_seeds --features all-codecs
//! ```
//!
//! The `tiff_` prefix is there because cargo example target names share
//! one workspace-global output directory (`target/debug/examples/`).
//!
//! `fuzz/corpus/` is git-ignored (root `TODO.md` Known Issue 9), so seeds are
//! regenerated, never committed -- which is why this generator is the
//! durable artefact and the corpus is not.
//!
//! The invariants each target's seeds are meant to reach are pinned as real
//! tests in `tests/fuzz_seeds.rs`, so a seed that stops being interesting
//! fails the suite rather than silently rotting.
//!
//! # The five targets this seeds, and the `fuzz_targets/*.rs` Wave 3 should
//! write for each (byte layout documented once here, re-derived in
//! `tests/fuzz_seeds.rs`, so the two never drift):
//!
//! * **`tiff_decode`**: `fuzz_target!(|data: &[u8]| { let _ =
//!   oxiarc_tiff::Decoder::new(Cursor::new(data)).and_then(|mut d|
//!   d.read_image()); })` -- whole files, no header.
//! * **`tiff_roundtrip`**: an `Unstructured`-driven `ImageSpec` + pixel
//!   buffer (small bounded dimensions), encode then decode, assert the
//!   pixels match. Seeds here are generic varied-byte buffers (this crate
//!   does not fix `ImageSpec`'s exact `Unstructured` parse order; Wave 3
//!   should feel free to regenerate more targeted seeds once it does).
//! * **`ccitt_decode`**: byte 0 selects the flavour (`0`=RLE, `1`=G3 1D,
//!   `2`=G3 2D, `3`=G4), bytes 1-2 are `width` (`u16`, `% 512`), byte 3 is
//!   `T4Options`/`T6Options`' raw bits, the rest is the coded payload, fed to
//!   [`oxiarc_tiff::compression::decode_into`] with a `CodecContext` built
//!   from those fields.
//! * **`packbits_decode`**: bytes 0-3 are the destination capacity (`u32`,
//!   `% (1<<20)`), the rest is the coded payload, fed to
//!   [`oxiarc_tiff::compression::decode_into`].
//! * **`lzw_tiff_decode`**: byte 0's low bit selects old-style vs. standard
//!   code width, bytes 1-4 are the expected output size (`u32`, `%
//!   (1<<20)`), the rest is the coded payload.

use oxiarc_tiff::compression::{CodecContext, CodecLevel, encode};
use oxiarc_tiff::tags::{PhotometricInterpretation, T4Options, T6Options};
use oxiarc_tiff::{ColorType, CompressionMethod, Encoder, Endian, ImageSpec, Layout};
use std::io::Cursor;
use std::path::{Path, PathBuf};

/// One seed input: a file name and its bytes.
type Seed = (String, Vec<u8>);

/// One fuzz target's corpus: the target's name and its seeds.
type Corpus = (&'static str, Vec<Seed>);

/// The five targets, each with its seed inputs.
fn corpora() -> Vec<Corpus> {
    vec![
        ("tiff_decode", tiff_decode_seeds()),
        ("tiff_roundtrip", tiff_roundtrip_seeds()),
        ("ccitt_decode", ccitt_decode_seeds()),
        ("packbits_decode", packbits_decode_seeds()),
        ("lzw_tiff_decode", lzw_tiff_decode_seeds()),
    ]
}

fn named(seeds: Vec<(&'static str, Vec<u8>)>) -> Vec<Seed> {
    seeds
        .into_iter()
        .map(|(name, bytes)| (name.to_string(), bytes))
        .collect()
}

/// A small deterministic bilevel test pattern (not all-zero or all-one, so
/// CCITT's changing-element engine actually has work to do): alternating
/// bars with one irregular row. One byte per pixel (0 or 1) -- the *native*
/// format [`Encoder::write_image`] wants; the on-disk 1-bit-per-pixel
/// packing happens inside the writer pipeline. For the CCITT codec
/// functions directly (which read/write already-packed bits, see
/// [`bilevel_pattern_packed`]), pack it first.
fn bilevel_pattern_native(width: usize, height: usize) -> Vec<u8> {
    let mut bits = vec![0u8; width * height];
    for (y, row) in bits.chunks_exact_mut(width).enumerate() {
        for (x, pixel) in row.iter_mut().enumerate() {
            *pixel = if y == height / 2 {
                u8::from((x * 7 + y) % 5 < 2)
            } else {
                u8::from((x / 4 + y) % 2 == 0)
            };
        }
    }
    bits
}

/// [`bilevel_pattern_native`], packed MSB-first at one bit per pixel
/// (`row_bytes = width.div_ceil(8)`) -- the format the CCITT codec
/// functions (`compression::{encode, decode_into}`) read and write
/// directly, bypassing the writer pipeline's own packing step. See
/// `compression/ccitt/mod.rs`'s own doctest: `rows = [0b1111_0000u8, ...]`
/// is 8 pixels per byte, not one byte per pixel.
fn bilevel_pattern_packed(width: usize, height: usize) -> Vec<u8> {
    let native = bilevel_pattern_native(width, height);
    let row_bytes = width.div_ceil(8);
    let mut packed = vec![0u8; row_bytes * height];
    for y in 0..height {
        for x in 0..width {
            if native[y * width + x] != 0 {
                packed[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    packed
}

/// One `ImageSpec` + native-endian pixel buffer, encoded to a complete
/// small TIFF -- already-valid seeds a raw-bytes decode fuzz target
/// benefits most from.
fn small_tiff(spec: ImageSpec, pixels: &[u8]) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder.write_image(&spec, pixels).expect("write");
    encoder.finish().expect("finish");
    buffer.into_inner()
}

fn tiff_decode_seeds() -> Vec<Seed> {
    let gray: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let mut seeds = named(vec![
        (
            "gray8_strips_none",
            small_tiff(
                ImageSpec::new(8, 8, ColorType::Gray(8))
                    .with_layout(Layout::Strips { rows_per_strip: 2 }),
                &gray,
            ),
        ),
        (
            "gray8_tiles_packbits",
            small_tiff(
                ImageSpec::new(8, 8, ColorType::Gray(8))
                    .with_compression(oxiarc_tiff::Compression::PackBits)
                    .with_layout(Layout::Tiles {
                        width: 16,
                        length: 16,
                    }),
                &gray,
            ),
        ),
        (
            "rgb8_big_endian",
            small_tiff(
                ImageSpec::new(4, 4, ColorType::Rgb(8))
                    .with_layout(Layout::Strips { rows_per_strip: 4 }),
                &(0..48u32).map(|i| i as u8).collect::<Vec<u8>>(),
            ),
        ),
        (
            "bilevel_g4",
            small_tiff(
                ImageSpec::new(32, 16, ColorType::Gray(1))
                    .with_compression(oxiarc_tiff::Compression::CcittGroup4)
                    .with_layout(Layout::Strips { rows_per_strip: 16 }),
                &bilevel_pattern_native(32, 16),
            ),
        ),
    ]);
    // Truncations and a bit flip: a decoder that reaches deep into the byte
    // stream before failing is the whole point of seeding truncated inputs.
    if let Some((_, full)) = seeds.first().cloned() {
        for cut in [full.len() / 2, full.len() - 1, 16] {
            if cut < full.len() {
                seeds.push((format!("truncated_{cut}"), full[..cut].to_vec()));
            }
        }
        let mut flipped = full.clone();
        if let Some(byte) = flipped.get_mut(full.len() / 2) {
            *byte ^= 0xFF;
        }
        seeds.push(("bit_flipped".to_string(), flipped));
    }
    seeds
}

fn tiff_roundtrip_seeds() -> Vec<Seed> {
    // Generic varied-length, varied-pattern buffers -- see the module docs:
    // this crate does not fix `ImageSpec`'s `Unstructured` parse order, so
    // there is no "already valid" seed to hand-build yet.
    named(vec![
        ("empty", vec![]),
        ("zeros_64", vec![0u8; 64]),
        ("ones_64", vec![0xFFu8; 64]),
        (
            "ramp_256",
            (0..256u32).map(|i| i as u8).collect::<Vec<u8>>(),
        ),
        (
            "alternating_128",
            (0..128u32)
                .map(|i| if i % 2 == 0 { 0xAA } else { 0x55 })
                .collect(),
        ),
    ])
}

fn ccitt_decode_seeds() -> Vec<Seed> {
    let width = 32u32;
    let height = 16u32;
    let pixels = bilevel_pattern_packed(width as usize, height as usize);
    let bits = [1u16];

    let mut seeds = Vec::new();
    for (flavour, method, t4, t6) in [
        (
            0u8,
            CompressionMethod::CcittRle,
            T4Options::default(),
            T6Options::default(),
        ),
        (
            1u8,
            CompressionMethod::CcittFax3,
            T4Options::default(),
            T6Options::default(),
        ),
        (
            2u8,
            CompressionMethod::CcittFax3,
            T4Options::from_u32(1), // bit 0: two-dimensional
            T6Options::default(),
        ),
        (
            3u8,
            CompressionMethod::CcittFax4,
            T4Options::default(),
            T6Options::default(),
        ),
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
        cx.t4_options = t4;
        cx.t6_options = t6;
        let Ok(coded) = encode(&pixels, &cx, CodecLevel::Default) else {
            continue;
        };

        // Header: flavour, width (u16 LE), t4/t6 raw bits, then the payload
        // -- the layout `ccitt_decode`'s fuzz target should parse (module
        // docs).
        let mut seed = vec![flavour];
        seed.extend_from_slice(&(width as u16).to_le_bytes());
        seed.push(t4.to_u32() as u8);
        seed.extend_from_slice(&coded);
        seeds.push((format!("flavour_{flavour}"), seed));
    }
    seeds
}

fn packbits_decode_seeds() -> Vec<Seed> {
    let bits = [8u16];
    let cx = CodecContext::new(CompressionMethod::PackBits, 32, 1, &bits, 1, Endian::Little);

    let literal_run: Vec<u8> = (0..32u32).map(|i| i as u8).collect();
    let repeat_run = vec![0x42u8; 32];
    let mixed: Vec<u8> = literal_run
        .iter()
        .take(16)
        .chain(repeat_run.iter().take(16))
        .copied()
        .collect();

    let mut seeds = Vec::new();
    for (name, pixels) in [
        ("literal_run", &literal_run),
        ("repeat_run", &repeat_run),
        ("mixed", &mixed),
    ] {
        let Ok(coded) = encode(pixels, &cx, CodecLevel::Default) else {
            continue;
        };
        let capacity = (pixels.len() as u32).to_le_bytes();
        let mut seed = capacity.to_vec();
        seed.extend_from_slice(&coded);
        seeds.push((name.to_string(), seed));
    }
    // The PackBits no-op byte (-128 / 0x80) and the two run-length
    // boundaries (127 and -127) never appear in a well-behaved encoder's
    // own output, but libtiff and real scanners emit them.
    seeds.push((
        "noop_byte".to_string(),
        [32u32.to_le_bytes().as_slice(), &[0x80, 0x00, 0x01]].concat(),
    ));
    seeds
}

fn lzw_tiff_decode_seeds() -> Vec<Seed> {
    let bits = [8u16];
    let cx = CodecContext::new(CompressionMethod::Lzw, 64, 1, &bits, 1, Endian::Little);
    let pixels: Vec<u8> = (0..64u32).map(|i| (i / 4) as u8).collect(); // runs, so LZW has matches to find
    let Ok(coded) = encode(&pixels, &cx, CodecLevel::Default) else {
        return Vec::new();
    };

    let mut standard = vec![0u8]; // low bit clear: standard code-width rule
    standard.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    standard.extend_from_slice(&coded);

    named(vec![("standard_early_change", standard)])
}

fn main() -> std::io::Result<()> {
    // Confirm every seed actually decodes before writing it out: a seed
    // that does not reach its intended codec is not "interesting", it is
    // noise.
    verify_seeds();

    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("oxiarc_tiff_fuzz_seeds"));

    let mut total = 0usize;
    for (target, seeds) in corpora() {
        let dir: &Path = &root.join(target);
        std::fs::create_dir_all(dir)?;
        for (name, bytes) in &seeds {
            std::fs::write(dir.join(name), bytes)?;
        }
        println!("{target:>20}: {:>3} seeds", seeds.len());
        total += seeds.len();
    }
    println!("\n{total} seeds written under {}", root.display());
    Ok(())
}

fn verify_seeds() {
    for (target, seeds) in corpora() {
        assert!(!seeds.is_empty(), "{target} produced no seeds at all");
    }
}

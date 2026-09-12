//! `use oxiarc_png as png;` compiles the exact call-site shapes catalogued
//! in `png-design.md` §1.2 -- the 24 real usages of the `png` crate found
//! across `fop`, `oxiui`, `oxigeo`, `oximedia`, and `oxiquant` in `~/work`
//! -- and running that pattern produces a correct result, not merely a
//! type-checked one.
//!
//! Each test is named after the file:line it reproduces and drives the
//! exact sequence of types/methods that call site uses (never a paraphrase
//! chosen for our own convenience), so a future change to, say, the
//! argument order of `next_frame` or the return type of `output_buffer_size`
//! fails *here*, at compile time, instead of silently breaking a downstream
//! crate's `Cargo.toml` swap to `png = { package = "oxiarc-png" }`.
//! Business logic specific to each caller (PDF emission, WGPU texture
//! upload, tile rendering, ...) is out of scope; only the `png`-surface
//! calls are reproduced, against synthetic pixel data built for the test.
//!
//! See also `png-design.md` §1.3 for the closed set of types/methods this
//! implies, checked for existence (not necessarily runtime behaviour) at
//! the bottom of this file.

use oxiarc_png as png;

use png::{BitDepth, ColorType, Compression, Decoder, Encoder, Transformations};
use std::io::{BufWriter, Cursor};

mod common;
use common::Rng;

fn rgb_pixels(width: u32, height: u32, seed: u64) -> Vec<u8> {
    Rng::new(seed).bytes(width as usize * height as usize * 3)
}

fn rgba_pixels(width: u32, height: u32, seed: u64) -> Vec<u8> {
    Rng::new(seed).bytes(width as usize * height as usize * 4)
}

fn gray_pixels(width: u32, height: u32, seed: u64) -> Vec<u8> {
    Rng::new(seed).bytes(width as usize * height as usize)
}

fn palette_256() -> Vec<u8> {
    (0..256u16)
        .flat_map(|i| [i as u8, i.wrapping_mul(7) as u8, i.wrapping_mul(13) as u8])
        .collect()
}

// ---------------------------------------------------------------------
// Decode-side call sites
// ---------------------------------------------------------------------

/// `fop/crates/fop-pdf-renderer/src/image.rs:89-141` -- decode an indexed
/// image with a palette and `tRNS`, normalizing to 8-bit color, then read
/// `Info::{palette,trns}`, `output_buffer_size()`, `next_frame`, and
/// `OutputInfo::{buffer_size,color_type,width,height}`.
#[test]
fn fop_pdf_renderer_image_rs_89_141() {
    let palette = palette_256();
    let trns = vec![0u8, 64, 255];
    let indices: Vec<u8> = (0..16u32).map(|i| (i % 256) as u8).collect();

    let mut raw = Vec::new();
    let mut encoder = Encoder::new(&mut raw, 4, 4);
    encoder.set_color(ColorType::Indexed);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_palette(palette.clone());
    encoder.set_trns(trns.clone());
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&indices).expect("write_image_data");
    writer.finish().expect("finish");

    let mut decoder = Decoder::new(Cursor::new(&raw));
    decoder.set_transformations(Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().expect("read_info");

    let palette_seen: Option<&[u8]> = reader.info().palette.as_deref();
    let trns_seen: Option<&[u8]> = reader.info().trns.as_deref();
    assert!(palette_seen.is_some(), "palette must survive read_info");
    assert!(trns_seen.is_some(), "tRNS must survive read_info");

    let size: Option<usize> = reader.output_buffer_size();
    let mut buf = vec![0u8; size.expect("size known before any pixel is read")];
    let output_info = reader.next_frame(&mut buf).expect("next_frame");
    assert_eq!(output_info.buffer_size(), buf.len());
    assert_eq!((output_info.width, output_info.height), (4, 4));
    // `normalize_to_color8` expands Indexed+tRNS to Rgba8.
    let _color_type: ColorType = output_info.color_type;
    let _err_path: fn(png::DecodingError) = |_| {};
}

/// `fop/crates/fop-render/src/ps/mod.rs:678-712` -- decode straight to the
/// stored colour type (no transformation), reading dimensions off
/// `Reader::info()` before calling `next_frame`.
#[test]
fn fop_render_ps_mod_rs_678_712() {
    let pixels = rgba_pixels(5, 3, 1);
    let mut raw = Vec::new();
    let mut encoder = Encoder::new(&mut raw, 5, 3);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");

    let decoder = Decoder::new(Cursor::new(&raw));
    let mut reader = decoder.read_info().expect("read_info");
    let (w, h) = (reader.info().width, reader.info().height);
    let color_type = reader.info().color_type;
    assert_eq!((w, h), (5, 3));
    assert!(matches!(
        color_type,
        ColorType::Rgb | ColorType::Rgba | ColorType::Grayscale | ColorType::GrayscaleAlpha
    ));

    let size = reader.output_buffer_size().expect("size known");
    let mut buf = vec![0u8; size];
    let output_info = reader.next_frame(&mut buf).expect("next_frame");
    assert_eq!(output_info.buffer_size(), buf.len());
}

/// `fop/crates/fop-render/src/image.rs:80-94` -- a metadata-only path: read
/// the header and never touch pixels at all.
#[test]
fn fop_render_image_rs_80_94_metadata_only() {
    let pixels = gray_pixels(6, 2, 2);
    let mut raw = Vec::new();
    let mut encoder = Encoder::new(&mut raw, 6, 2);
    encoder.set_color(ColorType::Grayscale);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");

    let decoder = Decoder::new(Cursor::new(&raw));
    let reader = decoder.read_info().expect("read_info");
    let info = reader.info();
    assert_eq!((info.width, info.height), (6, 2));
    let _color_type: ColorType = info.color_type;
    // No `next_frame` call -- `reader` is simply dropped.
}

/// `fop/crates/fop-render/src/pdf/image.rs:87-133` -- as above, plus a
/// `bit_depth` comparison and a `color_type == Rgba` check.
#[test]
fn fop_render_pdf_image_rs_87_133() {
    let pixels = rgba_pixels(3, 3, 3);
    let mut raw = Vec::new();
    let mut encoder = Encoder::new(&mut raw, 3, 3);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");

    let decoder = Decoder::new(Cursor::new(&raw));
    let reader = decoder.read_info().expect("read_info");
    let info = reader.info();
    assert!(info.bit_depth != BitDepth::One);
    assert_eq!(info.bit_depth, BitDepth::Eight);
    assert!(info.color_type == ColorType::Rgba);
}

/// `noffi/oxiui/crates/oxiui/src/icon.rs:29-56` and the two golden-image
/// test sites that share its shape (`oxiui-render-soft/tests/soft_tests.rs:93-164`,
/// `oxiui-render-wgpu/tests/golden_image_tests.rs:80-85`) -- decode from a
/// `Cursor`, assert `info.color_type`, then `output_buffer_size` +
/// `next_frame` + `frame.buffer_size()`.
#[test]
fn oxiui_icon_and_golden_image_decode_sites() {
    let pixels = rgba_pixels(8, 8, 4);
    let mut raw = Vec::new();
    let mut encoder = Encoder::new(&mut raw, 8, 8);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");

    let decoder = Decoder::new(Cursor::new(&raw));
    let mut reader = decoder.read_info().expect("read_info");
    assert_eq!(reader.info().color_type, ColorType::Rgba);

    let size = reader.output_buffer_size().expect("size known");
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf).expect("next_frame");
    assert_eq!(frame.buffer_size(), buf.len());
    assert_eq!(buf, pixels);
}

/// `oximedia/crates/oximedia-codec/src/image.rs:197-287` -- decode Indexed
/// and 16-bit sources, including the `chunks_exact(2)` big-endian sample
/// handling a 16-bit consumer needs.
#[test]
fn oximedia_codec_image_rs_197_287() {
    // Indexed leg.
    let indices: Vec<u8> = (0..(4 * 4u32)).map(|i| (i % 256) as u8).collect();
    let mut raw = Vec::new();
    let mut encoder = Encoder::new(&mut raw, 4, 4);
    encoder.set_color(ColorType::Indexed);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_palette(palette_256());
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&indices).expect("write_image_data");
    writer.finish().expect("finish");

    let decoder = Decoder::new(Cursor::new(&raw));
    let mut reader = decoder.read_info().expect("read_info");
    assert_eq!(reader.info().color_type, ColorType::Indexed);
    let size = reader.output_buffer_size().expect("size known");
    let mut buf = vec![0u8; size];
    reader.next_frame(&mut buf).expect("next_frame");
    assert_eq!(buf, indices);

    // 16-bit leg: two samples per pixel, big-endian.
    let samples16: Vec<u16> = (0..(3 * 3u32)).map(|i| (i as u16) * 4001).collect();
    let bytes16: Vec<u8> = samples16.iter().flat_map(|s| s.to_be_bytes()).collect();
    let mut raw16 = Vec::new();
    let mut encoder16 = Encoder::new(&mut raw16, 3, 3);
    encoder16.set_color(ColorType::Grayscale);
    encoder16.set_depth(BitDepth::Sixteen);
    let mut writer16 = encoder16.write_header().expect("write_header");
    writer16
        .write_image_data(&bytes16)
        .expect("write_image_data");
    writer16.finish().expect("finish");

    let decoder16 = Decoder::new(Cursor::new(&raw16));
    let mut reader16 = decoder16.read_info().expect("read_info");
    let size16 = reader16.output_buffer_size().expect("size known");
    let mut out16 = vec![0u8; size16];
    reader16.next_frame(&mut out16).expect("next_frame");
    let recovered: Vec<u16> = out16
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(recovered, samples16);
}

// ---------------------------------------------------------------------
// Encode-side call sites
// ---------------------------------------------------------------------

/// `fop/examples/phase5_complete_demo.rs:131-143`,
/// `fop/crates/fop-render/src/raster/mod.rs:135-148`,
/// `oxigeo/crates/oxigeo-server/src/handlers/{rendering.rs:604-613,tiles.rs:733-742}`,
/// `noffi/oxiui/crates/oxiui/src/icon.rs:108-178` (Rgba leg) --
/// `Encoder::new`/`set_color`/`set_depth`/`write_header`/`write_image_data`
/// into an owned `Vec<u8>`, Rgba8.
#[test]
fn encoder_new_set_color_write_image_data_rgba8_sites() {
    let pixels = rgba_pixels(4, 4, 5);
    let mut out: Vec<u8> = Vec::new();
    let mut encoder = Encoder::new(&mut out, 4, 4);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");
    assert!(png::is_png(&out));
}

/// `fop/crates/fop-render/src/ps/mod.rs:1410-1427` -- imports the encoder
/// trio through a single `use` statement, the shape that most directly
/// exercises `use oxiarc_png as png;` substitutability.
#[test]
fn fop_render_ps_mod_rs_1410_1427_grouped_use() {
    use png::{BitDepth as Bd, ColorType as Ct, Encoder as Enc};

    let pixels = gray_pixels(3, 5, 6);
    let mut out = Vec::new();
    let mut encoder = Enc::new(&mut out, 3, 5);
    encoder.set_color(Ct::Grayscale);
    encoder.set_depth(Bd::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");
    assert!(png::is_png(&out));
}

/// `fop/crates/fop-render/src/pdf/writer.rs:694-927` (three sites) and
/// `fop/crates/fop-render/src/pdf/image.rs:308-468` (four sites, Rgb /
/// Grayscale / Rgba) -- the same encode shape across every colour type
/// those files use.
#[test]
fn fop_render_pdf_writer_and_image_encode_sites() {
    for color_type in [ColorType::Rgb, ColorType::Grayscale, ColorType::Rgba] {
        let samples = match color_type {
            ColorType::Rgb => 3,
            ColorType::Grayscale => 1,
            ColorType::Rgba => 4,
            _ => unreachable!(),
        };
        let pixels = Rng::new(u64::from(color_type as u8) + 7).bytes(6 * 4 * samples);
        let mut out = Vec::new();
        let mut encoder = Encoder::new(&mut out, 6, 4);
        encoder.set_color(color_type);
        encoder.set_depth(BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write_header");
        writer.write_image_data(&pixels).expect("write_image_data");
        writer.finish().expect("finish");
        assert!(png::is_png(&out));
    }
}

/// `fop/crates/fop-pdf-renderer/src/rasterizer.rs:27-34` -- same encode
/// shape once more (Rgb).
#[test]
fn fop_pdf_renderer_rasterizer_rs_27_34() {
    let pixels = rgb_pixels(10, 6, 8);
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, 10, 6);
    encoder.set_color(ColorType::Rgb);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");
    assert!(png::is_png(&out));
}

/// `fop/crates/fop-pdf-renderer/src/image.rs:490-500` -- **the only
/// palette-writing call site in the whole survey**:
/// `Encoder::set_palette(vec![...])` + `ColorType::Indexed`.
#[test]
fn fop_pdf_renderer_image_rs_490_500_palette_write() {
    let palette = palette_256();
    let indices: Vec<u8> = (0..(5 * 5u32)).map(|i| (i % 256) as u8).collect();

    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, 5, 5);
    encoder.set_color(ColorType::Indexed);
    encoder.set_depth(BitDepth::Eight);
    encoder.set_palette(palette.clone());
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&indices).expect("write_image_data");
    writer.finish().expect("finish");

    let image = png::decode(&out).expect("decode");
    assert_eq!(image.info.palette.as_deref(), Some(palette.as_slice()));
    assert_eq!(image.data, indices);
}

/// `oxigeo/crates/oxigeo-examples/examples/wasm_integration_example.rs:375-382`
/// -- the Rgb leg of the same write pattern.
#[test]
fn oxigeo_examples_wasm_integration_example_375_382() {
    let pixels = rgb_pixels(16, 16, 9);
    let mut out = Vec::new();
    let mut encoder = Encoder::new(&mut out, 16, 16);
    encoder.set_color(ColorType::Rgb);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");
    assert!(png::is_png(&out));
}

/// `noffi/oxiui/crates/oxiui-render-soft/src/headless.rs:51-58` -- encoding
/// into a `BufWriter` rather than a bare `Vec`, proving the `W: Write` bound
/// is not accidentally narrower than `png` 0.18's.
#[test]
fn oxiui_render_soft_headless_rs_51_58_bufwriter() {
    let pixels = rgba_pixels(4, 2, 10);
    let file = std::io::Cursor::new(Vec::<u8>::new());
    let bufwriter = BufWriter::new(file);
    let mut encoder = Encoder::new(bufwriter, 4, 2);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");
}

/// `oximedia/crates/oximedia-codec/src/image.rs:528-562` -- `Encoder::new`
/// over a `Cursor<&mut Vec<u8>>`, `BitDepth::Sixteen`, an explicit
/// `set_compression(Compression::default())`, and the checked
/// `Writer::finish()` path (not `Drop`).
#[test]
fn oximedia_codec_image_rs_528_562() {
    let samples16: Vec<u16> = (0..(4 * 4u32)).map(|i| i as u16 * 257).collect();
    let bytes16: Vec<u8> = samples16.iter().flat_map(|s| s.to_be_bytes()).collect();

    let mut backing = Vec::new();
    let cursor = Cursor::new(&mut backing);
    let mut encoder = Encoder::new(cursor, 4, 4);
    encoder.set_color(ColorType::Grayscale);
    encoder.set_depth(BitDepth::Sixteen);
    encoder.set_compression(Compression::default());
    let mut writer = encoder.write_header().expect("write_header");
    writer.write_image_data(&bytes16).expect("write_image_data");
    writer.finish().expect("Writer::finish is the checked path");

    let image = png::decode(&backing).expect("decode");
    assert_eq!(image.bit_depth, BitDepth::Sixteen);
    assert_eq!(image.data, bytes16);
}

/// `oxiquant/crates/oxiquant-viz/src/error.rs:27` -- a doc-comment mention
/// of `png::EncodingError` only. The type must resolve under the alias even
/// though no value of it is ever constructed at that call site.
#[test]
fn oxiquant_viz_error_rs_27_type_reference_only() {
    fn _accepts_encoding_error(_e: png::EncodingError) {}
    // Reaching this line at all proves `png::EncodingError` resolved.
}

// ---------------------------------------------------------------------
// §1.3's closed set: every named type/method exists under the alias.

/// Every name in `png-design.md` §1.3's "exact compat surface required"
/// list, called for real (not merely coerced to a bare `fn` pointer, which
/// fights higher-ranked-lifetime inference for no benefit over an actual
/// call) so a rename or signature change fails to compile right here.
#[test]
fn closed_set_names_resolve() {
    let pixels = rgba_pixels(3, 2, 42);
    let mut out: Vec<u8> = Vec::new();
    let mut encoder: png::Encoder<'static, &mut Vec<u8>> = png::Encoder::new(&mut out, 3, 2);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::default());
    let mut writer: png::Writer<&mut Vec<u8>> = encoder.write_header().expect("write_header");
    writer.write_image_data(&pixels).expect("write_image_data");
    writer.finish().expect("finish");

    let decoder: png::Decoder<&[u8]> = png::Decoder::new(out.as_slice());
    let mut decoder = decoder;
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader: png::Reader<&[u8]> = decoder.read_info().expect("read_info");
    let _info: &png::Info<'static> = reader.info();
    let size: Option<usize> = reader.output_buffer_size();
    let mut buf = vec![0u8; size.expect("size known")];
    let output_info: png::OutputInfo = reader.next_frame(&mut buf).expect("next_frame");
    let _n: usize = output_info.buffer_size();
    assert!(png::is_png(&out));

    // Named in §1.3 as "not used anywhere in `~/work` (but still worth
    // providing for completeness / future work)" -- called here only to
    // prove the name resolves and the shape matches; not otherwise
    // exercised by this file (the encoder-side ones have their own
    // dedicated tests elsewhere in this crate).
    let mut out2: Vec<u8> = Vec::new();
    let mut encoder2 = png::Encoder::new(&mut out2, 3, 2);
    encoder2.set_color(png::ColorType::Rgba);
    encoder2.set_depth(png::BitDepth::Eight);
    let mut writer2 = encoder2.write_header().expect("write_header");
    {
        let mut sw: png::StreamWriter<'_, &mut Vec<u8>> =
            writer2.stream_writer().expect("stream_writer");
        std::io::Write::write_all(&mut sw, &pixels).expect("write_all");
        sw.finish().expect("stream finish");
    }
    writer2.finish().expect("finish");
    assert_eq!(out2, out, "stream_writer output matches write_image_data");

    let mut reader2 = png::Decoder::new(out2.as_slice())
        .read_info()
        .expect("read_info");
    let row: Option<png::Row<'_>> = reader2.next_row().expect("next_row");
    assert!(row.is_some());
    while reader2.next_row().expect("next_row").is_some() {}
}

/// The **0.17**-shaped call sites, behind the `compat-017` feature exactly
/// as `png-design.md` §11.6 specifies: `use oxiarc_png::v017 as png;`.
///
/// The design survey (§1.1/§1.2) found no *first-party* 0.17 call site in
/// `~/work` — 0.17 appears only transitively — so these reproduce the 0.17
/// API's own documented shapes rather than a surveyed file:line. What they
/// pin is the part that cannot be checked by reading: that the four items
/// whose shape changed between 0.17.16 and 0.18.1
/// (`Reader::output_buffer_size`, `FilterType`, `AdaptiveFilterType`, and
/// the `Decoder` bound) are usable together in one program, and that the
/// two filter knobs still control the bytes that come out.
#[cfg(feature = "compat-017")]
mod v017_shapes {
    use oxiarc_png::v017 as png;
    use std::io::Cursor;

    /// The canonical 0.17 decode loop, verbatim from that release's own
    /// `lib.rs` example: `output_buffer_size()` is a plain `usize`, so no
    /// `.unwrap_or(0)` appears anywhere.
    #[test]
    fn the_017_decode_loop_compiles_and_decodes() {
        let mut file = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut file, 4, 3);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write_header");
            writer
                .write_image_data(&[0xAB; 4 * 3 * 4])
                .expect("write_image_data");
            writer.finish().expect("finish");
        }

        let decoder = png::Decoder::new(Cursor::new(&file));
        let mut reader = decoder.read_info().expect("read_info");
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).expect("next_frame");
        let bytes = &buf[..info.buffer_size()];
        assert_eq!(bytes.len(), 4 * 3 * 4);
        assert!(bytes.iter().all(|b| *b == 0xAB));
        assert_eq!(reader.info().width, 4);
    }

    /// 0.17's split filter API: `set_filter` picks the fixed filter and
    /// `set_adaptive_filter` decides whether it is used at all. Both orders
    /// of the two calls must give the same result — the shim folds them into
    /// one value, so a stale fold would show up here.
    #[test]
    fn the_017_split_filter_api_controls_the_emitted_filter_bytes() {
        let filter_bytes = |ft: png::FilterType, ad: png::AdaptiveFilterType, swap: bool| {
            let mut file = Vec::new();
            let mut encoder = png::Encoder::new(&mut file, 5, 4);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            if swap {
                encoder.set_adaptive_filter(ad);
                encoder.set_filter(ft);
            } else {
                encoder.set_filter(ft);
                encoder.set_adaptive_filter(ad);
            }
            let mut writer = encoder.write_header().expect("write_header");
            let pixels: Vec<u8> = (0..20u8).map(|i| i.wrapping_mul(37)).collect();
            writer.write_image_data(&pixels).expect("write_image_data");
            writer.finish().expect("finish");

            let mut z = Vec::new();
            for item in png::chunk::ChunkIter::new(&file) {
                let (kind, data) = item.expect("chunk");
                if kind == png::chunk::IDAT {
                    z.extend_from_slice(data);
                }
            }
            let raw = oxiarc_deflate::zlib_decompress(&z).expect("inflate");
            raw.chunks_exact(6).map(|r| r[0]).collect::<Vec<u8>>()
        };

        for ft in [
            png::FilterType::NoFilter,
            png::FilterType::Sub,
            png::FilterType::Up,
            png::FilterType::Avg,
            png::FilterType::Paeth,
        ] {
            let fixed = filter_bytes(ft, png::AdaptiveFilterType::NonAdaptive, false);
            assert!(
                fixed.iter().all(|b| *b == ft as u8),
                "{ft:?}: NonAdaptive must use the fixed filter on every row, got {fixed:?}"
            );
            assert_eq!(
                fixed,
                filter_bytes(ft, png::AdaptiveFilterType::NonAdaptive, true),
                "{ft:?}: the order of set_filter/set_adaptive_filter must not matter"
            );
        }

        // Adaptive ignores the fixed choice: at least one row must differ
        // from a filter deliberately set to something the heuristic would
        // not pick for every row.
        let adaptive = filter_bytes(
            png::FilterType::NoFilter,
            png::AdaptiveFilterType::Adaptive,
            false,
        );
        assert!(
            adaptive.iter().any(|b| *b != 0),
            "Adaptive must not be pinned to the fixed FilterType, got {adaptive:?}"
        );
    }

    /// The 0.17 `Decoder` bound is `R: Read`, so a bare `&[u8]` works with
    /// no `BufReader`/`Cursor` wrapper — the difference from 0.18 that makes
    /// the root's bound "strictly wider" true rather than merely claimed.
    #[test]
    fn the_017_decoder_accepts_a_plain_read() {
        let mut file = Vec::new();
        let mut encoder = png::Encoder::new(&mut file, 2, 2);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write_header");
        writer.write_image_data(&[1, 2, 3, 4]).expect("data");
        writer.finish().expect("finish");

        let slice: &[u8] = &file;
        let mut reader = png::Decoder::new(slice).read_info().expect("read_info");
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).expect("next_frame");
        assert_eq!(buf, [1, 2, 3, 4]);
    }
}

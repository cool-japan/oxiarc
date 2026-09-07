//! Temporary probe: compare the TIFF-local JPEG encoder's tag 347 with
//! oxiarc-jpeg's `write_tables_only`, and with libtiff's own.
#![cfg(feature = "jpeg")]

use oxiarc_jpeg::{ColorSpace, EncodeOptions, Encoder, InputColor, Subsampling, TablesMode};
use oxiarc_tiff::compression::{CodecContext, CodecLevel, jpeg};
use oxiarc_tiff::tags::PhotometricInterpretation as P;
use oxiarc_tiff::{CompressionMethod, Endian};

fn cx<'a>(photometric: P, spp: u16, bits: &'a [u16], sub: (u16, u16)) -> CodecContext<'a> {
    let mut c = CodecContext::new(CompressionMethod::Jpeg, 16, 16, bits, spp, Endian::Little);
    c.photometric = photometric;
    c.ycbcr_subsampling = sub;
    c
}

fn theirs(color: InputColor, space: ColorSpace, sub: Subsampling, q: u8) -> Vec<u8> {
    let mut options = EncodeOptions::tiff_strip(q);
    options.jpeg_color_space = Some(space);
    options.subsampling = sub;
    options.force_baseline = true;
    let mut out = Vec::new();
    let mut encoder = Encoder::with_options(&mut out, options);
    encoder
        .write_tables_only(TablesMode::BOTH, color)
        .expect("tables");
    out
}

#[test]
fn dump() {
    for q in [1u8, 10, 25, 50, 75, 95, 100] {
        println!("--- quality {q} ---");
        let bits1 = [8u16];
        let bits3 = [8u16, 8, 8];
        let bits4 = [8u16, 8, 8, 8];
        let cases: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
            (
                "gray",
                jpeg::shared_tables(
                    &cx(P::BlackIsZero, 1, &bits1, (1, 1)),
                    CodecLevel::Level(q as i32),
                )
                .expect("gray"),
                theirs(InputColor::Luma, ColorSpace::Luma, Subsampling::S444, q),
            ),
            (
                "ycbcr_422",
                jpeg::shared_tables(
                    &cx(P::YCbCr, 3, &bits3, (2, 1)),
                    CodecLevel::Level(q as i32),
                )
                .expect("ycbcr"),
                theirs(
                    InputColor::Ycbcr,
                    ColorSpace::Ycbcr,
                    Subsampling::Custom([(2, 1), (1, 1), (1, 1), (1, 1)]),
                    q,
                ),
            ),
            (
                "rgb",
                jpeg::shared_tables(&cx(P::Rgb, 3, &bits3, (1, 1)), CodecLevel::Level(q as i32))
                    .expect("rgb"),
                theirs(InputColor::Rgb, ColorSpace::Rgb, Subsampling::S444, q),
            ),
            (
                "cmyk",
                jpeg::shared_tables(
                    &cx(P::Separated, 4, &bits4, (1, 1)),
                    CodecLevel::Level(q as i32),
                )
                .expect("cmyk"),
                theirs(InputColor::Cmyk, ColorSpace::Cmyk, Subsampling::S444, q),
            ),
        ];
        for (name, old, new) in cases {
            println!(
                "{name}: old {} bytes, new {} bytes, equal {}",
                old.len(),
                new.len(),
                old == new
            );
            if old != new {
                println!("  old: {}", hex(&old));
                println!("  new: {}", hex(&new));
            }
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(64)
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join("")
}

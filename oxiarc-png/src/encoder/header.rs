//! Header-chunk emission: everything written between the signature/`IHDR`
//! and the first `IDAT`.
//!
//! Ordering follows the table in the design report (critique §2, png-design
//! §8.6), cross-checked against the real `png` 0.18 crate's own
//! `Writer::encode_header` for the chunks both crates support (`pHYs`,
//! `sRGB`/`gAMA`/`cHRM` substitution, `iCCP`, `eXIf`, `acTL`, `PLTE`, `tRNS`,
//! text) — including its sRGB/gAMA/cHRM "write the fallback values only if
//! they were explicitly set to the sRGB substitutes" rule (PNG 3rd Edition
//! §11.3.2.5). The additional chunks this crate parses that `png` does not
//! (`sBIT`, `cICP`, `mDCv`, `cLLi`, `bKGD`, `hIST`, `sPLT`, `pHYs` already
//! covered, `tIME`, `oFFs`, `sCAL`, `pCAL`, `sTER`, retained unknown chunks)
//! are slotted into the same before-PLTE / after-PLTE / before-IDAT bands the
//! decoder's own ordering table (`decoder::stream::chunks`) enforces, so a
//! file this encoder writes always reads back through this same crate.

use std::io::Write;

use crate::ancillary::emit;
use crate::chunk::{self, write_chunk};
use crate::common::ScaledFloat;
use crate::error::EncodingError;
use crate::header::Ihdr;
use crate::info::Info;

/// The `gAMA` value PNG 3rd Edition §11.3.2.5 says an `sRGB` chunk implies.
fn srgb_substitute_gamma() -> ScaledFloat {
    ScaledFloat::from_scaled(45455)
}

/// Write the signature, `IHDR`, and every ancillary chunk that must precede
/// `IDAT`, in an order the decoder's own state machine accepts.
pub(crate) fn write_header<W: Write>(
    w: &mut W,
    ihdr: Ihdr,
    info: &Info<'_>,
    deflate_level: u8,
) -> Result<(), EncodingError> {
    w.write_all(&chunk::SIGNATURE)?;
    write_chunk(w, chunk::IHDR, &ihdr.to_bytes())?;

    // -- before PLTE and before IDAT --
    let wrote_srgb = if let Some(intent) = info.srgb {
        write_chunk(w, chunk::sRGB, &emit::srgb_payload(intent))?;
        // The spec's backward-compatibility idiom: emit the substitute gAMA
        // and cHRM only when the caller explicitly asked for exactly those
        // fallback values, never derived automatically from `srgb` alone.
        if info.source_gamma == Some(srgb_substitute_gamma()) {
            write_chunk(w, chunk::gAMA, &emit::gama_payload(srgb_substitute_gamma()))?;
        }
        if info.source_chromaticities == Some(crate::common::SourceChromaticities::from_srgb()) {
            write_chunk(
                w,
                chunk::cHRM,
                &emit::chrm_payload(crate::common::SourceChromaticities::from_srgb()),
            )?;
        }
        true
    } else {
        if let Some(gamma) = info.source_gamma {
            write_chunk(w, chunk::gAMA, &emit::gama_payload(gamma))?;
        }
        if let Some(chroma) = info.source_chromaticities {
            write_chunk(w, chunk::cHRM, &emit::chrm_payload(chroma))?;
        }
        false
    };
    // sRGB and iCCP are mutually exclusive (PNG 3rd Edition §11.3.3.5); an
    // sRGB image never also carries an ICC profile here.
    if !wrote_srgb {
        if let Some(profile) = &info.icc_profile {
            write_chunk(w, chunk::iCCP, &emit::iccp_payload(profile, deflate_level)?)?;
        }
    }
    if let Some(sbit) = &info.sbit {
        write_chunk(
            w,
            chunk::sBIT,
            &emit::sbit_payload(sbit, ihdr.color_type, ihdr.bit_depth)?,
        )?;
    }
    if let Some(cicp) = info.coding_independent_code_points {
        write_chunk(w, chunk::cICP, &emit::cicp_payload(cicp))?;
    }
    if let Some(mdcv) = info.mastering_display_color_volume {
        write_chunk(w, chunk::mDCV, &emit::mdcv_payload(mdcv)?)?;
    }
    if let Some(clli) = info.content_light_level {
        write_chunk(w, chunk::cLLI, &emit::clli_payload(clli))?;
    }

    if let Some(pd) = info.pixel_dims {
        write_chunk(w, chunk::pHYs, &emit::phys_payload(pd))?;
    }
    if let Some(exif) = &info.exif_metadata {
        write_chunk(w, chunk::eXIf, exif)?;
    }
    if let Some(t) = info.time {
        write_chunk(w, chunk::tIME, &emit::time_payload(t)?)?;
    }
    if let Some(offs) = info.offs {
        write_chunk(w, chunk::oFFs, &emit::offs_payload(offs))?;
    }
    if let Some(scal) = &info.scal {
        write_chunk(w, chunk::sCAL, &emit::scal_payload(scal)?)?;
    }
    if let Some(pcal) = &info.pcal {
        write_chunk(w, chunk::pCAL, &emit::pcal_payload(pcal)?)?;
    }
    if let Some(ster) = info.ster {
        write_chunk(w, chunk::sTER, &emit::ster_payload(ster))?;
    }
    if let Some(actl) = info.animation_control {
        let mut payload = [0u8; 8];
        payload[0..4].copy_from_slice(&actl.num_frames.to_be_bytes());
        payload[4..8].copy_from_slice(&actl.num_plays.to_be_bytes());
        write_chunk(w, chunk::acTL, &payload)?;
    }

    // -- PLTE --
    if let Some(palette) = &info.palette {
        write_chunk(
            w,
            chunk::PLTE,
            &emit::plte_payload(palette, ihdr.color_type)?,
        )?;
    }

    // -- after PLTE, before IDAT --
    if let Some(bkgd) = &info.bkgd {
        write_chunk(w, chunk::bKGD, &emit::bkgd_payload(bkgd, ihdr.color_type)?)?;
    }
    if let Some(hist) = &info.hist {
        let entries = info.palette_entries();
        write_chunk(w, chunk::hIST, &emit::hist_payload(hist, entries)?)?;
    }
    if let Some(trns) = &info.trns {
        write_chunk(
            w,
            chunk::tRNS,
            &emit::trns_payload(
                ihdr.color_type,
                ihdr.bit_depth,
                trns,
                info.trns_original.as_deref(),
            )?,
        )?;
    }
    for sp in &info.splt {
        write_chunk(w, chunk::sPLT, &emit::splt_payload(sp)?)?;
    }

    // -- text, may appear anywhere; grouped here for a stable byte layout --
    for t in &info.uncompressed_latin1_text {
        crate::text_metadata::EncodableTextChunk::encode(t, w)?;
    }
    for t in &info.compressed_latin1_text {
        crate::text_metadata::EncodableTextChunk::encode(t, w)?;
    }
    for t in &info.utf8_text {
        crate::text_metadata::EncodableTextChunk::encode(t, w)?;
    }

    // -- unknown ancillary chunks retained from a decode --
    for unknown in &info.unknown_chunks {
        write_chunk(w, unknown.kind, &unknown.data)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{BitDepth, ColorType, Interlace};

    #[test]
    fn minimal_header_is_signature_ihdr_only() {
        let ihdr = Ihdr {
            width: 2,
            height: 2,
            bit_depth: BitDepth::Eight,
            color_type: ColorType::Grayscale,
            interlace: Interlace::None,
        };
        let info = Info::from_ihdr(&ihdr);
        let mut out = Vec::new();
        write_header(&mut out, ihdr, &info, 6).expect("header");
        assert_eq!(&out[..8], &chunk::SIGNATURE);
        let chunks: Vec<_> = crate::chunk::ChunkIter::new(&out)
            .map(|c| c.expect("ok").0)
            .collect();
        assert_eq!(chunks, vec![chunk::IHDR]);
    }

    #[test]
    fn srgb_gates_the_fallback_gama_and_chrm() {
        let ihdr = Ihdr {
            width: 1,
            height: 1,
            bit_depth: BitDepth::Eight,
            color_type: ColorType::Rgb,
            interlace: Interlace::None,
        };
        let mut info = Info::from_ihdr(&ihdr);
        info.srgb = Some(crate::common::SrgbRenderingIntent::Perceptual);
        // No explicit gamma set: sRGB alone must not conjure a gAMA chunk.
        let mut out = Vec::new();
        write_header(&mut out, ihdr, &info, 6).expect("header");
        let kinds: Vec<_> = crate::chunk::ChunkIter::new(&out)
            .map(|c| c.expect("ok").0)
            .collect();
        assert!(kinds.contains(&chunk::sRGB));
        assert!(!kinds.contains(&chunk::gAMA));

        // Setting exactly the substitute value does write the fallback.
        info.source_gamma = Some(srgb_substitute_gamma());
        let mut out2 = Vec::new();
        write_header(&mut out2, ihdr, &info, 6).expect("header");
        let kinds2: Vec<_> = crate::chunk::ChunkIter::new(&out2)
            .map(|c| c.expect("ok").0)
            .collect();
        assert!(kinds2.contains(&chunk::gAMA));
    }

    #[test]
    fn srgb_and_iccp_never_both_appear() {
        let ihdr = Ihdr {
            width: 1,
            height: 1,
            bit_depth: BitDepth::Eight,
            color_type: ColorType::Rgb,
            interlace: Interlace::None,
        };
        let mut info = Info::from_ihdr(&ihdr);
        info.srgb = Some(crate::common::SrgbRenderingIntent::Perceptual);
        info.icc_profile = Some(std::borrow::Cow::Owned(vec![1, 2, 3]));
        let mut out = Vec::new();
        write_header(&mut out, ihdr, &info, 6).expect("header");
        let kinds: Vec<_> = crate::chunk::ChunkIter::new(&out)
            .map(|c| c.expect("ok").0)
            .collect();
        assert!(kinds.contains(&chunk::sRGB));
        assert!(!kinds.contains(&chunk::iCCP));
    }

    #[test]
    fn every_ancillary_chunk_this_crate_parses_can_be_emitted() {
        let ihdr = Ihdr {
            width: 2,
            height: 2,
            bit_depth: BitDepth::Eight,
            color_type: ColorType::Indexed,
            interlace: Interlace::None,
        };
        let mut info = Info::from_ihdr(&ihdr);
        info.palette = Some(std::borrow::Cow::Owned(vec![1, 2, 3, 4, 5, 6]));
        info.trns = Some(std::borrow::Cow::Owned(vec![0x80, 0xFF]));
        info.sbit = Some(std::borrow::Cow::Owned(vec![5, 5, 5]));
        info.hist = Some(vec![10, 20]);
        info.pixel_dims = Some(crate::common::PixelDimensions {
            xppu: 1,
            yppu: 1,
            unit: crate::common::Unit::Meter,
        });
        info.time = Some(crate::info::Time {
            year: 2026,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        });
        info.offs = Some(crate::info::ImageOffset {
            x: 1,
            y: 1,
            unit: crate::info::OffsetUnit::Pixel,
        });
        info.ster = Some(crate::common::StereoLayout::CrossFuse);
        info.coding_independent_code_points = Some(crate::common::CodingIndependentCodePoints {
            color_primaries: 1,
            transfer_function: 13,
            matrix_coefficients: 0,
            is_video_full_range_image: false,
        });
        info.uncompressed_latin1_text
            .push(crate::text_metadata::TEXtChunk::new("Title", "x"));
        let mut out = Vec::new();
        write_header(&mut out, ihdr, &info, 6).expect("header");
        let kinds: Vec<_> = crate::chunk::ChunkIter::new(&out)
            .map(|c| c.expect("ok").0)
            .collect();
        for want in [
            chunk::PLTE,
            chunk::tRNS,
            chunk::sBIT,
            chunk::hIST,
            chunk::pHYs,
            chunk::tIME,
            chunk::oFFs,
            chunk::sTER,
            chunk::cICP,
            chunk::tEXt,
        ] {
            assert!(kinds.contains(&want), "missing {want}");
        }
        // PLTE must precede tRNS/hIST/bKGD, per the ordering table.
        let plte_pos = kinds.iter().position(|k| *k == chunk::PLTE).expect("plte");
        let trns_pos = kinds.iter().position(|k| *k == chunk::tRNS).expect("trns");
        assert!(plte_pos < trns_pos);
    }
}

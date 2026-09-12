//! Marker segment emission.
//!
//! The order libjpeg writes, measured from `cjpeg` output rather than
//! recalled: `SOI`, `APP0`/`APP14`, any application markers, every `DQT` a
//! component references (in `Tq` order), the `SOF`, then per scan the `DHT`
//! segments that scan needs, a `DRI` when the interval changed, and the
//! `SOS`. `DHT` segments come out per table slot as `DC0 AC0 DC1 AC1`,
//! because the writer walks the scan's components and emits each one's DC
//! table immediately before its AC table.

use super::options::Density;
use super::plan::EncodePlan;
use crate::error::{JpegError, Result};
use crate::huffman::HuffmanTable;
use crate::quant::emit_dqt;

/// Longest payload a marker segment can carry after its two-byte length
/// field, which is libjpeg's `MAX_BYTES_IN_MARKER`.
pub(crate) const MAX_MARKER_PAYLOAD: usize = 65_533;

/// `ICC_PROFILE\0` plus the two sequencing bytes.
const ICC_OVERHEAD: usize = 14;
/// Largest slice of an ICC profile one `APP2` segment can hold.
pub(crate) const ICC_CHUNK: usize = MAX_MARKER_PAYLOAD - ICC_OVERHEAD;

/// `SOI`.
pub(crate) fn soi(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0xFF, 0xD8]);
}

/// `EOI`.
pub(crate) fn eoi(out: &mut Vec<u8>) {
    out.extend_from_slice(&[0xFF, 0xD9]);
}

/// A marker with a length-prefixed payload.
fn segment(out: &mut Vec<u8>, marker: u8, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_MARKER_PAYLOAD {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "marker",
            reason: "a marker segment payload exceeds 65 533 bytes",
        });
    }
    let length = payload.len() + 2;
    out.push(0xFF);
    out.push(marker);
    out.push((length >> 8) as u8);
    out.push((length & 0xFF) as u8);
    out.extend_from_slice(payload);
    Ok(())
}

/// The JFIF `APP0` segment, version 1.1 with no thumbnail.
pub(crate) fn jfif(out: &mut Vec<u8>, density: Density) {
    let mut payload = Vec::with_capacity(14);
    payload.extend_from_slice(b"JFIF\0");
    payload.extend_from_slice(&[1, 1]);
    payload.push(density.units);
    payload.extend_from_slice(&density.x.to_be_bytes());
    payload.extend_from_slice(&density.y.to_be_bytes());
    payload.extend_from_slice(&[0, 0]);
    let _ = segment(out, 0xE0, &payload);
}

/// The Adobe `APP14` segment carrying the colour transform code.
pub(crate) fn adobe(out: &mut Vec<u8>, transform: u8) {
    let mut payload = Vec::with_capacity(12);
    payload.extend_from_slice(b"Adobe");
    payload.extend_from_slice(&100u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.push(transform);
    let _ = segment(out, 0xEE, &payload);
}

/// An arbitrary `APPn` segment, `n` in `0..=15`.
pub(crate) fn app(out: &mut Vec<u8>, n: u8, data: &[u8]) -> Result<()> {
    if n > 15 {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "app_segment",
            reason: "APPn markers are numbered 0..=15",
        });
    }
    segment(out, 0xE0 | n, data)
}

/// A `COM` segment.
pub(crate) fn comment(out: &mut Vec<u8>, text: &[u8]) -> Result<()> {
    segment(out, 0xFE, text)
}

/// An `APP1` EXIF segment. `data` is the payload **without** the `Exif\0\0`
/// prefix, matching what [`crate::Decoder::exif`] returns.
pub(crate) fn exif(out: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    let mut payload = Vec::with_capacity(data.len() + 6);
    payload.extend_from_slice(b"Exif\0\0");
    payload.extend_from_slice(data);
    segment(out, 0xE1, &payload)
}

/// An `APP1` XMP packet. `data` is the packet **without** its namespace URI
/// prefix, matching what [`crate::Decoder::xmp`] returns.
pub(crate) fn xmp(out: &mut Vec<u8>, data: &[u8]) -> Result<()> {
    let mut payload = Vec::with_capacity(data.len() + 29);
    payload.extend_from_slice(b"http://ns.adobe.com/xap/1.0/\0");
    payload.extend_from_slice(data);
    segment(out, 0xE1, &payload)
}

/// An ICC profile, split across as many `APP2` segments as it needs.
pub(crate) fn icc_profile(out: &mut Vec<u8>, profile: &[u8]) -> Result<()> {
    if profile.is_empty() {
        return Ok(());
    }
    let chunks = profile.len().div_ceil(ICC_CHUNK);
    if chunks > 255 {
        return Err(JpegError::InvalidEncodeParameter {
            parameter: "icc_profile",
            reason: "an ICC profile may not exceed 255 APP2 chunks",
        });
    }
    for (index, chunk) in profile.chunks(ICC_CHUNK).enumerate() {
        let mut payload = Vec::with_capacity(chunk.len() + ICC_OVERHEAD);
        payload.extend_from_slice(b"ICC_PROFILE\0");
        payload.push((index + 1) as u8);
        payload.push(chunks as u8);
        payload.extend_from_slice(chunk);
        segment(out, 0xE2, &payload)?;
    }
    Ok(())
}

/// Every quantisation table the frame's components reference.
pub(crate) fn dqt(out: &mut Vec<u8>, plan: &EncodePlan) {
    emit_dqt(&plan.quant, out);
}

/// One `DHT` segment.
pub(crate) fn dht(out: &mut Vec<u8>, class: u8, slot: u8, table: &HuffmanTable) -> Result<()> {
    let mut payload = Vec::with_capacity(17 + table.values().len());
    payload.push((class << 4) | slot);
    payload.extend_from_slice(table.bits());
    payload.extend_from_slice(table.values());
    segment(out, 0xC4, &payload)
}

/// A `DRI` segment.
/// Write the `DAC` segment for one arithmetic scan (T.81 B.2.4.3).
///
/// libjpeg's `emit_dac` writes an entry for every table the scan actually
/// uses — including the T.81 defaults, which are therefore never left
/// implicit — in table-slot order with each slot's DC entry before its AC
/// entry, and writes nothing at all when the scan uses neither (a DC
/// refinement scan). `cjpeg -arithmetic` output is byte-identical to this.
#[cfg(feature = "arithmetic")]
pub(crate) fn dac(
    out: &mut Vec<u8>,
    plan: &EncodePlan,
    components: &[usize],
    code_dc: bool,
    code_ac: bool,
) {
    let mut dc_in_use = [false; 4];
    let mut ac_in_use = [false; 4];
    for &index in components {
        let component = &plan.components[index];
        if code_dc {
            dc_in_use[usize::from(component.dc_slot) & 3] = true;
        }
        if code_ac {
            ac_in_use[usize::from(component.ac_slot) & 3] = true;
        }
    }
    let entries = dc_in_use
        .iter()
        .chain(ac_in_use.iter())
        .filter(|&&f| f)
        .count();
    if entries == 0 {
        return;
    }
    let mut payload = Vec::with_capacity(2 * entries);
    for slot in 0..4usize {
        if dc_in_use[slot] {
            payload.push(slot as u8);
            payload.push(plan.arithmetic.dc[slot]);
        }
        if ac_in_use[slot] {
            payload.push(0x10 | slot as u8);
            payload.push(plan.arithmetic.ac[slot]);
        }
    }
    // The payload is at most sixteen bytes, so the length can never overflow.
    let length = payload.len() + 2;
    out.push(0xFF);
    out.push(0xCC);
    out.push((length >> 8) as u8);
    out.push((length & 0xFF) as u8);
    out.extend_from_slice(&payload);
}

pub(crate) fn dri(out: &mut Vec<u8>, interval: u16) {
    out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
    out.extend_from_slice(&interval.to_be_bytes());
}

/// The frame header.
pub(crate) fn sof(out: &mut Vec<u8>, plan: &EncodePlan) -> Result<()> {
    let mut payload = Vec::with_capacity(6 + 3 * plan.components.len());
    payload.push(plan.precision);
    payload.extend_from_slice(&plan.height.to_be_bytes());
    payload.extend_from_slice(&plan.width.to_be_bytes());
    payload.push(plan.components.len() as u8);
    for component in &plan.components {
        payload.push(component.id);
        payload.push((component.h << 4) | component.v);
        payload.push(component.quant_slot);
    }
    segment(out, plan.sof_marker(), &payload)
}

/// A scan header.
///
/// For a lossless frame `spectral_start` carries the predictor `Psv` and
/// `approx_low` carries the point transform `Pt`; `spectral_end` and
/// `approx_high` are zero. That is T.81 B.2.3's re-use of the same fields.
///
/// The table selectors are not simply the component's own slots. A
/// progressive scan carries either DC or AC coefficients but never both, and
/// libjpeg writes **zero** in the unused field — `Ta` in a DC scan, `Td` in
/// an AC scan, and both in a DC refinement, which needs no table at all.
/// `jcmarker.c` notes that this follows Pennebaker and Mitchell rather than
/// the standard, and a decoder that validated the unused field against a
/// table that was never sent would reject the file otherwise. A lossless
/// scan has no AC tables either, so `Ta` is zero there too.
pub(crate) fn sos(
    out: &mut Vec<u8>,
    plan: &EncodePlan,
    components: &[usize],
    spectral_start: u8,
    spectral_end: u8,
    approx_high: u8,
    approx_low: u8,
) -> Result<()> {
    let mut payload = Vec::with_capacity(4 + 2 * components.len());
    payload.push(components.len() as u8);
    let progressive = matches!(plan.process, crate::EncodeProcess::Progressive);
    let lossless = plan.is_lossless();
    let dc_band = spectral_start == 0;
    for &index in components {
        let component = &plan.components[index];
        let (mut td, mut ta) = (component.dc_slot, component.ac_slot);
        if progressive {
            if dc_band {
                ta = 0;
                if approx_high != 0 {
                    td = 0;
                }
            } else {
                td = 0;
            }
        } else if lossless {
            ta = 0;
        }
        payload.push(component.id);
        payload.push((td << 4) | ta);
    }
    payload.push(spectral_start);
    payload.push(spectral_end);
    payload.push((approx_high << 4) | approx_low);
    segment(out, 0xDA, &payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::options::{EncodeOptions, InputColor};
    use crate::encoder::plan::build_plan;

    fn plan() -> EncodePlan {
        build_plan(&EncodeOptions::default(), 17, 19, InputColor::Rgb).expect("plan")
    }

    #[test]
    fn jfif_matches_the_bytes_cjpeg_writes() {
        let mut out = Vec::new();
        jfif(&mut out, Density::default());
        assert_eq!(
            out,
            vec![
                0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0
            ]
        );
    }

    #[test]
    fn adobe_matches_the_bytes_cjpeg_writes() {
        let mut out = Vec::new();
        adobe(&mut out, 2);
        assert_eq!(
            out,
            vec![
                0xFF, 0xEE, 0x00, 0x0E, b'A', b'd', b'o', b'b', b'e', 0, 100, 0, 0, 0, 0, 2
            ]
        );
    }

    #[test]
    fn sof_carries_the_component_table() {
        let plan = plan();
        let mut out = Vec::new();
        sof(&mut out, &plan).expect("sof");
        assert_eq!(&out[..4], &[0xFF, 0xC0, 0x00, 0x11]);
        assert_eq!(out[4], 8, "precision");
        assert_eq!(&out[5..9], &[0x00, 19, 0x00, 17], "height then width");
        assert_eq!(out[9], 3);
        assert_eq!(&out[10..19], &[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    }

    #[test]
    fn sos_carries_the_table_selectors() {
        let plan = plan();
        let mut out = Vec::new();
        sos(&mut out, &plan, &[0, 1, 2], 0, 63, 0, 0).expect("sos");
        assert_eq!(&out[..4], &[0xFF, 0xDA, 0x00, 0x0C]);
        assert_eq!(out[4], 3);
        assert_eq!(&out[5..11], &[1, 0x00, 2, 0x11, 3, 0x11]);
        assert_eq!(&out[11..14], &[0, 63, 0]);
    }

    #[test]
    fn dri_is_four_bytes_of_payload() {
        let mut out = Vec::new();
        dri(&mut out, 2);
        assert_eq!(out, vec![0xFF, 0xDD, 0x00, 0x04, 0x00, 0x02]);
    }

    #[test]
    fn icc_profiles_are_chunked_and_numbered() {
        let mut out = Vec::new();
        let profile = vec![0x5Au8; ICC_CHUNK + 10];
        icc_profile(&mut out, &profile).expect("icc");
        assert_eq!(&out[..2], &[0xFF, 0xE2]);
        assert_eq!(&out[4..16], b"ICC_PROFILE\0");
        assert_eq!(out[16], 1, "chunk number");
        assert_eq!(out[17], 2, "chunk count");
        let first_len = usize::from(u16::from_be_bytes([out[2], out[3]]));
        let second = &out[2 + first_len..];
        assert_eq!(&second[..2], &[0xFF, 0xE2]);
        assert_eq!(second[16], 2);
        assert_eq!(second[17], 2);
        assert_eq!(
            usize::from(u16::from_be_bytes([second[2], second[3]])),
            12 + 2 + 10 + 2
        );
    }

    #[test]
    fn exif_and_xmp_get_their_prefixes_back() {
        let mut out = Vec::new();
        exif(&mut out, b"II*\0").expect("exif");
        assert_eq!(&out[4..10], b"Exif\0\0");
        assert_eq!(&out[10..], b"II*\0");

        let mut out = Vec::new();
        xmp(&mut out, b"<x/>").expect("xmp");
        assert_eq!(&out[4..33], b"http://ns.adobe.com/xap/1.0/\0");
    }

    #[test]
    fn oversized_segments_are_rejected() {
        let mut out = Vec::new();
        assert!(app(&mut out, 3, &vec![0u8; MAX_MARKER_PAYLOAD + 1]).is_err());
        assert!(app(&mut out, 3, &vec![0u8; MAX_MARKER_PAYLOAD]).is_ok());
        assert!(app(&mut out, 16, b"x").is_err());
        assert!(comment(&mut out, b"hello").is_ok());
    }

    #[test]
    fn an_empty_icc_profile_writes_nothing() {
        let mut out = Vec::new();
        icc_profile(&mut out, &[]).expect("icc");
        assert!(out.is_empty());
    }

    #[test]
    fn dht_is_class_slot_bits_then_values() {
        let table = HuffmanTable::new(crate::tables::ANNEX_K_DC_LUMA_BITS, (0..12u8).collect())
            .expect("valid");
        let mut out = Vec::new();
        dht(&mut out, 0, 1, &table).expect("dht");
        assert_eq!(&out[..5], &[0xFF, 0xC4, 0x00, 0x1F, 0x01]);
        assert_eq!(&out[5..21], &crate::tables::ANNEX_K_DC_LUMA_BITS);
        assert_eq!(out[21], 0);
    }
}

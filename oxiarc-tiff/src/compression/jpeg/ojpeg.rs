//! Compression 6: old-style JPEG (TIFF 6.0 §22), read-only.
//!
//! The pre-TTN2 scheme described the JPEG data through tags 512-521 instead of
//! putting a self-contained datastream in every strip. It was withdrawn
//! because writers disagreed about what those tags meant, and libtiff carries
//! a 2 500-line codec to cope. Three shapes cover what is actually in the
//! wild, and this module handles all three:
//!
//! * **(a) self-contained** — the chunk itself begins with `SOI` and ends with
//!   `EOI`. This is by far the most common flavour and needs no
//!   reconstruction at all;
//! * **(b) shared prefix** — `JPEGInterchangeFormat` (513) points at a stream
//!   holding the tables (and often the frame header), and each chunk holds the
//!   scan. The two are spliced: the prefix without its `EOI`, then the chunk
//!   without its `SOI`. When the prefix already contains a scan *and* the
//!   image has one chunk, the prefix is the whole image and is decoded alone;
//! * **(c) tag-driven** — no interchange stream at all: the quantisation
//!   (519) and Huffman (520/521) tables are read from the file offsets those
//!   tags carry, and a baseline frame is synthesised from `ImageWidth`,
//!   `ImageLength`, `SamplesPerPixel` and `YCbCrSubSampling` before the chunk
//!   is appended as the scan.
//!
//! Compression 6 is never *written*: [`crate::writer::Compression`] has no
//! variant for it, and TTN2 replaced it thirty years ago.

use super::scan::{self, FrameFacts};
use super::{CodecContext, decode_stream};
use crate::compression::OldJpegParams;
use crate::error::{Result, TiffError, UnsupportedError};
use crate::tags::PhotometricInterpretation;

/// Decodes one old-style JPEG chunk.
///
/// # Errors
/// [`UnsupportedError::OldJpeg`] naming the tag that made the chunk
/// undecodable, plus everything the JPEG decoder can report.
pub(super) fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let facts = scan::scan(src);
    if facts.has_soi && facts.has_frame && facts.has_scan {
        // (a) the chunk is already a complete datastream.
        return decode_stream(src, dst, cx, &facts, None);
    }
    let params = cx
        .old_jpeg
        .ok_or(TiffError::Unsupported(UnsupportedError::OldJpeg(
            "no JPEGProc (512) or table tags are present",
        )))?;
    if let Some(prefix) = params.interchange.as_deref() {
        let prefix_facts = scan::scan(prefix);
        if prefix_facts.has_frame && prefix_facts.has_scan && !facts.has_frame {
            // The interchange stream is the whole image.
            return decode_stream(prefix, dst, cx, &prefix_facts, None);
        }
        if prefix_facts.has_soi {
            // (b) splice the prefix and the chunk.
            let merged = splice(prefix, src);
            let merged_facts = scan::scan(&merged);
            return decode_stream(&merged, dst, cx, &merged_facts, None);
        }
    }
    // (c) synthesise a frame from the tags.
    let stream = synthesise(src, cx, params, &facts)?;
    let stream_facts = scan::scan(&stream);
    decode_stream(&stream, dst, cx, &stream_facts, None)
}

/// Joins a tables/frame prefix and a scan chunk into one datastream.
fn splice(prefix: &[u8], chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + chunk.len() + 2);
    let head = if prefix.ends_with(&[0xFF, 0xD9]) {
        prefix.get(..prefix.len() - 2).unwrap_or(prefix)
    } else {
        prefix
    };
    out.extend_from_slice(head);
    let tail = if chunk.starts_with(&[0xFF, 0xD8]) {
        chunk.get(2..).unwrap_or(&[])
    } else {
        chunk
    };
    out.extend_from_slice(tail);
    if !out.ends_with(&[0xFF, 0xD9]) {
        out.extend_from_slice(&[0xFF, 0xD9]);
    }
    out
}

/// Builds a baseline datastream out of the 512-521 tags and the chunk.
fn synthesise(
    chunk: &[u8],
    cx: &CodecContext<'_>,
    params: &OldJpegParams,
    facts: &FrameFacts,
) -> Result<Vec<u8>> {
    if params.proc != 1 && params.proc != 0 {
        return Err(TiffError::Unsupported(UnsupportedError::OldJpeg(
            "JPEGProc (512) selects a process with no reconstructable frame header",
        )));
    }
    if params.q_tables.iter().all(Vec::is_empty) {
        return Err(TiffError::Unsupported(UnsupportedError::OldJpeg(
            "JPEGQTables (519) resolves to no quantisation table",
        )));
    }
    let width = u16::try_from(cx.width).map_err(|_| TiffError::IntOverflow)?;
    let height = u16::try_from(cx.height).map_err(|_| TiffError::IntOverflow)?;
    let count = usize::from(cx.samples_per_pixel).clamp(1, 4);
    let (h, v) = if cx.photometric == PhotometricInterpretation::YCbCr && count == 3 {
        (
            u8::try_from(cx.ycbcr_subsampling.0.max(1)).unwrap_or(1),
            u8::try_from(cx.ycbcr_subsampling.1.max(1)).unwrap_or(1),
        )
    } else {
        (1, 1)
    };

    let mut out = Vec::with_capacity(chunk.len() + 1024);
    out.extend_from_slice(&[0xFF, 0xD8]);
    for (slot, table) in params.q_tables.iter().enumerate().take(4) {
        if table.len() < 64 {
            continue;
        }
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43]);
        out.push(slot as u8);
        out.extend_from_slice(table.get(..64).unwrap_or(&[]));
    }
    emit_huffman(&mut out, &params.dc_tables, 0x00);
    emit_huffman(&mut out, &params.ac_tables, 0x10);
    if params.restart_interval > 0 {
        out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04]);
        out.extend_from_slice(&params.restart_interval.to_be_bytes());
    }
    if !facts.has_frame {
        let length = 8 + 3 * count as u16;
        out.extend_from_slice(&[0xFF, 0xC0]);
        out.extend_from_slice(&length.to_be_bytes());
        out.push(8);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.push(count as u8);
        for index in 0..count {
            out.push((index + 1) as u8);
            if index == 0 {
                out.push((h << 4) | (v & 0x0F));
            } else {
                out.push(0x11);
            }
            out.push(u8::from(index > 0).min(quant_slots(params).saturating_sub(1) as u8));
        }
    }
    if !facts.has_scan {
        let length = 6 + 2 * count as u16;
        out.extend_from_slice(&[0xFF, 0xDA]);
        out.extend_from_slice(&length.to_be_bytes());
        out.push(count as u8);
        for index in 0..count {
            out.push((index + 1) as u8);
            let selector =
                u8::from(index > 0).min(table_slots(&params.dc_tables).saturating_sub(1) as u8);
            out.push((selector << 4) | selector);
        }
        out.extend_from_slice(&[0x00, 0x3F, 0x00]);
    }
    let body = if chunk.starts_with(&[0xFF, 0xD8]) {
        chunk.get(2..).unwrap_or(&[])
    } else {
        chunk
    };
    out.extend_from_slice(body);
    if !out.ends_with(&[0xFF, 0xD9]) {
        out.extend_from_slice(&[0xFF, 0xD9]);
    }
    Ok(out)
}

/// How many quantisation tables the tags actually resolved.
fn quant_slots(params: &OldJpegParams) -> usize {
    params
        .q_tables
        .iter()
        .filter(|t| t.len() >= 64)
        .count()
        .max(1)
}

/// How many Huffman tables a tag resolved.
fn table_slots(tables: &[Vec<u8>]) -> usize {
    tables.iter().filter(|t| t.len() >= 17).count().max(1)
}

/// Emits one `DHT` segment per resolved table.
///
/// `class` is `0x00` for DC tables and `0x10` for AC tables; the slot number is
/// the table's index in the tag, which is how TIFF 6.0 §22 addresses them.
fn emit_huffman(out: &mut Vec<u8>, tables: &[Vec<u8>], class: u8) {
    for (slot, table) in tables.iter().enumerate().take(4) {
        if table.len() < 17 {
            continue;
        }
        let counts: usize = table
            .get(..16)
            .map(|bits| bits.iter().map(|count| usize::from(*count)).sum())
            .unwrap_or(0);
        let body = table.get(..16 + counts).unwrap_or(table);
        let length = (3 + body.len()) as u16;
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&length.to_be_bytes());
        out.push(class | slot as u8);
        out.extend_from_slice(body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::compression::{CodecLevel, CodecState};
    use crate::tags::CompressionMethod;

    fn context<'a>(bits: &'a [u16], params: Option<&'a OldJpegParams>) -> CodecContext<'a> {
        let mut cx = CodecContext::new(CompressionMethod::OldJpeg, 32, 16, bits, 1, Endian::Little);
        cx.photometric = PhotometricInterpretation::BlackIsZero;
        cx.old_jpeg = params;
        cx
    }

    /// A complete baseline stream, produced by this crate's own encoder.
    fn complete_stream(width: usize, height: usize) -> (Vec<u8>, Vec<u8>) {
        let bits = [8u16];
        let mut cx = CodecContext::new(
            CompressionMethod::Jpeg,
            width,
            height,
            &bits,
            1,
            Endian::Little,
        );
        cx.photometric = PhotometricInterpretation::BlackIsZero;
        let pixels: Vec<u8> = (0..width * height).map(|i| ((i * 5) % 251) as u8).collect();
        let chunk = super::super::encode(&pixels, &cx, CodecLevel::Level(95)).expect("encode");
        (chunk, pixels)
    }

    /// The `JPEGTables`-shaped blob for the same image [`complete_stream`]
    /// builds, at the same quality.
    fn tables_blob(width: usize, height: usize) -> Vec<u8> {
        let bits = [8u16];
        let mut cx = CodecContext::new(
            CompressionMethod::Jpeg,
            width,
            height,
            &bits,
            1,
            Endian::Little,
        );
        cx.photometric = PhotometricInterpretation::BlackIsZero;
        super::super::shared_tables(&cx, CodecLevel::Level(95)).expect("tables")
    }

    #[test]
    fn flavour_a_self_contained_chunks_decode() {
        let (stream, pixels) = complete_stream(32, 16);
        let bits = [8u16];
        let cx = context(&bits, None);
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&stream, &mut out, &cx).expect("self-contained"),
            pixels.len()
        );
        for (got, want) in out.iter().zip(pixels.iter()) {
            assert!(got.abs_diff(*want) <= 16);
        }
    }

    #[test]
    fn flavour_b_splices_the_interchange_prefix() {
        let (stream, pixels) = complete_stream(32, 16);
        // Split the stream after the frame header: everything up to `SOS`
        // becomes the interchange prefix, the scan becomes the chunk.
        let split = stream
            .windows(2)
            .position(|pair| pair == [0xFF, 0xDA])
            .expect("SOS");
        let mut prefix = stream.get(..split).expect("prefix").to_vec();
        prefix.extend_from_slice(&[0xFF, 0xD9]);
        let chunk = stream.get(split..).expect("scan").to_vec();
        let params = OldJpegParams {
            proc: 1,
            interchange: Some(prefix),
            ..OldJpegParams::default()
        };
        let bits = [8u16];
        let cx = context(&bits, Some(&params));
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&chunk, &mut out, &cx).expect("spliced"),
            pixels.len()
        );
        for (got, want) in out.iter().zip(pixels.iter()) {
            assert!(got.abs_diff(*want) <= 16);
        }
    }

    #[test]
    fn flavour_b_decodes_a_whole_image_interchange_stream() {
        let (stream, pixels) = complete_stream(32, 16);
        let params = OldJpegParams {
            proc: 1,
            interchange: Some(stream),
            ..OldJpegParams::default()
        };
        let bits = [8u16];
        let cx = context(&bits, Some(&params));
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&[], &mut out, &cx).expect("whole image"),
            pixels.len()
        );
    }

    #[test]
    fn flavour_c_synthesises_a_frame_from_the_tags() {
        let (stream, pixels) = complete_stream(32, 16);
        // Take the tables out of the stream and hand them over as tags, the
        // way a 1992 writer would have. They are read from an abbreviated
        // tables-only blob for the same context rather than from `stream`
        // itself: the encoder writes libjpeg's marker order (`DQT SOF DHT
        // SOS`), and `TableSet::parse` stops at the frame header.
        let tables = oxiarc_jpeg::TableSet::parse(&tables_blob(32, 16)).expect("tables");
        let quant = tables.quant[0].expect("quant");
        let mut q_bytes = Vec::with_capacity(64);
        for index in 0..64 {
            q_bytes.push(quant.zigzag()[index] as u8);
        }
        let dc = tables.dc_huffman[0].as_ref().expect("dc");
        let ac = tables.ac_huffman[0].as_ref().expect("ac");
        let mut dc_bytes = dc.bits().to_vec();
        dc_bytes.extend_from_slice(dc.values());
        let mut ac_bytes = ac.bits().to_vec();
        ac_bytes.extend_from_slice(ac.values());
        let params = OldJpegParams {
            proc: 1,
            q_tables: vec![q_bytes],
            dc_tables: vec![dc_bytes],
            ac_tables: vec![ac_bytes],
            ..OldJpegParams::default()
        };
        // The chunk is the entropy-coded scan alone.
        let sos = stream
            .windows(2)
            .position(|pair| pair == [0xFF, 0xDA])
            .expect("SOS");
        let scan_length = usize::from(u16::from_be_bytes([stream[sos + 2], stream[sos + 3]]));
        let chunk = stream
            .get(sos + 2 + scan_length..)
            .expect("entropy")
            .to_vec();

        let bits = [8u16];
        let cx = context(&bits, Some(&params));
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&chunk, &mut out, &cx).expect("synthesised"),
            pixels.len()
        );
        for (got, want) in out.iter().zip(pixels.iter()) {
            assert!(got.abs_diff(*want) <= 16, "{got} vs {want}");
        }
    }

    #[test]
    fn missing_tags_are_named_not_guessed() {
        let bits = [8u16];
        let cx = context(&bits, None);
        let mut out = vec![0u8; 512];
        let err = decode_into(&[0u8; 16], &mut out, &cx).expect_err("no tags");
        assert!(err.to_string().contains("512"), "{err}");

        let params = OldJpegParams {
            proc: 14,
            ..OldJpegParams::default()
        };
        let cx = context(&bits, Some(&params));
        let err = decode_into(&[0u8; 16], &mut out, &cx).expect_err("lossless");
        assert!(err.to_string().contains("JPEGProc"), "{err}");

        let params = OldJpegParams {
            proc: 1,
            ..OldJpegParams::default()
        };
        let cx = context(&bits, Some(&params));
        let err = decode_into(&[0u8; 16], &mut out, &cx).expect_err("no tables");
        assert!(err.to_string().contains("519"), "{err}");
    }

    #[test]
    fn the_state_field_is_untouched_by_this_codec() {
        let state = CodecState::new();
        let bits = [8u16];
        let mut cx = context(&bits, None);
        cx.state = Some(&state);
        let mut out = vec![0u8; 512];
        let _ = decode_into(&[0u8; 4], &mut out, &cx);
        assert!(!state.lzw_is_old_style());
    }
}

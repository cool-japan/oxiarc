//! CLI-layer image-format recognition for `detect`/`info`: a fallback
//! consulted only when [`oxiarc_archive::ArchiveFormat`] reports `Unknown`,
//! never a new `ArchiveFormat` variant. Adding `Png`/`Jpeg`/`Tiff` variants
//! there would touch six exhaustive `match`es across `oxiarc-archive` and
//! `oxiarc-cli` for formats that are neither archives nor compression
//! streams — every one of `list`/`extract`/`test`/`convert`/`add` would need
//! a new "recognised, but not an archive" arm just to keep compiling
//! correctly. See `repo-conventions.md` §13's cost table; this module is the
//! recommended alternative it describes.
//!
//! An image is never routed through [`oxiarc_archive::ArchiveFormat`] at
//! all: [`sniff`] is consulted directly by `detect`/`info`, and by the
//! other subcommands only to enrich their existing "unrecognized format"
//! error with "(this looks like a PNG/JPEG/TIFF image, not an archive)"
//! when it applies.

use std::io::Cursor;

/// An image format this CLI can recognise and describe. Distinct from
/// [`oxiarc_archive::ArchiveFormat`]: an image is neither
/// [`oxiarc_archive::ArchiveFormat::is_archive`] nor
/// [`oxiarc_archive::ArchiveFormat::is_compression_only`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    /// ISO/IEC 15948 / W3C PNG.
    Png,
    /// ITU-T T.81 JPEG (JFIF/Exif/Adobe/raw).
    Jpeg,
    /// TIFF 6.0 or BigTIFF, either byte order.
    Tiff,
}

impl ImageKind {
    /// A short human-facing label, e.g. for `detect`'s `Format:` line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ImageKind::Png => "PNG image",
            ImageKind::Jpeg => "JPEG image",
            ImageKind::Tiff => "TIFF image",
        }
    }
}

/// Sniff `data`'s magic bytes for a recognised image format.
///
/// PNG delegates to [`oxiarc_png::is_png`], the crate's own predicate.
/// JPEG and TIFF are hand-rolled here: `oxiarc_jpeg::is_jpeg` and
/// `oxiarc_tiff::is_tiff` (the parallel predicates `repo-conventions.md`
/// §13 recommends those crates expose) do not exist in this codebase yet,
/// and adding them is outside this track's owned files — this is the
/// documented "smallest correct local workaround inside your own crate"
/// for that gap, not a stand-in for the crates eventually growing the real
/// thing.
#[must_use]
pub fn sniff(data: &[u8]) -> Option<ImageKind> {
    if oxiarc_png::is_png(data) {
        return Some(ImageKind::Png);
    }
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        return Some(ImageKind::Jpeg);
    }
    if data.len() >= 4 {
        let byte_order_ii = data[0] == b'I' && data[1] == b'I';
        let byte_order_mm = data[0] == b'M' && data[1] == b'M';
        let is_classic = (data[2] == 0x2A && data[3] == 0x00 && byte_order_ii)
            || (data[2] == 0x00 && data[3] == 0x2A && byte_order_mm);
        let is_big = (data[2] == 0x2B && data[3] == 0x00 && byte_order_ii)
            || (data[2] == 0x00 && data[3] == 0x2B && byte_order_mm);
        if is_classic || is_big {
            return Some(ImageKind::Tiff);
        }
    }
    None
}

/// One line of a chunk/segment/IFD summary (`info` only; `detect` never
/// requests these).
pub struct SummaryLine {
    /// Chunk type / marker name / tag name.
    pub label: String,
    /// A short human-facing detail (size, value, …).
    pub detail: String,
}

/// Everything `detect`/`info` print about a recognised image.
pub struct ImageSummary {
    /// Pixel dimensions.
    pub dimensions: (u32, u32),
    /// Colour type / photometric interpretation / colour space.
    pub colour: String,
    /// Sample precision.
    pub bit_depth: String,
    /// Compression scheme in force.
    pub compression: String,
    /// Chunk/segment/IFD entries, populated only when `with_chunks` is
    /// requested — walking every tag or segment is wasted work for
    /// `detect`, which never shows it.
    pub chunks: Vec<SummaryLine>,
}

/// Describe a recognised image, reading only what `kind` says to expect.
///
/// # Errors
///
/// Any structural problem parsing the header — a file `sniff` recognised by
/// magic but that turns out to be truncated or otherwise malformed past the
/// first few bytes.
pub fn describe(kind: ImageKind, data: &[u8], with_chunks: bool) -> Result<ImageSummary, String> {
    match kind {
        ImageKind::Png => describe_png(data, with_chunks),
        ImageKind::Jpeg => describe_jpeg(data, with_chunks),
        ImageKind::Tiff => describe_tiff(data, with_chunks),
    }
}

fn describe_png(data: &[u8], with_chunks: bool) -> Result<ImageSummary, String> {
    let info = oxiarc_png::peek_info(data).map_err(|e| e.to_string())?;
    Ok(ImageSummary {
        dimensions: info.size(),
        colour: format!("{:?}", info.color_type),
        bit_depth: format!("{}-bit", info.bit_depth as u8),
        // PNG's IHDR compression method is fixed at 0 (deflate/zlib) by the
        // spec and validated as such by `Ihdr::parse` — there is nothing
        // else it could be.
        compression: "Deflate (zlib)".to_string(),
        chunks: if with_chunks {
            walk_png_chunks(data)
        } else {
            Vec::new()
        },
    })
}

/// Walk PNG's public chunk framing (8-byte signature, then repeated
/// `[len:4][type:4][data:len][crc:4]`) to list every chunk's type and size.
/// Plain PNG container framing, not a reach into `oxiarc-png` internals.
fn walk_png_chunks(data: &[u8]) -> Vec<SummaryLine> {
    let mut out = Vec::new();
    let mut pos = 8usize; // past the 8-byte signature
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        let kind = String::from_utf8_lossy(&data[pos + 4..pos + 8]).into_owned();
        let data_start = pos + 8;
        let Some(data_end) = data_start.checked_add(len as usize) else {
            break;
        };
        if data_end.saturating_add(4) > data.len() {
            out.push(SummaryLine {
                label: kind,
                detail: format!("{len} bytes (truncated)"),
            });
            break;
        }
        out.push(SummaryLine {
            label: kind,
            detail: format!("{len} bytes"),
        });
        pos = data_end + 4; // past the trailing CRC
    }
    out
}

fn describe_jpeg(data: &[u8], with_chunks: bool) -> Result<ImageSummary, String> {
    let mut decoder = oxiarc_jpeg::Decoder::new(Cursor::new(data));
    let info = decoder.read_info().map_err(|e| e.to_string())?;
    Ok(ImageSummary {
        dimensions: (u32::from(info.width), u32::from(info.height)),
        colour: format!(
            "{:?} ({} component{})",
            info.output_color_space,
            info.num_components,
            if info.num_components == 1 { "" } else { "s" }
        ),
        bit_depth: format!("{}-bit", info.precision),
        compression: format!("{:?}/{:?}", info.process, info.entropy),
        chunks: if with_chunks {
            walk_jpeg_segments(data)
        } else {
            Vec::new()
        },
    })
}

/// A short, human-facing name for a JPEG marker byte (the byte after
/// `0xFF`), matching ITU-T T.81 Table B.1.
fn jpeg_marker_name(marker: u8) -> &'static str {
    match marker {
        0xC0 => "SOF0 (baseline)",
        0xC1 => "SOF1 (extended sequential)",
        0xC2 => "SOF2 (progressive)",
        0xC3 => "SOF3 (lossless)",
        0xC4 => "DHT",
        0xC5 => "SOF5 (differential sequential)",
        0xC6 => "SOF6 (differential progressive)",
        0xC7 => "SOF7 (differential lossless)",
        0xC8 => "JPG (reserved)",
        0xC9 => "SOF9 (extended sequential, arithmetic)",
        0xCA => "SOF10 (progressive, arithmetic)",
        0xCB => "SOF11 (lossless, arithmetic)",
        0xCC => "DAC",
        0xCD => "SOF13 (differential sequential, arithmetic)",
        0xCE => "SOF14 (differential progressive, arithmetic)",
        0xCF => "SOF15 (differential lossless, arithmetic)",
        0xDB => "DQT",
        0xDC => "DNL",
        0xDD => "DRI",
        0xDE => "DHP",
        0xDF => "EXP",
        0xE0..=0xEF => "APPn",
        0xFE => "COM",
        _ => "segment",
    }
}

/// Walk JPEG marker segments from `SOI` up to (and including) the first
/// `SOS`, then stop: everything past `SOS` is entropy-coded scan data with
/// byte-stuffed `0xFF00` and restart markers woven through it, which needs
/// the real entropy decoder to skip correctly, not a byte scanner. That
/// matches what a lightweight `info` summary needs — the tables and frame
/// header, not a full re-walk of the compressed data.
fn walk_jpeg_segments(data: &[u8]) -> Vec<SummaryLine> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    // Skip to the first 0xFF (the SOI marker itself, at offset 0 for
    // anything `sniff` recognised, but defensive regardless).
    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            break;
        }
        let marker = data[pos + 1];
        pos += 2;
        match marker {
            0x00 | 0xFF => continue, // stuffing / fill bytes between markers
            0xD8 => {
                out.push(SummaryLine {
                    label: "SOI".to_string(),
                    detail: String::new(),
                });
            }
            0xD9 => {
                out.push(SummaryLine {
                    label: "EOI".to_string(),
                    detail: String::new(),
                });
                break;
            }
            0xD0..=0xD7 | 0x01 => {
                // RSTn / TEM: no length field, and none expected before SOS.
                out.push(SummaryLine {
                    label: "RST/TEM".to_string(),
                    detail: String::new(),
                });
            }
            0xDA => {
                let Some(len) = read_be_u16(data, pos) else {
                    break;
                };
                out.push(SummaryLine {
                    label: "SOS".to_string(),
                    detail: format!("{len} bytes header, then entropy-coded data"),
                });
                break;
            }
            _ => {
                let Some(len) = read_be_u16(data, pos) else {
                    break;
                };
                if len < 2 {
                    break; // malformed: a segment length always includes itself
                }
                out.push(SummaryLine {
                    label: jpeg_marker_name(marker).to_string(),
                    detail: format!("{len} bytes"),
                });
                let Some(next) = pos.checked_add(len as usize) else {
                    break;
                };
                if next > data.len() {
                    break;
                }
                pos = next;
            }
        }
    }
    out
}

/// Read a big-endian `u16` at `pos`, or `None` if it does not fit.
fn read_be_u16(data: &[u8], pos: usize) -> Option<u16> {
    let bytes = data.get(pos..pos + 2)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn describe_tiff(data: &[u8], with_chunks: bool) -> Result<ImageSummary, String> {
    let mut decoder = oxiarc_tiff::Decoder::new(Cursor::new(data)).map_err(|e| e.to_string())?;
    let info = decoder.info().map_err(|e| e.to_string())?.clone();
    let chunks = if with_chunks {
        decoder
            .all_tags()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|(tag, value)| SummaryLine {
                label: format!("{tag:?}"),
                detail: format!("{value:?}"),
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(ImageSummary {
        dimensions: (info.width, info.height),
        colour: format!("{:?}", info.photometric),
        bit_depth: format!("{:?}-bit", info.bits_per_sample),
        compression: format!("{:?}", info.compression),
        chunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognises_png_magic() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        assert_eq!(sniff(png), Some(ImageKind::Png));
    }

    #[test]
    fn sniff_recognises_jpeg_magic() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageKind::Jpeg));
    }

    #[test]
    fn sniff_recognises_classic_tiff_both_byte_orders() {
        assert_eq!(sniff(&[b'I', b'I', 0x2A, 0x00]), Some(ImageKind::Tiff));
        assert_eq!(sniff(&[b'M', b'M', 0x00, 0x2A]), Some(ImageKind::Tiff));
    }

    #[test]
    fn sniff_recognises_bigtiff_both_byte_orders() {
        assert_eq!(sniff(&[b'I', b'I', 0x2B, 0x00]), Some(ImageKind::Tiff));
        assert_eq!(sniff(&[b'M', b'M', 0x00, 0x2B]), Some(ImageKind::Tiff));
    }

    #[test]
    fn sniff_rejects_unrelated_bytes() {
        assert_eq!(sniff(b"PK\x03\x04"), None); // ZIP local file header
        assert_eq!(sniff(&[]), None);
        assert_eq!(sniff(&[0x00, 0x01]), None);
    }

    #[test]
    fn walk_png_chunks_lists_ihdr_and_iend() {
        // Signature + a minimal IHDR (13 bytes) + IEND (0 bytes), with
        // placeholder (unchecked) CRCs — the walker never validates them.
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.extend_from_slice(&13u32.to_be_bytes());
        data.extend_from_slice(b"IHDR");
        data.extend_from_slice(&[0u8; 13]);
        data.extend_from_slice(&[0u8; 4]); // CRC placeholder
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(b"IEND");
        data.extend_from_slice(&[0u8; 4]);

        let chunks = walk_png_chunks(&data);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].label, "IHDR");
        assert_eq!(chunks[0].detail, "13 bytes");
        assert_eq!(chunks[1].label, "IEND");
        assert_eq!(chunks[1].detail, "0 bytes");
    }

    /// Deterministic xorshift, matching the house pattern used by
    /// `oxiarc-deflate/tests/adversarial_verify.rs` — reproducible on every
    /// machine, no `rand` dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// A real 6x5 RGB8 PNG, built with `oxiarc-png`'s own encoder.
    fn png_fixture() -> Vec<u8> {
        use oxiarc_png::{BitDepth, ColorType, Encoder};
        let (width, height) = (6u32, 5u32);
        let pixels: Vec<u8> = (0..(width * height * 3) as usize)
            .map(|i| (i * 7) as u8)
            .collect();
        let mut bytes = Vec::new();
        {
            let mut enc = Encoder::new(&mut bytes, width, height);
            enc.set_color(ColorType::Rgb);
            enc.set_depth(BitDepth::Eight);
            let mut writer = enc.write_header().expect("png header");
            writer.write_image_data(&pixels).expect("png image data");
            writer.finish().expect("png finish");
        }
        bytes
    }

    /// A real 8x8 RGB JPEG, built with `oxiarc-jpeg`'s own encoder.
    fn jpeg_fixture() -> Vec<u8> {
        use oxiarc_jpeg::InputColor;
        let pixels: Vec<u8> = (0..(8 * 8 * 3)).map(|i| (i * 3) as u8).collect();
        oxiarc_jpeg::encode_to_vec(&pixels, 8, 8, InputColor::Rgb, 80).expect("jpeg encode")
    }

    /// A real 5x4 RGB8 uncompressed TIFF, built with `oxiarc-tiff`'s own
    /// encoder.
    fn tiff_fixture() -> Vec<u8> {
        use oxiarc_tiff::{ColorType, Encoder, ImageSpec};
        let (width, height) = (5u32, 4u32);
        let pixels: Vec<u8> = (0..(width * height * 3) as usize)
            .map(|i| (i * 13) as u8)
            .collect();
        let spec = ImageSpec::new(width, height, ColorType::Rgb(8));
        let mut bytes = Vec::new();
        {
            let mut enc = Encoder::new(Cursor::new(&mut bytes)).expect("tiff encoder");
            enc.write_image(&spec, &pixels).expect("tiff write_image");
            let _ = enc.finish().expect("tiff finish");
        }
        bytes
    }

    /// Every prefix of a real file of each format, through the whole probe
    /// surface: `sniff` must not read out of bounds, and neither `describe`
    /// nor either container walker may panic, hang, or loop forever on a
    /// stream that stops mid-header, mid-chunk or mid-segment.
    #[test]
    fn describe_survives_truncation_at_every_offset() {
        for full in [png_fixture(), jpeg_fixture(), tiff_fixture()] {
            for cut in 0..=full.len() {
                let prefix = &full[..cut];
                if let Some(kind) = sniff(prefix) {
                    // Both `with_chunks` settings: `detect` uses `false`,
                    // `info` uses `true`, and only the latter runs the
                    // container walkers.
                    let _ = describe(kind, prefix, false);
                    let _ = describe(kind, prefix, true);
                }
            }
        }
    }

    /// A chunk/segment header that declares far more bytes than the file
    /// holds must terminate the walk, not index past the end and not spin.
    #[test]
    fn walkers_reject_absurd_declared_lengths() {
        // PNG: signature + a chunk header claiming `u32::MAX` bytes.
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&u32::MAX.to_be_bytes());
        png.extend_from_slice(b"IDAT");
        png.extend_from_slice(&[0u8; 4]);
        let chunks = walk_png_chunks(&png);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].label, "IDAT");
        assert!(
            chunks[0].detail.contains("truncated"),
            "{:?}",
            chunks[0].detail
        );

        // PNG: a zero-length chunk repeated to the end must still terminate
        // (each iteration has to advance by the full 12-byte frame).
        let mut zeros = b"\x89PNG\r\n\x1a\n".to_vec();
        for _ in 0..64 {
            zeros.extend_from_slice(&0u32.to_be_bytes());
            zeros.extend_from_slice(b"tEXt");
            zeros.extend_from_slice(&[0u8; 4]);
        }
        assert_eq!(walk_png_chunks(&zeros).len(), 64);

        // JPEG: SOI then an APP0 claiming 0xFFFF bytes it does not have.
        let jpeg: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0xFF, 0xFF, 0x00, 0x01];
        let segments = walk_jpeg_segments(jpeg);
        assert_eq!(segments.len(), 2, "{segments:?}", segments = segments.len());

        // JPEG: a segment whose declared length is below the 2-byte minimum
        // must stop the walk rather than move `pos` backwards.
        let bad_len: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x00, 0xFF, 0xD9];
        let segments = walk_jpeg_segments(bad_len);
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].label, "SOI");

        // JPEG: a run of `FF FF` fill bytes must terminate.
        let fills: Vec<u8> = std::iter::once(0xFFu8)
            .chain(std::iter::once(0xD8))
            .chain(std::iter::repeat_n(0xFFu8, 512))
            .collect();
        let _ = walk_jpeg_segments(&fills);
    }

    /// Single-byte and empty inputs, and the exact magic-length boundaries,
    /// must all be answered without reading past the slice.
    #[test]
    fn sniff_handles_short_inputs() {
        for len in 0..8usize {
            let png = &png_fixture()[..len];
            // Under 8 bytes the PNG signature cannot be complete.
            assert_eq!(sniff(png), None, "len {len}");
        }
        assert_eq!(sniff(&[0xFF]), None);
        assert_eq!(sniff(&[0xFF, 0xD8]), None);
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF]), Some(ImageKind::Jpeg));
        assert_eq!(sniff(&[b'I', b'I', 0x2A]), None);
        assert_eq!(sniff(&[b'M', b'M', 0x00]), None);
    }

    /// Byte flips, drops and insertions in a real file of each format: the
    /// probe may report anything it likes, but it may never panic.
    #[test]
    fn describe_survives_random_mutation() {
        let mut rng = Rng(0x0D15_EA5E_1234_5678);
        for full in [png_fixture(), jpeg_fixture(), tiff_fixture()] {
            for _ in 0..400 {
                let mut data = full.clone();
                match rng.next() % 3 {
                    0 => {
                        let at = (rng.next() as usize) % data.len();
                        data[at] ^= 1u8 << (rng.next() % 8);
                    }
                    1 => {
                        let at = (rng.next() as usize) % data.len();
                        data.remove(at);
                    }
                    _ => {
                        let at = (rng.next() as usize) % data.len();
                        data.insert(at, (rng.next() >> 33) as u8);
                    }
                }
                if let Some(kind) = sniff(&data) {
                    let _ = describe(kind, &data, true);
                }
                // The walkers are also driven directly, so a mutation that
                // breaks the magic (and therefore never reaches `describe`)
                // still exercises them.
                let _ = walk_png_chunks(&data);
                let _ = walk_jpeg_segments(&data);
            }
        }
    }

    #[test]
    fn walk_jpeg_segments_stops_at_sos() {
        // SOI, a 4-byte APP0-shaped segment, SOS with a 2-byte-only header,
        // then bytes that must never be interpreted as further segments.
        let data: &[u8] = &[
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, 0x00, 0x04, 0xAA, 0xBB, // APPn, length 4 (2 header + 2 payload)
            0xFF, 0xDA, 0x00, 0x02, // SOS, length 2 (header only)
            0x12, 0x34, 0x56, // "entropy-coded data", never parsed as markers
        ];
        let segments = walk_jpeg_segments(data);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].label, "SOI");
        assert_eq!(segments[1].label, "APPn");
        assert_eq!(segments[1].detail, "4 bytes");
        assert_eq!(segments[2].label, "SOS");
    }
}

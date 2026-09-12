//! `APPn` and `COM` segments: recognised headers plus verbatim passthrough.
//!
//! Nothing in this module *interprets* metadata — EXIF is not parsed, ICC
//! profiles are not applied and orientation is not honoured. Segments are
//! recognised only far enough to answer the colour-space question (`JFIF` and
//! `Adobe`) and are otherwise handed back byte for byte so that a container
//! such as `oxiarc-tiff`, or a `jpegtran`-style transcode, can carry them
//! through untouched.

use crate::error::{JpegError, Result};

/// Prefix of an `APP0` `JFIF` segment.
const JFIF_ID: &[u8] = b"JFIF\0";
/// Prefix of an `APP0` `JFXX` extension segment.
const JFXX_ID: &[u8] = b"JFXX\0";
/// Prefix of an `APP1` EXIF segment.
const EXIF_ID: &[u8] = b"Exif\0\0";
/// Prefix of an `APP1` XMP packet.
const XMP_ID: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// Prefix of an `APP2` ICC profile chunk.
const ICC_ID: &[u8] = b"ICC_PROFILE\0";
/// Prefix of an `APP14` Adobe segment (five bytes, no NUL).
const ADOBE_ID: &[u8] = b"Adobe";
/// Largest ICC profile this crate will reassemble.
const MAX_ICC_BYTES: usize = 64 << 20;

/// A `JFIF` `APP0` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JfifHeader {
    /// Major version.
    pub version_major: u8,
    /// Minor version.
    pub version_minor: u8,
    /// Density units: 0 = aspect ratio only, 1 = dots per inch, 2 = per cm.
    pub units: u8,
    /// Horizontal pixel density.
    pub x_density: u16,
    /// Vertical pixel density.
    pub y_density: u16,
    /// Thumbnail width in pixels.
    pub thumbnail_width: u8,
    /// Thumbnail height in pixels.
    pub thumbnail_height: u8,
}

/// An `Adobe` `APP14` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdobeHeader {
    /// DCT encoder version.
    pub version: u16,
    /// `flags0`.
    pub flags0: u16,
    /// `flags1`.
    pub flags1: u16,
    /// Colour transform: 0 = none (RGB or CMYK), 1 = YCbCr, 2 = YCCK.
    pub transform: u8,
}

/// One `APPn` or `COM` segment, retained verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSegment {
    /// The marker code (`0xE0..=0xEF` for `APPn`, `0xFE` for `COM`).
    pub marker: u8,
    /// The payload, excluding the marker and the two length bytes.
    pub data: Vec<u8>,
}

/// Everything recognised or retained from the metadata segments of one
/// datastream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    /// The `JFIF` `APP0` header, if one was present.
    pub jfif: Option<JfifHeader>,
    /// The `Adobe` `APP14` header, if one was present.
    pub adobe: Option<AdobeHeader>,
    /// `true` when a `JFXX` extension segment was present.
    pub jfxx: bool,
    /// The EXIF payload, with its `Exif\0\0` prefix removed.
    pub exif: Option<Vec<u8>>,
    /// The XMP packet, with its namespace URI prefix removed.
    pub xmp: Option<Vec<u8>>,
    /// `COM` segment contents, in order.
    pub comments: Vec<Vec<u8>>,
    /// Every `APPn` and `COM` segment, verbatim and in order.
    pub segments: Vec<AppSegment>,
    /// `APP2` ICC chunks, indexed by `(sequence, count, payload)`.
    icc_chunks: Vec<(u8, u8, Vec<u8>)>,
}

impl Metadata {
    /// Record one `APPn` or `COM` segment.
    pub(crate) fn push(&mut self, marker: u8, data: &[u8]) {
        match marker {
            0xE0 => self.parse_app0(data),
            0xE1 => self.parse_app1(data),
            0xE2 => self.parse_app2(data),
            0xEE => self.parse_app14(data),
            0xFE => self.comments.push(data.to_vec()),
            _ => {}
        }
        self.segments.push(AppSegment {
            marker,
            data: data.to_vec(),
        });
    }

    fn parse_app0(&mut self, data: &[u8]) {
        if data.starts_with(JFXX_ID) {
            self.jfxx = true;
            return;
        }
        if !data.starts_with(JFIF_ID) || data.len() < JFIF_ID.len() + 9 {
            return;
        }
        let body = &data[JFIF_ID.len()..];
        self.jfif = Some(JfifHeader {
            version_major: body[0],
            version_minor: body[1],
            units: body[2],
            x_density: u16::from_be_bytes([body[3], body[4]]),
            y_density: u16::from_be_bytes([body[5], body[6]]),
            thumbnail_width: body[7],
            thumbnail_height: body[8],
        });
    }

    fn parse_app1(&mut self, data: &[u8]) {
        if data.starts_with(EXIF_ID) {
            if self.exif.is_none() {
                self.exif = Some(data[EXIF_ID.len()..].to_vec());
            }
        } else if data.starts_with(XMP_ID) && self.xmp.is_none() {
            self.xmp = Some(data[XMP_ID.len()..].to_vec());
        }
    }

    fn parse_app2(&mut self, data: &[u8]) {
        if !data.starts_with(ICC_ID) || data.len() < ICC_ID.len() + 2 {
            return;
        }
        let body = &data[ICC_ID.len()..];
        self.icc_chunks.push((body[0], body[1], body[2..].to_vec()));
    }

    fn parse_app14(&mut self, data: &[u8]) {
        if !data.starts_with(ADOBE_ID) || data.len() < ADOBE_ID.len() + 7 {
            return;
        }
        let body = &data[ADOBE_ID.len()..];
        self.adobe = Some(AdobeHeader {
            version: u16::from_be_bytes([body[0], body[1]]),
            flags0: u16::from_be_bytes([body[2], body[3]]),
            flags1: u16::from_be_bytes([body[4], body[5]]),
            transform: body[6],
        });
    }

    /// Reassemble the `APP2` ICC chunks in sequence order.
    ///
    /// Returns `None` when no chunk was seen. Duplicated or missing sequence
    /// numbers, a disagreeing chunk count, or a profile larger than 64 MiB are
    /// rejected.
    pub(crate) fn icc_profile(&self) -> Result<Option<Vec<u8>>> {
        if self.icc_chunks.is_empty() {
            return Ok(None);
        }
        let count = self.icc_chunks[0].1;
        if count == 0 || self.icc_chunks.len() != usize::from(count) {
            return Err(JpegError::malformed(
                "APP2",
                0,
                "ICC chunk count disagrees with the number of chunks",
            ));
        }
        let mut ordered: Vec<Option<&Vec<u8>>> = vec![None; usize::from(count)];
        let mut total = 0usize;
        for (seq, chunk_count, payload) in &self.icc_chunks {
            if *chunk_count != count || *seq == 0 || *seq > count {
                return Err(JpegError::malformed(
                    "APP2",
                    0,
                    "ICC chunk sequence number is out of range",
                ));
            }
            let slot = &mut ordered[usize::from(*seq) - 1];
            if slot.is_some() {
                return Err(JpegError::malformed(
                    "APP2",
                    0,
                    "duplicate ICC chunk sequence number",
                ));
            }
            total = total.saturating_add(payload.len());
            if total > MAX_ICC_BYTES {
                return Err(JpegError::malformed("APP2", 0, "ICC profile is too large"));
            }
            *slot = Some(payload);
        }
        let mut profile = Vec::with_capacity(total);
        for slot in ordered {
            let chunk = slot.ok_or(JpegError::malformed(
                "APP2",
                0,
                "ICC profile has a missing chunk",
            ))?;
            profile.extend_from_slice(chunk);
        }
        Ok(Some(profile))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_jfif_header() {
        let mut data = JFIF_ID.to_vec();
        data.extend_from_slice(&[1, 2, 1, 0, 72, 0, 72, 0, 0]);
        let mut meta = Metadata::default();
        meta.push(0xE0, &data);
        let jfif = meta.jfif.expect("JFIF");
        assert_eq!(jfif.version_major, 1);
        assert_eq!(jfif.version_minor, 2);
        assert_eq!(jfif.units, 1);
        assert_eq!((jfif.x_density, jfif.y_density), (72, 72));
        assert_eq!(meta.segments.len(), 1);
        assert_eq!(meta.segments[0].marker, 0xE0);
    }

    #[test]
    fn recognises_jfxx_without_claiming_jfif() {
        let mut meta = Metadata::default();
        meta.push(0xE0, JFXX_ID);
        assert!(meta.jfxx);
        assert!(meta.jfif.is_none());
    }

    #[test]
    fn ignores_a_truncated_jfif() {
        let mut data = JFIF_ID.to_vec();
        data.extend_from_slice(&[1, 2]);
        let mut meta = Metadata::default();
        meta.push(0xE0, &data);
        assert!(meta.jfif.is_none());
        assert_eq!(meta.segments.len(), 1, "still retained verbatim");
    }

    #[test]
    fn parses_an_adobe_header() {
        let mut data = ADOBE_ID.to_vec();
        data.extend_from_slice(&[0x00, 0x64, 0x80, 0x00, 0x00, 0x00, 2]);
        let mut meta = Metadata::default();
        meta.push(0xEE, &data);
        let adobe = meta.adobe.expect("Adobe");
        assert_eq!(adobe.version, 100);
        assert_eq!(adobe.flags0, 0x8000);
        assert_eq!(adobe.transform, 2);
    }

    #[test]
    fn extracts_exif_and_xmp() {
        let mut meta = Metadata::default();
        let mut exif = EXIF_ID.to_vec();
        exif.extend_from_slice(b"II*\0");
        meta.push(0xE1, &exif);
        let mut xmp = XMP_ID.to_vec();
        xmp.extend_from_slice(b"<x:xmpmeta/>");
        meta.push(0xE1, &xmp);
        assert_eq!(meta.exif.as_deref(), Some(&b"II*\0"[..]));
        assert_eq!(meta.xmp.as_deref(), Some(&b"<x:xmpmeta/>"[..]));
        assert_eq!(meta.segments.len(), 2);
    }

    #[test]
    fn collects_comments() {
        let mut meta = Metadata::default();
        meta.push(0xFE, b"hello");
        meta.push(0xFE, b"world");
        assert_eq!(meta.comments, vec![b"hello".to_vec(), b"world".to_vec()]);
    }

    fn icc_chunk(seq: u8, count: u8, body: &[u8]) -> Vec<u8> {
        let mut data = ICC_ID.to_vec();
        data.push(seq);
        data.push(count);
        data.extend_from_slice(body);
        data
    }

    #[test]
    fn reassembles_icc_chunks_in_order() {
        let mut meta = Metadata::default();
        meta.push(0xE2, &icc_chunk(2, 2, b"world"));
        meta.push(0xE2, &icc_chunk(1, 2, b"hello "));
        assert_eq!(
            meta.icc_profile().expect("valid"),
            Some(b"hello world".to_vec())
        );
    }

    #[test]
    fn no_icc_chunks_means_no_profile() {
        assert_eq!(Metadata::default().icc_profile().expect("valid"), None);
    }

    #[test]
    fn rejects_broken_icc_sequences() {
        let mut meta = Metadata::default();
        meta.push(0xE2, &icc_chunk(1, 2, b"a"));
        assert!(meta.icc_profile().is_err(), "missing chunk 2");

        let mut meta = Metadata::default();
        meta.push(0xE2, &icc_chunk(1, 2, b"a"));
        meta.push(0xE2, &icc_chunk(1, 2, b"b"));
        assert!(meta.icc_profile().is_err(), "duplicate chunk 1");

        let mut meta = Metadata::default();
        meta.push(0xE2, &icc_chunk(0, 1, b"a"));
        assert!(meta.icc_profile().is_err(), "sequence 0");

        let mut meta = Metadata::default();
        meta.push(0xE2, &icc_chunk(1, 1, b"a"));
        meta.push(0xE2, &icc_chunk(2, 2, b"b"));
        assert!(meta.icc_profile().is_err(), "count disagreement");
    }

    #[test]
    fn unknown_app_segments_are_retained_but_not_interpreted() {
        let mut meta = Metadata::default();
        meta.push(0xE7, b"private");
        assert_eq!(meta.segments[0].data, b"private");
        assert!(meta.jfif.is_none());
        assert!(meta.adobe.is_none());
    }

    #[test]
    fn empty_payloads_do_not_panic() {
        let mut meta = Metadata::default();
        for marker in [0xE0u8, 0xE1, 0xE2, 0xEE, 0xFE] {
            meta.push(marker, &[]);
        }
        assert_eq!(meta.segments.len(), 5);
        assert!(meta.icc_profile().expect("no chunks").is_none());
    }
}

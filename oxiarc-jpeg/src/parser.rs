//! Marker-level segment scanner shared by the decoder and by
//! [`crate::TableSet::parse`].

use crate::error::{JpegError, Result};
use crate::marker::Marker;

/// One parsed segment: its marker code, the offset of the leading `0xFF`, and
/// the payload after the two length bytes (empty for standalone markers).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Segment<'a> {
    /// Marker code, i.e. the byte after `0xFF`.
    pub(crate) code: u8,
    /// Offset of the `0xFF` that introduced the marker.
    pub(crate) offset: usize,
    /// Payload after the length field.
    pub(crate) payload: &'a [u8],
}

/// A forward-only scanner over a JPEG datastream.
///
/// Bytes that are not part of a marker are skipped, matching libjpeg's
/// `next_marker`, which is what lets the scanner survive the padding real
/// scanners emit between segments.
pub(crate) struct Scanner<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    /// Start scanning at the beginning of `data`.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Current byte offset.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    /// Continue scanning from `pos`.
    pub(crate) fn seek(&mut self, pos: usize) {
        self.pos = pos.min(self.data.len());
    }

    /// The bytes from the current position to the end of the stream.
    #[cfg(test)]
    pub(crate) fn remaining(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Read the next segment, or `None` at end of stream.
    ///
    /// Stops *before* the entropy-coded data of a `SOS`: the returned segment
    /// is the `SOS` header, and [`Scanner::position`] then points at the first
    /// entropy byte.
    pub(crate) fn next_segment(&mut self) -> Result<Option<Segment<'a>>> {
        // Find the next marker, skipping any non-marker bytes.
        let mut i = self.pos;
        loop {
            while i < self.data.len() && self.data[i] != 0xFF {
                i += 1;
            }
            if i >= self.data.len() {
                self.pos = self.data.len();
                return Ok(None);
            }
            let start = i;
            let mut probe = i + 1;
            while self.data.get(probe) == Some(&0xFF) {
                probe += 1;
            }
            match self.data.get(probe) {
                None => {
                    self.pos = self.data.len();
                    return Ok(None);
                }
                Some(0) => {
                    // A stuffed byte outside entropy data: skip it.
                    i = probe + 1;
                    continue;
                }
                Some(&code) => {
                    let marker = match Marker::from_code(code) {
                        Some(marker) => marker,
                        None => {
                            i = probe + 1;
                            continue;
                        }
                    };
                    self.pos = probe + 1;
                    if marker.is_standalone() {
                        return Ok(Some(Segment {
                            code,
                            offset: start,
                            payload: &self.data[self.pos..self.pos],
                        }));
                    }
                    let length_bytes = self
                        .data
                        .get(self.pos..self.pos + 2)
                        .ok_or(JpegError::eof("segment length"))?;
                    let length =
                        usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
                    if length < 2 {
                        return Err(JpegError::malformed(
                            "segment",
                            start,
                            "length field is less than 2",
                        ));
                    }
                    let body_start = self.pos + 2;
                    let body_end = body_start
                        .checked_add(length - 2)
                        .ok_or(JpegError::eof("segment payload"))?;
                    if body_end > self.data.len() {
                        return Err(JpegError::eof("segment payload"));
                    }
                    self.pos = body_end;
                    return Ok(Some(Segment {
                        code,
                        offset: start,
                        payload: &self.data[body_start..body_end],
                    }));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_a_minimal_stream() {
        // SOI, COM("hi"), EOI
        let data = [0xFFu8, 0xD8, 0xFF, 0xFE, 0x00, 0x04, b'h', b'i', 0xFF, 0xD9];
        let mut scanner = Scanner::new(&data);
        let first = scanner.next_segment().expect("ok").expect("SOI");
        assert_eq!(first.code, 0xD8);
        assert!(first.payload.is_empty());
        let second = scanner.next_segment().expect("ok").expect("COM");
        assert_eq!(second.code, 0xFE);
        assert_eq!(second.payload, b"hi");
        let third = scanner.next_segment().expect("ok").expect("EOI");
        assert_eq!(third.code, 0xD9);
        assert!(scanner.next_segment().expect("ok").is_none());
    }

    #[test]
    fn skips_fill_bytes_and_leading_garbage() {
        let data = [0x00u8, 0x11, 0xFF, 0xFF, 0xFF, 0xD8, 0xFF, 0xD9];
        let mut scanner = Scanner::new(&data);
        assert_eq!(scanner.next_segment().expect("ok").expect("SOI").code, 0xD8);
        assert_eq!(scanner.next_segment().expect("ok").expect("EOI").code, 0xD9);
    }

    #[test]
    fn stuffed_bytes_between_segments_are_skipped() {
        let data = [0xFFu8, 0x00, 0xFF, 0xD8];
        let mut scanner = Scanner::new(&data);
        assert_eq!(scanner.next_segment().expect("ok").expect("SOI").code, 0xD8);
    }

    #[test]
    fn stops_before_entropy_data() {
        // SOS with one component, then two entropy bytes.
        let data = [
            0xFFu8, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xAB, 0xCD,
        ];
        let mut scanner = Scanner::new(&data);
        let sos = scanner.next_segment().expect("ok").expect("SOS");
        assert_eq!(sos.code, 0xDA);
        assert_eq!(sos.payload, &[0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        assert_eq!(scanner.remaining(), &[0xAB, 0xCD]);
    }

    #[test]
    fn rejects_a_length_below_two() {
        let data = [0xFFu8, 0xDB, 0x00, 0x01];
        let mut scanner = Scanner::new(&data);
        assert!(scanner.next_segment().is_err());
    }

    #[test]
    fn rejects_a_truncated_payload() {
        let data = [0xFFu8, 0xDB, 0x00, 0x40, 0x01];
        let mut scanner = Scanner::new(&data);
        assert!(matches!(
            scanner.next_segment(),
            Err(JpegError::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn truncated_length_field_is_eof() {
        let data = [0xFFu8, 0xDB, 0x00];
        let mut scanner = Scanner::new(&data);
        assert!(scanner.next_segment().is_err());
    }

    #[test]
    fn trailing_ff_run_ends_the_scan() {
        let data = [0xFFu8, 0xD8, 0xFF, 0xFF];
        let mut scanner = Scanner::new(&data);
        assert_eq!(scanner.next_segment().expect("ok").expect("SOI").code, 0xD8);
        assert!(scanner.next_segment().expect("ok").is_none());
    }

    #[test]
    fn seek_resumes_the_walk() {
        let data = [0xFFu8, 0xD8, 0xFF, 0xD9, 0xFF, 0xD8];
        let mut scanner = Scanner::new(&data);
        assert_eq!(scanner.next_segment().expect("ok").expect("SOI").code, 0xD8);
        scanner.seek(4);
        assert_eq!(scanner.position(), 4);
        assert_eq!(scanner.next_segment().expect("ok").expect("SOI").code, 0xD8);
        scanner.seek(usize::MAX);
        assert_eq!(scanner.position(), data.len());
    }

    #[test]
    fn empty_input_yields_nothing() {
        let mut scanner = Scanner::new(&[]);
        assert!(scanner.next_segment().expect("ok").is_none());
    }
}

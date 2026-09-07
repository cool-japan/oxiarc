//! A marker walk over an abbreviated JPEG datastream.
//!
//! TIFF needs three facts *before* it can decide how to decode a strip, and all
//! three live in the header: the frame's sampling factors (TTN2 makes them
//! authoritative over `YCbCrSubSampling`), the sample precision, and the Adobe
//! `APP14` colour transform. Reading them with a marker walk costs a few
//! hundred bytes of scanning and no allocation, where constructing a decoder
//! would copy the whole strip.

/// One component of the frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Component {
    /// Component identifier `Ci`.
    pub(super) id: u8,
    /// Horizontal sampling factor `Hi`.
    pub(super) h: u8,
    /// Vertical sampling factor `Vi`.
    pub(super) v: u8,
}

/// What the header of a strip (or of a `JPEGTables` blob) declares.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct FrameFacts {
    /// Sample precision from the `SOF`, or 0 when there is none.
    pub(super) precision: u8,
    /// Frame width `X`.
    pub(super) width: u16,
    /// Frame height `Y`.
    pub(super) height: u16,
    /// The frame's components, in `SOF` order.
    pub(super) components: Vec<Component>,
    /// The Adobe `APP14` transform byte, when that marker is present.
    pub(super) adobe_transform: Option<u8>,
    /// Whether a `SOF` marker was seen.
    pub(super) has_frame: bool,
    /// Whether a `SOS` marker was seen.
    pub(super) has_scan: bool,
    /// Whether the stream starts with `SOI`.
    pub(super) has_soi: bool,
    /// The `SOF` marker code, so a caller can tell baseline from progressive
    /// or lossless.
    pub(super) frame_marker: u8,
}

impl FrameFacts {
    /// `(Hmax, Vmax)` over the frame's components.
    pub(super) fn max_sampling(&self) -> (u8, u8) {
        let h = self
            .components
            .iter()
            .map(|c| c.h)
            .max()
            .unwrap_or(1)
            .max(1);
        let v = self
            .components
            .iter()
            .map(|c| c.v)
            .max()
            .unwrap_or(1)
            .max(1);
        (h, v)
    }

    /// `true` when the frame is one this crate can hand to the JPEG decoder as
    /// a TIFF strip: sequential or progressive, never hierarchical.
    pub(super) fn is_decodable_frame(&self) -> bool {
        matches!(
            self.frame_marker,
            0xC0 | 0xC1 | 0xC2 | 0xC3 | 0xC9 | 0xCA | 0xCB
        )
    }
}

/// Reads a big-endian `u16` at `offset`.
fn be16(data: &[u8], offset: usize) -> Option<u16> {
    let hi = data.get(offset).copied()?;
    let lo = data.get(offset + 1).copied()?;
    Some(u16::from(hi) << 8 | u16::from(lo))
}

/// Walks the markers of `data` up to the first `SOS`.
///
/// Never fails: a stream that ends early simply reports what it managed to
/// declare, and the decoder proper produces the error message.
pub(super) fn scan(data: &[u8]) -> FrameFacts {
    let mut facts = FrameFacts::default();
    let mut index = 0usize;
    if data.first() == Some(&0xFF) && data.get(1) == Some(&0xD8) {
        facts.has_soi = true;
        index = 2;
    }
    while index + 1 < data.len() {
        if data.get(index) != Some(&0xFF) {
            index += 1;
            continue;
        }
        let marker = data.get(index + 1).copied().unwrap_or(0);
        index += 2;
        match marker {
            // Fill bytes and standalone markers carry no segment.
            0xFF => index -= 1,
            0x01 | 0xD0..=0xD7 => {}
            0xD8 => {}
            0xD9 => break,
            0xDA => {
                facts.has_scan = true;
                break;
            }
            _ => {
                let Some(length) = be16(data, index) else {
                    break;
                };
                let length = usize::from(length).max(2);
                let body = data
                    .get(index + 2..index + length)
                    .unwrap_or_else(|| data.get(index + 2..).unwrap_or(&[]));
                match marker {
                    0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                        read_frame(&mut facts, marker, body);
                    }
                    0xEE => read_adobe(&mut facts, body),
                    _ => {}
                }
                index += length;
            }
        }
    }
    facts
}

/// Fills in the frame facts from a `SOFn` segment body.
fn read_frame(facts: &mut FrameFacts, marker: u8, body: &[u8]) {
    if facts.has_frame {
        return;
    }
    let Some(precision) = body.first().copied() else {
        return;
    };
    let (Some(height), Some(width), Some(count)) =
        (be16(body, 1), be16(body, 3), body.get(5).copied())
    else {
        return;
    };
    facts.precision = precision;
    facts.height = height;
    facts.width = width;
    facts.frame_marker = marker;
    facts.has_frame = true;
    facts.components.clear();
    for slot in 0..usize::from(count).min(4) {
        let base = 6 + slot * 3;
        let (Some(id), Some(sampling)) = (body.get(base).copied(), body.get(base + 1).copied())
        else {
            break;
        };
        facts.components.push(Component {
            id,
            h: (sampling >> 4).max(1),
            v: (sampling & 0x0F).max(1),
        });
    }
}

/// Reads the Adobe `APP14` transform byte.
fn read_adobe(facts: &mut FrameFacts, body: &[u8]) {
    if body.len() >= 12 && body.get(..5) == Some(b"Adobe") {
        facts.adobe_transform = body.get(11).copied();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SOI SOF0(gray 8x8) SOS`.
    const GRAY: &[u8] = &[
        0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11, 0x00,
        0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00,
    ];

    #[test]
    fn a_baseline_header_is_read() {
        let facts = scan(GRAY);
        assert!(facts.has_soi && facts.has_frame && facts.has_scan);
        assert_eq!(facts.precision, 8);
        assert_eq!((facts.width, facts.height), (8, 8));
        assert_eq!(facts.components, vec![Component { id: 1, h: 1, v: 1 }]);
        assert_eq!(facts.max_sampling(), (1, 1));
        assert!(facts.is_decodable_frame());
        assert_eq!(facts.adobe_transform, None);
    }

    #[test]
    fn subsampling_factors_come_from_the_frame_header() {
        // Three components, 2x2 luma sampling.
        let mut data = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x40, 0x03, 0x01, 0x22,
            0x00, 0x02, 0x11, 0x01, 0x03, 0x11, 0x01,
        ];
        data.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x0C]);
        let facts = scan(&data);
        assert_eq!(facts.max_sampling(), (2, 2));
        assert_eq!(facts.components.len(), 3);
        assert_eq!(facts.components[0].h, 2);
        assert_eq!(facts.components[1].id, 2);
        assert_eq!((facts.width, facts.height), (64, 16));
    }

    #[test]
    fn the_adobe_transform_is_found() {
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xEE, 0x00, 0x0E];
        data.extend_from_slice(b"Adobe");
        data.extend_from_slice(&[0, 100, 0, 0, 0, 0, 2]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        let facts = scan(&data);
        assert_eq!(facts.adobe_transform, Some(2));
        assert!(!facts.has_frame);
    }

    #[test]
    fn a_truncated_header_never_panics() {
        for cut in 0..GRAY.len() {
            let facts = scan(&GRAY[..cut]);
            assert!(facts.components.len() <= 4);
        }
        assert_eq!(scan(&[]), FrameFacts::default());
        assert_eq!(scan(&[0xFF]), FrameFacts::default());
        assert!(!scan(&[0xFF, 0xD8, 0xFF]).has_frame);
    }

    #[test]
    fn tables_only_streams_declare_no_frame() {
        // SOI DQT(...) EOI
        let mut data = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x43, 0x00];
        data.extend_from_slice(&[16u8; 64]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        let facts = scan(&data);
        assert!(facts.has_soi);
        assert!(!facts.has_frame);
        assert!(!facts.has_scan);
        assert!(!facts.is_decodable_frame());
    }

    #[test]
    fn a_hierarchical_frame_is_recognised_and_refused() {
        let data = [
            0xFF, 0xD8, 0xFF, 0xC5, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11,
            0x00,
        ];
        let facts = scan(&data);
        assert!(facts.has_frame);
        assert_eq!(facts.frame_marker, 0xC5);
        assert!(!facts.is_decodable_frame());
    }
}

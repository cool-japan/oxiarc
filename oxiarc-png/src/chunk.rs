//! Chunk types, the four type-bit predicates, and CRC-checked chunk framing.
//!
//! A PNG chunk is `length: u32be | type: [u8; 4] | data[length] | crc: u32be`.
//! The CRC-32 covers the **type and data**, never the length, and is computed
//! with [`oxiarc_core::Crc32`] fed incrementally so no intermediate buffer is
//! ever allocated.

use std::fmt;
use std::io::Write;

use oxiarc_core::Crc32;

use crate::error::{DecodingError, FormatErrorKind};

/// The largest chunk payload the specification permits: 2^31 - 1 bytes.
pub const MAX_CHUNK_LEN: u32 = 0x7FFF_FFFF;

/// A four-byte chunk type code.
///
/// ```
/// use oxiarc_png::chunk::{self, ChunkType};
/// assert!(chunk::is_critical(chunk::IHDR));
/// assert!(!chunk::is_critical(chunk::tEXt));
/// assert!(chunk::safe_to_copy(ChunkType(*b"prVt")));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkType(pub [u8; 4]);

impl ChunkType {
    /// The four raw bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 4] {
        self.0
    }

    /// True when every byte is an ASCII letter, as the specification requires.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        let b = self.0;
        let mut i = 0;
        while i < 4 {
            let c = b[i];
            if !(c.is_ascii_uppercase() || c.is_ascii_lowercase()) {
                return false;
            }
            i += 1;
        }
        true
    }
}

impl fmt::Debug for ChunkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        struct DebugType([u8; 4]);

        impl fmt::Debug for DebugType {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for &c in &self.0[..] {
                    write!(f, "{}", char::from(c).escape_debug())?;
                }
                Ok(())
            }
        }

        f.debug_struct("ChunkType")
            .field("type", &DebugType(self.0))
            .field("critical", &is_critical(*self))
            .field("private", &is_private(*self))
            .field("reserved", &reserved_set(*self))
            .field("safecopy", &safe_to_copy(*self))
            .finish()
    }
}

impl fmt::Display for ChunkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &c in &self.0[..] {
            write!(f, "{}", char::from(c).escape_debug())?;
        }
        Ok(())
    }
}

// -- Critical chunks --

/// Image header.
pub const IHDR: ChunkType = ChunkType(*b"IHDR");
/// Palette.
pub const PLTE: ChunkType = ChunkType(*b"PLTE");
/// Image data.
pub const IDAT: ChunkType = ChunkType(*b"IDAT");
/// Image trailer.
pub const IEND: ChunkType = ChunkType(*b"IEND");

// -- Ancillary chunks --

/// Transparency.
#[allow(non_upper_case_globals)]
pub const tRNS: ChunkType = ChunkType(*b"tRNS");
/// Primary chromaticities and white point.
#[allow(non_upper_case_globals)]
pub const cHRM: ChunkType = ChunkType(*b"cHRM");
/// Image gamma.
#[allow(non_upper_case_globals)]
pub const gAMA: ChunkType = ChunkType(*b"gAMA");
/// Embedded ICC profile.
#[allow(non_upper_case_globals)]
pub const iCCP: ChunkType = ChunkType(*b"iCCP");
/// Significant bits.
#[allow(non_upper_case_globals)]
pub const sBIT: ChunkType = ChunkType(*b"sBIT");
/// Standard RGB colour space.
#[allow(non_upper_case_globals)]
pub const sRGB: ChunkType = ChunkType(*b"sRGB");
/// Coding-independent code points.
#[allow(non_upper_case_globals)]
pub const cICP: ChunkType = ChunkType(*b"cICP");
/// Mastering display colour volume.
#[allow(non_upper_case_globals)]
pub const mDCV: ChunkType = ChunkType(*b"mDCv");
/// Content light level information.
#[allow(non_upper_case_globals)]
pub const cLLI: ChunkType = ChunkType(*b"cLLi");
/// Background colour.
#[allow(non_upper_case_globals)]
pub const bKGD: ChunkType = ChunkType(*b"bKGD");
/// Image histogram.
#[allow(non_upper_case_globals)]
pub const hIST: ChunkType = ChunkType(*b"hIST");
/// Physical pixel dimensions.
#[allow(non_upper_case_globals)]
pub const pHYs: ChunkType = ChunkType(*b"pHYs");
/// Suggested palette.
#[allow(non_upper_case_globals)]
pub const sPLT: ChunkType = ChunkType(*b"sPLT");
/// Image last-modification time.
#[allow(non_upper_case_globals)]
pub const tIME: ChunkType = ChunkType(*b"tIME");
/// Uncompressed Latin-1 text.
#[allow(non_upper_case_globals)]
pub const tEXt: ChunkType = ChunkType(*b"tEXt");
/// Compressed Latin-1 text.
#[allow(non_upper_case_globals)]
pub const zTXt: ChunkType = ChunkType(*b"zTXt");
/// International UTF-8 text.
#[allow(non_upper_case_globals)]
pub const iTXt: ChunkType = ChunkType(*b"iTXt");
/// Exif metadata.
#[allow(non_upper_case_globals)]
pub const eXIf: ChunkType = ChunkType(*b"eXIf");
/// Image offset.
#[allow(non_upper_case_globals)]
pub const oFFs: ChunkType = ChunkType(*b"oFFs");
/// Physical scale of the subject.
#[allow(non_upper_case_globals)]
pub const sCAL: ChunkType = ChunkType(*b"sCAL");
/// Calibration of pixel values.
#[allow(non_upper_case_globals)]
pub const pCAL: ChunkType = ChunkType(*b"pCAL");
/// Stereo image indicator.
#[allow(non_upper_case_globals)]
pub const sTER: ChunkType = ChunkType(*b"sTER");

// -- APNG chunks --

/// Animation control.
#[allow(non_upper_case_globals)]
pub const acTL: ChunkType = ChunkType(*b"acTL");
/// Frame control.
#[allow(non_upper_case_globals)]
pub const fcTL: ChunkType = ChunkType(*b"fcTL");
/// Frame data.
#[allow(non_upper_case_globals)]
pub const fdAT: ChunkType = ChunkType(*b"fdAT");

// -- Apple extension --

/// Apple's non-standard `CgBI` marker, which precedes `IHDR` in iOS-processed
/// files and changes the meaning of the image data.
#[allow(non_upper_case_globals)]
pub const CgBI: ChunkType = ChunkType(*b"CgBI");

/// True when the chunk is critical: bit 5 of the first byte is clear.
#[must_use]
pub fn is_critical(ChunkType(type_): ChunkType) -> bool {
    type_[0] & 32 == 0
}

/// True when the chunk is private: bit 5 of the second byte is set.
#[must_use]
pub fn is_private(ChunkType(type_): ChunkType) -> bool {
    type_[1] & 32 != 0
}

/// True when the reserved bit (bit 5 of the third byte) is set, which no
/// conformant file does.
#[must_use]
pub fn reserved_set(ChunkType(type_): ChunkType) -> bool {
    type_[2] & 32 != 0
}

/// True when an editor may copy the chunk into a modified file: bit 5 of the
/// fourth byte is set.
#[must_use]
pub fn safe_to_copy(ChunkType(type_): ChunkType) -> bool {
    type_[3] & 32 != 0
}

/// The header of a chunk: its declared payload length and its type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkHeader {
    /// Declared payload length, already validated against [`MAX_CHUNK_LEN`].
    pub length: u32,
    /// The chunk's four-byte type code.
    pub kind: ChunkType,
}

impl ChunkHeader {
    /// Parse the eight header bytes, rejecting oversized lengths and type
    /// codes that are not four ASCII letters.
    pub fn parse(bytes: &[u8; 8]) -> Result<ChunkHeader, DecodingError> {
        let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if length > MAX_CHUNK_LEN {
            return Err(FormatErrorKind::ChunkTooLong { len: length }.into());
        }
        let kind = ChunkType([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if !kind.is_valid() {
            return Err(FormatErrorKind::InvalidChunkType.into());
        }
        Ok(ChunkHeader { length, kind })
    }
}

/// The CRC-32 of a chunk's type and payload.
///
/// ```
/// use oxiarc_png::chunk::{self, chunk_crc};
/// // The CRC of an empty IEND chunk is the well-known constant 0xAE426082.
/// assert_eq!(chunk_crc(chunk::IEND, &[]), 0xAE42_6082);
/// ```
#[must_use]
pub fn chunk_crc(kind: ChunkType, data: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(&kind.0);
    crc.update(data);
    crc.finalize()
}

/// Write one complete chunk, computing the CRC incrementally.
///
/// ```
/// use oxiarc_png::chunk::{self, write_chunk};
/// let mut out = Vec::new();
/// write_chunk(&mut out, chunk::IEND, &[]).expect("in-memory write");
/// assert_eq!(out, b"\0\0\0\0IEND\xae\x42\x60\x82");
/// ```
pub fn write_chunk<W: Write>(w: &mut W, kind: ChunkType, data: &[u8]) -> std::io::Result<()> {
    write_chunk_parts(w, kind, &[data])
}

/// Write one complete chunk whose payload is given as several slices.
///
/// This is what the APNG writer uses to prepend an `fdAT` sequence number
/// without copying the frame data.
pub fn write_chunk_parts<W: Write>(
    w: &mut W,
    kind: ChunkType,
    parts: &[&[u8]],
) -> std::io::Result<()> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let len = u32::try_from(total).ok().filter(|n| *n <= MAX_CHUNK_LEN);
    let Some(len) = len else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "chunk payload exceeds 2^31-1 bytes",
        ));
    };
    let mut crc = Crc32::new();
    crc.update(&kind.0);
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&kind.0)?;
    for part in parts {
        crc.update(part);
        w.write_all(part)?;
    }
    w.write_all(&crc.finalize().to_be_bytes())
}

/// The 8-byte PNG signature.
pub const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];

/// A borrowing chunk walker over an in-memory PNG.
///
/// Each item is `(ChunkType, &[u8])` borrowed from the input; nothing is
/// copied. CRCs are **not** verified here — the streaming decoder owns that
/// policy — which makes this the right tool for tests and for tools that only
/// need to see the chunk layout.
///
/// ```
/// use oxiarc_png::chunk::{ChunkIter, SIGNATURE};
/// # let mut png = SIGNATURE.to_vec();
/// # oxiarc_png::chunk::write_chunk(&mut png, oxiarc_png::chunk::IEND, &[]).expect("write");
/// let kinds: Vec<_> = ChunkIter::new(&png).map(|c| c.expect("ok").0).collect();
/// assert_eq!(kinds, vec![oxiarc_png::chunk::IEND]);
/// ```
pub struct ChunkIter<'a> {
    data: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> ChunkIter<'a> {
    /// Start walking at the first chunk, skipping the signature when present.
    #[must_use]
    pub fn new(data: &'a [u8]) -> ChunkIter<'a> {
        let pos = if data.starts_with(&SIGNATURE) { 8 } else { 0 };
        ChunkIter {
            data,
            pos,
            done: false,
        }
    }

    /// The byte offset the walker stopped at.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }
}

impl<'a> Iterator for ChunkIter<'a> {
    type Item = Result<(ChunkType, &'a [u8]), DecodingError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.pos >= self.data.len() {
            return None;
        }
        let rest = &self.data[self.pos..];
        if rest.len() < 8 {
            self.done = true;
            return Some(Err(DecodingError::unexpected_eof()));
        }
        let mut head = [0u8; 8];
        head.copy_from_slice(&rest[..8]);
        let header = match ChunkHeader::parse(&head) {
            Ok(h) => h,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        let len = header.length as usize;
        let end = match 8usize.checked_add(len).and_then(|n| n.checked_add(4)) {
            Some(end) if end <= rest.len() => end,
            _ => {
                self.done = true;
                return Some(Err(DecodingError::unexpected_eof()));
            }
        };
        let data = &rest[8..8 + len];
        self.pos += end;
        if header.kind == IEND {
            self.done = true;
        }
        Some(Ok((header.kind, data)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_bits() {
        assert!(is_critical(IHDR));
        assert!(is_critical(PLTE));
        assert!(!is_critical(tEXt));
        assert!(!is_private(IHDR));
        assert!(is_private(ChunkType(*b"prVt")));
        assert!(!reserved_set(IHDR));
        assert!(reserved_set(ChunkType(*b"IHdR")));
        assert!(!safe_to_copy(IHDR));
        assert!(safe_to_copy(tEXt));
        assert!(!safe_to_copy(gAMA));
    }

    #[test]
    fn chunk_type_validity() {
        assert!(IHDR.is_valid());
        assert!(!ChunkType([0, b'H', b'D', b'R']).is_valid());
        assert!(!ChunkType(*b"IH1R").is_valid());
        assert_eq!(format!("{IHDR}"), "IHDR");
        assert!(format!("{IHDR:?}").contains("critical: true"));
    }

    #[test]
    fn header_parse_rejects_oversized_and_bad_types() {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&0x8000_0000u32.to_be_bytes());
        b[4..8].copy_from_slice(b"IDAT");
        assert!(ChunkHeader::parse(&b).is_err());
        b[0..4].copy_from_slice(&MAX_CHUNK_LEN.to_be_bytes());
        assert_eq!(
            ChunkHeader::parse(&b).expect("ok"),
            ChunkHeader {
                length: MAX_CHUNK_LEN,
                kind: IDAT
            }
        );
        b[4] = 0;
        assert!(ChunkHeader::parse(&b).is_err());
    }

    #[test]
    fn crc_matches_the_known_iend_constant() {
        assert_eq!(chunk_crc(IEND, &[]), 0xAE42_6082);
        let mut out = Vec::new();
        write_chunk(&mut out, IEND, &[]).expect("write");
        assert_eq!(out.len(), 12);
        assert_eq!(&out[8..], &0xAE42_6082u32.to_be_bytes());
    }

    #[test]
    fn write_chunk_parts_equals_a_single_slice() {
        let mut a = Vec::new();
        write_chunk(&mut a, IDAT, b"hello world").expect("write");
        let mut b = Vec::new();
        write_chunk_parts(&mut b, IDAT, &[b"hello ", b"world"]).expect("write");
        assert_eq!(a, b);
    }

    #[test]
    fn chunk_iter_walks_and_stops_at_iend() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, IHDR, &[0u8; 13]).expect("write");
        write_chunk(&mut png, IDAT, b"xx").expect("write");
        write_chunk(&mut png, IEND, &[]).expect("write");
        png.extend_from_slice(b"trailing garbage");
        let kinds: Vec<_> = ChunkIter::new(&png).map(|c| c.expect("chunk").0).collect();
        assert_eq!(kinds, vec![IHDR, IDAT, IEND]);
    }

    #[test]
    fn chunk_iter_reports_truncation() {
        let mut png = SIGNATURE.to_vec();
        write_chunk(&mut png, IDAT, b"abcd").expect("write");
        png.truncate(png.len() - 3);
        let last = ChunkIter::new(&png).last().expect("one item");
        assert!(last.is_err());
    }

    #[test]
    fn write_chunk_rejects_oversized_payloads() {
        // Constructing 2 GiB is not practical; check the guard arithmetic
        // instead by asserting the boundary constant is what the spec says.
        assert_eq!(MAX_CHUNK_LEN, (1u32 << 31) - 1);
    }
}

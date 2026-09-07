//! Classic TIFF and BigTIFF file headers.
//!
//! ```
//! use oxiarc_tiff::{Endian, Header, Variant};
//!
//! // "II", 42, first IFD at offset 8.
//! let bytes = [0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
//! let header = Header::parse(&bytes).expect("valid classic header");
//! assert_eq!(header.endian, Endian::Little);
//! assert_eq!(header.variant, Variant::Classic);
//! assert_eq!(header.first_ifd, 8);
//! ```

use std::io::Read;

use crate::byteorder::Endian;
use crate::error::{FormatError, Result, TiffError};

/// Magic bytes for a little-endian file.
pub const MAGIC_LE: [u8; 2] = [b'I', b'I'];
/// Magic bytes for a big-endian file.
pub const MAGIC_BE: [u8; 2] = [b'M', b'M'];
/// Version word of a classic TIFF.
pub const VERSION_CLASSIC: u16 = 42;
/// Version word of a BigTIFF.
pub const VERSION_BIG: u16 = 43;

/// Which of the two TIFF container shapes a file uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Variant {
    /// TIFF 6.0: 32-bit offsets, 12-byte IFD entries.
    Classic,
    /// BigTIFF: 64-bit offsets, 20-byte IFD entries.
    Big,
}

impl Variant {
    /// Width of a file offset in bytes (4 or 8).
    #[must_use]
    pub const fn offset_size(self) -> usize {
        match self {
            Self::Classic => 4,
            Self::Big => 8,
        }
    }

    /// Width of the file header in bytes (8 or 16).
    #[must_use]
    pub const fn header_size(self) -> usize {
        match self {
            Self::Classic => 8,
            Self::Big => 16,
        }
    }

    /// Width of one IFD entry in bytes (12 or 20).
    #[must_use]
    pub const fn entry_size(self) -> usize {
        match self {
            Self::Classic => 12,
            Self::Big => 20,
        }
    }

    /// Width of an IFD's entry-count field in bytes (2 or 8).
    #[must_use]
    pub const fn count_size(self) -> usize {
        match self {
            Self::Classic => 2,
            Self::Big => 8,
        }
    }

    /// How many value bytes fit inside an IFD entry (4 or 8).
    #[must_use]
    pub const fn inline_value_bytes(self) -> usize {
        self.offset_size()
    }

    /// The largest addressable file offset.
    #[must_use]
    pub const fn max_offset(self) -> u64 {
        match self {
            Self::Classic => u32::MAX as u64,
            Self::Big => u64::MAX,
        }
    }

    /// `true` for [`Variant::Big`].
    #[must_use]
    pub const fn is_big(self) -> bool {
        matches!(self, Self::Big)
    }

    /// The version word this variant writes.
    #[must_use]
    pub const fn version(self) -> u16 {
        match self {
            Self::Classic => VERSION_CLASSIC,
            Self::Big => VERSION_BIG,
        }
    }
}

/// A parsed TIFF file header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Byte order of every multi-byte field in the file.
    pub endian: Endian,
    /// Classic or BigTIFF.
    pub variant: Variant,
    /// Offset of the first IFD (0 means "no images").
    pub first_ifd: u64,
}

impl Header {
    /// Parses a header out of a byte slice.
    ///
    /// Accepts 8 bytes for a classic header and 16 for a BigTIFF one.
    ///
    /// # Errors
    /// * [`FormatError::HeaderTooShort`] when fewer bytes than the declared
    ///   variant requires are available;
    /// * [`FormatError::SignatureNotFound`] for a bad `II`/`MM` word;
    /// * [`FormatError::UnknownVersion`] for a version other than 42/43;
    /// * [`FormatError::InvalidBigTiffHeader`] when a BigTIFF header does not
    ///   declare an 8-byte offset size and a zero reserved word.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let magic = match (bytes.first(), bytes.get(1)) {
            (Some(a), Some(b)) => [*a, *b],
            _ => {
                return Err(TiffError::Format(FormatError::HeaderTooShort {
                    got: bytes.len(),
                }));
            }
        };
        let endian =
            Endian::from_magic(magic).ok_or(TiffError::Format(FormatError::SignatureNotFound))?;
        let version =
            endian
                .u16_at(bytes, 2)
                .ok_or(TiffError::Format(FormatError::HeaderTooShort {
                    got: bytes.len(),
                }))?;
        match version {
            VERSION_CLASSIC => {
                let first_ifd = endian.u32_at(bytes, 4).ok_or(TiffError::Format(
                    FormatError::HeaderTooShort { got: bytes.len() },
                ))?;
                Ok(Self {
                    endian,
                    variant: Variant::Classic,
                    first_ifd: u64::from(first_ifd),
                })
            }
            VERSION_BIG => {
                let offset_size = endian.u16_at(bytes, 4).ok_or(TiffError::Format(
                    FormatError::HeaderTooShort { got: bytes.len() },
                ))?;
                let reserved = endian.u16_at(bytes, 6).ok_or(TiffError::Format(
                    FormatError::HeaderTooShort { got: bytes.len() },
                ))?;
                if offset_size != 8 || reserved != 0 {
                    return Err(TiffError::Format(FormatError::InvalidBigTiffHeader {
                        offset_size,
                        reserved,
                    }));
                }
                let first_ifd = endian.u64_at(bytes, 8).ok_or(TiffError::Format(
                    FormatError::HeaderTooShort { got: bytes.len() },
                ))?;
                Ok(Self {
                    endian,
                    variant: Variant::Big,
                    first_ifd,
                })
            }
            other => Err(TiffError::Format(FormatError::UnknownVersion(other))),
        }
    }

    /// Reads a header from a stream, consuming exactly `header_size()` bytes.
    ///
    /// # Errors
    /// The same set as [`Self::parse`], plus I/O failures.
    pub fn read_from<R: Read>(reader: &mut R) -> Result<Self> {
        let mut prefix = [0u8; 8];
        reader.read_exact(&mut prefix)?;
        let magic = [prefix[0], prefix[1]];
        let endian =
            Endian::from_magic(magic).ok_or(TiffError::Format(FormatError::SignatureNotFound))?;
        let version = endian.u16([prefix[2], prefix[3]]);
        if version != VERSION_BIG {
            return Self::parse(&prefix);
        }
        let mut full = [0u8; 16];
        full[..8].copy_from_slice(&prefix);
        reader.read_exact(&mut full[8..])?;
        Self::parse(&full)
    }

    /// Serialises the header. Returns the buffer and the number of valid bytes.
    #[must_use]
    pub fn to_bytes(&self) -> ([u8; 16], usize) {
        let mut out = [0u8; 16];
        let magic = self.endian.magic();
        out[0] = magic[0];
        out[1] = magic[1];
        let version = self.endian.put_u16(self.variant.version());
        out[2] = version[0];
        out[3] = version[1];
        match self.variant {
            Variant::Classic => {
                let first = self
                    .endian
                    .put_u32(u32::try_from(self.first_ifd).unwrap_or(0));
                out[4..8].copy_from_slice(&first);
                (out, 8)
            }
            Variant::Big => {
                let size = self.endian.put_u16(8);
                out[4] = size[0];
                out[5] = size[1];
                out[6] = 0;
                out[7] = 0;
                let first = self.endian.put_u64(self.first_ifd);
                out[8..16].copy_from_slice(&first);
                (out, 16)
            }
        }
    }

    /// Offset of the header's first-IFD pointer field.
    #[must_use]
    pub const fn first_ifd_field_offset(&self) -> u64 {
        match self.variant {
            Variant::Classic => 4,
            Variant::Big => 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn variant_geometry_matches_the_spec() {
        assert_eq!(Variant::Classic.offset_size(), 4);
        assert_eq!(Variant::Classic.header_size(), 8);
        assert_eq!(Variant::Classic.entry_size(), 12);
        assert_eq!(Variant::Classic.count_size(), 2);
        assert_eq!(Variant::Classic.inline_value_bytes(), 4);
        assert_eq!(Variant::Classic.max_offset(), u64::from(u32::MAX));
        assert!(!Variant::Classic.is_big());
        assert_eq!(Variant::Big.offset_size(), 8);
        assert_eq!(Variant::Big.header_size(), 16);
        assert_eq!(Variant::Big.entry_size(), 20);
        assert_eq!(Variant::Big.count_size(), 8);
        assert_eq!(Variant::Big.inline_value_bytes(), 8);
        assert_eq!(Variant::Big.max_offset(), u64::MAX);
        assert!(Variant::Big.is_big());
    }

    #[test]
    fn classic_headers_parse_in_both_orders() {
        let le = [0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
        let header = Header::parse(&le).expect("little-endian classic");
        assert_eq!(header.endian, Endian::Little);
        assert_eq!(header.first_ifd, 8);
        let be = [0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x00, 0x00, 0x10];
        let header = Header::parse(&be).expect("big-endian classic");
        assert_eq!(header.endian, Endian::Big);
        assert_eq!(header.first_ifd, 16);
    }

    #[test]
    fn bigtiff_header_round_trips() {
        let header = Header {
            endian: Endian::Big,
            variant: Variant::Big,
            first_ifd: 0x1_0000_0000,
        };
        let (bytes, len) = header.to_bytes();
        assert_eq!(len, 16);
        let parsed = Header::parse(&bytes[..len]).expect("round trip");
        assert_eq!(parsed, header);
        assert_eq!(parsed.first_ifd_field_offset(), 8);
    }

    #[test]
    fn classic_header_round_trips() {
        let header = Header {
            endian: Endian::Little,
            variant: Variant::Classic,
            first_ifd: 4242,
        };
        let (bytes, len) = header.to_bytes();
        assert_eq!(len, 8);
        assert_eq!(Header::parse(&bytes[..len]).expect("round trip"), header);
        assert_eq!(header.first_ifd_field_offset(), 4);
    }

    #[test]
    fn bad_signature_is_rejected() {
        let err = Header::parse(b"XX\x2a\x00\x08\x00\x00\x00").expect_err("bad magic");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::SignatureNotFound)
        ));
    }

    #[test]
    fn short_headers_are_rejected_not_panicked_on() {
        let classic = [0x49u8, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00];
        for len in 0..8 {
            let err = Header::parse(&classic[..len]).expect_err("short classic header");
            assert!(
                matches!(err, TiffError::Format(FormatError::HeaderTooShort { .. })),
                "len {len}: {err}"
            );
        }
        let mut big = vec![0x4Du8, 0x4D, 0x00, 0x2B, 0x00, 0x08, 0x00, 0x00];
        big.extend_from_slice(&16u64.to_be_bytes());
        for len in 8..16 {
            let err = Header::parse(&big[..len]).expect_err("short bigtiff header");
            assert!(
                matches!(err, TiffError::Format(FormatError::HeaderTooShort { .. })),
                "len {len}: {err}"
            );
        }
        Header::parse(&big).expect("full bigtiff header");
    }

    #[test]
    fn unknown_version_is_named() {
        let err = Header::parse(&[0x49, 0x49, 0x2B, 0x00, 0, 0, 0, 0]).expect_err("version 43?");
        // 0x2B == 43 == BigTIFF, but the offset-size word is 0 -> invalid BigTIFF.
        assert!(matches!(
            err,
            TiffError::Format(FormatError::InvalidBigTiffHeader { .. })
        ));
        let err = Header::parse(&[0x49, 0x49, 0x2C, 0x00, 0, 0, 0, 0]).expect_err("version 44");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::UnknownVersion(44))
        ));
    }

    #[test]
    fn read_from_consumes_the_right_number_of_bytes() {
        let mut classic = Cursor::new(vec![
            0x49, 0x49, 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00, 0xFF, 0xFF,
        ]);
        let header = Header::read_from(&mut classic).expect("classic");
        assert_eq!(header.variant, Variant::Classic);
        assert_eq!(classic.position(), 8);

        let mut big = Vec::new();
        big.extend_from_slice(&[0x49, 0x49, 0x2B, 0x00, 0x08, 0x00, 0x00, 0x00]);
        big.extend_from_slice(&32u64.to_le_bytes());
        big.push(0xFF);
        let mut cursor = Cursor::new(big);
        let header = Header::read_from(&mut cursor).expect("bigtiff");
        assert_eq!(header.variant, Variant::Big);
        assert_eq!(header.first_ifd, 32);
        assert_eq!(cursor.position(), 16);
    }
}

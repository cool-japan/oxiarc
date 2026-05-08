//! Archive format auto-detection.
//!
//! This module provides automatic detection of archive formats based on
//! magic numbers (file signatures).

use oxiarc_core::error::Result;
use std::io::{Read, Seek, SeekFrom};

/// Known archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    /// ZIP archive (.zip).
    Zip,
    /// GZIP compressed file (.gz).
    Gzip,
    /// TAR archive (.tar).
    Tar,
    /// LZH/LHA archive (.lzh, .lha).
    Lzh,
    /// 7-Zip archive (.7z).
    SevenZip,
    /// XZ compressed file (.xz).
    Xz,
    /// Bzip2 compressed file (.bz2).
    Bzip2,
    /// Zstandard compressed file (.zst).
    Zstd,
    /// LZ4 compressed file (.lz4).
    Lz4,
    /// Microsoft Cabinet (.cab).
    Cab,
    /// Brotli compressed file (.br, .brotli).
    Brotli,
    /// Snappy compressed file (.sz, .snappy).
    Snappy,
    /// ISO 9660 CD/DVD image (.iso).
    Iso9660,
    /// Unknown format.
    Unknown,
}

impl ArchiveFormat {
    /// Detect format from magic bytes.
    pub fn from_magic(magic: &[u8]) -> Self {
        if magic.len() < 2 {
            return Self::Unknown;
        }

        // ZIP: 0x50 0x4B (PK)
        if magic.starts_with(&[0x50, 0x4B]) {
            // Check for specific ZIP signatures
            if magic.len() >= 4 {
                match &magic[2..4] {
                    [0x03, 0x04] => return Self::Zip, // Local file header
                    [0x05, 0x06] => return Self::Zip, // End of central directory
                    [0x07, 0x08] => return Self::Zip, // Data descriptor
                    _ => {}
                }
            }
            return Self::Zip;
        }

        // GZIP: 0x1F 0x8B
        if magic.starts_with(&[0x1F, 0x8B]) {
            return Self::Gzip;
        }

        // 7-Zip: 0x37 0x7A 0xBC 0xAF 0x27 0x1C
        if magic.len() >= 6 && magic.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
            return Self::SevenZip;
        }

        // XZ: 0xFD 0x37 0x7A 0x58 0x5A 0x00
        if magic.len() >= 6 && magic.starts_with(&[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]) {
            return Self::Xz;
        }

        // Bzip2: 0x42 0x5A 0x68 (BZh)
        if magic.len() >= 3 && magic.starts_with(&[0x42, 0x5A, 0x68]) {
            return Self::Bzip2;
        }

        // Zstandard: 0x28 0xB5 0x2F 0xFD
        if magic.len() >= 4 && magic.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            return Self::Zstd;
        }

        // LZ4: 0x04 0x22 0x4D 0x18 (standard frame) or our simple format 0x04 0x22 0x4D 0x18
        if magic.len() >= 4 && magic.starts_with(&[0x04, 0x22, 0x4D, 0x18]) {
            return Self::Lz4;
        }

        // Snappy framed: 0xFF 0x06 0x00 0x00 0x73 0x4E 0x61 0x50 0x70 0x59
        if magic.len() >= 10
            && magic.starts_with(&[0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59])
        {
            return Self::Snappy;
        }

        // CAB: "MSCF" (0x4D 0x53 0x43 0x46)
        if magic.len() >= 4 && magic.starts_with(b"MSCF") {
            return Self::Cab;
        }

        // LZH: Check for "-lh" or "-lz" at offset 2
        if magic.len() >= 7
            && magic[2] == b'-'
            && magic[3] == b'l'
            && (magic[4] == b'h' || magic[4] == b'z')
            && magic[6] == b'-'
        {
            return Self::Lzh;
        }

        // TAR: Check for "ustar" at offset 257
        // For initial detection, we check if it looks like a tar header
        if magic.len() >= 262 && &magic[257..262] == b"ustar" {
            return Self::Tar;
        }

        Self::Unknown
    }

    /// Detect format from a reader.
    ///
    /// Reads magic bytes from the current position. For ISO 9660, seeks to
    /// byte 32768 (LBA 16) to check the CD001 identifier.
    /// The reader is left at an unspecified position after this call; callers
    /// should seek back to 0 before further use.
    pub fn detect<R: Read + Seek>(reader: &mut R) -> Result<(Self, Vec<u8>)> {
        let mut magic = vec![0u8; 262]; // Enough for TAR detection
        let bytes_read = reader.read(&mut magic)?;
        magic.truncate(bytes_read);

        let format = Self::from_magic(&magic);
        if format != Self::Unknown {
            return Ok((format, magic));
        }

        // ISO 9660 detection: seek to byte 32768 (LBA 16) and check for CD001
        if let Ok(()) = reader.seek(SeekFrom::Start(32768)).map(|_| ()) {
            let mut iso_sig = [0u8; 6];
            if reader.read_exact(&mut iso_sig).is_ok() {
                // Byte 0 is VD type (1=Primary, 2=Supplementary, 3=Boot, 255=Terminator)
                // Bytes 1-5 must be "CD001"
                if &iso_sig[1..6] == b"CD001" && matches!(iso_sig[0], 1 | 2 | 3 | 255) {
                    return Ok((Self::Iso9660, magic));
                }
            }
        }

        Ok((Self::Unknown, magic))
    }

    /// Get the typical file extension.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Gzip => "gz",
            Self::Tar => "tar",
            Self::Lzh => "lzh",
            Self::SevenZip => "7z",
            Self::Xz => "xz",
            Self::Bzip2 => "bz2",
            Self::Zstd => "zst",
            Self::Lz4 => "lz4",
            Self::Cab => "cab",
            Self::Brotli => "br",
            Self::Snappy => "sz",
            Self::Iso9660 => "iso",
            Self::Unknown => "",
        }
    }

    /// Get the MIME type.
    pub fn mime_type(&self) -> &'static str {
        match self {
            Self::Zip => "application/zip",
            Self::Gzip => "application/gzip",
            Self::Tar => "application/x-tar",
            Self::Lzh => "application/x-lzh-compressed",
            Self::SevenZip => "application/x-7z-compressed",
            Self::Xz => "application/x-xz",
            Self::Bzip2 => "application/x-bzip2",
            Self::Zstd => "application/zstd",
            Self::Lz4 => "application/x-lz4",
            Self::Cab => "application/vnd.ms-cab-compressed",
            Self::Brotli => "application/x-brotli",
            Self::Snappy => "application/x-snappy",
            Self::Iso9660 => "application/x-iso9660-image",
            Self::Unknown => "application/octet-stream",
        }
    }

    /// Check if this is a compressed format (single file).
    pub fn is_compression_only(&self) -> bool {
        matches!(
            self,
            Self::Gzip
                | Self::Xz
                | Self::Bzip2
                | Self::Zstd
                | Self::Lz4
                | Self::Brotli
                | Self::Snappy
        )
    }

    /// Check if this is an archive format (multiple files).
    pub fn is_archive(&self) -> bool {
        matches!(
            self,
            Self::Zip | Self::Tar | Self::Lzh | Self::SevenZip | Self::Cab | Self::Iso9660
        )
    }
}

impl std::fmt::Display for ArchiveFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Zip => write!(f, "ZIP"),
            Self::Gzip => write!(f, "GZIP"),
            Self::Tar => write!(f, "TAR"),
            Self::Lzh => write!(f, "LZH"),
            Self::SevenZip => write!(f, "7-Zip"),
            Self::Xz => write!(f, "XZ"),
            Self::Bzip2 => write!(f, "Bzip2"),
            Self::Zstd => write!(f, "Zstandard"),
            Self::Lz4 => write!(f, "LZ4"),
            Self::Cab => write!(f, "Cabinet"),
            Self::Brotli => write!(f, "Brotli"),
            Self::Snappy => write!(f, "Snappy"),
            Self::Iso9660 => write!(f, "ISO 9660"),
            Self::Unknown => write!(f, "Unknown"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_zip() {
        let magic = [0x50, 0x4B, 0x03, 0x04];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Zip);
    }

    #[test]
    fn test_detect_gzip() {
        let magic = [0x1F, 0x8B, 0x08, 0x00];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Gzip);
    }

    #[test]
    fn test_detect_lzh() {
        // LZH header starts at byte 2: "-lh5-"
        let magic = [0x00, 0x00, b'-', b'l', b'h', b'5', b'-'];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Lzh);
    }

    #[test]
    fn test_detect_7z() {
        let magic = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::SevenZip);
    }

    #[test]
    fn test_detect_lz4() {
        // LZ4 frame magic: 0x184D2204 (little-endian)
        let magic = [0x04, 0x22, 0x4D, 0x18];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Lz4);
    }

    #[test]
    fn test_detect_zstd() {
        // Zstandard magic: 0xFD2FB528 (little-endian)
        let magic = [0x28, 0xB5, 0x2F, 0xFD];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Zstd);
    }

    #[test]
    fn test_detect_cab() {
        // CAB magic: "MSCF"
        let magic = [0x4D, 0x53, 0x43, 0x46];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Cab);
    }

    #[test]
    fn test_detect_snappy() {
        let magic = [0xFF, 0x06, 0x00, 0x00, 0x73, 0x4E, 0x61, 0x50, 0x70, 0x59];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Snappy);
    }

    #[test]
    fn test_brotli_properties() {
        assert!(ArchiveFormat::Brotli.is_compression_only());
        assert!(!ArchiveFormat::Brotli.is_archive());
        assert_eq!(ArchiveFormat::Brotli.extension(), "br");
        assert_eq!(ArchiveFormat::Brotli.mime_type(), "application/x-brotli");
    }

    #[test]
    fn test_snappy_properties() {
        assert!(ArchiveFormat::Snappy.is_compression_only());
        assert!(!ArchiveFormat::Snappy.is_archive());
        assert_eq!(ArchiveFormat::Snappy.extension(), "sz");
        assert_eq!(ArchiveFormat::Snappy.mime_type(), "application/x-snappy");
    }

    #[test]
    fn test_detect_unknown() {
        let magic = [0x00, 0x00, 0x00, 0x00];
        assert_eq!(ArchiveFormat::from_magic(&magic), ArchiveFormat::Unknown);
    }

    #[test]
    fn test_format_properties() {
        assert!(ArchiveFormat::Zip.is_archive());
        assert!(!ArchiveFormat::Zip.is_compression_only());
        assert!(ArchiveFormat::Gzip.is_compression_only());
        assert!(!ArchiveFormat::Gzip.is_archive());
        assert!(ArchiveFormat::Lz4.is_compression_only());
        assert!(!ArchiveFormat::Lz4.is_archive());
        assert!(ArchiveFormat::Cab.is_archive());
        assert!(!ArchiveFormat::Cab.is_compression_only());
    }

    #[test]
    fn test_detect_iso9660() {
        use std::io::Cursor;
        // Build a minimal buffer with CD001 at byte 32768 (LBA 16)
        let mut data = vec![0u8; 32768 + 6];
        data[32768] = 1; // PVD type
        data[32769..32774].copy_from_slice(b"CD001");
        let mut cursor = Cursor::new(data);
        let (format, _) = ArchiveFormat::detect(&mut cursor).expect("detect failed");
        assert_eq!(format, ArchiveFormat::Iso9660);
    }

    #[test]
    fn test_iso9660_properties() {
        assert!(ArchiveFormat::Iso9660.is_archive());
        assert!(!ArchiveFormat::Iso9660.is_compression_only());
        assert_eq!(ArchiveFormat::Iso9660.extension(), "iso");
        assert_eq!(
            ArchiveFormat::Iso9660.mime_type(),
            "application/x-iso9660-image"
        );
    }
}

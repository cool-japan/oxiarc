//! CAB file header structures.
//!
//! This module defines the low-level structures for parsing CAB files:
//! - CFHEADER: Main cabinet header
//! - CFFOLDER: Folder compression settings
//! - CFFILE: File metadata

use oxiarc_core::{OxiArcError, Result};
use std::io::Read;
use std::time::SystemTime;

/// CAB magic number: "MSCF"
pub const MAGIC: [u8; 4] = *b"MSCF";

/// Header flag: Previous cabinet present
pub const FLAG_PREV_CABINET: u16 = 0x0001;
/// Header flag: Next cabinet present
pub const FLAG_NEXT_CABINET: u16 = 0x0002;
/// Header flag: Reserve fields present
pub const FLAG_RESERVE_PRESENT: u16 = 0x0004;

/// File attribute: Read-only
const ATTR_READONLY: u16 = 0x01;
/// File attribute: Hidden
const ATTR_HIDDEN: u16 = 0x02;
/// File attribute: System
const ATTR_SYSTEM: u16 = 0x04;
/// File attribute: UTF-8 name encoding
const ATTR_NAME_IS_UTF: u16 = 0x80;

/// Maximum length accepted for any null-terminated string in a CAB header
/// (CFFILE names, prev/next cabinet names and disk labels).
///
/// The CAB specification limits `szName` in CFFILE to 256 bytes; 4096 is
/// generous headroom for non-conformant writers while still preventing an
/// attacker-controlled name with no NUL terminator from forcing an
/// unbounded byte-at-a-time read of the remaining file.
const MAX_NULL_STRING_LEN: usize = 4096;

/// Compression type enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionType {
    /// No compression (stored)
    None,
    /// MSZIP (Deflate-based)
    MsZip,
    /// Quantum compression
    Quantum,
    /// LZX compression with window size parameter
    Lzx(u8),
    /// Unrecognized compression method code.
    ///
    /// Carried through so extraction can report the offending code instead
    /// of silently treating the folder as stored.
    Unknown(u16),
}

impl CompressionType {
    /// Parse compression type from u16 value.
    ///
    /// Method codes outside the specified set `{0, 1, 2, 3}` map to
    /// [`CompressionType::Unknown`] and are rejected at extraction time,
    /// matching the treatment of the recognized-but-unimplemented Quantum
    /// and LZX methods (never a silent raw-copy fallback).
    pub fn from_u16(value: u16) -> Self {
        let method = value & 0x00FF;
        let param = ((value >> 8) & 0x1F) as u8;

        match method {
            0 => CompressionType::None,
            1 => CompressionType::MsZip,
            2 => CompressionType::Quantum,
            3 => CompressionType::Lzx(param),
            _ => CompressionType::Unknown(method),
        }
    }
}

/// CFHEADER - Cabinet file header.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CabHeader {
    /// Total cabinet file size
    pub cabinet_size: u32,
    /// Offset of first CFFILE entry
    pub files_offset: u32,
    /// Format version (minor)
    pub version_minor: u8,
    /// Format version (major)
    pub version_major: u8,
    /// Number of folder entries
    pub num_folders: u16,
    /// Number of file entries
    pub num_files: u16,
    /// Header flags
    pub flags: u16,
    /// Set identifier
    pub set_id: u16,
    /// Cabinet index in set
    pub cabinet_index: u16,
    /// Size of per-cabinet reserved area
    pub header_reserve_size: u16,
    /// Size of per-folder reserved area
    pub folder_reserve_size: u8,
    /// Size of per-data reserved area
    pub data_reserve_size: u8,
    /// Previous cabinet filename (if FLAG_PREV_CABINET)
    pub prev_cabinet: Option<String>,
    /// Previous disk name (if FLAG_PREV_CABINET)
    pub prev_disk: Option<String>,
    /// Next cabinet filename (if FLAG_NEXT_CABINET)
    pub next_cabinet: Option<String>,
    /// Next disk name (if FLAG_NEXT_CABINET)
    pub next_disk: Option<String>,
}

impl CabHeader {
    /// Read a CFHEADER from the input.
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        // Read fixed header (36 bytes minimum)
        let mut buf = [0u8; 36];
        reader.read_exact(&mut buf)?;

        // Verify magic
        if buf[0..4] != MAGIC {
            return Err(OxiArcError::invalid_magic(
                MAGIC.to_vec(),
                buf[0..4].to_vec(),
            ));
        }

        let cabinet_size = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let files_offset = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let version_minor = buf[24];
        let version_major = buf[25];
        let num_folders = u16::from_le_bytes([buf[26], buf[27]]);
        let num_files = u16::from_le_bytes([buf[28], buf[29]]);
        let flags = u16::from_le_bytes([buf[30], buf[31]]);
        let set_id = u16::from_le_bytes([buf[32], buf[33]]);
        let cabinet_index = u16::from_le_bytes([buf[34], buf[35]]);

        // Check version
        if version_major != 1 || version_minor != 3 {
            // Most CABs are version 1.3, but accept others
        }

        // Read optional reserved area sizes
        let (header_reserve_size, folder_reserve_size, data_reserve_size) =
            if flags & FLAG_RESERVE_PRESENT != 0 {
                let mut reserve_buf = [0u8; 4];
                reader.read_exact(&mut reserve_buf)?;
                (
                    u16::from_le_bytes([reserve_buf[0], reserve_buf[1]]),
                    reserve_buf[2],
                    reserve_buf[3],
                )
            } else {
                (0, 0, 0)
            };

        // Skip header reserved area
        if header_reserve_size > 0 {
            let mut skip = vec![0u8; header_reserve_size as usize];
            reader.read_exact(&mut skip)?;
        }

        // Read optional cabinet spanning info
        let (prev_cabinet, prev_disk) = if flags & FLAG_PREV_CABINET != 0 {
            (
                Some(read_null_string(reader)?),
                Some(read_null_string(reader)?),
            )
        } else {
            (None, None)
        };

        let (next_cabinet, next_disk) = if flags & FLAG_NEXT_CABINET != 0 {
            (
                Some(read_null_string(reader)?),
                Some(read_null_string(reader)?),
            )
        } else {
            (None, None)
        };

        Ok(Self {
            cabinet_size,
            files_offset,
            version_minor,
            version_major,
            num_folders,
            num_files,
            flags,
            set_id,
            cabinet_index,
            header_reserve_size,
            folder_reserve_size,
            data_reserve_size,
            prev_cabinet,
            prev_disk,
            next_cabinet,
            next_disk,
        })
    }
}

/// CFFOLDER - Folder entry.
#[derive(Debug, Clone)]
pub struct CabFolder {
    /// Offset of first CFDATA block
    pub data_offset: u32,
    /// Number of CFDATA blocks
    pub num_data_blocks: u16,
    /// Compression type
    pub compression_type: CompressionType,
}

impl CabFolder {
    /// Read a CFFOLDER from the input.
    pub fn read<R: Read>(reader: &mut R, reserve_size: u8) -> Result<Self> {
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf)?;

        let data_offset = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let num_data_blocks = u16::from_le_bytes([buf[4], buf[5]]);
        let type_compress = u16::from_le_bytes([buf[6], buf[7]]);

        // Skip reserved area
        if reserve_size > 0 {
            let mut skip = vec![0u8; reserve_size as usize];
            reader.read_exact(&mut skip)?;
        }

        Ok(Self {
            data_offset,
            num_data_blocks,
            compression_type: CompressionType::from_u16(type_compress),
        })
    }
}

/// CFFILE - File entry.
#[derive(Debug, Clone)]
pub struct CabFile {
    /// Uncompressed file size
    pub uncompressed_size: u32,
    /// Offset within uncompressed folder
    pub folder_offset: u32,
    /// Folder index
    pub folder_index: u16,
    /// DOS date
    pub date: u16,
    /// DOS time
    pub time: u16,
    /// File attributes
    pub attributes: u16,
    /// Filename
    pub name: String,
}

impl CabFile {
    /// Read a CFFILE from the input.
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 16];
        reader.read_exact(&mut buf)?;

        let uncompressed_size = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let folder_offset = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let folder_index = u16::from_le_bytes([buf[8], buf[9]]);
        let date = u16::from_le_bytes([buf[10], buf[11]]);
        let time = u16::from_le_bytes([buf[12], buf[13]]);
        let attributes = u16::from_le_bytes([buf[14], buf[15]]);

        // Read null-terminated filename
        let name_bytes = read_null_bytes(reader)?;

        // Decode filename (UTF-8 if flag set, otherwise Windows-1252)
        let name = if attributes & ATTR_NAME_IS_UTF != 0 {
            String::from_utf8_lossy(&name_bytes).into_owned()
        } else {
            // Try UTF-8 first, fallback to lossy
            String::from_utf8(name_bytes.clone())
                .unwrap_or_else(|_| String::from_utf8_lossy(&name_bytes).into_owned())
        };

        // Normalize path separators
        let name = name.replace('\\', "/");

        Ok(Self {
            uncompressed_size,
            folder_offset,
            folder_index,
            date,
            time,
            attributes,
            name,
        })
    }

    /// Check if file is readonly.
    pub fn is_readonly(&self) -> bool {
        self.attributes & ATTR_READONLY != 0
    }

    /// Check if file is hidden.
    pub fn is_hidden(&self) -> bool {
        self.attributes & ATTR_HIDDEN != 0
    }

    /// Check if file is a system file.
    pub fn is_system(&self) -> bool {
        self.attributes & ATTR_SYSTEM != 0
    }

    /// Check if entry is a directory placeholder.
    ///
    /// The CAB format has no real directory entries — directories are
    /// implied by path separators in file names. Some writers nevertheless
    /// emit zero-length entries whose name ends with a separator as
    /// placeholders; those are the only entries reported as directories.
    /// (`ATTR_ARCHIVE` is the DOS archive bit, not a directory marker, and
    /// is deliberately not consulted here.)
    pub fn is_directory(&self) -> bool {
        self.uncompressed_size == 0 && self.name.ends_with('/')
    }

    /// Get the modification time.
    pub fn modified_time(&self) -> Option<SystemTime> {
        dos_datetime_to_systime(self.date, self.time)
    }
}

/// Read a null-terminated string.
fn read_null_string<R: Read>(reader: &mut R) -> Result<String> {
    let bytes = read_null_bytes(reader)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Read null-terminated bytes, capped at [`MAX_NULL_STRING_LEN`].
///
/// The length is attacker-controlled (bounded only by a NUL byte in the
/// input), so refuse to buffer more than the cap: a CFFILE name with no
/// terminator must fail with a corruption error instead of slurping the
/// rest of the file into memory one byte at a time.
fn read_null_bytes<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 1];

    loop {
        reader.read_exact(&mut buf)?;
        if buf[0] == 0 {
            break;
        }
        if bytes.len() >= MAX_NULL_STRING_LEN {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "CAB string exceeds {} bytes without a NUL terminator",
                    MAX_NULL_STRING_LEN
                ),
            ));
        }
        bytes.push(buf[0]);
    }

    Ok(bytes)
}

/// Convert DOS date/time to SystemTime.
fn dos_datetime_to_systime(date: u16, time: u16) -> Option<SystemTime> {
    // DOS date: bits 0-4 = day, 5-8 = month, 9-15 = year since 1980
    let day = (date & 0x1F) as u32;
    let month = ((date >> 5) & 0x0F) as u32;
    let year = ((date >> 9) & 0x7F) as u32 + 1980;

    // DOS time: bits 0-4 = seconds/2, 5-10 = minutes, 11-15 = hours
    let seconds = ((time & 0x1F) as u32) * 2;
    let minutes = ((time >> 5) & 0x3F) as u32;
    let hours = ((time >> 11) & 0x1F) as u32;

    // Validate ranges
    if day == 0 || day > 31 || month == 0 || month > 12 {
        return None;
    }
    if hours > 23 || minutes > 59 || seconds > 59 {
        return None;
    }

    // Calculate Unix timestamp
    // Days since Unix epoch (Jan 1, 1970) to the start of the year
    let mut days = 0i64;

    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }

    // Days from start of year to start of month
    let month_days = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    days += month_days[month as usize - 1] as i64;

    // Add leap day if past February in a leap year
    if month > 2 && is_leap_year(year) {
        days += 1;
    }

    // Add day of month (1-indexed)
    days += (day - 1) as i64;

    let secs = days * 86400 + (hours as i64) * 3600 + (minutes as i64) * 60 + seconds as i64;

    if secs < 0 {
        return None;
    }

    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// Check if a year is a leap year.
fn is_leap_year(year: u32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_type() {
        assert_eq!(CompressionType::from_u16(0x0000), CompressionType::None);
        assert_eq!(CompressionType::from_u16(0x0001), CompressionType::MsZip);
        assert_eq!(CompressionType::from_u16(0x0002), CompressionType::Quantum);
        assert_eq!(CompressionType::from_u16(0x0F03), CompressionType::Lzx(15));
    }

    #[test]
    fn test_compression_type_unknown_codes_not_treated_as_stored() {
        // Any method code outside {0,1,2,3} must surface as Unknown, never
        // silently as stored/None (CAB-04 regression).
        for code in [4u16, 5, 0x7F, 0xFF] {
            assert_eq!(
                CompressionType::from_u16(code),
                CompressionType::Unknown(code),
                "method code {code:#06x} must map to Unknown"
            );
        }
        // High parameter bits must not rescue an unknown low-byte method.
        assert_eq!(
            CompressionType::from_u16(0x1F04),
            CompressionType::Unknown(4)
        );
    }

    #[test]
    fn test_read_null_bytes_caps_unterminated_names() {
        use std::io::Cursor;

        // A "name" longer than the cap with no NUL terminator must error
        // out rather than buffering the whole stream (CAB-05 regression).
        let unterminated = vec![b'A'; MAX_NULL_STRING_LEN + 16];
        let err = read_null_bytes(&mut Cursor::new(&unterminated));
        assert!(
            err.is_err(),
            "unterminated over-cap string must be an error"
        );

        // At the cap with a terminator is still accepted.
        let mut ok_data = vec![b'B'; MAX_NULL_STRING_LEN];
        ok_data.push(0);
        let ok = read_null_bytes(&mut Cursor::new(&ok_data)).expect("cap-length string");
        assert_eq!(ok.len(), MAX_NULL_STRING_LEN);
    }

    #[test]
    fn test_is_directory_placeholder_semantics() {
        let mk = |name: &str, size: u32, attrs: u16| CabFile {
            uncompressed_size: size,
            folder_offset: 0,
            folder_index: 0,
            date: 0,
            time: 0,
            attributes: attrs,
            name: name.to_string(),
        };

        // Zero-length trailing-slash placeholder is a directory regardless
        // of the DOS archive bit (CAB-07: bit 0x20 is not a dir marker).
        assert!(mk("nested/", 0, 0).is_directory());
        assert!(mk("nested/", 0, 0x20).is_directory());
        // Regular files are never directories.
        assert!(!mk("nested/file.bin", 10, 0x20).is_directory());
        assert!(!mk("file.txt", 0, 0).is_directory());
    }

    #[test]
    fn test_dos_datetime() {
        // Test a known date: 2024-01-15 12:30:00
        // Year: 2024 - 1980 = 44 = 0x2C
        // Month: 1 = 0x01
        // Day: 15 = 0x0F
        // date = (44 << 9) | (1 << 5) | 15 = 0x582F
        let date = 0x582F;

        // Hours: 12 = 0x0C
        // Minutes: 30 = 0x1E
        // Seconds: 0/2 = 0
        // time = (12 << 11) | (30 << 5) | 0 = 0x63C0
        let time = 0x63C0;

        let systime = dos_datetime_to_systime(date, time);
        assert!(systime.is_some());
    }

    #[test]
    fn test_file_attributes() {
        let file = CabFile {
            uncompressed_size: 100,
            folder_offset: 0,
            folder_index: 0,
            date: 0,
            time: 0,
            attributes: ATTR_READONLY | ATTR_HIDDEN,
            name: "test.txt".to_string(),
        };

        assert!(file.is_readonly());
        assert!(file.is_hidden());
        assert!(!file.is_system());
        assert!(!file.is_directory());
    }
}

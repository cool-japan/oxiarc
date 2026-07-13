//! ZIP header types, constants, and core structures.

use super::super::crypto::FLAG_ENCRYPTED;
use super::super::encryption::AesExtraField;
use oxiarc_core::entry::CompressionMethod as CoreMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Entry, EntryType, FileAttributes};
use std::io::{Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// ZIP local file header signature.
pub const LOCAL_FILE_HEADER_SIG: u32 = 0x04034B50;

/// ZIP central directory header signature.
pub const CENTRAL_DIR_HEADER_SIG: u32 = 0x02014B50;

/// ZIP end of central directory signature.
pub const END_OF_CENTRAL_DIR_SIG: u32 = 0x06054B50;

/// ZIP64 end of central directory signature.
pub const ZIP64_END_OF_CENTRAL_DIR_SIG: u32 = 0x06064B50;

/// ZIP64 end of central directory locator signature.
pub const ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG: u32 = 0x07064B50;

/// ZIP64 extra field header ID.
pub const ZIP64_EXTRA_FIELD_ID: u16 = 0x0001;

/// Marker value for Zip64 (0xFFFFFFFF for 32-bit fields).
pub const ZIP64_MARKER_32: u32 = 0xFFFF_FFFF;

/// Marker value for Zip64 (0xFFFF for 16-bit fields).
pub const ZIP64_MARKER_16: u16 = 0xFFFF;

/// Data descriptor signature (optional, PK\x07\x08).
pub const DATA_DESCRIPTOR_SIG: u32 = 0x08074B50;

/// Flag bit for data descriptor presence.
pub const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;

/// Flag bit for the language encoding flag (EFS, general-purpose bit 11):
/// when set, the entry name and comment are encoded in UTF-8.
pub const FLAG_EFS: u16 = 0x0800;

/// AES encryption method value in ZIP (compression method field).
pub const METHOD_AES_ENCRYPTED: u16 = 99;

/// Private extra-field ID used by the *reader* to persist per-entry ZIP
/// header metadata (the general-purpose bit flags and the raw DOS mtime
/// word) on [`Entry::extra`].
///
/// `Entry` (in `oxiarc-core`) has no dedicated field for the ZIP
/// general-purpose bit flags, yet encryption detection (APPNOTE §4.4.4,
/// bit 0) and the ZipCrypto check-byte rule for streamed entries
/// (bit 3 → DOS mtime high byte) both need them after header parsing.
/// The reader therefore appends one well-formed extra-field record
/// (`id(2) size(2)=4 flags(2) dos_mtime(2)`, all little-endian) when it
/// builds each `Entry`. The record is never written to disk.
pub(crate) const OXIARC_ENTRY_META_ID: u16 = 0x4F58;

/// Append the private per-entry metadata record (see
/// [`OXIARC_ENTRY_META_ID`]) to an entry's extra bytes.
pub(crate) fn append_entry_meta(extra: &mut Vec<u8>, flags: u16, dos_mtime: u16) {
    extra.extend_from_slice(&OXIARC_ENTRY_META_ID.to_le_bytes());
    extra.extend_from_slice(&4u16.to_le_bytes());
    extra.extend_from_slice(&flags.to_le_bytes());
    extra.extend_from_slice(&dos_mtime.to_le_bytes());
}

/// Find the payload of the *last* extra-field record with the given ID.
///
/// The last match is preferred because the reader appends its private
/// metadata record after any on-disk extra data, so a crafted archive
/// cannot shadow it with a forged record of the same ID.
fn find_last_extra_record(extra: &[u8], wanted: u16) -> Option<&[u8]> {
    let mut found = None;
    let mut offset = 0;
    while offset + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
        let size = u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]) as usize;
        offset += 4;
        if offset + size > extra.len() {
            break;
        }
        if id == wanted {
            found = Some(&extra[offset..offset + size]);
        }
        offset += size;
    }
    found
}

/// The ZIP general-purpose bit flags persisted on an entry by the reader,
/// if present (see [`OXIARC_ENTRY_META_ID`]).
pub(crate) fn entry_gp_flags(entry: &Entry) -> Option<u16> {
    find_last_extra_record(&entry.extra, OXIARC_ENTRY_META_ID)
        .filter(|data| data.len() >= 2)
        .map(|data| u16::from_le_bytes([data[0], data[1]]))
}

/// The raw DOS modification-time word persisted on an entry by the reader,
/// if present (see [`OXIARC_ENTRY_META_ID`]).
pub(crate) fn entry_dos_mtime(entry: &Entry) -> Option<u16> {
    find_last_extra_record(&entry.extra, OXIARC_ENTRY_META_ID)
        .filter(|data| data.len() >= 4)
        .map(|data| u16::from_le_bytes([data[2], data[3]]))
}

// =============================================================================
// DOS date/time conversion (single shared implementation)
// =============================================================================
//
// Both directions use the proleptic-Gregorian civil-date algorithms from
// Howard Hinnant's "chrono-compatible low-level date algorithms" so leap
// years and real month lengths are handled exactly. Earlier versions kept
// two divergent approximate copies (365-day years, 30-day months) which
// drifted by weeks and could panic on a crafted month-0 DOS date (ZIP-02)
// or emit an out-of-range month 13 (ZIP-05).

/// Days from the Unix epoch (1970-01-01) to the given civil date.
///
/// `month` is 1-12 and `day` 1-31; the caller clamps out-of-range values.
fn days_from_civil(year: i64, month: u64, day: u64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let mp = if month > 2 { month - 3 } else { month + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe as i64 - 719_468
}

/// Civil date (year, month 1-12, day 1-31) for a day count relative to the
/// Unix epoch.
fn civil_from_days(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Convert a DOS (date, time) word pair to a `SystemTime`.
///
/// Malformed month/day fields (a crafted header can encode 0, or month up
/// to 15) are clamped into the valid 1-12 / 1-31 range so a hostile
/// archive yields an approximate timestamp instead of a panic (debug) or
/// a wrapped ~2^64-second offset (release).
pub(crate) fn dos_date_time_to_system_time(mdate: u16, mtime: u16) -> SystemTime {
    let seconds = (mtime & 0x1F) as u64 * 2;
    let minutes = ((mtime >> 5) & 0x3F) as u64;
    let hours = ((mtime >> 11) & 0x1F) as u64;
    let day = ((mdate & 0x1F) as u64).clamp(1, 31);
    let month = (((mdate >> 5) & 0x0F) as u64).clamp(1, 12);
    let year = ((mdate >> 9) & 0x7F) as i64 + 1980;

    // DOS years span 1980-2107, so the day count is always positive.
    let days = days_from_civil(year, month, day).max(0) as u64;
    let total_seconds = days * 86400 + hours * 3600 + minutes * 60 + seconds;

    UNIX_EPOCH + Duration::from_secs(total_seconds)
}

/// Convert seconds since the Unix epoch to a DOS (mtime, mdate) word pair.
///
/// Times before the DOS epoch clamp to 1980-01-01 and times beyond the
/// DOS range clamp to 2107-12-31 (the extremes representable in the
/// 7-bit year field), so the emitted date is always well-formed.
pub(crate) fn dos_date_time_from_unix_secs(secs: u64) -> (u16, u16) {
    let time_of_day = secs % 86400;
    let hours = (time_of_day / 3600) as u16;
    let minutes = ((time_of_day % 3600) / 60) as u16;
    let seconds = ((time_of_day % 60) / 2) as u16; // 2-second granularity
    let mtime = (hours << 11) | (minutes << 5) | seconds;

    let (year, month, day) = civil_from_days((secs / 86400) as i64);
    let mdate = if year < 1980 {
        // Before the DOS epoch: clamp to 1980-01-01.
        (1 << 5) | 1
    } else if year > 2107 {
        // Beyond the 7-bit year field: clamp to 2107-12-31.
        (127 << 9) | (12 << 5) | 31
    } else {
        (((year - 1980) as u16) << 9) | ((month as u16) << 5) | day as u16
    };

    (mtime, mdate)
}

/// ZIP compression methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompressionMethod {
    /// Stored (no compression).
    Stored,
    /// Deflate compression.
    Deflate,
    /// LZMA (method 14) as specified in APPNOTE §5.8.8.
    Lzma,
    /// Unknown method.
    Unknown(u16),
}

impl CompressionMethod {
    /// Create from a u16 value.
    pub fn from_u16(value: u16) -> Self {
        match value {
            0 => Self::Stored,
            8 => Self::Deflate,
            14 => Self::Lzma,
            _ => Self::Unknown(value),
        }
    }

    /// Convert to core compression method.
    pub fn to_core(&self) -> CoreMethod {
        match self {
            Self::Stored => CoreMethod::Stored,
            Self::Deflate => CoreMethod::Deflate,
            Self::Lzma => CoreMethod::Lzma,
            Self::Unknown(id) => CoreMethod::Unknown(*id),
        }
    }

    /// Convert from core compression method.
    pub fn from_core(m: &CoreMethod) -> Self {
        match m {
            CoreMethod::Stored => Self::Stored,
            CoreMethod::Deflate => Self::Deflate,
            CoreMethod::Lzma => Self::Lzma,
            CoreMethod::Unknown(id) => Self::Unknown(*id),
            _ => Self::Unknown(0),
        }
    }

    /// Convert to u16 wire value.
    pub fn to_u16(self) -> u16 {
        match self {
            Self::Stored => 0,
            Self::Deflate => 8,
            Self::Lzma => 14,
            Self::Unknown(id) => id,
        }
    }
}

impl std::fmt::Display for CompressionMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stored => write!(f, "Stored"),
            Self::Deflate => write!(f, "Deflate"),
            Self::Lzma => write!(f, "LZMA"),
            Self::Unknown(id) => write!(f, "Unknown({})", id),
        }
    }
}

/// ZIP local file header.
#[derive(Debug, Clone)]
pub struct LocalFileHeader {
    /// Minimum version needed to extract.
    pub version_needed: u16,
    /// General purpose bit flag.
    pub flags: u16,
    /// Compression method.
    pub method: CompressionMethod,
    /// Last modification time.
    pub mtime: u16,
    /// Last modification date.
    pub mdate: u16,
    /// CRC-32 of uncompressed data.
    pub crc32: u32,
    /// Compressed size (use compressed_size_64 for actual value if Zip64).
    pub compressed_size: u32,
    /// Uncompressed size (use uncompressed_size_64 for actual value if Zip64).
    pub uncompressed_size: u32,
    /// File name, decoded from [`LocalFileHeader::filename_raw`]
    /// (strict UTF-8, then Shift_JIS when the EFS flag is absent, then
    /// an injective CP437 fallback).
    pub filename: String,
    /// Raw (undecoded) file-name bytes exactly as stored in the header.
    pub filename_raw: Vec<u8>,
    /// Extra field.
    pub extra: Vec<u8>,
    /// Offset to file data.
    pub data_offset: u64,
    /// Zip64 uncompressed size (if present in extra field).
    pub uncompressed_size_64: Option<u64>,
    /// Zip64 compressed size (if present in extra field).
    pub compressed_size_64: Option<u64>,
}

impl LocalFileHeader {
    /// Read a local file header.
    pub fn read<R: Read>(reader: &mut R) -> Result<Self> {
        let mut buf = [0u8; 30];
        reader.read_exact(&mut buf)?;

        let signature = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if signature != LOCAL_FILE_HEADER_SIG {
            return Err(OxiArcError::invalid_magic(
                LOCAL_FILE_HEADER_SIG.to_le_bytes().to_vec(),
                signature.to_le_bytes().to_vec(),
            ));
        }

        let version_needed = u16::from_le_bytes([buf[4], buf[5]]);
        let flags = u16::from_le_bytes([buf[6], buf[7]]);
        let method = CompressionMethod::from_u16(u16::from_le_bytes([buf[8], buf[9]]));
        let mtime = u16::from_le_bytes([buf[10], buf[11]]);
        let mdate = u16::from_le_bytes([buf[12], buf[13]]);
        let crc32 = u32::from_le_bytes([buf[14], buf[15], buf[16], buf[17]]);
        let compressed_size = u32::from_le_bytes([buf[18], buf[19], buf[20], buf[21]]);
        let uncompressed_size = u32::from_le_bytes([buf[22], buf[23], buf[24], buf[25]]);
        let filename_len = u16::from_le_bytes([buf[26], buf[27]]) as usize;
        let extra_len = u16::from_le_bytes([buf[28], buf[29]]) as usize;

        // Read filename and decode it (EFS flag: UTF-8; otherwise strict
        // UTF-8 -> Shift_JIS -> injective CP437 fallback).
        let mut filename_bytes = vec![0u8; filename_len];
        reader.read_exact(&mut filename_bytes)?;
        let filename =
            crate::zip::name_codec::decode_zip_text(&filename_bytes, flags & FLAG_EFS != 0);

        // Read extra field
        let mut extra = vec![0u8; extra_len];
        reader.read_exact(&mut extra)?;

        // Parse Zip64 extra field if sizes are 0xFFFFFFFF
        let (uncompressed_size_64, compressed_size_64) =
            if uncompressed_size == ZIP64_MARKER_32 || compressed_size == ZIP64_MARKER_32 {
                Self::parse_zip64_extra(&extra, uncompressed_size, compressed_size)
            } else {
                (None, None)
            };

        Ok(Self {
            version_needed,
            flags,
            method,
            mtime,
            mdate,
            crc32,
            compressed_size,
            uncompressed_size,
            filename,
            filename_raw: filename_bytes,
            extra,
            data_offset: 0, // Set by caller
            uncompressed_size_64,
            compressed_size_64,
        })
    }

    /// Parse Zip64 extended information extra field.
    pub fn parse_zip64_extra(
        extra: &[u8],
        uncompressed_size: u32,
        compressed_size: u32,
    ) -> (Option<u64>, Option<u64>) {
        let mut offset = 0;
        while offset + 4 <= extra.len() {
            let header_id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
            let data_size = u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]) as usize;
            offset += 4;

            if header_id == ZIP64_EXTRA_FIELD_ID && offset + data_size <= extra.len() {
                let mut field_offset = offset;
                let mut uncompressed_64 = None;
                let mut compressed_64 = None;

                // Order: uncompressed size, compressed size, relative header offset, disk start
                // Only present if corresponding field in header is 0xFFFFFFFF
                if uncompressed_size == ZIP64_MARKER_32 && field_offset + 8 <= offset + data_size {
                    uncompressed_64 = Some(u64::from_le_bytes([
                        extra[field_offset],
                        extra[field_offset + 1],
                        extra[field_offset + 2],
                        extra[field_offset + 3],
                        extra[field_offset + 4],
                        extra[field_offset + 5],
                        extra[field_offset + 6],
                        extra[field_offset + 7],
                    ]));
                    field_offset += 8;
                }

                if compressed_size == ZIP64_MARKER_32 && field_offset + 8 <= offset + data_size {
                    compressed_64 = Some(u64::from_le_bytes([
                        extra[field_offset],
                        extra[field_offset + 1],
                        extra[field_offset + 2],
                        extra[field_offset + 3],
                        extra[field_offset + 4],
                        extra[field_offset + 5],
                        extra[field_offset + 6],
                        extra[field_offset + 7],
                    ]));
                }

                return (uncompressed_64, compressed_64);
            }

            offset += data_size;
        }

        (None, None)
    }

    /// Convert DOS date/time to SystemTime.
    ///
    /// Delegates to the shared `dos_date_time_to_system_time` helper so
    /// the reader and this header type cannot drift apart (a duplicated
    /// copy of this math previously panicked on a crafted month-0 date).
    pub fn modified_time(&self) -> SystemTime {
        dos_date_time_to_system_time(self.mdate, self.mtime)
    }

    /// Convert to Entry.
    pub fn to_entry(&self) -> Entry {
        let entry_type = if self.filename.ends_with('/') {
            EntryType::Directory
        } else {
            EntryType::File
        };

        // Use Zip64 sizes if present
        let size = self
            .uncompressed_size_64
            .unwrap_or(self.uncompressed_size as u64);
        let compressed_size = self
            .compressed_size_64
            .unwrap_or(self.compressed_size as u64);

        // Persist the general-purpose bit flags and raw DOS mtime on the
        // entry so encryption detection can use flag bit 0 (APPNOTE) and
        // the ZipCrypto check-byte rule can use the mtime high byte.
        let mut extra = self.extra.clone();
        append_entry_meta(&mut extra, self.flags, self.mtime);

        Entry {
            name: self.filename.clone(),
            entry_type,
            size,
            compressed_size,
            method: self.method.to_core(),
            modified: Some(self.modified_time()),
            created: None,
            accessed: None,
            attributes: FileAttributes::default(),
            crc32: Some(self.crc32),
            comment: None,
            link_target: None,
            offset: self.data_offset,
            extra,
        }
    }

    /// Get the actual uncompressed size (respecting Zip64).
    pub fn actual_uncompressed_size(&self) -> u64 {
        self.uncompressed_size_64
            .unwrap_or(self.uncompressed_size as u64)
    }

    /// Get the actual compressed size (respecting Zip64).
    pub fn actual_compressed_size(&self) -> u64 {
        self.compressed_size_64
            .unwrap_or(self.compressed_size as u64)
    }

    /// Check if this entry has a data descriptor following the compressed data.
    pub fn has_data_descriptor(&self) -> bool {
        self.flags & FLAG_DATA_DESCRIPTOR != 0
    }
}

/// ZIP data descriptor (appears after compressed data when FLAG_DATA_DESCRIPTOR is set).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct DataDescriptor {
    /// CRC-32 of uncompressed data.
    pub crc32: u32,
    /// Compressed size.
    pub compressed_size: u64,
    /// Uncompressed size.
    pub uncompressed_size: u64,
}

impl DataDescriptor {
    /// Read a data descriptor.
    /// The descriptor may optionally start with a signature (0x08074B50).
    /// Returns (descriptor, bytes_consumed).
    pub fn read<R: Read>(reader: &mut R, is_zip64: bool) -> Result<(Self, usize)> {
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf)?;

        let first_word = u32::from_le_bytes(buf);
        let mut bytes_consumed = 4;

        // Check if this is the optional signature
        let crc32 = if first_word == DATA_DESCRIPTOR_SIG {
            // Signature present, read CRC32
            reader.read_exact(&mut buf)?;
            bytes_consumed += 4;
            u32::from_le_bytes(buf)
        } else {
            // No signature, first word is CRC32
            first_word
        };

        let (compressed_size, uncompressed_size) = if is_zip64 {
            // Zip64: 8-byte sizes
            let mut buf64 = [0u8; 8];
            reader.read_exact(&mut buf64)?;
            let compressed = u64::from_le_bytes(buf64);
            reader.read_exact(&mut buf64)?;
            let uncompressed = u64::from_le_bytes(buf64);
            bytes_consumed += 16;
            (compressed, uncompressed)
        } else {
            // Standard: 4-byte sizes
            reader.read_exact(&mut buf)?;
            let compressed = u32::from_le_bytes(buf) as u64;
            reader.read_exact(&mut buf)?;
            let uncompressed = u32::from_le_bytes(buf) as u64;
            bytes_consumed += 8;
            (compressed, uncompressed)
        };

        Ok((
            Self {
                crc32,
                compressed_size,
                uncompressed_size,
            },
            bytes_consumed,
        ))
    }
}

// =============================================================================
// Encryption Helper Functions (standalone, not associated with ZipReader)
// =============================================================================

/// Check if an entry is encrypted (any encryption type).
///
/// Detection follows APPNOTE §4.4.4: general-purpose bit flag bit 0 marks
/// an encrypted entry (any scheme), and compression method 99 marks WinZip
/// AES. The flags are persisted on [`Entry::extra`] by the reader (see
/// `OXIARC_ENTRY_META_ID`), so archives produced by external tools
/// (`zip -e`, 7-Zip, WinRAR, Python `zipfile`) are detected too — not just
/// archives oxiarc itself wrote.
#[allow(dead_code)]
pub fn is_entry_encrypted(entry: &Entry) -> bool {
    entry_gp_flags(entry).is_some_and(|flags| flags & FLAG_ENCRYPTED != 0)
        || entry.method == CoreMethod::Unknown(METHOD_AES_ENCRYPTED)
}

/// Check if an entry is encrypted with AES (WinZip AE-2).
///
/// Returns `Some(AesExtraField)` if AES-encrypted, `None` otherwise.
#[allow(dead_code)]
pub fn get_entry_aes_encryption_info(entry: &Entry) -> Option<AesExtraField> {
    // Check if method is AES
    if entry.method == CoreMethod::Unknown(METHOD_AES_ENCRYPTED) {
        AesExtraField::find_in_extra(&entry.extra)
    } else {
        None
    }
}

/// Check if an entry uses traditional PKWARE encryption.
///
/// Returns `true` if the entry uses ZipCrypto (traditional) encryption:
/// general-purpose bit 0 is set and the method is not AES (99).
#[allow(dead_code)]
pub fn is_entry_traditional_encrypted(entry: &Entry) -> bool {
    entry_gp_flags(entry).is_some_and(|flags| flags & FLAG_ENCRYPTED != 0)
        && entry.method != CoreMethod::Unknown(METHOD_AES_ENCRYPTED)
}

/// ZIP compression level for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ZipCompressionLevel {
    /// Store without compression (method 0).
    Store,
    /// Fast compression (deflate level 1).
    Fast,
    /// Normal compression (deflate level 6).
    #[default]
    Normal,
    /// Best compression (deflate level 9).
    Best,
}

/// Central directory entry for ZIP writing.
#[derive(Debug, Clone)]
pub struct CentralDirEntry {
    /// Version made by.
    pub version_made_by: u16,
    /// Version needed to extract.
    pub version_needed: u16,
    /// General purpose bit flag.
    pub flags: u16,
    /// Compression method.
    pub method: u16,
    /// Last modification time.
    pub mtime: u16,
    /// Last modification date.
    pub mdate: u16,
    /// CRC-32 of uncompressed data.
    pub crc32: u32,
    /// Compressed size (64-bit for Zip64).
    pub compressed_size: u64,
    /// Uncompressed size (64-bit for Zip64).
    pub uncompressed_size: u64,
    /// File name.
    pub filename: String,
    /// Extra field (not including Zip64 extra).
    pub extra: Vec<u8>,
    /// File comment.
    pub comment: String,
    /// Disk number start.
    pub disk_start: u16,
    /// Internal file attributes.
    pub internal_attr: u16,
    /// External file attributes.
    pub external_attr: u32,
    /// Relative offset of local header (64-bit for Zip64).
    pub local_header_offset: u64,
}

impl CentralDirEntry {
    /// Check if this entry requires Zip64.
    pub fn needs_zip64(&self) -> bool {
        self.compressed_size >= ZIP64_MARKER_32 as u64
            || self.uncompressed_size >= ZIP64_MARKER_32 as u64
            || self.local_header_offset >= ZIP64_MARKER_32 as u64
    }

    /// Build Zip64 extra field if needed.
    pub fn build_zip64_extra(&self) -> Vec<u8> {
        if !self.needs_zip64() {
            return Vec::new();
        }

        let mut extra = Vec::with_capacity(32);
        // Header ID
        extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());

        // Calculate data size
        let mut data_size = 0u16;
        if self.uncompressed_size >= ZIP64_MARKER_32 as u64 {
            data_size += 8;
        }
        if self.compressed_size >= ZIP64_MARKER_32 as u64 {
            data_size += 8;
        }
        if self.local_header_offset >= ZIP64_MARKER_32 as u64 {
            data_size += 8;
        }
        extra.extend_from_slice(&data_size.to_le_bytes());

        // Add values in order
        if self.uncompressed_size >= ZIP64_MARKER_32 as u64 {
            extra.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        }
        if self.compressed_size >= ZIP64_MARKER_32 as u64 {
            extra.extend_from_slice(&self.compressed_size.to_le_bytes());
        }
        if self.local_header_offset >= ZIP64_MARKER_32 as u64 {
            extra.extend_from_slice(&self.local_header_offset.to_le_bytes());
        }

        extra
    }

    /// Write the central directory entry.
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        let filename_bytes = self.filename.as_bytes();
        let comment_bytes = self.comment.as_bytes();

        // Build Zip64 extra field if needed
        let zip64_extra = self.build_zip64_extra();
        let total_extra_len = self.extra.len() + zip64_extra.len();

        // Use marker values for Zip64 fields
        let compressed_size_32 = if self.compressed_size >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            self.compressed_size as u32
        };
        let uncompressed_size_32 = if self.uncompressed_size >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            self.uncompressed_size as u32
        };
        let local_header_offset_32 = if self.local_header_offset >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            self.local_header_offset as u32
        };

        // Version needed: 45 for Zip64, otherwise original
        let version_needed = if self.needs_zip64() {
            45
        } else {
            self.version_needed
        };

        // Signature
        writer.write_all(&CENTRAL_DIR_HEADER_SIG.to_le_bytes())?;
        // Version made by
        writer.write_all(&self.version_made_by.to_le_bytes())?;
        // Version needed
        writer.write_all(&version_needed.to_le_bytes())?;
        // Flags
        writer.write_all(&self.flags.to_le_bytes())?;
        // Compression method
        writer.write_all(&self.method.to_le_bytes())?;
        // Modification time
        writer.write_all(&self.mtime.to_le_bytes())?;
        // Modification date
        writer.write_all(&self.mdate.to_le_bytes())?;
        // CRC-32
        writer.write_all(&self.crc32.to_le_bytes())?;
        // Compressed size
        writer.write_all(&compressed_size_32.to_le_bytes())?;
        // Uncompressed size
        writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        // Filename length
        writer.write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        // Extra field length
        writer.write_all(&(total_extra_len as u16).to_le_bytes())?;
        // Comment length
        writer.write_all(&(comment_bytes.len() as u16).to_le_bytes())?;
        // Disk number start
        writer.write_all(&self.disk_start.to_le_bytes())?;
        // Internal file attributes
        writer.write_all(&self.internal_attr.to_le_bytes())?;
        // External file attributes
        writer.write_all(&self.external_attr.to_le_bytes())?;
        // Relative offset of local header
        writer.write_all(&local_header_offset_32.to_le_bytes())?;
        // Filename
        writer.write_all(filename_bytes)?;
        // Zip64 extra field (if needed)
        writer.write_all(&zip64_extra)?;
        // Other extra fields
        writer.write_all(&self.extra)?;
        // Comment
        writer.write_all(comment_bytes)?;

        Ok(())
    }

    /// Get the size of this entry when written.
    pub fn written_size(&self) -> usize {
        let zip64_extra = self.build_zip64_extra();
        46 + self.filename.len() + self.extra.len() + zip64_extra.len() + self.comment.len()
    }
}

#[cfg(test)]
mod date_tests {
    use super::*;

    fn header_with_date(mdate: u16, mtime: u16) -> LocalFileHeader {
        LocalFileHeader {
            version_needed: 20,
            flags: 0,
            method: CompressionMethod::Stored,
            mtime,
            mdate,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            filename: "f".to_string(),
            filename_raw: b"f".to_vec(),
            extra: Vec::new(),
            data_offset: 0,
            uncompressed_size_64: None,
            compressed_size_64: None,
        }
    }

    #[test]
    fn modified_time_does_not_panic_on_zero_dos_date() {
        // A crafted local-file header with a zero DOS date encodes month 0 and
        // day 0, which used to underflow `month - 1` and panic (debug) / wrap
        // (release). It must now yield a valid SystemTime and a usable Entry.
        let header = header_with_date(0, 0);
        let _ = header.modified_time();
        let _ = header.to_entry();
    }

    #[test]
    fn modified_time_matches_known_date() {
        // 2021-06-15 00:00:00 UTC (year 41 since 1980, month 6, day 15)
        // is exactly 1623715200 seconds after the Unix epoch.
        let mdate = ((2021 - 1980) << 9) | (6 << 5) | 15;
        let header = header_with_date(mdate, 0);
        let t = header.modified_time();
        let secs = t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        assert_eq!(
            secs, 1_623_715_200,
            "civil-date DOS conversion must be exact"
        );
    }

    #[test]
    fn dos_conversion_round_trips_exactly() {
        // Round-trip a spread of known instants through both shared
        // helpers; the 2-second DOS granularity truncates the seconds.
        for &secs in &[
            315_532_800u64, // 1980-01-01 00:00:00 (DOS epoch)
            951_827_696,    // 2000-02-29 12:34:56 (leap day)
            1_783_900_800,  // 2026-07-13 00:00:00
            4_102_444_798,  // 2099-12-31 23:59:58
        ] {
            let (mtime, mdate) = dos_date_time_from_unix_secs(secs);
            let t = dos_date_time_to_system_time(mdate, mtime);
            let round = t
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            assert_eq!(round, secs - secs % 2, "round-trip mismatch for {secs}");
        }
    }

    #[test]
    fn dos_conversion_never_emits_invalid_month_or_day() {
        // The old 365/30-day approximation could emit month 13 near the
        // end of a year. Sweep a year of daily timestamps and check the
        // packed fields stay in range.
        let start = 1_735_689_600u64; // 2025-01-01 00:00:00 UTC
        for day in 0..730u64 {
            let (_, mdate) = dos_date_time_from_unix_secs(start + day * 86400);
            let month = (mdate >> 5) & 0x0F;
            let dom = mdate & 0x1F;
            assert!((1..=12).contains(&month), "invalid month {month}");
            assert!((1..=31).contains(&dom), "invalid day {dom}");
        }
    }

    #[test]
    fn entry_meta_record_round_trips_flags_and_mtime() {
        let mut header = header_with_date(0x5ACF, 0xB43D);
        header.flags = FLAG_ENCRYPTED | FLAG_DATA_DESCRIPTOR;
        let entry = header.to_entry();
        assert_eq!(
            entry_gp_flags(&entry),
            Some(FLAG_ENCRYPTED | FLAG_DATA_DESCRIPTOR)
        );
        assert_eq!(entry_dos_mtime(&entry), Some(0xB43D));
        assert!(is_entry_encrypted(&entry));
        assert!(is_entry_traditional_encrypted(&entry));
    }

    #[test]
    fn stray_ee_bytes_in_extra_do_not_mark_entry_encrypted() {
        // Regression for the removed homegrown 0xEE,0xEE marker: an extra
        // field that merely *contains* those bytes (here inside a
        // well-formed record of another ID) must not flag encryption.
        let mut header = header_with_date(0x21, 0);
        header.extra = vec![0x55, 0x54, 0x02, 0x00, 0xEE, 0xEE];
        let entry = header.to_entry();
        assert!(!is_entry_encrypted(&entry));
        assert!(!is_entry_traditional_encrypted(&entry));
    }
}

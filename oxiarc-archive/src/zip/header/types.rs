//! ZIP header types, constants, and core structures.

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

/// AES encryption method value in ZIP (compression method field).
pub const METHOD_AES_ENCRYPTED: u16 = 99;

/// ZIP compression methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// File name.
    pub filename: String,
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

        // Read filename
        let mut filename_bytes = vec![0u8; filename_len];
        reader.read_exact(&mut filename_bytes)?;
        let filename = String::from_utf8_lossy(&filename_bytes).into_owned();

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
    pub fn modified_time(&self) -> SystemTime {
        let seconds = (self.mtime & 0x1F) as u64 * 2;
        let minutes = ((self.mtime >> 5) & 0x3F) as u64;
        let hours = ((self.mtime >> 11) & 0x1F) as u64;
        let day = (self.mdate & 0x1F) as u64;
        let month = ((self.mdate >> 5) & 0x0F) as u64;
        let year = ((self.mdate >> 9) & 0x7F) as u64 + 1980;

        // Approximate: Days since Unix epoch
        let days = (year - 1970) * 365 + (year - 1969) / 4 + (month - 1) * 30 + day;
        let total_seconds = days * 86400 + hours * 3600 + minutes * 60 + seconds;

        UNIX_EPOCH + Duration::from_secs(total_seconds)
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
            extra: self.extra.clone(),
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
/// This checks for the encryption marker in the entry's extra field
/// or for the AES encryption method.
#[allow(dead_code)]
pub fn is_entry_encrypted(entry: &Entry) -> bool {
    // Check if the encryption marker exists in extra field
    // OR if the compression method indicates AES encryption
    entry.extra.windows(2).any(|w| w == [0xEE, 0xEE])
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
/// Returns `true` if the entry uses ZipCrypto (traditional) encryption.
#[allow(dead_code)]
pub fn is_entry_traditional_encrypted(entry: &Entry) -> bool {
    // Traditional encryption uses the 0xEE marker but not AES method
    entry.extra.windows(2).any(|w| w == [0xEE, 0xEE])
        && entry.method != CoreMethod::Unknown(METHOD_AES_ENCRYPTED)
}

/// ZIP compression level for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

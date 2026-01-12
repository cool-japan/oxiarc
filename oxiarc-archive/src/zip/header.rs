//! ZIP header structures.

use oxiarc_core::entry::CompressionMethod as CoreMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Crc32, Entry, EntryType, FileAttributes};
use oxiarc_deflate::{deflate, inflate};
use std::io::{Read, Seek, SeekFrom, Write};
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

/// ZIP compression methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMethod {
    /// Stored (no compression).
    Stored,
    /// Deflate compression.
    Deflate,
    /// Unknown method.
    Unknown(u16),
}

impl CompressionMethod {
    /// Create from a u16 value.
    pub fn from_u16(value: u16) -> Self {
        match value {
            0 => Self::Stored,
            8 => Self::Deflate,
            _ => Self::Unknown(value),
        }
    }

    /// Convert to core compression method.
    pub fn to_core(&self) -> CoreMethod {
        match self {
            Self::Stored => CoreMethod::Stored,
            Self::Deflate => CoreMethod::Deflate,
            Self::Unknown(id) => CoreMethod::Unknown(*id),
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
    fn parse_zip64_extra(
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

/// ZIP archive reader.
pub struct ZipReader<R: Read + Seek> {
    reader: R,
    entries: Vec<Entry>,
}

impl<R: Read + Seek> ZipReader<R> {
    /// Create a new ZIP reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let entries = Self::read_entries(&mut reader)?;
        Ok(Self { reader, entries })
    }

    /// Read all entries from the archive.
    /// Uses the central directory for accurate metadata (handles data descriptors).
    fn read_entries(reader: &mut R) -> Result<Vec<Entry>> {
        // Try to find and read from central directory first
        if let Ok(entries) = Self::read_from_central_directory(reader) {
            return Ok(entries);
        }

        // Fall back to scanning local headers
        Self::read_from_local_headers(reader)
    }

    /// Read entries from the central directory (preferred method).
    fn read_from_central_directory(reader: &mut R) -> Result<Vec<Entry>> {
        // Find end of central directory record
        let file_size = reader.seek(SeekFrom::End(0))?;

        // Search for EOCD signature (max comment is 65535 bytes)
        let search_start = file_size.saturating_sub(65535 + 22);
        reader.seek(SeekFrom::Start(search_start))?;

        let mut buf = vec![0u8; (file_size - search_start) as usize];
        reader.read_exact(&mut buf)?;

        // Find EOCD signature (backwards)
        let eocd_sig = END_OF_CENTRAL_DIR_SIG.to_le_bytes();
        let eocd_offset = buf
            .windows(4)
            .rposition(|w| w == eocd_sig)
            .ok_or_else(|| OxiArcError::invalid_header("End of central directory not found"))?;

        let eocd_pos = search_start + eocd_offset as u64;

        // Check for Zip64 EOCD locator
        let (cd_offset, cd_size, total_entries) = if eocd_pos >= 20 {
            reader.seek(SeekFrom::Start(eocd_pos - 20))?;
            let mut locator_buf = [0u8; 20];
            reader.read_exact(&mut locator_buf)?;

            let locator_sig = u32::from_le_bytes([
                locator_buf[0],
                locator_buf[1],
                locator_buf[2],
                locator_buf[3],
            ]);

            if locator_sig == ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG {
                // Zip64 EOCD locator found
                let zip64_eocd_offset = u64::from_le_bytes([
                    locator_buf[8],
                    locator_buf[9],
                    locator_buf[10],
                    locator_buf[11],
                    locator_buf[12],
                    locator_buf[13],
                    locator_buf[14],
                    locator_buf[15],
                ]);

                // Read Zip64 EOCD
                reader.seek(SeekFrom::Start(zip64_eocd_offset))?;
                let mut zip64_eocd = [0u8; 56];
                reader.read_exact(&mut zip64_eocd)?;

                let entries_count = u64::from_le_bytes([
                    zip64_eocd[32],
                    zip64_eocd[33],
                    zip64_eocd[34],
                    zip64_eocd[35],
                    zip64_eocd[36],
                    zip64_eocd[37],
                    zip64_eocd[38],
                    zip64_eocd[39],
                ]);

                let cd_size_64 = u64::from_le_bytes([
                    zip64_eocd[40],
                    zip64_eocd[41],
                    zip64_eocd[42],
                    zip64_eocd[43],
                    zip64_eocd[44],
                    zip64_eocd[45],
                    zip64_eocd[46],
                    zip64_eocd[47],
                ]);

                let cd_offset_64 = u64::from_le_bytes([
                    zip64_eocd[48],
                    zip64_eocd[49],
                    zip64_eocd[50],
                    zip64_eocd[51],
                    zip64_eocd[52],
                    zip64_eocd[53],
                    zip64_eocd[54],
                    zip64_eocd[55],
                ]);

                (cd_offset_64, cd_size_64, entries_count)
            } else {
                // Standard EOCD
                Self::parse_standard_eocd(&buf[eocd_offset..])?
            }
        } else {
            Self::parse_standard_eocd(&buf[eocd_offset..])?
        };

        // Read central directory entries
        reader.seek(SeekFrom::Start(cd_offset))?;
        let mut entries = Vec::with_capacity(total_entries as usize);

        for _ in 0..total_entries {
            let entry = Self::read_central_dir_entry(reader)?;
            entries.push(entry);
        }

        // Validate we consumed the expected amount
        let _expected_end = cd_offset + cd_size;

        Ok(entries)
    }

    /// Parse standard EOCD record.
    fn parse_standard_eocd(buf: &[u8]) -> Result<(u64, u64, u64)> {
        if buf.len() < 22 {
            return Err(OxiArcError::invalid_header("EOCD too short"));
        }

        let total_entries = u16::from_le_bytes([buf[10], buf[11]]) as u64;
        let cd_size = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]) as u64;
        let cd_offset = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]) as u64;

        Ok((cd_offset, cd_size, total_entries))
    }

    /// Read a single central directory entry.
    fn read_central_dir_entry(reader: &mut R) -> Result<Entry> {
        let mut buf = [0u8; 46];
        reader.read_exact(&mut buf)?;

        let signature = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if signature != CENTRAL_DIR_HEADER_SIG {
            return Err(OxiArcError::invalid_magic(
                CENTRAL_DIR_HEADER_SIG.to_le_bytes().to_vec(),
                signature.to_le_bytes().to_vec(),
            ));
        }

        let flags = u16::from_le_bytes([buf[8], buf[9]]);
        let method = CompressionMethod::from_u16(u16::from_le_bytes([buf[10], buf[11]]));
        let mtime = u16::from_le_bytes([buf[12], buf[13]]);
        let mdate = u16::from_le_bytes([buf[14], buf[15]]);
        let crc32 = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let compressed_size = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        let uncompressed_size = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
        let filename_len = u16::from_le_bytes([buf[28], buf[29]]) as usize;
        let extra_len = u16::from_le_bytes([buf[30], buf[31]]) as usize;
        let comment_len = u16::from_le_bytes([buf[32], buf[33]]) as usize;
        let local_header_offset = u32::from_le_bytes([buf[42], buf[43], buf[44], buf[45]]);

        // Read variable-length fields
        let mut filename_bytes = vec![0u8; filename_len];
        reader.read_exact(&mut filename_bytes)?;
        let filename = String::from_utf8_lossy(&filename_bytes).into_owned();

        let mut extra = vec![0u8; extra_len];
        reader.read_exact(&mut extra)?;

        let mut comment_bytes = vec![0u8; comment_len];
        reader.read_exact(&mut comment_bytes)?;
        let comment = String::from_utf8_lossy(&comment_bytes).into_owned();

        // Parse Zip64 extra field if needed
        let mut uncompressed_size_64 = None;
        let mut compressed_size_64 = None;
        let mut local_header_offset_64 = None;

        if uncompressed_size == ZIP64_MARKER_32
            || compressed_size == ZIP64_MARKER_32
            || local_header_offset == ZIP64_MARKER_32
        {
            let mut offset = 0;
            while offset + 4 <= extra.len() {
                let header_id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
                let data_size = u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]) as usize;
                offset += 4;

                if header_id == ZIP64_EXTRA_FIELD_ID && offset + data_size <= extra.len() {
                    let mut field_offset = offset;

                    if uncompressed_size == ZIP64_MARKER_32
                        && field_offset + 8 <= offset + data_size
                    {
                        uncompressed_size_64 = Some(u64::from_le_bytes([
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

                    if compressed_size == ZIP64_MARKER_32 && field_offset + 8 <= offset + data_size
                    {
                        compressed_size_64 = Some(u64::from_le_bytes([
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

                    if local_header_offset == ZIP64_MARKER_32
                        && field_offset + 8 <= offset + data_size
                    {
                        local_header_offset_64 = Some(u64::from_le_bytes([
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

                    break;
                }

                offset += data_size;
            }
        }

        // Calculate actual sizes and offset
        let actual_uncompressed = uncompressed_size_64.unwrap_or(uncompressed_size as u64);
        let actual_compressed = compressed_size_64.unwrap_or(compressed_size as u64);
        let actual_header_offset = local_header_offset_64.unwrap_or(local_header_offset as u64);

        // Calculate data offset by reading local header length
        // Local header: 30 bytes fixed + filename_len + extra_len
        // We need to peek at the local header's extra field length (may differ from central)
        let current_pos = reader.stream_position()?;
        reader.seek(SeekFrom::Start(actual_header_offset + 26))?;
        let mut local_lens = [0u8; 4];
        reader.read_exact(&mut local_lens)?;
        let local_filename_len = u16::from_le_bytes([local_lens[0], local_lens[1]]) as u64;
        let local_extra_len = u16::from_le_bytes([local_lens[2], local_lens[3]]) as u64;
        let data_offset = actual_header_offset + 30 + local_filename_len + local_extra_len;
        reader.seek(SeekFrom::Start(current_pos))?;

        let entry_type = if filename.ends_with('/') {
            EntryType::Directory
        } else {
            EntryType::File
        };

        // Convert DOS time to SystemTime
        let seconds = (mtime & 0x1F) as u64 * 2;
        let minutes = ((mtime >> 5) & 0x3F) as u64;
        let hours = ((mtime >> 11) & 0x1F) as u64;
        let day = (mdate & 0x1F) as u64;
        let month = ((mdate >> 5) & 0x0F) as u64;
        let year = ((mdate >> 9) & 0x7F) as u64 + 1980;
        let days = (year - 1970) * 365 + (year - 1969) / 4 + (month - 1) * 30 + day;
        let total_seconds = days * 86400 + hours * 3600 + minutes * 60 + seconds;
        let modified = UNIX_EPOCH + Duration::from_secs(total_seconds);

        // Mark entries with data descriptors in the extra data
        let mut entry_extra = extra.clone();
        if flags & FLAG_DATA_DESCRIPTOR != 0 {
            // Add a marker so we know this entry used a data descriptor
            entry_extra.extend_from_slice(&[0xDD, 0xDD]); // Custom marker
        }

        Ok(Entry {
            name: filename,
            entry_type,
            size: actual_uncompressed,
            compressed_size: actual_compressed,
            method: method.to_core(),
            modified: Some(modified),
            created: None,
            accessed: None,
            attributes: FileAttributes::default(),
            crc32: Some(crc32),
            comment: if comment.is_empty() {
                None
            } else {
                Some(comment)
            },
            link_target: None,
            offset: data_offset,
            extra: entry_extra,
        })
    }

    /// Read entries from local headers (fallback, doesn't handle data descriptors well).
    fn read_from_local_headers(reader: &mut R) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();

        // Start from beginning
        reader.seek(SeekFrom::Start(0))?;

        loop {
            let pos = reader.stream_position()?;

            // Try to read signature
            let mut sig_buf = [0u8; 4];
            if reader.read_exact(&mut sig_buf).is_err() {
                break;
            }

            let signature = u32::from_le_bytes(sig_buf);

            if signature == LOCAL_FILE_HEADER_SIG {
                // Seek back and read full header
                reader.seek(SeekFrom::Start(pos))?;
                let mut header = LocalFileHeader::read(reader)?;

                // Record data offset
                header.data_offset = reader.stream_position()?;

                // Handle data descriptor case
                if header.has_data_descriptor() && header.compressed_size == 0 {
                    // Can't skip properly without scanning for next header or reading central dir
                    // This is why we prefer central directory parsing
                    break;
                }

                // Skip compressed data (use actual size for Zip64 support)
                let compressed_size = header.actual_compressed_size();
                reader.seek(SeekFrom::Current(compressed_size as i64))?;

                // Skip data descriptor if present
                if header.has_data_descriptor() {
                    let is_zip64 = header.compressed_size == ZIP64_MARKER_32
                        || header.uncompressed_size == ZIP64_MARKER_32;
                    let (descriptor, _) = DataDescriptor::read(reader, is_zip64)?;
                    // Update header with data descriptor values if header had zeros
                    if header.crc32 == 0 {
                        // Note: Can't mutate header here, but we've already created entry
                        // This is fine since central directory path is preferred
                        let _ = descriptor;
                    }
                }

                entries.push(header.to_entry());
            } else if signature == CENTRAL_DIR_HEADER_SIG || signature == END_OF_CENTRAL_DIR_SIG {
                // Reached central directory, stop
                break;
            } else {
                // Unknown signature, stop
                break;
            }
        }

        Ok(entries)
    }

    /// Get the list of entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Extract an entry.
    pub fn extract(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        // Seek to data
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Read compressed data
        let mut compressed = vec![0u8; entry.compressed_size as usize];
        self.reader.read_exact(&mut compressed)?;

        // Decompress based on method
        let decompressed = match entry.method {
            CoreMethod::Stored => compressed,
            CoreMethod::Deflate => inflate(&compressed)?,
            _ => return Err(OxiArcError::unsupported_method(format!("{}", entry.method))),
        };

        // Verify CRC
        if let Some(expected_crc) = entry.crc32 {
            let actual_crc = Crc32::compute(&decompressed);
            if actual_crc != expected_crc {
                return Err(OxiArcError::crc_mismatch(expected_crc, actual_crc));
            }
        }

        Ok(decompressed)
    }

    /// Get entry by name.
    pub fn entry_by_name(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }
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
struct CentralDirEntry {
    /// Version made by.
    version_made_by: u16,
    /// Version needed to extract.
    version_needed: u16,
    /// General purpose bit flag.
    flags: u16,
    /// Compression method.
    method: u16,
    /// Last modification time.
    mtime: u16,
    /// Last modification date.
    mdate: u16,
    /// CRC-32 of uncompressed data.
    crc32: u32,
    /// Compressed size (64-bit for Zip64).
    compressed_size: u64,
    /// Uncompressed size (64-bit for Zip64).
    uncompressed_size: u64,
    /// File name.
    filename: String,
    /// Extra field (not including Zip64 extra).
    extra: Vec<u8>,
    /// File comment.
    comment: String,
    /// Disk number start.
    disk_start: u16,
    /// Internal file attributes.
    internal_attr: u16,
    /// External file attributes.
    external_attr: u32,
    /// Relative offset of local header (64-bit for Zip64).
    local_header_offset: u64,
}

impl CentralDirEntry {
    /// Check if this entry requires Zip64.
    fn needs_zip64(&self) -> bool {
        self.compressed_size >= ZIP64_MARKER_32 as u64
            || self.uncompressed_size >= ZIP64_MARKER_32 as u64
            || self.local_header_offset >= ZIP64_MARKER_32 as u64
    }

    /// Build Zip64 extra field if needed.
    fn build_zip64_extra(&self) -> Vec<u8> {
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
}

impl CentralDirEntry {
    /// Write the central directory entry.
    fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
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
    fn written_size(&self) -> usize {
        let zip64_extra = self.build_zip64_extra();
        46 + self.filename.len() + self.extra.len() + zip64_extra.len() + self.comment.len()
    }
}

/// ZIP archive writer.
pub struct ZipWriter<W: Write> {
    writer: W,
    entries: Vec<CentralDirEntry>,
    offset: u64,
    compression: ZipCompressionLevel,
    finished: bool,
}

impl<W: Write> ZipWriter<W> {
    /// Create a new ZIP writer with default compression.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            entries: Vec::new(),
            offset: 0,
            compression: ZipCompressionLevel::default(),
            finished: false,
        }
    }

    /// Set the compression level for subsequent files.
    pub fn set_compression(&mut self, level: ZipCompressionLevel) {
        self.compression = level;
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_options(name, data, self.compression)
    }

    /// Add a file with specific compression.
    pub fn add_file_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        compression: ZipCompressionLevel,
    ) -> Result<()> {
        let crc32 = Crc32::compute(data);

        // Get current time for DOS format
        let (mtime, mdate) = Self::current_dos_time();

        // Compress data
        let (compressed_data, method): (Vec<u8>, u16) = match compression {
            ZipCompressionLevel::Store => (data.to_vec(), 0),
            ZipCompressionLevel::Fast => {
                let compressed = deflate(data, 1)?;
                // Only use compression if it's smaller
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Normal => {
                let compressed = deflate(data, 6)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Best => {
                let compressed = deflate(data, 9)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
        };

        let compressed_size = compressed_data.len() as u64;
        let uncompressed_size = data.len() as u64;
        let local_header_offset = self.offset;

        // Check if we need Zip64
        let needs_zip64 = compressed_size >= ZIP64_MARKER_32 as u64
            || uncompressed_size >= ZIP64_MARKER_32 as u64
            || local_header_offset >= ZIP64_MARKER_32 as u64;

        // Version needed: 45 for Zip64, 20 for deflate, 10 for store
        let version_needed: u16 = if needs_zip64 {
            45
        } else if method == 8 {
            20
        } else {
            10
        };

        // Write local file header
        let filename_bytes = name.as_bytes();

        // Build Zip64 extra field for local header if needed
        let mut local_extra = Vec::new();
        if needs_zip64 {
            local_extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes()); // Data size
            local_extra.extend_from_slice(&uncompressed_size.to_le_bytes());
            local_extra.extend_from_slice(&compressed_size.to_le_bytes());
        }

        // Use marker values for Zip64
        let compressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            compressed_size as u32
        };
        let uncompressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            uncompressed_size as u32
        };

        // Signature
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        // Version needed
        self.writer.write_all(&version_needed.to_le_bytes())?;
        // Flags (0 = no special flags)
        self.writer.write_all(&0u16.to_le_bytes())?;
        // Compression method
        self.writer.write_all(&method.to_le_bytes())?;
        // Modification time
        self.writer.write_all(&mtime.to_le_bytes())?;
        // Modification date
        self.writer.write_all(&mdate.to_le_bytes())?;
        // CRC-32
        self.writer.write_all(&crc32.to_le_bytes())?;
        // Compressed size
        self.writer.write_all(&compressed_size_32.to_le_bytes())?;
        // Uncompressed size
        self.writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        // Filename length
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        // Extra field length
        self.writer
            .write_all(&(local_extra.len() as u16).to_le_bytes())?;
        // Filename
        self.writer.write_all(filename_bytes)?;
        // Extra field
        self.writer.write_all(&local_extra)?;

        // Write file data
        self.writer.write_all(&compressed_data)?;

        // Update offset (30 = local header fixed size)
        self.offset += 30
            + filename_bytes.len() as u64
            + local_extra.len() as u64
            + compressed_data.len() as u64;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E, // Unix, version 3.0
            version_needed,
            flags: 0,
            method,
            mtime,
            mdate,
            crc32,
            compressed_size,
            uncompressed_size,
            filename: name.to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16, // Regular file, rw-r--r--
            local_header_offset,
        });

        Ok(())
    }

    /// Add a directory to the archive.
    pub fn add_directory(&mut self, name: &str) -> Result<()> {
        // Ensure directory name ends with /
        let dir_name = if name.ends_with('/') {
            name.to_string()
        } else {
            format!("{}/", name)
        };

        let (mtime, mdate) = Self::current_dos_time();
        let local_header_offset = self.offset;
        let filename_bytes = dir_name.as_bytes();

        // Write local file header for directory
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        self.writer.write_all(&10u16.to_le_bytes())?; // Version needed
        self.writer.write_all(&0u16.to_le_bytes())?; // Flags
        self.writer.write_all(&0u16.to_le_bytes())?; // Method (stored)
        self.writer.write_all(&mtime.to_le_bytes())?;
        self.writer.write_all(&mdate.to_le_bytes())?;
        self.writer.write_all(&0u32.to_le_bytes())?; // CRC-32
        self.writer.write_all(&0u32.to_le_bytes())?; // Compressed size
        self.writer.write_all(&0u32.to_le_bytes())?; // Uncompressed size
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        self.writer.write_all(&0u16.to_le_bytes())?; // Extra field length
        self.writer.write_all(filename_bytes)?;

        self.offset += 30 + filename_bytes.len() as u64;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E,
            version_needed: 10,
            flags: 0,
            method: 0,
            mtime,
            mdate,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            filename: dir_name,
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o40755 << 16, // Directory, rwxr-xr-x
            local_header_offset,
        });

        Ok(())
    }

    /// Finish writing the archive.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        let central_dir_offset = self.offset;
        let mut central_dir_size = 0u64;

        // Write central directory
        for entry in &self.entries {
            let entry_size = entry.written_size() as u64;
            central_dir_size += entry_size;
            entry.write(&mut self.writer)?;
        }

        // Determine if Zip64 EOCD is needed
        let num_entries = self.entries.len() as u64;
        let needs_zip64 = num_entries > ZIP64_MARKER_16 as u64
            || central_dir_size >= ZIP64_MARKER_32 as u64
            || central_dir_offset >= ZIP64_MARKER_32 as u64
            || self.entries.iter().any(|e| e.needs_zip64());

        if needs_zip64 {
            let zip64_eocd_offset = central_dir_offset + central_dir_size;

            // Write Zip64 End of Central Directory Record
            // Signature
            self.writer
                .write_all(&ZIP64_END_OF_CENTRAL_DIR_SIG.to_le_bytes())?;
            // Size of Zip64 EOCD record (44 bytes following this field)
            self.writer.write_all(&44u64.to_le_bytes())?;
            // Version made by
            self.writer.write_all(&0x031Eu16.to_le_bytes())?;
            // Version needed to extract
            self.writer.write_all(&45u16.to_le_bytes())?;
            // Number of this disk
            self.writer.write_all(&0u32.to_le_bytes())?;
            // Disk where central directory starts
            self.writer.write_all(&0u32.to_le_bytes())?;
            // Number of central directory records on this disk
            self.writer.write_all(&num_entries.to_le_bytes())?;
            // Total number of central directory records
            self.writer.write_all(&num_entries.to_le_bytes())?;
            // Size of central directory
            self.writer.write_all(&central_dir_size.to_le_bytes())?;
            // Offset of start of central directory
            self.writer.write_all(&central_dir_offset.to_le_bytes())?;

            // Write Zip64 End of Central Directory Locator
            // Signature
            self.writer
                .write_all(&ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG.to_le_bytes())?;
            // Number of disk with Zip64 EOCD
            self.writer.write_all(&0u32.to_le_bytes())?;
            // Relative offset of Zip64 EOCD
            self.writer.write_all(&zip64_eocd_offset.to_le_bytes())?;
            // Total number of disks
            self.writer.write_all(&1u32.to_le_bytes())?;
        }

        // Write (regular) End of Central Directory record
        // Use marker values for Zip64
        let num_entries_16 = if num_entries > ZIP64_MARKER_16 as u64 {
            ZIP64_MARKER_16
        } else {
            num_entries as u16
        };
        let central_dir_size_32 = if central_dir_size >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            central_dir_size as u32
        };
        let central_dir_offset_32 = if central_dir_offset >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            central_dir_offset as u32
        };

        self.writer
            .write_all(&END_OF_CENTRAL_DIR_SIG.to_le_bytes())?;
        // Disk number
        self.writer.write_all(&0u16.to_le_bytes())?;
        // Disk with central directory
        self.writer.write_all(&0u16.to_le_bytes())?;
        // Number of entries on this disk
        self.writer.write_all(&num_entries_16.to_le_bytes())?;
        // Total number of entries
        self.writer.write_all(&num_entries_16.to_le_bytes())?;
        // Size of central directory
        self.writer.write_all(&central_dir_size_32.to_le_bytes())?;
        // Offset of central directory
        self.writer
            .write_all(&central_dir_offset_32.to_le_bytes())?;
        // Comment length
        self.writer.write_all(&0u16.to_le_bytes())?;

        self.writer.flush()?;
        self.finished = true;
        Ok(())
    }

    /// Consume the writer and return the inner writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.finish()?;
        // Use ManuallyDrop to prevent Drop from running
        let this = std::mem::ManuallyDrop::new(self);
        Ok(unsafe { std::ptr::read(&this.writer) })
    }

    /// Get current time in DOS format.
    fn current_dos_time() -> (u16, u16) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);

        // Convert to DOS time (simplified)
        let secs = now.as_secs();
        let days = secs / 86400;
        let time_of_day = secs % 86400;

        let hours = (time_of_day / 3600) as u16;
        let minutes = ((time_of_day % 3600) / 60) as u16;
        let seconds = ((time_of_day % 60) / 2) as u16; // DOS stores in 2-second increments

        let mtime = (hours << 11) | (minutes << 5) | seconds;

        // Approximate date calculation (days since 1970-01-01)
        let years = days / 365;
        let year = (1970 + years) as u16;
        let day_of_year = days % 365;
        let month = ((day_of_year / 30) + 1) as u16;
        let day = ((day_of_year % 30) + 1) as u16;

        let mdate = if year >= 1980 {
            ((year - 1980) << 9) | (month << 5) | day
        } else {
            0 // Before DOS epoch
        };

        (mtime, mdate)
    }
}

impl<W: Write> Drop for ZipWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_compression_method() {
        assert_eq!(CompressionMethod::from_u16(0), CompressionMethod::Stored);
        assert_eq!(CompressionMethod::from_u16(8), CompressionMethod::Deflate);
        assert!(matches!(
            CompressionMethod::from_u16(99),
            CompressionMethod::Unknown(99)
        ));
    }

    #[test]
    fn test_zip_writer_single_file() {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("hello.txt", b"Hello, World!").unwrap();
            writer.finish().unwrap();
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, 13);

        let data = reader.extract(&entry).unwrap();
        assert_eq!(&data, b"Hello, World!");
    }

    #[test]
    fn test_zip_writer_stored() {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer
                .add_file_with_options("test.bin", b"short", ZipCompressionLevel::Store)
                .unwrap();
            writer.finish().unwrap();
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor).unwrap();

        let entry = reader.entries()[0].clone();
        assert_eq!(entry.method, CoreMethod::Stored);

        let data = reader.extract(&entry).unwrap();
        assert_eq!(&data, b"short");
    }

    #[test]
    fn test_zip_writer_multiple_files() {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("file1.txt", b"Content 1").unwrap();
            writer
                .add_file("file2.txt", b"Content 2 is longer")
                .unwrap();
            writer.add_file("empty.txt", b"").unwrap();
            writer.finish().unwrap();
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 3);
        assert_eq!(reader.entries()[0].name, "file1.txt");
        assert_eq!(reader.entries()[1].name, "file2.txt");
        assert_eq!(reader.entries()[2].name, "empty.txt");

        let data1 = reader.extract(&reader.entries()[0].clone()).unwrap();
        let data2 = reader.extract(&reader.entries()[1].clone()).unwrap();
        let data3 = reader.extract(&reader.entries()[2].clone()).unwrap();

        assert_eq!(&data1, b"Content 1");
        assert_eq!(&data2, b"Content 2 is longer");
        assert_eq!(&data3, b"");
    }

    #[test]
    fn test_zip_writer_directory() {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_directory("mydir").unwrap();
            writer
                .add_file("mydir/file.txt", b"Inside directory")
                .unwrap();
            writer.finish().unwrap();
        }

        let cursor = Cursor::new(output);
        let reader = ZipReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.entries()[0].name, "mydir/");
        assert!(reader.entries()[0].is_dir());
        assert_eq!(reader.entries()[1].name, "mydir/file.txt");
        assert!(reader.entries()[1].is_file());
    }

    #[test]
    fn test_zip_roundtrip_compressed() {
        // Create compressible data
        let data = "This is a test string that repeats. ".repeat(100);
        let data_bytes = data.as_bytes();

        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("large.txt", data_bytes).unwrap();
            writer.finish().unwrap();
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor).unwrap();

        let entry = reader.entries()[0].clone();
        // Should be compressed (smaller than original)
        assert!(entry.compressed_size < entry.size);
        assert_eq!(entry.method, CoreMethod::Deflate);

        let extracted = reader.extract(&entry).unwrap();
        assert_eq!(extracted, data_bytes);
    }

    #[test]
    fn test_zip64_extra_field_parsing() {
        // Test parsing of Zip64 extra field
        let extra = [
            0x01, 0x00, // Header ID: 0x0001 (Zip64)
            0x10, 0x00, // Data size: 16 bytes
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, // Uncompressed size: 0x100000000 (4GB)
            0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x00, // Compressed size: 0x80000000 (2GB)
        ];

        let (uncompressed, compressed) =
            LocalFileHeader::parse_zip64_extra(&extra, ZIP64_MARKER_32, ZIP64_MARKER_32);

        assert_eq!(uncompressed, Some(0x100000000u64));
        assert_eq!(compressed, Some(0x80000000u64));
    }

    #[test]
    fn test_zip64_extra_field_no_marker() {
        // When sizes don't have marker values, Zip64 extra should be ignored
        let extra = [
            0x01, 0x00, // Header ID: 0x0001 (Zip64)
            0x10, 0x00, // Data size: 16 bytes
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // Uncompressed size
            0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // Compressed size
        ];

        // No markers, so sizes should remain None
        let (uncompressed, compressed) = LocalFileHeader::parse_zip64_extra(&extra, 1000, 500);

        assert_eq!(uncompressed, None);
        assert_eq!(compressed, None);
    }

    #[test]
    fn test_central_dir_entry_needs_zip64() {
        let entry = CentralDirEntry {
            version_made_by: 0x031E,
            version_needed: 20,
            flags: 0,
            method: 0,
            mtime: 0,
            mdate: 0,
            crc32: 0,
            compressed_size: 100,
            uncompressed_size: 200,
            filename: "test.txt".to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0,
            local_header_offset: 0,
        };
        assert!(!entry.needs_zip64());

        // Large compressed size
        let entry_large = CentralDirEntry {
            compressed_size: 0x1_0000_0000,
            ..entry.clone()
        };
        assert!(entry_large.needs_zip64());

        // Large uncompressed size
        let entry_large_uncompressed = CentralDirEntry {
            uncompressed_size: 0x1_0000_0000,
            ..entry.clone()
        };
        assert!(entry_large_uncompressed.needs_zip64());

        // Large offset
        let entry_large_offset = CentralDirEntry {
            local_header_offset: 0x1_0000_0000,
            ..entry.clone()
        };
        assert!(entry_large_offset.needs_zip64());
    }

    #[test]
    fn test_central_dir_entry_build_zip64_extra() {
        let entry = CentralDirEntry {
            version_made_by: 0x031E,
            version_needed: 20,
            flags: 0,
            method: 0,
            mtime: 0,
            mdate: 0,
            crc32: 0,
            compressed_size: 0x1_0000_0000, // 4GB+
            uncompressed_size: 0x2_0000_0000,
            filename: "test.txt".to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0,
            local_header_offset: 0x3_0000_0000,
        };

        let extra = entry.build_zip64_extra();
        // Header (4) + uncompressed (8) + compressed (8) + offset (8) = 28 bytes
        assert_eq!(extra.len(), 28);
        // Check header ID
        assert_eq!(
            u16::from_le_bytes([extra[0], extra[1]]),
            ZIP64_EXTRA_FIELD_ID
        );
        // Check data size (24 bytes)
        assert_eq!(u16::from_le_bytes([extra[2], extra[3]]), 24);
    }

    #[test]
    fn test_data_descriptor_with_signature() {
        // Data descriptor with signature
        let data = [
            0x50, 0x4B, 0x07, 0x08, // Signature
            0x12, 0x34, 0x56, 0x78, // CRC-32
            0x00, 0x10, 0x00, 0x00, // Compressed size (4096)
            0x00, 0x20, 0x00, 0x00, // Uncompressed size (8192)
        ];

        let mut cursor = Cursor::new(data);
        let (descriptor, bytes) = DataDescriptor::read(&mut cursor, false).unwrap();

        assert_eq!(bytes, 16); // 4 (sig) + 4 (crc) + 4 (comp) + 4 (uncomp)
        assert_eq!(descriptor.crc32, 0x78563412);
        assert_eq!(descriptor.compressed_size, 4096);
        assert_eq!(descriptor.uncompressed_size, 8192);
    }

    #[test]
    fn test_data_descriptor_without_signature() {
        // Data descriptor without signature
        let data = [
            0x12, 0x34, 0x56, 0x78, // CRC-32 (no signature)
            0x00, 0x10, 0x00, 0x00, // Compressed size (4096)
            0x00, 0x20, 0x00, 0x00, // Uncompressed size (8192)
        ];

        let mut cursor = Cursor::new(data);
        let (descriptor, bytes) = DataDescriptor::read(&mut cursor, false).unwrap();

        assert_eq!(bytes, 12); // 4 (crc) + 4 (comp) + 4 (uncomp)
        assert_eq!(descriptor.crc32, 0x78563412);
        assert_eq!(descriptor.compressed_size, 4096);
        assert_eq!(descriptor.uncompressed_size, 8192);
    }

    #[test]
    fn test_data_descriptor_zip64() {
        // Zip64 data descriptor with 8-byte sizes
        let data = [
            0x50, 0x4B, 0x07, 0x08, // Signature
            0xAB, 0xCD, 0xEF, 0x12, // CRC-32
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // Compressed: 0x100000000
            0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // Uncompressed: 0x200000000
        ];

        let mut cursor = Cursor::new(data);
        let (descriptor, bytes) = DataDescriptor::read(&mut cursor, true).unwrap();

        assert_eq!(bytes, 24); // 4 (sig) + 4 (crc) + 8 (comp) + 8 (uncomp)
        assert_eq!(descriptor.crc32, 0x12EFCDAB);
        assert_eq!(descriptor.compressed_size, 0x100000000);
        assert_eq!(descriptor.uncompressed_size, 0x200000000);
    }

    #[test]
    fn test_local_header_has_data_descriptor() {
        let header = LocalFileHeader {
            version_needed: 20,
            flags: FLAG_DATA_DESCRIPTOR, // Bit 3 set
            method: CompressionMethod::Deflate,
            mtime: 0,
            mdate: 0,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            filename: "test.txt".to_string(),
            extra: Vec::new(),
            data_offset: 0,
            uncompressed_size_64: None,
            compressed_size_64: None,
        };
        assert!(header.has_data_descriptor());

        let header_no_dd = LocalFileHeader {
            flags: 0, // No data descriptor
            ..header
        };
        assert!(!header_no_dd.has_data_descriptor());
    }
}

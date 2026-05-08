//! LZH header types and parsing logic.
//!
//! Provides [`LzhHeader`], the per-entry header struct read from LZH archives,
//! and the private `LzhExtensionData` accumulator that collects extension-header
//! metadata during header parsing.

use encoding_rs::SHIFT_JIS;
use oxiarc_core::entry::CompressionMethod as CoreMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Entry, EntryType, FileAttributes};
use oxiarc_lzhuf::LzhMethod;
use std::io::Read;
use std::time::{Duration, UNIX_EPOCH};

/// LZH header.
#[derive(Debug, Clone)]
pub struct LzhHeader {
    /// Header size.
    pub header_size: u16,
    /// Compression method.
    pub method: LzhMethod,
    /// Compressed size.
    pub compressed_size: u32,
    /// Original (uncompressed) size.
    pub original_size: u32,
    /// Modification time (Unix timestamp or DOS time).
    pub mtime: u32,
    /// File attributes.
    pub attributes: u8,
    /// Header level (0, 1, or 2).
    pub level: u8,
    /// File name.
    pub filename: String,
    /// CRC-16 of original data.
    pub crc16: u16,
    /// OS identifier.
    pub os_id: u8,
    /// Data offset in archive.
    pub data_offset: u64,

    /// DOS attribute byte parsed from extension header `0x40`.
    pub dos_attr: Option<u8>,
    /// Unix UID parsed from extension header `0x41` (second u16 LE).
    pub unix_uid: Option<u16>,
    /// Unix GID parsed from extension header `0x41` (first u16 LE).
    pub unix_gid: Option<u16>,
    /// Unix group name parsed from extension header `0x42`.
    pub unix_group_name: Option<String>,
    /// Unix user name parsed from extension header `0x43`.
    pub unix_user_name: Option<String>,
    /// Unix mtime parsed from extension header `0x44` (u32 LE, seconds since epoch).
    pub unix_mtime: Option<u32>,
    /// Free-form comment parsed from extension header `0x46`.
    pub comment: Option<String>,
    /// Unix file permissions parsed from extension header `0x50` (u16 LE).
    pub unix_permission: Option<u16>,
}

/// Accumulator for extension-header metadata parsed from a Level-2 or
/// Level-3 header. Populated by [`LzhExtensionData::apply`] as each
/// extension block is decoded; copied into [`LzhHeader`] at the end.
#[derive(Debug, Default)]
pub(crate) struct LzhExtensionData {
    pub(crate) dos_attr: Option<u8>,
    pub(crate) unix_uid: Option<u16>,
    pub(crate) unix_gid: Option<u16>,
    pub(crate) unix_group_name: Option<String>,
    pub(crate) unix_user_name: Option<String>,
    pub(crate) unix_mtime: Option<u32>,
    pub(crate) comment: Option<String>,
    pub(crate) unix_permission: Option<u16>,
}

impl LzhExtensionData {
    /// Decode a single `[type + data]` extension header payload and
    /// fold the contained value into `self`.
    pub(crate) fn apply(&mut self, ext_type: u8, data: &[u8]) {
        match ext_type {
            0x40 => {
                self.dos_attr = data.first().copied();
            }
            0x41 if data.len() >= 4 => {
                self.unix_gid = Some(u16::from_le_bytes([data[0], data[1]]));
                self.unix_uid = Some(u16::from_le_bytes([data[2], data[3]]));
            }
            0x42 => {
                self.unix_group_name = Some(String::from_utf8_lossy(data).into_owned());
            }
            0x43 => {
                self.unix_user_name = Some(String::from_utf8_lossy(data).into_owned());
            }
            0x44 if data.len() >= 4 => {
                self.unix_mtime = Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
            }
            0x46 => {
                self.comment = Some(String::from_utf8_lossy(data).into_owned());
            }
            0x50 if data.len() >= 2 => {
                self.unix_permission = Some(u16::from_le_bytes([data[0], data[1]]));
            }
            _ => {
                // Silently ignore unknown extension types or types
                // with unexpected payload lengths. Lenient mode still
                // needs reader progress to advance; strict mode
                // tolerates unknown extensions by design (forward
                // compatibility with newer LHA metadata).
            }
        }
    }
}

impl LzhHeader {
    /// Read a LZH header.
    pub fn read<R: Read>(reader: &mut R, offset: u64) -> Result<Option<Self>> {
        // Read first two bytes to determine header type
        let mut first_buf = [0u8; 2];
        if reader.read_exact(&mut first_buf).is_err() {
            return Ok(None);
        }

        // Check for Level 3 header (word size == 4, method at offset 2)
        // Level 3 headers start with 2-byte word size field (always 0x0004)
        if first_buf[0] == 0x04 && first_buf[1] == 0x00 {
            return Self::read_level3(reader, offset);
        }

        // Level 0/1/2: first byte is header size
        let header_size = first_buf[0];
        if header_size == 0 {
            return Ok(None); // End of archive
        }

        // Second byte is checksum (already read in first_buf[1])
        let _checksum = first_buf[1];

        // Read method ID (5 bytes)
        let mut method_buf = [0u8; 5];
        reader.read_exact(&mut method_buf)?;

        let method = LzhMethod::from_id(&method_buf).ok_or_else(|| {
            OxiArcError::unsupported_method(String::from_utf8_lossy(&method_buf).into_owned())
        })?;

        // Read common fields (14 bytes: compressed, original, mtime, attr, level)
        let mut common = [0u8; 14];
        reader.read_exact(&mut common)?;

        let compressed_size = u32::from_le_bytes([common[0], common[1], common[2], common[3]]);
        let original_size = u32::from_le_bytes([common[4], common[5], common[6], common[7]]);
        let mtime = u32::from_le_bytes([common[8], common[9], common[10], common[11]]);
        let attributes = common[12];
        let level = common[13];

        // Parse based on header level
        let mut ext_data = LzhExtensionData::default();
        let (filename, crc16, os_id, extra_size) = match level {
            0 => Self::parse_level0(reader, &mut [0u8; 256])?,
            1 => Self::parse_level1(reader)?,
            2 => Self::parse_level2(reader, &mut ext_data)?,
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unsupported header level: {}",
                    level
                )));
            }
        };

        // Calculate data offset
        let data_offset = offset
            + 2  // size + checksum
            + 5  // method
            + 14 // common fields (compressed, original, mtime, attr, level)
            + extra_size as u64;

        Ok(Some(Self {
            header_size: header_size as u16,
            method,
            compressed_size,
            original_size,
            mtime,
            attributes,
            level,
            filename,
            crc16,
            os_id,
            data_offset,
            dos_attr: ext_data.dos_attr,
            unix_uid: ext_data.unix_uid,
            unix_gid: ext_data.unix_gid,
            unix_group_name: ext_data.unix_group_name,
            unix_user_name: ext_data.unix_user_name,
            unix_mtime: ext_data.unix_mtime,
            comment: ext_data.comment,
            unix_permission: ext_data.unix_permission,
        }))
    }

    /// Read a Level 3 header.
    /// Level 3 uses word-sized fields and 4-byte extended header sizes.
    fn read_level3<R: Read>(reader: &mut R, offset: u64) -> Result<Option<Self>> {
        // Word size already read (0x0004), now read method ID (5 bytes)
        let mut method_buf = [0u8; 5];
        reader.read_exact(&mut method_buf)?;

        let method = LzhMethod::from_id(&method_buf).ok_or_else(|| {
            OxiArcError::unsupported_method(String::from_utf8_lossy(&method_buf).into_owned())
        })?;

        // Read sizes (4 bytes each)
        let mut size_buf = [0u8; 4];
        reader.read_exact(&mut size_buf)?;
        let compressed_size = u32::from_le_bytes(size_buf);

        reader.read_exact(&mut size_buf)?;
        let original_size = u32::from_le_bytes(size_buf);

        // Read mtime (4 bytes, Unix timestamp)
        reader.read_exact(&mut size_buf)?;
        let mtime = u32::from_le_bytes(size_buf);

        // Read reserved (1 byte, should be 0x20) and level (1 byte, should be 3)
        let mut reserved_level = [0u8; 2];
        reader.read_exact(&mut reserved_level)?;
        let attributes = reserved_level[0];
        let level = reserved_level[1];

        if level != 3 {
            return Err(OxiArcError::invalid_header(format!(
                "Expected header level 3, got {}",
                level
            )));
        }

        // Read CRC-16 (2 bytes)
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        // Read OS ID (1 byte)
        let mut os_buf = [0u8; 1];
        reader.read_exact(&mut os_buf)?;
        let os_id = os_buf[0];

        // Read total header size (4 bytes) - this is the complete header size
        reader.read_exact(&mut size_buf)?;
        let header_size = u32::from_le_bytes(size_buf);

        // Read next extended header size (4 bytes)
        reader.read_exact(&mut size_buf)?;
        let mut next_size = u32::from_le_bytes(size_buf);

        let mut filename = String::new();
        let mut ext_data = LzhExtensionData::default();

        // Read extended headers (4-byte size fields in Level 3)
        while next_size > 0 {
            let mut header = vec![0u8; next_size as usize];
            reader.read_exact(&mut header)?;

            if !header.is_empty() {
                let header_type = header[0];
                let data = &header[1..];

                match header_type {
                    0x01 => {
                        // Filename
                        filename = Self::decode_filename(data);
                    }
                    0x02 => {
                        // Directory name
                        let dirname = Self::decode_filename(data);
                        if !dirname.is_empty() {
                            filename = format!("{}/{}", dirname, filename);
                        }
                    }
                    _ => ext_data.apply(header_type, data),
                }
            }

            // Read next extended header size (4 bytes in Level 3)
            if reader.read_exact(&mut size_buf).is_ok() {
                next_size = u32::from_le_bytes(size_buf);
            } else {
                break;
            }
        }

        // Data offset: header_size tells us the complete header size from start
        let data_offset = offset + header_size as u64;

        Ok(Some(Self {
            header_size: header_size as u16,
            method,
            compressed_size,
            original_size,
            mtime,
            attributes,
            level,
            filename,
            crc16,
            os_id,
            data_offset,
            dos_attr: ext_data.dos_attr,
            unix_uid: ext_data.unix_uid,
            unix_gid: ext_data.unix_gid,
            unix_group_name: ext_data.unix_group_name,
            unix_user_name: ext_data.unix_user_name,
            unix_mtime: ext_data.unix_mtime,
            comment: ext_data.comment,
            unix_permission: ext_data.unix_permission,
        }))
    }

    /// Parse level 0 header.
    fn parse_level0<R: Read>(
        reader: &mut R,
        _buf: &mut [u8; 256],
    ) -> Result<(String, u16, u8, usize)> {
        // Filename length
        let mut len_buf = [0u8; 1];
        reader.read_exact(&mut len_buf)?;
        let filename_len = len_buf[0] as usize;

        // Filename
        let mut filename_buf = vec![0u8; filename_len];
        reader.read_exact(&mut filename_buf)?;
        let filename = Self::decode_filename(&filename_buf);

        // CRC-16
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        Ok((filename, crc16, 0, 1 + filename_len + 2))
    }

    /// Parse level 1 header.
    fn parse_level1<R: Read>(reader: &mut R) -> Result<(String, u16, u8, usize)> {
        // Filename length
        let mut len_buf = [0u8; 1];
        reader.read_exact(&mut len_buf)?;
        let filename_len = len_buf[0] as usize;

        // Filename
        let mut filename_buf = vec![0u8; filename_len];
        reader.read_exact(&mut filename_buf)?;
        let filename = Self::decode_filename(&filename_buf);

        // CRC-16
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        // OS ID
        let mut os_buf = [0u8; 1];
        reader.read_exact(&mut os_buf)?;
        let os_id = os_buf[0];

        // Extended header size (skip for now)
        let mut ext_size_buf = [0u8; 2];
        reader.read_exact(&mut ext_size_buf)?;
        let ext_size = u16::from_le_bytes(ext_size_buf);

        // Skip extended headers
        if ext_size > 0 {
            let mut skip = vec![0u8; ext_size as usize];
            reader.read_exact(&mut skip)?;
        }

        Ok((
            filename,
            crc16,
            os_id,
            1 + filename_len + 2 + 1 + 2 + ext_size as usize,
        ))
    }

    /// Parse level 2 header.
    fn parse_level2<R: Read>(
        reader: &mut R,
        ext_data: &mut LzhExtensionData,
    ) -> Result<(String, u16, u8, usize)> {
        // CRC-16
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        // OS ID
        let mut os_buf = [0u8; 1];
        reader.read_exact(&mut os_buf)?;
        let os_id = os_buf[0];

        // Next header size
        let mut next_size_buf = [0u8; 2];
        reader.read_exact(&mut next_size_buf)?;
        let mut next_size = u16::from_le_bytes(next_size_buf);

        let mut filename = String::new();
        let mut extra_bytes = 2 + 1 + 2;

        // Read extended headers
        while next_size > 0 {
            let mut header = vec![0u8; next_size as usize];
            reader.read_exact(&mut header)?;
            extra_bytes += next_size as usize;

            if !header.is_empty() {
                let header_type = header[0];
                let data = &header[1..];

                match header_type {
                    0x01 => {
                        // Filename
                        filename = Self::decode_filename(data);
                    }
                    0x02 => {
                        // Directory name
                        let dirname = Self::decode_filename(data);
                        if !dirname.is_empty() {
                            filename = format!("{}/{}", dirname, filename);
                        }
                    }
                    _ => ext_data.apply(header_type, data),
                }
            }

            // Read next header size
            if reader.read_exact(&mut next_size_buf).is_ok() {
                next_size = u16::from_le_bytes(next_size_buf);
                extra_bytes += 2;
            } else {
                break;
            }
        }

        Ok((filename, crc16, os_id, extra_bytes))
    }

    /// Decode filename from bytes (Shift_JIS or UTF-8).
    pub(crate) fn decode_filename(bytes: &[u8]) -> String {
        // Try Shift_JIS first
        let (decoded, _, had_errors) = SHIFT_JIS.decode(bytes);
        if !had_errors {
            return decoded.into_owned();
        }

        // Fall back to UTF-8 lossy
        String::from_utf8_lossy(bytes).into_owned()
    }

    /// Convert to Entry.
    ///
    /// Extension-header metadata (0x40 / 0x41 / 0x44 / 0x46 / 0x50) is
    /// merged into the returned [`Entry`] so that callers get a uniform
    /// view: `dos_attr` shadows the fixed-header attribute byte,
    /// `unix_uid`/`unix_gid` populate [`FileAttributes::uid`] and
    /// [`FileAttributes::gid`], `unix_permission` populates
    /// [`FileAttributes::unix_mode`], `unix_mtime` replaces the fixed
    /// header mtime, and `comment` is surfaced on the entry directly.
    pub fn to_entry(&self) -> Entry {
        let entry_type = if self.filename.ends_with('/') || self.filename.ends_with('\\') {
            EntryType::Directory
        } else {
            EntryType::File
        };

        let method = match self.method {
            LzhMethod::Lh0 => CoreMethod::Lh0,
            LzhMethod::Lh4 => CoreMethod::Lh4,
            LzhMethod::Lh5 => CoreMethod::Lh5,
            LzhMethod::Lh6 => CoreMethod::Lh6,
            LzhMethod::Lh7 => CoreMethod::Lh7,
        };

        // Prefer extension-provided Unix mtime over the fixed-header
        // value when both are present. Extension 0x44 is the richer,
        // modern encoding; the fixed-header mtime remains for
        // compatibility with level-0/1/2 headers that lack 0x44.
        let mtime_secs = self
            .unix_mtime
            .map(|m| m as u64)
            .unwrap_or(self.mtime as u64);

        Entry {
            name: self.filename.replace('\\', "/"),
            entry_type,
            size: self.original_size as u64,
            compressed_size: self.compressed_size as u64,
            method,
            modified: Some(UNIX_EPOCH + Duration::from_secs(mtime_secs)),
            created: None,
            accessed: None,
            attributes: FileAttributes {
                dos_attributes: self.dos_attr.or(Some(self.attributes)),
                unix_mode: self.unix_permission.map(u32::from),
                uid: self.unix_uid.map(u32::from),
                gid: self.unix_gid.map(u32::from),
            },
            crc32: None, // LZH uses CRC-16
            comment: self.comment.clone(),
            link_target: None,
            offset: self.data_offset,
            extra: Vec::new(),
        }
    }
}

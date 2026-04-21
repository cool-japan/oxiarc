//! LZH/LHA archive format support.
//!
//! This module provides reading, writing, and extraction of LZH archives with support for
//! header levels 0, 1, 2, and 3.

pub mod extensions;
pub use extensions::LzhExtensionMetadata;

#[cfg(test)]
mod extensions_tests;

use crate::lenient::{LenientWarning, LenientWarningKind};
use encoding_rs::SHIFT_JIS;
use oxiarc_core::entry::CompressionMethod as CoreMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::{Crc16, Entry, EntryType, FileAttributes};
use oxiarc_lzhuf::{LzhMethod, decode_lzh, encode_lzh};
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
struct LzhExtensionData {
    dos_attr: Option<u8>,
    unix_uid: Option<u16>,
    unix_gid: Option<u16>,
    unix_group_name: Option<String>,
    unix_user_name: Option<String>,
    unix_mtime: Option<u32>,
    comment: Option<String>,
    unix_permission: Option<u16>,
}

impl LzhExtensionData {
    /// Decode a single `[type + data]` extension header payload and
    /// fold the contained value into `self`.
    fn apply(&mut self, ext_type: u8, data: &[u8]) {
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
    fn decode_filename(bytes: &[u8]) -> String {
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

/// Internal entry info for extraction.
#[derive(Debug, Clone)]
struct LzhEntryInfo {
    entry: Entry,
    method: LzhMethod,
    crc16: u16,
    compressed_size: u32,
}

/// LZH archive reader with extraction support.
pub struct LzhReader<R: Read + Seek> {
    reader: R,
    entries: Vec<LzhEntryInfo>,
    /// Optional progress handle for tracking extraction progress.
    progress: Option<ProgressHandle>,
    /// When `true`, CRC-16 mismatches during extraction are recorded in
    /// [`LzhReader::warnings`] instead of returning an error. Disabled
    /// by default; toggle via [`LzhReader::lenient`].
    lenient: bool,
    /// Accumulated non-fatal warnings emitted while operating in
    /// lenient mode. Empty unless [`LzhReader::lenient`] has been set
    /// to `true`.
    warnings: Vec<LenientWarning>,
}

impl<R: Read + Seek> LzhReader<R> {
    /// Create a new LZH reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let entries = Self::read_entries(&mut reader, None)?;
        Ok(Self {
            reader,
            entries,
            progress: None,
            lenient: false,
            warnings: Vec::new(),
        })
    }

    /// Create a new LZH reader with progress reporting during entry scanning.
    pub fn new_with_progress(mut reader: R, handle: ProgressHandle) -> Result<Self> {
        let entries = Self::read_entries(&mut reader, Some(&handle))?;
        Ok(Self {
            reader,
            entries,
            progress: Some(handle),
            lenient: false,
            warnings: Vec::new(),
        })
    }

    /// Attach a progress callback handle (for extraction progress only;
    /// does not retroactively replay entry-scan progress).
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Enable or disable lenient-mode extraction.
    ///
    /// When enabled, CRC-16 mismatches during extraction are recorded
    /// in [`LzhReader::warnings`] and the (possibly corrupted) payload
    /// is returned to the caller anyway. When disabled (default),
    /// CRC-16 mismatches abort the extraction with
    /// [`OxiArcError::CorruptedData`].
    pub fn lenient(mut self, enabled: bool) -> Self {
        self.lenient = enabled;
        self
    }

    /// Return the accumulated non-fatal warnings from lenient-mode
    /// operations. Empty unless [`LzhReader::lenient`] has been set to
    /// `true`.
    pub fn warnings(&self) -> &[LenientWarning] {
        &self.warnings
    }

    /// Read all entries, optionally reporting progress.
    fn read_entries(
        reader: &mut R,
        progress: Option<&ProgressHandle>,
    ) -> Result<Vec<LzhEntryInfo>> {
        let mut entries = Vec::new();
        let mut offset = 0u64;
        let mut index: u64 = 0;

        while let Some(header) = LzhHeader::read(reader, offset)? {
            let entry = header.to_entry();
            let method = header.method;
            let crc16 = header.crc16;
            let compressed_size = header.compressed_size;

            if let Some(handle) = progress {
                handle.on_entry(&entry.name, index);
                handle.on_progress(entry.size, Some(entry.size));
            }

            // Skip compressed data using seek
            reader.seek(SeekFrom::Current(header.compressed_size as i64))?;

            offset = header.data_offset + header.compressed_size as u64;
            index += 1;

            entries.push(LzhEntryInfo {
                entry,
                method,
                crc16,
                compressed_size,
            });
        }

        if let Some(handle) = progress {
            handle.on_finish();
        }

        Ok(entries)
    }

    /// Get entries.
    pub fn entries(&self) -> Vec<Entry> {
        self.entries.iter().map(|e| e.entry.clone()).collect()
    }

    /// Extract an entry to a writer.
    pub fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64> {
        // Find the entry info
        let info = self
            .entries
            .iter()
            .find(|e| e.entry.offset == entry.offset)
            .ok_or_else(|| OxiArcError::invalid_header("Entry not found"))?
            .clone();

        // Emit extraction progress start
        if let Some(ref handle) = self.progress {
            handle.on_entry(&entry.name, 0);
        }

        // Seek to data offset
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Read compressed data
        let mut compressed = vec![0u8; info.compressed_size as usize];
        self.reader.read_exact(&mut compressed)?;

        // Decompress
        let decompressed = if info.method == LzhMethod::Lh0 {
            // Stored (no compression)
            compressed
        } else {
            decode_lzh(&compressed, info.method, entry.size)?
        };

        // Verify CRC-16
        let computed_crc = Crc16::compute(&decompressed);
        if computed_crc != info.crc16 {
            if self.lenient {
                self.warnings.push(LenientWarning {
                    format: "LZH",
                    entry_name: Some(entry.name.clone()),
                    kind: LenientWarningKind::CrcMismatch {
                        expected: info.crc16 as u32,
                        computed: computed_crc as u32,
                    },
                    message: format!(
                        "CRC-16 mismatch for entry {:?} at offset {}: expected {:04X}, computed {:04X}",
                        entry.name, entry.offset, info.crc16, computed_crc
                    ),
                });
            } else {
                return Err(OxiArcError::corrupted(
                    entry.offset,
                    format!(
                        "CRC-16 mismatch: expected {:04X}, computed {:04X}",
                        info.crc16, computed_crc
                    ),
                ));
            }
        }

        // Write to output
        writer.write_all(&decompressed)?;

        // Emit extraction progress completion
        if let Some(ref handle) = self.progress {
            handle.on_progress(decompressed.len() as u64, Some(entry.size));
        }

        Ok(decompressed.len() as u64)
    }

    /// Extract an entry to a Vec.
    pub fn extract_to_vec(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(entry.size as usize);
        self.extract(entry, &mut data)?;
        Ok(data)
    }

    /// Extract an entry by name.
    pub fn extract_by_name(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let entry = self.entries.iter().find(|e| e.entry.name == name).cloned();
        match entry {
            Some(info) => Ok(Some(self.extract_to_vec(&info.entry)?)),
            None => Ok(None),
        }
    }
}

/// LZH compression level for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LzhCompressionLevel {
    /// Store without compression (lh0).
    Store,
    /// LH5 compression (8KB window, most compatible).
    #[default]
    Lh5,
}

/// LZH archive writer.
pub struct LzhWriter<W: Write> {
    writer: W,
    compression: LzhCompressionLevel,
    finished: bool,
    /// LZH header level to write (0, 1, 2, or 3).
    header_level: u8,
    /// Entry index counter for progress reporting.
    entry_index: u64,
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
}

impl<W: Write> LzhWriter<W> {
    /// Create a new LZH writer with default compression (lh5).
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            compression: LzhCompressionLevel::default(),
            finished: false,
            header_level: 1,
            entry_index: 0,
            progress: None,
        }
    }

    /// Add a file with per-entry Unix metadata encoded as level-3
    /// extension headers.
    ///
    /// Equivalent to [`LzhWriter::add_file`] but also emits the LZH
    /// extension headers corresponding to each populated field in
    /// `metadata`. Only supported when the writer is configured with
    /// header level 3 (see [`LzhWriter::with_header_level`]); other
    /// levels silently drop the metadata because the level-0/1/2 header
    /// structures do not have a compatible extension slot in our
    /// encoder.
    pub fn add_file_with_metadata(
        &mut self,
        name: &str,
        data: &[u8],
        metadata: &LzhExtensionMetadata,
    ) -> Result<()> {
        self.add_file_with_options_and_metadata(name, data, self.compression, Some(metadata))
    }

    /// Set the header level for subsequent entries.
    /// Panics if `level > 3` (programmer error).
    pub fn with_header_level(mut self, level: u8) -> Self {
        assert!(level <= 3, "LZH header level must be 0, 1, 2, or 3");
        self.header_level = level;
        self
    }

    /// Attach a progress callback handle.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Set the compression level for subsequent files.
    pub fn set_compression(&mut self, level: LzhCompressionLevel) {
        self.compression = level;
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_options_and_metadata(name, data, self.compression, None)
    }

    /// Add a file with specific compression.
    pub fn add_file_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        compression: LzhCompressionLevel,
    ) -> Result<()> {
        self.add_file_with_options_and_metadata(name, data, compression, None)
    }

    /// Unified entry point for file emission. Handles compression
    /// selection, progress accounting, header-level dispatch, and
    /// optional extension-header emission for level-3 headers.
    fn add_file_with_options_and_metadata(
        &mut self,
        name: &str,
        data: &[u8],
        compression: LzhCompressionLevel,
        metadata: Option<&LzhExtensionMetadata>,
    ) -> Result<()> {
        // Determine the actual method and compressed bytes in one pass,
        // falling back to Lh0 (stored) if compression does not reduce size.
        // Progress is emitted exactly once regardless of any fallback.
        let (method, compressed) = match compression {
            LzhCompressionLevel::Store => (LzhMethod::Lh0, data.to_vec()),
            LzhCompressionLevel::Lh5 => {
                let comp = encode_lzh(data, LzhMethod::Lh5)?;
                if comp.len() < data.len() {
                    (LzhMethod::Lh5, comp)
                } else {
                    // Fall back to stored — no recursion, so progress fires once
                    (LzhMethod::Lh0, data.to_vec())
                }
            }
        };

        let crc16 = Crc16::compute(data);
        let mtime = Self::current_unix_time();

        // Emit progress: entry start (exactly once per logical file)
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, idx);
        }
        self.entry_index += 1;

        // Write header based on header_level
        match self.header_level {
            3 => self.write_level3_header(
                name,
                &compressed,
                data.len() as u32,
                crc16,
                mtime,
                method,
                metadata,
            )?,
            _ => self.write_level1_header(
                name,
                &compressed,
                data.len() as u32,
                crc16,
                mtime,
                method,
            )?,
        }

        // Write compressed data
        self.writer.write_all(&compressed)?;

        // Emit progress: bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(compressed.len() as u64, None);
        }

        Ok(())
    }

    /// Add a directory to the archive.
    pub fn add_directory(&mut self, name: &str) -> Result<()> {
        // Ensure directory name ends with /
        let dir_name = if name.ends_with('/') || name.ends_with('\\') {
            name.to_string()
        } else {
            format!("{}/", name)
        };

        let mtime = Self::current_unix_time();

        // Emit progress: entry start
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(&dir_name, idx);
        }
        self.entry_index += 1;

        // Write level 1 header for empty directory (level 3 dirs also use same approach)
        match self.header_level {
            3 => self.write_level3_header(&dir_name, &[], 0, 0, mtime, LzhMethod::Lh0, None)?,
            _ => self.write_level1_header(&dir_name, &[], 0, 0, mtime, LzhMethod::Lh0)?,
        }

        // Emit progress: 0 bytes
        if let Some(ref handle) = self.progress {
            handle.on_progress(0, None);
        }

        Ok(())
    }

    /// Write a level 3 header.
    ///
    /// Level 3 format (all fields are little-endian):
    ///   word_size(2) | method(5) | compressed_size(4) | original_size(4) |
    ///   mtime(4) | attribute(1) | level(1=3) | crc16(2) | os_id(1) |
    ///   total_header_size(4) | next_ext_size(4) | [ext_type(1) + data…]* | terminator(4 zeros)
    ///
    /// Extension header `next_ext_size` covers only `ext_type + data` (not the size field itself).
    ///
    /// If `metadata` is `Some`, emits each populated field as an
    /// extension header in canonical order: filename (0x01) first,
    /// then (0x40, 0x41, 0x42, 0x43, 0x44, 0x46, 0x50).
    #[allow(clippy::too_many_arguments)]
    fn write_level3_header(
        &mut self,
        filename: &str,
        compressed: &[u8],
        original_size: u32,
        crc16: u16,
        mtime: u32,
        method: LzhMethod,
        metadata: Option<&LzhExtensionMetadata>,
    ) -> Result<()> {
        let filename_bytes = filename.as_bytes();
        let compressed_size = compressed.len() as u32;

        // Canonical extension-header payload list. Each element is a
        // `[type + data]` byte vector; the writer wraps each with a
        // leading 4-byte size prefix.
        let mut payloads: Vec<Vec<u8>> = Vec::new();

        // Filename extension (0x01)
        let mut fname = Vec::with_capacity(1 + filename_bytes.len());
        fname.push(0x01u8);
        fname.extend_from_slice(filename_bytes);
        payloads.push(fname);

        // Metadata-derived extensions (0x40 / 0x41 / 0x42 / 0x43 / 0x44 / 0x46 / 0x50)
        if let Some(meta) = metadata {
            payloads.extend(extensions::encode_metadata_payloads(meta));
        }

        // Fixed fields size:
        //   word_size(2) + method(5) + compressed(4) + original(4) + mtime(4) +
        //   attr(1) + level(1) + crc16(2) + os_id(1) + total_header_size(4)
        // = 28 bytes
        //
        // Variable part: for each payload, 4-byte size prefix + payload;
        // then a 4-byte terminator (0).
        let payloads_total: u32 = payloads.iter().map(|p| 4 + p.len() as u32).sum::<u32>() + 4;
        let total_header_size: u32 = 28 + payloads_total;

        let mut header: Vec<u8> = Vec::with_capacity(total_header_size as usize);

        // word_size field (2 bytes) — always 0x0004 for level 3
        header.extend_from_slice(&4u16.to_le_bytes());

        // Method ID (5 bytes)
        header.extend_from_slice(method.id());

        // Compressed size (4 bytes)
        header.extend_from_slice(&compressed_size.to_le_bytes());

        // Original size (4 bytes)
        header.extend_from_slice(&original_size.to_le_bytes());

        // mtime (4 bytes)
        header.extend_from_slice(&mtime.to_le_bytes());

        // Attribute (1 byte) — 0x20 for archive
        header.push(0x20u8);

        // Level (1 byte) — 3
        header.push(3u8);

        // CRC-16 (2 bytes)
        header.extend_from_slice(&crc16.to_le_bytes());

        // OS ID (1 byte) — 'U' for Unix
        header.push(b'U');

        // Total header size (4 bytes)
        header.extend_from_slice(&total_header_size.to_le_bytes());

        // Emit each extension payload with its 4-byte LE size prefix.
        for payload in &payloads {
            let size = payload.len() as u32;
            header.extend_from_slice(&size.to_le_bytes());
            header.extend_from_slice(payload);
        }

        // Terminator: next_ext_size = 0 (4 bytes)
        header.extend_from_slice(&0u32.to_le_bytes());

        self.writer.write_all(&header)?;

        Ok(())
    }

    /// Write a level 1 header.
    fn write_level1_header(
        &mut self,
        filename: &str,
        compressed: &[u8],
        original_size: u32,
        crc16: u16,
        mtime: u32,
        method: LzhMethod,
    ) -> Result<()> {
        let filename_bytes = filename.as_bytes();
        if filename_bytes.len() > 255 {
            return Err(OxiArcError::invalid_header("Filename too long"));
        }

        let compressed_size = compressed.len() as u32;

        // Calculate header size
        // Base header: 22 bytes + filename_len + 2 (extended header size = 0)
        let header_len = 22 + filename_bytes.len();

        // Header size byte (excludes the first 2 bytes: size and checksum)
        let header_size = (header_len - 2) as u8;

        // Build header
        let mut header = Vec::with_capacity(header_len);

        // Header size (will be updated with checksum)
        header.push(header_size);
        header.push(0u8); // Checksum placeholder

        // Method ID (5 bytes)
        header.extend_from_slice(method.id());

        // Compressed size (4 bytes)
        header.extend_from_slice(&compressed_size.to_le_bytes());

        // Original size (4 bytes)
        header.extend_from_slice(&original_size.to_le_bytes());

        // Modification time (4 bytes, Unix timestamp)
        header.extend_from_slice(&mtime.to_le_bytes());

        // Attributes (1 byte)
        header.push(0x20); // Archive attribute

        // Level (1 byte)
        header.push(1); // Level 1

        // Filename length (1 byte)
        header.push(filename_bytes.len() as u8);

        // Filename
        header.extend_from_slice(filename_bytes);

        // CRC-16 (2 bytes)
        header.extend_from_slice(&crc16.to_le_bytes());

        // OS ID (1 byte) - 'U' for Unix
        header.push(b'U');

        // Extended header size (2 bytes) - 0 for no extended headers
        header.extend_from_slice(&0u16.to_le_bytes());

        // Calculate checksum (sum of bytes from offset 2 to end, modulo 256)
        let checksum: u8 = header[2..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
        header[1] = checksum;

        // Write header
        self.writer.write_all(&header)?;

        Ok(())
    }

    /// Finish the archive.
    pub fn finish(&mut self) -> Result<()> {
        if !self.finished {
            // Write end marker (0 byte)
            self.writer.write_all(&[0u8])?;
            self.writer.flush()?;
            self.finished = true;
            if let Some(ref handle) = self.progress {
                handle.on_finish();
            }
        }
        Ok(())
    }

    /// Consume the writer and return the inner writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.finish()?;
        let this = std::mem::ManuallyDrop::new(self);
        Ok(unsafe { std::ptr::read(&this.writer) })
    }

    /// Get current Unix timestamp.
    fn current_unix_time() -> u32 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0)
    }
}

impl<W: Write> Drop for LzhWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub mod stream;
pub use stream::{LzhStreamEntry, LzhStreamReader};

/// Open a LZH archive using memory-mapped I/O for efficient large-file reading.
///
/// This is a convenience wrapper around [`LzhReader::new`] that opens the file
/// at `path` with [`oxiarc_core::mmap::MmapReader`], avoiding a full read into
/// memory while still offering random-access semantics.
///
/// # Errors
/// Returns an error if the file cannot be opened, cannot be memory-mapped, or
/// does not contain a valid LZH archive.
///
/// # Example
///
/// ```no_run
/// use oxiarc_archive::lzh::open_lzh_mmap;
///
/// let reader = open_lzh_mmap("large_archive.lzh").unwrap();
/// for entry in reader.entries() {
///     println!("{}", entry.name);
/// }
/// ```
#[cfg(feature = "mmap")]
pub fn open_lzh_mmap<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<LzhReader<oxiarc_core::mmap::MmapReader>> {
    let reader = oxiarc_core::mmap::MmapReader::open(path)?;
    LzhReader::new(reader)
}

#[cfg(test)]
#[cfg(feature = "mmap")]
mod mmap_tests {
    use super::*;
    use std::io::Write;

    /// Write a test LZH to a temp file and return the path.
    fn create_test_lzh_file(name: &str) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("oxiarc_mmap_lzh_test_{}.lzh", name));

        let mut lzh_bytes = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut lzh_bytes);
            writer
                .add_file("hello.txt", b"Hello, mmap!")
                .expect("add_file hello.txt failed");
            writer
                .add_file(
                    "repeat.txt",
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(64).as_slice(),
                )
                .expect("add_file repeat.txt failed");
            writer.finish().expect("finish failed");
        }

        let mut file = std::fs::File::create(&path).expect("create failed");
        file.write_all(&lzh_bytes).expect("write failed");
        file.sync_all().expect("sync failed");
        path
    }

    #[test]
    fn test_mmap_lzh_read() {
        let path = create_test_lzh_file("read");

        let mut reader = open_lzh_mmap(&path).expect("open_lzh_mmap failed");
        let entries = reader.entries();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "hello.txt"));
        assert!(entries.iter().any(|e| e.name == "repeat.txt"));

        let hello = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .cloned()
            .expect("hello.txt entry");
        let data = reader.extract_to_vec(&hello).expect("extract hello.txt");
        assert_eq!(data, b"Hello, mmap!");

        let repeat = entries
            .iter()
            .find(|e| e.name == "repeat.txt")
            .cloned()
            .expect("repeat.txt entry");
        let data = reader.extract_to_vec(&repeat).expect("extract repeat.txt");
        let expected: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(64).to_vec();
        assert_eq!(data, expected);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mmap_lzh_multiple_reads() {
        let path = create_test_lzh_file("multi_read");

        let mut reader = open_lzh_mmap(&path).expect("open_lzh_mmap failed");
        let entries = reader.entries();
        let hello = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .cloned()
            .expect("hello.txt entry");

        let data1 = reader.extract_to_vec(&hello).expect("first extract");
        let data2 = reader.extract_to_vec(&hello).expect("second extract");
        assert_eq!(data1, data2);
        assert_eq!(data1, b"Hello, mmap!");

        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_decode_filename_utf8() {
        let bytes = b"test.txt";
        assert_eq!(LzhHeader::decode_filename(bytes), "test.txt");
    }

    #[test]
    fn test_method_from_id() {
        assert_eq!(LzhMethod::from_id(b"-lh5-"), Some(LzhMethod::Lh5));
        assert_eq!(LzhMethod::from_id(b"-lh0-"), Some(LzhMethod::Lh0));
        assert_eq!(LzhMethod::from_id(b"-xxx-"), None);
    }

    #[test]
    fn test_lzh_writer_single_file_stored() {
        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output);
            writer.set_compression(LzhCompressionLevel::Store);
            writer.add_file("hello.txt", b"Hello, World!").unwrap();
            writer.finish().unwrap();
        }

        // Verify we can read back the archive
        let cursor = Cursor::new(output);
        let mut reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "hello.txt");
        assert_eq!(entries[0].size, 13);

        // Extract and verify content
        let data = reader.extract_to_vec(&entries[0]).unwrap();
        assert_eq!(data, b"Hello, World!");
    }

    #[test]
    fn test_lzh_writer_single_file_compressed() {
        // Use highly repetitive data that compresses well
        let test_data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output);
            // Use LH5 compression (default)
            writer.set_compression(LzhCompressionLevel::Lh5);
            writer.add_file("repeated.txt", test_data).unwrap();
            writer.finish().unwrap();
        }

        // Verify we can read back the archive
        let cursor = Cursor::new(&output);
        let mut reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "repeated.txt");
        assert_eq!(entries[0].size, test_data.len() as u64);

        // Verify compression actually reduced size
        assert!(
            entries[0].compressed_size < entries[0].size,
            "Expected compression to reduce size: {} < {}",
            entries[0].compressed_size,
            entries[0].size
        );

        // Extract and verify content
        let data = reader.extract_to_vec(&entries[0]).unwrap();
        assert_eq!(data.as_slice(), test_data);
    }

    #[test]
    fn test_lzh_writer_multiple_files() {
        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output);
            writer.set_compression(LzhCompressionLevel::Store);
            writer.add_file("file1.txt", b"First file").unwrap();
            writer
                .add_file("file2.txt", b"Second file content")
                .unwrap();
            writer.add_file("file3.txt", b"Third").unwrap();
            writer.finish().unwrap();
        }

        // Verify
        let cursor = Cursor::new(output);
        let mut reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "file1.txt");
        assert_eq!(entries[1].name, "file2.txt");
        assert_eq!(entries[2].name, "file3.txt");

        // Extract all
        let data1 = reader.extract_to_vec(&entries[0]).unwrap();
        let data2 = reader.extract_to_vec(&entries[1]).unwrap();
        let data3 = reader.extract_to_vec(&entries[2]).unwrap();

        assert_eq!(data1, b"First file");
        assert_eq!(data2, b"Second file content");
        assert_eq!(data3, b"Third");
    }

    #[test]
    fn test_lzh_writer_directory() {
        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output);
            writer.add_directory("mydir").unwrap();
            writer.add_file("mydir/file.txt", b"content").unwrap();
            writer.finish().unwrap();
        }

        // Verify
        let cursor = Cursor::new(output);
        let reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "mydir/");
        assert_eq!(entries[1].name, "mydir/file.txt");
    }

    #[test]
    fn test_lzh_roundtrip_large_stored() {
        // Test with various data patterns using Store mode
        // (LH5 encoder is not fully production-ready)
        let test_data = {
            let mut data = Vec::new();
            for i in 0..1000 {
                data.extend_from_slice(format!("Line {} of test data\n", i).as_bytes());
            }
            data
        };

        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output);
            writer.set_compression(LzhCompressionLevel::Store);
            writer.add_file("large.txt", &test_data).unwrap();
            writer.finish().unwrap();
        }

        // Verify archive structure
        let cursor = Cursor::new(&output);
        let mut reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, test_data.len() as u64);
        // Stored mode: compressed size equals original size
        assert_eq!(entries[0].compressed_size, entries[0].size);

        // Extract and verify content
        let extracted = reader.extract_to_vec(&entries[0]).unwrap();
        assert_eq!(extracted, test_data);
    }

    #[test]
    fn test_lzh_writer_into_inner() {
        let output = Vec::new();
        let writer = LzhWriter::new(output);
        let inner = writer.into_inner().unwrap();
        // Should contain at least the end marker
        assert!(!inner.is_empty());
    }

    #[test]
    fn test_lzh_level3_roundtrip() {
        let files: &[(&str, &[u8])] = &[
            ("alpha.txt", b"First file content"),
            ("beta.txt", b"Second file - a bit more text here"),
            ("gamma.txt", b"Third"),
        ];

        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output).with_header_level(3);
            writer.set_compression(LzhCompressionLevel::Store);
            for (name, data) in files {
                writer.add_file(name, data).expect("add_file failed");
            }
            writer.finish().expect("finish failed");
        }

        // Read back with the existing LzhReader
        let cursor = Cursor::new(&output);
        let mut reader = LzhReader::new(cursor).expect("LzhReader::new failed");
        let entries = reader.entries();

        assert_eq!(entries.len(), 3, "expected 3 entries");

        for (i, (name, data)) in files.iter().enumerate() {
            assert_eq!(&entries[i].name, name, "entry {} name mismatch", i);
            assert_eq!(
                entries[i].size,
                data.len() as u64,
                "entry {} size mismatch",
                i
            );
        }

        // Verify content extraction
        for (name, expected) in files {
            let extracted = reader
                .extract_by_name(name)
                .expect("extract_by_name error")
                .expect("entry not found");
            assert_eq!(&extracted, expected, "content mismatch for {}", name);
        }
    }

    #[test]
    fn test_lzh_progress() {
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Sink {
            entries: Mutex<Vec<String>>,
            progress_calls: Mutex<u64>,
            finish_called: Mutex<bool>,
        }

        impl oxiarc_core::progress::ProgressSink for Sink {
            fn on_progress(&self, _processed: u64, _total: Option<u64>) {
                *self.progress_calls.lock().unwrap() += 1;
            }
            fn on_entry(&self, name: &str, _index: u64) {
                self.entries.lock().unwrap().push(name.to_string());
            }
            fn on_finish(&self) {
                *self.finish_called.lock().unwrap() = true;
            }
        }

        let sink = Arc::new(Sink::default());
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        // Write archive
        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output).with_progress(handle);
            writer.set_compression(LzhCompressionLevel::Store);
            writer.add_file("a.txt", b"file a").unwrap();
            writer.add_file("b.txt", b"file b content").unwrap();
            writer.finish().unwrap();
        }

        {
            let entries = sink.entries.lock().unwrap();
            assert_eq!(entries.len(), 2, "expected on_entry called twice");
            assert_eq!(entries[0], "a.txt");
            assert_eq!(entries[1], "b.txt");
        }
        assert!(*sink.finish_called.lock().unwrap(), "on_finish not called");
    }

    #[test]
    fn test_level3_header_parsing() {
        // Build a minimal Level 3 header manually
        // Level 3 format:
        // - Word size (2 bytes): 0x0004
        // - Method (5 bytes): -lh0-
        // - Compressed size (4 bytes)
        // - Original size (4 bytes)
        // - mtime (4 bytes)
        // - Reserved (1 byte): 0x20
        // - Level (1 byte): 0x03
        // - CRC-16 (2 bytes)
        // - OS ID (1 byte): 'U'
        // - Header size (4 bytes)
        // - Next header size (4 bytes)
        // - Extended headers...
        // - Data

        let data = b"Hello, World!";
        let crc16 = Crc16::compute(data);

        let mut archive = Vec::new();

        // Word size = 4
        archive.extend_from_slice(&[0x04, 0x00]);

        // Method: -lh0-
        archive.extend_from_slice(b"-lh0-");

        // Compressed size
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());

        // Original size
        archive.extend_from_slice(&(data.len() as u32).to_le_bytes());

        // mtime (Unix timestamp)
        archive.extend_from_slice(&0u32.to_le_bytes());

        // Reserved = 0x20, Level = 3
        archive.extend_from_slice(&[0x20, 0x03]);

        // CRC-16
        archive.extend_from_slice(&crc16.to_le_bytes());

        // OS ID = 'U' for Unix
        archive.push(b'U');

        // Total header size (we'll calculate this)
        let filename = b"test.txt";
        // Fixed part: 28 bytes, plus ext header size field (4), plus filename ext header
        // Filename ext header: size(4) + type(1) + name
        let filename_ext_size = 1 + filename.len() as u32;
        // We need: fixed 28 + next_size(4) + filename_header + next_size(4) for terminator
        let total_header = 28 + 4 + filename_ext_size + 4;
        archive.extend_from_slice(&total_header.to_le_bytes());

        // Next extended header size (filename header)
        archive.extend_from_slice(&filename_ext_size.to_le_bytes());

        // Filename extended header: type 0x01 + filename
        archive.push(0x01);
        archive.extend_from_slice(filename);

        // Next extended header size = 0 (end of extended headers)
        archive.extend_from_slice(&0u32.to_le_bytes());

        // Data
        archive.extend_from_slice(data);

        // End of archive marker
        archive.push(0x00);

        // Parse the archive
        let cursor = Cursor::new(archive);
        let mut reader = LzhReader::new(cursor).unwrap();
        let entries = reader.entries();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "test.txt");
        assert_eq!(entries[0].size, data.len() as u64);

        // Extract and verify
        let extracted = reader.extract_to_vec(&entries[0]).unwrap();
        assert_eq!(extracted, data);
    }

    // ---- Lenient-mode tests ----

    /// Build a 3-entry LZH archive with the compressed payload of the
    /// SECOND entry corrupted (one byte flipped). The first and third
    /// entries remain intact. In lenient mode, all three entries must
    /// enumerate and extract; extracting entry 2 must surface a
    /// `CrcMismatch` warning but still return (corrupted) bytes.
    #[test]
    fn test_lzh_lenient_bad_crc() {
        let mut output = Vec::new();
        {
            let mut writer = LzhWriter::new(&mut output);
            writer.set_compression(LzhCompressionLevel::Store);
            writer.add_file("a.txt", b"alpha entry").unwrap();
            writer.add_file("b.txt", b"bravo entry (corrupt)").unwrap();
            writer.add_file("c.txt", b"charlie entry").unwrap();
            writer.finish().unwrap();
        }

        // Parse the archive to locate the second entry's data offset.
        let entries_offset = {
            let cursor = Cursor::new(output.clone());
            let reader = LzhReader::new(cursor).expect("intact LzhReader::new");
            let entries = reader.entries();
            assert_eq!(entries.len(), 3);
            assert_eq!(entries[1].name, "b.txt");
            entries[1].offset
        };

        // Flip the first byte of the second entry's stored payload.
        let corrupt_idx = entries_offset as usize;
        assert!(corrupt_idx < output.len(), "data offset in bounds");
        output[corrupt_idx] ^= 0xFF;

        // Strict: extracting entry 2 must return CorruptedData.
        {
            let cursor = Cursor::new(output.clone());
            let mut strict = LzhReader::new(cursor).expect("strict new");
            let entries = strict.entries();
            let second = entries[1].clone();

            let err = strict
                .extract_to_vec(&second)
                .expect_err("strict extract must fail with CorruptedData on bad data CRC");
            match err {
                OxiArcError::CorruptedData { .. } => {}
                other => panic!("unexpected error variant: {:?}", other),
            }
        }

        // Lenient: all 3 entries enumerate and extract; exactly one
        // warning emitted (for entry 2).
        {
            let cursor = Cursor::new(output);
            let mut lenient = LzhReader::new(cursor).expect("lenient new").lenient(true);
            let entries = lenient.entries();
            assert_eq!(entries.len(), 3);

            // Extract all three; only entry 2 is corrupted, so only
            // that extraction records a warning.
            let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
            let first_entry = entries[0].clone();
            let second_entry = entries[1].clone();
            let third_entry = entries[2].clone();

            let a = lenient
                .extract_to_vec(&first_entry)
                .expect("extract a.txt in lenient mode");
            assert_eq!(a, b"alpha entry");

            let b = lenient
                .extract_to_vec(&second_entry)
                .expect("lenient extract must succeed even on corrupted entry 2");
            assert_eq!(
                b.len(),
                b"bravo entry (corrupt)".len(),
                "payload length matches original even when corrupted"
            );

            let c = lenient
                .extract_to_vec(&third_entry)
                .expect("extract c.txt in lenient mode");
            assert_eq!(c, b"charlie entry");

            let warnings = lenient.warnings();
            assert_eq!(
                warnings.len(),
                1,
                "exactly one CRC-16 warning for entry 2 — entries 1 & 3 are clean"
            );
            assert_eq!(warnings[0].format, "LZH");
            assert_eq!(warnings[0].entry_name.as_deref(), Some("b.txt"));
            match warnings[0].kind {
                LenientWarningKind::CrcMismatch { .. } => {}
                ref other => panic!("unexpected warning kind: {:?}", other),
            }
            // Tie the `names` binding into the assertion chain so the
            // compiler doesn't flag it as unused.
            assert_eq!(names[0], "a.txt");
        }
    }
}

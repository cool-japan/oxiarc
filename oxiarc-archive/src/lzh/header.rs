//! LZH header types and parsing logic.
//!
//! Provides [`LzhHeader`], the per-entry header struct read from LZH archives,
//! and the private `LzhExtensionData` accumulator that collects extension-header
//! metadata during header parsing.

use crate::lzh::name_codec;
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
    /// Compressed size (32-bit field from base header).
    pub compressed_size: u32,
    /// Original (uncompressed) size (32-bit field from base header).
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

    /// OS/2 / MS-DOS attribute word parsed from extension header `0x40` (u16 LE).
    pub dos_attr: Option<u16>,
    /// Windows FILETIME creation timestamp from extension header `0x41` (first 8 bytes).
    pub windows_creation: Option<u64>,
    /// Windows FILETIME last-access timestamp from extension header `0x41` (second 8 bytes).
    pub windows_access: Option<u64>,
    /// Windows FILETIME last-modify timestamp from extension header `0x41` (third 8 bytes).
    pub windows_modify: Option<u64>,
    /// Uncompressed size as 64-bit value from extension header `0x42`.
    pub uncompressed_size64: Option<u64>,
    /// Compressed size as 64-bit value from extension header `0x43`.
    pub compressed_size64: Option<u64>,
    /// Free-form comment parsed from extension header `0x44`.
    pub comment: Option<String>,
    /// Unix file permissions parsed from extension header `0x46` (u16 LE).
    pub unix_permission: Option<u16>,
    /// Unix owner user name parsed from extension header `0x50`.
    pub unix_owner_user: Option<String>,
    /// Unix owner group name parsed from extension header `0x50`.
    pub unix_owner_group: Option<String>,
    /// Unix UID parsed from extension header `0x51` (first u32 LE).
    pub unix_uid: Option<u32>,
    /// Unix GID parsed from extension header `0x51` (second u32 LE).
    pub unix_gid: Option<u32>,
    /// Unix mtime parsed from extension header `0x54` (u32 LE, seconds since epoch).
    pub unix_mtime: Option<u32>,
}

/// Accumulator for extension-header metadata parsed from a Level-1,
/// Level-2 or Level-3 header. Populated by [`LzhExtensionData::apply`] as
/// each extension block is decoded; copied into [`LzhHeader`] at the end.
#[derive(Debug, Default)]
pub(crate) struct LzhExtensionData {
    /// Filename from extension header `0x01`.
    pub(crate) filename: Option<String>,
    /// Directory path from extension header `0x02` (components joined with `/`).
    pub(crate) dirname: Option<String>,
    pub(crate) dos_attr: Option<u16>,
    pub(crate) windows_creation: Option<u64>,
    pub(crate) windows_access: Option<u64>,
    pub(crate) windows_modify: Option<u64>,
    pub(crate) uncompressed_size64: Option<u64>,
    pub(crate) compressed_size64: Option<u64>,
    pub(crate) comment: Option<String>,
    pub(crate) unix_permission: Option<u16>,
    pub(crate) unix_owner_user: Option<String>,
    pub(crate) unix_owner_group: Option<String>,
    pub(crate) unix_uid: Option<u32>,
    pub(crate) unix_gid: Option<u32>,
    pub(crate) unix_mtime: Option<u32>,
}

impl LzhExtensionData {
    /// Decode a single `[type + data]` extension header payload and
    /// fold the contained value into `self`.
    ///
    /// Type byte assignments follow the standard LHA specification:
    ///
    /// | Type | Meaning                  | Payload                              |
    /// |------|--------------------------|--------------------------------------|
    /// | 0x40 | OS/2 / MS-DOS attributes | 2 bytes LE u16                       |
    /// | 0x41 | Windows timestamps       | 24 bytes (3 × u64 LE FILETIME)       |
    /// | 0x42 | Uncompressed size 64-bit | 8 bytes LE u64                       |
    /// | 0x43 | Compressed size 64-bit   | 8 bytes LE u64                       |
    /// | 0x44 | Comment                  | variable UTF-8                       |
    /// | 0x46 | Unix file permissions    | 2 bytes LE u16                       |
    /// | 0x50 | Unix owner names         | user\0group (NUL-separated strings)  |
    /// | 0x51 | Unix owner IDs           | 8 bytes: uid(4 LE) + gid(4 LE)       |
    /// | 0x54 | Unix mtime               | 4 bytes LE u32 seconds-since-epoch   |
    pub(crate) fn apply(&mut self, ext_type: u8, data: &[u8]) {
        match ext_type {
            // 0x01 — filename (Shift_JIS per LZH convention)
            0x01 => {
                self.filename = Some(name_codec::decode_lzh_name(data));
            }
            // 0x02 — directory path (components terminated by 0xFF)
            0x02 => {
                self.dirname = Some(name_codec::decode_lzh_dirname(data));
            }
            // 0x40 — OS/2 / MS-DOS attribute word (2 bytes LE)
            0x40 if data.len() >= 2 => {
                self.dos_attr = Some(u16::from_le_bytes([data[0], data[1]]));
            }
            // 0x41 — Windows FILETIME × 3 (24 bytes = 3 × u64 LE)
            0x41 if data.len() >= 24 => {
                self.windows_creation = Some(u64::from_le_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]));
                self.windows_access = Some(u64::from_le_bytes([
                    data[8], data[9], data[10], data[11], data[12], data[13], data[14], data[15],
                ]));
                self.windows_modify = Some(u64::from_le_bytes([
                    data[16], data[17], data[18], data[19], data[20], data[21], data[22], data[23],
                ]));
            }
            // 0x42 — uncompressed size as u64 (8 bytes LE)
            0x42 if data.len() >= 8 => {
                self.uncompressed_size64 = Some(u64::from_le_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]));
            }
            // 0x43 — compressed size as u64 (8 bytes LE)
            0x43 if data.len() >= 8 => {
                self.compressed_size64 = Some(u64::from_le_bytes([
                    data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
                ]));
            }
            // 0x44 — comment (variable-length UTF-8)
            0x44 => {
                self.comment = Some(String::from_utf8_lossy(data).into_owned());
            }
            // 0x46 — Unix file permissions (2 bytes LE u16)
            0x46 if data.len() >= 2 => {
                self.unix_permission = Some(u16::from_le_bytes([data[0], data[1]]));
            }
            // 0x50 — Unix owner names: NUL-separated "user\0group"
            0x50 => {
                if let Some(nul_pos) = data.iter().position(|&b| b == 0) {
                    let user = String::from_utf8_lossy(&data[..nul_pos]).into_owned();
                    let group = String::from_utf8_lossy(&data[nul_pos + 1..]).into_owned();
                    self.unix_owner_user = Some(user);
                    self.unix_owner_group = Some(group);
                } else {
                    // No NUL separator — treat the whole payload as username
                    self.unix_owner_user = Some(String::from_utf8_lossy(data).into_owned());
                }
            }
            // 0x51 — Unix owner IDs: uid(u32 LE) + gid(u32 LE)
            0x51 if data.len() >= 8 => {
                self.unix_uid = Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
                self.unix_gid = Some(u32::from_le_bytes([data[4], data[5], data[6], data[7]]));
            }
            // 0x54 — Unix mtime (4 bytes LE u32, seconds since epoch)
            0x54 if data.len() >= 4 => {
                self.unix_mtime = Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]));
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

/// Read a 2-byte-sized extension-header chain (levels 1 and 2).
///
/// Each block is `[type(1)][data(N-3)][next_size(2)]` where `N` is the size
/// declared by the previous block (or the base header's first-size field).
/// Returns the total number of bytes consumed.
fn read_ext_chain<R: Read>(
    reader: &mut R,
    first_size: u16,
    ext_data: &mut LzhExtensionData,
) -> Result<u64> {
    let mut total: u64 = 0;
    let mut size = first_size as usize;
    while size > 0 {
        if size < 3 {
            return Err(OxiArcError::invalid_header(
                "LZH extension header size too small",
            ));
        }
        let mut block = vec![0u8; size];
        reader.read_exact(&mut block)?;
        total += size as u64;

        let ext_type = block[0];
        let data = &block[1..size - 2];
        ext_data.apply(ext_type, data);

        size = u16::from_le_bytes([block[size - 2], block[size - 1]]) as usize;
    }
    Ok(total)
}

/// Assemble the final entry name from the inline name and the 0x01/0x02
/// extension headers.
fn assemble_name(inline_name: String, ext: &LzhExtensionData) -> String {
    let file = ext.filename.clone().unwrap_or(inline_name);
    match ext.dirname.as_deref() {
        Some(dir) if !dir.is_empty() && file.is_empty() => dir.to_string(),
        Some(dir) if !dir.is_empty() => format!("{}/{}", dir, file),
        _ => file,
    }
}

/// Result of parsing the level-specific tail of a header.
struct ParsedTail {
    /// Inline filename (levels 0/1) — may be overridden by ext header 0x01.
    inline_name: String,
    crc16: u16,
    os_id: u8,
    /// Bytes consumed from the reader *after* the fixed 21-byte prefix.
    consumed_after_base: u64,
    /// Total size of the extension-header chain. For level-1 headers this
    /// amount is included in the base header's "compressed size" (which is
    /// really a *skip* size) and must be subtracted to get the data size.
    ext_total: u64,
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

        // Level 0/1: first byte is header size; level 2: first two bytes are
        // the total header size (u16 LE). A zero first byte marks the end of
        // the archive (which is why level-2 writers pad headers whose total
        // size is a multiple of 256).
        let header_size = first_buf[0];
        if header_size == 0 {
            return Ok(None); // End of archive
        }

        // Read method ID (5 bytes)
        let mut method_buf = [0u8; 5];
        reader.read_exact(&mut method_buf)?;

        // Unknown method IDs must not abort the archive scan: real-world
        // archives mix methods, and unsupported entries are listed and only
        // rejected at extraction time.
        let method = LzhMethod::from_id_lossy(method_buf);

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
        let tail = match level {
            0 => Self::parse_level0(reader, header_size, &mut ext_data)?,
            1 => Self::parse_level1(reader, header_size, &mut ext_data)?,
            2 => {
                let total_size = u16::from_le_bytes(first_buf);
                Self::parse_level2(reader, total_size, &mut ext_data)?
            }
            _ => {
                return Err(OxiArcError::invalid_header(format!(
                    "Unsupported header level: {}",
                    level
                )));
            }
        };

        // Calculate data offset: fixed 21-byte prefix plus whatever the
        // level-specific tail consumed (the parsers consume padding and
        // extension bytes exactly, so this also holds for pure `Read`
        // streams without seeking).
        let data_offset = offset + 21 + tail.consumed_after_base;

        // For level-1 headers, the base "compressed size" field is a skip
        // size that includes the extension-header chain.
        let compressed_size = if level == 1 {
            compressed_size
                .checked_sub(u32::try_from(tail.ext_total).unwrap_or(u32::MAX))
                .ok_or_else(|| {
                    OxiArcError::invalid_header("Level 1 skip size smaller than extension chain")
                })?
        } else {
            compressed_size
        };

        // Assemble the final name from the inline name and any 0x01/0x02
        // extension headers.
        let filename = assemble_name(tail.inline_name, &ext_data);

        // Level-2 headers store the total header size in the first two
        // bytes; levels 0/1 store the base header size byte.
        let header_size_field = if level == 2 {
            u16::from_le_bytes(first_buf)
        } else {
            header_size as u16
        };

        Ok(Some(Self {
            header_size: header_size_field,
            method,
            compressed_size,
            original_size,
            mtime,
            attributes,
            level,
            filename,
            crc16: tail.crc16,
            os_id: tail.os_id,
            data_offset,
            dos_attr: ext_data.dos_attr,
            windows_creation: ext_data.windows_creation,
            windows_access: ext_data.windows_access,
            windows_modify: ext_data.windows_modify,
            uncompressed_size64: ext_data.uncompressed_size64,
            compressed_size64: ext_data.compressed_size64,
            comment: ext_data.comment,
            unix_permission: ext_data.unix_permission,
            unix_owner_user: ext_data.unix_owner_user,
            unix_owner_group: ext_data.unix_owner_group,
            unix_uid: ext_data.unix_uid,
            unix_gid: ext_data.unix_gid,
            unix_mtime: ext_data.unix_mtime,
        }))
    }

    /// Read a Level 3 header.
    /// Level 3 uses word-sized fields and 4-byte extended header sizes.
    fn read_level3<R: Read>(reader: &mut R, offset: u64) -> Result<Option<Self>> {
        // Word size already read (0x0004), now read method ID (5 bytes)
        let mut method_buf = [0u8; 5];
        reader.read_exact(&mut method_buf)?;

        // Unknown methods are listed and only rejected at extraction time.
        let method = LzhMethod::from_id_lossy(method_buf);

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

        let mut ext_data = LzhExtensionData::default();

        // Read extended headers (4-byte size fields in Level 3)
        while next_size > 0 {
            let mut header = vec![0u8; next_size as usize];
            reader.read_exact(&mut header)?;

            if !header.is_empty() {
                let header_type = header[0];
                let data = &header[1..];
                ext_data.apply(header_type, data);
            }

            // Read next extended header size (4 bytes in Level 3)
            if reader.read_exact(&mut size_buf).is_ok() {
                next_size = u32::from_le_bytes(size_buf);
            } else {
                break;
            }
        }

        let filename = assemble_name(String::new(), &ext_data);

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
            windows_creation: ext_data.windows_creation,
            windows_access: ext_data.windows_access,
            windows_modify: ext_data.windows_modify,
            uncompressed_size64: ext_data.uncompressed_size64,
            compressed_size64: ext_data.compressed_size64,
            comment: ext_data.comment,
            unix_permission: ext_data.unix_permission,
            unix_owner_user: ext_data.unix_owner_user,
            unix_owner_group: ext_data.unix_owner_group,
            unix_uid: ext_data.unix_uid,
            unix_gid: ext_data.unix_gid,
            unix_mtime: ext_data.unix_mtime,
        }))
    }

    /// Parse level 0 header.
    ///
    /// `header_size` is the byte at offset 0: the size of the header
    /// starting at offset 2 (method ID) up to the end of the extension
    /// area. Any bytes after the CRC (OS-specific extension area) are
    /// consumed and ignored.
    fn parse_level0<R: Read>(
        reader: &mut R,
        header_size: u8,
        _ext_data: &mut LzhExtensionData,
    ) -> Result<ParsedTail> {
        // Filename length
        let mut len_buf = [0u8; 1];
        reader.read_exact(&mut len_buf)?;
        let filename_len = len_buf[0] as usize;

        // Filename (level 0 names may contain '\'-separated paths)
        let mut filename_buf = vec![0u8; filename_len];
        reader.read_exact(&mut filename_buf)?;
        let filename = Self::decode_filename(&filename_buf).replace('\\', "/");

        // CRC-16
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        // Skip the OS-specific extension area (if any) so that the reader
        // ends up exactly at the start of the compressed data.
        let consumed_in_header = 19 + 1 + filename_len + 2; // from offset 2
        let extra = (header_size as usize)
            .checked_sub(consumed_in_header)
            .ok_or_else(|| OxiArcError::invalid_header("Level 0 header size too small"))?;
        if extra > 0 {
            let mut skip = vec![0u8; extra];
            reader.read_exact(&mut skip)?;
        }

        Ok(ParsedTail {
            inline_name: filename,
            crc16,
            os_id: 0,
            consumed_after_base: (1 + filename_len + 2 + extra) as u64,
            ext_total: 0,
        })
    }

    /// Parse level 1 header.
    ///
    /// `header_size` is the byte at offset 0: the size of the *base* header
    /// counted from offset 0 (size and checksum included), excluding the
    /// extension-header chain that follows it. The extension chain's total
    /// size is included in the base header's "compressed size" field and is
    /// returned via [`ParsedTail::ext_total`].
    fn parse_level1<R: Read>(
        reader: &mut R,
        header_size: u8,
        ext_data: &mut LzhExtensionData,
    ) -> Result<ParsedTail> {
        // Filename length
        let mut len_buf = [0u8; 1];
        reader.read_exact(&mut len_buf)?;
        let filename_len = len_buf[0] as usize;

        // Filename
        let mut filename_buf = vec![0u8; filename_len];
        reader.read_exact(&mut filename_buf)?;
        let filename = Self::decode_filename(&filename_buf).replace('\\', "/");

        // CRC-16
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        // OS ID
        let mut os_buf = [0u8; 1];
        reader.read_exact(&mut os_buf)?;
        let os_id = os_buf[0];

        // Skip any surplus bytes inside the base header (rare, but legal).
        let consumed = 2 + 19 + 1 + filename_len + 2 + 1; // from offset 0
        let extra = (header_size as usize)
            .checked_sub(consumed)
            .ok_or_else(|| OxiArcError::invalid_header("Level 1 header size too small"))?;
        if extra > 0 {
            let mut skip = vec![0u8; extra];
            reader.read_exact(&mut skip)?;
        }

        // First extension-header size, then the chain. Each chained block
        // is `[type(1)][data(size-3)][next_size(2)]` — the declared size
        // includes the trailing next-size field (LHa for UNIX convention).
        let mut first_buf = [0u8; 2];
        reader.read_exact(&mut first_buf)?;
        let first_size = u16::from_le_bytes(first_buf);
        let ext_total = read_ext_chain(reader, first_size, ext_data)?;

        Ok(ParsedTail {
            inline_name: filename,
            crc16,
            os_id,
            consumed_after_base: (1 + filename_len + 2 + 1 + extra + 2) as u64 + ext_total,
            ext_total,
        })
    }

    /// Parse level 2 header.
    ///
    /// `total_size` is the u16 at offset 0: the size of the *entire* header
    /// including the extension chain and any padding byte. The reader is
    /// left positioned exactly at the start of the compressed data.
    fn parse_level2<R: Read>(
        reader: &mut R,
        total_size: u16,
        ext_data: &mut LzhExtensionData,
    ) -> Result<ParsedTail> {
        // CRC-16
        let mut crc_buf = [0u8; 2];
        reader.read_exact(&mut crc_buf)?;
        let crc16 = u16::from_le_bytes(crc_buf);

        // OS ID
        let mut os_buf = [0u8; 1];
        reader.read_exact(&mut os_buf)?;
        let os_id = os_buf[0];

        // First extension-header size, then the chain (same block layout as
        // level 1: the declared size includes the trailing next-size field).
        let mut next_size_buf = [0u8; 2];
        reader.read_exact(&mut next_size_buf)?;
        let first_size = u16::from_le_bytes(next_size_buf);
        let ext_total = read_ext_chain(reader, first_size, ext_data)?;

        // Consume padding up to the declared total header size (writers pad
        // headers whose total size is a multiple of 256 to avoid a zero
        // low byte, which would read as the end-of-archive marker).
        let consumed = 21 + 2 + 1 + 2 + ext_total; // from offset 0
        let padding = u64::from(total_size)
            .checked_sub(consumed)
            .ok_or_else(|| OxiArcError::invalid_header("Level 2 header size too small"))?;
        if padding > 0 {
            let mut skip = vec![0u8; padding as usize];
            reader.read_exact(&mut skip)?;
        }

        Ok(ParsedTail {
            inline_name: String::new(),
            crc16,
            os_id,
            consumed_after_base: 2 + 1 + 2 + ext_total + padding,
            ext_total,
        })
    }

    /// Decode filename from bytes (Shift_JIS first, UTF-8 fallback).
    pub(crate) fn decode_filename(bytes: &[u8]) -> String {
        name_codec::decode_lzh_name(bytes)
    }

    /// Convert to Entry.
    ///
    /// Extension-header metadata is merged into the returned [`Entry`]:
    /// - `dos_attr` (0x40, u16) low byte shadows the fixed-header attribute byte
    /// - `unix_uid`/`unix_gid` (0x51, u32) populate [`FileAttributes::uid`]/[`FileAttributes::gid`]
    /// - `unix_permission` (0x46) populates [`FileAttributes::unix_mode`]
    /// - `unix_mtime` (0x54) replaces the fixed-header mtime when present
    /// - `uncompressed_size64` (0x42) overrides the 32-bit size when present
    /// - `comment` (0x44) is surfaced on the entry directly
    pub fn to_entry(&self) -> Entry {
        // `-lhd-` marks a directory entry regardless of name shape; a
        // trailing separator also marks one (legacy writers).
        let is_dir = self.method.is_directory()
            || self.filename.ends_with('/')
            || self.filename.ends_with('\\');
        let entry_type = if is_dir {
            EntryType::Directory
        } else {
            EntryType::File
        };

        let method = match self.method {
            LzhMethod::Lh0 => CoreMethod::Lh0,
            LzhMethod::Lh1 => CoreMethod::Lh1,
            LzhMethod::Lh2 => CoreMethod::Lh2,
            LzhMethod::Lh3 => CoreMethod::Lh3,
            LzhMethod::Lh4 => CoreMethod::Lh4,
            LzhMethod::Lh5 => CoreMethod::Lh5,
            LzhMethod::Lh6 => CoreMethod::Lh6,
            LzhMethod::Lh7 => CoreMethod::Lh7,
            LzhMethod::Lhd => CoreMethod::Lhd,
            LzhMethod::Lzs => CoreMethod::Lzs,
            LzhMethod::Lz4 => CoreMethod::Lz4,
            LzhMethod::Lz5 => CoreMethod::Lz5,
            LzhMethod::Pm0 => CoreMethod::Pm0,
            LzhMethod::Unknown(_) => CoreMethod::Unknown(0),
            // `LzhMethod` is `#[non_exhaustive]`. A method variant added by a
            // future oxiarc-lzhuf release that this mapping does not know about
            // must degrade to the "unknown/unsupported" core method — the same
            // path as an unrecognised `-lhX-` ID — so extraction rejects it
            // rather than silently decoding with the wrong codec.
            _ => CoreMethod::Unknown(0),
        };

        // Prefer extension-provided Unix mtime (0x54) over the fixed-header
        // value when both are present.
        let mtime_secs = self
            .unix_mtime
            .map(|m| m as u64)
            .unwrap_or(self.mtime as u64);

        // Prefer 64-bit uncompressed size (0x42) when present; fall back to
        // the 32-bit base-header field.
        let uncompressed = self
            .uncompressed_size64
            .unwrap_or(self.original_size as u64);

        // Prefer 64-bit compressed size (0x43) when present; fall back to
        // the 32-bit base-header field.
        let compressed = self
            .compressed_size64
            .unwrap_or(self.compressed_size as u64);

        // dos_attr (0x40) is a u16 attribute word; take the low byte for
        // FileAttributes which stores a u8 DOS attribute byte.  Fall back
        // to the base-header attribute when no extension is present.
        let dos_attributes = self.dos_attr.map(|w| w as u8).or(Some(self.attributes));

        Entry {
            name: {
                // Canonicalise: '/' separators, and directories always end
                // with '/' (real `-lhd-` entries usually carry a bare path).
                let mut name = self.filename.replace('\\', "/");
                if is_dir && !name.is_empty() && !name.ends_with('/') {
                    name.push('/');
                }
                name
            },
            entry_type,
            size: if is_dir { 0 } else { uncompressed },
            compressed_size: if is_dir { 0 } else { compressed },
            method,
            modified: Some(UNIX_EPOCH + Duration::from_secs(mtime_secs)),
            created: None,
            accessed: None,
            attributes: FileAttributes {
                dos_attributes,
                unix_mode: self.unix_permission.map(u32::from),
                uid: self.unix_uid,
                gid: self.unix_gid,
            },
            crc32: None, // LZH uses CRC-16
            comment: self.comment.clone(),
            link_target: None,
            offset: self.data_offset,
            extra: Vec::new(),
        }
    }
}

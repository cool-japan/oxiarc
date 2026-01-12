//! LZH/LHA archive format support.
//!
//! This module provides reading, writing, and extraction of LZH archives with support for
//! header levels 0, 1, and 2.

use encoding_rs::SHIFT_JIS;
use oxiarc_core::entry::CompressionMethod as CoreMethod;
use oxiarc_core::error::{OxiArcError, Result};
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
        let (filename, crc16, os_id, extra_size) = match level {
            0 => Self::parse_level0(reader, &mut [0u8; 256])?,
            1 => Self::parse_level1(reader)?,
            2 => Self::parse_level2(reader)?,
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
                    _ => {} // Skip unknown headers
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
    fn parse_level2<R: Read>(reader: &mut R) -> Result<(String, u16, u8, usize)> {
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
                    _ => {} // Skip unknown headers
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

        Entry {
            name: self.filename.replace('\\', "/"),
            entry_type,
            size: self.original_size as u64,
            compressed_size: self.compressed_size as u64,
            method,
            modified: Some(UNIX_EPOCH + Duration::from_secs(self.mtime as u64)),
            created: None,
            accessed: None,
            attributes: FileAttributes {
                dos_attributes: Some(self.attributes),
                ..Default::default()
            },
            crc32: None, // LZH uses CRC-16
            comment: None,
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
}

impl<R: Read + Seek> LzhReader<R> {
    /// Create a new LZH reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let entries = Self::read_entries(&mut reader)?;
        Ok(Self { reader, entries })
    }

    /// Read all entries.
    fn read_entries(reader: &mut R) -> Result<Vec<LzhEntryInfo>> {
        let mut entries = Vec::new();
        let mut offset = 0u64;

        while let Some(header) = LzhHeader::read(reader, offset)? {
            let entry = header.to_entry();
            let method = header.method;
            let crc16 = header.crc16;
            let compressed_size = header.compressed_size;

            // Skip compressed data using seek
            reader.seek(SeekFrom::Current(header.compressed_size as i64))?;

            offset = header.data_offset + header.compressed_size as u64;

            entries.push(LzhEntryInfo {
                entry,
                method,
                crc16,
                compressed_size,
            });
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
            return Err(OxiArcError::corrupted(
                entry.offset,
                format!(
                    "CRC-16 mismatch: expected {:04X}, computed {:04X}",
                    info.crc16, computed_crc
                ),
            ));
        }

        // Write to output
        writer.write_all(&decompressed)?;

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
}

impl<W: Write> LzhWriter<W> {
    /// Create a new LZH writer with default compression (lh5).
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            compression: LzhCompressionLevel::default(),
            finished: false,
        }
    }

    /// Set the compression level for subsequent files.
    pub fn set_compression(&mut self, level: LzhCompressionLevel) {
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
        compression: LzhCompressionLevel,
    ) -> Result<()> {
        let method = match compression {
            LzhCompressionLevel::Store => LzhMethod::Lh0,
            LzhCompressionLevel::Lh5 => LzhMethod::Lh5,
        };

        // Compress the data
        let compressed = if method == LzhMethod::Lh0 {
            data.to_vec()
        } else {
            let comp = encode_lzh(data, method)?;
            // Only use compression if it actually reduces size
            if comp.len() < data.len() {
                comp
            } else {
                // Fall back to stored
                return self.add_file_with_options(name, data, LzhCompressionLevel::Store);
            }
        };

        let crc16 = Crc16::compute(data);
        let mtime = Self::current_unix_time();

        // Write level 1 header
        self.write_level1_header(name, &compressed, data.len() as u32, crc16, mtime, method)?;

        // Write compressed data
        self.writer.write_all(&compressed)?;

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

        // Write level 1 header for empty directory
        self.write_level1_header(&dir_name, &[], 0, 0, mtime, LzhMethod::Lh0)?;

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
}

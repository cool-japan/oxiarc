//! TAR archive format support.
//!
//! This module provides reading and extraction of TAR archives with support for:
//! - UStar format (POSIX.1-1988)
//! - PAX extended headers (POSIX.1-2001) for long filenames and additional metadata

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Entry, EntryType, FileAttributes};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, UNIX_EPOCH};

/// TAR block size.
pub const BLOCK_SIZE: usize = 512;

/// PAX typeflag for extended header (applies to next file only).
const PAX_HEADER: u8 = b'x';

/// PAX typeflag for global extended header (applies to all subsequent files).
const PAX_GLOBAL_HEADER: u8 = b'g';

/// GNU LongName typeflag.
const GNU_LONGNAME: u8 = b'L';

/// GNU LongLink typeflag.
const GNU_LONGLINK: u8 = b'K';

/// TAR header.
#[derive(Debug, Clone)]
pub struct TarHeader {
    /// File name.
    pub name: String,
    /// File mode.
    pub mode: u32,
    /// Owner UID.
    pub uid: u32,
    /// Owner GID.
    pub gid: u32,
    /// File size.
    pub size: u64,
    /// Modification time.
    pub mtime: u64,
    /// Type flag.
    pub typeflag: u8,
    /// Link name.
    pub linkname: String,
    /// UStar indicator.
    pub ustar: bool,
    /// Owner name.
    pub uname: String,
    /// Group name.
    pub gname: String,
    /// Prefix for long names.
    pub prefix: String,
}

impl TarHeader {
    /// Read a TAR header from a block.
    pub fn from_block(block: &[u8; BLOCK_SIZE]) -> Result<Option<Self>> {
        // Check for empty block (end of archive)
        if block.iter().all(|&b| b == 0) {
            return Ok(None);
        }

        // Parse fields
        let name = Self::parse_string(&block[0..100]);
        let mode = Self::parse_octal(&block[100..108])?;
        let uid = Self::parse_octal(&block[108..116])?;
        let gid = Self::parse_octal(&block[116..124])?;
        let size = Self::parse_octal_u64(&block[124..136])?;
        let mtime = Self::parse_octal_u64(&block[136..148])?;
        let _checksum = Self::parse_octal(&block[148..156])?;
        let typeflag = block[156];
        let linkname = Self::parse_string(&block[157..257]);

        // Check for UStar format
        let ustar = &block[257..262] == b"ustar";

        let (uname, gname, prefix) = if ustar {
            (
                Self::parse_string(&block[265..297]),
                Self::parse_string(&block[297..329]),
                Self::parse_string(&block[345..500]),
            )
        } else {
            (String::new(), String::new(), String::new())
        };

        // Combine prefix and name
        let full_name = if prefix.is_empty() {
            name
        } else {
            format!("{}/{}", prefix, name)
        };

        Ok(Some(Self {
            name: full_name,
            mode,
            uid,
            gid,
            size,
            mtime,
            typeflag,
            linkname,
            ustar,
            uname,
            gname,
            prefix: String::new(),
        }))
    }

    /// Parse a null-terminated string.
    fn parse_string(data: &[u8]) -> String {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        String::from_utf8_lossy(&data[..end]).into_owned()
    }

    /// Parse an octal number.
    fn parse_octal(data: &[u8]) -> Result<u32> {
        let s = Self::parse_string(data);
        let s = s.trim();
        if s.is_empty() {
            return Ok(0);
        }
        u32::from_str_radix(s, 8)
            .map_err(|_| OxiArcError::invalid_header(format!("Invalid octal: {}", s)))
    }

    /// Parse an octal number as u64.
    fn parse_octal_u64(data: &[u8]) -> Result<u64> {
        let s = Self::parse_string(data);
        let s = s.trim();
        if s.is_empty() {
            return Ok(0);
        }
        u64::from_str_radix(s, 8)
            .map_err(|_| OxiArcError::invalid_header(format!("Invalid octal: {}", s)))
    }

    /// Get entry type.
    pub fn entry_type(&self) -> EntryType {
        match self.typeflag {
            b'0' | 0 => EntryType::File,
            b'5' => EntryType::Directory,
            b'1' => EntryType::Hardlink,
            b'2' => EntryType::Symlink,
            _ => EntryType::Unknown,
        }
    }

    /// Check if this is a PAX extended header.
    pub fn is_pax_header(&self) -> bool {
        self.typeflag == PAX_HEADER
    }

    /// Check if this is a PAX global extended header.
    pub fn is_pax_global_header(&self) -> bool {
        self.typeflag == PAX_GLOBAL_HEADER
    }

    /// Check if this is a GNU LongName header.
    pub fn is_gnu_longname(&self) -> bool {
        self.typeflag == GNU_LONGNAME
    }

    /// Check if this is a GNU LongLink header.
    pub fn is_gnu_longlink(&self) -> bool {
        self.typeflag == GNU_LONGLINK
    }

    /// Apply PAX extended attributes to this header.
    pub fn apply_pax_attrs(&mut self, attrs: &HashMap<String, String>) {
        if let Some(path) = attrs.get("path") {
            self.name = path.clone();
        }
        if let Some(linkpath) = attrs.get("linkpath") {
            self.linkname = linkpath.clone();
        }
        if let Some(size) = attrs.get("size") {
            if let Ok(s) = size.parse::<u64>() {
                self.size = s;
            }
        }
        if let Some(mtime) = attrs.get("mtime") {
            // PAX mtime can be a float, parse just the integer part
            if let Some(dot_pos) = mtime.find('.') {
                if let Ok(t) = mtime[..dot_pos].parse::<u64>() {
                    self.mtime = t;
                }
            } else if let Ok(t) = mtime.parse::<u64>() {
                self.mtime = t;
            }
        }
        if let Some(uid) = attrs.get("uid") {
            if let Ok(u) = uid.parse::<u32>() {
                self.uid = u;
            }
        }
        if let Some(gid) = attrs.get("gid") {
            if let Ok(g) = gid.parse::<u32>() {
                self.gid = g;
            }
        }
        if let Some(uname) = attrs.get("uname") {
            self.uname = uname.clone();
        }
        if let Some(gname) = attrs.get("gname") {
            self.gname = gname.clone();
        }
    }

    /// Parse PAX extended header data.
    /// Format: "length key=value\n" repeated
    pub fn parse_pax_data(data: &[u8]) -> HashMap<String, String> {
        let mut attrs = HashMap::new();
        let mut pos = 0;

        while pos < data.len() {
            // Find the space after length
            let space_pos = match data[pos..].iter().position(|&b| b == b' ') {
                Some(p) => pos + p,
                None => break,
            };

            // Parse length
            let len_str = String::from_utf8_lossy(&data[pos..space_pos]);
            let record_len: usize = match len_str.trim().parse() {
                Ok(l) => l,
                Err(_) => break,
            };

            if record_len == 0 || pos + record_len > data.len() {
                break;
            }

            // Get the record (excluding the newline at the end)
            let record_end = pos + record_len;
            // The record is from after the space to before the newline
            let mut value_end = record_end;
            if value_end > 0 && data.get(value_end - 1) == Some(&b'\n') {
                value_end -= 1;
            }
            let record = &data[space_pos + 1..value_end];

            // Find the = separator
            if let Some(eq_pos) = record.iter().position(|&b| b == b'=') {
                let key = String::from_utf8_lossy(&record[..eq_pos]).into_owned();
                let value = String::from_utf8_lossy(&record[eq_pos + 1..]).into_owned();
                attrs.insert(key, value);
            }

            pos = record_end;
        }

        attrs
    }

    /// Convert to Entry.
    pub fn to_entry(&self, offset: u64) -> Entry {
        let mut entry = Entry::file(&self.name, self.size);
        entry.entry_type = self.entry_type();
        entry.modified = Some(UNIX_EPOCH + Duration::from_secs(self.mtime));
        entry.attributes = FileAttributes {
            unix_mode: Some(self.mode),
            dos_attributes: None,
            uid: Some(self.uid),
            gid: Some(self.gid),
        };
        entry.offset = offset;

        if !self.linkname.is_empty() {
            entry.link_target = Some(self.linkname.clone().into());
        }

        entry
    }
}

/// TAR archive writer.
pub struct TarWriter<W: Write> {
    writer: W,
    finished: bool,
}

impl<W: Write> TarWriter<W> {
    /// Create a new TAR writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            finished: false,
        }
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_mode(name, data, 0o644)
    }

    /// Add a file with specific mode.
    pub fn add_file_with_mode(&mut self, name: &str, data: &[u8], mode: u32) -> Result<()> {
        // Check if we need PAX extended header for long filename
        let needs_pax = name.len() > 100;

        if needs_pax {
            self.write_pax_header(name, None)?;
            // Use truncated name for the regular header
            let short_name = &name[name.len().saturating_sub(100)..];
            let header = TarHeader::new_file(short_name, data.len() as u64, mode);
            self.write_header(&header)?;
        } else {
            let header = TarHeader::new_file(name, data.len() as u64, mode);
            self.write_header(&header)?;
        }
        self.write_data(data)?;
        Ok(())
    }

    /// Write a PAX extended header for long filenames/linknames.
    fn write_pax_header(&mut self, path: &str, linkpath: Option<&str>) -> Result<()> {
        // Build PAX data
        let mut pax_data = Vec::new();

        if !path.is_empty() {
            let record = Self::format_pax_record("path", path);
            pax_data.extend_from_slice(record.as_bytes());
        }
        if let Some(link) = linkpath {
            let record = Self::format_pax_record("linkpath", link);
            pax_data.extend_from_slice(record.as_bytes());
        }

        // Create PAX header
        let mut pax_header = TarHeader::new_file("PaxHeader", pax_data.len() as u64, 0o644);
        pax_header.typeflag = PAX_HEADER;

        // Write PAX header block
        self.write_header(&pax_header)?;
        self.write_data(&pax_data)?;

        Ok(())
    }

    /// Format a single PAX record: "len key=value\n"
    fn format_pax_record(key: &str, value: &str) -> String {
        // Format: "length key=value\n"
        // length includes: digits of length + space + key + "=" + value + "\n"
        let base_len = key.len() + value.len() + 3; // " " + "=" + "\n"

        // Need to figure out how many digits the length will be
        // Start with 1 digit and keep trying until we find the right size
        let mut total_len = base_len + 1;
        loop {
            let digits = total_len.to_string().len();
            let expected = base_len + digits;
            if expected == total_len {
                break;
            }
            total_len = expected;
        }

        format!("{} {}={}\n", total_len, key, value)
    }

    /// Add a directory to the archive.
    pub fn add_directory(&mut self, name: &str) -> Result<()> {
        self.add_directory_with_mode(name, 0o755)
    }

    /// Add a directory with specific mode.
    pub fn add_directory_with_mode(&mut self, name: &str, mode: u32) -> Result<()> {
        // Ensure directory name ends with /
        let dir_name = if name.ends_with('/') {
            name.to_string()
        } else {
            format!("{}/", name)
        };
        let header = TarHeader::new_directory(&dir_name, mode);
        self.write_header(&header)?;
        Ok(())
    }

    /// Add a symlink to the archive.
    pub fn add_symlink(&mut self, name: &str, target: &str) -> Result<()> {
        let header = TarHeader::new_symlink(name, target);
        self.write_header(&header)?;
        Ok(())
    }

    /// Write a header block.
    fn write_header(&mut self, header: &TarHeader) -> Result<()> {
        let block = header.to_block()?;
        self.writer.write_all(&block)?;
        Ok(())
    }

    /// Write data blocks.
    fn write_data(&mut self, data: &[u8]) -> Result<()> {
        self.writer.write_all(data)?;

        // Pad to block boundary
        let padding = (BLOCK_SIZE - (data.len() % BLOCK_SIZE)) % BLOCK_SIZE;
        if padding > 0 {
            self.writer.write_all(&vec![0u8; padding])?;
        }

        Ok(())
    }

    /// Finish the archive by writing two zero blocks.
    pub fn finish(&mut self) -> Result<()> {
        if !self.finished {
            self.writer.write_all(&[0u8; BLOCK_SIZE])?;
            self.writer.write_all(&[0u8; BLOCK_SIZE])?;
            self.writer.flush()?;
            self.finished = true;
        }
        Ok(())
    }

    /// Consume the writer and return the inner writer.
    /// Finishes the archive first.
    pub fn into_inner(self) -> Result<W> {
        // Use ManuallyDrop to prevent the Drop impl from running
        let mut this = std::mem::ManuallyDrop::new(self);
        if !this.finished {
            this.writer.write_all(&[0u8; BLOCK_SIZE])?;
            this.writer.write_all(&[0u8; BLOCK_SIZE])?;
            this.writer.flush()?;
        }
        // SAFETY: We're consuming self via ManuallyDrop, so we can take ownership
        Ok(unsafe { std::ptr::read(&this.writer) })
    }
}

impl<W: Write> Drop for TarWriter<W> {
    fn drop(&mut self) {
        // Attempt to finish on drop, ignore errors
        let _ = self.finish();
    }
}

impl TarHeader {
    /// Create a header for a regular file.
    pub fn new_file(name: &str, size: u64, mode: u32) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            name: name.to_string(),
            mode,
            uid: 1000,
            gid: 1000,
            size,
            mtime: now,
            typeflag: b'0',
            linkname: String::new(),
            ustar: true,
            uname: String::new(),
            gname: String::new(),
            prefix: String::new(),
        }
    }

    /// Create a header for a directory.
    pub fn new_directory(name: &str, mode: u32) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            name: name.to_string(),
            mode,
            uid: 1000,
            gid: 1000,
            size: 0,
            mtime: now,
            typeflag: b'5',
            linkname: String::new(),
            ustar: true,
            uname: String::new(),
            gname: String::new(),
            prefix: String::new(),
        }
    }

    /// Create a header for a symlink.
    pub fn new_symlink(name: &str, target: &str) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        Self {
            name: name.to_string(),
            mode: 0o777,
            uid: 1000,
            gid: 1000,
            size: 0,
            mtime: now,
            typeflag: b'2',
            linkname: target.to_string(),
            ustar: true,
            uname: String::new(),
            gname: String::new(),
            prefix: String::new(),
        }
    }

    /// Convert header to a 512-byte block.
    pub fn to_block(&self) -> Result<[u8; BLOCK_SIZE]> {
        let mut block = [0u8; BLOCK_SIZE];

        // Split name if too long
        let (prefix, name) = if self.name.len() > 100 {
            // Find a good split point (at a /)
            let split_pos = self.name[..155.min(self.name.len())]
                .rfind('/')
                .unwrap_or(0);
            if split_pos > 0 && self.name.len() - split_pos - 1 <= 100 {
                (&self.name[..split_pos], &self.name[split_pos + 1..])
            } else {
                return Err(OxiArcError::invalid_header("Filename too long for TAR"));
            }
        } else {
            ("", self.name.as_str())
        };

        // Name (100 bytes)
        Self::write_string(&mut block[0..100], name);

        // Mode (8 bytes octal)
        Self::write_octal(&mut block[100..108], self.mode as u64);

        // UID (8 bytes octal)
        Self::write_octal(&mut block[108..116], self.uid as u64);

        // GID (8 bytes octal)
        Self::write_octal(&mut block[116..124], self.gid as u64);

        // Size (12 bytes octal)
        Self::write_octal(&mut block[124..136], self.size);

        // Mtime (12 bytes octal)
        Self::write_octal(&mut block[136..148], self.mtime);

        // Leave checksum as spaces for now
        block[148..156].copy_from_slice(b"        ");

        // Typeflag
        block[156] = self.typeflag;

        // Linkname (100 bytes)
        Self::write_string(&mut block[157..257], &self.linkname);

        // UStar magic
        block[257..263].copy_from_slice(b"ustar\0");
        // UStar version
        block[263..265].copy_from_slice(b"00");

        // Uname (32 bytes)
        Self::write_string(&mut block[265..297], &self.uname);

        // Gname (32 bytes)
        Self::write_string(&mut block[297..329], &self.gname);

        // Dev major/minor (skip, leave as zeros)

        // Prefix (155 bytes)
        Self::write_string(&mut block[345..500], prefix);

        // Calculate and write checksum
        let checksum: u32 = block.iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        block[148..156].copy_from_slice(&checksum_str.as_bytes()[..8]);

        Ok(block)
    }

    /// Write a null-terminated string to a field.
    fn write_string(field: &mut [u8], s: &str) {
        let bytes = s.as_bytes();
        let len = bytes.len().min(field.len() - 1);
        field[..len].copy_from_slice(&bytes[..len]);
        // Rest is already zeroed
    }

    /// Write an octal number to a field.
    fn write_octal(field: &mut [u8], value: u64) {
        let s = format!("{:0width$o}", value, width = field.len() - 1);
        let bytes = s.as_bytes();
        if bytes.len() < field.len() {
            field[..bytes.len()].copy_from_slice(bytes);
        }
    }
}

/// TAR archive reader with extraction support.
pub struct TarReader<R: Read + Seek> {
    reader: R,
    entries: Vec<Entry>,
}

impl<R: Read + Seek> TarReader<R> {
    /// Create a new TAR reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let entries = Self::read_entries(&mut reader)?;
        Ok(Self { reader, entries })
    }

    /// Read all entries.
    fn read_entries(reader: &mut R) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        let mut offset = 0u64;
        let mut pax_attrs: HashMap<String, String> = HashMap::new();
        let mut global_pax_attrs: HashMap<String, String> = HashMap::new();
        let mut gnu_longname: Option<String> = None;
        let mut gnu_longlink: Option<String> = None;

        loop {
            let mut block = [0u8; BLOCK_SIZE];
            if reader.read_exact(&mut block).is_err() {
                break;
            }

            match TarHeader::from_block(&block)? {
                Some(mut header) => {
                    // Handle special header types
                    if header.is_pax_header() || header.is_pax_global_header() {
                        // Read PAX extended header data
                        let data = Self::read_header_data(reader, header.size)?;
                        let attrs = TarHeader::parse_pax_data(&data);

                        if header.is_pax_global_header() {
                            // Global headers apply to all subsequent entries
                            global_pax_attrs.extend(attrs);
                        } else {
                            // Regular PAX headers apply only to next entry
                            pax_attrs = attrs;
                        }

                        // Update offset and continue to next header
                        let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                        offset += BLOCK_SIZE as u64 + data_blocks * BLOCK_SIZE as u64;
                        continue;
                    }

                    if header.is_gnu_longname() {
                        // Read GNU LongName data
                        let data = Self::read_header_data(reader, header.size)?;
                        gnu_longname = Some(
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string(),
                        );

                        let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                        offset += BLOCK_SIZE as u64 + data_blocks * BLOCK_SIZE as u64;
                        continue;
                    }

                    if header.is_gnu_longlink() {
                        // Read GNU LongLink data
                        let data = Self::read_header_data(reader, header.size)?;
                        gnu_longlink = Some(
                            String::from_utf8_lossy(&data)
                                .trim_end_matches('\0')
                                .to_string(),
                        );

                        let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                        offset += BLOCK_SIZE as u64 + data_blocks * BLOCK_SIZE as u64;
                        continue;
                    }

                    // Apply accumulated attributes
                    // First global, then local PAX (local overrides global)
                    if !global_pax_attrs.is_empty() {
                        header.apply_pax_attrs(&global_pax_attrs);
                    }
                    if !pax_attrs.is_empty() {
                        header.apply_pax_attrs(&pax_attrs);
                        pax_attrs.clear(); // Only applies to this entry
                    }

                    // Apply GNU long name/link
                    if let Some(name) = gnu_longname.take() {
                        header.name = name;
                    }
                    if let Some(link) = gnu_longlink.take() {
                        header.linkname = link;
                    }

                    let data_offset = offset + BLOCK_SIZE as u64;
                    entries.push(header.to_entry(data_offset));

                    // Skip file data (rounded up to block boundary)
                    let data_blocks = header.size.div_ceil(BLOCK_SIZE as u64);
                    let skip_bytes = data_blocks * BLOCK_SIZE as u64;

                    // Use seek for efficiency
                    reader.seek(SeekFrom::Current(skip_bytes as i64))?;

                    offset += BLOCK_SIZE as u64 + skip_bytes;
                }
                None => break, // End of archive
            }
        }

        Ok(entries)
    }

    /// Read header data (for PAX extended headers and GNU long name/link).
    fn read_header_data(reader: &mut R, size: u64) -> Result<Vec<u8>> {
        let mut data = vec![0u8; size as usize];
        reader.read_exact(&mut data)?;

        // Skip padding to block boundary
        let padding = (BLOCK_SIZE - (size as usize % BLOCK_SIZE)) % BLOCK_SIZE;
        if padding > 0 {
            let mut skip = [0u8; BLOCK_SIZE];
            reader.read_exact(&mut skip[..padding])?;
        }

        Ok(data)
    }

    /// Get entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Extract an entry to a writer.
    pub fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64> {
        // Seek to data offset
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Read and write data in chunks
        let mut remaining = entry.size;
        let mut buffer = [0u8; 8192];
        let mut written = 0u64;

        while remaining > 0 {
            let to_read = remaining.min(buffer.len() as u64) as usize;
            self.reader.read_exact(&mut buffer[..to_read])?;
            writer.write_all(&buffer[..to_read])?;
            remaining -= to_read as u64;
            written += to_read as u64;
        }

        Ok(written)
    }

    /// Extract an entry to a Vec.
    pub fn extract_to_vec(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(entry.size as usize);
        self.extract(entry, &mut data)?;
        Ok(data)
    }

    /// Extract an entry by name.
    pub fn extract_by_name(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let entry = self.entries.iter().find(|e| e.name == name).cloned();
        match entry {
            Some(e) => Ok(Some(self.extract_to_vec(&e)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_octal() {
        assert_eq!(TarHeader::parse_octal(b"0000644\0").unwrap(), 0o644);
        assert_eq!(TarHeader::parse_octal(b"0001750\0").unwrap(), 0o1750);
    }

    #[test]
    fn test_parse_string() {
        assert_eq!(TarHeader::parse_string(b"hello\0world"), "hello");
        assert_eq!(TarHeader::parse_string(b"test"), "test");
    }

    /// Create a minimal TAR archive for testing.
    fn create_test_tar() -> Vec<u8> {
        let mut tar = vec![0u8; BLOCK_SIZE * 4];

        // Header block for "test.txt"
        let name = b"test.txt";
        tar[..name.len()].copy_from_slice(name);

        // Mode: 0644
        tar[100..107].copy_from_slice(b"0000644");

        // UID: 1000
        tar[108..115].copy_from_slice(b"0001750");

        // GID: 1000
        tar[116..123].copy_from_slice(b"0001750");

        // Size: 13 bytes ("Hello, TAR!\n")
        tar[124..135].copy_from_slice(b"00000000015");

        // Mtime: some timestamp
        tar[136..147].copy_from_slice(b"14723456700");

        // Typeflag: regular file
        tar[156] = b'0';

        // Calculate checksum
        tar[148..156].copy_from_slice(b"        "); // Initialize with spaces
        let checksum: u32 = tar[..BLOCK_SIZE].iter().map(|&b| b as u32).sum();
        let checksum_str = format!("{:06o}\0 ", checksum);
        tar[148..156].copy_from_slice(checksum_str.as_bytes());

        // Data block
        let data = b"Hello, TAR!\n\0";
        tar[BLOCK_SIZE..BLOCK_SIZE + data.len()].copy_from_slice(data);

        // Two zero blocks mark end of archive
        // (already zeroed)

        tar
    }

    #[test]
    fn test_tar_reader() {
        let tar_data = create_test_tar();
        let cursor = Cursor::new(tar_data);
        let reader = TarReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 1);
        let entry = &reader.entries()[0];
        assert_eq!(entry.name, "test.txt");
        assert_eq!(entry.size, 13);
    }

    #[test]
    fn test_tar_extraction() {
        let tar_data = create_test_tar();
        let cursor = Cursor::new(tar_data);
        let mut reader = TarReader::new(cursor).unwrap();

        let entry = reader.entries()[0].clone();
        let data = reader.extract_to_vec(&entry).unwrap();

        assert_eq!(data.len(), 13);
        assert_eq!(&data[..12], b"Hello, TAR!\n");
    }

    #[test]
    fn test_tar_extract_by_name() {
        let tar_data = create_test_tar();
        let cursor = Cursor::new(tar_data);
        let mut reader = TarReader::new(cursor).unwrap();

        let data = reader.extract_by_name("test.txt").unwrap();
        assert!(data.is_some());
        assert_eq!(&data.unwrap()[..12], b"Hello, TAR!\n");

        let missing = reader.extract_by_name("nonexistent.txt").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_tar_writer_single_file() {
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_file("hello.txt", b"Hello, World!").unwrap();
            writer.finish().unwrap();
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, 13);

        let data = reader.extract_to_vec(&entry).unwrap();
        assert_eq!(&data, b"Hello, World!");
    }

    #[test]
    fn test_tar_writer_multiple_files() {
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_file("file1.txt", b"Content 1").unwrap();
            writer
                .add_file("file2.txt", b"Content 2 is longer")
                .unwrap();
            writer.add_file("empty.txt", b"").unwrap();
            writer.finish().unwrap();
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 3);
        assert_eq!(reader.entries()[0].name, "file1.txt");
        assert_eq!(reader.entries()[1].name, "file2.txt");
        assert_eq!(reader.entries()[2].name, "empty.txt");

        let data1 = reader.extract_by_name("file1.txt").unwrap().unwrap();
        let data2 = reader.extract_by_name("file2.txt").unwrap().unwrap();
        let data3 = reader.extract_by_name("empty.txt").unwrap().unwrap();

        assert_eq!(&data1, b"Content 1");
        assert_eq!(&data2, b"Content 2 is longer");
        assert_eq!(&data3, b"");
    }

    #[test]
    fn test_tar_writer_directory() {
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_directory("mydir").unwrap();
            writer
                .add_file("mydir/file.txt", b"Inside directory")
                .unwrap();
            writer.finish().unwrap();
        }

        // Read back
        let cursor = Cursor::new(output);
        let reader = TarReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.entries()[0].name, "mydir/");
        assert!(reader.entries()[0].is_dir());
        assert_eq!(reader.entries()[1].name, "mydir/file.txt");
        assert!(reader.entries()[1].is_file());
    }

    #[test]
    fn test_tar_roundtrip() {
        // Create archive with various content
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_directory("docs").unwrap();
            writer
                .add_file("docs/readme.txt", b"Read me first!")
                .unwrap();
            writer
                .add_file_with_mode("script.sh", b"#!/bin/sh\necho hello", 0o755)
                .unwrap();
            writer.finish().unwrap();
        }

        // Read and verify
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).unwrap();

        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 3);

        // Verify directory
        assert!(entries[0].is_dir());
        assert_eq!(entries[0].name, "docs/");

        // Verify files
        let readme = reader.extract_by_name("docs/readme.txt").unwrap().unwrap();
        assert_eq!(&readme, b"Read me first!");

        let script = reader.extract_by_name("script.sh").unwrap().unwrap();
        assert_eq!(&script, b"#!/bin/sh\necho hello");

        // Check mode
        assert_eq!(entries[2].attributes.unix_mode, Some(0o755));
    }

    #[test]
    fn test_pax_record_format() {
        // Test the PAX record formatting
        // "path=test.txt\n" = 14 chars, plus "17 " = 17 total
        let record = TarWriter::<Vec<u8>>::format_pax_record("path", "test.txt");
        assert_eq!(record, "17 path=test.txt\n");
        assert_eq!(record.len(), 17);

        // Longer path
        let long_path = "a".repeat(200);
        let record = TarWriter::<Vec<u8>>::format_pax_record("path", &long_path);
        // "path=" + 200 chars + "\n" = 206, plus "209 " = 209... but 209 is 3 digits
        // Actually: 3 (digits) + 1 (space) + 4 (path) + 1 (=) + 200 (value) + 1 (\n) = 210
        assert!(record.starts_with("210 path="));
        assert_eq!(record.len(), 210);
    }

    #[test]
    fn test_parse_pax_data() {
        // Test single record first
        let pax_single = b"17 path=test.txt\n";
        let attrs_single = TarHeader::parse_pax_data(pax_single);
        assert_eq!(
            attrs_single.get("path").map(|s| s.as_str()),
            Some("test.txt")
        );

        // Test multiple records
        // "17 path=test.txt\n" = 17 bytes
        // "20 size=1234567890\n" = 20 bytes (2 + 1 + 4 + 1 + 10 + 1 = 19... wait that's 19)
        // Actually: "2" + "0" + " " + "size" + "=" + "1234567890" + "\n" = 2+1+4+1+10+1 = 19
        // So need length 19: "19 size=1234567890\n" = 19 bytes
        let pax_data = b"17 path=test.txt\n19 size=1234567890\n";
        let attrs = TarHeader::parse_pax_data(pax_data);

        assert_eq!(attrs.get("path").map(|s| s.as_str()), Some("test.txt"));
        assert_eq!(attrs.get("size").map(|s| s.as_str()), Some("1234567890"));
    }

    #[test]
    fn test_tar_pax_long_filename() {
        // Create a file with a very long filename (>100 chars)
        let long_name = "very_long_directory_name_that_exceeds_one_hundred_characters/\
            another_long_subdirectory_name/\
            and_finally_the_actual_filename.txt";

        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer
                .add_file(long_name, b"Content of file with long name")
                .unwrap();
            writer.finish().unwrap();
        }

        // Read back and verify
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 1);
        assert_eq!(reader.entries()[0].name, long_name);

        // Extract and verify content
        let data = reader.extract_to_vec(&reader.entries()[0].clone()).unwrap();
        assert_eq!(&data, b"Content of file with long name");
    }

    #[test]
    fn test_tar_pax_roundtrip() {
        // Mix of normal and long filenames
        let short_name = "short.txt";
        let long_name = "this/is/a/very/long/path/name/that/definitely/exceeds/\
            the/one/hundred/character/limit/for/standard/tar/headers/file.txt";

        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output);
            writer.add_file(short_name, b"Short content").unwrap();
            writer.add_file(long_name, b"Long path content").unwrap();
            writer.finish().unwrap();
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = TarReader::new(cursor).unwrap();

        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.entries()[0].name, short_name);
        assert_eq!(reader.entries()[1].name, long_name);

        // Verify content
        let data1 = reader.extract_by_name(short_name).unwrap().unwrap();
        let data2 = reader.extract_by_name(long_name).unwrap().unwrap();
        assert_eq!(&data1, b"Short content");
        assert_eq!(&data2, b"Long path content");
    }
}

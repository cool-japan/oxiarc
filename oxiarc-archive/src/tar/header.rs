//! TAR header types and parsing/serialization logic.

use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Entry, EntryType, FileAttributes};
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

use super::{BLOCK_SIZE, GNU_LONGLINK, GNU_LONGNAME, PAX_GLOBAL_HEADER, PAX_HEADER};

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
    /// Compute the POSIX tar header checksum.
    ///
    /// The checksum is defined as the sum of the unsigned byte values
    /// in the 512-byte header block, with the 8-byte checksum field
    /// (offsets 148..156) treated as eight ASCII spaces. Returns the
    /// resulting 32-bit sum.
    pub fn compute_checksum(block: &[u8; BLOCK_SIZE]) -> u32 {
        let mut sum: u32 = 0;
        for (i, &b) in block.iter().enumerate() {
            if (148..156).contains(&i) {
                sum = sum.wrapping_add(b' ' as u32);
            } else {
                sum = sum.wrapping_add(b as u32);
            }
        }
        sum
    }

    /// Return `true` when the stored header checksum in `block` matches
    /// the value computed via [`TarHeader::compute_checksum`].
    ///
    /// Tolerates both the standard `"%06o\0 "` and the GNU-tar
    /// `"%06o  "` (trailing space) formats by comparing numeric values
    /// rather than byte strings.
    pub fn verify_checksum(block: &[u8; BLOCK_SIZE]) -> bool {
        let stored = match Self::parse_octal(&block[148..156]) {
            Ok(v) => v,
            Err(_) => return false,
        };
        let computed = Self::compute_checksum(block);
        stored == computed
    }

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
    pub(crate) fn parse_string(data: &[u8]) -> String {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        String::from_utf8_lossy(&data[..end]).into_owned()
    }

    /// Parse an octal number.
    pub(crate) fn parse_octal(data: &[u8]) -> Result<u32> {
        let s = Self::parse_string(data);
        let s = s.trim();
        if s.is_empty() {
            return Ok(0);
        }
        u32::from_str_radix(s, 8)
            .map_err(|_| OxiArcError::invalid_header(format!("Invalid octal: {}", s)))
    }

    /// Parse an octal number as u64.
    pub(crate) fn parse_octal_u64(data: &[u8]) -> Result<u64> {
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

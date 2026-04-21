//! TAR archive format support.
//!
//! This module provides reading and extraction of TAR archives with support for:
//! - UStar format (POSIX.1-1988)
//! - PAX extended headers (POSIX.1-2001) for long filenames and additional metadata

use crate::lenient::{LenientWarning, LenientWarningKind};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::{Entry, EntryType, FileAttributes};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::time::{Duration, UNIX_EPOCH};

pub(crate) mod sparse;
use sparse::{SparseMap, SparseMapTable};

/// Maximum number of consecutive corrupt 512-byte blocks the lenient
/// TAR reader will skip while searching for the next valid header.
///
/// Tuning guidance: 16 * 512 = 8 KiB of scan. Archives with larger
/// corrupt regions require callers to repair the archive (e.g., via an
/// external tool) before extraction. Raising this caps scan time at
/// O(`CAP * BLOCK_SIZE`) in the worst case.
const LENIENT_SCAN_MAX_BLOCKS: usize = 16;

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
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
    /// Entry index counter for progress reporting.
    entry_index: u64,
}

impl<W: Write> TarWriter<W> {
    /// Create a new TAR writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            finished: false,
            progress: None,
            entry_index: 0,
        }
    }

    /// Attach a progress callback handle.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_mode(name, data, 0o644)
    }

    /// Add a file with specific mode.
    pub fn add_file_with_mode(&mut self, name: &str, data: &[u8], mode: u32) -> Result<()> {
        // Emit progress: entry start
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, idx);
        }
        self.entry_index += 1;

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

        // Emit progress: bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(data.len() as u64, None);
        }

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
            if let Some(ref handle) = self.progress {
                handle.on_finish();
            }
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
    /// Optional progress handle.
    progress: Option<ProgressHandle>,
    /// Per-entry sparse maps, keyed by `entry.offset` (= data offset).
    ///
    /// Populated during `read_entries` for GNU old-format `'S'` entries and
    /// for PAX-marked sparse entries. Absent keys indicate a normal
    /// (non-sparse) entry. Wrapped so that `extract()` can short-circuit
    /// to the sparse materialization path.
    sparse_maps: SparseMapTable,
    /// When `true`, TAR header-checksum mismatches during scanning are
    /// recorded in [`TarReader::warnings`] and the reader scans forward
    /// to locate the next valid header. When `false` (default), a
    /// mismatch aborts [`TarReader::new`] with
    /// [`OxiArcError::InvalidHeader`].
    lenient: bool,
    /// Accumulated non-fatal warnings emitted while operating in
    /// lenient mode.
    warnings: Vec<LenientWarning>,
}

impl<R: Read + Seek> TarReader<R> {
    /// Create a new TAR reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut warnings = Vec::new();
        let (entries, sparse_maps) = Self::read_entries(&mut reader, None, false, &mut warnings)?;
        Ok(Self {
            reader,
            entries,
            progress: None,
            sparse_maps,
            lenient: false,
            warnings,
        })
    }

    /// Create a new lenient TAR reader.
    ///
    /// Equivalent to `TarReader::new(reader).lenient(true)` except that
    /// the initial entry scan is itself permitted to skip over corrupt
    /// blocks. Use this when the archive may contain mid-stream damage
    /// that the default (strict) scanner cannot tolerate.
    pub fn new_lenient(mut reader: R) -> Result<Self> {
        let mut warnings = Vec::new();
        let (entries, sparse_maps) = Self::read_entries(&mut reader, None, true, &mut warnings)?;
        Ok(Self {
            reader,
            entries,
            progress: None,
            sparse_maps,
            lenient: true,
            warnings,
        })
    }

    /// Create a new TAR reader with progress reporting during entry scanning.
    pub fn new_with_progress(mut reader: R, handle: ProgressHandle) -> Result<Self> {
        let mut warnings = Vec::new();
        let (entries, sparse_maps) =
            Self::read_entries(&mut reader, Some(&handle), false, &mut warnings)?;
        Ok(Self {
            reader,
            entries,
            progress: Some(handle),
            sparse_maps,
            lenient: false,
            warnings,
        })
    }

    /// Attach a progress callback handle.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Toggle lenient-mode **extraction**. Note: this does NOT re-run
    /// the initial entry scan. If you need lenient scanning to skip
    /// corrupt header blocks, use [`TarReader::new_lenient`] instead.
    pub fn lenient(mut self, enabled: bool) -> Self {
        self.lenient = enabled;
        self
    }

    /// Return the accumulated non-fatal warnings from lenient-mode
    /// operations.
    pub fn warnings(&self) -> &[LenientWarning] {
        &self.warnings
    }

    /// Read all entries, optionally reporting progress.
    ///
    /// Returns both the list of parsed entries and a side-channel table of
    /// sparse maps keyed by entry data offset. The map is consulted by
    /// `extract()` to switch into the sparse materialization path; entries
    /// absent from the map are extracted via plain sequential copy.
    ///
    /// When `lenient` is `true`, a header block whose POSIX checksum
    /// field does not match the computed checksum (and which is not an
    /// all-zero end-of-archive marker) is treated as a recoverable
    /// scanning error: the reader records a [`LenientWarning`] in
    /// `warnings`, advances 512 bytes, and probes the next candidate
    /// block. Recovery gives up after [`LENIENT_SCAN_MAX_BLOCKS`]
    /// consecutive failed probes and returns
    /// [`OxiArcError::InvalidHeader`]. Non-lenient mode preserves the
    /// legacy behavior of propagating the first parse error.
    fn read_entries(
        reader: &mut R,
        progress: Option<&ProgressHandle>,
        lenient: bool,
        warnings: &mut Vec<LenientWarning>,
    ) -> Result<(Vec<Entry>, SparseMapTable)> {
        let mut entries = Vec::new();
        let mut sparse_maps: SparseMapTable = HashMap::new();
        let mut offset = 0u64;
        let mut pax_attrs: HashMap<String, String> = HashMap::new();
        let mut global_pax_attrs: HashMap<String, String> = HashMap::new();
        let mut gnu_longname: Option<String> = None;
        let mut gnu_longlink: Option<String> = None;
        let mut index: u64 = 0;

        loop {
            let mut block = [0u8; BLOCK_SIZE];
            if reader.read_exact(&mut block).is_err() {
                break;
            }

            // Verify header checksum *before* attempting to parse.
            //
            // A fully-zero block is the legitimate end-of-archive
            // marker and is handled below by `TarHeader::from_block`
            // returning `None`. Every other block MUST pass the
            // checksum verification; if it does not, strict mode
            // returns an error and lenient mode scans forward.
            let is_zero_block = block.iter().all(|&b| b == 0);
            if !is_zero_block && !TarHeader::verify_checksum(&block) {
                if !lenient {
                    return Err(OxiArcError::invalid_header(format!(
                        "TAR header checksum mismatch at offset {offset}"
                    )));
                }

                // Lenient: scan forward up to LENIENT_SCAN_MAX_BLOCKS
                // blocks for the next block whose checksum matches and
                // whose `size` field is a parseable octal number
                // (otherwise we could easily snap onto random data).
                warnings.push(LenientWarning {
                    format: "TAR",
                    entry_name: None,
                    kind: LenientWarningKind::HeaderChecksumMismatch,
                    message: format!(
                        "TAR header checksum mismatch at offset {offset}; scanning forward"
                    ),
                });

                let scan_start = offset + BLOCK_SIZE as u64;
                let mut scanned_blocks = 0usize;
                let mut recovered = false;

                while scanned_blocks < LENIENT_SCAN_MAX_BLOCKS {
                    let mut probe = [0u8; BLOCK_SIZE];
                    if reader.read_exact(&mut probe).is_err() {
                        break;
                    }
                    scanned_blocks += 1;

                    let probe_offset = scan_start + (scanned_blocks as u64 - 1) * BLOCK_SIZE as u64;

                    let probe_is_zero = probe.iter().all(|&b| b == 0);
                    if probe_is_zero
                        || (TarHeader::verify_checksum(&probe)
                            && TarHeader::parse_octal_u64(&probe[124..136]).is_ok())
                    {
                        // Found either a valid header OR a zero block
                        // (genuine end-of-archive). In both cases,
                        // rewind one block so the outer loop re-reads
                        // the block and dispatches normally:
                        //   - valid header → parsed by `from_block`
                        //   - zero block   → `from_block` returns None
                        //     and the outer loop breaks
                        reader.seek(SeekFrom::Current(-(BLOCK_SIZE as i64)))?;
                        warnings.push(LenientWarning {
                            format: "TAR",
                            entry_name: None,
                            kind: LenientWarningKind::ScannedForward {
                                bytes: (scanned_blocks as u64) * BLOCK_SIZE as u64,
                            },
                            message: format!(
                                "TAR scan recovered a valid header/EOA after skipping {} bytes",
                                scanned_blocks * BLOCK_SIZE
                            ),
                        });
                        offset = probe_offset;
                        recovered = true;
                        break;
                    }
                }

                if !recovered {
                    return Err(OxiArcError::invalid_header(format!(
                        "TAR lenient scan gave up after {LENIENT_SCAN_MAX_BLOCKS} bad blocks starting at offset {offset}"
                    )));
                }

                // Continue outer loop — the next iteration will
                // `read_exact` the recovered header block.
                continue;
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

                    // ---- GNU old-format sparse entry (typeflag 'S') ----
                    //
                    // These have the sparse map encoded in the primary
                    // header's unused bytes, optionally extended by
                    // continuation 512-byte blocks. The payload that
                    // follows is the concatenation of non-hole runs,
                    // padded to `BLOCK_SIZE`.
                    if header.typeflag == b'S' {
                        // Parse the sparse map starting from the primary
                        // header; `parse_gnu_old_format` pulls extra
                        // 512-byte continuation blocks from `reader`
                        // whenever the `isextended` flag is set.
                        //
                        // We track how many continuation blocks were
                        // consumed by measuring the seek delta; this lets
                        // us update `offset` correctly without relying on
                        // an internal counter from the parser.
                        let start_pos = reader.stream_position()?;
                        let map = SparseMap::parse_gnu_old_format(&block, reader)?;
                        map.validate()?;
                        let cont_bytes = reader.stream_position()? - start_pos;

                        // Apply any accumulated attributes.
                        if !global_pax_attrs.is_empty() {
                            header.apply_pax_attrs(&global_pax_attrs);
                        }
                        if !pax_attrs.is_empty() {
                            header.apply_pax_attrs(&pax_attrs);
                            pax_attrs.clear();
                        }
                        if let Some(name) = gnu_longname.take() {
                            header.name = name;
                        }
                        if let Some(link) = gnu_longlink.take() {
                            header.linkname = link;
                        }

                        // Data begins immediately after the primary and
                        // all continuation blocks.
                        let data_offset = offset + BLOCK_SIZE as u64 + cont_bytes;

                        let mut entry = header.to_entry(data_offset);
                        // Override the size reported to callers: `header.size`
                        // holds the *stored* byte count for GNU old-format
                        // sparse, but users expect logical (real) size.
                        entry.size = map.realsize;

                        if let Some(handle) = progress {
                            handle.on_entry(&entry.name, index);
                            handle.on_progress(entry.size, Some(entry.size));
                        }
                        index += 1;

                        let skip_bytes = map.padded_stored_size();
                        sparse_maps.insert(data_offset, map);

                        entries.push(entry);

                        reader.seek(SeekFrom::Current(skip_bytes as i64))?;

                        offset += BLOCK_SIZE as u64 + cont_bytes + skip_bytes;
                        continue;
                    }

                    // ---- Standard path (non-sparse or PAX-encoded sparse) ----

                    // Apply accumulated attributes
                    // First global, then local PAX (local overrides global)
                    if !global_pax_attrs.is_empty() {
                        header.apply_pax_attrs(&global_pax_attrs);
                    }

                    // Detect PAX-encoded sparse (`GNU.sparse.map` present in
                    // local PAX attrs) before `pax_attrs.clear()` drains
                    // them.
                    let is_pax_sparse = pax_attrs.contains_key("GNU.sparse.map");

                    if !pax_attrs.is_empty() {
                        header.apply_pax_attrs(&pax_attrs);
                    }

                    // Apply GNU long name/link
                    if let Some(name) = gnu_longname.take() {
                        header.name = name;
                    }
                    if let Some(link) = gnu_longlink.take() {
                        header.linkname = link;
                    }

                    let data_offset = offset + BLOCK_SIZE as u64;
                    let mut entry = header.to_entry(data_offset);

                    // Read count of bytes the payload occupies on the
                    // medium. For non-sparse entries this is `header.size`;
                    // for PAX-sparse entries we rederive it from the map.
                    let stored_bytes = if is_pax_sparse {
                        let map = SparseMap::from_pax_attrs(&pax_attrs)?;
                        map.validate()?;
                        // PAX sparse: logical size is realsize, stored
                        // bytes are sum of runs padded to BLOCK_SIZE.
                        entry.size = map.realsize;

                        // PAX 0.1 shadows the data-entry name: real archives
                        // emit dummy names like `./GNUSparseFile.XXXX/<real>`
                        // on the data header and supply the canonical name
                        // in `GNU.sparse.name`. Prefer that when present.
                        if let Some(real_name) = pax_attrs.get("GNU.sparse.name") {
                            entry.name = real_name.clone();
                        }

                        let padded = map.padded_stored_size();
                        sparse_maps.insert(data_offset, map);
                        padded
                    } else {
                        // Normal entry: skip header.size rounded up.
                        header.size.div_ceil(BLOCK_SIZE as u64) * BLOCK_SIZE as u64
                    };

                    // Done consuming PAX attrs for this entry.
                    pax_attrs.clear();

                    if let Some(handle) = progress {
                        handle.on_entry(&entry.name, index);
                        handle.on_progress(entry.size, Some(entry.size));
                    }
                    index += 1;

                    entries.push(entry);

                    // Use seek for efficiency
                    reader.seek(SeekFrom::Current(stored_bytes as i64))?;

                    offset += BLOCK_SIZE as u64 + stored_bytes;
                }
                None => break, // End of archive
            }
        }

        if let Some(handle) = progress {
            handle.on_finish();
        }

        Ok((entries, sparse_maps))
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
    ///
    /// For sparse entries (GNU old-format `'S'` or PAX `GNU.sparse.*`), the
    /// logical content is materialized: non-hole runs are read from the
    /// data stream, and hole regions are filled with zero bytes. The full
    /// `entry.size` bytes (the logical size) are written to `writer`.
    pub fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64> {
        // Emit extraction progress
        if let Some(ref handle) = self.progress {
            handle.on_entry(&entry.name, 0);
        }

        // Seek to data offset
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Sparse entries go through the materialization helper so that
        // callers see a contiguous logical file rather than the packed
        // on-disk form.
        if let Some(map) = self.sparse_maps.get(&entry.offset).cloned() {
            let buf = sparse::extract_sparse(&mut self.reader, &map)?;
            writer.write_all(&buf)?;
            let written = buf.len() as u64;
            if let Some(ref handle) = self.progress {
                handle.on_progress(written, Some(entry.size));
            }
            return Ok(written);
        }

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

        if let Some(ref handle) = self.progress {
            handle.on_progress(written, Some(entry.size));
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

pub mod stream;
pub use stream::{TarStreamEntry, TarStreamReader};

/// Open a TAR archive using memory-mapped I/O for efficient large-file reading.
///
/// This is a convenience wrapper around [`TarReader::new`] that opens the file
/// at `path` with [`oxiarc_core::mmap::MmapReader`], avoiding a full read into
/// memory while still offering random-access semantics.
///
/// # Errors
/// Returns an error if the file cannot be opened, cannot be memory-mapped, or
/// does not contain a valid TAR archive.
///
/// # Example
///
/// ```no_run
/// use oxiarc_archive::tar::open_tar_mmap;
///
/// let reader = open_tar_mmap("large_archive.tar").unwrap();
/// for entry in reader.entries() {
///     println!("{}", entry.name);
/// }
/// ```
#[cfg(feature = "mmap")]
pub fn open_tar_mmap<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<TarReader<oxiarc_core::mmap::MmapReader>> {
    let reader = oxiarc_core::mmap::MmapReader::open(path)?;
    TarReader::new(reader)
}

#[cfg(test)]
#[cfg(feature = "mmap")]
mod mmap_tests {
    use super::*;
    use std::io::Write;

    /// Write a test TAR to a temp file and return the path.
    fn create_test_tar_file(name: &str) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("oxiarc_mmap_tar_test_{}.tar", name));

        let mut tar_bytes = Vec::new();
        {
            let mut writer = TarWriter::new(&mut tar_bytes);
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
        file.write_all(&tar_bytes).expect("write failed");
        file.sync_all().expect("sync failed");
        path
    }

    #[test]
    fn test_mmap_tar_read() {
        let path = create_test_tar_file("read");

        let mut reader = open_tar_mmap(&path).expect("open_tar_mmap failed");
        let entries = reader.entries().to_vec();

        assert_eq!(entries.len(), 2);
        assert!(entries.iter().any(|e| e.name == "hello.txt"));
        assert!(entries.iter().any(|e| e.name == "repeat.txt"));

        let hello = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .expect("hello.txt entry");
        let data = reader.extract_to_vec(hello).expect("extract hello.txt");
        assert_eq!(data, b"Hello, mmap!");

        let repeat = entries
            .iter()
            .find(|e| e.name == "repeat.txt")
            .expect("repeat.txt entry");
        let data = reader.extract_to_vec(repeat).expect("extract repeat.txt");
        let expected: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(64).to_vec();
        assert_eq!(data, expected);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mmap_tar_multiple_reads() {
        let path = create_test_tar_file("multi_read");

        let mut reader = open_tar_mmap(&path).expect("open_tar_mmap failed");
        let entries = reader.entries().to_vec();
        let hello = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .expect("hello.txt entry");

        let data1 = reader.extract_to_vec(hello).expect("first extract");
        let data2 = reader.extract_to_vec(hello).expect("second extract");
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
    fn test_tar_progress() {
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

        // Write a test archive
        let mut output = Vec::new();
        {
            let mut writer = TarWriter::new(&mut output).with_progress(handle);
            writer.add_file("first.txt", b"hello").unwrap();
            writer.add_file("second.txt", b"world").unwrap();
            writer.finish().unwrap();
        }

        {
            let entries = sink.entries.lock().unwrap();
            assert_eq!(entries.len(), 2, "expected on_entry called twice");
            assert_eq!(entries[0], "first.txt");
            assert_eq!(entries[1], "second.txt");
        }
        assert!(*sink.finish_called.lock().unwrap(), "on_finish not called");
    }

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

    // ---- Sparse file tests ----

    /// Append `data` plus zero-padding to reach the next 512-byte boundary.
    fn pad_to_block(dest: &mut Vec<u8>, data: &[u8]) {
        dest.extend_from_slice(data);
        let rem = dest.len() % BLOCK_SIZE;
        if rem != 0 {
            dest.extend(std::iter::repeat_n(0u8, BLOCK_SIZE - rem));
        }
    }

    #[test]
    fn test_tar_sparse_gnu_old_format() {
        // Sparse file: realsize=16_384, two runs
        //   (0, 100)  -> 100 bytes of 'A'
        //   (10_000, 500) -> 500 bytes of 'B'
        let realsize: u64 = 16_384;
        let runs: Vec<(u64, u64)> = vec![(0, 100), (10_000, 500)];

        let primary = sparse::build_gnu_sparse_primary("sparse.bin", realsize, &runs, false);

        let mut archive = Vec::new();
        archive.extend_from_slice(&primary);

        // Data: 100 * 'A' || 500 * 'B', padded to 1024 bytes (2 blocks).
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(b'A', 100));
        data.extend(std::iter::repeat_n(b'B', 500));
        pad_to_block(&mut archive, &data);

        // Two zero blocks mark end of archive.
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let cursor = Cursor::new(archive);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1, "expected one sparse entry");
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "sparse.bin");
        assert_eq!(entry.size, realsize, "entry.size must be realsize");

        let out = reader.extract_to_vec(&entry).expect("extract");
        assert_eq!(out.len() as u64, realsize);
        assert!(out[0..100].iter().all(|&b| b == b'A'), "run 0 content");
        assert!(
            out[100..10_000].iter().all(|&b| b == 0),
            "hole between runs must be zeros"
        );
        assert!(
            out[10_000..10_500].iter().all(|&b| b == b'B'),
            "run 1 content"
        );
        assert!(
            out[10_500..].iter().all(|&b| b == 0),
            "trailing hole must be zeros"
        );
    }

    #[test]
    fn test_tar_sparse_gnu_old_format_extended() {
        // Forces a continuation block: 5 runs total, only first 4 fit in
        // primary. realsize=1_048_576 (1 MiB).
        let realsize: u64 = 1_048_576;
        let primary_runs: Vec<(u64, u64)> =
            vec![(0, 200), (100_000, 300), (300_000, 150), (600_000, 400)];
        let cont_runs: Vec<(u64, u64)> = vec![(900_000, 250)];

        let primary = sparse::build_gnu_sparse_primary(
            "big_sparse.bin",
            realsize,
            &primary_runs,
            true, // isextended
        );
        let cont = sparse::build_gnu_sparse_continuation(&cont_runs, false);

        let mut archive = Vec::new();
        archive.extend_from_slice(&primary);
        archive.extend_from_slice(&cont);

        // Concatenated run data (sum = 1300 bytes) padded to 1536 (3 blocks).
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(b'A', 200));
        data.extend(std::iter::repeat_n(b'B', 300));
        data.extend(std::iter::repeat_n(b'C', 150));
        data.extend(std::iter::repeat_n(b'D', 400));
        data.extend(std::iter::repeat_n(b'E', 250));
        pad_to_block(&mut archive, &data);

        // End-of-archive.
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let cursor = Cursor::new(archive);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "big_sparse.bin");
        assert_eq!(entry.size, realsize);

        let out = reader.extract_to_vec(&entry).expect("extract");
        assert_eq!(out.len() as u64, realsize);
        assert!(out[0..200].iter().all(|&b| b == b'A'));
        assert!(out[200..100_000].iter().all(|&b| b == 0));
        assert!(out[100_000..100_300].iter().all(|&b| b == b'B'));
        assert!(out[100_300..300_000].iter().all(|&b| b == 0));
        assert!(out[300_000..300_150].iter().all(|&b| b == b'C'));
        assert!(out[600_000..600_400].iter().all(|&b| b == b'D'));
        assert!(out[900_000..900_250].iter().all(|&b| b == b'E'));
        assert!(out[900_250..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_tar_sparse_pax_format() {
        // PAX 0.1 sparse: extended header with GNU.sparse.{name,map,realsize},
        // followed by a regular '0' data entry whose name is the GNU-tar
        // sentinel `./GNUSparseFile.0/sparse.dat`. The reader must shadow
        // the sentinel name with `GNU.sparse.name=sparse.dat`.
        // Payload: 100 bytes of 'X' at offset 0, 200 bytes of 'Y' at 5000.
        let realsize: u64 = 10_000;
        let runs: Vec<(u64, u64)> = vec![(0, 100), (5_000, 200)];
        let stored: u64 = runs.iter().map(|(_, n)| *n).sum();

        // Build PAX payload.
        let mk_record =
            |k: &str, v: &str| -> String { TarWriter::<Vec<u8>>::format_pax_record(k, v) };
        let mut pax_payload = String::new();
        pax_payload.push_str(&mk_record("GNU.sparse.name", "sparse.dat"));
        pax_payload.push_str(&mk_record("GNU.sparse.realsize", &realsize.to_string()));
        pax_payload.push_str(&mk_record("GNU.sparse.map", "0,100,5000,200"));
        pax_payload.push_str(&mk_record("size", &stored.to_string()));

        let pax_bytes = pax_payload.as_bytes();
        let pax_hdr = sparse::build_pax_header_block(b'x', pax_bytes.len() as u64);

        // Build the data-entry header: typeflag '0', size = stored size,
        // name = the GNU-tar `./GNUSparseFile.XXXX/<file>` sentinel. A
        // real-world GNU-tar archive places this dummy name on the data
        // header; `GNU.sparse.name` holds the canonical name and the
        // reader must prefer it.
        let dummy_name = "./GNUSparseFile.12345/sparse.dat";
        let mut data_hdr = TarHeader::new_file(dummy_name, stored, 0o644);
        data_hdr.typeflag = b'0';
        let data_hdr_block = data_hdr.to_block().expect("data_hdr.to_block");

        let mut archive = Vec::new();
        archive.extend_from_slice(&pax_hdr);
        pad_to_block(&mut archive, pax_bytes);
        archive.extend_from_slice(&data_hdr_block);

        // Run payload.
        let mut payload = Vec::new();
        payload.extend(std::iter::repeat_n(b'X', 100));
        payload.extend(std::iter::repeat_n(b'Y', 200));
        pad_to_block(&mut archive, &payload);

        // End-of-archive.
        archive.extend_from_slice(&[0u8; BLOCK_SIZE * 2]);

        let cursor = Cursor::new(archive);
        let mut reader = TarReader::new(cursor).expect("TarReader::new");

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(
            entry.name, "sparse.dat",
            "GNU.sparse.name must shadow the dummy GNUSparseFile path"
        );
        assert_eq!(
            entry.size, realsize,
            "entry.size must reflect GNU.sparse.realsize, not stored size"
        );

        let out = reader.extract_to_vec(&entry).expect("extract");
        assert_eq!(out.len() as u64, realsize);
        assert!(out[0..100].iter().all(|&b| b == b'X'));
        assert!(out[100..5_000].iter().all(|&b| b == 0));
        assert!(out[5_000..5_200].iter().all(|&b| b == b'Y'));
        assert!(out[5_200..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_tar_sparse_malformed_map() {
        // Two runs that overlap: (0,200) and (100,50). Parser should
        // reject at `validate()` time with InvalidHeader.
        let realsize: u64 = 1_000;
        let runs: Vec<(u64, u64)> = vec![(0, 200), (100, 50)];
        let primary = sparse::build_gnu_sparse_primary("bad.bin", realsize, &runs, false);

        let mut archive = Vec::new();
        archive.extend_from_slice(&primary);
        // A block of filler data (not that we'd read it — validate() errors first).
        archive.extend_from_slice(&[0u8; BLOCK_SIZE]);

        let cursor = Cursor::new(archive);
        let err = TarReader::new(cursor)
            .err()
            .expect("overlapping sparse map must be rejected");
        match err {
            OxiArcError::InvalidHeader { .. } => {}
            other => panic!("unexpected error variant: {:?}", other),
        }
    }
}

#[cfg(test)]
mod lenient_tests;

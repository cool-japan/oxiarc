//! TAR archive writer.

use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;
use std::io::Write;

use super::header::TarHeader;
use super::{BLOCK_SIZE, PAX_HEADER};

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
    pub(crate) fn format_pax_record(key: &str, value: &str) -> String {
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

    /// Write an entry using a pre-existing [`TarHeader`] verbatim, preserving
    /// all metadata (uid, gid, uname, gname, mtime, mode, linkname, typeflag).
    ///
    /// For regular file entries the caller must supply `data`; for all other
    /// entry types (directories, symlinks, hard links) `data` must be empty.
    ///
    /// This is used by `oxiarc add` to copy existing TAR entries without any
    /// metadata loss.
    pub fn add_entry_from_header(&mut self, header: &TarHeader, data: &[u8]) -> Result<()> {
        // Emit progress: entry start
        let idx = self.entry_index;
        if let Some(ref handle) = self.progress {
            handle.on_entry(&header.name, idx);
        }
        self.entry_index += 1;

        // For long names/links, emit PAX headers first so downstream readers
        // can handle names longer than 100 bytes and links longer than 100
        // bytes correctly. Both conditions are checked independently.
        let needs_pax_path = header.name.len() > 100;
        let needs_pax_link = !header.linkname.is_empty() && header.linkname.len() > 100;

        if needs_pax_path || needs_pax_link {
            let path_str = if needs_pax_path {
                header.name.as_str()
            } else {
                ""
            };
            let link_str = if needs_pax_link {
                Some(header.linkname.as_str())
            } else {
                None
            };
            self.write_pax_header(path_str, link_str)?;
        }

        self.write_header(header)?;

        if !data.is_empty() {
            self.write_data(data)?;
        }

        // Emit progress: bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(data.len() as u64, None);
        }

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

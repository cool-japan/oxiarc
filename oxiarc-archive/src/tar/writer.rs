//! TAR archive writer.

use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;
use std::io::Write;

use super::header::TarHeader;
use super::{BLOCK_SIZE, PAX_HEADER};

/// Maximum byte length of the UStar `name` field.
const TAR_NAME_MAX: usize = 100;

/// Maximum byte length of the UStar `linkname` field.
const TAR_LINKNAME_MAX: usize = 100;

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
        let needs_pax = name.len() > TAR_NAME_MAX;

        if needs_pax {
            self.write_pax_header(name, None)?;
            // Use a char-boundary-safe truncated fallback name for the
            // regular header; PAX-aware readers restore the full name.
            let short_name = Self::tar_fallback_name(name);
            let header = TarHeader::new_file(&short_name, data.len() as u64, mode);
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

    /// Build a fallback name for the UStar `name` field when the real name
    /// exceeds [`TAR_NAME_MAX`] bytes and the full name travels in a PAX
    /// `path` record.
    ///
    /// Keeps the trailing portion of the name (most significant for humans
    /// inspecting the archive with non-PAX tools), truncated at a UTF-8
    /// character boundary (floored) so multi-byte names such as Japanese
    /// never panic or produce invalid UTF-8. A trailing `/` (directory
    /// marker) is preserved.
    pub(crate) fn tar_fallback_name(name: &str) -> String {
        // The header serializer NUL-terminates the 100-byte `name` field,
        // leaving 99 usable bytes; budget accordingly so the fallback lands
        // in the block without further truncation.
        let field_budget = TAR_NAME_MAX - 1;
        let had_trailing_slash = name.ends_with('/');
        let trimmed = name.trim_end_matches('/');
        let budget = if had_trailing_slash {
            field_budget - 1
        } else {
            field_budget
        };
        let mut start = trimmed.len().saturating_sub(budget);
        while start < trimmed.len() && !trimmed.is_char_boundary(start) {
            start += 1;
        }
        let mut fallback = trimmed[start..].to_string();
        if had_trailing_slash {
            fallback.push('/');
        }
        if fallback.trim_end_matches('/').is_empty() {
            fallback = "long_name".to_string();
        }
        fallback
    }

    /// Build a fallback link target for the UStar `linkname` field when the
    /// real target exceeds [`TAR_LINKNAME_MAX`] bytes and the full target
    /// travels in a PAX `linkpath` record.
    ///
    /// Keeps the leading portion, truncated at a UTF-8 character boundary
    /// (floored).
    pub(crate) fn tar_fallback_linkname(target: &str) -> String {
        // 99 usable bytes: the serializer NUL-terminates the field.
        let mut end = target.len().min(TAR_LINKNAME_MAX - 1);
        while end > 0 && !target.is_char_boundary(end) {
            end -= 1;
        }
        target[..end].to_string()
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

        if dir_name.len() > TAR_NAME_MAX {
            // Long directory name: full path travels in a PAX record, the
            // UStar header carries a char-boundary-safe fallback.
            self.write_pax_header(&dir_name, None)?;
            let short_name = Self::tar_fallback_name(&dir_name);
            let header = TarHeader::new_directory(&short_name, mode);
            self.write_header(&header)?;
        } else {
            let header = TarHeader::new_directory(&dir_name, mode);
            self.write_header(&header)?;
        }
        Ok(())
    }

    /// Add a symlink to the archive.
    pub fn add_symlink(&mut self, name: &str, target: &str) -> Result<()> {
        let needs_pax_path = name.len() > TAR_NAME_MAX;
        let needs_pax_link = target.len() > TAR_LINKNAME_MAX;

        if needs_pax_path || needs_pax_link {
            let path_str = if needs_pax_path { name } else { "" };
            let link_str = if needs_pax_link { Some(target) } else { None };
            self.write_pax_header(path_str, link_str)?;
        }

        let short_name = if needs_pax_path {
            Self::tar_fallback_name(name)
        } else {
            name.to_string()
        };
        let short_target = if needs_pax_link {
            Self::tar_fallback_linkname(target)
        } else {
            target.to_string()
        };

        let header = TarHeader::new_symlink(&short_name, &short_target);
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
        let needs_pax_path = header.name.len() > TAR_NAME_MAX;
        let needs_pax_link =
            !header.linkname.is_empty() && header.linkname.len() > TAR_LINKNAME_MAX;

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

            // The UStar block itself must carry char-boundary-safe fallback
            // values; PAX-aware readers restore the exact name and target.
            let mut short = header.clone();
            if needs_pax_path {
                short.name = Self::tar_fallback_name(&header.name);
            }
            if needs_pax_link {
                short.linkname = Self::tar_fallback_linkname(&header.linkname);
            }
            self.write_header(&short)?;
        } else {
            self.write_header(header)?;
        }

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
        // Use ManuallyDrop to prevent the Drop impl from running (it would
        // otherwise try to finish the archive a second time).
        let mut this = std::mem::ManuallyDrop::new(self);
        let write_result: std::io::Result<()> = if this.finished {
            Ok(())
        } else {
            this.writer
                .write_all(&[0u8; BLOCK_SIZE])
                .and_then(|_| this.writer.write_all(&[0u8; BLOCK_SIZE]))
                .and_then(|_| this.writer.flush())
        };

        // SAFETY: `this` is `ManuallyDrop`, so none of its fields have been
        // dropped yet. Read `writer` out without dropping it — it becomes
        // either the returned value, or an ordinary local that drops
        // normally if `write_result` turns out to be an error below — then
        // explicitly drop `progress`, the only other field owning a
        // resource (an `Arc` clone), so it is never leaked. `finished` and
        // `entry_index` are `Copy` and own nothing.
        let writer = unsafe { std::ptr::read(&this.writer) };
        unsafe {
            std::ptr::drop_in_place(&mut this.progress);
        }

        write_result?;
        Ok(writer)
    }
}

impl<W: Write> Drop for TarWriter<W> {
    fn drop(&mut self) {
        // Attempt to finish on drop, ignore errors
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxiarc_core::progress::{ProgressHandle, ProgressSink};
    use std::sync::Arc;

    /// Regression test (manual code review, not Miri-flagged): `into_inner()`
    /// used to suppress `Drop` for the *entire* struct via `ManuallyDrop`,
    /// which silently leaked every other owned field too — most observably
    /// the `Arc<dyn ProgressSink>` clone held in `progress`, whose refcount
    /// was never decremented. Confirms `into_inner()` now drops `progress`
    /// properly instead of leaking it.
    #[test]
    fn test_tar_writer_into_inner_does_not_leak_progress() {
        struct NoopSink;
        impl ProgressSink for NoopSink {
            fn on_progress(&self, _processed: u64, _total: Option<u64>) {}
        }

        let sink = Arc::new(NoopSink);
        let handle: ProgressHandle = sink.clone();

        let mut writer = TarWriter::new(Vec::new()).with_progress(handle);
        writer
            .add_file("leak_check.txt", b"regression test data")
            .expect("add_file");

        let _inner = writer.into_inner().expect("into_inner");

        assert_eq!(
            Arc::strong_count(&sink),
            1,
            "into_inner() must drop the writer's internal Arc<ProgressSink> clone, not leak it"
        );
    }
}

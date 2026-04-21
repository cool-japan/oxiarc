//! Async TAR I/O via `spawn_blocking`.
//!
//! Async streams are not yet supported (requires tokio-aware codec internals);
//! use the sync API for streaming access.
//!
//! # Feature Flag
//!
//! This module is only available when the `async-io` feature is enabled:
//!
//! ```toml
//! [dependencies]
//! oxiarc-archive = { version = "0.2.7", features = ["async-io"] }
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! use oxiarc_archive::async_tar::read_tar_entries_async;
//!
//! let entries = read_tar_entries_async("archive.tar").await?;
//! for entry in entries {
//!     println!("{}: {} bytes", entry.name, entry.size);
//! }
//! ```

use oxiarc_core::Entry;
use oxiarc_core::error::{OxiArcError, Result};
use std::path::PathBuf;

use crate::tar::TarReader;

/// Read all entries' metadata from a TAR file asynchronously.
///
/// This function opens the TAR archive at `path`, scans all entries, and
/// returns their metadata.  The actual file I/O is performed on a Tokio
/// blocking thread via [`tokio::task::spawn_blocking`].
///
/// # Arguments
///
/// * `path` - Path to the TAR file.
///
/// # Errors
///
/// Returns an error if the file cannot be opened, read, or parsed.
pub async fn read_tar_entries_async<P: AsRef<std::path::Path>>(path: P) -> Result<Vec<Entry>> {
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let tar = TarReader::new(reader)?;
        Ok(tar.entries().to_vec())
    })
    .await
    .map_err(|join_err| OxiArcError::invalid_header(join_err.to_string()))?
}

/// Read the data of a specific TAR entry by index asynchronously.
///
/// This function opens the TAR archive at `path`, locates the entry at
/// position `index` (0-based), decompresses it if necessary, and returns
/// its raw bytes.  The actual file I/O is performed on a Tokio blocking
/// thread via [`tokio::task::spawn_blocking`].
///
/// # Arguments
///
/// * `path`  - Path to the TAR file.
/// * `index` - Zero-based index of the entry to read.
///
/// # Errors
///
/// Returns an error if the file cannot be opened, the index is out of
/// bounds, or I/O fails.
pub async fn read_tar_entry_async<P: AsRef<std::path::Path>>(
    path: P,
    index: usize,
) -> Result<Vec<u8>> {
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut tar = TarReader::new(reader)?;

        let num_entries = tar.entries().len();
        let entry = tar
            .entries()
            .get(index)
            .ok_or_else(|| {
                OxiArcError::invalid_header(format!(
                    "TAR entry index {} out of range (archive has {} entries)",
                    index, num_entries,
                ))
            })?
            .clone();

        tar.extract_to_vec(&entry)
    })
    .await
    .map_err(|join_err| OxiArcError::invalid_header(join_err.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tar::TarWriter;

    /// Build a TAR archive in a temp file and return the path.
    fn build_test_tar() -> (std::path::PathBuf, Vec<(&'static str, &'static [u8])>) {
        let mut dir = std::env::temp_dir();
        dir.push(format!("oxiarc_async_tar_test_{}.tar", std::process::id()));
        let files: Vec<(&str, &[u8])> = vec![
            ("hello.txt", b"Hello, async TAR!"),
            ("world.txt", b"Another file with content."),
        ];
        {
            let f = std::fs::File::create(&dir).expect("create temp file");
            let mut writer = TarWriter::new(f);
            for (name, data) in &files {
                writer.add_file(name, data).expect("add_file");
            }
            writer.finish().expect("finish");
            // writer and the file are both dropped here, flushing automatically
        }
        (dir, files)
    }

    #[tokio::test]
    async fn test_read_tar_entries_async() {
        let (path, expected_files) = build_test_tar();

        let entries = read_tar_entries_async(&path)
            .await
            .expect("read_tar_entries_async");

        let _ = std::fs::remove_file(&path);

        assert_eq!(entries.len(), expected_files.len());
        for (i, (name, data)) in expected_files.iter().enumerate() {
            assert_eq!(entries[i].name, *name);
            assert_eq!(entries[i].size, data.len() as u64);
        }
    }

    #[tokio::test]
    async fn test_read_tar_entry_async() {
        let (path, expected_files) = build_test_tar();

        for (i, (_name, data)) in expected_files.iter().enumerate() {
            let content = read_tar_entry_async(&path, i)
                .await
                .expect("read_tar_entry_async");
            assert_eq!(&content, data);
        }

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_read_tar_entry_async_out_of_bounds() {
        let (path, _) = build_test_tar();

        let result = read_tar_entry_async(&path, 999).await;
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err(), "expected error for out-of-bounds index");
    }
}

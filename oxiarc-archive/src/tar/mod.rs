//! TAR archive format support.
//!
//! This module provides reading and extraction of TAR archives with support for:
//! - UStar format (POSIX.1-1988)
//! - PAX extended headers (POSIX.1-2001) for long filenames and additional metadata

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
pub(crate) const PAX_HEADER: u8 = b'x';

/// PAX typeflag for global extended header (applies to all subsequent files).
pub(crate) const PAX_GLOBAL_HEADER: u8 = b'g';

/// GNU LongName typeflag.
pub(crate) const GNU_LONGNAME: u8 = b'L';

/// GNU LongLink typeflag.
pub(crate) const GNU_LONGLINK: u8 = b'K';

#[cfg(feature = "mmap")]
use oxiarc_core::error::Result;

// Sub-modules
pub mod header;
pub mod reader;
pub(crate) mod sparse;
pub mod writer;

// Re-exports for public API compatibility
pub use header::TarHeader;
pub use reader::TarReader;
pub use writer::TarWriter;

// Re-export EntryType so test modules using `super::*` can access it.
#[cfg(test)]
pub(crate) use oxiarc_core::EntryType;

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
mod lenient_tests;

#[cfg(test)]
mod raw_preserve_tests;

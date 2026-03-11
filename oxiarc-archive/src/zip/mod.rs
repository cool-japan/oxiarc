//! ZIP archive format support.
//!
//! This module provides reading and writing of ZIP archives as specified
//! in the PKWARE APPNOTE.
//!
//! ## Encryption Support
//!
//! This module supports two types of encryption:
//!
//! ### Traditional PKWARE Encryption (ZipCrypto)
//!
//! The original ZIP encryption specified in APPNOTE. Available via the [`crypto`] module.
//!
//! **Security Note**: ZipCrypto is cryptographically weak and should only be used
//! for legacy compatibility. Consider using AES encryption for new archives.
//!
//! ### AES-256 Encryption (WinZip AE-2)
//!
//! Modern strong encryption following the WinZip AE-2 specification.
//! Available via the [`encryption`] module.
//!
//! ```rust,no_run
//! use oxiarc_archive::zip::ZipWriter;
//!
//! let mut output = Vec::new();
//! let mut writer = ZipWriter::new(&mut output);
//! writer.add_encrypted_file("secret.txt", b"Secret data", b"password123").unwrap();
//! writer.finish().unwrap();
//! ```

pub mod crypto;
pub mod encryption;
mod header;

pub use crypto::{
    ENCRYPTION_HEADER_SIZE, FLAG_ENCRYPTED, ZipCrypto, ZipCryptoReader, ZipCryptoWriter,
};
pub use encryption::{
    AesExtraField, AesStrength, PASSWORD_VERIFICATION_LEN, WINZIP_AES_EXTRA_ID,
    WINZIP_AUTH_CODE_LEN, ZipAesDecryptor, ZipAesEncryptor,
};
pub use header::{
    CompressionMethod, LocalFileHeader, ZipCompressionLevel, ZipReader, ZipWriter,
    get_entry_aes_encryption_info, is_entry_encrypted, is_entry_traditional_encrypted,
};

use oxiarc_core::error::Result;
use std::io::{Read, Seek, Write};

/// Read a ZIP archive.
pub fn read_zip<R: Read + Seek>(reader: R) -> Result<ZipReader<R>> {
    ZipReader::new(reader)
}

/// Create a new ZIP archive writer.
pub fn write_zip<W: Write>(writer: W) -> ZipWriter<W> {
    ZipWriter::new(writer)
}

/// Open a ZIP archive using memory-mapped I/O for efficient large-file reading.
///
/// This function memory-maps the file at `path` and returns a [`ZipReader`]
/// backed by an [`oxiarc_core::mmap::MmapReader`]. Memory-mapped access can
/// be significantly faster than buffered I/O for large archives, especially
/// when performing random-access extraction.
///
/// # Arguments
///
/// * `path` - Path to the ZIP file to open
///
/// # Returns
///
/// A `ZipReader` backed by a memory-mapped reader.
///
/// # Errors
///
/// Returns an error if the file cannot be opened, mapped, or does not
/// contain a valid ZIP archive.
///
/// # Example
///
/// ```no_run
/// use oxiarc_archive::zip::open_zip_mmap;
///
/// let mut reader = open_zip_mmap("large_archive.zip").unwrap();
/// let entries = reader.entries().to_vec();
/// for entry in &entries {
///     println!("{}", entry.name);
/// }
/// ```
#[cfg(feature = "mmap")]
pub fn open_zip_mmap<P: AsRef<std::path::Path>>(
    path: P,
) -> Result<ZipReader<oxiarc_core::mmap::MmapReader>> {
    let reader = oxiarc_core::mmap::MmapReader::open(path)?;
    ZipReader::new(reader)
}

#[cfg(test)]
#[cfg(feature = "mmap")]
mod mmap_tests {
    use super::*;
    use std::io::Write;

    /// Write a test ZIP to a temp file and return the path.
    fn create_test_zip_file(name: &str) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("oxiarc_mmap_zip_test_{}.zip", name));

        let mut zip_bytes = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut zip_bytes);
            writer
                .add_file("hello.txt", b"Hello, mmap world!")
                .expect("add_file failed");
            writer
                .add_file(
                    "repeat.txt",
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(200).as_slice(),
                )
                .expect("add_file failed");
            writer.finish().expect("finish failed");
        }

        let mut file = std::fs::File::create(&path).expect("create failed");
        file.write_all(&zip_bytes).expect("write failed");
        file.sync_all().expect("sync failed");
        path
    }

    #[test]
    fn test_mmap_zip_read() {
        let path = create_test_zip_file("read");

        let mut reader = open_zip_mmap(&path).expect("open_zip_mmap failed");
        let entries = reader.entries().to_vec();

        assert_eq!(entries.len(), 2);

        let hello = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .expect("hello.txt");
        let data = reader.extract(hello).expect("extract hello.txt");
        assert_eq!(data, b"Hello, mmap world!");

        let repeat = entries
            .iter()
            .find(|e| e.name == "repeat.txt")
            .expect("repeat.txt");
        let data = reader.extract(repeat).expect("extract repeat.txt");
        let expected: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(200).to_vec();
        assert_eq!(data, expected);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mmap_zip_multiple_reads() {
        let path = create_test_zip_file("multi_read");

        let mut reader = open_zip_mmap(&path).expect("open_zip_mmap failed");

        // Read the same entry twice to verify seek-based re-reading
        let entries = reader.entries().to_vec();
        let hello = entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .expect("hello.txt");

        let data1 = reader.extract(hello).expect("first extract");
        let data2 = reader.extract(hello).expect("second extract");
        assert_eq!(data1, data2);
        assert_eq!(data1, b"Hello, mmap world!");

        let _ = std::fs::remove_file(&path);
    }
}

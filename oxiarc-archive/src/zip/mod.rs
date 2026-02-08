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

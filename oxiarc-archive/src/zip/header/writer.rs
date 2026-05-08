//! ZIP archive writer implementation.

use super::super::crypto::{ENCRYPTION_HEADER_SIZE, FLAG_ENCRYPTED, ZipCrypto};
use super::super::encryption::{
    AesExtraField, AesStrength, PASSWORD_VERIFICATION_LEN, WINZIP_AUTH_CODE_LEN, ZipAesEncryptor,
    generate_salt,
};
use super::types::{
    CentralDirEntry, CompressionMethod, END_OF_CENTRAL_DIR_SIG, LOCAL_FILE_HEADER_SIG,
    METHOD_AES_ENCRYPTED, ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG, ZIP64_END_OF_CENTRAL_DIR_SIG,
    ZIP64_EXTRA_FIELD_ID, ZIP64_MARKER_16, ZIP64_MARKER_32, ZipCompressionLevel,
};
use oxiarc_core::Crc32;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_deflate::deflate;
use oxiarc_lzma::{LzmaEncoder, LzmaLevel};
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// General-purpose bit flag bit 1: LZMA EOS marker present.
const FLAG_LZMA_EOS: u16 = 0x0002;

/// LZMA method-14 version bytes written into the method-14 header.
const LZMA_METHOD14_MAJOR_VER: u8 = 0x13;
const LZMA_METHOD14_MINOR_VER: u8 = 0x00;

/// Fixed dict_size for LZMA compression when writing ZIP entries.
/// 16 MB — a good balance between speed and compression ratio.
const LZMA_DICT_SIZE: u32 = 1 << 24;

/// ZIP archive writer.
pub struct ZipWriter<W: Write> {
    writer: W,
    entries: Vec<CentralDirEntry>,
    offset: u64,
    compression: ZipCompressionLevel,
    finished: bool,
    progress: Option<ProgressHandle>,
}

impl<W: Write> ZipWriter<W> {
    /// Create a new ZIP writer with default compression.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            entries: Vec::new(),
            offset: 0,
            compression: ZipCompressionLevel::default(),
            finished: false,
            progress: None,
        }
    }

    /// Attach a progress handle to this writer.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Set the compression level for subsequent files.
    pub fn set_compression(&mut self, level: ZipCompressionLevel) {
        self.compression = level;
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_options(name, data, self.compression)
    }

    /// Add a file with Stored (no compression) regardless of the writer's
    /// configured compression level.
    ///
    /// Useful when the caller has already determined that compression is not
    /// worthwhile — e.g. the CLI `--compress-threshold` flag routes small
    /// files here so they skip deflate entirely.
    pub fn add_file_stored(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_options(name, data, ZipCompressionLevel::Store)
    }

    /// Add a file with specific compression.
    pub fn add_file_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        compression: ZipCompressionLevel,
    ) -> Result<()> {
        // Progress: notify about entry start
        let file_index = self.entries.len() as u64;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, file_index);
        }

        let crc32 = Crc32::compute(data);

        // Get current time for DOS format
        let (mtime, mdate) = Self::current_dos_time();

        // Compress data
        let (compressed_data, method): (Vec<u8>, u16) = match compression {
            ZipCompressionLevel::Store => (data.to_vec(), 0),
            ZipCompressionLevel::Fast => {
                let compressed = deflate(data, 1)?;
                // Only use compression if it's smaller
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Normal => {
                let compressed = deflate(data, 6)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Best => {
                let compressed = deflate(data, 9)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
        };

        let compressed_size = compressed_data.len() as u64;
        let uncompressed_size = data.len() as u64;
        let local_header_offset = self.offset;

        // Check if we need Zip64
        let needs_zip64 = compressed_size >= ZIP64_MARKER_32 as u64
            || uncompressed_size >= ZIP64_MARKER_32 as u64
            || local_header_offset >= ZIP64_MARKER_32 as u64;

        // Version needed: 45 for Zip64, 20 for deflate, 10 for store
        let version_needed: u16 = if needs_zip64 {
            45
        } else if method == 8 {
            20
        } else {
            10
        };

        // Write local file header
        let filename_bytes = name.as_bytes();

        // Build Zip64 extra field for local header if needed
        let mut local_extra = Vec::new();
        if needs_zip64 {
            local_extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes()); // Data size
            local_extra.extend_from_slice(&uncompressed_size.to_le_bytes());
            local_extra.extend_from_slice(&compressed_size.to_le_bytes());
        }

        // Use marker values for Zip64
        let compressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            compressed_size as u32
        };
        let uncompressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            uncompressed_size as u32
        };

        // Signature
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        // Version needed
        self.writer.write_all(&version_needed.to_le_bytes())?;
        // Flags (0 = no special flags)
        self.writer.write_all(&0u16.to_le_bytes())?;
        // Compression method
        self.writer.write_all(&method.to_le_bytes())?;
        // Modification time
        self.writer.write_all(&mtime.to_le_bytes())?;
        // Modification date
        self.writer.write_all(&mdate.to_le_bytes())?;
        // CRC-32
        self.writer.write_all(&crc32.to_le_bytes())?;
        // Compressed size
        self.writer.write_all(&compressed_size_32.to_le_bytes())?;
        // Uncompressed size
        self.writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        // Filename length
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        // Extra field length
        self.writer
            .write_all(&(local_extra.len() as u16).to_le_bytes())?;
        // Filename
        self.writer.write_all(filename_bytes)?;
        // Extra field
        self.writer.write_all(&local_extra)?;

        // Write file data
        self.writer.write_all(&compressed_data)?;

        // Update offset (30 = local header fixed size)
        self.offset += 30
            + filename_bytes.len() as u64
            + local_extra.len() as u64
            + compressed_data.len() as u64;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E, // Unix, version 3.0
            version_needed,
            flags: 0,
            method,
            mtime,
            mdate,
            crc32,
            compressed_size,
            uncompressed_size,
            filename: name.to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16, // Regular file, rw-r--r--
            local_header_offset,
        });

        // Progress: notify about bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(uncompressed_size, None);
        }

        Ok(())
    }

    /// Add a file to the archive compressed with LZMA (method 14).
    ///
    /// The data is compressed using LZMA and stored per APPNOTE §5.8.8:
    /// - Local file header has compression method = 14
    /// - General-purpose bit flag bit 1 is set (EOS marker present in stream)
    /// - Compressed data starts with `[major_ver][minor_ver][props_size_le16][5-byte-props][stream]`
    pub fn add_file_lzma(&mut self, name: &str, data: &[u8]) -> Result<()> {
        // Progress: notify about entry start
        let file_index = self.entries.len() as u64;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, file_index);
        }

        let crc32 = Crc32::compute(data);
        let (mtime, mdate) = Self::current_dos_time();

        // Compress using LZMA — the encoder always emits an EOS marker
        let encoder = LzmaEncoder::new(LzmaLevel::DEFAULT, LZMA_DICT_SIZE);
        let props = encoder.properties();
        let lzma_stream = encoder
            .compress(data)
            .map_err(|e| OxiArcError::invalid_header(format!("LZMA compression failed: {}", e)))?;

        // Build the 5-byte props block: [props_byte][dict_size_le32]
        let mut props_block = [0u8; 5];
        props_block[0] = props.to_byte();
        props_block[1..5].copy_from_slice(&LZMA_DICT_SIZE.to_le_bytes());

        // Build the method-14 header: [major][minor][props_size_le16][props_block]
        let props_size: u16 = 5;
        let mut method14_payload = Vec::with_capacity(4 + 5 + lzma_stream.len());
        method14_payload.push(LZMA_METHOD14_MAJOR_VER);
        method14_payload.push(LZMA_METHOD14_MINOR_VER);
        method14_payload.extend_from_slice(&props_size.to_le_bytes());
        method14_payload.extend_from_slice(&props_block);
        method14_payload.extend_from_slice(&lzma_stream);

        let compressed_size = method14_payload.len() as u64;
        let uncompressed_size = data.len() as u64;
        let local_header_offset = self.offset;

        // Version needed: 63 for LZMA (per APPNOTE §4.4.3.2)
        let needs_zip64 = compressed_size >= ZIP64_MARKER_32 as u64
            || uncompressed_size >= ZIP64_MARKER_32 as u64
            || local_header_offset >= ZIP64_MARKER_32 as u64;

        // Version 63 required for LZMA regardless of Zip64 status
        let version_needed: u16 = 63;

        let filename_bytes = name.as_bytes();

        // Build Zip64 extra field if needed
        let mut local_extra = Vec::new();
        if needs_zip64 {
            local_extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes());
            local_extra.extend_from_slice(&uncompressed_size.to_le_bytes());
            local_extra.extend_from_slice(&compressed_size.to_le_bytes());
        }

        let compressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            compressed_size as u32
        };
        let uncompressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            uncompressed_size as u32
        };

        // General-purpose bit flag: bit 1 = EOS marker present in LZMA stream
        let flags: u16 = FLAG_LZMA_EOS;
        // Compression method 14 = LZMA
        let method: u16 = 14;

        // Signature
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        // Version needed
        self.writer.write_all(&version_needed.to_le_bytes())?;
        // Flags (bit 1 = EOS marker present)
        self.writer.write_all(&flags.to_le_bytes())?;
        // Compression method (14 = LZMA)
        self.writer.write_all(&method.to_le_bytes())?;
        // Modification time
        self.writer.write_all(&mtime.to_le_bytes())?;
        // Modification date
        self.writer.write_all(&mdate.to_le_bytes())?;
        // CRC-32
        self.writer.write_all(&crc32.to_le_bytes())?;
        // Compressed size
        self.writer.write_all(&compressed_size_32.to_le_bytes())?;
        // Uncompressed size
        self.writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        // Filename length
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        // Extra field length
        self.writer
            .write_all(&(local_extra.len() as u16).to_le_bytes())?;
        // Filename
        self.writer.write_all(filename_bytes)?;
        // Extra field
        self.writer.write_all(&local_extra)?;
        // Compressed LZMA data (method-14 format)
        self.writer.write_all(&method14_payload)?;

        // Update offset (30 = local header fixed size)
        self.offset +=
            30 + filename_bytes.len() as u64 + local_extra.len() as u64 + compressed_size;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E, // Unix, version 3.0
            version_needed,
            flags,
            method,
            mtime,
            mdate,
            crc32,
            compressed_size,
            uncompressed_size,
            filename: name.to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16,
            local_header_offset,
        });

        // Progress: notify about bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(uncompressed_size, None);
        }

        Ok(())
    }

    /// Add an encrypted file to the archive using AES-256 encryption.
    ///
    /// This method encrypts the file data using the WinZip AE-2 specification:
    /// - AES-256 encryption in CTR mode
    /// - PBKDF2-SHA1 key derivation (1000 iterations)
    /// - HMAC-SHA1 authentication
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the file in the archive
    /// * `data` - The uncompressed file data
    /// * `password` - The password for encryption
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use oxiarc_archive::zip::ZipWriter;
    ///
    /// let mut output = Vec::new();
    /// let mut writer = ZipWriter::new(&mut output);
    /// writer.add_encrypted_file("secret.txt", b"Secret data", b"password123").unwrap();
    /// writer.finish().unwrap();
    /// ```
    pub fn add_encrypted_file(&mut self, name: &str, data: &[u8], password: &[u8]) -> Result<()> {
        self.add_encrypted_file_with_options(
            name,
            data,
            password,
            self.compression,
            AesStrength::Aes256,
        )
    }

    /// Add an encrypted file with specific compression and encryption strength.
    pub fn add_encrypted_file_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        password: &[u8],
        compression: ZipCompressionLevel,
        strength: AesStrength,
    ) -> Result<()> {
        // Compute CRC-32 of original data
        let crc32 = Crc32::compute(data);

        // Get current time for DOS format
        let (mtime, mdate) = Self::current_dos_time();

        // Compress data first (before encryption)
        let (compressed_data, actual_method): (Vec<u8>, u16) = match compression {
            ZipCompressionLevel::Store => (data.to_vec(), 0),
            ZipCompressionLevel::Fast => {
                let compressed = deflate(data, 1)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Normal => {
                let compressed = deflate(data, 6)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Best => {
                let compressed = deflate(data, 9)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
        };

        // Generate salt
        let salt = generate_salt(strength.salt_len());

        // Create encryptor and get password verification bytes
        let (mut encryptor, pw_verification): (ZipAesEncryptor, [u8; 2]) =
            ZipAesEncryptor::new(password, &salt, strength)?;

        // Encrypt the compressed data
        let mut encrypted_data = compressed_data.clone();
        encryptor.encrypt(&mut encrypted_data);

        // Get authentication code
        let auth_code = encryptor.finalize();

        // Build AES extra field
        let aes_extra = AesExtraField::new(strength, actual_method);
        let aes_extra_bytes = aes_extra.to_bytes();

        // Calculate sizes
        // Encrypted data layout: salt + pw_verification + encrypted_data + auth_code
        let salt_len = strength.salt_len() as u64;
        let encrypted_payload_size = salt_len
            + PASSWORD_VERIFICATION_LEN as u64
            + encrypted_data.len() as u64
            + WINZIP_AUTH_CODE_LEN as u64;

        let uncompressed_size = data.len() as u64;
        let local_header_offset = self.offset;

        // Check if we need Zip64
        let needs_zip64 = encrypted_payload_size >= ZIP64_MARKER_32 as u64
            || uncompressed_size >= ZIP64_MARKER_32 as u64
            || local_header_offset >= ZIP64_MARKER_32 as u64;

        // Version needed: 51 for AES encryption
        let version_needed: u16 = 51;

        // Build Zip64 extra field for local header if needed
        let mut local_extra = Vec::new();
        if needs_zip64 {
            local_extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes());
            local_extra.extend_from_slice(&uncompressed_size.to_le_bytes());
            local_extra.extend_from_slice(&encrypted_payload_size.to_le_bytes());
        }
        // Add AES extra field
        local_extra.extend_from_slice(&aes_extra_bytes);

        // Use marker values for Zip64
        let compressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            encrypted_payload_size as u32
        };
        let uncompressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            uncompressed_size as u32
        };

        // Write local file header
        let filename_bytes = name.as_bytes();

        // Signature
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        // Version needed
        self.writer.write_all(&version_needed.to_le_bytes())?;
        // Flags (bit 0 = encrypted)
        self.writer.write_all(&FLAG_ENCRYPTED.to_le_bytes())?;
        // Compression method (99 = AES encrypted)
        self.writer.write_all(&METHOD_AES_ENCRYPTED.to_le_bytes())?;
        // Modification time
        self.writer.write_all(&mtime.to_le_bytes())?;
        // Modification date
        self.writer.write_all(&mdate.to_le_bytes())?;
        // CRC-32 (for AE-2, this is stored in local header; for AE-1 it would be 0)
        self.writer.write_all(&crc32.to_le_bytes())?;
        // Compressed size (includes encryption overhead)
        self.writer.write_all(&compressed_size_32.to_le_bytes())?;
        // Uncompressed size
        self.writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        // Filename length
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        // Extra field length
        self.writer
            .write_all(&(local_extra.len() as u16).to_le_bytes())?;
        // Filename
        self.writer.write_all(filename_bytes)?;
        // Extra field
        self.writer.write_all(&local_extra)?;

        // Write encrypted data: salt + pw_verification + encrypted_data + auth_code
        self.writer.write_all(&salt)?;
        self.writer.write_all(&pw_verification)?;
        self.writer.write_all(&encrypted_data)?;
        self.writer.write_all(&auth_code)?;

        // Update offset
        self.offset +=
            30 + filename_bytes.len() as u64 + local_extra.len() as u64 + encrypted_payload_size;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E, // Unix, version 3.0
            version_needed,
            flags: FLAG_ENCRYPTED,
            method: METHOD_AES_ENCRYPTED,
            mtime,
            mdate,
            crc32,
            compressed_size: encrypted_payload_size,
            uncompressed_size,
            filename: name.to_string(),
            extra: aes_extra_bytes, // Include AES extra in central directory
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16,
            local_header_offset,
        });

        Ok(())
    }

    /// Add a file encrypted with traditional PKWARE (ZipCrypto) encryption.
    ///
    /// This method uses the original ZIP encryption algorithm. While widely
    /// compatible, this encryption is cryptographically weak and should only
    /// be used for legacy compatibility.
    ///
    /// For secure encryption, use `add_encrypted_file()` which uses AES-256.
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the file in the archive.
    /// * `data` - The uncompressed file data.
    /// * `password` - The password for encryption.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use oxiarc_archive::zip::ZipWriter;
    ///
    /// let mut output = Vec::new();
    /// let mut writer = ZipWriter::new(&mut output);
    /// writer.add_encrypted_file_traditional("legacy.txt", b"Data", b"password").unwrap();
    /// writer.finish().unwrap();
    /// ```
    pub fn add_encrypted_file_traditional(
        &mut self,
        name: &str,
        data: &[u8],
        password: &[u8],
    ) -> Result<()> {
        self.add_encrypted_file_traditional_with_options(name, data, password, self.compression)
    }

    /// Add a file with traditional PKWARE encryption and specific compression.
    pub fn add_encrypted_file_traditional_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        password: &[u8],
        compression: ZipCompressionLevel,
    ) -> Result<()> {
        // Compute CRC-32 of original data
        let crc32 = Crc32::compute(data);

        // Get current time for DOS format
        let (mtime, mdate) = Self::current_dos_time();

        // Compress data first (before encryption)
        let (compressed_data, method): (Vec<u8>, u16) = match compression {
            ZipCompressionLevel::Store => (data.to_vec(), 0),
            ZipCompressionLevel::Fast => {
                let compressed = deflate(data, 1)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Normal => {
                let compressed = deflate(data, 6)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
            ZipCompressionLevel::Best => {
                let compressed = deflate(data, 9)?;
                if compressed.len() < data.len() {
                    (compressed, 8)
                } else {
                    (data.to_vec(), 0)
                }
            }
        };

        // Create cipher and generate encryption header
        let mut cipher = ZipCrypto::new(password);

        // Generate encryption header using CRC and time-based seeds
        let seed1 = mtime as u64 * 1000 + mdate as u64;
        let seed2 = crc32 as u64 ^ (compressed_data.len() as u64);
        let header = cipher.generate_header_seeded(crc32, seed1, seed2);

        // Encrypt the compressed data
        let mut encrypted_data = compressed_data.clone();
        cipher.encrypt_buffer(&mut encrypted_data);

        // Total size includes encryption header + encrypted data
        let total_encrypted_size = (ENCRYPTION_HEADER_SIZE + encrypted_data.len()) as u64;
        let uncompressed_size = data.len() as u64;
        let local_header_offset = self.offset;

        // Check if we need Zip64
        let needs_zip64 = total_encrypted_size >= ZIP64_MARKER_32 as u64
            || uncompressed_size >= ZIP64_MARKER_32 as u64
            || local_header_offset >= ZIP64_MARKER_32 as u64;

        // Version needed: 45 for Zip64, 20 for deflate+encryption or ZipCrypto
        let version_needed: u16 = if needs_zip64 {
            45
        } else {
            20 // ZipCrypto requires at least version 2.0
        };

        // Build extra field with encryption marker and optionally Zip64
        let mut local_extra = Vec::new();
        if needs_zip64 {
            local_extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes());
            local_extra.extend_from_slice(&uncompressed_size.to_le_bytes());
            local_extra.extend_from_slice(&total_encrypted_size.to_le_bytes());
        }
        // Add encryption marker (0xEE, 0xEE)
        local_extra.extend_from_slice(&[0xEE, 0xEE]);

        // Use marker values for Zip64
        let compressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            total_encrypted_size as u32
        };
        let uncompressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            uncompressed_size as u32
        };

        // Write local file header
        let filename_bytes = name.as_bytes();

        // Signature
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        // Version needed
        self.writer.write_all(&version_needed.to_le_bytes())?;
        // Flags (bit 0 = encrypted)
        self.writer.write_all(&FLAG_ENCRYPTED.to_le_bytes())?;
        // Compression method
        self.writer.write_all(&method.to_le_bytes())?;
        // Modification time
        self.writer.write_all(&mtime.to_le_bytes())?;
        // Modification date
        self.writer.write_all(&mdate.to_le_bytes())?;
        // CRC-32
        self.writer.write_all(&crc32.to_le_bytes())?;
        // Compressed size (includes encryption header)
        self.writer.write_all(&compressed_size_32.to_le_bytes())?;
        // Uncompressed size
        self.writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        // Filename length
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        // Extra field length
        self.writer
            .write_all(&(local_extra.len() as u16).to_le_bytes())?;
        // Filename
        self.writer.write_all(filename_bytes)?;
        // Extra field
        self.writer.write_all(&local_extra)?;

        // Write encryption header
        self.writer.write_all(&header)?;

        // Write encrypted data
        self.writer.write_all(&encrypted_data)?;

        // Update offset
        self.offset +=
            30 + filename_bytes.len() as u64 + local_extra.len() as u64 + total_encrypted_size;

        // Store central directory entry with encryption marker
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E,
            version_needed,
            flags: FLAG_ENCRYPTED,
            method,
            mtime,
            mdate,
            crc32,
            compressed_size: total_encrypted_size,
            uncompressed_size,
            filename: name.to_string(),
            extra: vec![0xEE, 0xEE], // Encryption marker
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16,
            local_header_offset,
        });

        Ok(())
    }

    /// Add a file to the archive with pre-compressed data verbatim.
    ///
    /// This method writes the raw compressed bytes directly without any
    /// re-compression, preserving byte-for-byte fidelity for existing archive
    /// entries. The caller must supply the correct `crc32` of the *uncompressed*
    /// data and the `uncompressed_size`.
    ///
    /// For LZMA (method 14) entries the EOS-marker general-purpose flag is set
    /// automatically. All other methods use flags = 0.
    ///
    /// # Arguments
    ///
    /// * `name` – Entry name in the archive
    /// * `method` – Compression method stored in the source entry
    /// * `crc32` – CRC-32 of the *uncompressed* data
    /// * `uncompressed_size` – Original (uncompressed) size in bytes
    /// * `mtime_opt` – Optional modification time; current time is used when `None`
    /// * `compressed_data` – Raw compressed payload to write verbatim
    pub fn add_file_raw(
        &mut self,
        name: &str,
        method: CompressionMethod,
        crc32: u32,
        uncompressed_size: u64,
        mtime_opt: Option<std::time::SystemTime>,
        compressed_data: &[u8],
    ) -> Result<()> {
        // Progress: notify about entry start
        let file_index = self.entries.len() as u64;
        if let Some(ref handle) = self.progress {
            handle.on_entry(name, file_index);
        }

        let (mtime, mdate) = match mtime_opt {
            Some(t) => Self::dos_time_from_systime(t),
            None => Self::current_dos_time(),
        };

        let method_u16 = method.to_u16();
        // LZMA (method 14) requires bit 1 set in flags to indicate EOS marker.
        let flags: u16 = if method_u16 == 14 { FLAG_LZMA_EOS } else { 0 };

        let compressed_size = compressed_data.len() as u64;
        let local_header_offset = self.offset;

        // Check if Zip64 is needed
        let needs_zip64 = compressed_size >= ZIP64_MARKER_32 as u64
            || uncompressed_size >= ZIP64_MARKER_32 as u64
            || local_header_offset >= ZIP64_MARKER_32 as u64;

        // Version needed depends on method and Zip64 usage
        let version_needed: u16 = if needs_zip64 {
            45
        } else if method_u16 == 14 {
            63
        } else if method_u16 == 8 {
            20
        } else {
            10
        };

        let filename_bytes = name.as_bytes();

        // Build Zip64 extra field for local header if needed
        let mut local_extra = Vec::new();
        if needs_zip64 {
            local_extra.extend_from_slice(&ZIP64_EXTRA_FIELD_ID.to_le_bytes());
            local_extra.extend_from_slice(&16u16.to_le_bytes());
            local_extra.extend_from_slice(&uncompressed_size.to_le_bytes());
            local_extra.extend_from_slice(&compressed_size.to_le_bytes());
        }

        let compressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            compressed_size as u32
        };
        let uncompressed_size_32 = if needs_zip64 {
            ZIP64_MARKER_32
        } else {
            uncompressed_size as u32
        };

        // Write local file header
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        self.writer.write_all(&version_needed.to_le_bytes())?;
        self.writer.write_all(&flags.to_le_bytes())?;
        self.writer.write_all(&method_u16.to_le_bytes())?;
        self.writer.write_all(&mtime.to_le_bytes())?;
        self.writer.write_all(&mdate.to_le_bytes())?;
        self.writer.write_all(&crc32.to_le_bytes())?;
        self.writer.write_all(&compressed_size_32.to_le_bytes())?;
        self.writer.write_all(&uncompressed_size_32.to_le_bytes())?;
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        self.writer
            .write_all(&(local_extra.len() as u16).to_le_bytes())?;
        self.writer.write_all(filename_bytes)?;
        self.writer.write_all(&local_extra)?;

        // Write pre-compressed data verbatim
        self.writer.write_all(compressed_data)?;

        // Update offset (30 = fixed local header size)
        self.offset +=
            30 + filename_bytes.len() as u64 + local_extra.len() as u64 + compressed_size;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E, // Unix, version 3.0
            version_needed,
            flags,
            method: method_u16,
            mtime,
            mdate,
            crc32,
            compressed_size,
            uncompressed_size,
            filename: name.to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o100644 << 16,
            local_header_offset,
        });

        // Progress: notify about bytes written
        if let Some(ref handle) = self.progress {
            handle.on_progress(uncompressed_size, None);
        }

        Ok(())
    }

    /// Add a directory to the archive.
    pub fn add_directory(&mut self, name: &str) -> Result<()> {
        // Ensure directory name ends with /
        let dir_name = if name.ends_with('/') {
            name.to_string()
        } else {
            format!("{}/", name)
        };

        let (mtime, mdate) = Self::current_dos_time();
        let local_header_offset = self.offset;
        let filename_bytes = dir_name.as_bytes();

        // Write local file header for directory
        self.writer
            .write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes())?;
        self.writer.write_all(&10u16.to_le_bytes())?; // Version needed
        self.writer.write_all(&0u16.to_le_bytes())?; // Flags
        self.writer.write_all(&0u16.to_le_bytes())?; // Method (stored)
        self.writer.write_all(&mtime.to_le_bytes())?;
        self.writer.write_all(&mdate.to_le_bytes())?;
        self.writer.write_all(&0u32.to_le_bytes())?; // CRC-32
        self.writer.write_all(&0u32.to_le_bytes())?; // Compressed size
        self.writer.write_all(&0u32.to_le_bytes())?; // Uncompressed size
        self.writer
            .write_all(&(filename_bytes.len() as u16).to_le_bytes())?;
        self.writer.write_all(&0u16.to_le_bytes())?; // Extra field length
        self.writer.write_all(filename_bytes)?;

        self.offset += 30 + filename_bytes.len() as u64;

        // Store central directory entry
        self.entries.push(CentralDirEntry {
            version_made_by: 0x031E,
            version_needed: 10,
            flags: 0,
            method: 0,
            mtime,
            mdate,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            filename: dir_name,
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0o40755 << 16, // Directory, rwxr-xr-x
            local_header_offset,
        });

        Ok(())
    }

    /// Finish writing the archive.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        let central_dir_offset = self.offset;
        let mut central_dir_size = 0u64;

        // Write central directory
        for entry in &self.entries {
            let entry_size = entry.written_size() as u64;
            central_dir_size += entry_size;
            entry.write(&mut self.writer)?;
        }

        // Determine if Zip64 EOCD is needed
        let num_entries = self.entries.len() as u64;
        let needs_zip64 = num_entries > ZIP64_MARKER_16 as u64
            || central_dir_size >= ZIP64_MARKER_32 as u64
            || central_dir_offset >= ZIP64_MARKER_32 as u64
            || self.entries.iter().any(|e| e.needs_zip64());

        if needs_zip64 {
            let zip64_eocd_offset = central_dir_offset + central_dir_size;

            // Write Zip64 End of Central Directory Record
            // Signature
            self.writer
                .write_all(&ZIP64_END_OF_CENTRAL_DIR_SIG.to_le_bytes())?;
            // Size of Zip64 EOCD record (44 bytes following this field)
            self.writer.write_all(&44u64.to_le_bytes())?;
            // Version made by
            self.writer.write_all(&0x031Eu16.to_le_bytes())?;
            // Version needed to extract
            self.writer.write_all(&45u16.to_le_bytes())?;
            // Number of this disk
            self.writer.write_all(&0u32.to_le_bytes())?;
            // Disk where central directory starts
            self.writer.write_all(&0u32.to_le_bytes())?;
            // Number of central directory records on this disk
            self.writer.write_all(&num_entries.to_le_bytes())?;
            // Total number of central directory records
            self.writer.write_all(&num_entries.to_le_bytes())?;
            // Size of central directory
            self.writer.write_all(&central_dir_size.to_le_bytes())?;
            // Offset of start of central directory
            self.writer.write_all(&central_dir_offset.to_le_bytes())?;

            // Write Zip64 End of Central Directory Locator
            // Signature
            self.writer
                .write_all(&ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG.to_le_bytes())?;
            // Number of disk with Zip64 EOCD
            self.writer.write_all(&0u32.to_le_bytes())?;
            // Relative offset of Zip64 EOCD
            self.writer.write_all(&zip64_eocd_offset.to_le_bytes())?;
            // Total number of disks
            self.writer.write_all(&1u32.to_le_bytes())?;
        }

        // Write (regular) End of Central Directory record
        // Use marker values for Zip64
        let num_entries_16 = if num_entries > ZIP64_MARKER_16 as u64 {
            ZIP64_MARKER_16
        } else {
            num_entries as u16
        };
        let central_dir_size_32 = if central_dir_size >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            central_dir_size as u32
        };
        let central_dir_offset_32 = if central_dir_offset >= ZIP64_MARKER_32 as u64 {
            ZIP64_MARKER_32
        } else {
            central_dir_offset as u32
        };

        self.writer
            .write_all(&END_OF_CENTRAL_DIR_SIG.to_le_bytes())?;
        // Disk number
        self.writer.write_all(&0u16.to_le_bytes())?;
        // Disk with central directory
        self.writer.write_all(&0u16.to_le_bytes())?;
        // Number of entries on this disk
        self.writer.write_all(&num_entries_16.to_le_bytes())?;
        // Total number of entries
        self.writer.write_all(&num_entries_16.to_le_bytes())?;
        // Size of central directory
        self.writer.write_all(&central_dir_size_32.to_le_bytes())?;
        // Offset of central directory
        self.writer
            .write_all(&central_dir_offset_32.to_le_bytes())?;
        // Comment length
        self.writer.write_all(&0u16.to_le_bytes())?;

        self.writer.flush()?;
        self.finished = true;
        Ok(())
    }

    /// Consume the writer and return the inner writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.finish()?;
        // Use ManuallyDrop to prevent Drop from running
        let this = std::mem::ManuallyDrop::new(self);
        Ok(unsafe { std::ptr::read(&this.writer) })
    }

    /// Convert a `SystemTime` to DOS (mtime, mdate) pair.
    fn dos_time_from_systime(t: SystemTime) -> (u16, u16) {
        let secs = t
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();

        let days = secs / 86400;
        let time_of_day = secs % 86400;

        let hours = (time_of_day / 3600) as u16;
        let minutes = ((time_of_day % 3600) / 60) as u16;
        let seconds = ((time_of_day % 60) / 2) as u16;

        let mtime = (hours << 11) | (minutes << 5) | seconds;

        let years = days / 365;
        let year = (1970 + years) as u16;
        let day_of_year = days % 365;
        let month = ((day_of_year / 30) + 1) as u16;
        let day = ((day_of_year % 30) + 1) as u16;

        let mdate = if year >= 1980 {
            ((year - 1980) << 9) | (month << 5) | day
        } else {
            0
        };

        (mtime, mdate)
    }

    /// Get current time in DOS format.
    fn current_dos_time() -> (u16, u16) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO);

        // Convert to DOS time (simplified)
        let secs = now.as_secs();
        let days = secs / 86400;
        let time_of_day = secs % 86400;

        let hours = (time_of_day / 3600) as u16;
        let minutes = ((time_of_day % 3600) / 60) as u16;
        let seconds = ((time_of_day % 60) / 2) as u16; // DOS stores in 2-second increments

        let mtime = (hours << 11) | (minutes << 5) | seconds;

        // Approximate date calculation (days since 1970-01-01)
        let years = days / 365;
        let year = (1970 + years) as u16;
        let day_of_year = days % 365;
        let month = ((day_of_year / 30) + 1) as u16;
        let day = ((day_of_year % 30) + 1) as u16;

        let mdate = if year >= 1980 {
            ((year - 1980) << 9) | (month << 5) | day
        } else {
            0 // Before DOS epoch
        };

        (mtime, mdate)
    }
}

impl<W: Write> Drop for ZipWriter<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::header::reader::ZipReader;
    use std::io::Cursor;

    /// Create a ZIP with one Deflate-compressed file, extract_raw,
    /// add_file_raw into a new ZIP, then verify the compressed bytes are
    /// byte-identical through the round-trip.
    #[test]
    fn test_zip_add_file_raw_preserves_bytes() {
        // Build source archive with a compressible file
        let test_data: Vec<u8> = b"Hello raw world! "
            .iter()
            .cycle()
            .take(512)
            .copied()
            .collect();

        let mut src_bytes = Vec::new();
        {
            let mut zw = ZipWriter::new(&mut src_bytes);
            zw.add_file_with_options("hello.txt", &test_data, ZipCompressionLevel::Normal)
                .expect("add_file_with_options failed");
            zw.finish().expect("finish failed");
        }

        // Extract raw compressed payload from source
        let mut src_reader = ZipReader::new(Cursor::new(&src_bytes)).expect("ZipReader::new");
        let entries = src_reader.entries().to_vec();
        assert_eq!(entries.len(), 1);
        let entry = &entries[0];
        let raw1 = src_reader.extract_raw(entry).expect("extract_raw");

        // Build destination archive using add_file_raw
        let method = CompressionMethod::from_core(&entry.method);
        let crc32 = entry.crc32.unwrap_or(0);
        let uncompressed_size = entry.size;

        let mut dst_bytes = Vec::new();
        {
            let mut zw2 = ZipWriter::new(&mut dst_bytes);
            zw2.add_file_raw(
                &entry.name,
                method,
                crc32,
                uncompressed_size,
                entry.modified,
                &raw1,
            )
            .expect("add_file_raw failed");
            zw2.finish().expect("finish failed");
        }

        // Read back and extract_raw from destination
        let mut dst_reader = ZipReader::new(Cursor::new(&dst_bytes)).expect("ZipReader::new dst");
        let dst_entries = dst_reader.entries().to_vec();
        assert_eq!(dst_entries.len(), 1);
        let raw2 = dst_reader
            .extract_raw(&dst_entries[0])
            .expect("extract_raw dst");

        assert_eq!(
            raw1, raw2,
            "compressed bytes must be byte-identical through add_file_raw round-trip"
        );

        // Also verify decompression still works
        let decoded = dst_reader
            .extract(&dst_entries[0])
            .expect("extract after add_file_raw");
        assert_eq!(decoded, test_data, "decompressed content mismatch");
    }

    /// Verify that both Stored and Deflate entries survive an
    /// add_file_raw round-trip with byte-equal compressed payloads.
    #[test]
    fn test_zip_add_file_raw_mixed_methods() {
        let small_data = b"tiny";
        let big_data: Vec<u8> = b"AAAA".iter().cycle().take(256).copied().collect();

        // Build source archive: small_data stored, big_data deflated
        let mut src_bytes = Vec::new();
        {
            let mut zw = ZipWriter::new(&mut src_bytes);
            zw.add_file_with_options("small.bin", small_data, ZipCompressionLevel::Store)
                .expect("add small");
            zw.add_file_with_options("big.bin", &big_data, ZipCompressionLevel::Best)
                .expect("add big");
            zw.finish().expect("finish");
        }

        let mut src_reader = ZipReader::new(Cursor::new(&src_bytes)).expect("src reader");
        let src_entries = src_reader.entries().to_vec();
        assert_eq!(src_entries.len(), 2);

        // Gather raw payloads
        let raws: Vec<Vec<u8>> = src_entries
            .iter()
            .map(|e| src_reader.extract_raw(e).expect("extract_raw"))
            .collect();

        // Rebuild via add_file_raw
        let mut dst_bytes = Vec::new();
        {
            let mut zw2 = ZipWriter::new(&mut dst_bytes);
            for (entry, raw) in src_entries.iter().zip(raws.iter()) {
                let method = CompressionMethod::from_core(&entry.method);
                zw2.add_file_raw(
                    &entry.name,
                    method,
                    entry.crc32.unwrap_or(0),
                    entry.size,
                    entry.modified,
                    raw,
                )
                .expect("add_file_raw");
            }
            zw2.finish().expect("finish dst");
        }

        // Verify byte-equality of raw payloads
        let mut dst_reader = ZipReader::new(Cursor::new(&dst_bytes)).expect("dst reader");
        let dst_entries = dst_reader.entries().to_vec();
        assert_eq!(dst_entries.len(), 2);

        for (i, (src_e, dst_e)) in src_entries.iter().zip(dst_entries.iter()).enumerate() {
            let raw_src = &raws[i];
            let raw_dst = dst_reader.extract_raw(dst_e).expect("extract_raw dst");
            assert_eq!(
                *raw_src, raw_dst,
                "compressed bytes differ for entry {}",
                src_e.name
            );
            // Verify decompression correctness
            let decoded = dst_reader.extract(dst_e).expect("extract");
            let expected = src_reader.extract(src_e).expect("extract src");
            assert_eq!(
                decoded, expected,
                "decompressed content differs for {}",
                src_e.name
            );
        }
    }
}

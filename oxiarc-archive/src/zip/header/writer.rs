//! ZIP archive writer implementation.

use super::super::crypto::{ENCRYPTION_HEADER_SIZE, FLAG_ENCRYPTED, ZipCrypto};
use super::super::encryption::{
    AesExtraField, AesStrength, PASSWORD_VERIFICATION_LEN, WINZIP_AUTH_CODE_LEN, ZipAesEncryptor,
    generate_salt,
};
use super::types::{
    CentralDirEntry, END_OF_CENTRAL_DIR_SIG, LOCAL_FILE_HEADER_SIG, METHOD_AES_ENCRYPTED,
    ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG, ZIP64_END_OF_CENTRAL_DIR_SIG, ZIP64_EXTRA_FIELD_ID,
    ZIP64_MARKER_16, ZIP64_MARKER_32, ZipCompressionLevel,
};
use oxiarc_core::Crc32;
use oxiarc_core::error::Result;
use oxiarc_deflate::deflate;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// ZIP archive writer.
pub struct ZipWriter<W: Write> {
    writer: W,
    entries: Vec<CentralDirEntry>,
    offset: u64,
    compression: ZipCompressionLevel,
    finished: bool,
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
        }
    }

    /// Set the compression level for subsequent files.
    pub fn set_compression(&mut self, level: ZipCompressionLevel) {
        self.compression = level;
    }

    /// Add a file to the archive.
    pub fn add_file(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.add_file_with_options(name, data, self.compression)
    }

    /// Add a file with specific compression.
    pub fn add_file_with_options(
        &mut self,
        name: &str,
        data: &[u8],
        compression: ZipCompressionLevel,
    ) -> Result<()> {
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

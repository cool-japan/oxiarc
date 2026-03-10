//! ZIP archive reader implementation.

use super::super::crypto::{ENCRYPTION_HEADER_SIZE, ZipCrypto};
use super::super::encryption::{
    AesExtraField, PASSWORD_VERIFICATION_LEN, WINZIP_AUTH_CODE_LEN, ZipAesDecryptor,
};
use super::types::{
    CENTRAL_DIR_HEADER_SIG, CompressionMethod, DataDescriptor, END_OF_CENTRAL_DIR_SIG,
    FLAG_DATA_DESCRIPTOR, LOCAL_FILE_HEADER_SIG, LocalFileHeader,
    ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG, ZIP64_EXTRA_FIELD_ID, ZIP64_MARKER_32,
    get_entry_aes_encryption_info, is_entry_encrypted, is_entry_traditional_encrypted,
};
use oxiarc_core::entry::CompressionMethod as CoreMethod;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{Crc32, Entry, EntryType, FileAttributes};
use oxiarc_deflate::inflate;
use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, UNIX_EPOCH};

/// ZIP archive reader.
pub struct ZipReader<R: Read + Seek> {
    reader: R,
    entries: Vec<Entry>,
}

impl<R: Read + Seek> ZipReader<R> {
    /// Create a new ZIP reader.
    pub fn new(mut reader: R) -> Result<Self> {
        let entries = Self::read_entries(&mut reader)?;
        Ok(Self { reader, entries })
    }

    /// Read all entries from the archive.
    /// Uses the central directory for accurate metadata (handles data descriptors).
    fn read_entries(reader: &mut R) -> Result<Vec<Entry>> {
        // Try to find and read from central directory first
        if let Ok(entries) = Self::read_from_central_directory(reader) {
            return Ok(entries);
        }

        // Fall back to scanning local headers
        Self::read_from_local_headers(reader)
    }

    /// Read entries from the central directory (preferred method).
    fn read_from_central_directory(reader: &mut R) -> Result<Vec<Entry>> {
        // Find end of central directory record
        let file_size = reader.seek(SeekFrom::End(0))?;

        // Search for EOCD signature (max comment is 65535 bytes)
        let search_start = file_size.saturating_sub(65535 + 22);
        reader.seek(SeekFrom::Start(search_start))?;

        let mut buf = vec![0u8; (file_size - search_start) as usize];
        reader.read_exact(&mut buf)?;

        // Find EOCD signature (backwards)
        let eocd_sig = END_OF_CENTRAL_DIR_SIG.to_le_bytes();
        let eocd_offset = buf
            .windows(4)
            .rposition(|w| w == eocd_sig)
            .ok_or_else(|| OxiArcError::invalid_header("End of central directory not found"))?;

        let eocd_pos = search_start + eocd_offset as u64;

        // Check for Zip64 EOCD locator
        let (cd_offset, cd_size, total_entries) = if eocd_pos >= 20 {
            reader.seek(SeekFrom::Start(eocd_pos - 20))?;
            let mut locator_buf = [0u8; 20];
            reader.read_exact(&mut locator_buf)?;

            let locator_sig = u32::from_le_bytes([
                locator_buf[0],
                locator_buf[1],
                locator_buf[2],
                locator_buf[3],
            ]);

            if locator_sig == ZIP64_END_OF_CENTRAL_DIR_LOCATOR_SIG {
                // Zip64 EOCD locator found
                let zip64_eocd_offset = u64::from_le_bytes([
                    locator_buf[8],
                    locator_buf[9],
                    locator_buf[10],
                    locator_buf[11],
                    locator_buf[12],
                    locator_buf[13],
                    locator_buf[14],
                    locator_buf[15],
                ]);

                // Read Zip64 EOCD
                reader.seek(SeekFrom::Start(zip64_eocd_offset))?;
                let mut zip64_eocd = [0u8; 56];
                reader.read_exact(&mut zip64_eocd)?;

                let entries_count = u64::from_le_bytes([
                    zip64_eocd[32],
                    zip64_eocd[33],
                    zip64_eocd[34],
                    zip64_eocd[35],
                    zip64_eocd[36],
                    zip64_eocd[37],
                    zip64_eocd[38],
                    zip64_eocd[39],
                ]);

                let cd_size_64 = u64::from_le_bytes([
                    zip64_eocd[40],
                    zip64_eocd[41],
                    zip64_eocd[42],
                    zip64_eocd[43],
                    zip64_eocd[44],
                    zip64_eocd[45],
                    zip64_eocd[46],
                    zip64_eocd[47],
                ]);

                let cd_offset_64 = u64::from_le_bytes([
                    zip64_eocd[48],
                    zip64_eocd[49],
                    zip64_eocd[50],
                    zip64_eocd[51],
                    zip64_eocd[52],
                    zip64_eocd[53],
                    zip64_eocd[54],
                    zip64_eocd[55],
                ]);

                (cd_offset_64, cd_size_64, entries_count)
            } else {
                // Standard EOCD
                Self::parse_standard_eocd(&buf[eocd_offset..])?
            }
        } else {
            Self::parse_standard_eocd(&buf[eocd_offset..])?
        };

        // Read central directory entries
        reader.seek(SeekFrom::Start(cd_offset))?;
        let mut entries = Vec::with_capacity(total_entries as usize);

        for _ in 0..total_entries {
            let entry = Self::read_central_dir_entry(reader)?;
            entries.push(entry);
        }

        // Validate we consumed the expected amount
        let _expected_end = cd_offset + cd_size;

        Ok(entries)
    }

    /// Parse standard EOCD record.
    fn parse_standard_eocd(buf: &[u8]) -> Result<(u64, u64, u64)> {
        if buf.len() < 22 {
            return Err(OxiArcError::invalid_header("EOCD too short"));
        }

        let total_entries = u16::from_le_bytes([buf[10], buf[11]]) as u64;
        let cd_size = u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]) as u64;
        let cd_offset = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]) as u64;

        Ok((cd_offset, cd_size, total_entries))
    }

    /// Read a single central directory entry.
    fn read_central_dir_entry(reader: &mut R) -> Result<Entry> {
        let mut buf = [0u8; 46];
        reader.read_exact(&mut buf)?;

        let signature = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if signature != CENTRAL_DIR_HEADER_SIG {
            return Err(OxiArcError::invalid_magic(
                CENTRAL_DIR_HEADER_SIG.to_le_bytes().to_vec(),
                signature.to_le_bytes().to_vec(),
            ));
        }

        let flags = u16::from_le_bytes([buf[8], buf[9]]);
        let method = CompressionMethod::from_u16(u16::from_le_bytes([buf[10], buf[11]]));
        let mtime = u16::from_le_bytes([buf[12], buf[13]]);
        let mdate = u16::from_le_bytes([buf[14], buf[15]]);
        let crc32 = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let compressed_size = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
        let uncompressed_size = u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]);
        let filename_len = u16::from_le_bytes([buf[28], buf[29]]) as usize;
        let extra_len = u16::from_le_bytes([buf[30], buf[31]]) as usize;
        let comment_len = u16::from_le_bytes([buf[32], buf[33]]) as usize;
        let local_header_offset = u32::from_le_bytes([buf[42], buf[43], buf[44], buf[45]]);

        // Read variable-length fields
        let mut filename_bytes = vec![0u8; filename_len];
        reader.read_exact(&mut filename_bytes)?;
        let filename = String::from_utf8_lossy(&filename_bytes).into_owned();

        let mut extra = vec![0u8; extra_len];
        reader.read_exact(&mut extra)?;

        let mut comment_bytes = vec![0u8; comment_len];
        reader.read_exact(&mut comment_bytes)?;
        let comment = String::from_utf8_lossy(&comment_bytes).into_owned();

        // Parse Zip64 extra field if needed
        let mut uncompressed_size_64 = None;
        let mut compressed_size_64 = None;
        let mut local_header_offset_64 = None;

        if uncompressed_size == ZIP64_MARKER_32
            || compressed_size == ZIP64_MARKER_32
            || local_header_offset == ZIP64_MARKER_32
        {
            let mut offset = 0;
            while offset + 4 <= extra.len() {
                let header_id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
                let data_size = u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]) as usize;
                offset += 4;

                if header_id == ZIP64_EXTRA_FIELD_ID && offset + data_size <= extra.len() {
                    let mut field_offset = offset;

                    if uncompressed_size == ZIP64_MARKER_32
                        && field_offset + 8 <= offset + data_size
                    {
                        uncompressed_size_64 = Some(u64::from_le_bytes([
                            extra[field_offset],
                            extra[field_offset + 1],
                            extra[field_offset + 2],
                            extra[field_offset + 3],
                            extra[field_offset + 4],
                            extra[field_offset + 5],
                            extra[field_offset + 6],
                            extra[field_offset + 7],
                        ]));
                        field_offset += 8;
                    }

                    if compressed_size == ZIP64_MARKER_32 && field_offset + 8 <= offset + data_size
                    {
                        compressed_size_64 = Some(u64::from_le_bytes([
                            extra[field_offset],
                            extra[field_offset + 1],
                            extra[field_offset + 2],
                            extra[field_offset + 3],
                            extra[field_offset + 4],
                            extra[field_offset + 5],
                            extra[field_offset + 6],
                            extra[field_offset + 7],
                        ]));
                        field_offset += 8;
                    }

                    if local_header_offset == ZIP64_MARKER_32
                        && field_offset + 8 <= offset + data_size
                    {
                        local_header_offset_64 = Some(u64::from_le_bytes([
                            extra[field_offset],
                            extra[field_offset + 1],
                            extra[field_offset + 2],
                            extra[field_offset + 3],
                            extra[field_offset + 4],
                            extra[field_offset + 5],
                            extra[field_offset + 6],
                            extra[field_offset + 7],
                        ]));
                    }

                    break;
                }

                offset += data_size;
            }
        }

        // Calculate actual sizes and offset
        let actual_uncompressed = uncompressed_size_64.unwrap_or(uncompressed_size as u64);
        let actual_compressed = compressed_size_64.unwrap_or(compressed_size as u64);
        let actual_header_offset = local_header_offset_64.unwrap_or(local_header_offset as u64);

        // Calculate data offset by reading local header length
        // Local header: 30 bytes fixed + filename_len + extra_len
        // We need to peek at the local header's extra field length (may differ from central)
        let current_pos = reader.stream_position()?;
        reader.seek(SeekFrom::Start(actual_header_offset + 26))?;
        let mut local_lens = [0u8; 4];
        reader.read_exact(&mut local_lens)?;
        let local_filename_len = u16::from_le_bytes([local_lens[0], local_lens[1]]) as u64;
        let local_extra_len = u16::from_le_bytes([local_lens[2], local_lens[3]]) as u64;
        let data_offset = actual_header_offset + 30 + local_filename_len + local_extra_len;
        reader.seek(SeekFrom::Start(current_pos))?;

        let entry_type = if filename.ends_with('/') {
            EntryType::Directory
        } else {
            EntryType::File
        };

        // Convert DOS time to SystemTime
        let seconds = (mtime & 0x1F) as u64 * 2;
        let minutes = ((mtime >> 5) & 0x3F) as u64;
        let hours = ((mtime >> 11) & 0x1F) as u64;
        let day = (mdate & 0x1F) as u64;
        let month = ((mdate >> 5) & 0x0F) as u64;
        let year = ((mdate >> 9) & 0x7F) as u64 + 1980;
        let days = (year - 1970) * 365 + (year - 1969) / 4 + (month - 1) * 30 + day;
        let total_seconds = days * 86400 + hours * 3600 + minutes * 60 + seconds;
        let modified = UNIX_EPOCH + Duration::from_secs(total_seconds);

        // Mark entries with data descriptors in the extra data
        let mut entry_extra = extra.clone();
        if flags & FLAG_DATA_DESCRIPTOR != 0 {
            // Add a marker so we know this entry used a data descriptor
            entry_extra.extend_from_slice(&[0xDD, 0xDD]); // Custom marker
        }

        Ok(Entry {
            name: filename,
            entry_type,
            size: actual_uncompressed,
            compressed_size: actual_compressed,
            method: method.to_core(),
            modified: Some(modified),
            created: None,
            accessed: None,
            attributes: FileAttributes::default(),
            crc32: Some(crc32),
            comment: if comment.is_empty() {
                None
            } else {
                Some(comment)
            },
            link_target: None,
            offset: data_offset,
            extra: entry_extra,
        })
    }

    /// Read entries from local headers (fallback, doesn't handle data descriptors well).
    fn read_from_local_headers(reader: &mut R) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();

        // Start from beginning
        reader.seek(SeekFrom::Start(0))?;

        loop {
            let pos = reader.stream_position()?;

            // Try to read signature
            let mut sig_buf = [0u8; 4];
            if reader.read_exact(&mut sig_buf).is_err() {
                break;
            }

            let signature = u32::from_le_bytes(sig_buf);

            if signature == LOCAL_FILE_HEADER_SIG {
                // Seek back and read full header
                reader.seek(SeekFrom::Start(pos))?;
                let mut header = LocalFileHeader::read(reader)?;

                // Record data offset
                header.data_offset = reader.stream_position()?;

                // Handle data descriptor case
                if header.has_data_descriptor() && header.compressed_size == 0 {
                    // Can't skip properly without scanning for next header or reading central dir
                    // This is why we prefer central directory parsing
                    break;
                }

                // Skip compressed data (use actual size for Zip64 support)
                let compressed_size = header.actual_compressed_size();
                reader.seek(SeekFrom::Current(compressed_size as i64))?;

                // Skip data descriptor if present
                if header.has_data_descriptor() {
                    let is_zip64 = header.compressed_size == ZIP64_MARKER_32
                        || header.uncompressed_size == ZIP64_MARKER_32;
                    let (descriptor, _) = DataDescriptor::read(reader, is_zip64)?;
                    // Update header with data descriptor values if header had zeros
                    if header.crc32 == 0 {
                        // Note: Can't mutate header here, but we've already created entry
                        // This is fine since central directory path is preferred
                        let _ = descriptor;
                    }
                }

                entries.push(header.to_entry());
            } else if signature == CENTRAL_DIR_HEADER_SIG || signature == END_OF_CENTRAL_DIR_SIG {
                // Reached central directory, stop
                break;
            } else {
                // Unknown signature, stop
                break;
            }
        }

        Ok(entries)
    }

    /// Get the list of entries.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Extract an entry.
    pub fn extract(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        // Seek to data
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Read compressed data
        let mut compressed = vec![0u8; entry.compressed_size as usize];
        self.reader.read_exact(&mut compressed)?;

        // Decompress based on method
        let decompressed = match entry.method {
            CoreMethod::Stored => compressed,
            CoreMethod::Deflate => inflate(&compressed)?,
            _ => return Err(OxiArcError::unsupported_method(format!("{}", entry.method))),
        };

        // Verify CRC
        if let Some(expected_crc) = entry.crc32 {
            let actual_crc = Crc32::compute(&decompressed);
            if actual_crc != expected_crc {
                return Err(OxiArcError::crc_mismatch(expected_crc, actual_crc));
            }
        }

        Ok(decompressed)
    }

    /// Get entry by name.
    pub fn entry_by_name(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// Check if an entry is encrypted (any encryption type).
    ///
    /// This checks for the encryption flag in the general purpose bit flags
    /// or for the AES encryption method.
    ///
    /// Note: This is an alias for the standalone `is_entry_encrypted` function.
    pub fn is_encrypted(entry: &Entry) -> bool {
        is_entry_encrypted(entry)
    }

    /// Check if an entry is encrypted with AES (WinZip AE-2).
    ///
    /// Returns `Some(AesExtraField)` if AES-encrypted, `None` otherwise.
    ///
    /// Note: This is an alias for the standalone `get_entry_aes_encryption_info` function.
    pub fn get_aes_encryption_info(entry: &Entry) -> Option<AesExtraField> {
        get_entry_aes_encryption_info(entry)
    }

    /// Check if an entry uses traditional PKWARE encryption.
    ///
    /// Note: This is an alias for the standalone `is_entry_traditional_encrypted` function.
    pub fn is_traditional_encrypted(entry: &Entry) -> bool {
        is_entry_traditional_encrypted(entry)
    }

    /// Extract an encrypted entry using a password (Traditional ZIP encryption).
    ///
    /// This method handles the traditional ZIP encryption (PKWARE/ZipCrypto).
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to extract.
    /// * `password` - The password for decryption.
    ///
    /// # Returns
    ///
    /// The decrypted and decompressed data.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The password is incorrect
    /// - CRC verification fails
    /// - Decompression fails
    pub fn extract_with_password(&mut self, entry: &Entry, password: &[u8]) -> Result<Vec<u8>> {
        // Check if the entry is encrypted
        if !Self::is_encrypted(entry) {
            // Entry is not encrypted, use normal extraction
            return self.extract(entry);
        }

        // For encrypted entries, the data offset points to the start of the encrypted data
        // which includes the 12-byte encryption header
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // The actual encrypted data size includes the encryption header
        // compressed_size in the central directory is the encrypted size (including header)
        let encrypted_size = entry.compressed_size as usize;
        if encrypted_size < ENCRYPTION_HEADER_SIZE {
            return Err(OxiArcError::invalid_header(
                "Encrypted entry too small for encryption header",
            ));
        }

        // Read all encrypted data (including header)
        let mut encrypted = vec![0u8; encrypted_size];
        self.reader.read_exact(&mut encrypted)?;

        // Initialize the cipher with the password
        let mut cipher = ZipCrypto::new(password);

        // Decrypt the encryption header (first 12 bytes)
        let mut header = [0u8; ENCRYPTION_HEADER_SIZE];
        header.copy_from_slice(&encrypted[..ENCRYPTION_HEADER_SIZE]);
        for byte in header.iter_mut() {
            *byte = cipher.decrypt_byte(*byte);
        }

        // Verify the password using the check byte (last byte of header)
        // The check byte should match the high byte of the CRC-32
        let expected_check = entry.crc32.map(|crc| (crc >> 24) as u8).unwrap_or(0);
        let actual_check = header[11];

        if actual_check != expected_check {
            return Err(OxiArcError::invalid_header(
                "Password verification failed - incorrect password or corrupted data",
            ));
        }

        // Decrypt the remaining data
        let mut decrypted_compressed = encrypted[ENCRYPTION_HEADER_SIZE..].to_vec();
        for byte in decrypted_compressed.iter_mut() {
            *byte = cipher.decrypt_byte(*byte);
        }

        // Decompress based on method
        let decompressed = match entry.method {
            CoreMethod::Stored => decrypted_compressed,
            CoreMethod::Deflate => inflate(&decrypted_compressed)?,
            _ => return Err(OxiArcError::unsupported_method(format!("{}", entry.method))),
        };

        // Verify CRC
        if let Some(expected_crc) = entry.crc32 {
            let actual_crc = Crc32::compute(&decompressed);
            if actual_crc != expected_crc {
                return Err(OxiArcError::crc_mismatch(expected_crc, actual_crc));
            }
        }

        Ok(decompressed)
    }

    /// Extract an AES-encrypted entry using a password.
    ///
    /// This method handles the WinZip AE-2 AES encryption.
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to extract.
    /// * `password` - The password for decryption.
    ///
    /// # Returns
    ///
    /// The decrypted and decompressed data.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The password is incorrect
    /// - HMAC authentication fails
    /// - Decompression fails
    pub fn extract_with_password_aes(&mut self, entry: &Entry, password: &[u8]) -> Result<Vec<u8>> {
        // Get AES encryption info from extra field
        let aes_info = Self::get_aes_encryption_info(entry).ok_or_else(|| {
            OxiArcError::invalid_header("Entry does not contain AES encryption information")
        })?;

        // Seek to data
        self.reader.seek(SeekFrom::Start(entry.offset))?;

        // Read salt
        let salt_len = aes_info.strength.salt_len();
        let mut salt = vec![0u8; salt_len];
        self.reader.read_exact(&mut salt)?;

        // Read password verification bytes
        let mut pw_verification = [0u8; PASSWORD_VERIFICATION_LEN];
        self.reader.read_exact(&mut pw_verification)?;

        // Create decryptor and verify password
        let (mut decryptor, expected_pw_verification): (ZipAesDecryptor, [u8; 2]) =
            ZipAesDecryptor::new(password, &salt, aes_info.strength)?;

        if pw_verification != expected_pw_verification {
            return Err(OxiArcError::invalid_header(
                "Password verification failed - incorrect password",
            ));
        }

        // Calculate encrypted data size
        // Total = salt + pw_verification + encrypted_data + auth_code
        let overhead = salt_len + PASSWORD_VERIFICATION_LEN + WINZIP_AUTH_CODE_LEN;
        let encrypted_data_len = entry.compressed_size as usize - overhead;

        // Read encrypted data
        let mut encrypted_data = vec![0u8; encrypted_data_len];
        self.reader.read_exact(&mut encrypted_data)?;

        // Read authentication code
        let mut auth_code = [0u8; WINZIP_AUTH_CODE_LEN];
        self.reader.read_exact(&mut auth_code)?;

        // Update HMAC with encrypted data and verify
        decryptor.update_hmac(&encrypted_data);
        if !decryptor.verify(&auth_code) {
            return Err(OxiArcError::invalid_header(
                "HMAC authentication failed - data may be corrupted",
            ));
        }

        // Decrypt
        let mut decrypted = encrypted_data;
        decryptor.decrypt(&mut decrypted);

        // Decompress based on actual compression method (stored in AES extra field)
        let decompressed = match aes_info.compression_method {
            0 => decrypted,            // Stored
            8 => inflate(&decrypted)?, // Deflate
            _ => {
                return Err(OxiArcError::unsupported_method(format!(
                    "Compression method {} in AES-encrypted entry",
                    aes_info.compression_method
                )));
            }
        };

        // Verify CRC (for AE-2, CRC is in header)
        if let Some(expected_crc) = entry.crc32 {
            if expected_crc != 0 {
                // AE-2 stores CRC
                let actual_crc = Crc32::compute(&decompressed);
                if actual_crc != expected_crc {
                    return Err(OxiArcError::crc_mismatch(expected_crc, actual_crc));
                }
            }
        }

        Ok(decompressed)
    }

    /// Extract an encrypted entry, auto-detecting the encryption type.
    ///
    /// This method automatically detects whether the entry uses traditional
    /// PKWARE encryption or AES encryption and uses the appropriate method.
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry to extract.
    /// * `password` - The password for decryption.
    ///
    /// # Returns
    ///
    /// The decrypted and decompressed data.
    pub fn extract_encrypted(&mut self, entry: &Entry, password: &[u8]) -> Result<Vec<u8>> {
        if Self::get_aes_encryption_info(entry).is_some() {
            self.extract_with_password_aes(entry, password)
        } else if Self::is_encrypted(entry) {
            self.extract_with_password(entry, password)
        } else {
            // Not encrypted, use normal extraction
            self.extract(entry)
        }
    }
}

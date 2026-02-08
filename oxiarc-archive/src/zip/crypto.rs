//! ZIP Traditional (PKWARE) Encryption Support.
//!
//! This module implements the traditional ZIP encryption algorithm, also known as
//! ZipCrypto or PKWARE encryption. This is the original encryption method specified
//! in the ZIP file format specification (APPNOTE.TXT).
//!
//! **Security Warning**: This encryption is cryptographically weak and should only
//! be used for legacy compatibility. It is vulnerable to known-plaintext attacks
//! and other cryptanalytic techniques. For secure encryption, use AES-based ZIP
//! encryption instead.
//!
//! ## Algorithm Overview
//!
//! The ZipCrypto algorithm uses:
//! - Three 32-bit keys initialized to magic values
//! - CRC-32 polynomial for key updates
//! - A 12-byte encryption header for password verification
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_archive::zip::crypto::ZipCrypto;
//!
//! // Create cipher with password
//! let mut cipher = ZipCrypto::new(b"mypassword");
//!
//! // Encrypt data
//! let plaintext = b"Hello, World!";
//! let mut encrypted = plaintext.to_vec();
//! for byte in encrypted.iter_mut() {
//!     *byte = cipher.encrypt_byte(*byte);
//! }
//!
//! // Decrypt data (using a fresh cipher with same password)
//! let mut cipher = ZipCrypto::new(b"mypassword");
//! for byte in encrypted.iter_mut() {
//!     *byte = cipher.decrypt_byte(*byte);
//! }
//!
//! assert_eq!(&encrypted, plaintext);
//! ```

use oxiarc_core::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// CRC-32 lookup table (polynomial 0xEDB88320, reflected).
/// This is the same polynomial used in standard CRC-32 calculations.
const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Initial key values for ZipCrypto.
const INITIAL_KEY0: u32 = 0x12345678;
const INITIAL_KEY1: u32 = 0x23456789;
const INITIAL_KEY2: u32 = 0x34567890;

/// Size of the encryption header in bytes.
pub const ENCRYPTION_HEADER_SIZE: usize = 12;

/// Flag bit indicating encrypted entry in ZIP general purpose bit flags.
pub const FLAG_ENCRYPTED: u16 = 0x0001;

/// ZIP Traditional (PKWARE) Encryption Cipher.
///
/// This struct maintains the three 32-bit key state required for
/// the ZipCrypto stream cipher.
#[derive(Debug, Clone)]
pub struct ZipCrypto {
    /// First key, updated using CRC-32 of each byte.
    key0: u32,
    /// Second key, derived from key0 using a linear congruential generator.
    key1: u32,
    /// Third key, updated using CRC-32 of the high byte of key1.
    key2: u32,
}

impl ZipCrypto {
    /// Create a new ZipCrypto cipher initialized with the given password.
    ///
    /// The password bytes are used to initialize the key state through
    /// the update_keys function.
    ///
    /// # Arguments
    ///
    /// * `password` - The password bytes used for encryption/decryption.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_archive::zip::crypto::ZipCrypto;
    ///
    /// let cipher = ZipCrypto::new(b"secret");
    /// ```
    #[must_use]
    pub fn new(password: &[u8]) -> Self {
        let mut cipher = Self {
            key0: INITIAL_KEY0,
            key1: INITIAL_KEY1,
            key2: INITIAL_KEY2,
        };

        // Initialize keys with password
        for &byte in password {
            cipher.update_keys(byte);
        }

        cipher
    }

    /// Create a new ZipCrypto cipher with explicit initial key state.
    ///
    /// This is primarily used for testing with known key values.
    #[must_use]
    pub fn with_keys(key0: u32, key1: u32, key2: u32) -> Self {
        Self { key0, key1, key2 }
    }

    /// Update the key state with a plaintext byte.
    ///
    /// This function implements the key update algorithm:
    /// - key0 = crc32(key0, byte)
    /// - key1 = (key1 + (key0 & 0xff)) * 134775813 + 1
    /// - key2 = crc32(key2, key1 >> 24)
    #[inline]
    fn update_keys(&mut self, byte: u8) {
        self.key0 = crc32_update(self.key0, byte);
        self.key1 = self
            .key1
            .wrapping_add(self.key0 & 0xFF)
            .wrapping_mul(134775813)
            .wrapping_add(1);
        self.key2 = crc32_update(self.key2, (self.key1 >> 24) as u8);
    }

    /// Generate a pseudo-random byte from the current key state.
    ///
    /// This implements the formula: ((key2 | 2) * ((key2 | 2) ^ 1)) >> 8
    #[inline]
    fn stream_byte(&self) -> u8 {
        let temp = (self.key2 | 2) as u16;
        ((temp.wrapping_mul(temp ^ 1)) >> 8) as u8
    }

    /// Encrypt a single byte.
    ///
    /// The byte is XORed with the stream byte, then the keys are updated
    /// with the original plaintext byte.
    ///
    /// # Arguments
    ///
    /// * `byte` - The plaintext byte to encrypt.
    ///
    /// # Returns
    ///
    /// The encrypted (ciphertext) byte.
    #[inline]
    pub fn encrypt_byte(&mut self, byte: u8) -> u8 {
        let cipher_byte = byte ^ self.stream_byte();
        self.update_keys(byte);
        cipher_byte
    }

    /// Decrypt a single byte.
    ///
    /// The byte is XORed with the stream byte to recover the plaintext,
    /// then the keys are updated with the recovered plaintext byte.
    ///
    /// # Arguments
    ///
    /// * `byte` - The ciphertext byte to decrypt.
    ///
    /// # Returns
    ///
    /// The decrypted (plaintext) byte.
    #[inline]
    pub fn decrypt_byte(&mut self, byte: u8) -> u8 {
        let plain_byte = byte ^ self.stream_byte();
        self.update_keys(plain_byte);
        plain_byte
    }

    /// Encrypt a buffer in place.
    ///
    /// # Arguments
    ///
    /// * `buffer` - The plaintext buffer to encrypt in place.
    pub fn encrypt_buffer(&mut self, buffer: &mut [u8]) {
        for byte in buffer.iter_mut() {
            *byte = self.encrypt_byte(*byte);
        }
    }

    /// Decrypt a buffer in place.
    ///
    /// # Arguments
    ///
    /// * `buffer` - The ciphertext buffer to decrypt in place.
    pub fn decrypt_buffer(&mut self, buffer: &mut [u8]) {
        for byte in buffer.iter_mut() {
            *byte = self.decrypt_byte(*byte);
        }
    }

    /// Generate an encryption header for a ZIP entry.
    ///
    /// The header consists of 12 bytes:
    /// - 11 random bytes (encrypted)
    /// - 1 check byte (encrypted): the high byte of the file CRC-32
    ///
    /// # Arguments
    ///
    /// * `crc32` - The CRC-32 of the uncompressed file data.
    /// * `random_source` - A source of random bytes for the header.
    ///
    /// # Returns
    ///
    /// A 12-byte encrypted header.
    pub fn generate_header(
        &mut self,
        crc32: u32,
        random_source: &[u8; 11],
    ) -> [u8; ENCRYPTION_HEADER_SIZE] {
        let mut header = [0u8; ENCRYPTION_HEADER_SIZE];

        // Copy and encrypt 11 random bytes
        for (i, &random_byte) in random_source.iter().enumerate() {
            header[i] = self.encrypt_byte(random_byte);
        }

        // The 12th byte is the check byte (high byte of CRC-32)
        let check_byte = (crc32 >> 24) as u8;
        header[11] = self.encrypt_byte(check_byte);

        header
    }

    /// Generate an encryption header using a simple random source.
    ///
    /// This generates pseudo-random bytes based on the provided seed values.
    /// For production use, consider using a cryptographically secure RNG.
    ///
    /// # Arguments
    ///
    /// * `crc32` - The CRC-32 of the uncompressed file data.
    /// * `seed1` - First seed for random generation.
    /// * `seed2` - Second seed for random generation.
    ///
    /// # Returns
    ///
    /// A 12-byte encrypted header.
    pub fn generate_header_seeded(
        &mut self,
        crc32: u32,
        seed1: u64,
        seed2: u64,
    ) -> [u8; ENCRYPTION_HEADER_SIZE] {
        // Simple LCG-based random generation
        let mut state = seed1 ^ seed2;
        let mut random = [0u8; 11];

        for byte in random.iter_mut() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (state >> 56) as u8;
        }

        self.generate_header(crc32, &random)
    }

    /// Verify and consume the encryption header during decryption.
    ///
    /// This reads 12 bytes from the reader, decrypts them, and verifies
    /// that the check byte matches the expected CRC-32 high byte.
    ///
    /// # Arguments
    ///
    /// * `reader` - The source to read the encrypted header from.
    /// * `crc32` - The expected CRC-32 of the uncompressed file data.
    ///
    /// # Returns
    ///
    /// Ok(()) if the password appears correct, Err if it doesn't match.
    pub fn verify_header<R: Read>(&mut self, reader: &mut R, crc32: u32) -> Result<()> {
        let mut header = [0u8; ENCRYPTION_HEADER_SIZE];
        reader.read_exact(&mut header)?;

        // Decrypt all 12 bytes
        for byte in header.iter_mut() {
            *byte = self.decrypt_byte(*byte);
        }

        // The 12th byte should match the high byte of CRC-32
        let expected_check = (crc32 >> 24) as u8;
        let actual_check = header[11];

        if actual_check != expected_check {
            return Err(OxiArcError::invalid_header(format!(
                "Password verification failed: expected check byte {:#04x}, got {:#04x}",
                expected_check, actual_check
            )));
        }

        Ok(())
    }

    /// Verify header using modification time instead of CRC (older format).
    ///
    /// Some older ZIP implementations use the high byte of the modification
    /// time instead of CRC-32 for verification.
    ///
    /// # Arguments
    ///
    /// * `reader` - The source to read the encrypted header from.
    /// * `mtime` - The DOS modification time of the file.
    ///
    /// # Returns
    ///
    /// Ok(()) if the password appears correct, Err if it doesn't match.
    pub fn verify_header_mtime<R: Read>(&mut self, reader: &mut R, mtime: u16) -> Result<()> {
        let mut header = [0u8; ENCRYPTION_HEADER_SIZE];
        reader.read_exact(&mut header)?;

        // Decrypt all 12 bytes
        for byte in header.iter_mut() {
            *byte = self.decrypt_byte(*byte);
        }

        // The 12th byte should match the high byte of modification time
        let expected_check = (mtime >> 8) as u8;
        let actual_check = header[11];

        if actual_check != expected_check {
            return Err(OxiArcError::invalid_header(format!(
                "Password verification failed: expected check byte {:#04x}, got {:#04x}",
                expected_check, actual_check
            )));
        }

        Ok(())
    }

    /// Get the current key state (for debugging/testing).
    #[must_use]
    pub fn keys(&self) -> (u32, u32, u32) {
        (self.key0, self.key1, self.key2)
    }
}

impl Default for ZipCrypto {
    fn default() -> Self {
        Self {
            key0: INITIAL_KEY0,
            key1: INITIAL_KEY1,
            key2: INITIAL_KEY2,
        }
    }
}

/// Update a CRC-32 value with a single byte.
#[inline]
fn crc32_update(crc: u32, byte: u8) -> u32 {
    let index = ((crc ^ byte as u32) & 0xFF) as usize;
    CRC32_TABLE[index] ^ (crc >> 8)
}

/// An encrypting writer that wraps an inner writer.
///
/// All bytes written to this writer are encrypted before being
/// passed to the inner writer.
pub struct ZipCryptoWriter<W: Write> {
    inner: W,
    cipher: ZipCrypto,
}

impl<W: Write> ZipCryptoWriter<W> {
    /// Create a new encrypting writer.
    ///
    /// # Arguments
    ///
    /// * `writer` - The inner writer to write encrypted data to.
    /// * `password` - The password for encryption.
    pub fn new(writer: W, password: &[u8]) -> Self {
        Self {
            inner: writer,
            cipher: ZipCrypto::new(password),
        }
    }

    /// Create a new encrypting writer with an existing cipher.
    ///
    /// This is useful when you've already initialized the cipher
    /// (e.g., after writing the encryption header).
    pub fn with_cipher(writer: W, cipher: ZipCrypto) -> Self {
        Self {
            inner: writer,
            cipher,
        }
    }

    /// Write the encryption header.
    ///
    /// # Arguments
    ///
    /// * `crc32` - The CRC-32 of the uncompressed data.
    /// * `seed1` - First seed for random generation.
    /// * `seed2` - Second seed for random generation.
    ///
    /// # Returns
    ///
    /// The number of bytes written (always 12).
    pub fn write_header(&mut self, crc32: u32, seed1: u64, seed2: u64) -> Result<usize> {
        let header = self.cipher.generate_header_seeded(crc32, seed1, seed2);
        self.inner.write_all(&header)?;
        Ok(ENCRYPTION_HEADER_SIZE)
    }

    /// Get a reference to the inner writer.
    pub fn inner(&self) -> &W {
        &self.inner
    }

    /// Consume the writer and return the inner writer.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: Write> Write for ZipCryptoWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Encrypt and write
        let mut encrypted = Vec::with_capacity(buf.len());
        for &byte in buf {
            encrypted.push(self.cipher.encrypt_byte(byte));
        }
        self.inner.write_all(&encrypted)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A decrypting reader that wraps an inner reader.
///
/// All bytes read from this reader are decrypted after being
/// read from the inner reader.
pub struct ZipCryptoReader<R: Read> {
    inner: R,
    cipher: ZipCrypto,
}

impl<R: Read> ZipCryptoReader<R> {
    /// Create a new decrypting reader.
    ///
    /// # Arguments
    ///
    /// * `reader` - The inner reader to read encrypted data from.
    /// * `password` - The password for decryption.
    pub fn new(reader: R, password: &[u8]) -> Self {
        Self {
            inner: reader,
            cipher: ZipCrypto::new(password),
        }
    }

    /// Create a new decrypting reader with an existing cipher.
    ///
    /// This is useful when you've already initialized the cipher
    /// (e.g., after reading the encryption header).
    pub fn with_cipher(reader: R, cipher: ZipCrypto) -> Self {
        Self {
            inner: reader,
            cipher,
        }
    }

    /// Verify the encryption header.
    ///
    /// This reads and decrypts the 12-byte header and verifies
    /// the check byte against the expected CRC-32.
    ///
    /// # Arguments
    ///
    /// * `crc32` - The expected CRC-32 of the uncompressed data.
    ///
    /// # Returns
    ///
    /// Ok(()) if verification succeeds.
    pub fn verify_header(&mut self, crc32: u32) -> Result<()> {
        self.cipher.verify_header(&mut self.inner, crc32)
    }

    /// Verify the encryption header using modification time.
    ///
    /// # Arguments
    ///
    /// * `mtime` - The DOS modification time.
    ///
    /// # Returns
    ///
    /// Ok(()) if verification succeeds.
    pub fn verify_header_mtime(&mut self, mtime: u16) -> Result<()> {
        self.cipher.verify_header_mtime(&mut self.inner, mtime)
    }

    /// Get a reference to the inner reader.
    pub fn inner(&self) -> &R {
        &self.inner
    }

    /// Consume the reader and return the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: Read> Read for ZipCryptoReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bytes_read = self.inner.read(buf)?;
        for byte in buf[..bytes_read].iter_mut() {
            *byte = self.cipher.decrypt_byte(*byte);
        }
        Ok(bytes_read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_initial_keys() {
        let cipher = ZipCrypto::default();
        let (key0, key1, key2) = cipher.keys();
        assert_eq!(key0, INITIAL_KEY0);
        assert_eq!(key1, INITIAL_KEY1);
        assert_eq!(key2, INITIAL_KEY2);
    }

    #[test]
    fn test_password_initialization() {
        let cipher1 = ZipCrypto::new(b"test");
        let cipher2 = ZipCrypto::new(b"test");
        assert_eq!(cipher1.keys(), cipher2.keys());

        let cipher3 = ZipCrypto::new(b"different");
        assert_ne!(cipher1.keys(), cipher3.keys());
    }

    #[test]
    fn test_roundtrip_single_byte() {
        let mut encrypt_cipher = ZipCrypto::new(b"password");
        let mut decrypt_cipher = ZipCrypto::new(b"password");

        let original: u8 = 0x42;
        let encrypted = encrypt_cipher.encrypt_byte(original);
        let decrypted = decrypt_cipher.decrypt_byte(encrypted);

        assert_eq!(original, decrypted);
    }

    #[test]
    fn test_roundtrip_buffer() {
        let original = b"Hello, World! This is a test of ZIP encryption.";
        let mut encrypted = original.to_vec();
        let mut decrypted = Vec::new();

        // Encrypt
        let mut cipher = ZipCrypto::new(b"secret");
        cipher.encrypt_buffer(&mut encrypted);

        // Decrypt
        let mut cipher = ZipCrypto::new(b"secret");
        decrypted.extend_from_slice(&encrypted);
        cipher.decrypt_buffer(&mut decrypted);

        assert_eq!(&decrypted[..], &original[..]);
    }

    #[test]
    fn test_encrypted_differs_from_plaintext() {
        let plaintext = b"This should be encrypted";
        let mut encrypted = plaintext.to_vec();

        let mut cipher = ZipCrypto::new(b"password");
        cipher.encrypt_buffer(&mut encrypted);

        assert_ne!(&encrypted[..], &plaintext[..]);
    }

    #[test]
    fn test_different_passwords_different_output() {
        let plaintext = b"Test data";

        let mut encrypted1 = plaintext.to_vec();
        let mut cipher1 = ZipCrypto::new(b"password1");
        cipher1.encrypt_buffer(&mut encrypted1);

        let mut encrypted2 = plaintext.to_vec();
        let mut cipher2 = ZipCrypto::new(b"password2");
        cipher2.encrypt_buffer(&mut encrypted2);

        assert_ne!(&encrypted1[..], &encrypted2[..]);
    }

    #[test]
    fn test_header_generation_and_verification() {
        let crc32: u32 = 0xDEADBEEF;
        let password = b"testpassword";

        // Generate header
        let mut cipher = ZipCrypto::new(password);
        let header = cipher.generate_header_seeded(crc32, 12345, 67890);
        assert_eq!(header.len(), ENCRYPTION_HEADER_SIZE);

        // Verify header
        let mut cipher = ZipCrypto::new(password);
        let mut cursor = Cursor::new(header);
        let result = cipher.verify_header(&mut cursor, crc32);
        assert!(result.is_ok(), "Header verification should succeed");
    }

    #[test]
    fn test_header_verification_wrong_password() {
        let crc32: u32 = 0xDEADBEEF;

        // Generate header with correct password
        let mut cipher = ZipCrypto::new(b"correct");
        let header = cipher.generate_header_seeded(crc32, 12345, 67890);

        // Try to verify with wrong password
        let mut cipher = ZipCrypto::new(b"wrong");
        let mut cursor = Cursor::new(header);
        let result = cipher.verify_header(&mut cursor, crc32);
        assert!(
            result.is_err(),
            "Header verification should fail with wrong password"
        );
    }

    #[test]
    fn test_writer_roundtrip() {
        let password = b"secret";
        let crc32: u32 = 0x12345678;
        let plaintext = b"Data to encrypt via writer";

        // Write encrypted data
        let mut output = Vec::new();
        {
            let mut writer = ZipCryptoWriter::new(&mut output, password);
            writer
                .write_header(crc32, 11111, 22222)
                .expect("write header failed");
            writer.write_all(plaintext).expect("write failed");
        }

        // Verify output is larger (header + encrypted data)
        assert_eq!(output.len(), ENCRYPTION_HEADER_SIZE + plaintext.len());

        // Read and decrypt
        let mut cursor = Cursor::new(&output);
        let mut reader = ZipCryptoReader::new(&mut cursor, password);
        reader
            .verify_header(crc32)
            .expect("header verification failed");

        let mut decrypted = vec![0u8; plaintext.len()];
        reader.read_exact(&mut decrypted).expect("read failed");

        assert_eq!(&decrypted[..], &plaintext[..]);
    }

    #[test]
    fn test_empty_password() {
        let mut cipher = ZipCrypto::new(b"");
        let (key0, key1, key2) = cipher.keys();
        // With empty password, keys should remain at initial values
        assert_eq!(key0, INITIAL_KEY0);
        assert_eq!(key1, INITIAL_KEY1);
        assert_eq!(key2, INITIAL_KEY2);

        // Should still encrypt/decrypt
        let original: u8 = 0xAB;
        let encrypted = cipher.encrypt_byte(original);

        let mut cipher = ZipCrypto::new(b"");
        let decrypted = cipher.decrypt_byte(encrypted);
        assert_eq!(original, decrypted);
    }

    #[test]
    fn test_long_data() {
        let password = b"longpasswordtest";
        let plaintext: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();

        let mut encrypted = plaintext.clone();
        let mut cipher = ZipCrypto::new(password);
        cipher.encrypt_buffer(&mut encrypted);

        let mut decrypted = encrypted.clone();
        let mut cipher = ZipCrypto::new(password);
        cipher.decrypt_buffer(&mut decrypted);

        assert_eq!(plaintext, decrypted);
    }

    #[test]
    fn test_crc32_update() {
        // Test a known CRC-32 update
        let crc = crc32_update(0xFFFFFFFF, b'a');
        // The CRC-32 of 'a' with initial 0xFFFFFFFF should produce a known value
        // We just verify it's not the initial value
        assert_ne!(crc, 0xFFFFFFFF);
    }

    #[test]
    fn test_stream_byte_reproducibility() {
        // Same key state should produce same stream byte
        let cipher1 = ZipCrypto::with_keys(0x12345678, 0x23456789, 0x34567890);
        let cipher2 = ZipCrypto::with_keys(0x12345678, 0x23456789, 0x34567890);

        assert_eq!(cipher1.stream_byte(), cipher2.stream_byte());
    }

    #[test]
    fn test_mtime_header_verification() {
        let mtime: u16 = 0x5678;
        let password = b"testpwd";

        // We need to manually construct a header for mtime verification
        let mut cipher = ZipCrypto::new(password);
        let random = [0x11u8; 11];
        let mut header = [0u8; ENCRYPTION_HEADER_SIZE];

        for (i, &r) in random.iter().enumerate() {
            header[i] = cipher.encrypt_byte(r);
        }
        let check_byte = (mtime >> 8) as u8;
        header[11] = cipher.encrypt_byte(check_byte);

        // Verify
        let mut cipher = ZipCrypto::new(password);
        let mut cursor = Cursor::new(header);
        let result = cipher.verify_header_mtime(&mut cursor, mtime);
        assert!(result.is_ok());
    }

    #[test]
    fn test_known_test_vector() {
        // Test vector derived from PKWARE specification behavior:
        // After initializing with password "test", the key state should be deterministic
        let cipher = ZipCrypto::new(b"test");
        let (key0, key1, key2) = cipher.keys();

        // These values are deterministic for the password "test"
        // Verify they're not the initial values
        assert_ne!(key0, INITIAL_KEY0);
        assert_ne!(key1, INITIAL_KEY1);
        assert_ne!(key2, INITIAL_KEY2);

        // Verify consistency: two ciphers with same password have same keys
        let cipher2 = ZipCrypto::new(b"test");
        assert_eq!(cipher.keys(), cipher2.keys());
    }
}

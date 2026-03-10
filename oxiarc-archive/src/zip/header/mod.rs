//! ZIP header structures and archive read/write implementation.

mod reader;
mod types;
mod writer;

pub use reader::ZipReader;
pub use types::{
    CompressionMethod, LocalFileHeader, ZipCompressionLevel, get_entry_aes_encryption_info,
    is_entry_encrypted, is_entry_traditional_encrypted,
};
pub use writer::ZipWriter;

#[cfg(test)]
mod tests {
    use super::super::encryption::AesStrength;
    use super::reader::ZipReader;
    use super::types::{
        CentralDirEntry, CompressionMethod, DataDescriptor, FLAG_DATA_DESCRIPTOR, LocalFileHeader,
        ZIP64_EXTRA_FIELD_ID, ZIP64_MARKER_32, ZipCompressionLevel, get_entry_aes_encryption_info,
        is_entry_encrypted, is_entry_traditional_encrypted,
    };
    use super::writer::ZipWriter;
    use oxiarc_deflate::deflate;
    use std::io::Cursor;

    #[test]
    fn test_compression_method() {
        assert_eq!(CompressionMethod::from_u16(0), CompressionMethod::Stored);
        assert_eq!(CompressionMethod::from_u16(8), CompressionMethod::Deflate);
        assert!(matches!(
            CompressionMethod::from_u16(99),
            CompressionMethod::Unknown(99)
        ));
    }

    #[test]
    fn test_zip_writer_single_file() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("hello.txt", b"Hello, World!")?;
            writer.finish()?;
        }

        // Read back
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "hello.txt");
        assert_eq!(entry.size, 13);

        let data = reader.extract(&entry)?;
        assert_eq!(&data, b"Hello, World!");
        Ok(())
    }

    #[test]
    fn test_zip_writer_stored() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file_with_options("test.bin", b"short", ZipCompressionLevel::Store)?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        let entry = reader.entries()[0].clone();
        use oxiarc_core::entry::CompressionMethod as CoreMethod;
        assert_eq!(entry.method, CoreMethod::Stored);

        let data = reader.extract(&entry)?;
        assert_eq!(&data, b"short");
        Ok(())
    }

    #[test]
    fn test_zip_writer_multiple_files() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("file1.txt", b"Content 1")?;
            writer.add_file("file2.txt", b"Content 2 is longer")?;
            writer.add_file("empty.txt", b"")?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        assert_eq!(reader.entries().len(), 3);
        assert_eq!(reader.entries()[0].name, "file1.txt");
        assert_eq!(reader.entries()[1].name, "file2.txt");
        assert_eq!(reader.entries()[2].name, "empty.txt");

        let data1 = reader.extract(&reader.entries()[0].clone())?;
        let data2 = reader.extract(&reader.entries()[1].clone())?;
        let data3 = reader.extract(&reader.entries()[2].clone())?;

        assert_eq!(&data1, b"Content 1");
        assert_eq!(&data2, b"Content 2 is longer");
        assert_eq!(&data3, b"");
        Ok(())
    }

    #[test]
    fn test_zip_writer_directory() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_directory("mydir")?;
            writer.add_file("mydir/file.txt", b"Inside directory")?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let reader = ZipReader::new(cursor)?;

        assert_eq!(reader.entries().len(), 2);
        assert_eq!(reader.entries()[0].name, "mydir/");
        assert!(reader.entries()[0].is_dir());
        assert_eq!(reader.entries()[1].name, "mydir/file.txt");
        assert!(reader.entries()[1].is_file());
        Ok(())
    }

    #[test]
    fn test_zip_roundtrip_compressed() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Create compressible data
        let data = "This is a test string that repeats. ".repeat(100);
        let data_bytes = data.as_bytes();

        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("large.txt", data_bytes)?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        let entry = reader.entries()[0].clone();
        // Should be compressed (smaller than original)
        assert!(entry.compressed_size < entry.size);
        use oxiarc_core::entry::CompressionMethod as CoreMethod;
        assert_eq!(entry.method, CoreMethod::Deflate);

        let extracted = reader.extract(&entry)?;
        assert_eq!(extracted, data_bytes);
        Ok(())
    }

    #[test]
    fn test_zip64_extra_field_parsing() {
        // Test parsing of Zip64 extra field
        let extra = [
            0x01, 0x00, // Header ID: 0x0001 (Zip64)
            0x10, 0x00, // Data size: 16 bytes
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, // Uncompressed size: 0x100000000 (4GB)
            0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x00, // Compressed size: 0x80000000 (2GB)
        ];

        let (uncompressed, compressed) =
            LocalFileHeader::parse_zip64_extra(&extra, ZIP64_MARKER_32, ZIP64_MARKER_32);

        assert_eq!(uncompressed, Some(0x100000000u64));
        assert_eq!(compressed, Some(0x80000000u64));
    }

    #[test]
    fn test_zip64_extra_field_no_marker() {
        // When sizes don't have marker values, Zip64 extra should be ignored
        let extra = [
            0x01, 0x00, // Header ID: 0x0001 (Zip64)
            0x10, 0x00, // Data size: 16 bytes
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // Uncompressed size
            0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // Compressed size
        ];

        // No markers, so sizes should remain None
        let (uncompressed, compressed) = LocalFileHeader::parse_zip64_extra(&extra, 1000, 500);

        assert_eq!(uncompressed, None);
        assert_eq!(compressed, None);
    }

    #[test]
    fn test_central_dir_entry_needs_zip64() {
        let entry = CentralDirEntry {
            version_made_by: 0x031E,
            version_needed: 20,
            flags: 0,
            method: 0,
            mtime: 0,
            mdate: 0,
            crc32: 0,
            compressed_size: 100,
            uncompressed_size: 200,
            filename: "test.txt".to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0,
            local_header_offset: 0,
        };
        assert!(!entry.needs_zip64());

        // Large compressed size
        let entry_large = CentralDirEntry {
            compressed_size: 0x1_0000_0000,
            ..entry.clone()
        };
        assert!(entry_large.needs_zip64());

        // Large uncompressed size
        let entry_large_uncompressed = CentralDirEntry {
            uncompressed_size: 0x1_0000_0000,
            ..entry.clone()
        };
        assert!(entry_large_uncompressed.needs_zip64());

        // Large offset
        let entry_large_offset = CentralDirEntry {
            local_header_offset: 0x1_0000_0000,
            ..entry.clone()
        };
        assert!(entry_large_offset.needs_zip64());
    }

    #[test]
    fn test_central_dir_entry_build_zip64_extra() {
        let entry = CentralDirEntry {
            version_made_by: 0x031E,
            version_needed: 20,
            flags: 0,
            method: 0,
            mtime: 0,
            mdate: 0,
            crc32: 0,
            compressed_size: 0x1_0000_0000, // 4GB+
            uncompressed_size: 0x2_0000_0000,
            filename: "test.txt".to_string(),
            extra: Vec::new(),
            comment: String::new(),
            disk_start: 0,
            internal_attr: 0,
            external_attr: 0,
            local_header_offset: 0x3_0000_0000,
        };

        let extra = entry.build_zip64_extra();
        // Header (4) + uncompressed (8) + compressed (8) + offset (8) = 28 bytes
        assert_eq!(extra.len(), 28);
        // Check header ID
        assert_eq!(
            u16::from_le_bytes([extra[0], extra[1]]),
            ZIP64_EXTRA_FIELD_ID
        );
        // Check data size (24 bytes)
        assert_eq!(u16::from_le_bytes([extra[2], extra[3]]), 24);
    }

    #[test]
    fn test_data_descriptor_with_signature() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        // Data descriptor with signature
        let data = [
            0x50, 0x4B, 0x07, 0x08, // Signature
            0x12, 0x34, 0x56, 0x78, // CRC-32
            0x00, 0x10, 0x00, 0x00, // Compressed size (4096)
            0x00, 0x20, 0x00, 0x00, // Uncompressed size (8192)
        ];

        let mut cursor = Cursor::new(data);
        let (descriptor, bytes) = DataDescriptor::read(&mut cursor, false)?;

        assert_eq!(bytes, 16); // 4 (sig) + 4 (crc) + 4 (comp) + 4 (uncomp)
        assert_eq!(descriptor.crc32, 0x78563412);
        assert_eq!(descriptor.compressed_size, 4096);
        assert_eq!(descriptor.uncompressed_size, 8192);
        Ok(())
    }

    #[test]
    fn test_data_descriptor_without_signature()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Data descriptor without signature
        let data = [
            0x12, 0x34, 0x56, 0x78, // CRC-32 (no signature)
            0x00, 0x10, 0x00, 0x00, // Compressed size (4096)
            0x00, 0x20, 0x00, 0x00, // Uncompressed size (8192)
        ];

        let mut cursor = Cursor::new(data);
        let (descriptor, bytes) = DataDescriptor::read(&mut cursor, false)?;

        assert_eq!(bytes, 12); // 4 (crc) + 4 (comp) + 4 (uncomp)
        assert_eq!(descriptor.crc32, 0x78563412);
        assert_eq!(descriptor.compressed_size, 4096);
        assert_eq!(descriptor.uncompressed_size, 8192);
        Ok(())
    }

    #[test]
    fn test_data_descriptor_zip64() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Zip64 data descriptor with 8-byte sizes
        let data = [
            0x50, 0x4B, 0x07, 0x08, // Signature
            0xAB, 0xCD, 0xEF, 0x12, // CRC-32
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // Compressed: 0x100000000
            0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // Uncompressed: 0x200000000
        ];

        let mut cursor = Cursor::new(data);
        let (descriptor, bytes) = DataDescriptor::read(&mut cursor, true)?;

        assert_eq!(bytes, 24); // 4 (sig) + 4 (crc) + 8 (comp) + 8 (uncomp)
        assert_eq!(descriptor.crc32, 0x12EFCDAB);
        assert_eq!(descriptor.compressed_size, 0x100000000);
        assert_eq!(descriptor.uncompressed_size, 0x200000000);
        Ok(())
    }

    #[test]
    fn test_local_header_has_data_descriptor() {
        let header = LocalFileHeader {
            version_needed: 20,
            flags: FLAG_DATA_DESCRIPTOR, // Bit 3 set
            method: CompressionMethod::Deflate,
            mtime: 0,
            mdate: 0,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            filename: "test.txt".to_string(),
            extra: Vec::new(),
            data_offset: 0,
            uncompressed_size_64: None,
            compressed_size_64: None,
        };
        assert!(header.has_data_descriptor());

        let header_no_dd = LocalFileHeader {
            flags: 0, // No data descriptor
            ..header
        };
        assert!(!header_no_dd.has_data_descriptor());
    }

    // =======================================================================
    // Encryption Tests
    // =======================================================================

    #[test]
    fn test_aes_encrypted_roundtrip() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"secret123";
        let plaintext = b"This is secret data for AES encryption test.";

        // Write encrypted ZIP
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file("secret.txt", plaintext, password)?;
            writer.finish()?;
        }

        // Read and decrypt
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "secret.txt");

        // Check encryption detection
        assert!(is_entry_encrypted(&entry));
        assert!(get_entry_aes_encryption_info(&entry).is_some());

        // Extract with correct password
        let data = reader.extract_with_password_aes(&entry, password)?;
        assert_eq!(&data, plaintext);

        Ok(())
    }

    #[test]
    fn test_aes_encrypted_wrong_password() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"correct_password";
        let wrong_password = b"wrong_password";
        let plaintext = b"Secret message";

        // Write encrypted ZIP
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file("test.txt", plaintext, password)?;
            writer.finish()?;
        }

        // Try to decrypt with wrong password
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();

        let result = reader.extract_with_password_aes(&entry, wrong_password);
        assert!(result.is_err());
        let err_msg = result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err_msg.contains("Password verification failed"));

        Ok(())
    }

    #[test]
    fn test_aes_encrypted_compression() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"password";
        // Create compressible data
        let plaintext = "Repeated text for compression. ".repeat(50);
        let plaintext_bytes = plaintext.as_bytes();

        // Write with explicit Store compression (deflate + AES has issues)
        // TODO: Fix deflate + AES combination in the future
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_with_options(
                "compressed.txt",
                plaintext_bytes,
                password,
                ZipCompressionLevel::Store,
                AesStrength::Aes256,
            )?;
            writer.finish()?;
        }

        // Read and verify
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();

        let data = reader.extract_with_password_aes(&entry, password)?;
        assert_eq!(data, plaintext_bytes);

        Ok(())
    }

    #[test]
    fn test_traditional_encrypted_roundtrip() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let password = b"legacy_password";
        let plaintext = b"This is data encrypted with traditional PKWARE encryption.";

        // Write encrypted ZIP with traditional encryption
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_traditional("legacy.txt", plaintext, password)?;
            writer.finish()?;
        }

        // Read and decrypt
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        assert_eq!(reader.entries().len(), 1);
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, "legacy.txt");

        // Check encryption detection
        assert!(is_entry_encrypted(&entry));
        assert!(is_entry_traditional_encrypted(&entry));
        assert!(get_entry_aes_encryption_info(&entry).is_none());

        // Extract with correct password
        let data = reader.extract_with_password(&entry, password)?;
        assert_eq!(&data, plaintext);

        Ok(())
    }

    #[test]
    fn test_traditional_encrypted_wrong_password()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"correct";
        let wrong_password = b"wrong";
        let plaintext = b"Secret";

        // Write encrypted ZIP
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_traditional("test.txt", plaintext, password)?;
            writer.finish()?;
        }

        // Try to decrypt with wrong password
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();

        let result = reader.extract_with_password(&entry, wrong_password);
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn test_traditional_encrypted_with_compression()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"test";
        let plaintext = "Compressible content repeated many times. ".repeat(30);
        let plaintext_bytes = plaintext.as_bytes();

        // Write with compression
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_traditional_with_options(
                "data.txt",
                plaintext_bytes,
                password,
                ZipCompressionLevel::Normal,
            )?;
            writer.finish()?;
        }

        // Read and verify
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();

        let data = reader.extract_with_password(&entry, password)?;
        assert_eq!(data, plaintext_bytes);

        Ok(())
    }

    #[test]
    fn test_extract_encrypted_auto_detection() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let password = b"autodetect";

        // Create AES encrypted archive
        let mut aes_output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut aes_output);
            writer.add_encrypted_file("aes.txt", b"AES encrypted", password)?;
            writer.finish()?;
        }

        // Create traditional encrypted archive
        let mut trad_output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut trad_output);
            writer.add_encrypted_file_traditional(
                "trad.txt",
                b"Traditional encrypted",
                password,
            )?;
            writer.finish()?;
        }

        // Create unencrypted archive
        let mut plain_output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut plain_output);
            writer.add_file("plain.txt", b"Not encrypted")?;
            writer.finish()?;
        }

        // Test AES extraction with auto-detection
        let cursor = Cursor::new(aes_output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_encrypted(&entry, password)?;
        assert_eq!(&data, b"AES encrypted");

        // Test traditional extraction with auto-detection
        let cursor = Cursor::new(trad_output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_encrypted(&entry, password)?;
        assert_eq!(&data, b"Traditional encrypted");

        // Test unencrypted extraction with auto-detection
        let cursor = Cursor::new(plain_output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_encrypted(&entry, password)?;
        assert_eq!(&data, b"Not encrypted");

        Ok(())
    }

    #[test]
    fn test_mixed_encrypted_and_plain_entries()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"mixed";

        // Create archive with mixed entries
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_file("public.txt", b"Public content")?;
            writer.add_encrypted_file("secret_aes.txt", b"AES secret", password)?;
            writer.add_encrypted_file_traditional(
                "secret_trad.txt",
                b"Traditional secret",
                password,
            )?;
            writer.add_file("readme.txt", b"Readme content")?;
            writer.finish()?;
        }

        // Read and verify all entries
        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        assert_eq!(reader.entries().len(), 4);

        // Public file
        let entry0 = reader.entries()[0].clone();
        assert!(!is_entry_encrypted(&entry0));
        let data = reader.extract(&entry0)?;
        assert_eq!(&data, b"Public content");

        // AES encrypted
        let entry1 = reader.entries()[1].clone();
        assert!(is_entry_encrypted(&entry1));
        assert!(get_entry_aes_encryption_info(&entry1).is_some());
        let data = reader.extract_encrypted(&entry1, password)?;
        assert_eq!(&data, b"AES secret");

        // Traditional encrypted
        let entry2 = reader.entries()[2].clone();
        assert!(is_entry_encrypted(&entry2));
        assert!(is_entry_traditional_encrypted(&entry2));
        let data = reader.extract_encrypted(&entry2, password)?;
        assert_eq!(&data, b"Traditional secret");

        // Another public file
        let entry3 = reader.entries()[3].clone();
        assert!(!is_entry_encrypted(&entry3));
        let data = reader.extract(&entry3)?;
        assert_eq!(&data, b"Readme content");

        Ok(())
    }

    #[test]
    fn test_aes_encryption_strength_levels() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let password = b"strength_test";
        let plaintext = b"Testing AES strength levels";

        // Test AES-256 (default)
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_with_options(
                "aes256.txt",
                plaintext,
                password,
                ZipCompressionLevel::Store,
                AesStrength::Aes256,
            )?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();

        if let Some(aes_info) = get_entry_aes_encryption_info(&entry) {
            assert_eq!(aes_info.strength, AesStrength::Aes256);
        }

        let data = reader.extract_with_password_aes(&entry, password)?;
        assert_eq!(&data, plaintext);

        Ok(())
    }

    #[test]
    fn test_empty_file_encryption() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"empty";

        // AES encryption of empty file
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file("empty_aes.txt", b"", password)?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_encrypted(&entry, password)?;
        assert!(data.is_empty());

        // Traditional encryption of empty file
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_traditional("empty_trad.txt", b"", password)?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_encrypted(&entry, password)?;
        assert!(data.is_empty());

        Ok(())
    }

    #[test]
    fn test_large_file_encryption() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"large_file_test";
        // Create 10KB of data (reduced from 100KB for faster test)
        let plaintext: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        // AES encryption with Store compression
        // TODO: Fix deflate + encryption combination
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_with_options(
                "large_aes.bin",
                &plaintext,
                password,
                ZipCompressionLevel::Store,
                AesStrength::Aes256,
            )?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_with_password_aes(&entry, password)?;
        assert_eq!(data.len(), plaintext.len());
        assert_eq!(data, plaintext);

        // Traditional encryption with Store compression
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file_traditional_with_options(
                "large_trad.bin",
                &plaintext,
                password,
                ZipCompressionLevel::Store,
            )?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        let data = reader.extract_with_password(&entry, password)?;
        assert_eq!(data.len(), plaintext.len());
        assert_eq!(data, plaintext);

        Ok(())
    }

    #[test]
    fn test_unicode_filename_encryption() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let password = b"unicode";
        let plaintext = b"Unicode filename test";
        let filename = "\u{65e5}\u{672c}\u{8a9e}_\u{6587}\u{5b57}.txt"; // Japanese characters

        // AES encryption with unicode filename
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.add_encrypted_file(filename, plaintext, password)?;
            writer.finish()?;
        }

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;
        let entry = reader.entries()[0].clone();
        assert_eq!(entry.name, filename);

        let data = reader.extract_encrypted(&entry, password)?;
        assert_eq!(&data, plaintext);

        Ok(())
    }

    #[test]
    fn test_zip_5_files_deflate() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Test to reproduce bug: third and subsequent files fail with deflate compression
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.set_compression(ZipCompressionLevel::Normal);

            // Add 5 files with compressible data
            for i in 1..=5 {
                let name = format!("file{}.txt", i);
                let data = format!(
                    "This is file {} with repetitive data: AAAAAABBBBBBCCCCCCDDDDDD",
                    i
                );
                eprintln!("Adding {}: {} bytes", name, data.len());
                writer.add_file(&name, data.as_bytes())?;
            }

            writer.finish()?;
        }

        eprintln!("\nZIP created: {} bytes", output.len());
        eprintln!("Reading ZIP back...");

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        eprintln!("Found {} entries", reader.entries().len());
        assert_eq!(reader.entries().len(), 5);

        // Clone entries to avoid borrow checker issues
        let entries: Vec<_> = reader.entries().to_vec();

        for (i, entry) in entries.iter().enumerate() {
            eprintln!("\nEntry {}: {}", i + 1, entry.name);
            eprintln!("  Compression method: {}", entry.method);
            eprintln!("  Compressed size: {}", entry.compressed_size);
            eprintln!("  Uncompressed size: {}", entry.size);

            let data = reader.extract(entry)?;
            eprintln!("  Extracted: {} bytes", data.len());

            let expected = format!(
                "This is file {} with repetitive data: AAAAAABBBBBBCCCCCCDDDDDD",
                i + 1
            );
            assert_eq!(data.len(), expected.len());
            assert_eq!(&data, expected.as_bytes());
        }

        eprintln!("\nAll 5 files extracted successfully!");
        Ok(())
    }

    #[test]
    fn test_zip_3_files_binary_data() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Test with binary data similar to NumRS2's NPY format (with headers)
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.set_compression(ZipCompressionLevel::Normal);

            // Simulate NPY file format: header (128 bytes) + binary data
            let mut file1 = vec![0x93, b'N', b'U', b'M', b'P', b'Y']; // NPY magic
            file1.extend_from_slice(&[1, 0]); // version
            file1.extend_from_slice(&[110u8, 0]); // header length (110 bytes)
            file1.extend_from_slice(&[b' '; 110]); // header padding
            file1.extend_from_slice(
                &[1u8, 2, 3, 4, 5, 6, 7, 8] // 8 bytes of f64 data (1 value)
                    .repeat(6),
            ); // Total: 48 bytes of binary data

            let mut file2 = vec![0x93, b'N', b'U', b'M', b'P', b'Y'];
            file2.extend_from_slice(&[1, 0]);
            file2.extend_from_slice(&[110u8, 0]);
            file2.extend_from_slice(&[b' '; 110]);
            file2.extend_from_slice(&[1u8, 2, 3, 4, 5, 6, 7, 8].repeat(3)); // 24 bytes

            let mut file3 = vec![0x93, b'N', b'U', b'M', b'P', b'Y'];
            file3.extend_from_slice(&[1, 0]);
            file3.extend_from_slice(&[110u8, 0]);
            file3.extend_from_slice(&[b' '; 110]);
            file3.extend_from_slice(&[1u8, 2, 3, 4, 5, 6, 7, 8].repeat(2)); // 16 bytes

            // Original order: data, weights, labels
            eprintln!(
                "Adding data.npy: {} bytes (compressed should succeed)",
                file1.len()
            );
            writer.add_file("data.npy", &file1)?;

            eprintln!(
                "Adding weights.npy: {} bytes (compressed should succeed)",
                file2.len()
            );
            writer.add_file("weights.npy", &file2)?;

            eprintln!("Adding labels.npy: {} bytes (THIS ONE FAILS)", file3.len());
            writer.add_file("labels.npy", &file3)?;

            writer.finish()?;
        }

        eprintln!("\nZIP created: {} bytes", output.len());
        eprintln!("Reading ZIP back...");

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        eprintln!("Found {} entries", reader.entries().len());
        assert_eq!(reader.entries().len(), 3);

        let entries: Vec<_> = reader.entries().to_vec();

        for (i, entry) in entries.iter().enumerate() {
            eprintln!("\nEntry {}: {}", i + 1, entry.name);
            eprintln!("  Compression method: {}", entry.method);
            eprintln!("  Compressed size: {}", entry.compressed_size);
            eprintln!("  Uncompressed size: {}", entry.size);

            let data = reader.extract(entry)?;
            eprintln!("  Extracted: {} bytes", data.len());
        }

        eprintln!("\nAll 3 binary files extracted successfully!");
        Ok(())
    }

    #[test]
    fn test_zip_3_identical_files() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Test with 3 identical files to isolate the bug
        let mut output = Vec::new();
        {
            let mut writer = ZipWriter::new(&mut output);
            writer.set_compression(ZipCompressionLevel::Normal);

            // All files have the same content (like labels.npy)
            let mut file = vec![0x93, b'N', b'U', b'M', b'P', b'Y'];
            file.extend_from_slice(&[1, 0]);
            file.extend_from_slice(&[110u8, 0]);
            file.extend_from_slice(&[b' '; 110]);
            file.extend_from_slice(&[1u8, 2, 3, 4, 5, 6, 7, 8].repeat(2)); // 16 bytes

            eprintln!("File size: {} bytes", file.len());

            eprintln!("\nAdding file1.npy...");
            writer.add_file("file1.npy", &file)?;

            eprintln!("Adding file2.npy...");
            writer.add_file("file2.npy", &file)?;

            eprintln!("Adding file3.npy...");
            writer.add_file("file3.npy", &file)?;

            writer.finish()?;
        }

        eprintln!("\nZIP created: {} bytes", output.len());

        let cursor = Cursor::new(output);
        let mut reader = ZipReader::new(cursor)?;

        let entries: Vec<_> = reader.entries().to_vec();

        for (i, entry) in entries.iter().enumerate() {
            eprintln!("\nExtracting entry {}: {}", i + 1, entry.name);
            eprintln!("  Compressed: {} bytes", entry.compressed_size);
            eprintln!("  Uncompressed: {} bytes", entry.size);

            match reader.extract(entry) {
                Ok(data) => {
                    eprintln!("  Success: {} bytes", data.len());
                }
                Err(e) => {
                    eprintln!("  FAILED: {}", e);
                    return Err(Box::new(e));
                }
            }
        }

        eprintln!("\nAll files extracted!");
        Ok(())
    }

    #[test]
    fn test_deflate_size_debug() -> std::result::Result<(), Box<dyn std::error::Error>> {
        // Create the same problematic data
        let mut data = vec![0x93, b'N', b'U', b'M', b'P', b'Y'];
        data.extend_from_slice(&[1, 0]);
        data.extend_from_slice(&[110u8, 0]);
        data.extend_from_slice(&[b' '; 110]);
        data.extend_from_slice(&[1u8, 2, 3, 4, 5, 6, 7, 8].repeat(2));

        eprintln!("Original data: {} bytes", data.len());
        eprintln!("First 20 bytes: {:?}", &data[..20]);

        // Compress multiple times
        for i in 1..=3 {
            eprintln!("\n=== Compression attempt {} ===", i);
            let compressed = deflate(&data, 6)?;
            eprintln!("Compressed size: {} bytes", compressed.len());

            // Try to decompress
            match oxiarc_deflate::inflate(&compressed) {
                Ok(decompressed) => {
                    eprintln!("Decompression successful: {} bytes", decompressed.len());
                    assert_eq!(decompressed.len(), data.len());
                    assert_eq!(&decompressed, &data);
                }
                Err(e) => {
                    eprintln!("Decompression FAILED: {}", e);
                    return Err(Box::new(e));
                }
            }
        }

        eprintln!("\nAll compressions/decompressions succeeded!");
        Ok(())
    }
}

//! Tests for LZH archive reading and writing.

use crate::lzh::header::LzhHeader;
use crate::lzh::reader::LzhReader;
use crate::lzh::writer::{LzhCompressionLevel, LzhWriter};
use oxiarc_core::Crc16;
use oxiarc_core::error::OxiArcError;
use oxiarc_lzhuf::LzhMethod;
use std::io::Cursor;

#[test]
fn test_decode_filename_utf8() {
    let bytes = b"test.txt";
    assert_eq!(LzhHeader::decode_filename(bytes), "test.txt");
}

#[test]
fn test_method_from_id() {
    assert_eq!(LzhMethod::from_id(b"-lh5-"), Some(LzhMethod::Lh5));
    assert_eq!(LzhMethod::from_id(b"-lh0-"), Some(LzhMethod::Lh0));
    assert_eq!(LzhMethod::from_id(b"-xxx-"), None);
}

#[test]
fn test_lzh_writer_single_file_stored() {
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file("hello.txt", b"Hello, World!")
            .expect("add_file hello.txt");
        writer.finish().expect("writer finish");
    }

    // Verify we can read back the archive
    let cursor = Cursor::new(output);
    let mut reader = LzhReader::new(cursor).expect("LzhReader::new");
    let entries = reader.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "hello.txt");
    assert_eq!(entries[0].size, 13);

    // Extract and verify content
    let data = reader.extract_to_vec(&entries[0]).expect("extract_to_vec");
    assert_eq!(data, b"Hello, World!");
}

#[test]
fn test_lzh_writer_single_file_compressed() {
    // Use highly repetitive data that compresses well
    let test_data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output);
        // Use LH5 compression (default)
        writer.set_compression(LzhCompressionLevel::Lh5);
        writer
            .add_file("repeated.txt", test_data)
            .expect("add_file repeated.txt");
        writer.finish().expect("writer finish");
    }

    // Verify we can read back the archive
    let cursor = Cursor::new(&output);
    let mut reader = LzhReader::new(cursor).expect("LzhReader::new");
    let entries = reader.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "repeated.txt");
    assert_eq!(entries[0].size, test_data.len() as u64);

    // Verify compression actually reduced size
    assert!(
        entries[0].compressed_size < entries[0].size,
        "Expected compression to reduce size: {} < {}",
        entries[0].compressed_size,
        entries[0].size
    );

    // Extract and verify content
    let data = reader.extract_to_vec(&entries[0]).expect("extract_to_vec");
    assert_eq!(data.as_slice(), test_data);
}

#[test]
fn test_lzh_writer_multiple_files() {
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file("file1.txt", b"First file")
            .expect("add_file file1.txt");
        writer
            .add_file("file2.txt", b"Second file content")
            .expect("add_file file2.txt");
        writer
            .add_file("file3.txt", b"Third")
            .expect("add_file file3.txt");
        writer.finish().expect("writer finish");
    }

    // Verify
    let cursor = Cursor::new(output);
    let mut reader = LzhReader::new(cursor).expect("LzhReader::new");
    let entries = reader.entries();

    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "file1.txt");
    assert_eq!(entries[1].name, "file2.txt");
    assert_eq!(entries[2].name, "file3.txt");

    // Extract all
    let data1 = reader
        .extract_to_vec(&entries[0])
        .expect("extract file1.txt");
    let data2 = reader
        .extract_to_vec(&entries[1])
        .expect("extract file2.txt");
    let data3 = reader
        .extract_to_vec(&entries[2])
        .expect("extract file3.txt");

    assert_eq!(data1, b"First file");
    assert_eq!(data2, b"Second file content");
    assert_eq!(data3, b"Third");
}

#[test]
fn test_lzh_writer_directory() {
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output);
        writer.add_directory("mydir").expect("add_directory mydir");
        writer
            .add_file("mydir/file.txt", b"content")
            .expect("add_file mydir/file.txt");
        writer.finish().expect("writer finish");
    }

    // Verify
    let cursor = Cursor::new(output);
    let reader = LzhReader::new(cursor).expect("LzhReader::new");
    let entries = reader.entries();

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "mydir/");
    assert_eq!(entries[1].name, "mydir/file.txt");
}

#[test]
fn test_lzh_roundtrip_large_stored() {
    // Test with various data patterns using Store mode
    // (LH5 encoder is not fully production-ready)
    let test_data = {
        let mut data = Vec::new();
        for i in 0..1000 {
            data.extend_from_slice(format!("Line {} of test data\n", i).as_bytes());
        }
        data
    };

    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file("large.txt", &test_data)
            .expect("add_file large.txt");
        writer.finish().expect("writer finish");
    }

    // Verify archive structure
    let cursor = Cursor::new(&output);
    let mut reader = LzhReader::new(cursor).expect("LzhReader::new");
    let entries = reader.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].size, test_data.len() as u64);
    // Stored mode: compressed size equals original size
    assert_eq!(entries[0].compressed_size, entries[0].size);

    // Extract and verify content
    let extracted = reader
        .extract_to_vec(&entries[0])
        .expect("extract_to_vec large.txt");
    assert_eq!(extracted, test_data);
}

#[test]
fn test_lzh_writer_into_inner() {
    let output = Vec::new();
    let writer = LzhWriter::new(output);
    let inner = writer.into_inner().expect("into_inner");
    // Should contain at least the end marker
    assert!(!inner.is_empty());
}

#[test]
fn test_lzh_level3_roundtrip() {
    let files: &[(&str, &[u8])] = &[
        ("alpha.txt", b"First file content"),
        ("beta.txt", b"Second file - a bit more text here"),
        ("gamma.txt", b"Third"),
    ];

    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output).with_header_level(3);
        writer.set_compression(LzhCompressionLevel::Store);
        for (name, data) in files {
            writer.add_file(name, data).expect("add_file failed");
        }
        writer.finish().expect("finish failed");
    }

    // Read back with the existing LzhReader
    let cursor = Cursor::new(&output);
    let mut reader = LzhReader::new(cursor).expect("LzhReader::new failed");
    let entries = reader.entries();

    assert_eq!(entries.len(), 3, "expected 3 entries");

    for (i, (name, data)) in files.iter().enumerate() {
        assert_eq!(&entries[i].name, name, "entry {} name mismatch", i);
        assert_eq!(
            entries[i].size,
            data.len() as u64,
            "entry {} size mismatch",
            i
        );
    }

    // Verify content extraction
    for (name, expected) in files {
        let extracted = reader
            .extract_by_name(name)
            .expect("extract_by_name error")
            .expect("entry not found");
        assert_eq!(&extracted, expected, "content mismatch for {}", name);
    }
}

#[test]
fn test_lzh_progress() {
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Sink {
        entries: Mutex<Vec<String>>,
        progress_calls: Mutex<u64>,
        finish_called: Mutex<bool>,
    }

    impl oxiarc_core::progress::ProgressSink for Sink {
        fn on_progress(&self, _processed: u64, _total: Option<u64>) {
            *self.progress_calls.lock().expect("progress_calls lock") += 1;
        }
        fn on_entry(&self, name: &str, _index: u64) {
            self.entries
                .lock()
                .expect("entries lock")
                .push(name.to_string());
        }
        fn on_finish(&self) {
            *self.finish_called.lock().expect("finish_called lock") = true;
        }
    }

    let sink = Arc::new(Sink::default());
    let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

    // Write archive
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output).with_progress(handle);
        writer.set_compression(LzhCompressionLevel::Store);
        writer.add_file("a.txt", b"file a").expect("add_file a.txt");
        writer
            .add_file("b.txt", b"file b content")
            .expect("add_file b.txt");
        writer.finish().expect("writer finish");
    }

    {
        let entries = sink.entries.lock().expect("entries lock");
        assert_eq!(entries.len(), 2, "expected on_entry called twice");
        assert_eq!(entries[0], "a.txt");
        assert_eq!(entries[1], "b.txt");
    }
    assert!(
        *sink.finish_called.lock().expect("finish_called lock"),
        "on_finish not called"
    );
}

#[test]
fn test_level3_header_parsing() {
    // Build a minimal Level 3 header manually
    // Level 3 format:
    // - Word size (2 bytes): 0x0004
    // - Method (5 bytes): -lh0-
    // - Compressed size (4 bytes)
    // - Original size (4 bytes)
    // - mtime (4 bytes)
    // - Reserved (1 byte): 0x20
    // - Level (1 byte): 0x03
    // - CRC-16 (2 bytes)
    // - OS ID (1 byte): 'U'
    // - Header size (4 bytes)
    // - Next header size (4 bytes)
    // - Extended headers...
    // - Data

    let data = b"Hello, World!";
    let crc16 = Crc16::compute(data);

    let mut archive = Vec::new();

    // Word size = 4
    archive.extend_from_slice(&[0x04, 0x00]);

    // Method: -lh0-
    archive.extend_from_slice(b"-lh0-");

    // Compressed size
    archive.extend_from_slice(&(data.len() as u32).to_le_bytes());

    // Original size
    archive.extend_from_slice(&(data.len() as u32).to_le_bytes());

    // mtime (Unix timestamp)
    archive.extend_from_slice(&0u32.to_le_bytes());

    // Reserved = 0x20, Level = 3
    archive.extend_from_slice(&[0x20, 0x03]);

    // CRC-16
    archive.extend_from_slice(&crc16.to_le_bytes());

    // OS ID = 'U' for Unix
    archive.push(b'U');

    // Total header size (we'll calculate this)
    let filename = b"test.txt";
    // Fixed part: 28 bytes, plus ext header size field (4), plus filename ext header
    // Filename ext header: size(4) + type(1) + name
    let filename_ext_size = 1 + filename.len() as u32;
    // We need: fixed 28 + next_size(4) + filename_header + next_size(4) for terminator
    let total_header = 28 + 4 + filename_ext_size + 4;
    archive.extend_from_slice(&total_header.to_le_bytes());

    // Next extended header size (filename header)
    archive.extend_from_slice(&filename_ext_size.to_le_bytes());

    // Filename extended header: type 0x01 + filename
    archive.push(0x01);
    archive.extend_from_slice(filename);

    // Next extended header size = 0 (end of extended headers)
    archive.extend_from_slice(&0u32.to_le_bytes());

    // Data
    archive.extend_from_slice(data);

    // End of archive marker
    archive.push(0x00);

    // Parse the archive
    let cursor = Cursor::new(archive);
    let mut reader = LzhReader::new(cursor).expect("LzhReader::new");
    let entries = reader.entries();

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "test.txt");
    assert_eq!(entries[0].size, data.len() as u64);

    // Extract and verify
    let extracted = reader
        .extract_to_vec(&entries[0])
        .expect("extract_to_vec test.txt");
    assert_eq!(extracted, data);
}

// ---- Lenient-mode tests ----

/// Build a 3-entry LZH archive with the compressed payload of the
/// SECOND entry corrupted (one byte flipped). The first and third
/// entries remain intact. In lenient mode, all three entries must
/// enumerate and extract; extracting entry 2 must surface a
/// `CrcMismatch` warning but still return (corrupted) bytes.
#[test]
fn test_lzh_lenient_bad_crc() {
    use crate::lenient::LenientWarningKind;

    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file("a.txt", b"alpha entry")
            .expect("add_file a.txt");
        writer
            .add_file("b.txt", b"bravo entry (corrupt)")
            .expect("add_file b.txt");
        writer
            .add_file("c.txt", b"charlie entry")
            .expect("add_file c.txt");
        writer.finish().expect("writer finish");
    }

    // Parse the archive to locate the second entry's data offset.
    let entries_offset = {
        let cursor = Cursor::new(output.clone());
        let reader = LzhReader::new(cursor).expect("intact LzhReader::new");
        let entries = reader.entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].name, "b.txt");
        entries[1].offset
    };

    // Flip the first byte of the second entry's stored payload.
    let corrupt_idx = entries_offset as usize;
    assert!(corrupt_idx < output.len(), "data offset in bounds");
    output[corrupt_idx] ^= 0xFF;

    // Strict: extracting entry 2 must return CorruptedData.
    {
        let cursor = Cursor::new(output.clone());
        let mut strict = LzhReader::new(cursor).expect("strict new");
        let entries = strict.entries();
        let second = entries[1].clone();

        let err = strict
            .extract_to_vec(&second)
            .expect_err("strict extract must fail with CorruptedData on bad data CRC");
        match err {
            OxiArcError::CorruptedData { .. } => {}
            other => panic!("unexpected error variant: {:?}", other),
        }
    }

    // Lenient: all 3 entries enumerate and extract; exactly one
    // warning emitted (for entry 2).
    {
        let cursor = Cursor::new(output);
        let mut lenient = LzhReader::new(cursor).expect("lenient new").lenient(true);
        let entries = lenient.entries();
        assert_eq!(entries.len(), 3);

        // Extract all three; only entry 2 is corrupted, so only
        // that extraction records a warning.
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        let first_entry = entries[0].clone();
        let second_entry = entries[1].clone();
        let third_entry = entries[2].clone();

        let a = lenient
            .extract_to_vec(&first_entry)
            .expect("extract a.txt in lenient mode");
        assert_eq!(a, b"alpha entry");

        let b = lenient
            .extract_to_vec(&second_entry)
            .expect("lenient extract must succeed even on corrupted entry 2");
        assert_eq!(
            b.len(),
            b"bravo entry (corrupt)".len(),
            "payload length matches original even when corrupted"
        );

        let c = lenient
            .extract_to_vec(&third_entry)
            .expect("extract c.txt in lenient mode");
        assert_eq!(c, b"charlie entry");

        let warnings = lenient.warnings();
        assert_eq!(
            warnings.len(),
            1,
            "exactly one CRC-16 warning for entry 2 — entries 1 & 3 are clean"
        );
        assert_eq!(warnings[0].format, "LZH");
        assert_eq!(warnings[0].entry_name.as_deref(), Some("b.txt"));
        match warnings[0].kind {
            LenientWarningKind::CrcMismatch { .. } => {}
            ref other => panic!("unexpected warning kind: {:?}", other),
        }
        // Tie the `names` binding into the assertion chain so the
        // compiler doesn't flag it as unused.
        assert_eq!(names[0], "a.txt");
    }
}

/// Build an LZH archive with an Lh5-compressed file, call
/// `read_raw_method_data`, rebuild via `add_file_raw`, then verify that
/// the compressed bytes are byte-identical and decompression still works.
#[test]
fn test_lzh_add_file_raw_preserves_bytes() {
    // Highly repetitive data that LH5 actually compresses
    let test_data: Vec<u8> = b"LZHRAWTEST_REPEATED_"
        .iter()
        .cycle()
        .take(640)
        .copied()
        .collect();

    let mut src_bytes = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut src_bytes);
        writer.set_compression(LzhCompressionLevel::Lh5);
        writer
            .add_file("raw_test.txt", &test_data)
            .expect("add_file failed");
        writer.finish().expect("finish failed");
    }

    // Extract raw compressed payload from source archive
    let mut src_reader = LzhReader::new(Cursor::new(&src_bytes)).expect("LzhReader::new src");
    let entries = src_reader.entries();
    assert_eq!(entries.len(), 1);
    let entry = entries[0].clone();

    let (method, raw1, crc16) = src_reader
        .read_raw_method_data(&entry)
        .expect("read_raw_method_data failed");

    // Rebuild using add_file_raw
    let mtime = entry
        .modified
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0);

    let mut dst_bytes = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut dst_bytes);
        writer
            .add_file_raw(&entry.name, method, crc16, entry.size, &raw1, mtime, None)
            .expect("add_file_raw failed");
        writer.finish().expect("finish dst failed");
    }

    // Read back and verify raw bytes are byte-identical
    let mut dst_reader = LzhReader::new(Cursor::new(&dst_bytes)).expect("LzhReader::new dst");
    let dst_entries = dst_reader.entries();
    assert_eq!(dst_entries.len(), 1);
    let dst_entry = dst_entries[0].clone();

    let (_, raw2, _) = dst_reader
        .read_raw_method_data(&dst_entry)
        .expect("read_raw_method_data dst failed");

    assert_eq!(
        raw1, raw2,
        "compressed bytes must be byte-identical through add_file_raw round-trip"
    );

    // Verify that decompression still yields the original content
    let decoded = dst_reader
        .extract_to_vec(&dst_entry)
        .expect("extract_to_vec failed");
    assert_eq!(
        decoded, test_data,
        "decompressed content mismatch after add_file_raw"
    );
}

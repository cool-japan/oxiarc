//! Round-trip tests for LZH extension headers 0x40 / 0x41 / 0x42 / 0x43 /
//! 0x44 / 0x46 / 0x50.
//!
//! Every test writes a level-3 archive via [`LzhWriter::add_file_with_metadata`]
//! and then reads it back via [`LzhHeader::read`], asserting that each
//! populated metadata field is faithfully preserved.

#![cfg(test)]

use super::LzhExtensionMetadata;
use super::{LzhCompressionLevel, LzhHeader, LzhWriter};
use std::io::Cursor;

/// Helper: write a single file with metadata at level 3, then parse
/// back the first header of the resulting archive.
fn roundtrip_single(name: &str, data: &[u8], meta: &LzhExtensionMetadata) -> LzhHeader {
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output).with_header_level(3);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file_with_metadata(name, data, meta)
            .expect("add_file_with_metadata");
        writer.finish().expect("finish");
    }

    // Parse the first header back directly from the bytes.
    let mut cursor = Cursor::new(&output);
    LzhHeader::read(&mut cursor, 0)
        .expect("LzhHeader::read error")
        .expect("LzhHeader::read returned None")
}

#[test]
fn test_lzh_level3_extension_0x40_dos_attr() {
    let meta = LzhExtensionMetadata {
        dos_attr: Some(0x20),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.filename, "file.txt");
    assert_eq!(header.dos_attr, Some(0x20));
}

#[test]
fn test_lzh_level3_extension_0x41_unix_ids() {
    let meta = LzhExtensionMetadata {
        unix_uid: Some(1000),
        unix_gid: Some(100),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.unix_uid, Some(1000), "uid roundtrip");
    assert_eq!(header.unix_gid, Some(100), "gid roundtrip");
}

#[test]
fn test_lzh_level3_extension_0x42_0x43_names() {
    let meta = LzhExtensionMetadata {
        unix_group_name: Some("users".into()),
        unix_user_name: Some("testuser".into()),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.unix_group_name.as_deref(), Some("users"));
    assert_eq!(header.unix_user_name.as_deref(), Some("testuser"));
}

#[test]
fn test_lzh_level3_extension_0x44_mtime() {
    // 2024-01-01 00:00:00 UTC
    let meta = LzhExtensionMetadata {
        unix_mtime: Some(1_704_067_200),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.unix_mtime, Some(1_704_067_200));

    // And the Entry's modified timestamp should reflect the
    // extension-provided mtime rather than the fixed-header mtime.
    use std::time::{Duration, UNIX_EPOCH};
    let entry = header.to_entry();
    let expected_modified = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
    assert_eq!(entry.modified, Some(expected_modified));
}

#[test]
fn test_lzh_level3_extension_0x46_comment() {
    // Use a mix of ASCII + multi-byte UTF-8 to stress the
    // String::from_utf8_lossy path. "こんにちは" = 15 bytes in UTF-8.
    let comment = "hello こんにちは world";
    let meta = LzhExtensionMetadata {
        comment: Some(comment.into()),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.comment.as_deref(), Some(comment));
}

#[test]
fn test_lzh_level3_extension_0x50_permission() {
    let meta = LzhExtensionMetadata {
        unix_permission: Some(0o755),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.unix_permission, Some(0o755));

    // Entry's FileAttributes.unix_mode should also carry the bits.
    let entry = header.to_entry();
    assert_eq!(entry.attributes.unix_mode, Some(0o755));
}

#[test]
fn test_lzh_level3_all_extensions_together() {
    let comment = "round-trip all ext headers";
    let meta = LzhExtensionMetadata {
        dos_attr: Some(0x20),
        unix_uid: Some(1234),
        unix_gid: Some(56),
        unix_group_name: Some("staff".into()),
        unix_user_name: Some("alice".into()),
        unix_mtime: Some(1_704_067_200),
        comment: Some(comment.into()),
        unix_permission: Some(0o644),
    };
    let header = roundtrip_single("archive.log", b"payload", &meta);

    assert_eq!(header.filename, "archive.log", "filename roundtrip");
    assert_eq!(header.dos_attr, Some(0x20), "0x40 dos_attr roundtrip");
    assert_eq!(header.unix_uid, Some(1234), "0x41 uid roundtrip");
    assert_eq!(header.unix_gid, Some(56), "0x41 gid roundtrip");
    assert_eq!(
        header.unix_group_name.as_deref(),
        Some("staff"),
        "0x42 group_name roundtrip"
    );
    assert_eq!(
        header.unix_user_name.as_deref(),
        Some("alice"),
        "0x43 user_name roundtrip"
    );
    assert_eq!(
        header.unix_mtime,
        Some(1_704_067_200),
        "0x44 mtime roundtrip"
    );
    assert_eq!(
        header.comment.as_deref(),
        Some(comment),
        "0x46 comment roundtrip"
    );
    assert_eq!(
        header.unix_permission,
        Some(0o644),
        "0x50 permission roundtrip"
    );
}

#[test]
fn test_lzh_level3_no_metadata_emits_no_extensions() {
    // Sanity check: when no metadata is provided (legacy add_file
    // path), none of the optional extension fields are populated on
    // the parsed header.
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output).with_header_level(3);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file("plain.txt", b"no extensions")
            .expect("add_file");
        writer.finish().expect("finish");
    }
    let mut cursor = Cursor::new(&output);
    let header = LzhHeader::read(&mut cursor, 0)
        .expect("LzhHeader::read error")
        .expect("LzhHeader::read returned None");

    assert_eq!(header.filename, "plain.txt");
    assert_eq!(header.dos_attr, None);
    assert_eq!(header.unix_uid, None);
    assert_eq!(header.unix_gid, None);
    assert_eq!(header.unix_group_name, None);
    assert_eq!(header.unix_user_name, None);
    assert_eq!(header.unix_mtime, None);
    assert_eq!(header.comment, None);
    assert_eq!(header.unix_permission, None);
}

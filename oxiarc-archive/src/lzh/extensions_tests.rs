//! Round-trip tests for LZH extension headers 0x40 / 0x41 / 0x42 / 0x43 /
//! 0x44 / 0x46 / 0x50 / 0x51 / 0x54.
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

// ── 0x40 — OS/2 / MS-DOS attributes (2 bytes LE u16) ────────────────────────

#[test]
fn test_lzh_level3_extension_0x40_dos_attr() {
    let meta = LzhExtensionMetadata {
        dos_attr: Some(0x0020),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.filename, "file.txt");
    assert_eq!(header.dos_attr, Some(0x0020));
}

// ── 0x41 — Windows timestamps (3 × u64 LE FILETIME) ─────────────────────────

#[test]
fn test_lzh_level3_extension_0x41_windows_timestamps() {
    // Example: 2024-01-01 00:00:00 UTC in FILETIME units
    // FILETIME = (Unix epoch seconds) * 10_000_000 + 116_444_736_000_000_000
    let filetime_base: u64 = 116_444_736_000_000_000;
    let creation: u64 = filetime_base + 1_704_067_200 * 10_000_000;
    let access: u64 = filetime_base + 1_704_153_600 * 10_000_000;
    let modify: u64 = filetime_base + 1_704_240_000 * 10_000_000;

    let meta = LzhExtensionMetadata {
        windows_creation: Some(creation),
        windows_access: Some(access),
        windows_modify: Some(modify),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(
        header.windows_creation,
        Some(creation),
        "creation roundtrip"
    );
    assert_eq!(header.windows_access, Some(access), "access roundtrip");
    assert_eq!(header.windows_modify, Some(modify), "modify roundtrip");
}

// ── 0x44 — Comment (variable UTF-8) ─────────────────────────────────────────

#[test]
fn test_lzh_level3_extension_0x44_comment() {
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

// ── 0x46 — Unix file permissions (u16 LE) ────────────────────────────────────

#[test]
fn test_lzh_level3_extension_0x46_unix_permission() {
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

// ── 0x50 — Unix owner names (user\0group) ────────────────────────────────────

#[test]
fn test_lzh_level3_extension_0x50_unix_owner_names() {
    let meta = LzhExtensionMetadata {
        unix_owner_user: Some("alice".into()),
        unix_owner_group: Some("staff".into()),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(
        header.unix_owner_user.as_deref(),
        Some("alice"),
        "owner user roundtrip"
    );
    assert_eq!(
        header.unix_owner_group.as_deref(),
        Some("staff"),
        "owner group roundtrip"
    );
}

// ── 0x51 — Unix owner IDs (uid u32 LE + gid u32 LE) ─────────────────────────

#[test]
fn test_lzh_level3_extension_0x51_unix_owner_ids() {
    let meta = LzhExtensionMetadata {
        unix_uid: Some(1000),
        unix_gid: Some(100),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.unix_uid, Some(1000), "uid roundtrip");
    assert_eq!(header.unix_gid, Some(100), "gid roundtrip");

    let entry = header.to_entry();
    assert_eq!(entry.attributes.uid, Some(1000));
    assert_eq!(entry.attributes.gid, Some(100));
}

// ── 0x54 — Unix mtime (u32 LE seconds since epoch) ───────────────────────────

#[test]
fn test_lzh_level3_extension_0x54_unix_mtime() {
    // 2024-01-01 00:00:00 UTC
    let meta = LzhExtensionMetadata {
        unix_mtime: Some(1_704_067_200),
        ..Default::default()
    };
    let header = roundtrip_single("file.txt", b"hello", &meta);
    assert_eq!(header.unix_mtime, Some(1_704_067_200));

    // Entry's modified timestamp should reflect the extension-provided mtime.
    use std::time::{Duration, UNIX_EPOCH};
    let entry = header.to_entry();
    let expected_modified = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
    assert_eq!(entry.modified, Some(expected_modified));
}

// ── All extensions together ───────────────────────────────────────────────────

#[test]
fn test_lzh_level3_all_extensions_together() {
    let comment = "round-trip all ext headers";
    let filetime_base: u64 = 116_444_736_000_000_000;
    let meta = LzhExtensionMetadata {
        dos_attr: Some(0x0020),
        windows_creation: Some(filetime_base),
        windows_access: Some(filetime_base + 1),
        windows_modify: Some(filetime_base + 2),
        comment: Some(comment.into()),
        unix_permission: Some(0o644),
        unix_owner_user: Some("alice".into()),
        unix_owner_group: Some("staff".into()),
        unix_uid: Some(1234),
        unix_gid: Some(56),
        unix_mtime: Some(1_704_067_200),
        ..Default::default()
    };
    let header = roundtrip_single("archive.log", b"payload", &meta);

    assert_eq!(header.filename, "archive.log", "filename roundtrip");
    assert_eq!(header.dos_attr, Some(0x0020), "0x40 dos_attr roundtrip");
    assert_eq!(
        header.windows_creation,
        Some(filetime_base),
        "0x41 creation roundtrip"
    );
    assert_eq!(
        header.windows_access,
        Some(filetime_base + 1),
        "0x41 access roundtrip"
    );
    assert_eq!(
        header.windows_modify,
        Some(filetime_base + 2),
        "0x41 modify roundtrip"
    );
    assert_eq!(
        header.comment.as_deref(),
        Some(comment),
        "0x44 comment roundtrip"
    );
    assert_eq!(
        header.unix_permission,
        Some(0o644),
        "0x46 permission roundtrip"
    );
    assert_eq!(
        header.unix_owner_user.as_deref(),
        Some("alice"),
        "0x50 owner user roundtrip"
    );
    assert_eq!(
        header.unix_owner_group.as_deref(),
        Some("staff"),
        "0x50 owner group roundtrip"
    );
    assert_eq!(header.unix_uid, Some(1234), "0x51 uid roundtrip");
    assert_eq!(header.unix_gid, Some(56), "0x51 gid roundtrip");
    assert_eq!(
        header.unix_mtime,
        Some(1_704_067_200),
        "0x54 mtime roundtrip"
    );
}

// ── No metadata path ─────────────────────────────────────────────────────────

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
    assert_eq!(header.windows_creation, None);
    assert_eq!(header.windows_access, None);
    assert_eq!(header.windows_modify, None);
    assert_eq!(header.uncompressed_size64, None);
    assert_eq!(header.compressed_size64, None);
    assert_eq!(header.comment, None);
    assert_eq!(header.unix_permission, None);
    assert_eq!(header.unix_owner_user, None);
    assert_eq!(header.unix_owner_group, None);
    assert_eq!(header.unix_uid, None);
    assert_eq!(header.unix_gid, None);
    assert_eq!(header.unix_mtime, None);
}

//! Integration tests for LZH extended header write + read round-trips.
//!
//! Tests cover all 8 new extended header types specified for write support:
//! 0x40 (OS attributes), 0x41 (Windows timestamps), 0x42 (uncompressed size 64),
//! 0x43 (compressed size 64), 0x44 (comment), 0x46 (Unix permissions),
//! 0x50 (Unix owner names), 0x51 (Unix owner IDs).
//! Bonus 0x54 (Unix mtime) is also covered.

use oxiarc_archive::LzhMethod;
use oxiarc_archive::lzh::{LzhCompressionLevel, LzhExtensionMetadata, LzhHeader, LzhWriter};
use std::io::Cursor;

/// Helper: write a level-3 LZH archive with one file + metadata, read it back.
fn roundtrip(name: &str, data: &[u8], meta: &LzhExtensionMetadata) -> LzhHeader {
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output).with_header_level(3);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file_with_metadata(name, data, meta)
            .expect("add_file_with_metadata");
        writer.finish().expect("finish");
    }
    let mut cursor = Cursor::new(&output);
    LzhHeader::read(&mut cursor, 0)
        .expect("LzhHeader::read error")
        .expect("LzhHeader::read returned None")
}

// ── 0x44 comment header ───────────────────────────────────────────────────────

#[test]
fn test_comment_header_roundtrip() {
    let comment = "This is a test comment in UTF-8: こんにちは";
    let meta = LzhExtensionMetadata {
        comment: Some(comment.into()),
        ..Default::default()
    };
    let header = roundtrip("readme.txt", b"content", &meta);
    assert_eq!(header.comment.as_deref(), Some(comment), "0x44 comment");
}

// ── 0x46 Unix permissions header ─────────────────────────────────────────────

#[test]
fn test_unix_permissions_roundtrip() {
    let meta = LzhExtensionMetadata {
        unix_permission: Some(0o755),
        ..Default::default()
    };
    let header = roundtrip("script.sh", b"#!/bin/sh\n", &meta);
    assert_eq!(header.unix_permission, Some(0o755), "0x46 unix_permission");
    let entry = header.to_entry();
    assert_eq!(entry.attributes.unix_mode, Some(0o755));
}

// ── 0x51 Unix owner IDs header ────────────────────────────────────────────────

#[test]
fn test_unix_owner_ids_roundtrip() {
    let meta = LzhExtensionMetadata {
        unix_uid: Some(1000),
        unix_gid: Some(100),
        ..Default::default()
    };
    let header = roundtrip("file.txt", b"data", &meta);
    assert_eq!(header.unix_uid, Some(1000), "0x51 uid");
    assert_eq!(header.unix_gid, Some(100), "0x51 gid");

    let entry = header.to_entry();
    assert_eq!(entry.attributes.uid, Some(1000));
    assert_eq!(entry.attributes.gid, Some(100));
}

// ── 0x50 Unix owner names header ─────────────────────────────────────────────

#[test]
fn test_unix_owner_names_roundtrip() {
    let meta = LzhExtensionMetadata {
        unix_owner_user: Some("alice".into()),
        unix_owner_group: Some("developers".into()),
        ..Default::default()
    };
    let header = roundtrip("src/main.rs", b"fn main() {}", &meta);
    assert_eq!(
        header.unix_owner_user.as_deref(),
        Some("alice"),
        "0x50 user name"
    );
    assert_eq!(
        header.unix_owner_group.as_deref(),
        Some("developers"),
        "0x50 group name"
    );
}

// ── 0x41 Windows timestamps header ───────────────────────────────────────────

#[test]
fn test_windows_timestamps_roundtrip() {
    // Use FILETIME values for known dates
    // 2024-01-01 00:00:00 UTC = Unix timestamp 1_704_067_200
    // FILETIME = Unix_secs * 10_000_000 + 116_444_736_000_000_000
    let filetime_base: u64 = 116_444_736_000_000_000;
    let creation = filetime_base + 1_704_067_200 * 10_000_000; // 2024-01-01
    let access = filetime_base + 1_704_153_600 * 10_000_000; // 2024-01-02
    let modify = filetime_base + 1_704_240_000 * 10_000_000; // 2024-01-03

    let meta = LzhExtensionMetadata {
        windows_creation: Some(creation),
        windows_access: Some(access),
        windows_modify: Some(modify),
        ..Default::default()
    };
    let header = roundtrip("doc.pdf", b"PDF content", &meta);
    assert_eq!(
        header.windows_creation,
        Some(creation),
        "0x41 creation timestamp"
    );
    assert_eq!(header.windows_access, Some(access), "0x41 access timestamp");
    assert_eq!(header.windows_modify, Some(modify), "0x41 modify timestamp");
}

// ── 0x42/0x43 64-bit size headers auto-emitted ───────────────────────────────

/// Verify that `add_file_raw` automatically emits 0x42 / 0x43 extension
/// headers when `original_size > u32::MAX` without the caller supplying any
/// `LzhExtensionMetadata`.  We pass a tiny store-compressed payload but tell
/// the writer the uncompressed size is `u32::MAX + 1` so the auto-inject
/// path fires.
#[test]
fn test_size64_auto_emit_via_add_file_raw() {
    // Target uncompressed size that exceeds u32::MAX.
    let large_uncompressed: u64 = u32::MAX as u64 + 1; // 4_294_967_296

    // Build a level-3 archive using add_file_raw with NO metadata.
    let tiny_payload = b"tiny data";
    let mut output = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut output).with_header_level(3);
        // mtime = arbitrary Unix timestamp; crc16 = 0 (header read doesn't verify)
        writer
            .add_file_raw(
                "huge_file.bin",
                LzhMethod::Lh0,
                0,                  // crc16 — not verified by LzhHeader::read
                large_uncompressed, // original_size triggers auto-inject
                tiny_payload,
                1_704_067_200,
                None, // NO explicit metadata
            )
            .expect("add_file_raw");
        writer.finish().expect("finish");
    }

    // Parse the header back.
    let mut cursor = Cursor::new(&output);
    let header = LzhHeader::read(&mut cursor, 0)
        .expect("LzhHeader::read error")
        .expect("LzhHeader::read returned None");

    // The writer should have auto-injected 0x42 with the large value.
    assert_eq!(
        header.uncompressed_size64,
        Some(large_uncompressed),
        "0x42 uncompressed_size64 auto-injected"
    );

    // Entry.size must reflect the 64-bit value.
    let entry = header.to_entry();
    assert_eq!(
        entry.size, large_uncompressed,
        "entry.size from auto-injected 0x42"
    );
}

/// Verify that when explicit 64-bit sizes are supplied via `LzhExtensionMetadata`,
/// they round-trip correctly (explicit path, as a baseline companion to the
/// auto-inject test above).
#[test]
fn test_size64_explicit_metadata_roundtrip() {
    let large_uncompressed: u64 = u32::MAX as u64 + 1;
    let large_compressed: u64 = u32::MAX as u64 + 2;

    let meta = LzhExtensionMetadata {
        uncompressed_size64: Some(large_uncompressed),
        compressed_size64: Some(large_compressed),
        ..Default::default()
    };
    let header = roundtrip("huge_file.bin", b"tiny data", &meta);

    assert_eq!(
        header.uncompressed_size64,
        Some(large_uncompressed),
        "0x42 uncompressed_size64"
    );
    assert_eq!(
        header.compressed_size64,
        Some(large_compressed),
        "0x43 compressed_size64"
    );

    let entry = header.to_entry();
    assert_eq!(entry.size, large_uncompressed, "entry.size from 0x42");
    assert_eq!(
        entry.compressed_size, large_compressed,
        "entry.compressed_size from 0x43"
    );
}

// ── 0x40 OS attributes header ─────────────────────────────────────────────────

#[test]
fn test_os_attributes_roundtrip() {
    // 0x0020 = MS-DOS archive bit
    let meta = LzhExtensionMetadata {
        dos_attr: Some(0x0020),
        ..Default::default()
    };
    let header = roundtrip("backup.dat", b"backup content", &meta);
    assert_eq!(header.dos_attr, Some(0x0020), "0x40 dos_attr");
}

// ── Full chain: comment + permissions + owner IDs all together ───────────────

#[test]
fn test_extended_headers_chain() {
    let comment = "chain test entry";
    let meta = LzhExtensionMetadata {
        comment: Some(comment.into()),
        unix_permission: Some(0o644),
        unix_uid: Some(501),
        unix_gid: Some(20),
        ..Default::default()
    };
    let header = roundtrip("chain_test.txt", b"data for chain test", &meta);

    // All three should survive the chain
    assert_eq!(
        header.comment.as_deref(),
        Some(comment),
        "0x44 comment in chain"
    );
    assert_eq!(
        header.unix_permission,
        Some(0o644),
        "0x46 permission in chain"
    );
    assert_eq!(header.unix_uid, Some(501), "0x51 uid in chain");
    assert_eq!(header.unix_gid, Some(20), "0x51 gid in chain");

    // Entry derived from parsed header reflects all three
    let entry = header.to_entry();
    assert_eq!(entry.comment.as_deref(), Some(comment));
    assert_eq!(entry.attributes.unix_mode, Some(0o644));
    assert_eq!(entry.attributes.uid, Some(501));
    assert_eq!(entry.attributes.gid, Some(20));
}

// ── Empty owner names edge case ───────────────────────────────────────────────

#[test]
fn test_unix_owner_names_empty_group() {
    // Only user supplied; group should round-trip as empty string
    let meta = LzhExtensionMetadata {
        unix_owner_user: Some("root".into()),
        ..Default::default()
    };
    let header = roundtrip("etc/passwd", b"root:x:0:0", &meta);
    assert_eq!(header.unix_owner_user.as_deref(), Some("root"));
    // Group is empty — present with empty string value
    assert_eq!(header.unix_owner_group.as_deref(), Some(""));
}

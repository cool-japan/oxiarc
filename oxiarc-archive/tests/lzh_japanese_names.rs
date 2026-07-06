//! LZH filename-encoding round-trip tests.
//!
//! LzhWriter used to store names as raw UTF-8 while LzhReader decodes
//! Shift_JIS first, so Japanese names came back as mojibake and archives
//! were unreadable by legacy Japanese tools (which expect Shift_JIS).
//! The writer now emits level-2 headers with Shift_JIS names (the LHA /
//! Lhaplus convention); these tests pin the exact round-trip and the raw
//! on-disk encoding.

use oxiarc_archive::lzh::{LzhCompressionLevel, LzhReader, LzhWriter};
use oxiarc_core::EntryType;
use std::io::Cursor;

/// Shift_JIS bytes for 日本語ファイル.txt.
const SJIS_FILENAME: &[u8] = &[
    0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, // 日本語
    0x83, 0x74, 0x83, 0x40, 0x83, 0x43, 0x83, 0x8B, // ファイル
    b'.', b't', b'x', b't',
];

/// Locate a byte needle in a haystack.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn japanese_name_roundtrip_exact() {
    let content = "これは日本語のテストデータです。".as_bytes().repeat(8);

    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive);
        writer
            .add_file("日本語ファイル.txt", &content)
            .expect("add_file with Japanese name");
        writer.finish().expect("finish");
    }

    let mut reader = LzhReader::new(Cursor::new(&archive)).expect("open archive");
    let entries = reader.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].name, "日本語ファイル.txt",
        "Japanese filename must round-trip exactly"
    );

    let data = reader.extract_to_vec(&entries[0]).expect("extract");
    assert_eq!(data, content, "content must round-trip (CRC-verified)");
}

#[test]
fn japanese_name_is_stored_as_shift_jis_on_disk() {
    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive);
        writer
            .add_file("日本語ファイル.txt", b"shift jis check")
            .expect("add_file");
        writer.finish().expect("finish");
    }

    assert!(
        contains(&archive, SJIS_FILENAME),
        "archive must contain the Shift_JIS encoding of the filename"
    );
    assert!(
        !contains(&archive, "日本語ファイル".as_bytes()),
        "archive must not contain the raw UTF-8 encoding of the filename"
    );
}

#[test]
fn japanese_directory_tree_roundtrip() {
    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive);
        writer
            .add_directory("日本語ディレクトリ")
            .expect("add_directory");
        writer
            .add_file("日本語ディレクトリ/日本語ファイル.txt", b"nested content")
            .expect("add nested file");
        writer.finish().expect("finish");
    }

    let mut reader = LzhReader::new(Cursor::new(&archive)).expect("open archive");
    let entries = reader.entries();
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0].name, "日本語ディレクトリ/");
    assert_eq!(entries[0].entry_type, EntryType::Directory);

    assert_eq!(entries[1].name, "日本語ディレクトリ/日本語ファイル.txt");
    assert_eq!(entries[1].entry_type, EntryType::File);

    let data = reader.extract_to_vec(&entries[1]).expect("extract nested");
    assert_eq!(data, b"nested content");
}

#[test]
fn ascii_names_unaffected_by_shift_jis_encoding() {
    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive);
        writer.set_compression(LzhCompressionLevel::Store);
        writer.add_file("plain_ascii.txt", b"ascii").expect("add");
        writer.finish().expect("finish");
    }

    assert!(contains(&archive, b"plain_ascii.txt"));

    let mut reader = LzhReader::new(Cursor::new(&archive)).expect("open");
    let entries = reader.entries();
    assert_eq!(entries[0].name, "plain_ascii.txt");
    assert_eq!(reader.extract_to_vec(&entries[0]).expect("extract"), b"ascii");
}

#[test]
fn level1_headers_also_encode_shift_jis() {
    // Explicit level-1 headers must also write Shift_JIS names.
    let mut archive = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut archive).with_header_level(1);
        writer
            .add_file("日本語ファイル.txt", b"level 1")
            .expect("add_file level 1");
        writer.finish().expect("finish");
    }

    assert!(
        contains(&archive, SJIS_FILENAME),
        "level-1 header must contain the Shift_JIS filename"
    );

    let mut reader = LzhReader::new(Cursor::new(&archive)).expect("open");
    let entries = reader.entries();
    assert_eq!(entries[0].name, "日本語ファイル.txt");
    assert_eq!(
        reader.extract_to_vec(&entries[0]).expect("extract"),
        b"level 1"
    );
}

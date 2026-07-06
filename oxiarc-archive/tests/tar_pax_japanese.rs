//! Hermetic TAR PAX tests for long multi-byte (Japanese) names.
//!
//! Covers the fix for the `TarHeader::to_block` char-boundary panic
//! ("byte index 155 is not a char boundary") and the write-side PAX
//! extended-header support for names/linknames exceeding UStar limits.
//!
//! Golden vectors were generated once during development:
//! - `PAX_JAPANESE_GOLDEN_TAR`: python3 `tarfile` (PAX_FORMAT, deterministic
//!   metadata), verified parseable by bsdtar 3.5.3 / libarchive 3.7.4.
//! - `GOLDEN_*_BLOCK_HEX`: `TarHeader::to_block` output captured from the
//!   pre-fix implementation, locking byte-compatibility for UStar-short
//!   names. The committed suite invokes no external tools.

use oxiarc_archive::tar::{TarHeader, TarReader, TarWriter};
use std::io::Cursor;

/// PAX-format tar produced by python3 `tarfile` with two long Japanese
/// names (60-kanji filename and a 342-byte nested path).
static PAX_JAPANESE_GOLDEN_TAR: &[u8] = include_bytes!("data/pax_japanese_golden.tar");

/// 60-kanji base name (180 bytes of UTF-8).
fn kanji60() -> String {
    "漢".repeat(60)
}

/// Decode a lowercase hex string into bytes (test helper).
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert_eq!(hex.len() % 2, 0, "hex string must have even length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex digit pair"))
        .collect()
}

// ---------------------------------------------------------------------------
// Write-side: long Japanese names must not panic and must round-trip.
// ---------------------------------------------------------------------------

#[test]
fn test_write_60_kanji_filename_roundtrip() {
    let name = format!("{}.txt", kanji60());
    assert!(name.len() > 100, "test premise: name exceeds UStar limit");

    let content = "こんにちは世界".as_bytes();

    let mut output = Vec::new();
    {
        let mut writer = TarWriter::new(&mut output);
        writer.add_file(&name, content).expect("add_file 60-kanji");
        writer.finish().expect("finish");
    }

    let mut reader = TarReader::new(Cursor::new(output)).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, name, "exact name must round-trip via PAX");

    let data = reader.extract_to_vec(&entries[0]).expect("extract");
    assert_eq!(data, content);
}

#[test]
fn test_write_nested_japanese_long_path_roundtrip() {
    let dir = format!("{}/", "日本語のとても長いディレクトリ名".repeat(4));
    let file = format!("{}深い階層のフォルダ/{}.dat", dir, kanji60());
    assert!(dir.len() > 100 && file.len() > 100);

    let content: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();

    let mut output = Vec::new();
    {
        let mut writer = TarWriter::new(&mut output);
        writer.add_directory(&dir).expect("add_directory long dir");
        writer.add_file(&file, &content).expect("add_file nested");
        writer.finish().expect("finish");
    }

    let mut reader = TarReader::new(Cursor::new(output)).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, dir, "directory name must round-trip");
    assert!(entries[0].is_dir(), "directory typeflag preserved");
    assert_eq!(entries[1].name, file, "nested file name must round-trip");

    let data = reader.extract_to_vec(&entries[1]).expect("extract nested");
    assert_eq!(data, content);
}

#[test]
fn test_write_symlink_long_japanese_name_and_target() {
    let link_name = format!("{}リンク", "とても長いシンボリックリンク名".repeat(4));
    let target = format!("{}/実体.txt", "参照先のとても長いディレクトリ".repeat(4));
    assert!(link_name.len() > 100 && target.len() > 100);

    let mut output = Vec::new();
    {
        let mut writer = TarWriter::new(&mut output);
        writer
            .add_symlink(&link_name, &target)
            .expect("add_symlink long");
        writer.finish().expect("finish");
    }

    let reader = TarReader::new(Cursor::new(output)).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, link_name, "symlink name via PAX path");
    let link_target = entries[0]
        .link_target
        .as_ref()
        .expect("link_target must be present");
    assert_eq!(
        link_target.to_string_lossy(),
        target,
        "symlink target via PAX linkpath"
    );
}

#[test]
fn test_add_entry_from_header_long_japanese_name() {
    let name = format!("{}/{}.bin", "案".repeat(40), kanji60());
    let content = b"payload bytes";

    let mut header = TarHeader::new_file(&name, content.len() as u64, 0o640);
    header.mtime = 1_600_000_000;
    header.uid = 1234;
    header.gid = 5678;
    header.uname = "alice".to_string();
    header.gname = "staff".to_string();

    let mut output = Vec::new();
    {
        let mut writer = TarWriter::new(&mut output);
        writer
            .add_entry_from_header(&header, content)
            .expect("add_entry_from_header long Japanese");
        writer.finish().expect("finish");
    }

    let mut reader = TarReader::new(Cursor::new(output)).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, name, "exact name must round-trip via PAX");

    let hdr = reader.header_for(&entries[0]).expect("header_for");
    assert_eq!(hdr.mode, 0o640, "mode preserved");
    assert_eq!(hdr.uid, 1234, "uid preserved");
    assert_eq!(hdr.gid, 5678, "gid preserved");
    assert_eq!(hdr.uname, "alice", "uname preserved");
    assert_eq!(hdr.gname, "staff", "gname preserved");

    let data = reader.extract_to_vec(&entries[0]).expect("extract");
    assert_eq!(data, content);
}

/// Direct `to_block` on an unsplittable long multi-byte name must return an
/// error instead of panicking at a non-char boundary.
#[test]
fn test_to_block_long_kanji_no_slash_errors_without_panic() {
    let name = format!("{}.txt", kanji60()); // 184 bytes, no '/'
    let header = TarHeader::new_file(&name, 0, 0o644);
    let result = header.to_block();
    assert!(
        result.is_err(),
        "unsplittable >100-byte name must be a clean error, not a panic"
    );
}

/// The UStar block written alongside a PAX record must itself contain a
/// valid-UTF-8, char-boundary-truncated fallback name (never split kanji).
#[test]
fn test_fallback_name_in_ustar_block_is_valid_utf8() {
    let name = format!("{}.txt", kanji60());

    let mut output = Vec::new();
    {
        let mut writer = TarWriter::new(&mut output);
        writer.add_file(&name, b"x").expect("add_file");
        writer.finish().expect("finish");
    }

    // Block 0: PAX header ('x'), block 1: PAX data, block 2: file header.
    assert!(output.len() >= 512 * 3);
    assert_eq!(output[156], b'x', "first block must be the PAX header");
    let file_block = &output[512 * 2..512 * 3];
    let name_field = &file_block[0..100];
    let end = name_field
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_field.len());
    let fallback =
        std::str::from_utf8(&name_field[..end]).expect("fallback name must be valid UTF-8");
    assert!(!fallback.is_empty(), "fallback name must not be empty");
    assert!(
        name.ends_with(fallback),
        "fallback must be a suffix of the real name"
    );
}

// ---------------------------------------------------------------------------
// Read-side interop: PAX archive produced by a third-party writer
// (python3 tarfile, PAX_FORMAT; verified with bsdtar during development).
// ---------------------------------------------------------------------------

#[test]
fn test_reader_parses_third_party_pax_japanese_golden() {
    let name1 = format!("{}.txt", kanji60());
    let name2 = format!(
        "{}/深い階層のフォルダ/最終的なファイル名{}.dat",
        "日本語のとても長いディレクトリ名".repeat(4),
        "案".repeat(30)
    );

    let mut reader =
        TarReader::new(Cursor::new(PAX_JAPANESE_GOLDEN_TAR.to_vec())).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, name1, "60-kanji name from python PAX");
    assert_eq!(entries[1].name, name2, "nested long path from python PAX");

    let data1 = reader.extract_to_vec(&entries[0]).expect("extract 1");
    assert_eq!(data1, "こんにちは世界".as_bytes());

    let data2 = reader.extract_to_vec(&entries[1]).expect("extract 2");
    let expected: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
    assert_eq!(data2, expected);

    let hdr1 = reader.header_for(&entries[0]).expect("header_for 1");
    assert_eq!(hdr1.mode, 0o644);
    assert_eq!(hdr1.mtime, 1_600_000_000);
    let hdr2 = reader.header_for(&entries[1]).expect("header_for 2");
    assert_eq!(hdr2.mode, 0o600);
}

// ---------------------------------------------------------------------------
// Byte-compatibility: UStar-short names must serialize exactly as before
// the fix (goldens captured from the pre-fix implementation).
// ---------------------------------------------------------------------------

/// `TarHeader::new_file("hello.txt", 13, 0o644)` with mtime=1600000000,
/// uname="user", gname="group" — pre-fix `to_block` output.
static GOLDEN_FILE_BLOCK_HEX: &str = concat!(
    "68656c6c6f2e747874000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000003030303036343400",
    "303030313735300030303031373530003030303030303030303135003133373237343130",
    "303030003031313634340020300000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000075737461720030307573657200000000000000000000000000000000000000",
    "00000000000000000067726f757000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000",
);

/// `TarHeader::new_directory("docs/", 0o755)` with mtime=1600000000 —
/// pre-fix `to_block` output.
static GOLDEN_DIR_BLOCK_HEX: &str = concat!(
    "646f63732f00000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000003030303037353500",
    "303030313735300030303031373530003030303030303030303030003133373237343130",
    "303030003030363736300020350000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000075737461720030300000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000",
);

/// `TarHeader::new_symlink("link.txt", "real.txt")` with mtime=1600000000 —
/// pre-fix `to_block` output.
static GOLDEN_SYMLINK_BLOCK_HEX: &str = concat!(
    "6c696e6b2e74787400000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000003030303037373700",
    "303030313735300030303031373530003030303030303030303030003133373237343130",
    "303030003031313230370020327265616c2e747874000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000075737461720030300000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000",
);

/// 141-byte ASCII path split into UStar prefix/name (80×'d' + '/' + 60×'f'),
/// size=1, mtime=1600000000 — pre-fix `to_block` output.
static GOLDEN_SPLIT_BLOCK_HEX: &str = concat!(
    "666666666666666666666666666666666666666666666666666666666666666666666666",
    "666666666666666666666666666666666666666666666666000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000003030303036343400",
    "303030313735300030303031373530003030303030303030303031003133373237343130",
    "303030003034313437310020300000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000075737461720030300000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000646464646464646464646464646464",
    "646464646464646464646464646464646464646464646464646464646464646464646464",
    "646464646464646464646464646464646464646464646464646464646400000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "000000000000000000000000000000000000000000000000000000000000000000000000",
    "0000000000000000",
);

#[test]
fn test_ustar_short_file_block_byte_compatible() {
    let mut header = TarHeader::new_file("hello.txt", 13, 0o644);
    header.mtime = 1_600_000_000;
    header.uname = "user".to_string();
    header.gname = "group".to_string();
    let block = header.to_block().expect("to_block file");
    assert_eq!(
        block.as_slice(),
        hex_to_bytes(GOLDEN_FILE_BLOCK_HEX).as_slice(),
        "short file header must be byte-identical to pre-fix output"
    );
}

#[test]
fn test_ustar_short_dir_block_byte_compatible() {
    let mut header = TarHeader::new_directory("docs/", 0o755);
    header.mtime = 1_600_000_000;
    let block = header.to_block().expect("to_block dir");
    assert_eq!(
        block.as_slice(),
        hex_to_bytes(GOLDEN_DIR_BLOCK_HEX).as_slice(),
        "short directory header must be byte-identical to pre-fix output"
    );
}

#[test]
fn test_ustar_short_symlink_block_byte_compatible() {
    let mut header = TarHeader::new_symlink("link.txt", "real.txt");
    header.mtime = 1_600_000_000;
    let block = header.to_block().expect("to_block symlink");
    assert_eq!(
        block.as_slice(),
        hex_to_bytes(GOLDEN_SYMLINK_BLOCK_HEX).as_slice(),
        "short symlink header must be byte-identical to pre-fix output"
    );
}

#[test]
fn test_ustar_prefix_split_block_byte_compatible() {
    let long = format!("{}/{}", "d".repeat(80), "f".repeat(60));
    let mut header = TarHeader::new_file(&long, 1, 0o644);
    header.mtime = 1_600_000_000;
    let block = header.to_block().expect("to_block split");
    assert_eq!(
        block.as_slice(),
        hex_to_bytes(GOLDEN_SPLIT_BLOCK_HEX).as_slice(),
        "UStar prefix/name split must be byte-identical to pre-fix output"
    );
}

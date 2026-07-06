//! Hermetic integration tests for real-world-style LZH archives containing
//! `-lhd-` (directory) and `-lh1-` (LHarc 1.x adaptive-Huffman) entries.
//!
//! Archives produced by LHA / Lhaplus store directories as `-lhd-` entries
//! and frequently contain `-lh1-` payloads; oxiarc-archive used to abort the
//! whole archive on either. The fixture below is synthesized per the LHA
//! level-1 header specification (base header + chained extension headers,
//! whose total size is included in the base "compressed size" skip field),
//! with Shift_JIS names and a genuine `-lh1-` bitstream produced by the
//! spec-faithful LZHUF coder in `oxiarc-lzhuf`. No external tools are
//! invoked — the fixture is fully deterministic and self-contained.

use oxiarc_archive::lzh::{LzhReader, LzhStreamReader};
use oxiarc_core::{Crc16, EntryType};
use std::io::Cursor;

/// Shift_JIS bytes for 日本語 (the directory name).
const SJIS_NIHONGO: &[u8] = &[0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA];
/// Shift_JIS bytes for 日本語ファイル.txt (the file name).
const SJIS_FILENAME: &[u8] = &[
    0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, // 日本語
    0x83, 0x74, 0x83, 0x40, 0x83, 0x43, 0x83, 0x8B, // ファイル
    b'.', b't', b'x', b't',
];

/// Build one LZH level-1 member: base header + extension chain + payload.
///
/// Extension chain blocks are `[type(1)][data][next_size(2)]`; each block's
/// declared size (written before it) covers all three parts. The base
/// header's "compressed size" field is a skip size that includes the whole
/// extension chain (LHA level-1 semantics).
fn level1_member(
    method: &[u8; 5],
    inline_name: &[u8],
    original_size: u32,
    crc16: u16,
    ext_blocks: &[(u8, Vec<u8>)],
    payload: &[u8],
) -> Vec<u8> {
    let ext_total: usize = ext_blocks.iter().map(|(_, d)| 3 + d.len()).sum();
    let compressed_field = (payload.len() + ext_total) as u32;

    // Base header: [size][checksum][method 5][compressed 4][original 4]
    //              [mtime 4][attr][level][name_len][name][crc 2][os 1]
    let header_size = 2 + 19 + 1 + inline_name.len() + 2 + 1;
    assert!(header_size <= u8::MAX as usize, "fixture name too long");

    let mut member = Vec::new();
    member.push(header_size as u8);
    member.push(0); // checksum placeholder
    member.extend_from_slice(method);
    member.extend_from_slice(&compressed_field.to_le_bytes());
    member.extend_from_slice(&original_size.to_le_bytes());
    member.extend_from_slice(&0u32.to_le_bytes()); // mtime (DOS, unused here)
    member.push(0x20); // attribute
    member.push(1); // level 1
    member.push(inline_name.len() as u8);
    member.extend_from_slice(inline_name);
    member.extend_from_slice(&crc16.to_le_bytes());
    member.push(b'M'); // OS id

    // Checksum over bytes [2..header_size].
    let checksum: u8 = member[2..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    member[1] = checksum;

    // Extension chain: first size, then each block with the next block's
    // size (or the terminator) trailing it.
    let mut sizes: Vec<u16> = ext_blocks
        .iter()
        .map(|(_, d)| (3 + d.len()) as u16)
        .collect();
    sizes.push(0); // terminator
    member.extend_from_slice(&sizes[0].to_le_bytes());
    for (i, (ext_type, data)) in ext_blocks.iter().enumerate() {
        member.push(*ext_type);
        member.extend_from_slice(data);
        member.extend_from_slice(&sizes[i + 1].to_le_bytes());
    }

    member.extend_from_slice(payload);
    member
}

/// The file content stored in the `-lh1-` entry.
fn lh1_content() -> Vec<u8> {
    "こんにちは、LZH の世界！ Hello from -lh1-. "
        .as_bytes()
        .iter()
        .cycle()
        .take(6000)
        .copied()
        .collect()
}

/// Build the complete fixture archive:
///   1. `-lhd-` directory 日本語 (dirname in a 0x02 extension header)
///   2. `-lh1-` file 日本語/日本語ファイル.txt (basename inline, dirname in
///      0x02, Unix mtime in 0x54)
///   3. `-pm2-` entry with an unsupported method (must list, must not abort)
fn build_fixture() -> Vec<u8> {
    let mut archive = Vec::new();

    // 1. Directory entry: -lhd-, no inline name, 0x02 dirname.
    let mut dir_payload = SJIS_NIHONGO.to_vec();
    dir_payload.push(0xFF); // component terminator
    archive.extend_from_slice(&level1_member(
        b"-lhd-",
        &[],
        0,
        0,
        &[(0x02, dir_payload.clone())],
        &[],
    ));

    // 2. File entry: -lh1- with a genuine LZHUF stream.
    let content = lh1_content();
    let crc16 = Crc16::compute(&content);
    let lh1_stream = oxiarc_lzhuf::encode_lh1(&content);
    archive.extend_from_slice(&level1_member(
        b"-lh1-",
        SJIS_FILENAME,
        content.len() as u32,
        crc16,
        &[
            (0x02, dir_payload),
            (0x54, 1_234_567_890u32.to_le_bytes().to_vec()),
        ],
        &lh1_stream,
    ));

    // 3. Unsupported method entry (must be listed and skipped, not abort).
    archive.extend_from_slice(&level1_member(
        b"-pm2-",
        b"unknown.bin",
        4,
        0xBEEF,
        &[],
        &[0xDE, 0xAD, 0xBE, 0xEF],
    ));

    archive.push(0x00); // end-of-archive marker
    archive
}

#[test]
fn fixture_lists_all_entries_including_lhd_and_unknown() {
    let archive = build_fixture();
    let reader = LzhReader::new(Cursor::new(archive)).expect("archive with -lhd-/-lh1- must open");
    let entries = reader.entries();

    assert_eq!(entries.len(), 3, "all three entries must be listed");

    assert_eq!(entries[0].name, "日本語/", "directory name (Shift_JIS)");
    assert_eq!(entries[0].entry_type, EntryType::Directory);
    assert_eq!(entries[0].size, 0);

    assert_eq!(
        entries[1].name, "日本語/日本語ファイル.txt",
        "file name assembled from 0x02 dirname + inline Shift_JIS basename"
    );
    assert_eq!(entries[1].entry_type, EntryType::File);
    assert_eq!(entries[1].size, lh1_content().len() as u64);

    assert_eq!(entries[2].name, "unknown.bin");
    assert_eq!(entries[2].entry_type, EntryType::File);
}

#[test]
fn fixture_extracts_lh1_content_with_crc_verification() {
    let archive = build_fixture();
    let mut reader = LzhReader::new(Cursor::new(archive)).expect("open fixture");
    let entries = reader.entries();

    // Directory extraction yields no bytes.
    let dir_data = reader.extract_to_vec(&entries[0]).expect("extract -lhd-");
    assert!(dir_data.is_empty(), "-lhd- entries carry no data");

    // The -lh1- payload decodes byte-exactly (extract() verifies CRC-16).
    let data = reader
        .extract_to_vec(&entries[1])
        .expect("extract -lh1- entry");
    assert_eq!(data, lh1_content(), "-lh1- content mismatch");
}

#[test]
fn fixture_unknown_method_fails_per_entry_not_per_archive() {
    let archive = build_fixture();
    let mut reader = LzhReader::new(Cursor::new(archive)).expect("open fixture");
    let entries = reader.entries();

    // The unsupported entry errors...
    let err = reader
        .extract_to_vec(&entries[2])
        .expect_err("-pm2- must be rejected at extraction time");
    let msg = format!("{err}");
    assert!(
        msg.contains("pm2"),
        "error should identify the unsupported method: {msg}"
    );

    // ...while the other entries remain fully extractable afterwards.
    let data = reader
        .extract_to_vec(&entries[1])
        .expect("-lh1- entry must still extract after the unsupported one");
    assert_eq!(data, lh1_content());
}

#[test]
fn fixture_streams_with_read_only_reader() {
    let archive = build_fixture();
    let mut stream = LzhStreamReader::new(Cursor::new(archive));

    // Entry 1: directory.
    let entry = stream
        .next_entry()
        .expect("next_entry -lhd-")
        .expect("-lhd- present");
    assert_eq!(entry.header.filename, "日本語");
    assert!(entry.header.method.is_directory());
    drop(entry);

    // Entry 2: -lh1- file.
    let mut entry = stream
        .next_entry()
        .expect("next_entry -lh1-")
        .expect("-lh1- present");
    assert_eq!(entry.header.filename, "日本語/日本語ファイル.txt");
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut data).expect("read -lh1- entry");
    assert_eq!(data, lh1_content());
    drop(entry);

    // Entry 3: unsupported method — errors for this entry only...
    let err = match stream.next_entry() {
        Err(e) => e,
        Ok(_) => panic!("-pm2- must error in the streaming reader"),
    };
    assert!(format!("{err}").contains("pm2"));

    // ...and the stream can continue to the end-of-archive marker.
    assert!(
        stream.next_entry().expect("end of archive").is_none(),
        "stream must reach the terminator after skipping the unsupported entry"
    );
}

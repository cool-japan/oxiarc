//! Hermetic interop tests for the 7z reader.
//!
//! Golden fixtures were generated once during development and embedded under
//! `tests/data/`; the suite never invokes external tools.
//!
//! * `sevenz_bsdtar_copy.7z` — created by `bsdtar --format 7zip --options
//!   7zip:compression=copy` from four files (17 B, 1000 B, 31 B nested, 0 B)
//!   plus a directory. Plain (non-encoded) header, one Copy folder per file,
//!   substream CRCs, `kEmptyStream` / `kEmptyFile` markers.
//! * `sevenz_bsdtar_lzma1.7z` / `sevenz_bsdtar_lzma2.7z` — same input tree,
//!   compressed with LZMA1 / LZMA2 into a single solid folder behind an
//!   LZMA-encoded header.
//! * `sevenz_solid_copy.7z` — single Copy folder holding three substreams,
//!   exercising `kNumUnpackStream` + per-substream `kSize` + `kCRC` parsing;
//!   validated with bsdtar at generation time.

use oxiarc_archive::SevenZReader;
use std::io::Cursor;

const BSDTAR_COPY_7Z: &[u8] = include_bytes!("data/sevenz_bsdtar_copy.7z");
const BSDTAR_LZMA1_7Z: &[u8] = include_bytes!("data/sevenz_bsdtar_lzma1.7z");
const BSDTAR_LZMA2_7Z: &[u8] = include_bytes!("data/sevenz_bsdtar_lzma2.7z");
const SOLID_COPY_7Z: &[u8] = include_bytes!("data/sevenz_solid_copy.7z");

const ALPHA: &[u8] = b"Hello, 7z world!\n";
const GAMMA: &[u8] = b"Nested file content in subdir.\n";

/// Contents of `beta.bin` in the bsdtar fixtures.
fn beta_bytes() -> Vec<u8> {
    (0u32..1000).map(|i| ((i * 7 + 3) % 256) as u8).collect()
}

fn open(bytes: &'static [u8]) -> SevenZReader<Cursor<&'static [u8]>> {
    SevenZReader::new(Cursor::new(bytes)).expect("open embedded 7z fixture")
}

/// Assert names, sizes, and directory flags of the bsdtar fixture tree.
fn assert_bsdtar_listing(reader: &SevenZReader<Cursor<&'static [u8]>>) {
    let entries = reader.sevenz_entries();
    assert_eq!(entries.len(), 5, "expected 5 entries");

    let find = |name: &str| {
        entries
            .iter()
            .position(|e| e.name == name)
            .unwrap_or_else(|| panic!("entry not found: {}", name))
    };

    let alpha = &entries[find("alpha.txt")];
    assert_eq!(alpha.size, 17, "alpha.txt size");
    assert!(!alpha.is_dir);

    let beta = &entries[find("beta.bin")];
    assert_eq!(beta.size, 1000, "beta.bin size");
    assert!(!beta.is_dir);

    let gamma = &entries[find("subdir/gamma.txt")];
    assert_eq!(gamma.size, 31, "subdir/gamma.txt size");
    assert!(!gamma.is_dir);

    let empty = &entries[find("empty.txt")];
    assert_eq!(empty.size, 0, "empty.txt size");
    assert!(
        !empty.is_dir,
        "empty.txt must be a zero-byte file, not a dir"
    );
    assert!(empty.folder_index.is_none(), "empty.txt has no substream");

    let dir = &entries[find("subdir")];
    assert!(dir.is_dir, "subdir must be a directory");
    assert_eq!(dir.size, 0);
}

/// Extract every bsdtar-fixture entry and compare byte-exact.
fn assert_bsdtar_extraction(reader: &mut SevenZReader<Cursor<&'static [u8]>>) {
    let beta = beta_bytes();
    let names: Vec<String> = reader
        .sevenz_entries()
        .iter()
        .map(|e| e.name.clone())
        .collect();

    for (index, name) in names.iter().enumerate() {
        let data = reader
            .extract(index)
            .unwrap_or_else(|e| panic!("extract {} failed: {}", name, e));
        let expected: &[u8] = match name.as_str() {
            "alpha.txt" => ALPHA,
            "beta.bin" => &beta,
            "subdir/gamma.txt" => GAMMA,
            "empty.txt" | "subdir" => &[],
            other => panic!("unexpected entry: {}", other),
        };
        assert_eq!(data, expected, "byte-exact mismatch for {}", name);
    }
}

#[test]
fn bsdtar_copy_listing_reports_real_sizes() {
    let reader = open(BSDTAR_COPY_7Z);
    assert_bsdtar_listing(&reader);
}

#[test]
fn bsdtar_copy_public_entries_report_real_sizes() {
    // Regression: listings used to report size 0 for every entry because
    // `entry.size` was overwritten with the (never advanced) folder offset.
    let reader = open(BSDTAR_COPY_7Z);
    let entries = reader.entries();
    let mut sizes: Vec<u64> = entries.iter().map(|e| e.size).collect();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![0, 0, 17, 31, 1000]);
    assert!(
        entries.iter().any(|e| e.size == 1000),
        "public Entry list must carry the substream sizes"
    );
}

#[test]
fn bsdtar_copy_extracts_byte_exact() {
    let mut reader = open(BSDTAR_COPY_7Z);
    assert_bsdtar_extraction(&mut reader);
}

#[test]
fn zero_byte_member_does_not_abort_extraction() {
    // Regression: an entry without a folder (empty stream + empty file
    // marker) used to return Err and abort whole-archive extraction.
    let mut reader = open(BSDTAR_COPY_7Z);
    let count = reader.sevenz_entries().len();
    for index in 0..count {
        let name = reader.sevenz_entries()[index].name.clone();
        let result = reader.extract(index);
        assert!(
            result.is_ok(),
            "extraction aborted on entry {} ({}): {:?}",
            index,
            name,
            result.err()
        );
    }

    let empty_index = reader
        .sevenz_entries()
        .iter()
        .position(|e| e.name == "empty.txt")
        .expect("empty.txt present");
    let data = reader.extract(empty_index).expect("extract empty.txt");
    assert!(
        data.is_empty(),
        "zero-byte member must extract to empty data"
    );
}

#[test]
fn solid_copy_substream_sizes_and_offsets() {
    let reader = open(SOLID_COPY_7Z);
    let entries = reader.sevenz_entries();
    assert_eq!(entries.len(), 3);

    assert_eq!(entries[0].name, "one.txt");
    assert_eq!(entries[0].size, 21);
    assert_eq!(entries[0].offset_in_folder, 0);

    assert_eq!(entries[1].name, "two.txt");
    assert_eq!(entries[1].size, 3);
    assert_eq!(entries[1].offset_in_folder, 21);

    assert_eq!(entries[2].name, "three.txt");
    assert_eq!(entries[2].size, 27);
    assert_eq!(entries[2].offset_in_folder, 24);

    // All three substreams share the single solid folder.
    for entry in entries {
        assert_eq!(entry.folder_index, Some(0));
    }
}

#[test]
fn solid_copy_extracts_byte_exact_in_any_order() {
    let mut reader = open(SOLID_COPY_7Z);
    let expected: [&[u8]; 3] = [
        b"First stream payload\n",
        b"2nd",
        b"The third file body here!!\n",
    ];

    // Reverse order first to make sure offsets are honored without relying
    // on sequential state, then forward order to exercise the folder cache.
    for index in (0..3).rev() {
        let data = reader.extract(index).expect("extract (reverse order)");
        assert_eq!(data, expected[index], "entry {} (reverse)", index);
    }
    for (index, want) in expected.iter().enumerate() {
        let data = reader.extract(index).expect("extract (forward order)");
        assert_eq!(&data, want, "entry {} (forward)", index);
    }
}

#[test]
fn bsdtar_lzma1_listing_and_extraction() {
    let mut reader = open(BSDTAR_LZMA1_7Z);
    assert_bsdtar_listing(&reader);
    assert_bsdtar_extraction(&mut reader);
}

#[test]
fn bsdtar_lzma2_listing_and_extraction() {
    let mut reader = open(BSDTAR_LZMA2_7Z);
    assert_bsdtar_listing(&reader);
    assert_bsdtar_extraction(&mut reader);
}

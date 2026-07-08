//! Interop tests for the CAB (Microsoft Cabinet) reader against real
//! independently generated fixtures, plus a handful of corruption cases.
//!
//! Fixtures were generated once with the independent Python `cabarchive`
//! library (<https://pypi.org/project/cabarchive/>) and embedded under
//! `tests/data/`; the suite never invokes external tools at test time.
//!
//! * `cab_stored.cab` — three files (`hello.txt`, `readme.md`,
//!   `nested/data.bin`) written with `compress=False`, i.e. `CompressionType::None`
//!   (stored) folders. Exercises multi-file, multi-directory-name listing and
//!   byte-exact stored extraction.
//! * `cab_mszip.cab` — two files (`compressed.txt`, `small.txt`) written with
//!   `compress=True`, i.e. MSZIP (`CK`-prefixed raw-deflate) folders.
//!   Exercises the MSZIP decompression path.
//! * `cab_bad_header.cab` — `cab_stored.cab` with its `CFHEADER` magic
//!   (`MSCF`) replaced by `XXXX`. `CabReader::new` must return `Err`.
//! * `cab_truncated.cab` — `cab_stored.cab` truncated to ~55% of its length,
//!   cutting off the `CFDATA` blocks. Header/folder/file-entry parsing
//!   succeeds (still well-formed up to that point) but extraction of any
//!   entry must return `Err` because the data blocks cannot be read in full.

use oxiarc_archive::CabReader;
use std::io::Cursor;

const CAB_STORED: &[u8] = include_bytes!("data/cab_stored.cab");
const CAB_MSZIP: &[u8] = include_bytes!("data/cab_mszip.cab");
const CAB_BAD_HEADER: &[u8] = include_bytes!("data/cab_bad_header.cab");
const CAB_TRUNCATED: &[u8] = include_bytes!("data/cab_truncated.cab");

/// Contents of `nested/data.bin` in `cab_stored.cab`: `(i * 13 + 7) % 256` for
/// `i` in `0..256`.
fn nested_data_bin_bytes() -> Vec<u8> {
    (0u32..256).map(|i| ((i * 13 + 7) % 256) as u8).collect()
}

fn open(bytes: &'static [u8]) -> CabReader<Cursor<&'static [u8]>> {
    CabReader::new(Cursor::new(bytes)).expect("open embedded CAB fixture")
}

#[test]
fn stored_cab_lists_all_entries() {
    let reader = open(CAB_STORED);
    let entries = reader.entries();
    assert_eq!(entries.len(), 3, "expected 3 entries in cab_stored.cab");

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "names: {:?}", names);
    assert!(names.contains(&"readme.md"), "names: {:?}", names);
    assert!(names.contains(&"nested/data.bin"), "names: {:?}", names);
}

#[test]
fn stored_cab_reports_stored_compression_method() {
    use oxiarc_core::CompressionMethod;

    let reader = open(CAB_STORED);
    for entry in reader.entries() {
        assert_eq!(
            entry.method,
            CompressionMethod::Stored,
            "entry {} should be stored (uncompressed)",
            entry.name
        );
    }
}

#[test]
fn stored_cab_extracts_byte_exact() {
    let mut reader = open(CAB_STORED);
    let entries = reader.entries().to_vec();

    let hello = entries
        .iter()
        .find(|e| e.name == "hello.txt")
        .expect("hello.txt present");
    let data = reader.extract(hello).expect("extract hello.txt");
    assert_eq!(data, b"Hello from a real CAB fixture!\n");

    let readme = entries
        .iter()
        .find(|e| e.name == "readme.md")
        .expect("readme.md present");
    let data = reader.extract(readme).expect("extract readme.md");
    assert_eq!(
        data,
        b"# OxiArc CAB interop test fixture\n\nValidates CabReader against cabarchive.py output.\n"
    );

    let nested = entries
        .iter()
        .find(|e| e.name == "nested/data.bin")
        .expect("nested/data.bin present");
    let data = reader.extract(nested).expect("extract nested/data.bin");
    assert_eq!(data, nested_data_bin_bytes());
}

#[test]
fn stored_cab_extract_by_index_matches_extract_by_entry() {
    let mut reader = open(CAB_STORED);
    let entries = reader.entries().to_vec();

    for (index, entry) in entries.iter().enumerate() {
        let by_index = reader
            .extract_by_index(index)
            .unwrap_or_else(|e| panic!("extract_by_index({index}) failed: {e}"));
        let by_entry = reader
            .extract(entry)
            .unwrap_or_else(|e| panic!("extract({}) failed: {e}", entry.name));
        assert_eq!(by_index, by_entry, "entry {} index {}", entry.name, index);
    }
}

#[test]
fn mszip_cab_lists_all_entries() {
    let reader = open(CAB_MSZIP);
    let entries = reader.entries();
    assert_eq!(entries.len(), 2, "expected 2 entries in cab_mszip.cab");

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"compressed.txt"), "names: {:?}", names);
    assert!(names.contains(&"small.txt"), "names: {:?}", names);
}

#[test]
fn mszip_cab_reports_deflate_compression_method() {
    use oxiarc_core::CompressionMethod;

    let reader = open(CAB_MSZIP);
    for entry in reader.entries() {
        assert_eq!(
            entry.method,
            CompressionMethod::Deflate,
            "entry {} should report Deflate (MSZIP) compression",
            entry.name
        );
    }
}

#[test]
fn mszip_cab_extracts_byte_exact() {
    let mut reader = open(CAB_MSZIP);
    let entries = reader.entries().to_vec();

    let compressed = entries
        .iter()
        .find(|e| e.name == "compressed.txt")
        .expect("compressed.txt present");
    let data = reader.extract(compressed).expect("extract compressed.txt");
    let expected = b"This payload should compress nicely with MSZIP deflate. ".repeat(20);
    assert_eq!(data, expected);

    let small = entries
        .iter()
        .find(|e| e.name == "small.txt")
        .expect("small.txt present");
    let data = reader.extract(small).expect("extract small.txt");
    assert_eq!(data, b"tiny");
}

#[test]
fn bad_cfheader_magic_is_rejected() {
    let result = CabReader::new(Cursor::new(CAB_BAD_HEADER));
    assert!(
        result.is_err(),
        "CAB with corrupted CFHEADER magic must be rejected, not silently opened"
    );
}

#[test]
fn truncated_cab_data_blocks_fail_on_extract() {
    // Header/folder/file-entry metadata is still well-formed at the point of
    // truncation, so opening succeeds, but every CFDATA block extends past
    // EOF: extraction of any entry must surface an error instead of
    // returning short/garbage data.
    let mut reader = CabReader::new(Cursor::new(CAB_TRUNCATED))
        .expect("truncated CAB should still parse header/folder/file metadata");
    let entries = reader.entries().to_vec();
    assert!(
        !entries.is_empty(),
        "truncated CAB should still list entries"
    );

    for entry in &entries {
        let result = reader.extract(entry);
        assert!(
            result.is_err(),
            "extracting {} from a truncated CAB must fail, got {:?}",
            entry.name,
            result.ok()
        );
    }
}

#[test]
fn truncated_cab_extract_by_index_also_fails() {
    let mut reader = CabReader::new(Cursor::new(CAB_TRUNCATED)).expect("truncated CAB parses");
    let count = reader.entries().len();
    for index in 0..count {
        let result = reader.extract_by_index(index);
        assert!(
            result.is_err(),
            "extract_by_index({index}) on truncated CAB must fail"
        );
    }
}

#[test]
fn out_of_range_index_is_rejected() {
    let mut reader = open(CAB_STORED);
    let count = reader.entries().len();
    let result = reader.extract_by_index(count + 10);
    assert!(result.is_err(), "out-of-range index must return Err");
}

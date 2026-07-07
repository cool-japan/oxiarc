//! Reverse-direction interop: read genuine, externally-produced `.lzh`
//! archives through `oxiarc-archive`'s real archive reader (`LzhReader` /
//! `LzhHeader`), not the lower-level `oxiarc-lzhuf` codec decode.
//!
//! The fixtures in `oxiarc-lzhuf/tests/data/` are complete `.lzh` archives
//! produced by genuine, independent LHA-family tools (LHa for UNIX 1.14i and
//! LHA 2.55e for DOS) across header levels 0, 1 and 2 — see that directory's
//! `README.md` for full provenance and a sha256 manifest. The
//! `corpus_fixtures.rs` test in `oxiarc-lzhuf` decodes only the raw payload of
//! each; this test instead exercises the *archive layer*: header-level
//! detection, filename / size / CRC-16 metadata parsing, and CRC-verified
//! extraction through `LzhReader`.
//!
//! A byte-exact extraction here (against the paired `*.expected` plaintext) is
//! proof that `oxiarc-archive` reads real-world LZH archive headers correctly,
//! not merely that it is internally self-consistent.

use oxiarc_archive::lzh::{LzhHeader, LzhReader};
use std::io::Cursor;
use std::path::PathBuf;

/// Path to a fixture file inside `oxiarc-lzhuf/tests/data/`. The corpus lives
/// with the codec crate (single source of truth, ~1.3 MB); this sibling crate
/// references it across the workspace rather than duplicating it.
fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../oxiarc-lzhuf/tests/data")
        .join(name)
}

fn read_fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name))
        .unwrap_or_else(|e| panic!("reading corpus fixture {name}: {e}"))
}

/// One corpus entry's ground truth, taken from
/// `oxiarc-lzhuf/tests/data/README.md`.
struct Expected {
    archive: &'static str,
    expected_plaintext: &'static str,
    /// Internal member name as stored by the original encoder.
    member_name: &'static str,
    /// Header level the original tool wrote.
    level: u8,
    /// Method id string (without the surrounding dashes).
    method: &'static str,
    /// Uncompressed size in the base header.
    original_size: u64,
    /// CRC-16 stored in the header (README manifest).
    crc16: u16,
}

const CORPUS: &[Expected] = &[
    Expected {
        archive: "lha_unix114i_h0_lh0.lzh",
        expected_plaintext: "lha_unix114i_h0_lh0.expected",
        member_name: "gpl-2.gz",
        level: 0,
        method: "lh0",
        original_size: 6829,
        crc16: 0xb6d5,
    },
    Expected {
        archive: "lha_unix114i_h0_lh5.lzh",
        expected_plaintext: "lha_unix114i_h0_lh5.expected",
        member_name: "gpl-2",
        level: 0,
        method: "lh5",
        original_size: 18092,
        crc16: 0xa33a,
    },
    Expected {
        archive: "lha_unix114i_h1_lh5.lzh",
        expected_plaintext: "lha_unix114i_h1_lh5.expected",
        member_name: "gpl-2",
        level: 1,
        method: "lh5",
        original_size: 18092,
        crc16: 0xa33a,
    },
    Expected {
        archive: "lha_unix114i_h2_lh5.lzh",
        expected_plaintext: "lha_unix114i_h2_lh5.expected",
        member_name: "gpl-2",
        level: 2,
        method: "lh5",
        original_size: 18092,
        crc16: 0xa33a,
    },
    Expected {
        // DOS-produced archives (OS id 'M') store the name in FAT upper-case;
        // Lhasa lower-cases it only for display. The library faithfully
        // returns the bytes actually stored ("LONG.TXT"), not a cosmetic
        // lower-cased form.
        archive: "lha213_lh5_long.lzh",
        expected_plaintext: "lha213_lh5_long.expected",
        member_name: "LONG.TXT",
        level: 1,
        method: "lh5",
        original_size: 1_241_658,
        crc16: 0x6a7c,
    },
    Expected {
        // Likewise DOS-upper-cased on disk ("GPL-2").
        archive: "lha255e_lh5.lzh",
        expected_plaintext: "lha255e_lh5.expected",
        member_name: "GPL-2",
        level: 1,
        method: "lh5",
        original_size: 18092,
        crc16: 0xa33a,
    },
];

/// Read `archive` through the low-level `LzhHeader` reader and assert the
/// header-level, method, filename, size and CRC-16 metadata all match the
/// externally-known ground truth.
fn assert_header_metadata(exp: &Expected) {
    let bytes = read_fixture(exp.archive);
    let mut cursor = Cursor::new(&bytes);
    let header = LzhHeader::read(&mut cursor, 0)
        .unwrap_or_else(|e| panic!("{}: header read failed: {e}", exp.archive))
        .unwrap_or_else(|| panic!("{}: expected one header, found none", exp.archive));

    assert_eq!(header.level, exp.level, "{}: header level", exp.archive);
    assert_eq!(
        header.method.name(),
        exp.method,
        "{}: method id",
        exp.archive
    );
    assert_eq!(
        header.filename, exp.member_name,
        "{}: member filename",
        exp.archive
    );
    assert_eq!(
        header.original_size as u64, exp.original_size,
        "{}: original (uncompressed) size",
        exp.archive
    );
    assert_eq!(header.crc16, exp.crc16, "{}: header CRC-16", exp.archive);
}

/// Open `archive` through the full `LzhReader`, confirm the single entry's
/// name/size, then CRC-verified-extract it and diff against `*.expected`.
fn assert_reader_extracts_exact(exp: &Expected) {
    let bytes = read_fixture(exp.archive);
    let expected_plaintext = read_fixture(exp.expected_plaintext);

    let mut reader = LzhReader::new(Cursor::new(&bytes))
        .unwrap_or_else(|e| panic!("{}: LzhReader::new failed: {e}", exp.archive));
    let entries = reader.entries();
    assert_eq!(
        entries.len(),
        1,
        "{}: corpus archives are single-entry",
        exp.archive
    );
    assert_eq!(
        entries[0].name, exp.member_name,
        "{}: entry name",
        exp.archive
    );
    assert_eq!(
        entries[0].size, exp.original_size,
        "{}: entry size",
        exp.archive
    );

    // `extract_to_vec` internally recomputes the CRC-16 and errors on mismatch,
    // so a successful extraction is itself a CRC-metadata check.
    let extracted = reader
        .extract_to_vec(&entries[0])
        .unwrap_or_else(|e| panic!("{}: extraction (CRC-16 verified) failed: {e}", exp.archive));

    assert_eq!(
        extracted.len(),
        expected_plaintext.len(),
        "{}: extracted length",
        exp.archive
    );
    if extracted != expected_plaintext {
        let at = extracted
            .iter()
            .zip(expected_plaintext.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(usize::MAX);
        panic!(
            "{}: extracted bytes diverge from {} at offset {at}",
            exp.archive, exp.expected_plaintext
        );
    }
}

#[test]
fn corpus_headers_parse_with_correct_metadata() {
    for exp in CORPUS {
        assert_header_metadata(exp);
    }
}

#[test]
fn corpus_reader_extracts_byte_exact() {
    for exp in CORPUS {
        assert_reader_extracts_exact(exp);
    }
}

#[test]
fn corpus_lh5_variants_extract_to_identical_gpl2() {
    // The four -lh5- archives carrying gpl-2 come from different tools and
    // header levels but must all extract to byte-identical content through the
    // archive reader (independent cross-tool confirmation).
    let names = [
        "lha_unix114i_h0_lh5.lzh",
        "lha_unix114i_h1_lh5.lzh",
        "lha_unix114i_h2_lh5.lzh",
        "lha255e_lh5.lzh",
    ];
    let mut decoded: Vec<Vec<u8>> = Vec::new();
    for name in names {
        let bytes = read_fixture(name);
        let mut reader = LzhReader::new(Cursor::new(&bytes))
            .unwrap_or_else(|e| panic!("{name}: open failed: {e}"));
        let entries = reader.entries();
        decoded.push(
            reader
                .extract_to_vec(&entries[0])
                .unwrap_or_else(|e| panic!("{name}: extract failed: {e}")),
        );
    }
    for other in &decoded[1..] {
        assert_eq!(&decoded[0], other, "all -lh5- gpl-2 variants must match");
    }
}

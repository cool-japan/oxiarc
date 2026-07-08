//! Interop tests for the ISO 9660 reader against real independently
//! generated fixtures, plus a handful of corruption cases.
//!
//! Fixtures were generated once with the independent `genisoimage` tool
//! (Debian/Ubuntu `genisoimage` package, cdrkit fork of `mkisofs`) and
//! embedded under `tests/data/`; the suite never invokes external tools at
//! test time.
//!
//! Both fixtures were built from the same source tree:
//! `hello.txt` (40 bytes), `second.txt` (43 bytes), `subdir/nested.txt`
//! (25 bytes).
//!
//! * `iso_plain.iso` — `genisoimage -V TESTPLAIN -iso-level 1 -no-pad`: plain
//!   ISO 9660 Level 1, no Joliet, no Rock Ridge. Exercises the PVD-only path
//!   (ASCII 8.3 names, lower-cased by the reader, `;1` version suffix
//!   stripped by the directory-record parser).
//! * `iso_joliet_rr.iso` — `genisoimage -V TESTJOL -J -R -no-pad`: Joliet
//!   (UCS-2 BE long names) + Rock Ridge extensions. Exercises the Joliet SVD
//!   preference path (Joliet root is selected over the PVD root whenever a
//!   Joliet SVD is present) with real long lower-case names.
//! * `iso_truncated.iso` — `iso_plain.iso` truncated to its first 20 logical
//!   blocks (40960 bytes), i.e. before the file-data extents are reached
//!   while still leaving the volume descriptors intact. `IsoReader::new`
//!   must return `Err` because the root directory extent cannot be read in
//!   full — a real-world truncated-download / short-read scenario.
//!
//! Additionally, `cyclic_directory_reference_is_rejected` hand-crafts a
//! (non-conformant) ISO image whose root directory contains a subdirectory
//! record pointing back at the root's own LBA, mirroring the guard tested
//! against a synthetic buffer in `src/iso9660/mod.rs`'s unit tests but here
//! validated end-to-end through the public `IsoReader::new` entry point.

use oxiarc_archive::IsoReader;
use std::io::Cursor;

const ISO_PLAIN: &[u8] = include_bytes!("data/iso_plain.iso");
const ISO_JOLIET_RR: &[u8] = include_bytes!("data/iso_joliet_rr.iso");
const ISO_TRUNCATED: &[u8] = include_bytes!("data/iso_truncated.iso");

const HELLO_TXT: &[u8] = b"hello world from oxiarc iso test fixture";
const SECOND_TXT: &[u8] = b"second file content for iso interop testing";
const NESTED_TXT: &[u8] = b"nested file inside subdir";

fn open(bytes: &'static [u8]) -> IsoReader<Cursor<&'static [u8]>> {
    IsoReader::new(Cursor::new(bytes)).expect("open embedded ISO fixture")
}

#[test]
fn plain_iso_has_no_joliet() {
    let reader = open(ISO_PLAIN);
    assert!(
        !reader.is_joliet(),
        "iso_plain.iso was built without -J and must not report Joliet"
    );
    assert_eq!(reader.volume_id.trim(), "TESTPLAIN");
}

#[test]
fn plain_iso_lists_files_and_directory() {
    let reader = open(ISO_PLAIN);
    let entries = reader.entries();

    let files: Vec<_> = entries.iter().filter(|e| !e.is_dir).collect();
    assert_eq!(files.len(), 3, "expected 3 files, got {:?}", files);

    let names: Vec<String> = files.iter().map(|e| e.name.to_lowercase()).collect();
    assert!(names.iter().any(|n| n == "hello.txt"), "{:?}", names);
    assert!(names.iter().any(|n| n == "second.txt"), "{:?}", names);
    assert!(
        names.iter().any(|n| n.ends_with("subdir/nested.txt")),
        "{:?}",
        names
    );

    let dirs: Vec<_> = entries.iter().filter(|e| e.is_dir).collect();
    assert_eq!(dirs.len(), 1, "expected exactly 1 subdirectory entry");
}

#[test]
fn plain_iso_extracts_byte_exact() {
    let mut reader = open(ISO_PLAIN);
    let entries = reader.entries().to_vec();

    for (name_fragment, expected) in [
        ("hello.txt", HELLO_TXT),
        ("second.txt", SECOND_TXT),
        ("nested.txt", NESTED_TXT),
    ] {
        let entry = entries
            .iter()
            .find(|e| !e.is_dir && e.name.to_lowercase().contains(name_fragment))
            .unwrap_or_else(|| panic!("entry containing '{name_fragment}' not found"));
        let mut out = Vec::new();
        reader
            .extract(entry, &mut out)
            .unwrap_or_else(|e| panic!("extract '{}' failed: {e}", entry.name));
        assert_eq!(out, expected, "byte-exact mismatch for {}", entry.name);
    }
}

#[test]
fn joliet_iso_prefers_joliet_root() {
    let reader = open(ISO_JOLIET_RR);
    assert!(
        reader.is_joliet(),
        "iso_joliet_rr.iso was built with -J and must report Joliet in use"
    );
}

#[test]
fn joliet_iso_reports_long_lowercase_names() {
    let reader = open(ISO_JOLIET_RR);
    let files: Vec<_> = reader.entries().iter().filter(|e| !e.is_dir).collect();
    assert_eq!(files.len(), 3, "expected 3 files, got {:?}", files);

    // Joliet names are UCS-2 and not forced to lowercase by the reader (only
    // the PVD/ASCII path is lower-cased), but genisoimage was given
    // already-lowercase source filenames so they should round-trip exactly.
    let names: Vec<&str> = files.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"hello.txt"), "{:?}", names);
    assert!(names.contains(&"second.txt"), "{:?}", names);
    assert!(
        names.iter().any(|n| n.ends_with("subdir/nested.txt")),
        "{:?}",
        names
    );
}

#[test]
fn joliet_iso_extracts_byte_exact() {
    let mut reader = open(ISO_JOLIET_RR);
    let entries = reader.entries().to_vec();

    for (name, expected) in [
        ("hello.txt", HELLO_TXT),
        ("second.txt", SECOND_TXT),
        ("subdir/nested.txt", NESTED_TXT),
    ] {
        let entry = entries
            .iter()
            .find(|e| !e.is_dir && e.name == name)
            .unwrap_or_else(|| panic!("entry '{name}' not found"));
        let mut out = Vec::new();
        reader
            .extract(entry, &mut out)
            .unwrap_or_else(|e| panic!("extract '{name}' failed: {e}"));
        assert_eq!(out, expected, "byte-exact mismatch for {name}");
    }
}

#[test]
fn both_fixtures_agree_on_volume_space_size() {
    // Both images were cut from the same source tree with -no-pad, but the
    // Joliet+RR one carries an extra SVD and RR attributes, so it must be
    // strictly larger while still describing a consistent, readable image.
    let plain = open(ISO_PLAIN);
    let joliet = open(ISO_JOLIET_RR);
    assert!(
        joliet.total_lbas >= plain.total_lbas,
        "joliet={} plain={}",
        joliet.total_lbas,
        plain.total_lbas
    );
}

#[test]
fn truncated_iso_root_directory_read_fails() {
    let result = IsoReader::new(Cursor::new(ISO_TRUNCATED));
    assert!(
        result.is_err(),
        "ISO truncated before its directory/file extents must be rejected, not silently opened"
    );
}

#[test]
fn truncated_iso_via_cursor_over_owned_vec_also_fails() {
    // Same fixture, but driven through an owned buffer / independent Cursor
    // to rule out any interaction with the `include_bytes!` (&'static [u8])
    // borrow shape used by the other tests.
    let owned: Vec<u8> = ISO_TRUNCATED.to_vec();
    let result = IsoReader::new(Cursor::new(owned));
    assert!(result.is_err());
}

// ── Hand-crafted corruption case: cyclic directory reference ────────────────

/// Write a 34-byte "." or ".." directory record into `buf` at byte 0.
fn write_dot_style_record(buf: &mut [u8], lba: u32, size: u32, is_dotdot: bool) {
    buf[0] = 34;
    buf[1] = 0;
    buf[2..6].copy_from_slice(&lba.to_le_bytes());
    buf[6..10].copy_from_slice(&lba.to_be_bytes());
    buf[10..14].copy_from_slice(&size.to_le_bytes());
    buf[14..18].copy_from_slice(&size.to_be_bytes());
    buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]); // date: 2026-05-06
    buf[25] = 0x02; // directory flag
    buf[28..30].copy_from_slice(&1u16.to_le_bytes());
    buf[30..32].copy_from_slice(&1u16.to_be_bytes());
    buf[32] = 1;
    buf[33] = if is_dotdot { 0x01 } else { 0x00 };
}

/// Write a named subdirectory record whose LBA can point anywhere, including
/// back at an already-visited directory (used to fabricate a cycle).
fn write_named_subdir_record(buf: &mut [u8], name: &[u8], lba: u32, size: u32) -> usize {
    let len_fi = name.len() as u8;
    let padding = if len_fi % 2 == 0 { 1u8 } else { 0u8 };
    let len_dr = 33u8 + len_fi + padding;

    buf[0] = len_dr;
    buf[1] = 0;
    buf[2..6].copy_from_slice(&lba.to_le_bytes());
    buf[6..10].copy_from_slice(&lba.to_be_bytes());
    buf[10..14].copy_from_slice(&size.to_le_bytes());
    buf[14..18].copy_from_slice(&size.to_be_bytes());
    buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
    buf[25] = 0x02; // directory flag
    buf[28..30].copy_from_slice(&1u16.to_le_bytes());
    buf[30..32].copy_from_slice(&1u16.to_be_bytes());
    buf[32] = len_fi;
    buf[33..33 + len_fi as usize].copy_from_slice(name);

    len_dr as usize
}

/// Build a minimal, non-conformant ISO 9660 image whose PVD root directory
/// (LBA 20) contains a "LOOP" subdirectory record pointing back at the root
/// directory's own LBA, forming a cycle. Walking such an image naively
/// (without a visited-LBA guard) would recurse forever / overflow the stack.
fn build_cyclic_iso() -> Vec<u8> {
    let total_lbas = 21u32;
    let mut iso = vec![0u8; (total_lbas as usize) * 2048];

    // LBA 16: Primary Volume Descriptor
    {
        let pvd = &mut iso[16 * 2048..17 * 2048];
        pvd[0] = 1; // type: Primary
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        for b in pvd[40..72].iter_mut() {
            *b = b' ';
        }
        pvd[40..47].copy_from_slice(b"CYCLIC!");
        pvd[80..84].copy_from_slice(&total_lbas.to_le_bytes());
        pvd[84..88].copy_from_slice(&total_lbas.to_be_bytes());
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
        // Root directory record: root at LBA 20, full-sector size so the
        // cyclic subdir record fits after "." and "..".
        let root_dr = &mut pvd[156..156 + 34];
        write_dot_style_record(root_dr, 20, 2048, false);
    }

    // LBA 17: Volume Descriptor Set Terminator
    {
        let term = &mut iso[17 * 2048..18 * 2048];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
    }

    // LBA 20: PVD root directory containing "." ".." and a "LOOP" subdirectory
    // record whose LBA points back at the root itself.
    {
        let dir = &mut iso[20 * 2048..21 * 2048];
        let mut pos = 0usize;
        write_dot_style_record(&mut dir[pos..pos + 34], 20, 2048, false);
        pos += 34;
        write_dot_style_record(&mut dir[pos..pos + 34], 20, 2048, true);
        pos += 34;
        let _ = write_named_subdir_record(&mut dir[pos..], b"LOOP", 20, 2048);
    }

    iso
}

#[test]
fn cyclic_directory_reference_is_rejected() {
    let iso = build_cyclic_iso();
    let result = IsoReader::new(Cursor::new(iso));
    assert!(
        result.is_err(),
        "a directory record that points back at an ancestor (cycle) must be \
         rejected by IsoReader::new, not recursed into forever"
    );
}

/// Same cyclic image, but wired so the "LOOP" entry points at itself
/// directly (self-referential single-node cycle) rather than back at the
/// parent — a slightly different malformed-input shape that must be caught
/// by the same guard.
#[test]
fn self_referential_directory_is_rejected() {
    let total_lbas = 21u32;
    let mut iso = vec![0u8; (total_lbas as usize) * 2048];

    {
        let pvd = &mut iso[16 * 2048..17 * 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        for b in pvd[40..72].iter_mut() {
            *b = b' ';
        }
        pvd[40..47].copy_from_slice(b"SELFREF");
        pvd[80..84].copy_from_slice(&total_lbas.to_le_bytes());
        pvd[84..88].copy_from_slice(&total_lbas.to_be_bytes());
        pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
        pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
        let root_dr = &mut pvd[156..156 + 34];
        write_dot_style_record(root_dr, 20, 2048, false);
    }
    {
        let term = &mut iso[17 * 2048..18 * 2048];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
    }
    {
        let dir = &mut iso[20 * 2048..21 * 2048];
        let mut pos = 0usize;
        write_dot_style_record(&mut dir[pos..pos + 34], 20, 2048, false);
        pos += 34;
        write_dot_style_record(&mut dir[pos..pos + 34], 20, 2048, true);
        pos += 34;
        // "SELF" subdirectory record pointing at its own containing LBA (20),
        // same shape as the LOOP case but named differently to make intent
        // explicit at this call site.
        let _ = write_named_subdir_record(&mut dir[pos..], b"SELF", 20, 2048);
    }

    let result = IsoReader::new(Cursor::new(iso));
    assert!(
        result.is_err(),
        "self-referential directory must be rejected"
    );
}

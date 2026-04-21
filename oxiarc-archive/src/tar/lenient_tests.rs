//! Lenient-mode recovery tests for [`super::TarReader`].
//!
//! Strict mode preserves the legacy behavior of aborting
//! [`super::TarReader::new`] on the first parse error; lenient mode
//! (via [`super::TarReader::new_lenient`]) skips corrupt 512-byte
//! header blocks and records a [`crate::lenient::LenientWarning`] for
//! each anomaly.

#![cfg(test)]

use super::{BLOCK_SIZE, TarReader, TarWriter};
use crate::lenient::LenientWarningKind;
use oxiarc_core::error::OxiArcError;
use std::io::Cursor;

/// Build a minimal single-entry TAR archive (one header + data padded
/// to block boundary + two EOA zero blocks) for corruption tests.
/// Returns the raw byte vector.
fn build_single_entry_tar(name: &str, data: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let mut writer = TarWriter::new(&mut output);
        writer
            .add_file(name, data)
            .expect("add_file in build_single_entry_tar");
        writer.finish().expect("finish in build_single_entry_tar");
    }
    output
}

/// Corrupt the first header block's checksum by XOR-ing the first
/// checksum byte with 0xFF. This guarantees the checksum no longer
/// matches the byte-sum of the rest of the block.
fn corrupt_first_header_checksum(archive: &mut [u8]) {
    // Checksum field lives at offsets 148..156. Flip the first digit's
    // high bit.
    archive[148] ^= 0xFF;
}

#[test]
fn test_tar_lenient_header_checksum() {
    // Two entries so that scan recovery has something to resume on.
    let mut archive = Vec::new();
    {
        let mut writer = TarWriter::new(&mut archive);
        writer
            .add_file("first.txt", b"first entry content")
            .expect("add_file first");
        writer
            .add_file("second.txt", b"second entry content")
            .expect("add_file second");
        writer.finish().expect("finish");
    }

    // Corrupt the FIRST header's checksum. The reader must skip
    // forward and find the second header intact.
    //
    // Note: we can't corrupt the second header instead, because the
    // scan would then have no surviving entry to anchor on — we'd get
    // zero entries, which is still legitimate behavior but makes the
    // test less interesting.
    corrupt_first_header_checksum(&mut archive);

    // Strict: must return InvalidHeader.
    {
        let cursor = Cursor::new(archive.clone());
        let err = TarReader::new(cursor)
            .err()
            .expect("strict new must fail on corrupted checksum");
        match err {
            OxiArcError::InvalidHeader { .. } => {}
            other => panic!("unexpected error variant: {:?}", other),
        }
    }

    // Lenient: should skip the corrupted block, locate the second
    // header, and record a warning.
    {
        let cursor = Cursor::new(archive);
        let reader = TarReader::new_lenient(cursor).expect("new_lenient must succeed");
        let entries = reader.entries();
        assert_eq!(
            entries.len(),
            1,
            "lenient scan must recover second entry after skipping first"
        );
        assert_eq!(entries[0].name, "second.txt");

        let warnings = reader.warnings();
        assert!(
            !warnings.is_empty(),
            "expected at least one warning from lenient scan"
        );
        // At least one HeaderChecksumMismatch and one ScannedForward
        // warning (usually both).
        assert!(
            warnings
                .iter()
                .any(|w| matches!(w.kind, LenientWarningKind::HeaderChecksumMismatch)),
            "expected a HeaderChecksumMismatch warning"
        );
    }
}

#[test]
fn test_tar_lenient_scan_recovery() {
    // Build: 3 good entries + 1 block of garbage + 2 good entries.
    // The lenient scan should emit all 5 surviving entries.
    let mut archive = Vec::new();
    {
        let mut writer = TarWriter::new(&mut archive);
        for name in &["a.txt", "b.txt", "c.txt"] {
            writer
                .add_file(name, format!("body of {}", name).as_bytes())
                .expect("add_file good-1");
        }
        writer.into_inner().expect("into_inner");
    }

    // Strip the trailing two zero blocks so we can append more entries
    // after garbage.
    assert!(archive.len() >= BLOCK_SIZE * 2);
    archive.truncate(archive.len() - BLOCK_SIZE * 2);

    // Inject a block of non-zero garbage. We use 0xAA so the checksum
    // will be a large non-zero value that the stored (zero) checksum
    // bytes cannot match.
    archive.extend_from_slice(&[0xAAu8; BLOCK_SIZE]);

    // Append 2 more good entries + EOA.
    let mut tail = Vec::new();
    {
        let mut writer = TarWriter::new(&mut tail);
        for name in &["d.txt", "e.txt"] {
            writer
                .add_file(name, format!("body of {}", name).as_bytes())
                .expect("add_file good-2");
        }
        writer.finish().expect("finish");
    }
    archive.extend_from_slice(&tail);

    // Lenient scan must find all 5 entries.
    let cursor = Cursor::new(archive);
    let reader = TarReader::new_lenient(cursor).expect("new_lenient");
    let entries = reader.entries();

    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"],
        "all 5 entries must be recovered by lenient scan"
    );

    let warnings = reader.warnings();
    assert!(!warnings.is_empty(), "expected lenient warnings");
    assert!(
        warnings
            .iter()
            .any(|w| matches!(w.kind, LenientWarningKind::ScannedForward { .. })),
        "expected a ScannedForward warning"
    );
}

/// Sanity-check that `build_single_entry_tar` produces an archive
/// that the strict reader accepts. Guards against silent breakage of
/// the fixture helper that other corruption tests depend on.
#[test]
fn test_tar_fixture_builder_roundtrips() {
    let archive = build_single_entry_tar("one.txt", b"alpha");
    let cursor = Cursor::new(archive);
    let reader = TarReader::new(cursor).expect("strict new on intact archive");
    assert_eq!(reader.entries().len(), 1);
    assert_eq!(reader.entries()[0].name, "one.txt");
}

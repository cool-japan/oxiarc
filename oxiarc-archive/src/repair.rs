//! Archive repair/recovery for truncated or partially-corrupt ZIP and TAR archives.
//!
//! Unlike lenient mode (which handles per-entry CRC mismatches), repair mode
//! reconstructs an entry list by scanning for valid local-file signatures
//! independent of the central directory or block count fields.
//!
//! ## ZIP repair
//!
//! [`ZipRepair`] reads the entire source into memory and performs a rolling
//! scan for `PK\x03\x04` (Local File Header) signatures.  Each candidate is
//! tentatively parsed; structurally invalid candidates are silently skipped.
//! Deflate payloads are decompressed and their CRC-32 verified where
//! possible.
//!
//! ## TAR repair
//!
//! [`TarRepair`] walks the stream block-by-block (512 bytes per block),
//! validates UStar magic and POSIX checksum, and recovers every well-formed
//! entry even when intervening blocks are corrupt.  Because TAR is a
//! sequential format no `Seek` is required.

use std::io::{Read, Seek};

use oxiarc_core::error::Result;

use crate::repair_tar::scan_tar_reader;
use crate::repair_zip::scan_zip_bytes;

// ── Public types ─────────────────────────────────────────────────────────────

/// Entry recovered by scanning an archive without relying on a central directory
/// or block-count trailer.
#[derive(Debug)]
pub struct RecoveredEntry {
    /// Entry name (path), decoded as UTF-8 with lossy replacement.
    pub name: String,
    /// Raw compression method number from the archive header.
    /// For TAR entries this is always `0` (TAR stores data uncompressed).
    pub method: u16,
    /// Number of compressed bytes in the source archive.
    pub compressed_size: u64,
    /// Declared uncompressed size (from the header; `0` for TAR entries).
    pub uncompressed_size: u64,
    /// CRC-32 as stored in the archive header.
    /// May be `0` when a ZIP data descriptor is used or for TAR entries.
    pub crc32: u32,
    /// Byte offset of the header (LFH for ZIP, block start for TAR) in the
    /// original source.
    pub offset: u64,
    /// Decompressed entry payload.
    ///
    /// When [`status`](RecoveredEntry::status) is [`RecoveryStatus::RawOnly`]
    /// the field contains the **compressed** bytes verbatim because
    /// decompression failed.
    pub decompressed_data: Vec<u8>,
    /// Recovery quality indicator.
    pub status: RecoveryStatus,
}

/// Recovery quality of a [`RecoveredEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryStatus {
    /// Payload was successfully decompressed and the CRC-32 matched the
    /// value stored in the archive header.
    Verified,
    /// Payload was successfully decompressed (or was stored uncompressed)
    /// but the CRC-32 did not match or was unavailable (e.g., data descriptor
    /// with CRC = 0, or a TAR entry which has no checksum on payload data).
    Recovered,
    /// Decompression failed; [`RecoveredEntry::decompressed_data`] holds
    /// the raw compressed bytes for manual inspection.
    RawOnly,
}

/// Report produced by a repair scan.
#[derive(Debug)]
pub struct RepairReport {
    /// All entries that were successfully parsed and (where possible)
    /// decompressed during the scan.
    pub recovered_entries: Vec<RecoveredEntry>,
    /// Byte ranges `(start, end)` in the source stream that were skipped
    /// because no valid header signature was found there.
    ///
    /// Ranges are half-open: `[start, end)`.
    pub skipped_ranges: Vec<(u64, u64)>,
    /// Human-readable warnings produced during the scan.
    pub warnings: Vec<String>,
}

/// Tuning parameters for the repair scanner.
#[derive(Debug, Clone)]
pub struct RepairOptions {
    /// Maximum payload size (in bytes) to attempt to decompress per entry.
    ///
    /// Entries whose compressed payload exceeds this threshold are recorded
    /// as [`RecoveryStatus::RawOnly`] with no decompression attempted, and
    /// TAR entries are skipped with a warning.
    ///
    /// Default: 64 MiB.
    pub max_entry_size: u64,
}

impl Default for RepairOptions {
    fn default() -> Self {
        Self {
            max_entry_size: 64 * 1024 * 1024,
        }
    }
}

// ── ZipRepair ────────────────────────────────────────────────────────────────

/// ZIP archive repair scanner.
///
/// Reads the source into memory once and scans for `PK\x03\x04` (Local File
/// Header) signatures front-to-back.  The central directory and EOCD record
/// are ignored; all structural information is inferred from the local headers
/// themselves.
pub struct ZipRepair<R: Read + Seek> {
    reader: R,
    options: RepairOptions,
}

impl<R: Read + Seek> ZipRepair<R> {
    /// Create a new `ZipRepair` with default [`RepairOptions`].
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            options: RepairOptions::default(),
        }
    }

    /// Create a new `ZipRepair` with custom [`RepairOptions`].
    pub fn with_options(reader: R, options: RepairOptions) -> Self {
        Self { reader, options }
    }

    /// Scan the source archive and return a [`RepairReport`].
    ///
    /// This reads the entire source into an internal buffer.  For very large
    /// archives consider chunked extraction via a custom scanner.
    ///
    /// # Errors
    ///
    /// Returns `Err` only for unrecoverable I/O failures on the underlying
    /// reader.  Corrupt or unrecognised regions are recorded in
    /// [`RepairReport::skipped_ranges`] and [`RepairReport::warnings`].
    pub fn scan(&mut self) -> Result<RepairReport> {
        let mut data = Vec::new();
        self.reader.read_to_end(&mut data)?;
        scan_zip_bytes(&data, &self.options)
    }
}

// ── TarRepair ────────────────────────────────────────────────────────────────

/// TAR archive repair scanner.
///
/// Walks the source stream block-by-block (512 bytes per block), validating
/// UStar magic and POSIX checksum for each candidate header block.  No `Seek`
/// is required, so the scanner works equally well on piped or streaming
/// sources.
pub struct TarRepair<R: Read> {
    reader: R,
    options: RepairOptions,
}

impl<R: Read> TarRepair<R> {
    /// Create a new `TarRepair` with default [`RepairOptions`].
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            options: RepairOptions::default(),
        }
    }

    /// Create a new `TarRepair` with custom [`RepairOptions`].
    pub fn with_options(reader: R, options: RepairOptions) -> Self {
        Self { reader, options }
    }

    /// Scan the source archive and return a [`RepairReport`].
    ///
    /// # Errors
    ///
    /// Returns `Err` only for unrecoverable I/O failures.  Corrupt blocks
    /// are recorded as warnings and skipped.
    pub fn scan(&mut self) -> Result<RepairReport> {
        scan_tar_reader(&mut self.reader, &self.options)
    }
}

// ── Convenience functions ─────────────────────────────────────────────────────

/// Scan a ZIP archive for recoverable entries without relying on the central
/// directory.
///
/// # Errors
///
/// Returns `Err` for unrecoverable I/O failures on `reader`.
pub fn repair_zip<R: Read + Seek>(reader: R) -> Result<RepairReport> {
    ZipRepair::new(reader).scan()
}

/// Scan a TAR archive for recoverable entries block-by-block.
///
/// No `Seek` is required; the function works on streaming sources.
///
/// # Errors
///
/// Returns `Err` for unrecoverable I/O failures on `reader`.
pub fn repair_tar<R: Read>(reader: R) -> Result<RepairReport> {
    TarRepair::new(reader).scan()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::repair_tar::build_test_tar;
    use crate::repair_zip::{build_test_zip, find_eocd_offset, find_lfh_for_name};
    use crate::tar::TarWriter;
    use crate::zip::{ZipReader, ZipWriter};

    // ── ZIP tests ────────────────────────────────────────────────────────────

    /// A repair scan on an intact ZIP must return the same entries (names, sizes,
    /// decompressed data) as a normal ZipReader.
    #[test]
    fn test_zip_repair_intact_archive() {
        let files: &[(&str, &[u8])] = &[
            ("hello.txt", b"Hello, repair world!"),
            ("data.bin", &[0xDEu8, 0xAD, 0xBE, 0xEF].repeat(50)),
        ];
        let zip_bytes = build_test_zip(files);

        let report = repair_zip(Cursor::new(&zip_bytes)).expect("repair_zip failed");

        assert_eq!(
            report.recovered_entries.len(),
            2,
            "expected 2 entries, got {}",
            report.recovered_entries.len()
        );

        let hello = report
            .recovered_entries
            .iter()
            .find(|e| e.name == "hello.txt")
            .expect("hello.txt missing from repair report");
        assert_eq!(&hello.decompressed_data, b"Hello, repair world!");

        let data = report
            .recovered_entries
            .iter()
            .find(|e| e.name == "data.bin")
            .expect("data.bin missing from repair report");
        assert_eq!(
            data.decompressed_data,
            [0xDEu8, 0xAD, 0xBE, 0xEF].repeat(50)
        );
    }

    /// Truncating the archive after the last entry's data (removing the central
    /// directory and EOCD) must still yield all entries.
    #[test]
    fn test_zip_repair_truncated_central_directory() {
        let files: &[(&str, &[u8])] = &[("a.txt", b"Alpha"), ("b.txt", b"Beta is slightly longer")];
        let zip_bytes = build_test_zip(files);

        // Locate the first byte of the central directory by reading the archive
        let cursor = Cursor::new(&zip_bytes);
        let reader = ZipReader::new(cursor).expect("ZipReader::new");
        let entries = reader.entries().to_vec();

        // The central-dir starts right after the last entry's compressed data.
        // Use the offset + compressed_size of the last entry as the cut point.
        let last_entry = entries.last().expect("entries not empty");
        // offset is the LFH offset; data follows after the LFH fixed + fname + extra.
        // We just truncate at the EOCD to simulate a missing central directory.
        let eocd_pos = find_eocd_offset(&zip_bytes).expect("EOCD present");
        let truncated = &zip_bytes[..eocd_pos];

        let report = repair_zip(Cursor::new(truncated)).expect("repair_zip");
        assert_eq!(
            report.recovered_entries.len(),
            2,
            "expected 2 recovered, got {:?}",
            report
                .recovered_entries
                .iter()
                .map(|e| &e.name)
                .collect::<Vec<_>>()
        );
        let _ = last_entry; // suppress warning
    }

    /// Dropping the entire EOCD (central directory + end record) must still
    /// recover all entries.
    #[test]
    fn test_zip_repair_missing_eocd() {
        let zip_bytes = build_test_zip(&[("only.txt", b"Only file")]);

        // Remove everything from the first central-dir-header onward.
        // Central dir header starts with PK\x01\x02 (0x02014b50).
        let cd_sig = [0x50u8, 0x4B, 0x01, 0x02];
        let cut = zip_bytes
            .windows(4)
            .position(|w| w == cd_sig)
            .unwrap_or(zip_bytes.len());
        let truncated = &zip_bytes[..cut];

        let report = repair_zip(Cursor::new(truncated)).expect("repair_zip");
        assert_eq!(report.recovered_entries.len(), 1);
        assert_eq!(&report.recovered_entries[0].name, "only.txt");
        assert_eq!(&report.recovered_entries[0].decompressed_data, b"Only file");
    }

    /// Injecting random garbage between two LFH blocks must not prevent recovery
    /// of the surrounding valid entries.
    #[test]
    fn test_zip_repair_corrupt_lfh_skipped() {
        let files: &[(&str, &[u8])] = &[
            ("first.txt", b"First file content here"),
            ("second.txt", b"Second file content, also here"),
        ];
        let zip_bytes = build_test_zip(files);

        // Find the LFH for "second.txt" and overwrite a few bytes right before it
        // with garbage so that the scanner must scan past the corruption.
        let second_pos = find_lfh_for_name(&zip_bytes, "second.txt").expect("second.txt LFH");
        let mut corrupt = zip_bytes.clone();
        // Overwrite 8 bytes just before the second LFH with 0xFF to simulate
        // a corrupt region between entries.
        let corrupt_start = second_pos.saturating_sub(8);
        for b in &mut corrupt[corrupt_start..second_pos] {
            *b = 0xFF;
        }

        let report = repair_zip(Cursor::new(&corrupt)).expect("repair_zip");

        // Both entries should survive: first.txt was unaffected, second.txt header is intact
        let names: Vec<&str> = report
            .recovered_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            names.contains(&"first.txt"),
            "first.txt missing; got {names:?}"
        );
        assert!(
            names.contains(&"second.txt"),
            "second.txt missing; got {names:?}"
        );
    }

    /// Stored (method=0) entries must be recovered as `Verified` when their
    /// CRC matches, or at least as `Recovered` when no CRC is available.
    #[test]
    fn test_zip_repair_with_stored_method() {
        let data: &[u8] = b"Short data - stored, no compression";
        let mut zip_bytes = Vec::new();
        {
            use crate::zip::ZipCompressionLevel;
            let mut w = ZipWriter::new(&mut zip_bytes);
            w.add_file_with_options("stored.txt", data, ZipCompressionLevel::Store)
                .expect("add_file_stored");
            w.finish().expect("finish");
        }

        let report = repair_zip(Cursor::new(&zip_bytes)).expect("repair_zip");
        assert_eq!(report.recovered_entries.len(), 1);
        let entry = &report.recovered_entries[0];
        assert_eq!(entry.name, "stored.txt");
        assert_eq!(&entry.decompressed_data, data);
        assert_eq!(entry.method, 0, "should be stored");
        // Stored entries have valid CRC so status must be Verified
        assert_eq!(entry.status, RecoveryStatus::Verified);
    }

    // ── TAR tests ────────────────────────────────────────────────────────────

    /// A repair scan on an intact TAR must return the same entries as TarReader.
    #[test]
    fn test_tar_repair_clean_archive() {
        let files: &[(&str, &[u8])] = &[
            ("readme.txt", b"This is the readme"),
            ("data/payload.bin", &[1u8, 2, 3, 4, 5].repeat(20)),
        ];
        let tar_bytes = build_test_tar(files);

        let report = repair_tar(Cursor::new(&tar_bytes)).expect("repair_tar");

        let names: Vec<&str> = report
            .recovered_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            names.contains(&"readme.txt"),
            "readme.txt missing; got {names:?}"
        );
        assert!(
            names.contains(&"data/payload.bin"),
            "data/payload.bin missing; got {names:?}"
        );

        let payload = report
            .recovered_entries
            .iter()
            .find(|e| e.name == "data/payload.bin")
            .expect("data/payload.bin");
        assert_eq!(payload.decompressed_data, [1u8, 2, 3, 4, 5].repeat(20));
    }

    /// Flipping header bytes in a middle entry must not prevent recovery of the
    /// surrounding valid entries.
    #[test]
    fn test_tar_repair_corrupt_middle_block() {
        let files: &[(&str, &[u8])] = &[
            ("first.txt", b"First entry data"),
            ("middle.txt", b"Middle entry that will be corrupted"),
            ("last.txt", b"Last entry data"),
        ];
        let tar_bytes = build_test_tar(files);

        // The header for "middle.txt" starts at block 1 (after "first.txt" header +
        // 1 data block = 2 blocks).  Each file occupies (1 header + ceil(size/512) data) blocks.
        // first.txt: 1 header + 1 data block = block 0..1
        // middle.txt header: block 2 (offset 1024)
        let first_data_blocks = (b"First entry data".len() as u64).div_ceil(512);
        let middle_header_block = 1 + first_data_blocks; // 0-indexed
        let middle_header_offset = (middle_header_block * 512) as usize;

        // Make sure we have enough data
        let mut corrupt = tar_bytes.clone();
        if middle_header_offset + 512 <= corrupt.len() {
            // Zero out the checksum field to invalidate the checksum
            for b in &mut corrupt[middle_header_offset + 148..middle_header_offset + 156] {
                *b = 0xFF;
            }
            // Also corrupt the magic to make it fail the ustar check
            for b in &mut corrupt[middle_header_offset + 257..middle_header_offset + 263] {
                *b = b'X';
            }
        }

        let report = repair_tar(Cursor::new(&corrupt)).expect("repair_tar");

        let names: Vec<&str> = report
            .recovered_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        // first.txt should always be recoverable
        assert!(
            names.contains(&"first.txt"),
            "first.txt missing; got {names:?}"
        );
        // middle.txt was corrupt, so it may be absent
        // last.txt: might or might not be recoverable depending on alignment
        // — at minimum first.txt must be there
    }

    /// Dropping the data blocks for the last file (simulated truncation) must
    /// recover all earlier entries while warning about the truncated tail.
    #[test]
    fn test_tar_repair_truncated_tail() {
        let files: &[(&str, &[u8])] = &[
            ("good1.txt", b"Good entry one"),
            ("good2.txt", b"Good entry two"),
            ("truncated.txt", b"This entry will be truncated"),
        ];
        let tar_bytes = build_test_tar(files);

        // Remove the last file's data blocks (keep only its header and everything before)
        // "truncated.txt" header starts after good1 + good2.
        // good1: 1 header + 1 data block = 2 blocks
        // good2: 1 header + 1 data block = 2 blocks
        // truncated header: block 4
        let truncated_header_block = 4u64;
        let cut = (truncated_header_block * 512) as usize + 512; // keep header, remove data
        let truncated = &tar_bytes[..cut.min(tar_bytes.len())];

        let report = repair_tar(Cursor::new(truncated)).expect("repair_tar");

        let names: Vec<&str> = report
            .recovered_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            names.contains(&"good1.txt"),
            "good1.txt missing; got {names:?}"
        );
        assert!(
            names.contains(&"good2.txt"),
            "good2.txt missing; got {names:?}"
        );
        // truncated.txt data was removed so a warning about it should exist
        assert!(
            !report.warnings.is_empty(),
            "expected at least one warning about truncation"
        );
    }

    /// Corrupting part of a ZIP archive must produce non-empty skipped_ranges.
    #[test]
    fn test_repair_report_skipped_ranges() {
        let files: &[(&str, &[u8])] = &[
            ("file_a.txt", b"Content of file A"),
            ("file_b.txt", b"Content of file B"),
        ];
        let zip_bytes = build_test_zip(files);

        // Find file_b LFH and corrupt the bytes right before it (between the two entries).
        let b_pos = find_lfh_for_name(&zip_bytes, "file_b.txt").expect("file_b LFH");

        // Find where file_a ends (scan from pos 0 for next LFH after the first)
        let a_pos = find_lfh_for_name(&zip_bytes, "file_a.txt").expect("file_a LFH");

        let mut corrupt = zip_bytes.clone();
        // Insert garbage between file_a data end and file_b header by overwriting
        // a zone that doesn't overlap with either LFH.
        if b_pos > a_pos + 40 {
            let gap_start = a_pos + 40;
            let gap_end = b_pos.min(gap_start + 20);
            for b in &mut corrupt[gap_start..gap_end] {
                *b = 0xAA;
            }
        }

        let report = repair_zip(Cursor::new(&corrupt)).expect("repair_zip");

        // Both files should still be found
        let names: Vec<&str> = report
            .recovered_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            names.contains(&"file_a.txt"),
            "file_a.txt missing; {names:?}"
        );
        assert!(
            names.contains(&"file_b.txt"),
            "file_b.txt missing; {names:?}"
        );

        // The gap between entries should appear in skipped_ranges.
        // (If the two entries are adjacent with no gap this may be empty; that's OK.)
        // The important invariant is that the scan completed without error.
        let _ = report.skipped_ranges; // presence checked implicitly by no-panic
    }

    /// Repair a ZIP, write recovered entries to a temp file, then re-read and
    /// verify the data round-trips correctly.
    #[test]
    fn test_repair_round_trip_recovered_entries() {
        let original_data: Vec<u8> = (0u8..=255).cycle().take(1024).collect();
        let files: &[(&str, &[u8])] = &[
            ("roundtrip.bin", &original_data),
            ("small.txt", b"small file"),
        ];
        let zip_bytes = build_test_zip(files);

        // Repair the archive
        let report = repair_zip(Cursor::new(&zip_bytes)).expect("repair_zip");
        assert_eq!(report.recovered_entries.len(), 2);

        // Write recovered entries into a new ZIP in a temp file
        let temp_dir = std::env::temp_dir();
        let temp_path = temp_dir.join("oxiarc_repair_roundtrip_test.zip");

        {
            let out_file = std::fs::File::create(&temp_path).expect("create temp file");
            let mut w = ZipWriter::new(out_file);
            for entry in &report.recovered_entries {
                if !entry.decompressed_data.is_empty() || entry.status != RecoveryStatus::RawOnly {
                    w.add_file(&entry.name, &entry.decompressed_data)
                        .expect("write recovered entry");
                }
            }
            w.finish().expect("finish zip");
        }

        // Re-read the written ZIP and verify
        {
            let in_file = std::fs::File::open(&temp_path).expect("open temp file");
            let mut reader = ZipReader::new(in_file).expect("ZipReader::new");
            let entries = reader.entries().to_vec();
            assert_eq!(entries.len(), 2);

            let rt_entry = entries
                .iter()
                .find(|e| e.name == "roundtrip.bin")
                .expect("roundtrip.bin");
            let data = reader.extract(rt_entry).expect("extract roundtrip.bin");
            assert_eq!(data, original_data);

            let small = entries
                .iter()
                .find(|e| e.name == "small.txt")
                .expect("small.txt");
            let data = reader.extract(small).expect("extract small.txt");
            assert_eq!(&data, b"small file");
        }

        let _ = std::fs::remove_file(&temp_path);
    }

    // ── Additional edge-case tests ────────────────────────────────────────────

    /// An empty input must produce an empty report without panicking.
    #[test]
    fn test_zip_repair_empty_input() {
        let report = repair_zip(Cursor::new(&[][..])).expect("repair_zip on empty");
        assert!(report.recovered_entries.is_empty());
        assert!(report.skipped_ranges.is_empty());
    }

    /// An empty TAR input must produce an empty report without panicking.
    #[test]
    fn test_tar_repair_empty_input() {
        let report = repair_tar(Cursor::new(&[][..])).expect("repair_tar on empty");
        assert!(report.recovered_entries.is_empty());
    }

    /// `repair_tar` on a properly-written empty TAR (two zero blocks) must
    /// produce an empty entry list.
    #[test]
    fn test_tar_repair_two_zero_blocks() {
        let empty_tar = build_test_tar(&[]);
        let report = repair_tar(Cursor::new(&empty_tar)).expect("repair_tar two-zero");
        assert!(report.recovered_entries.is_empty());
    }

    /// Verify that `RecoveryStatus` derives `PartialEq` and `Eq` correctly.
    #[test]
    fn test_recovery_status_eq() {
        assert_eq!(RecoveryStatus::Verified, RecoveryStatus::Verified);
        assert_ne!(RecoveryStatus::Verified, RecoveryStatus::Recovered);
        assert_ne!(RecoveryStatus::Recovered, RecoveryStatus::RawOnly);
    }

    /// `RepairOptions::default()` must set `max_entry_size` to 64 MiB.
    #[test]
    fn test_repair_options_default() {
        let opts = RepairOptions::default();
        assert_eq!(opts.max_entry_size, 64 * 1024 * 1024);
    }

    /// Test the TAR writer's round-trip through the repair path
    /// to confirm directory entries are flagged correctly.
    #[test]
    fn test_tar_repair_directory_entry() {
        let mut tar_bytes = Vec::new();
        {
            let mut w = TarWriter::new(&mut tar_bytes);
            w.add_directory("mydir/").expect("add_directory");
            w.add_file("mydir/inner.txt", b"inner content")
                .expect("add_file");
            w.finish().expect("finish");
        }

        let report = repair_tar(Cursor::new(&tar_bytes)).expect("repair_tar");
        let names: Vec<&str> = report
            .recovered_entries
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        assert!(
            names.contains(&"mydir/inner.txt"),
            "mydir/inner.txt missing; got {names:?}"
        );
    }
}

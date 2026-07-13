//! Repair a truncated/corrupted ZIP archive using [`repair_zip`].
//!
//! Builds a valid ZIP, corrupts the End-Of-Central-Directory record (as if
//! the archive had been truncated mid-transfer), and shows that
//! [`repair_zip`] can still recover the entries by scanning for local file
//! header signatures.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-archive --example repair_corrupted_zip
//! ```

use oxiarc_archive::repair::{RecoveryStatus, repair_zip};
use oxiarc_archive::zip::{ZipCompressionLevel, ZipWriter};
use std::io::Cursor;

fn main() {
    let mut good_archive = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut good_archive);
        writer.set_compression(ZipCompressionLevel::Normal);
        writer
            .add_file("alpha.txt", b"alpha payload")
            .expect("add_file alpha.txt");
        writer
            .add_file("beta.txt", b"beta payload, a little longer this time")
            .expect("add_file beta.txt");
        writer.finish().expect("finish writer");
    }
    println!("Built a well-formed ZIP: {} bytes", good_archive.len());

    // Simulate corruption: truncate away the central directory + EOCD
    // record entirely, keeping only the local file headers + data.
    let truncate_at = good_archive.len() * 2 / 3;
    let corrupted = good_archive[..truncate_at].to_vec();
    println!(
        "Simulated truncation: kept {} of {} bytes (central directory lost)",
        corrupted.len(),
        good_archive.len()
    );

    let report = repair_zip(Cursor::new(corrupted)).expect("repair_zip scan failed");

    println!(
        "Repair scan recovered {} entr{} ({} skipped byte range(s), {} warning(s))",
        report.recovered_entries.len(),
        if report.recovered_entries.len() == 1 {
            "y"
        } else {
            "ies"
        },
        report.skipped_ranges.len(),
        report.warnings.len(),
    );

    for entry in &report.recovered_entries {
        let status = match entry.status {
            RecoveryStatus::Verified => "verified (CRC OK)",
            RecoveryStatus::Recovered => "recovered (CRC unavailable/mismatched)",
            RecoveryStatus::RawOnly => "raw only (decompression failed)",
            // `RecoveryStatus` is `#[non_exhaustive]`; report any future status
            // kind rather than failing to build against a newer oxiarc-archive.
            _ => "unknown recovery status",
        };
        println!(
            "  - {} @ offset {}: {} bytes, {}",
            entry.name,
            entry.offset,
            entry.decompressed_data.len(),
            status
        );
    }

    for warning in &report.warnings {
        println!("  ! {warning}");
    }

    assert!(
        !report.recovered_entries.is_empty(),
        "expected at least one entry to be recoverable from the truncated archive"
    );
    println!("Repair succeeded: at least one entry was recovered despite truncation.");
}

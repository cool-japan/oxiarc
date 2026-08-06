//! TAR archive repair/recovery by sequential block scanning.
//!
//! Walks the stream block-by-block (512 bytes per block), validates UStar
//! magic and POSIX checksum, and recovers every well-formed entry even when
//! intervening blocks are corrupt.  No `Seek` is required so the scanner
//! works on streaming sources.

use oxiarc_core::error::Result;

use crate::repair::{RecoveredEntry, RecoveryStatus, RepairOptions, RepairReport};
use crate::tar::header::TarHeader;

/// TAR block size in bytes.
const BLOCK_SIZE: usize = 512;

/// Typeflags that represent a regular file.
const TYPEFLAG_REGULAR_ALT: u8 = 0;
const TYPEFLAG_REGULAR: u8 = b'0';
const TYPEFLAG_CONTIGUOUS: u8 = b'7';

// ── Internal helpers ────────────────────────────────────────────────────────

/// Return `true` when the UStar magic at byte offset 257 matches.
fn is_ustar_block(block: &[u8; BLOCK_SIZE]) -> bool {
    let magic = &block[257..263];
    magic == b"ustar\0" || magic == b"ustar "
}

/// Return `true` when the block is all-zero (end-of-archive sentinel).
fn is_zero_block(block: &[u8; BLOCK_SIZE]) -> bool {
    block.iter().all(|&b| b == 0)
}

/// Parse the NUL-terminated file name from the header block.
///
/// Combines the `name` field (bytes 0..100) and the `prefix` field
/// (bytes 345..500) using the UStar convention.
fn parse_tar_name(block: &[u8; BLOCK_SIZE]) -> String {
    let name = TarHeader::parse_string(&block[0..100]);
    let prefix = TarHeader::parse_string(&block[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// Parse the file size (octal ASCII, 12 bytes at offset 124).
fn parse_tar_size(block: &[u8; BLOCK_SIZE]) -> Result<u64> {
    TarHeader::parse_octal_u64(&block[124..136])
}

// ── Public scan entry point ─────────────────────────────────────────────────

/// Scan `reader` for UStar TAR blocks and recover all parseable file entries.
///
/// # Errors
///
/// Returns `Err` only for hard I/O failures on the underlying reader.
/// Corrupt or unrecognised blocks are silently skipped (with a warning).
pub(crate) fn scan_tar_reader<R: std::io::Read>(
    reader: &mut R,
    opts: &RepairOptions,
) -> Result<RepairReport> {
    let mut entries: Vec<RecoveredEntry> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut block_num = 0u64;
    let mut zero_block_count = 0u32;

    loop {
        let mut block = [0u8; BLOCK_SIZE];
        match reader.read_exact(&mut block) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        block_num += 1;

        // Two consecutive zero blocks = end-of-archive
        if is_zero_block(&block) {
            zero_block_count += 1;
            if zero_block_count >= 2 {
                break;
            }
            continue;
        }
        zero_block_count = 0;

        // Validate UStar magic
        if !is_ustar_block(&block) {
            warnings.push(format!(
                "block {block_num}: not a UStar header (magic {:?}), skipping",
                &block[257..263]
            ));
            continue;
        }

        // Validate POSIX checksum
        if !TarHeader::verify_checksum(&block) {
            warnings.push(format!("block {block_num}: checksum mismatch, skipping"));
            continue;
        }

        // Parse the file name and size
        let name = parse_tar_name(&block);
        let size = match parse_tar_size(&block) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!(
                    "block {block_num}: cannot parse size for '{name}': {e}"
                ));
                continue;
            }
        };
        let typeflag = block[156];
        let entry_offset = (block_num - 1) * BLOCK_SIZE as u64;

        // Dispatch by typeflag
        if typeflag == TYPEFLAG_REGULAR
            || typeflag == TYPEFLAG_REGULAR_ALT
            || typeflag == TYPEFLAG_CONTIGUOUS
        {
            // Regular file: read data blocks
            let blocks_needed = size.div_ceil(BLOCK_SIZE as u64);

            // Guard against unreasonably large entries
            if size > opts.max_entry_size {
                warnings.push(format!(
                    "block {block_num}: entry '{name}' size {size} exceeds max_entry_size {}; \
                     reading raw bytes up to limit",
                    opts.max_entry_size
                ));
                // Still need to consume the blocks from the stream
                let to_read = blocks_needed * BLOCK_SIZE as u64;
                drain_bytes(reader, to_read, &mut warnings, &name)?;
                block_num += blocks_needed;
                continue;
            }

            let buf_size = (blocks_needed as usize) * BLOCK_SIZE;
            let mut raw = vec![0u8; buf_size];
            match reader.read_exact(&mut raw) {
                Ok(()) => {
                    raw.truncate(size as usize);
                    block_num += blocks_needed;
                    entries.push(RecoveredEntry {
                        name,
                        method: 0,
                        compressed_size: size,
                        uncompressed_size: size,
                        crc32: 0,
                        offset: entry_offset,
                        decompressed_data: raw,
                        status: RecoveryStatus::Recovered,
                    });
                }
                Err(_) => {
                    warnings.push(format!("block {block_num}: truncated data for '{name}'"));
                    break;
                }
            }
        } else {
            // Directory, symlink, hard link, char/block device, FIFO, etc.
            entries.push(RecoveredEntry {
                name,
                method: 0,
                compressed_size: 0,
                uncompressed_size: 0,
                crc32: 0,
                offset: entry_offset,
                decompressed_data: Vec::new(),
                status: RecoveryStatus::Verified,
            });
        }
    }

    Ok(RepairReport {
        recovered_entries: entries,
        skipped_ranges: Vec::new(),
        warnings,
    })
}

/// Drain exactly `count` bytes from `reader`, discarding them.
fn drain_bytes<R: std::io::Read>(
    reader: &mut R,
    count: u64,
    warnings: &mut Vec<String>,
    entry_name: &str,
) -> Result<()> {
    let mut remaining = count;
    let mut buf = vec![0u8; 4096];
    while remaining > 0 {
        let chunk = remaining.min(buf.len() as u64) as usize;
        match reader.read_exact(&mut buf[..chunk]) {
            Ok(()) => remaining -= chunk as u64,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                warnings.push(format!(
                    "truncated while draining oversized entry '{entry_name}'"
                ));
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Build a minimal TAR archive in memory.  Used exclusively in tests.
#[cfg(test)]
pub(crate) fn build_test_tar(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        use crate::tar::TarWriter;
        let mut w = TarWriter::new(&mut buf);
        for (name, data) in files {
            w.add_file(name, data).expect("tar add_file");
        }
        w.finish().expect("tar finish");
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A small deterministic PRNG so the mutation sweep is reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next_u64(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0
        }
    }

    /// An entry larger than `max_entry_size` must take the drain path: it is
    /// warned about and its bytes are consumed from the stream (not buffered
    /// into a giant allocation), while earlier/later entries stay recoverable.
    #[test]
    fn test_tar_scan_oversized_entry_uses_drain_path() {
        let big = vec![0x5Au8; 4096]; // 8 blocks of payload.
        let tar = build_test_tar(&[("small.txt", b"tiny"), ("big.bin", &big)]);

        // Force the oversized branch for `big.bin` by capping at 8 bytes.
        let opts = RepairOptions { max_entry_size: 8 };
        let report = scan_tar_reader(&mut Cursor::new(&tar), &opts).expect("scan Ok");

        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("exceeds max_entry_size")),
            "oversized entry must produce a warning; got {:?}",
            report.warnings
        );
        // `small.txt` (4 bytes < 8) must still be recovered.
        assert!(
            report
                .recovered_entries
                .iter()
                .any(|e| e.name == "small.txt"),
            "small.txt should still be recovered"
        );
        // Nothing may report a payload larger than the cap.
        for entry in &report.recovered_entries {
            assert!(entry.decompressed_data.len() as u64 <= opts.max_entry_size);
        }
    }

    /// Deterministically mutate a valid TAR thousands of ways and assert the
    /// block scanner always terminates without panicking. Substitutes for a
    /// multi-million-exec fuzz campaign that cannot run in-session.
    #[test]
    fn test_tar_scan_mutation_never_panics() {
        let base = build_test_tar(&[
            ("one.txt", b"first tar entry body"),
            ("dir/two.bin", &[0xABu8, 0xCD].repeat(60)),
            ("three", b"third"),
        ]);
        // Cap decoded size so a mutated octal size field cannot allocate much.
        let opts = RepairOptions {
            max_entry_size: 64 * 1024,
        };
        let mut rng = Lcg(0x7A12_9F3C_5511_2244);

        for _ in 0..4000 {
            let mut m = base.clone();
            let flips = 1 + (rng.next_u64() % 8) as usize;
            for _ in 0..flips {
                let pos = (rng.next_u64() as usize) % m.len();
                m[pos] ^= (rng.next_u64() >> 24) as u8;
            }

            let report =
                scan_tar_reader(&mut Cursor::new(&m), &opts).expect("scan Ok for corrupt input");
            for entry in &report.recovered_entries {
                assert!(entry.decompressed_data.len() as u64 <= opts.max_entry_size);
                assert!(entry.offset < m.len() as u64);
            }
        }
    }

    /// Truncating a valid TAR at every 512-block boundary must never panic.
    #[test]
    fn test_tar_scan_block_truncations_never_panic() {
        let base = build_test_tar(&[
            ("a.txt", b"alpha"),
            ("b.txt", &[7u8; 700]), // spans two data blocks
        ]);
        let opts = RepairOptions::default();
        let mut cut = 0;
        while cut <= base.len() {
            let _ = scan_tar_reader(&mut Cursor::new(&base[..cut]), &opts).expect("scan Ok");
            cut += BLOCK_SIZE;
        }
        // Also the exact full length and a mid-block cut.
        let _ = scan_tar_reader(&mut Cursor::new(&base[..base.len().min(300)]), &opts)
            .expect("scan Ok");
    }
}

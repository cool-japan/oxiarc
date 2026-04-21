//! TAR sparse file support.
//!
//! This module implements two variants of TAR sparse encoding:
//!
//! 1. **GNU old-format** (typeflag `'S'`): the sparse map is encoded in the
//!    512-byte TAR header's unused bytes, with optional continuation headers
//!    of 512 bytes each when the map does not fit in a single header.
//!
//! 2. **PAX 0.1** (`GNU.sparse.*` extended attributes): the map lives in a
//!    PAX extended header preceding a regular `'0'` data entry, where
//!    `GNU.sparse.map` holds a comma-separated list of
//!    `offset,numbytes,offset,numbytes,...` pairs and
//!    `GNU.sparse.realsize` holds the logical file size.
//!
//! Sparse entries are *materialized*: the reader allocates a buffer of
//! `realsize` bytes, fills each `(offset, numbytes)` run from the data
//! stream, and zero-fills the remaining regions. This matches the behavior
//! of `tar -x` on typical Unix filesystems without explicit hole punching.
//! Sparse-hole preservation on disk is out of scope.

use oxiarc_core::error::{OxiArcError, Result};
use std::collections::HashMap;
use std::io::Read;

use super::BLOCK_SIZE;

/// Maximum logical size we are willing to materialize for a sparse entry,
/// in bytes. Prevents a malicious or malformed archive from forcing a
/// multi-gigabyte allocation through a small on-disk footprint.
///
/// 16 GiB strikes a balance between legitimate large sparse files and
/// preventing obvious denial-of-service on typical 64-bit hosts.
pub(crate) const MAX_SPARSE_REALSIZE: u64 = 16 * 1024 * 1024 * 1024;

/// Byte offset in the primary TAR header where GNU sparse entries begin.
const GNU_SPARSE_OFFSET: usize = 386;

/// Number of sparse entries in the primary TAR header.
const GNU_SPARSE_PRIMARY_COUNT: usize = 4;

/// Size in bytes of a single GNU sparse entry (two 12-byte octal fields).
const GNU_SPARSE_ENTRY_SIZE: usize = 24;

/// Size in bytes of the `offset` field within a GNU sparse entry.
const GNU_SPARSE_OFFSET_FIELD: usize = 12;

/// Size in bytes of the `numbytes` field within a GNU sparse entry.
const GNU_SPARSE_NUMBYTES_FIELD: usize = 12;

/// Offset in the primary TAR header of the `isextended` byte.
const GNU_ISEXTENDED_PRIMARY: usize = 482;

/// Offset in the primary TAR header of the 12-byte ASCII octal `realsize`.
const GNU_REALSIZE_PRIMARY: usize = 483;

/// Size of the `realsize` field.
const GNU_REALSIZE_SIZE: usize = 12;

/// Number of sparse entries in a continuation header.
///
/// Layout of a 512-byte continuation header:
/// * 21 entries × 24 bytes = 504 bytes of sparse map
/// * 1 byte `isextended` at offset 504
/// * 7 bytes padding to reach 512
const GNU_SPARSE_CONT_COUNT: usize = 21;

/// Offset of the `isextended` byte inside a continuation header.
const GNU_ISEXTENDED_CONT: usize = 504;

/// Upper bound on continuation headers we are willing to follow. A malicious
/// archive could otherwise set `isextended=1` forever. 4096 continuation
/// headers yields up to 4096 × 21 + 4 ≈ 86_020 run segments, which is far
/// beyond any legitimate sparse file.
const MAX_SPARSE_CONT_HEADERS: usize = 4096;

/// Parsed sparse map: the logical file size plus the list of non-hole runs.
#[derive(Debug, Clone)]
pub(crate) struct SparseMap {
    /// Logical (realsize) of the sparse file in bytes.
    pub(crate) realsize: u64,
    /// `(offset, numbytes)` pairs describing the contiguous regions of the
    /// logical file that are explicitly stored in the data stream. Pairs are
    /// in stream order; the regions between them are implicit holes.
    pub(crate) runs: Vec<(u64, u64)>,
}

impl SparseMap {
    /// Sum of all run lengths — the number of bytes present in the data
    /// stream (before padding to the 512-byte block boundary).
    pub(crate) fn stored_size(&self) -> u64 {
        self.runs
            .iter()
            .fold(0u64, |acc, &(_, n)| acc.saturating_add(n))
    }

    /// Stored-size rounded up to the next 512-byte boundary.
    ///
    /// The TAR spec pads every data payload to `BLOCK_SIZE`, whether or not
    /// the payload is sparse. `read_entries` uses this to decide how many
    /// bytes to seek over when skipping an entry.
    pub(crate) fn padded_stored_size(&self) -> u64 {
        let stored = self.stored_size();
        let rem = stored % BLOCK_SIZE as u64;
        if rem == 0 {
            stored
        } else {
            stored + (BLOCK_SIZE as u64 - rem)
        }
    }

    /// Validate that the map is self-consistent.
    ///
    /// Rejects:
    /// * zero-length runs (legal per spec but trivially useless; we treat
    ///   as malformed to simplify extraction),
    /// * runs whose end exceeds `realsize`,
    /// * runs that are not in monotonically non-decreasing offset order, or
    /// * overlapping runs.
    ///
    /// Overlap detection and "end > realsize" both use `u128` arithmetic so
    /// that `offset + numbytes` cannot overflow even near `u64::MAX`.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.realsize > MAX_SPARSE_REALSIZE {
            return Err(OxiArcError::invalid_header(format!(
                "sparse realsize {} exceeds maximum {}",
                self.realsize, MAX_SPARSE_REALSIZE
            )));
        }

        let mut prev_end: u128 = 0;
        let realsize = u128::from(self.realsize);

        for (idx, &(offset, numbytes)) in self.runs.iter().enumerate() {
            if numbytes == 0 {
                return Err(OxiArcError::invalid_header(format!(
                    "sparse run {} has zero length",
                    idx
                )));
            }

            let start = u128::from(offset);
            let end = start.saturating_add(u128::from(numbytes));

            if end > realsize {
                return Err(OxiArcError::invalid_header(format!(
                    "sparse run {} (offset={}, numbytes={}) exceeds realsize {}",
                    idx, offset, numbytes, self.realsize
                )));
            }

            if start < prev_end {
                return Err(OxiArcError::invalid_header(format!(
                    "sparse run {} (offset={}) overlaps previous run ending at {}",
                    idx, offset, prev_end
                )));
            }

            prev_end = end;
        }

        Ok(())
    }

    /// Parse the GNU old-format sparse map from a primary TAR header block.
    ///
    /// Reads continuation blocks from `reader` when the header's
    /// `isextended` byte is non-zero. The caller must hand in a reference to
    /// the already-consumed 512-byte primary header block.
    pub(crate) fn parse_gnu_old_format<R: Read>(
        primary: &[u8; BLOCK_SIZE],
        reader: &mut R,
    ) -> Result<Self> {
        let realsize = parse_octal_u64(
            &primary[GNU_REALSIZE_PRIMARY..GNU_REALSIZE_PRIMARY + GNU_REALSIZE_SIZE],
        )?;

        let mut runs = Vec::new();
        parse_entries(
            &primary[GNU_SPARSE_OFFSET
                ..GNU_SPARSE_OFFSET + GNU_SPARSE_PRIMARY_COUNT * GNU_SPARSE_ENTRY_SIZE],
            GNU_SPARSE_PRIMARY_COUNT,
            &mut runs,
        )?;

        let mut isextended = primary[GNU_ISEXTENDED_PRIMARY];
        let mut followed = 0usize;

        while isextended != 0 {
            if followed >= MAX_SPARSE_CONT_HEADERS {
                return Err(OxiArcError::invalid_header(
                    "too many sparse continuation headers",
                ));
            }
            followed += 1;

            let mut block = [0u8; BLOCK_SIZE];
            reader.read_exact(&mut block)?;

            parse_entries(
                &block[0..GNU_SPARSE_CONT_COUNT * GNU_SPARSE_ENTRY_SIZE],
                GNU_SPARSE_CONT_COUNT,
                &mut runs,
            )?;

            isextended = block[GNU_ISEXTENDED_CONT];
        }

        Ok(Self { realsize, runs })
    }

    /// Construct a sparse map from PAX 0.1 `GNU.sparse.*` extended
    /// attributes.
    ///
    /// Required keys:
    /// * `GNU.sparse.realsize` — logical size, base-10 ASCII.
    /// * `GNU.sparse.map` — comma-separated `offset,numbytes,...`.
    pub(crate) fn from_pax_attrs(attrs: &HashMap<String, String>) -> Result<Self> {
        let realsize_str = attrs.get("GNU.sparse.realsize").ok_or_else(|| {
            OxiArcError::invalid_header("sparse PAX header missing GNU.sparse.realsize")
        })?;
        let realsize: u64 = realsize_str.parse().map_err(|_| {
            OxiArcError::invalid_header(format!(
                "sparse PAX GNU.sparse.realsize not a decimal integer: {}",
                realsize_str
            ))
        })?;

        let map_str = attrs.get("GNU.sparse.map").ok_or_else(|| {
            OxiArcError::invalid_header("sparse PAX header missing GNU.sparse.map")
        })?;

        let mut numbers = Vec::new();
        if !map_str.is_empty() {
            for tok in map_str.split(',') {
                let n: u64 = tok.parse().map_err(|_| {
                    OxiArcError::invalid_header(format!(
                        "sparse PAX GNU.sparse.map contains non-integer token: {}",
                        tok
                    ))
                })?;
                numbers.push(n);
            }
        }

        if numbers.len() % 2 != 0 {
            return Err(OxiArcError::invalid_header(
                "sparse PAX GNU.sparse.map has odd number of tokens",
            ));
        }

        let mut runs = Vec::with_capacity(numbers.len() / 2);
        let mut iter = numbers.into_iter();
        while let (Some(off), Some(len)) = (iter.next(), iter.next()) {
            runs.push((off, len));
        }

        Ok(Self { realsize, runs })
    }
}

/// Read at most `count` sparse entries from `data`, pushing those with
/// non-zero length onto `runs`. Zero-length entries terminate parsing for
/// the current block (both offset and numbytes must be zero to count as a
/// terminator; otherwise it is a malformed entry).
fn parse_entries(data: &[u8], count: usize, runs: &mut Vec<(u64, u64)>) -> Result<()> {
    for i in 0..count {
        let base = i * GNU_SPARSE_ENTRY_SIZE;
        let offset_field = &data[base..base + GNU_SPARSE_OFFSET_FIELD];
        let numbytes_field = &data[base + GNU_SPARSE_OFFSET_FIELD
            ..base + GNU_SPARSE_OFFSET_FIELD + GNU_SPARSE_NUMBYTES_FIELD];

        // An entry where both fields are entirely zero-bytes terminates the
        // map within this block (common for the trailing unused slots of the
        // primary header). Treat any entry that parses to (0, 0) the same
        // way.
        if offset_field.iter().all(|&b| b == 0) && numbytes_field.iter().all(|&b| b == 0) {
            return Ok(());
        }

        let offset = parse_octal_u64(offset_field)?;
        let numbytes = parse_octal_u64(numbytes_field)?;

        if offset == 0 && numbytes == 0 {
            return Ok(());
        }

        runs.push((offset, numbytes));
    }
    Ok(())
}

/// Parse a null- or space-padded ASCII octal number. Shared with
/// `TarHeader::parse_octal_u64` in layout but kept local so that the sparse
/// module does not depend on the private parser in `super`.
fn parse_octal_u64(data: &[u8]) -> Result<u64> {
    // Take the longest prefix of octal digits; stop at the first non-digit
    // (typically NUL or space padding). Matches GNU tar's permissive
    // behavior for corner cases like a trailing space inside the field.
    let end = data
        .iter()
        .position(|&b| !matches!(b, b'0'..=b'7'))
        .unwrap_or(data.len());

    if end == 0 {
        return Ok(0);
    }

    let s = std::str::from_utf8(&data[..end])
        .map_err(|_| OxiArcError::invalid_header("sparse field not valid UTF-8 octal"))?;
    u64::from_str_radix(s, 8)
        .map_err(|_| OxiArcError::invalid_header(format!("sparse field not valid octal: {}", s)))
}

/// Extract a sparse payload from `reader` given a validated sparse `map`.
///
/// Allocates `map.realsize` bytes, reads each run from the data stream into
/// the appropriate window, and returns the fully materialized buffer. After
/// the run bytes are consumed, any zero-padding that brings the data
/// payload up to the next 512-byte TAR block boundary is also consumed, so
/// that the caller can continue parsing subsequent TAR headers without an
/// explicit seek.
///
/// # Errors
/// Returns `invalid_header` if `realsize` exceeds `usize::MAX` on the
/// current target, or `corrupted` if the data stream ends before the last
/// run or padding byte.
pub(crate) fn extract_sparse<R: Read>(reader: &mut R, map: &SparseMap) -> Result<Vec<u8>> {
    let realsize_usize: usize = map.realsize.try_into().map_err(|_| {
        OxiArcError::invalid_header(format!(
            "sparse realsize {} does not fit in usize on this target",
            map.realsize
        ))
    })?;
    let mut out = vec![0u8; realsize_usize];

    for &(offset, numbytes) in &map.runs {
        let off_usize: usize = offset.try_into().map_err(|_| {
            OxiArcError::invalid_header(format!(
                "sparse run offset {} does not fit in usize on this target",
                offset
            ))
        })?;
        let len_usize: usize = numbytes.try_into().map_err(|_| {
            OxiArcError::invalid_header(format!(
                "sparse run numbytes {} does not fit in usize on this target",
                numbytes
            ))
        })?;
        let end = off_usize
            .checked_add(len_usize)
            .ok_or_else(|| OxiArcError::invalid_header("sparse run end overflows usize"))?;
        if end > out.len() {
            return Err(OxiArcError::invalid_header(format!(
                "sparse run (offset={}, numbytes={}) exceeds realsize {}",
                offset, numbytes, map.realsize
            )));
        }
        reader.read_exact(&mut out[off_usize..end]).map_err(|e| {
            OxiArcError::corrupted(
                offset,
                format!("sparse run ({}, {}): {}", offset, numbytes, e),
            )
        })?;
    }

    // Consume zero-padding up to the next 512-byte block boundary so that
    // subsequent header parsing is block-aligned.
    let stored = map.stored_size();
    let rem = (stored % BLOCK_SIZE as u64) as usize;
    if rem != 0 {
        let pad = BLOCK_SIZE - rem;
        let mut buf = [0u8; BLOCK_SIZE];
        reader
            .read_exact(&mut buf[..pad])
            .map_err(|e| OxiArcError::corrupted(stored, format!("sparse padding: {}", e)))?;
    }

    Ok(out)
}

/// Map keyed by an entry's data offset to its parsed sparse map. Used by
/// `TarReader` as a side channel: the normal `extract()` path consults this
/// map first and falls back to raw copy when the key is absent.
pub(crate) type SparseMapTable = HashMap<u64, SparseMap>;

/// Test-only helper: build a synthetic GNU old-format `'S'` primary header
/// block that carries the first `k` entries of `runs` inline, the total
/// `realsize`, and (if `isextended`) a flag telling the reader to pull
/// continuation blocks afterwards. The checksum is computed exactly like
/// real TAR headers so parsers do not reject the forged block.
#[cfg(test)]
pub(crate) fn build_gnu_sparse_primary(
    name: &str,
    realsize: u64,
    inline_runs: &[(u64, u64)],
    isextended: bool,
) -> [u8; BLOCK_SIZE] {
    assert!(
        inline_runs.len() <= GNU_SPARSE_PRIMARY_COUNT,
        "primary block holds at most {} entries; supply a continuation",
        GNU_SPARSE_PRIMARY_COUNT
    );

    let mut block = [0u8; BLOCK_SIZE];

    // Name (100 bytes, null-padded)
    let name_bytes = name.as_bytes();
    let n = name_bytes.len().min(99);
    block[..n].copy_from_slice(&name_bytes[..n]);

    // Mode / uid / gid / size / mtime — minimal valid ASCII octal fields.
    write_octal(&mut block[100..108], 0o644);
    write_octal(&mut block[108..116], 0);
    write_octal(&mut block[116..124], 0);
    // For sparse files the standard `size` field holds the stored size, not
    // realsize. Tests don't exercise the normal parser here so zero is fine.
    let stored: u64 = inline_runs.iter().map(|(_, n)| n).sum();
    write_octal(&mut block[124..136], stored);
    write_octal(&mut block[136..148], 0);

    // Checksum placeholder — spaces
    block[148..156].copy_from_slice(b"        ");

    // Typeflag 'S'
    block[156] = b'S';

    // No linkname.
    // Magic: GNU tar uses "ustar  \0" (two spaces + null) for the old format,
    // but the sparse parser doesn't care about magic, so leave zeros.

    // Sparse entries
    for (i, &(offset, numbytes)) in inline_runs.iter().enumerate() {
        let base = GNU_SPARSE_OFFSET + i * GNU_SPARSE_ENTRY_SIZE;
        write_octal(&mut block[base..base + GNU_SPARSE_OFFSET_FIELD], offset);
        write_octal(
            &mut block[base + GNU_SPARSE_OFFSET_FIELD
                ..base + GNU_SPARSE_OFFSET_FIELD + GNU_SPARSE_NUMBYTES_FIELD],
            numbytes,
        );
    }

    block[GNU_ISEXTENDED_PRIMARY] = u8::from(isextended);
    write_octal(
        &mut block[GNU_REALSIZE_PRIMARY..GNU_REALSIZE_PRIMARY + GNU_REALSIZE_SIZE],
        realsize,
    );

    // Compute and fill checksum.
    let checksum: u32 = block.iter().map(|&b| b as u32).sum();
    let s = format!("{:06o}\0 ", checksum);
    block[148..156].copy_from_slice(&s.as_bytes()[..8]);

    block
}

/// Test-only helper: build a 512-byte continuation block holding up to 21
/// sparse entries plus the `isextended` flag.
#[cfg(test)]
pub(crate) fn build_gnu_sparse_continuation(
    entries: &[(u64, u64)],
    isextended: bool,
) -> [u8; BLOCK_SIZE] {
    assert!(
        entries.len() <= GNU_SPARSE_CONT_COUNT,
        "continuation block holds at most {} entries",
        GNU_SPARSE_CONT_COUNT
    );

    let mut block = [0u8; BLOCK_SIZE];
    for (i, &(offset, numbytes)) in entries.iter().enumerate() {
        let base = i * GNU_SPARSE_ENTRY_SIZE;
        write_octal(&mut block[base..base + GNU_SPARSE_OFFSET_FIELD], offset);
        write_octal(
            &mut block[base + GNU_SPARSE_OFFSET_FIELD
                ..base + GNU_SPARSE_OFFSET_FIELD + GNU_SPARSE_NUMBYTES_FIELD],
            numbytes,
        );
    }
    block[GNU_ISEXTENDED_CONT] = u8::from(isextended);
    block
}

/// Test-only helper: build a 512-byte block containing a minimal PAX
/// extended header whose `size` field equals `payload_len`. This mirrors
/// what `TarWriter::write_pax_header` produces for local PAX, but writes
/// the block in-place so tests can forge sparse PAX fixtures without
/// pulling in the full writer.
#[cfg(test)]
pub(crate) fn build_pax_header_block(typeflag: u8, payload_len: u64) -> [u8; BLOCK_SIZE] {
    let mut block = [0u8; BLOCK_SIZE];

    // Name: "PaxHeader" — pax readers typically ignore this field.
    let name = b"PaxHeader";
    block[..name.len()].copy_from_slice(name);

    // Mode / uid / gid
    write_octal(&mut block[100..108], 0o644);
    write_octal(&mut block[108..116], 0);
    write_octal(&mut block[116..124], 0);
    // Size = payload_len
    write_octal(&mut block[124..136], payload_len);
    // Mtime
    write_octal(&mut block[136..148], 0);

    // Checksum placeholder
    block[148..156].copy_from_slice(b"        ");

    // Typeflag
    block[156] = typeflag;

    // UStar magic
    block[257..263].copy_from_slice(b"ustar\0");
    block[263..265].copy_from_slice(b"00");

    // Compute and fill checksum.
    let checksum: u32 = block.iter().map(|&b| b as u32).sum();
    let s = format!("{:06o}\0 ", checksum);
    block[148..156].copy_from_slice(&s.as_bytes()[..8]);

    block
}

/// Test-only helper: write an ASCII octal number into `field`,
/// null-terminated, left-padded with '0'. Produces `field.len() - 1`
/// octal digits followed by a single NUL byte. Shared between
/// `build_gnu_sparse_primary` and `build_pax_header_block`.
#[cfg(test)]
fn write_octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let s = format!("{:0width$o}", value, width = width);
    let bytes = s.as_bytes();
    let take = bytes.len().min(width);
    field[..take].copy_from_slice(&bytes[..take]);
    field[take] = 0; // null terminator
    for b in &mut field[take + 1..] {
        *b = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_accepts_empty() {
        let m = SparseMap {
            realsize: 0,
            runs: vec![],
        };
        m.validate().expect("empty map should validate");
    }

    #[test]
    fn test_validate_accepts_ordered_non_overlapping() {
        let m = SparseMap {
            realsize: 10_000,
            runs: vec![(0, 100), (500, 200), (1_000, 500)],
        };
        m.validate()
            .expect("ordered non-overlapping should validate");
    }

    #[test]
    fn test_validate_rejects_overlap() {
        let m = SparseMap {
            realsize: 10_000,
            runs: vec![(0, 100), (50, 200)],
        };
        let err = m.validate().expect_err("overlap should be rejected");
        match err {
            OxiArcError::InvalidHeader { .. } => {}
            other => panic!("unexpected error variant: {:?}", other),
        }
    }

    #[test]
    fn test_validate_rejects_out_of_bounds() {
        let m = SparseMap {
            realsize: 100,
            runs: vec![(0, 50), (80, 30)],
        };
        m.validate()
            .expect_err("run past realsize should be rejected");
    }

    #[test]
    fn test_validate_rejects_realsize_limit() {
        let m = SparseMap {
            realsize: MAX_SPARSE_REALSIZE + 1,
            runs: vec![],
        };
        m.validate()
            .expect_err("realsize over cap should be rejected");
    }

    #[test]
    fn test_validate_rejects_zero_length_run() {
        let m = SparseMap {
            realsize: 100,
            runs: vec![(10, 0)],
        };
        m.validate()
            .expect_err("zero-length run should be rejected");
    }

    #[test]
    fn test_from_pax_attrs_basic() {
        let mut attrs = HashMap::new();
        attrs.insert("GNU.sparse.realsize".into(), "10000".into());
        attrs.insert("GNU.sparse.map".into(), "0,100,5000,200".into());
        let m = SparseMap::from_pax_attrs(&attrs).expect("from_pax_attrs");
        assert_eq!(m.realsize, 10_000);
        assert_eq!(m.runs, vec![(0, 100), (5000, 200)]);
        m.validate().expect("validate");
    }

    #[test]
    fn test_from_pax_attrs_empty_map_is_ok() {
        let mut attrs = HashMap::new();
        attrs.insert("GNU.sparse.realsize".into(), "0".into());
        attrs.insert("GNU.sparse.map".into(), "".into());
        let m = SparseMap::from_pax_attrs(&attrs).expect("from_pax_attrs");
        assert_eq!(m.realsize, 0);
        assert!(m.runs.is_empty());
    }

    #[test]
    fn test_from_pax_attrs_rejects_missing_realsize() {
        let mut attrs = HashMap::new();
        attrs.insert("GNU.sparse.map".into(), "0,100".into());
        SparseMap::from_pax_attrs(&attrs).expect_err("missing realsize");
    }

    #[test]
    fn test_from_pax_attrs_rejects_odd_tokens() {
        let mut attrs = HashMap::new();
        attrs.insert("GNU.sparse.realsize".into(), "1000".into());
        attrs.insert("GNU.sparse.map".into(), "0,100,5000".into());
        SparseMap::from_pax_attrs(&attrs).expect_err("odd tokens");
    }

    #[test]
    fn test_parse_gnu_old_format_single_header() {
        // 4 inline runs, all fit in primary header.
        let realsize = 16_384u64;
        let runs = vec![(0u64, 100u64), (500, 200), (4_000, 50), (10_000, 500)];
        let block = build_gnu_sparse_primary("sparse.bin", realsize, &runs, false);

        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let map = SparseMap::parse_gnu_old_format(&block, &mut empty).expect("parse primary-only");

        assert_eq!(map.realsize, realsize);
        assert_eq!(map.runs, runs);
        map.validate().expect("validate parsed");
    }

    #[test]
    fn test_parse_gnu_old_format_with_one_continuation() {
        // 4 runs in primary + 3 more in a continuation block.
        let realsize = 100_000u64;
        let primary_runs = vec![(0u64, 100u64), (500, 200), (4_000, 50), (10_000, 500)];
        let cont_runs = vec![(20_000u64, 300u64), (40_000, 600), (70_000, 1000)];

        let primary = build_gnu_sparse_primary("sparse.bin", realsize, &primary_runs, true);
        let cont = build_gnu_sparse_continuation(&cont_runs, false);

        let mut stream = std::io::Cursor::new(cont.to_vec());
        let map = SparseMap::parse_gnu_old_format(&primary, &mut stream)
            .expect("parse with continuation");

        assert_eq!(map.realsize, realsize);
        let mut expected = primary_runs.clone();
        expected.extend_from_slice(&cont_runs);
        assert_eq!(map.runs, expected);
        map.validate().expect("validate parsed");
    }

    #[test]
    fn test_extract_sparse_materializes_zeros() {
        let map = SparseMap {
            realsize: 1_000,
            runs: vec![(0, 10), (500, 5)],
        };
        map.validate().expect("validate");

        // Construct data: 10 bytes of 'A', 5 bytes of 'B', padded to 512.
        let mut data = Vec::new();
        data.extend(std::iter::repeat_n(b'A', 10));
        data.extend(std::iter::repeat_n(b'B', 5));
        let stored = data.len();
        let pad = (BLOCK_SIZE - (stored % BLOCK_SIZE)) % BLOCK_SIZE;
        data.extend(std::iter::repeat_n(0u8, pad));

        let mut cur = std::io::Cursor::new(data);
        let out = extract_sparse(&mut cur, &map).expect("extract");

        assert_eq!(out.len(), 1_000);
        assert!(out[0..10].iter().all(|&b| b == b'A'));
        assert!(out[10..500].iter().all(|&b| b == 0));
        assert!(out[500..505].iter().all(|&b| b == b'B'));
        assert!(out[505..].iter().all(|&b| b == 0));
    }

    #[test]
    fn test_stored_and_padded_size() {
        let map = SparseMap {
            realsize: 10_000,
            runs: vec![(0, 100), (5_000, 600)],
        };
        assert_eq!(map.stored_size(), 700);
        // 700 rounds up to 1024 (2 blocks).
        assert_eq!(map.padded_stored_size(), 1024);
    }
}

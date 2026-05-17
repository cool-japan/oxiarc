//! ZIP archive repair/recovery by scanning for Local File Header signatures.
//!
//! Scans a raw byte slice front-to-back looking for PK\x03\x04 signatures
//! independent of the central directory, enabling recovery from truncated
//! archives and missing/corrupt EOCD blocks.

use oxiarc_core::crc::Crc32;
use oxiarc_core::error::Result;
use oxiarc_deflate::inflate;

use crate::repair::{RecoveredEntry, RecoveryStatus, RepairOptions, RepairReport};

// ── ZIP format constants ────────────────────────────────────────────────────

/// Local File Header signature (PK\x03\x04 little-endian).
const LFH_SIG: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];

/// Data Descriptor signature (PK\x07\x08 little-endian, optional).
const DD_SIG: [u8; 4] = [0x50, 0x4B, 0x07, 0x08];

/// Bit 3 of the General Purpose Bit Flag: data descriptor present.
const FLAG_DATA_DESCRIPTOR: u16 = 0x0008;

/// Compression method: Stored.
const METHOD_STORED: u16 = 0;

/// Compression method: Deflate.
const METHOD_DEFLATE: u16 = 8;

/// Fixed size of the LFH up to (but not including) filename and extra field.
const LFH_FIXED: usize = 30;

// ── Parsed LFH fields ───────────────────────────────────────────────────────

struct ParsedLfh {
    flags: u16,
    method: u16,
    crc32_header: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    filename: String,
    data_start: usize, // absolute offset in the original buffer
}

// ── Internal scan implementation ────────────────────────────────────────────

/// Read a u16 little-endian from a slice at `offset`.
#[inline]
fn read_u16_le(buf: &[u8], offset: usize) -> Option<u16> {
    if offset + 2 > buf.len() {
        return None;
    }
    Some(u16::from_le_bytes([buf[offset], buf[offset + 1]]))
}

/// Read a u32 little-endian from a slice at `offset`.
#[inline]
fn read_u32_le(buf: &[u8], offset: usize) -> Option<u32> {
    if offset + 4 > buf.len() {
        return None;
    }
    Some(u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ]))
}

/// Try to parse an LFH at `pos` in `data`.
///
/// Returns `None` when the data at `pos` is structurally invalid (e.g. the
/// filename length would reach past the end of the buffer).  Returns `Some`
/// when the fixed fields could be read and the filename extracted.
fn try_parse_lfh(data: &[u8], pos: usize) -> Option<ParsedLfh> {
    // Must start with PK\x03\x04
    if data.get(pos..pos + 4)? != LFH_SIG {
        return None;
    }

    // Check we have at least the fixed 30-byte header
    if pos + LFH_FIXED > data.len() {
        return None;
    }

    let flags = read_u16_le(data, pos + 6)?;
    let method = read_u16_le(data, pos + 8)?;
    let crc32_header = read_u32_le(data, pos + 14)?;
    let compressed_size = read_u32_le(data, pos + 18)?;
    let uncompressed_size = read_u32_le(data, pos + 22)?;
    let fname_len = read_u16_le(data, pos + 26)? as usize;
    let extra_len = read_u16_le(data, pos + 28)? as usize;

    // Sanity-check field lengths (reject obviously bogus headers)
    if fname_len > 65535 || extra_len > 65535 {
        return None;
    }

    let fname_start = pos + LFH_FIXED;
    let fname_end = fname_start.checked_add(fname_len)?;
    let extra_end = fname_end.checked_add(extra_len)?;

    if extra_end > data.len() {
        return None;
    }

    let fname_bytes = &data[fname_start..fname_end];
    // Accept any non-empty filename (or allow empty for valid but unusual ZIPs)
    let filename = String::from_utf8_lossy(fname_bytes).into_owned();

    Some(ParsedLfh {
        flags,
        method,
        crc32_header,
        compressed_size,
        uncompressed_size,
        filename,
        data_start: extra_end,
    })
}

/// Attempt to resolve sizes when the data-descriptor flag (bit 3) is set and
/// the compressed/uncompressed sizes in the LFH are 0.
///
/// Strategy: scan from `data_start` forward looking for either:
///   1. A Data Descriptor signature (PK\x07\x08) followed by plausible fields, or
///   2. The next LFH signature (PK\x03\x04) which bounds the compressed region.
///
/// Returns `(compressed_size, uncompressed_size, crc32)` on success or `None`
/// if no plausible boundary was found.
fn resolve_data_descriptor(
    data: &[u8],
    data_start: usize,
    max_entry_size: u64,
) -> Option<(usize, u32)> {
    // Walk forward looking for DD sig or next LFH
    let max_scan = data_start + (max_entry_size as usize).min(data.len() - data_start);

    let mut i = data_start;
    while i + 4 <= max_scan.min(data.len()) {
        // Check for optional DD signature PK\x07\x08
        if data[i..i + 4] == DD_SIG && i + 16 <= data.len() {
            // crc32 at i+4, comp_size at i+8, uncomp_size at i+12
            let comp = read_u32_le(data, i + 8)?;
            let compressed_end = data_start + comp as usize;
            // The DD must come exactly after the compressed data
            if compressed_end == i {
                return Some((comp as usize, read_u32_le(data, i + 4)?));
            }
        }
        // Check for next LFH: the region [data_start..i] is the compressed payload
        if i > data_start && data[i..i + 4] == LFH_SIG {
            let comp = i - data_start;
            // No CRC available from header; return 0
            return Some((comp, 0));
        }
        i += 1;
    }
    // Last resort: treat everything remaining (up to max_entry_size) as data
    let comp = (max_scan.min(data.len()) - data_start).min(max_entry_size as usize);
    Some((comp, 0))
}

/// Try to parse an LFH at `pos`, extract and decompress its payload, and
/// return a `RecoveredEntry` plus the position just after the entry's data.
///
/// Returns `Ok(None)` for soft failures (skip and advance by 1).
pub(crate) fn try_parse_lfh_and_extract(
    data: &[u8],
    pos: usize,
    opts: &RepairOptions,
) -> Result<Option<(RecoveredEntry, usize)>> {
    let lfh = match try_parse_lfh(data, pos) {
        Some(h) => h,
        None => return Ok(None),
    };

    let has_dd = lfh.flags & FLAG_DATA_DESCRIPTOR != 0;
    let offset = pos as u64;

    // Determine the compressed region
    let (compressed_slice, crc_header, next_pos) =
        if has_dd && lfh.compressed_size == 0 && lfh.uncompressed_size == 0 {
            // Sizes unknown; scan for data descriptor / next LFH
            match resolve_data_descriptor(data, lfh.data_start, opts.max_entry_size) {
                Some((comp_len, dd_crc)) => {
                    let end = lfh.data_start + comp_len;
                    // Advance past the data descriptor if present
                    let next = advance_past_dd(data, end);
                    (&data[lfh.data_start..end.min(data.len())], dd_crc, next)
                }
                None => return Ok(None),
            }
        } else {
            let comp = if lfh.compressed_size == 0 && !has_dd {
                0usize
            } else {
                lfh.compressed_size as usize
            };
            // Cap to buffer
            let end = (lfh.data_start + comp).min(data.len());
            let next = advance_past_dd(data, end);
            (&data[lfh.data_start..end], lfh.crc32_header, next)
        };

    // Reject payloads that are obviously too large
    if compressed_slice.len() as u64 > opts.max_entry_size {
        return Ok(None);
    }

    let compressed_size = compressed_slice.len() as u64;
    let uncompressed_size = lfh.uncompressed_size as u64;

    let (decompressed, status) =
        decompress_entry(compressed_slice, lfh.method, crc_header, has_dd, opts);

    let entry = RecoveredEntry {
        name: lfh.filename,
        method: lfh.method,
        compressed_size,
        uncompressed_size,
        crc32: crc_header,
        offset,
        decompressed_data: decompressed,
        status,
    };

    Ok(Some((entry, next_pos)))
}

/// Advance `pos` past a Data Descriptor if one is present at that position.
fn advance_past_dd(data: &[u8], pos: usize) -> usize {
    if pos + 4 <= data.len() && data[pos..pos + 4] == DD_SIG {
        // DD: optional sig (4) + crc32 (4) + comp_size (4) + uncomp_size (4) = 16
        if pos + 16 <= data.len() {
            return pos + 16;
        }
    }
    // No DD sig — but there might be a DD without sig (crc + sizes = 12)
    pos
}

/// Decompress entry payload according to `method`, verify CRC if available.
fn decompress_entry(
    compressed: &[u8],
    method: u16,
    crc_header: u32,
    has_dd: bool,
    _opts: &RepairOptions,
) -> (Vec<u8>, RecoveryStatus) {
    match method {
        METHOD_STORED => {
            let data = compressed.to_vec();
            let status = verify_crc(&data, crc_header, has_dd);
            (data, status)
        }
        METHOD_DEFLATE => match inflate(compressed) {
            Ok(decompressed) => {
                let status = verify_crc(&decompressed, crc_header, has_dd);
                (decompressed, status)
            }
            Err(_) => {
                // Return raw compressed bytes; caller knows it failed
                (compressed.to_vec(), RecoveryStatus::RawOnly)
            }
        },
        _ => {
            // Unknown compression — return raw bytes
            (compressed.to_vec(), RecoveryStatus::RawOnly)
        }
    }
}

/// Check CRC and return the appropriate `RecoveryStatus`.
fn verify_crc(data: &[u8], crc_header: u32, has_dd: bool) -> RecoveryStatus {
    // If data descriptor flag is set and header CRC is 0, we can't verify
    if has_dd && crc_header == 0 {
        return RecoveryStatus::Recovered;
    }
    let computed = Crc32::compute(data);
    if computed == crc_header {
        RecoveryStatus::Verified
    } else {
        RecoveryStatus::Recovered
    }
}

// ── Public scan entry point ─────────────────────────────────────────────────

/// Scan `data` for ZIP Local File Headers and recover all parseable entries.
pub(crate) fn scan_zip_bytes(data: &[u8], opts: &RepairOptions) -> Result<RepairReport> {
    let mut entries: Vec<RecoveredEntry> = Vec::new();
    let mut skipped: Vec<(u64, u64)> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut pos = 0usize;
    let mut last_recovered_end = 0usize;

    while pos + 4 <= data.len() {
        // Fast path: skip bytes that are not the start of a PK signature
        if data[pos] != 0x50 {
            pos += 1;
            continue;
        }
        if data[pos..pos + 4] != LFH_SIG {
            pos += 1;
            continue;
        }

        match try_parse_lfh_and_extract(data, pos, opts) {
            Ok(Some((entry, next_pos))) => {
                if pos > last_recovered_end {
                    skipped.push((last_recovered_end as u64, pos as u64));
                    warnings.push(format!(
                        "skipped {} unreadable bytes at offset {:#x}",
                        pos - last_recovered_end,
                        last_recovered_end
                    ));
                }
                last_recovered_end = next_pos;
                entries.push(entry);
                pos = next_pos;
            }
            Ok(None) => {
                pos += 1;
            }
            Err(e) => {
                warnings.push(format!("error at offset {pos:#x}: {e}"));
                pos += 1;
            }
        }
    }

    // Record any trailing gap
    if data.len() > last_recovered_end && !entries.is_empty() {
        // Only interesting if there were some entries
        let gap_start = last_recovered_end as u64;
        let gap_end = data.len() as u64;
        // Only record if there's actual unaccounted data beyond the last entry
        let _ = (gap_start, gap_end); // ignore small trailing EOCD remnants
    }

    Ok(RepairReport {
        recovered_entries: entries,
        skipped_ranges: skipped,
        warnings,
    })
}

/// Build a minimal valid ZIP archive in memory containing one deflated file.
/// Used exclusively for tests.
#[cfg(test)]
pub(crate) fn build_test_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        use crate::zip::ZipWriter;
        let mut w = ZipWriter::new(&mut buf);
        for (name, data) in files {
            w.add_file(name, data).expect("add_file");
        }
        w.finish().expect("finish");
    }
    buf
}

/// Find the position of the first LFH with a given filename in `data`.
/// Used exclusively for tests to locate specific entries.
#[cfg(test)]
pub(crate) fn find_lfh_for_name(data: &[u8], name: &str) -> Option<usize> {
    let mut pos = 0;
    while pos + 4 <= data.len() {
        if data[pos..pos + 4] != LFH_SIG {
            pos += 1;
            continue;
        }
        if let Some(lfh) = try_parse_lfh(data, pos) {
            if lfh.filename == name {
                return Some(pos);
            }
        }
        pos += 1;
    }
    None
}

/// Locate the byte offset of the End-Of-Central-Directory record in `data`.
#[cfg(test)]
pub(crate) fn find_eocd_offset(data: &[u8]) -> Option<usize> {
    const EOCD_SIG: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    let mut i = data.len().saturating_sub(22);
    while i > 0 {
        if data[i..i + 4] == EOCD_SIG {
            return Some(i);
        }
        i -= 1;
    }
    None
}

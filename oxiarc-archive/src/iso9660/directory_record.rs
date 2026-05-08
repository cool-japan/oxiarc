//! ISO 9660 Directory Record parsing.
//!
//! Directory records are variable-length structures within directory extents.
//! They must be word-aligned (even byte boundaries) within the sector.
//! Padding records (LEN_DR == 0) indicate that the rest of the current sector
//! should be skipped.
//!
//! ECMA-119 §9.1 describes the field layout.

use crate::iso9660::joliet::decode_ucs2_be;

/// A parsed directory record.
#[derive(Debug, Clone)]
pub struct DirRecord {
    /// Decoded filename (UTF-8). Dots and version suffixes are stripped.
    pub name: String,
    /// Logical Block Address of the file data extent.
    pub lba: u32,
    /// Size of the file data in bytes.
    pub size: u64,
    /// `true` if this entry is a directory.
    pub is_dir: bool,
}

/// Parse one directory record from `data` starting at `offset`.
///
/// Returns `None` if `LEN_DR == 0` (padding record, caller should skip to
/// next sector boundary).
///
/// On success, returns `(record, bytes_consumed)` where `bytes_consumed` is
/// equal to `LEN_DR` (the full record length including identifier and padding).
pub fn parse_dir_record(data: &[u8], offset: usize, joliet: bool) -> Option<(DirRecord, usize)> {
    if offset >= data.len() {
        return None;
    }

    let len_dr = data[offset] as usize;
    if len_dr == 0 {
        return None;
    }

    if offset + len_dr > data.len() {
        return None;
    }

    let record = &data[offset..offset + len_dr];

    // LBA: LE at bytes 2-5
    let lba = u32::from_le_bytes([record[2], record[3], record[4], record[5]]);
    // Size: LE at bytes 10-13
    let size = u32::from_le_bytes([record[10], record[11], record[12], record[13]]) as u64;
    // File flags: byte 25. Bit 1 = directory.
    let flags = record[25];
    let is_dir = (flags & 0x02) != 0;
    // Length of File Identifier: byte 32
    let len_fi = record[32] as usize;

    // Bounds check for identifier
    if 33 + len_fi > len_dr {
        return None;
    }

    let fi_bytes = &record[33..33 + len_fi];

    // Skip '.' (current dir: LEN_FI=1, byte=0x00) and '..' (parent: LEN_FI=1, byte=0x01)
    if len_fi == 1 && (fi_bytes[0] == 0x00 || fi_bytes[0] == 0x01) {
        let record = DirRecord {
            name: String::new(),
            lba,
            size,
            is_dir,
        };
        return Some((record, len_dr));
    }

    let name = if joliet {
        // Joliet: UCS-2 BE filename, no version suffix
        let decoded = decode_ucs2_be(fi_bytes);
        // Joliet filenames don't have ;version suffixes
        decoded
    } else {
        // ISO Level 1 (PVD): ASCII, strip ";1" version suffix
        let ascii = String::from_utf8_lossy(fi_bytes).into_owned();
        strip_version_suffix(&ascii)
    };

    let record = DirRecord {
        name,
        lba,
        size,
        is_dir,
    };

    Some((record, len_dr))
}

/// Strip the `;N` version suffix from an ISO 9660 Level 1 filename.
///
/// For example, `"HELLO.TXT;1"` becomes `"HELLO.TXT"`.
fn strip_version_suffix(name: &str) -> String {
    if let Some(pos) = name.rfind(';') {
        name[..pos].to_owned()
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_file_record(fi: &[u8], lba: u32, size: u32, is_dir: bool) -> Vec<u8> {
        let len_fi = fi.len();
        let padding = if len_fi % 2 == 0 { 1usize } else { 0usize };
        let len_dr = 33 + len_fi + padding;
        let mut rec = vec![0u8; len_dr];
        rec[0] = len_dr as u8;
        rec[2..6].copy_from_slice(&lba.to_le_bytes());
        rec[6..10].copy_from_slice(&lba.to_be_bytes());
        rec[10..14].copy_from_slice(&size.to_le_bytes());
        rec[14..18].copy_from_slice(&size.to_be_bytes());
        rec[25] = if is_dir { 0x02 } else { 0x00 };
        rec[32] = len_fi as u8;
        rec[33..33 + len_fi].copy_from_slice(fi);
        rec
    }

    #[test]
    fn test_parse_ascii_file() {
        let rec = make_file_record(b"HELLO.TXT;1", 22, 6, false);
        let (dr, consumed) = parse_dir_record(&rec, 0, false).expect("should parse");
        assert_eq!(dr.name, "HELLO.TXT");
        assert_eq!(dr.lba, 22);
        assert_eq!(dr.size, 6);
        assert!(!dr.is_dir);
        assert_eq!(consumed, rec.len());
    }

    #[test]
    fn test_parse_joliet_file() {
        // "hi.txt" in UCS-2 BE = 12 bytes (even)
        let fi: Vec<u8> = "hi.txt"
            .chars()
            .flat_map(|c| [(c as u16 >> 8) as u8, c as u16 as u8])
            .collect();
        let rec = make_file_record(&fi, 10, 100, false);
        let (dr, _) = parse_dir_record(&rec, 0, true).expect("should parse");
        assert_eq!(dr.name, "hi.txt");
        assert_eq!(dr.lba, 10);
        assert_eq!(dr.size, 100);
    }

    #[test]
    fn test_parse_dot_record_skipped() {
        // "." directory entry
        let mut rec = vec![0u8; 34];
        rec[0] = 34;
        rec[32] = 1;
        rec[33] = 0x00; // "." marker
        rec[25] = 0x02; // directory flag
        let (dr, _) = parse_dir_record(&rec, 0, false).expect("should parse dot");
        // Name will be empty — caller skips these
        assert!(dr.name.is_empty());
    }

    #[test]
    fn test_parse_zero_len_dr() {
        let data = [0u8; 64];
        assert!(parse_dir_record(&data, 0, false).is_none());
    }

    #[test]
    fn test_strip_version_suffix() {
        assert_eq!(strip_version_suffix("HELLO.TXT;1"), "HELLO.TXT");
        assert_eq!(strip_version_suffix("FILE;2"), "FILE");
        assert_eq!(strip_version_suffix("NOVERSION"), "NOVERSION");
    }
}

//! ISO 9660 (ECMA-119) read support.
//!
//! Supports:
//! - Primary Volume Descriptor (PVD) with ASCII / ISO Level-1 filenames
//! - Joliet Supplementary Volume Descriptor (SVD) with UCS-2 BE filenames
//! - Recursive directory walking
//! - File data extraction by LBA seek
//!
//! Limitations:
//! - Read-only (no write support)
//! - No Rock Ridge, El Torito, or UDF extensions
//! - No multi-session support
//! - No write support

pub mod directory_record;
pub mod joliet;
pub mod volume_descriptor;

use directory_record::parse_dir_record;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use volume_descriptor::{
    VolumeDescriptor, parse_volume_descriptor, read_logical_block_size, read_volume_space_size,
};

/// Logical block size for ISO 9660 (always 2048 bytes).
const SECTOR_SIZE: u64 = 2048;

/// An entry (file or directory) found in the ISO 9660 image.
#[derive(Debug, Clone)]
pub struct IsoEntry {
    /// Full path from root, e.g. `"dir/file.txt"`.
    pub name: String,
    /// Logical Block Address of the data extent.
    pub lba: u32,
    /// Size of the data in bytes.
    pub size: u64,
    /// `true` if this is a directory entry.
    pub is_dir: bool,
}

/// ISO 9660 image reader.
///
/// Parses PVD and optional Joliet SVD, walks the directory tree, and
/// exposes file entries for listing and extraction.
pub struct IsoReader<R: Read + Seek> {
    reader: R,
    entries: Vec<IsoEntry>,
    joliet: bool,
    /// Volume identifier string from the PVD.
    pub volume_id: String,
    /// Total number of logical blocks in the image.
    pub total_lbas: u32,
    /// Logical block size (should always be 2048).
    pub logical_block_size: u16,
}

impl<R: Read + Seek> IsoReader<R> {
    /// Open an ISO 9660 image from a `Read + Seek` source.
    ///
    /// Reads all volume descriptors, selects the best root (Joliet preferred),
    /// walks the complete directory tree, and populates `entries`.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut pvd_root_lba: Option<u32> = None;
        let mut pvd_root_size: Option<u32> = None;
        let mut joliet_root_lba: Option<u32> = None;
        let mut joliet_root_size: Option<u32> = None;
        let mut volume_id = String::new();
        let mut total_lbas = 0u32;
        let mut logical_block_size = 2048u16;

        // Walk volume descriptors starting at LBA 16
        let mut lba = 16u64;
        loop {
            let byte_offset = lba * SECTOR_SIZE;
            reader.seek(SeekFrom::Start(byte_offset)).map_err(|e| {
                OxiArcError::invalid_header(format!("ISO: seek to LBA {lba} failed: {e}"))
            })?;

            let mut sector = [0u8; 2048];
            reader.read_exact(&mut sector).map_err(|e| {
                OxiArcError::invalid_header(format!("ISO: read LBA {lba} failed: {e}"))
            })?;

            match parse_volume_descriptor(&sector) {
                VolumeDescriptor::Primary {
                    root_dir_lba,
                    root_dir_size,
                    volume_id: vid,
                } => {
                    pvd_root_lba = Some(root_dir_lba);
                    pvd_root_size = Some(root_dir_size);
                    volume_id = vid;
                    total_lbas = read_volume_space_size(&sector);
                    logical_block_size = read_logical_block_size(&sector);
                }
                VolumeDescriptor::Joliet {
                    root_dir_lba,
                    root_dir_size,
                } => {
                    joliet_root_lba = Some(root_dir_lba);
                    joliet_root_size = Some(root_dir_size);
                }
                VolumeDescriptor::Terminator => break,
                VolumeDescriptor::Other => {}
            }

            lba += 1;
        }

        // Prefer Joliet root if available
        let use_joliet = joliet_root_lba.is_some();
        let (root_lba, root_size) = if use_joliet {
            let lba = joliet_root_lba
                .ok_or_else(|| OxiArcError::invalid_header("ISO: Joliet root LBA missing"))?;
            let size = joliet_root_size
                .ok_or_else(|| OxiArcError::invalid_header("ISO: Joliet root size missing"))?;
            (lba, size)
        } else {
            let lba = pvd_root_lba
                .ok_or_else(|| OxiArcError::invalid_header("ISO: no PVD found in image"))?;
            let size = pvd_root_size
                .ok_or_else(|| OxiArcError::invalid_header("ISO: PVD root size missing"))?;
            (lba, size)
        };

        let mut entries = Vec::new();
        walk_directory(
            &mut reader,
            root_lba,
            root_size as u64,
            String::new(),
            use_joliet,
            &mut entries,
        )?;

        Ok(IsoReader {
            reader,
            entries,
            joliet: use_joliet,
            volume_id,
            total_lbas,
            logical_block_size,
        })
    }

    /// Return a slice of all entries (files and directories).
    pub fn entries(&self) -> &[IsoEntry] {
        &self.entries
    }

    /// Return `true` if the image has a Joliet SVD that is being used.
    pub fn is_joliet(&self) -> bool {
        self.joliet
    }

    /// Extract a file entry's data, writing it to `writer`.
    ///
    /// Returns the number of bytes written.
    pub fn extract(&mut self, entry: &IsoEntry, writer: &mut dyn Write) -> Result<u64> {
        if entry.is_dir {
            return Err(OxiArcError::invalid_header(format!(
                "ISO: '{}' is a directory, cannot extract",
                entry.name
            )));
        }

        let byte_offset = (entry.lba as u64) * SECTOR_SIZE;
        self.reader
            .seek(SeekFrom::Start(byte_offset))
            .map_err(|e| {
                OxiArcError::invalid_header(format!("ISO: seek to data LBA failed: {e}"))
            })?;

        let mut remaining = entry.size;
        let mut buf = [0u8; 8192];
        let mut written = 0u64;

        while remaining > 0 {
            let to_read = (remaining as usize).min(buf.len());
            let n = self.reader.read(&mut buf[..to_read]).map_err(|e| {
                OxiArcError::invalid_header(format!(
                    "ISO: read data for '{}' failed: {e}",
                    entry.name
                ))
            })?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n]).map_err(|e| {
                OxiArcError::invalid_header(format!(
                    "ISO: write data for '{}' failed: {e}",
                    entry.name
                ))
            })?;
            written += n as u64;
            remaining -= n as u64;
        }

        Ok(written)
    }
}

/// Recursively walk a directory extent, populating `out` with entries.
fn walk_directory<R: Read + Seek>(
    reader: &mut R,
    dir_lba: u32,
    dir_size: u64,
    prefix: String,
    joliet: bool,
    out: &mut Vec<IsoEntry>,
) -> Result<()> {
    let byte_offset = (dir_lba as u64) * SECTOR_SIZE;
    reader.seek(SeekFrom::Start(byte_offset)).map_err(|e| {
        OxiArcError::invalid_header(format!("ISO: seek to dir LBA {dir_lba} failed: {e}"))
    })?;

    // Read the entire directory extent into a buffer (size is typically small)
    let buf_size = dir_size as usize;
    let mut buf = vec![0u8; buf_size];
    reader
        .read_exact(&mut buf)
        .map_err(|e| OxiArcError::invalid_header(format!("ISO: read dir extent failed: {e}")))?;

    let mut offset = 0usize;
    let mut subdirs: Vec<(u32, u64, String)> = Vec::new();

    while offset < buf_size {
        // Check if we've hit a sector boundary with LEN_DR == 0 (padding)
        if buf[offset] == 0 {
            // Advance to the next 2048-byte sector boundary
            let next_sector = ((offset / 2048) + 1) * 2048;
            if next_sector >= buf_size {
                break;
            }
            offset = next_sector;
            continue;
        }

        match parse_dir_record(&buf, offset, joliet) {
            None => {
                // Padding or truncation — advance sector
                let next_sector = ((offset / 2048) + 1) * 2048;
                if next_sector >= buf_size {
                    break;
                }
                offset = next_sector;
            }
            Some((record, consumed)) => {
                offset += consumed;

                // Skip "." and ".." (empty name set by parse_dir_record)
                if record.name.is_empty() {
                    continue;
                }

                let full_path = if prefix.is_empty() {
                    record.name.clone()
                } else {
                    format!("{}/{}", prefix, record.name)
                };

                if record.is_dir {
                    out.push(IsoEntry {
                        name: full_path.clone(),
                        lba: record.lba,
                        size: record.size,
                        is_dir: true,
                    });
                    // Queue subdirectory for recursion after we finish this dir
                    subdirs.push((record.lba, record.size, full_path));
                } else {
                    // Normalize ASCII names to lowercase for PVD mode
                    let display_name = if joliet {
                        full_path
                    } else {
                        full_path.to_lowercase()
                    };
                    out.push(IsoEntry {
                        name: display_name,
                        lba: record.lba,
                        size: record.size,
                        is_dir: false,
                    });
                }
            }
        }
    }

    // Recurse into subdirectories
    for (sub_lba, sub_size, sub_path) in subdirs {
        walk_directory(reader, sub_lba, sub_size, sub_path, joliet, out)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso9660::joliet::decode_ucs2_be;
    use std::io::Cursor;

    /// Build a minimal hand-crafted 24-LBA ISO 9660 image for testing.
    ///
    /// Layout:
    /// - LBAs 0-15: system area (zeros)
    /// - LBA 16: Primary Volume Descriptor
    /// - LBA 17: Joliet Supplementary Volume Descriptor
    /// - LBA 18: Volume Descriptor Set Terminator
    /// - LBA 19: Path Table
    /// - LBA 20: PVD root directory (ASCII: HELLO.TXT;1, WORLD.TXT;1)
    /// - LBA 21: Joliet root directory (UCS-2 BE: hello.txt, world.txt)
    /// - LBA 22: File data for hello.txt ("hello\n")
    /// - LBA 23: File data for world.txt ("world\n")
    fn build_minimal_iso() -> Vec<u8> {
        let total_lbas = 24u32;
        let mut iso = vec![0u8; (total_lbas as usize) * 2048];

        // ── LBA 16: Primary Volume Descriptor ────────────────────────────────
        {
            let pvd = &mut iso[16 * 2048..17 * 2048];
            pvd[0] = 1; // type: Primary
            pvd[1..6].copy_from_slice(b"CD001");
            pvd[6] = 1; // version

            // System Identifier (bytes 8-39): space-padded A-chars
            for b in pvd[8..40].iter_mut() {
                *b = b' ';
            }
            pvd[8..14].copy_from_slice(b"OXIARC");

            // Volume Identifier (bytes 40-71): space-padded D-chars
            for b in pvd[40..72].iter_mut() {
                *b = b' ';
            }
            pvd[40..47].copy_from_slice(b"TESTVOL");

            // Volume Space Size (BDWORD at 80-87) = total_lbas
            pvd[80..84].copy_from_slice(&total_lbas.to_le_bytes());
            pvd[84..88].copy_from_slice(&total_lbas.to_be_bytes());

            // Logical Block Size (BWORD at 128-131) = 2048
            pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
            pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());

            // Path Table Size (BDWORD at 132-139) = 28
            let pt_size = 28u32;
            pvd[132..136].copy_from_slice(&pt_size.to_le_bytes());
            pvd[136..140].copy_from_slice(&pt_size.to_be_bytes());

            // L Path Table at LBA 19
            pvd[140..144].copy_from_slice(&19u32.to_le_bytes());
            // M Path Table at LBA 19
            pvd[148..152].copy_from_slice(&19u32.to_be_bytes());

            // Root Directory Record (bytes 156-189, 34 bytes)
            // Points to PVD root dir at LBA 20, size = 256 bytes (2 sectors of small records)
            write_dir_record_dot(pvd, 156, 20u32, 256u32);
        }

        // ── LBA 17: Joliet Supplementary Volume Descriptor ───────────────────
        {
            let svd = &mut iso[17 * 2048..18 * 2048];
            svd[0] = 2; // type: Supplementary
            svd[1..6].copy_from_slice(b"CD001");
            svd[6] = 1;

            // Escape sequences for Joliet UCS-2 Level 3: %/E
            svd[88] = 0x25;
            svd[89] = 0x2F;
            svd[90] = 0x45;

            // Volume Space Size
            svd[80..84].copy_from_slice(&total_lbas.to_le_bytes());
            svd[84..88].copy_from_slice(&total_lbas.to_be_bytes());

            // Logical Block Size
            svd[128..130].copy_from_slice(&2048u16.to_le_bytes());
            svd[130..132].copy_from_slice(&2048u16.to_be_bytes());

            // Root Directory Record points to Joliet dir at LBA 21
            write_dir_record_dot(svd, 156, 21u32, 256u32);
        }

        // ── LBA 18: Volume Descriptor Set Terminator ──────────────────────────
        {
            let term = &mut iso[18 * 2048..19 * 2048];
            term[0] = 255; // type: Terminator
            term[1..6].copy_from_slice(b"CD001");
            term[6] = 1;
        }

        // ── LBA 19: Path Table (minimal) ──────────────────────────────────────
        {
            let pt = &mut iso[19 * 2048..20 * 2048];
            pt[0] = 1; // dir id length
            pt[1] = 0; // ext attr
            pt[2..6].copy_from_slice(&20u32.to_le_bytes()); // root dir LBA
            pt[6..8].copy_from_slice(&1u16.to_le_bytes()); // parent dir #1
            pt[8] = 0x00; // root dir id = NUL
            pt[9] = 0x00; // padding
        }

        // ── LBA 20: PVD root directory ────────────────────────────────────────
        // Contains . and .. records, plus HELLO.TXT;1 and WORLD.TXT;1
        {
            let dir = &mut iso[20 * 2048..21 * 2048];
            let mut pos = 0usize;

            // "." self-referencing record
            let dot_len = write_dot_record(&mut dir[pos..], 20u32, 256u32);
            pos += dot_len;

            // ".." parent record
            let dotdot_len = write_dotdot_record(&mut dir[pos..], 20u32, 256u32);
            pos += dotdot_len;

            // HELLO.TXT;1 at LBA 22, size=6
            let fi1 = b"HELLO.TXT;1";
            let dr1_len = write_file_record(&mut dir[pos..], fi1, 22u32, 6u32);
            pos += dr1_len;

            // WORLD.TXT;1 at LBA 23, size=6
            let fi2 = b"WORLD.TXT;1";
            let _dr2_len = write_file_record(&mut dir[pos..], fi2, 23u32, 6u32);
        }

        // ── LBA 21: Joliet root directory ─────────────────────────────────────
        // Contains . and .. records, plus UCS-2 BE "hello.txt" and "world.txt"
        {
            let dir = &mut iso[21 * 2048..22 * 2048];
            let mut pos = 0usize;

            // "." self-referencing record
            let dot_len = write_dot_record(&mut dir[pos..], 21u32, 256u32);
            pos += dot_len;

            // ".." parent record
            let dotdot_len = write_dotdot_record(&mut dir[pos..], 21u32, 256u32);
            pos += dotdot_len;

            // "hello.txt" in UCS-2 BE: 9 chars × 2 bytes = 18 bytes (even → no padding)
            let fi1_joliet = encode_ucs2_be("hello.txt");
            let dr1_len = write_file_record_joliet(&mut dir[pos..], &fi1_joliet, 22u32, 6u32);
            pos += dr1_len;

            // "world.txt" in UCS-2 BE: 9 chars × 2 bytes = 18 bytes (even → no padding)
            let fi2_joliet = encode_ucs2_be("world.txt");
            let _dr2_len = write_file_record_joliet(&mut dir[pos..], &fi2_joliet, 23u32, 6u32);
        }

        // ── LBA 22: File data for hello.txt / HELLO.TXT ───────────────────────
        {
            let data = &mut iso[22 * 2048..23 * 2048];
            data[..6].copy_from_slice(b"hello\n");
        }

        // ── LBA 23: File data for world.txt / WORLD.TXT ───────────────────────
        {
            let data = &mut iso[23 * 2048..24 * 2048];
            data[..6].copy_from_slice(b"world\n");
        }

        iso
    }

    /// Encode an ASCII string as UCS-2 Big Endian bytes.
    fn encode_ucs2_be(s: &str) -> Vec<u8> {
        s.chars()
            .flat_map(|c| {
                let u = c as u16;
                [(u >> 8) as u8, u as u8]
            })
            .collect()
    }

    /// Write a 34-byte "." directory record into `buf` at byte 0.
    /// Returns the record length (34).
    fn write_dot_record(buf: &mut [u8], lba: u32, size: u32) -> usize {
        buf[0] = 34; // LEN_DR
        buf[1] = 0;
        buf[2..6].copy_from_slice(&lba.to_le_bytes());
        buf[6..10].copy_from_slice(&lba.to_be_bytes());
        buf[10..14].copy_from_slice(&size.to_le_bytes());
        buf[14..18].copy_from_slice(&size.to_be_bytes());
        buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]); // date 2026-05-06
        buf[25] = 0x02; // directory flag
        buf[28..30].copy_from_slice(&1u16.to_le_bytes());
        buf[30..32].copy_from_slice(&1u16.to_be_bytes());
        buf[32] = 1;
        buf[33] = 0x00; // "." identifier
        34
    }

    /// Write a 34-byte ".." directory record into `buf` at byte 0.
    /// Returns the record length (34).
    fn write_dotdot_record(buf: &mut [u8], lba: u32, size: u32) -> usize {
        buf[0] = 34; // LEN_DR
        buf[1] = 0;
        buf[2..6].copy_from_slice(&lba.to_le_bytes());
        buf[6..10].copy_from_slice(&lba.to_be_bytes());
        buf[10..14].copy_from_slice(&size.to_le_bytes());
        buf[14..18].copy_from_slice(&size.to_be_bytes());
        buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
        buf[25] = 0x02; // directory flag
        buf[28..30].copy_from_slice(&1u16.to_le_bytes());
        buf[30..32].copy_from_slice(&1u16.to_be_bytes());
        buf[32] = 1;
        buf[33] = 0x01; // ".." identifier
        34
    }

    /// Write a file directory record for PVD (ASCII FI) into `buf`.
    /// Returns the record length.
    fn write_file_record(buf: &mut [u8], fi: &[u8], lba: u32, size: u32) -> usize {
        let len_fi = fi.len() as u8;
        // If LEN_FI is even, add 1 padding byte so LEN_DR is even
        let padding = if len_fi % 2 == 0 { 1u8 } else { 0u8 };
        let len_dr = 33u8 + len_fi + padding;

        buf[0] = len_dr;
        buf[1] = 0;
        buf[2..6].copy_from_slice(&lba.to_le_bytes());
        buf[6..10].copy_from_slice(&lba.to_be_bytes());
        buf[10..14].copy_from_slice(&size.to_le_bytes());
        buf[14..18].copy_from_slice(&size.to_be_bytes());
        buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
        buf[25] = 0x00; // file flag
        buf[28..30].copy_from_slice(&1u16.to_le_bytes());
        buf[30..32].copy_from_slice(&1u16.to_be_bytes());
        buf[32] = len_fi;
        buf[33..33 + len_fi as usize].copy_from_slice(fi);

        len_dr as usize
    }

    /// Write a file directory record for Joliet (UCS-2 BE FI) into `buf`.
    /// Returns the record length.
    fn write_file_record_joliet(buf: &mut [u8], fi: &[u8], lba: u32, size: u32) -> usize {
        let len_fi = fi.len() as u8;
        // Joliet FI lengths are always even (UCS-2 pairs), so padding if even → 1 byte
        let padding = if len_fi % 2 == 0 { 1u8 } else { 0u8 };
        let len_dr = 33u8 + len_fi + padding;

        buf[0] = len_dr;
        buf[1] = 0;
        buf[2..6].copy_from_slice(&lba.to_le_bytes());
        buf[6..10].copy_from_slice(&lba.to_be_bytes());
        buf[10..14].copy_from_slice(&size.to_le_bytes());
        buf[14..18].copy_from_slice(&size.to_be_bytes());
        buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
        buf[25] = 0x00; // file flag
        buf[28..30].copy_from_slice(&1u16.to_le_bytes());
        buf[30..32].copy_from_slice(&1u16.to_be_bytes());
        buf[32] = len_fi;
        buf[33..33 + len_fi as usize].copy_from_slice(fi);

        len_dr as usize
    }

    /// Write a 34-byte "." root directory record at `offset` within a VD sector buffer.
    fn write_dir_record_dot(sector: &mut [u8], offset: usize, lba: u32, size: u32) {
        let r = &mut sector[offset..offset + 34];
        r[0] = 34;
        r[1] = 0;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[6..10].copy_from_slice(&lba.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
        r[25] = 0x02; // directory
        r[28..30].copy_from_slice(&1u16.to_le_bytes());
        r[30..32].copy_from_slice(&1u16.to_be_bytes());
        r[32] = 1;
        r[33] = 0x00;
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    #[test]
    fn test_iso_detect_magic_at_lba_16() {
        use crate::detect::ArchiveFormat;
        let iso = build_minimal_iso();
        let mut cursor = Cursor::new(iso);
        let (format, _) = ArchiveFormat::detect(&mut cursor).expect("detect failed");
        assert_eq!(format, ArchiveFormat::Iso9660);
    }

    #[test]
    fn test_iso_pvd_parses() {
        let iso = build_minimal_iso();
        let reader = IsoReader::new(Cursor::new(iso)).expect("IsoReader::new failed");
        assert_eq!(reader.volume_id.trim(), "TESTVOL");
        assert_eq!(reader.logical_block_size, 2048);
        assert_eq!(reader.total_lbas, 24);
    }

    #[test]
    fn test_iso_joliet_filename_decode() {
        let bytes = b"\x00h\x00e\x00l\x00l\x00o";
        let s = decode_ucs2_be(bytes);
        assert_eq!(s, "hello");
    }

    #[test]
    fn test_iso_directory_record_walk() {
        let iso = build_minimal_iso();
        let reader = IsoReader::new(Cursor::new(iso)).expect("IsoReader::new failed");
        let files: Vec<_> = reader.entries().iter().filter(|e| !e.is_dir).collect();
        assert_eq!(
            files.len(),
            2,
            "expected 2 files, got {:?}",
            files.iter().map(|e| &e.name).collect::<Vec<_>>()
        );
        let names: Vec<_> = files.iter().map(|e| e.name.to_lowercase()).collect();
        assert!(
            names.iter().any(|n| n.contains("hello")),
            "hello not found: {:?}",
            names
        );
        assert!(
            names.iter().any(|n| n.contains("world")),
            "world not found: {:?}",
            names
        );
    }

    #[test]
    fn test_iso_extract_file_content() {
        let iso = build_minimal_iso();
        let mut reader = IsoReader::new(Cursor::new(iso)).expect("IsoReader::new failed");
        let entry = reader
            .entries()
            .iter()
            .find(|e| !e.is_dir && e.name.to_lowercase().contains("hello"))
            .cloned()
            .expect("hello entry not found");

        let mut out = Vec::new();
        reader.extract(&entry, &mut out).expect("extract failed");
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn test_iso_extract_world_content() {
        let iso = build_minimal_iso();
        let mut reader = IsoReader::new(Cursor::new(iso)).expect("IsoReader::new failed");
        let entry = reader
            .entries()
            .iter()
            .find(|e| !e.is_dir && e.name.to_lowercase().contains("world"))
            .cloned()
            .expect("world entry not found");

        let mut out = Vec::new();
        reader.extract(&entry, &mut out).expect("extract failed");
        assert_eq!(out, b"world\n");
    }

    #[test]
    fn test_iso_level1_fallback() {
        // Build a minimal ISO with PVD only — no Joliet SVD
        let total_lbas = 22u32;
        let mut iso = vec![0u8; (total_lbas as usize) * 2048];

        // LBA 16: PVD
        {
            let pvd = &mut iso[16 * 2048..17 * 2048];
            pvd[0] = 1;
            pvd[1..6].copy_from_slice(b"CD001");
            pvd[6] = 1;
            for b in pvd[40..72].iter_mut() {
                *b = b' ';
            }
            pvd[40..47].copy_from_slice(b"NOJOLT!");
            pvd[80..84].copy_from_slice(&total_lbas.to_le_bytes());
            pvd[84..88].copy_from_slice(&total_lbas.to_be_bytes());
            pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
            pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
            write_dir_record_dot_local(pvd, 156, 18u32, 128u32);
        }

        // LBA 17: Terminator
        {
            let term = &mut iso[17 * 2048..18 * 2048];
            term[0] = 255;
            term[1..6].copy_from_slice(b"CD001");
            term[6] = 1;
        }

        // LBA 18: Root directory with one ASCII file
        {
            let dir = &mut iso[18 * 2048..19 * 2048];
            let mut pos = 0;
            // "."
            dir[pos] = 34;
            dir[pos + 1] = 0;
            dir[pos + 2..pos + 6].copy_from_slice(&18u32.to_le_bytes());
            dir[pos + 6..pos + 10].copy_from_slice(&18u32.to_be_bytes());
            dir[pos + 10..pos + 14].copy_from_slice(&128u32.to_le_bytes());
            dir[pos + 14..pos + 18].copy_from_slice(&128u32.to_be_bytes());
            dir[pos + 18..pos + 25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
            dir[pos + 25] = 0x02;
            dir[pos + 28..pos + 30].copy_from_slice(&1u16.to_le_bytes());
            dir[pos + 30..pos + 32].copy_from_slice(&1u16.to_be_bytes());
            dir[pos + 32] = 1;
            dir[pos + 33] = 0x00;
            pos += 34;

            // ".."
            dir[pos] = 34;
            dir[pos + 1] = 0;
            dir[pos + 2..pos + 6].copy_from_slice(&18u32.to_le_bytes());
            dir[pos + 6..pos + 10].copy_from_slice(&18u32.to_be_bytes());
            dir[pos + 10..pos + 14].copy_from_slice(&128u32.to_le_bytes());
            dir[pos + 14..pos + 18].copy_from_slice(&128u32.to_be_bytes());
            dir[pos + 18..pos + 25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
            dir[pos + 25] = 0x02;
            dir[pos + 28..pos + 30].copy_from_slice(&1u16.to_le_bytes());
            dir[pos + 30..pos + 32].copy_from_slice(&1u16.to_be_bytes());
            dir[pos + 32] = 1;
            dir[pos + 33] = 0x01;
            pos += 34;

            // FILE.TXT;1 at LBA 21, size=4
            let fi = b"FILE.TXT;1";
            let len_fi = fi.len() as u8; // 10 bytes, even → pad
            let padding = 1u8;
            let len_dr = 33 + len_fi + padding;
            dir[pos] = len_dr;
            dir[pos + 1] = 0;
            dir[pos + 2..pos + 6].copy_from_slice(&21u32.to_le_bytes());
            dir[pos + 6..pos + 10].copy_from_slice(&21u32.to_be_bytes());
            dir[pos + 10..pos + 14].copy_from_slice(&4u32.to_le_bytes());
            dir[pos + 14..pos + 18].copy_from_slice(&4u32.to_be_bytes());
            dir[pos + 18..pos + 25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
            dir[pos + 25] = 0x00; // file
            dir[pos + 28..pos + 30].copy_from_slice(&1u16.to_le_bytes());
            dir[pos + 30..pos + 32].copy_from_slice(&1u16.to_be_bytes());
            dir[pos + 32] = len_fi;
            dir[pos + 33..pos + 33 + fi.len()].copy_from_slice(fi);
        }

        // LBA 21: file data "test"
        {
            let data = &mut iso[21 * 2048..22 * 2048];
            data[..4].copy_from_slice(b"test");
        }

        let reader = IsoReader::new(Cursor::new(iso)).expect("IsoReader::new failed");
        assert!(!reader.is_joliet());
        let files: Vec<_> = reader.entries().iter().filter(|e| !e.is_dir).collect();
        assert_eq!(files.len(), 1);
        // ASCII name should be lowercased
        assert_eq!(files[0].name, "file.txt");
    }

    fn write_dir_record_dot_local(sector: &mut [u8], offset: usize, lba: u32, size: u32) {
        let r = &mut sector[offset..offset + 34];
        r[0] = 34;
        r[1] = 0;
        r[2..6].copy_from_slice(&lba.to_le_bytes());
        r[6..10].copy_from_slice(&lba.to_be_bytes());
        r[10..14].copy_from_slice(&size.to_le_bytes());
        r[14..18].copy_from_slice(&size.to_be_bytes());
        r[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]);
        r[25] = 0x02;
        r[28..30].copy_from_slice(&1u16.to_le_bytes());
        r[30..32].copy_from_slice(&1u16.to_be_bytes());
        r[32] = 1;
        r[33] = 0x00;
    }
}

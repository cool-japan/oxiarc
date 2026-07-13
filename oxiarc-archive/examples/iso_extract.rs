//! Read a minimal ISO 9660 image with [`IsoReader`] and extract its files.
//!
//! `oxiarc-archive` only *reads* ISO 9660 images (no writer), so this example
//! hand-crafts a tiny, spec-compliant 24-sector image in memory (mirroring
//! the crate's own internal test fixture) rather than requiring a real
//! `.iso` file on disk, then walks it with [`IsoReader`].
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-archive --example iso_extract
//! ```

use oxiarc_archive::detect::ArchiveFormat;
use oxiarc_archive::iso9660::IsoReader;
use std::io::Cursor;

const SECTOR: usize = 2048;

/// Write a 34-byte ECMA-119 directory record for `.`/`..`/a plain file.
fn write_dir_record(buf: &mut [u8], fi: &[u8], lba: u32, size: u32, is_dir: bool) -> usize {
    let len_fi = fi.len().max(1) as u8;
    let padding = if len_fi % 2 == 0 { 1u8 } else { 0u8 };
    let len_dr = 33 + len_fi + padding;

    buf[0] = len_dr;
    buf[1] = 0;
    buf[2..6].copy_from_slice(&lba.to_le_bytes());
    buf[6..10].copy_from_slice(&lba.to_be_bytes());
    buf[10..14].copy_from_slice(&size.to_le_bytes());
    buf[14..18].copy_from_slice(&size.to_be_bytes());
    buf[18..25].copy_from_slice(&[126, 5, 6, 0, 0, 0, 0]); // recording date (2026-05-06)
    buf[25] = if is_dir { 0x02 } else { 0x00 };
    buf[28..30].copy_from_slice(&1u16.to_le_bytes());
    buf[30..32].copy_from_slice(&1u16.to_be_bytes());
    buf[32] = len_fi;
    if fi.is_empty() {
        buf[33] = 0; // "." identifier byte
    } else {
        buf[33..33 + fi.len()].copy_from_slice(fi);
    }
    len_dr as usize
}

/// Build a minimal, hand-crafted ISO 9660 image containing one file:
/// `HELLO.TXT;1` with the contents `b"hello, iso9660!\n"`.
fn build_minimal_iso() -> Vec<u8> {
    let total_lbas = 20u32;
    let mut iso = vec![0u8; total_lbas as usize * SECTOR];
    let file_data = b"hello, iso9660!\n";

    // LBA 16: Primary Volume Descriptor
    {
        let pvd = &mut iso[16 * SECTOR..17 * SECTOR];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        for b in pvd[40..72].iter_mut() {
            *b = b' ';
        }
        pvd[40..47].copy_from_slice(b"EXAMPLE");
        pvd[80..84].copy_from_slice(&total_lbas.to_le_bytes());
        pvd[84..88].copy_from_slice(&total_lbas.to_be_bytes());
        pvd[128..130].copy_from_slice(&(SECTOR as u16).to_le_bytes());
        pvd[130..132].copy_from_slice(&(SECTOR as u16).to_be_bytes());
        // Root directory record at bytes 156..190, root dir at LBA 18.
        write_dir_record(&mut pvd[156..190], &[], 18, SECTOR as u32, true);
    }

    // LBA 17: Volume Descriptor Set Terminator
    {
        let term = &mut iso[17 * SECTOR..18 * SECTOR];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
    }

    // LBA 18: root directory (".", "..", "HELLO.TXT;1")
    {
        let dir = &mut iso[18 * SECTOR..19 * SECTOR];
        let mut pos = 0usize;
        pos += write_dir_record(&mut dir[pos..], &[], 18, SECTOR as u32, true);
        pos += write_dir_record(&mut dir[pos..], &[1], 18, SECTOR as u32, true);
        write_dir_record(
            &mut dir[pos..],
            b"HELLO.TXT;1",
            19,
            file_data.len() as u32,
            false,
        );
    }

    // LBA 19: file data
    {
        let data = &mut iso[19 * SECTOR..20 * SECTOR];
        data[..file_data.len()].copy_from_slice(file_data);
    }

    iso
}

fn main() {
    let image = build_minimal_iso();
    println!("Built in-memory ISO 9660 image: {} bytes", image.len());

    // Sanity-check format auto-detection first (magic lives at LBA 16).
    let mut cursor = Cursor::new(image.clone());
    let (format, _) = ArchiveFormat::detect(&mut cursor).expect("detect ISO format");
    assert_eq!(format, ArchiveFormat::Iso9660);
    println!("Auto-detected format: {format}");

    let mut reader = IsoReader::new(Cursor::new(image)).expect("IsoReader::new");
    println!(
        "Volume '{}' (joliet: {})",
        reader.volume_id.trim(),
        reader.is_joliet()
    );

    let entries = reader.entries().to_vec();
    for entry in &entries {
        println!("  - {} ({} bytes)", entry.name, entry.size);
    }

    let hello = entries
        .iter()
        .find(|e| e.name.to_ascii_uppercase().starts_with("HELLO.TXT"))
        .expect("HELLO.TXT entry present");

    let mut extracted = Vec::new();
    reader
        .extract(hello, &mut extracted)
        .expect("extract HELLO.TXT");
    assert_eq!(extracted, b"hello, iso9660!\n");
    println!(
        "Extracted {}: {:?}",
        hello.name,
        String::from_utf8_lossy(&extracted)
    );
}

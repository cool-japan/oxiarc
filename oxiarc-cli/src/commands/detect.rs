//! Detect command implementation.

use oxiarc_archive::ArchiveFormat;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;

pub fn cmd_detect(file: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let f = File::open(file)?;
    let mut reader = BufReader::new(f);

    let (format, magic) = ArchiveFormat::detect(&mut reader)?;

    println!("File: {}", file.display());
    println!("Format: {}", format);
    println!("Extension: .{}", format.extension());
    println!("MIME type: {}", format.mime_type());
    println!("Magic bytes: {:02X?}", &magic[..magic.len().min(16)]);

    if format.is_archive() {
        println!("Type: Archive (multiple files)");
    } else if format.is_compression_only() {
        println!("Type: Compression (single file)");
    }

    Ok(())
}

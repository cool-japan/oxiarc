//! Info command implementation.

use oxiarc_archive::{ArchiveFormat, CabReader, SevenZReader, ZipReader};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::PathBuf;

pub fn cmd_info(archive: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    let (format, _) = ArchiveFormat::detect(&mut reader)?;
    let metadata = std::fs::metadata(archive)?;

    println!("Archive Information");
    println!("===================");
    println!("File: {}", archive.display());
    println!("Format: {}", format);
    println!("Size: {} bytes", metadata.len());
    println!("MIME type: {}", format.mime_type());

    reader.seek(SeekFrom::Start(0))?;

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let entries = zip.entries();
            let total_size: u64 = entries.iter().map(|e| e.size).sum();
            let total_compressed: u64 = entries.iter().map(|e| e.compressed_size).sum();

            println!();
            println!("Contents:");
            println!(
                "  Files: {}",
                entries.iter().filter(|e| e.is_file()).count()
            );
            println!(
                "  Directories: {}",
                entries.iter().filter(|e| e.is_dir()).count()
            );
            println!("  Total size: {} bytes", total_size);
            println!("  Compressed size: {} bytes", total_compressed);
            if total_size > 0 {
                println!(
                    "  Compression ratio: {:.1}%",
                    (1.0 - total_compressed as f64 / total_size as f64) * 100.0
                );
            }
        }
        ArchiveFormat::Gzip => {
            let gzip = oxiarc_archive::GzipReader::new(reader)?;
            let header = gzip.header();

            println!();
            println!("GZIP Header:");
            if let Some(name) = &header.filename {
                println!("  Original filename: {}", name);
            }
            if header.mtime > 0 {
                println!("  Modification time: {} (Unix timestamp)", header.mtime);
            }
        }
        ArchiveFormat::SevenZip => {
            let sevenz = SevenZReader::new(reader)?;
            let entries = sevenz.entries();
            let total_size: u64 = entries.iter().map(|e| e.size).sum();

            println!();
            println!("Contents:");
            println!(
                "  Files: {}",
                entries.iter().filter(|e| e.is_file()).count()
            );
            println!(
                "  Directories: {}",
                entries.iter().filter(|e| e.is_dir()).count()
            );
            println!("  Total size: {} bytes", total_size);
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let (major, minor) = cab.version();
            let entries = cab.entries();
            let total_size: u64 = entries.iter().map(|e| e.size).sum();

            println!();
            println!("Cabinet Info:");
            println!("  Version: {}.{}", major, minor);
            println!("  Folders: {}", cab.num_folders());
            println!("  Cabinet size: {} bytes", cab.cabinet_size());
            println!();
            println!("Contents:");
            println!(
                "  Files: {}",
                entries.iter().filter(|e| e.is_file()).count()
            );
            println!(
                "  Directories: {}",
                entries.iter().filter(|e| e.is_dir()).count()
            );
            println!("  Total size: {} bytes", total_size);
        }
        _ => {}
    }

    Ok(())
}

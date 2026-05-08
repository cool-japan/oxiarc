use crate::style::Styler;
use oxiarc_archive::{ArchiveFormat, CabReader, IsoReader, SevenZReader, ZipReader};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::PathBuf;

pub fn cmd_info(archive: &PathBuf, styler: &Styler) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    let (format, _) = ArchiveFormat::detect(&mut reader)?;
    let metadata = std::fs::metadata(archive)?;

    println!("{}", styler.header("Archive Information"));
    println!("{}", styler.header("==================="));
    println!("File: {}", styler.path(&archive.display().to_string()));
    println!("Format: {}", format);
    println!(
        "Size: {}",
        styler.size(&format!("{} bytes", metadata.len()))
    );
    println!("MIME type: {}", format.mime_type());

    reader.seek(SeekFrom::Start(0))?;

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let entries = zip.entries();
            let total_size: u64 = entries.iter().map(|e| e.size).sum();
            let total_compressed: u64 = entries.iter().map(|e| e.compressed_size).sum();

            println!();
            println!("{}", styler.header("Contents:"));
            println!(
                "  Files: {}",
                entries.iter().filter(|e| e.is_file()).count()
            );
            println!(
                "  Directories: {}",
                entries.iter().filter(|e| e.is_dir()).count()
            );
            println!(
                "  Total size: {}",
                styler.size(&format!("{total_size} bytes"))
            );
            println!(
                "  Compressed size: {}",
                styler.size(&format!("{total_compressed} bytes"))
            );
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
            println!("{}", styler.header("GZIP Header:"));
            if let Some(name) = &header.filename {
                println!("  Original filename: {}", styler.path(name));
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
            println!("{}", styler.header("Contents:"));
            println!(
                "  Files: {}",
                entries.iter().filter(|e| e.is_file()).count()
            );
            println!(
                "  Directories: {}",
                entries.iter().filter(|e| e.is_dir()).count()
            );
            println!(
                "  Total size: {}",
                styler.size(&format!("{total_size} bytes"))
            );
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let (major, minor) = cab.version();
            let entries = cab.entries();
            let total_size: u64 = entries.iter().map(|e| e.size).sum();

            println!();
            println!("{}", styler.header("Cabinet Info:"));
            println!("  Version: {}.{}", major, minor);
            println!("  Folders: {}", cab.num_folders());
            println!(
                "  Cabinet size: {}",
                styler.size(&format!("{} bytes", cab.cabinet_size()))
            );
            println!();
            println!("{}", styler.header("Contents:"));
            println!(
                "  Files: {}",
                entries.iter().filter(|e| e.is_file()).count()
            );
            println!(
                "  Directories: {}",
                entries.iter().filter(|e| e.is_dir()).count()
            );
            println!(
                "  Total size: {}",
                styler.size(&format!("{total_size} bytes"))
            );
        }
        ArchiveFormat::Iso9660 => {
            let iso = IsoReader::new(reader)?;
            let file_count = iso.entries().iter().filter(|e| !e.is_dir).count();
            let dir_count = iso.entries().iter().filter(|e| e.is_dir).count();
            let total_size: u64 = iso
                .entries()
                .iter()
                .filter(|e| !e.is_dir)
                .map(|e| e.size)
                .sum();

            println!();
            println!("{}", styler.header("ISO 9660 Image Info:"));
            println!("  Volume ID: {}", styler.path(iso.volume_id.trim()));
            println!("  Total LBAs: {}", iso.total_lbas);
            println!("  Logical block size: {} bytes", iso.logical_block_size);
            println!(
                "  Joliet extensions: {}",
                if iso.is_joliet() { "yes" } else { "no" }
            );
            println!();
            println!("{}", styler.header("Contents:"));
            println!("  Files: {}", file_count);
            println!("  Directories: {}", dir_count);
            println!(
                "  Total file data: {}",
                styler.size(&format!("{total_size} bytes"))
            );
        }
        _ => {}
    }

    Ok(())
}

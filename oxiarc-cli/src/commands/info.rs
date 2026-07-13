use crate::style::Styler;
use crate::utils::{input_display_name, open_input};
use oxiarc_archive::{ArchiveFormat, CabReader, IsoReader, SevenZReader, ZipReader};
use std::io::{Seek, SeekFrom};

pub fn cmd_info(
    archive: &str,
    quiet: bool,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = open_input(archive)?;

    // Determine the stream length by seeking to the end, then rewind. This
    // works uniformly for a file-backed reader and the in-memory stdin buffer,
    // avoiding a separate `fs::metadata` call (which cannot describe stdin).
    let size = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;

    // `detect_with_path` adds a filename-extension fallback for the magic-less
    // formats (raw Brotli `.br`, raw Snappy `.sz`); `-` (stdin) has no
    // extension, so it degrades to plain content detection.
    let (format, _) = ArchiveFormat::detect_with_path(&mut reader, archive)?;

    // An unrecognized input has no "archive information" to report, so fail
    // loudly (non-zero exit) instead of printing `Format: Unknown` and exiting
    // 0 — matching `list`/`test`. `detect` stays exempt: reporting `Unknown` is
    // literally that command's job.
    if format == ArchiveFormat::Unknown {
        return Err(format!(
            "unsupported or unrecognized archive format for {}: {}",
            input_display_name(archive),
            format
        )
        .into());
    }

    // --quiet suppresses the informational report but detection above still
    // runs, so an unreadable/unrecognized input is still reported as an error.
    if quiet {
        return Ok(());
    }

    println!("{}", styler.header("Archive Information"));
    println!("{}", styler.header("==================="));
    println!("File: {}", styler.path(&input_display_name(archive)));
    println!("Format: {}", format);
    println!("Size: {}", styler.size(&format!("{} bytes", size)));
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

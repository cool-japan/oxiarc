//! Test command implementation.

use oxiarc_archive::{
    ArchiveFormat, Bzip2Reader, CabReader, Lz4Reader, SevenZReader, ZipReader, ZstdReader,
};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::PathBuf;

pub fn cmd_test(archive: &PathBuf, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    let (format, _) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    println!("Testing {} ({})", archive.display(), format);

    let mut total_files = 0usize;
    let mut ok_count = 0usize;
    let mut error_count = 0usize;
    let mut errors: Vec<(String, String)> = Vec::new();

    match format {
        ArchiveFormat::Zip => {
            let mut zip = ZipReader::new(reader)?;
            let entries: Vec<_> = zip.entries().to_vec();

            for entry in &entries {
                if entry.is_dir() {
                    continue;
                }
                total_files += 1;

                match zip.extract(entry) {
                    Ok(_) => {
                        ok_count += 1;
                        if verbose {
                            println!("  OK: {}", entry.name);
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        errors.push((entry.name.clone(), e.to_string()));
                        if verbose {
                            println!("  FAILED: {} - {}", entry.name, e);
                        }
                    }
                }
            }
        }
        ArchiveFormat::Gzip => {
            total_files = 1;
            let mut gzip = oxiarc_archive::GzipReader::new(reader)?;
            let name = gzip
                .header()
                .filename
                .clone()
                .unwrap_or_else(|| "<unnamed>".to_string());

            match gzip.decompress() {
                Ok(_) => {
                    ok_count = 1;
                    if verbose {
                        println!("  OK: {}", name);
                    }
                }
                Err(e) => {
                    error_count = 1;
                    errors.push((name.clone(), e.to_string()));
                    if verbose {
                        println!("  FAILED: {} - {}", name, e);
                    }
                }
            }
        }
        ArchiveFormat::Tar => {
            let mut tar = oxiarc_archive::TarReader::new(reader)?;
            let entries: Vec<_> = tar.entries().to_vec();

            for entry in &entries {
                if entry.is_dir() {
                    continue;
                }
                total_files += 1;

                match tar.extract_to_vec(entry) {
                    Ok(_) => {
                        ok_count += 1;
                        if verbose {
                            println!("  OK: {}", entry.name);
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        errors.push((entry.name.clone(), e.to_string()));
                        if verbose {
                            println!("  FAILED: {} - {}", entry.name, e);
                        }
                    }
                }
            }
        }
        ArchiveFormat::Lzh => {
            let mut lzh = oxiarc_archive::LzhReader::new(reader)?;
            let entries: Vec<_> = lzh.entries().to_vec();

            for entry in &entries {
                if entry.is_dir() {
                    continue;
                }
                total_files += 1;

                match lzh.extract_to_vec(entry) {
                    Ok(_) => {
                        ok_count += 1;
                        if verbose {
                            println!("  OK: {}", entry.name);
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        errors.push((entry.name.clone(), e.to_string()));
                        if verbose {
                            println!("  FAILED: {} - {}", entry.name, e);
                        }
                    }
                }
            }
        }
        ArchiveFormat::Xz => {
            total_files = 1;
            let name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            match oxiarc_archive::xz::decompress(&mut reader) {
                Ok(_) => {
                    ok_count = 1;
                    if verbose {
                        println!("  OK: {}", name);
                    }
                }
                Err(e) => {
                    error_count = 1;
                    errors.push((name.clone(), e.to_string()));
                    if verbose {
                        println!("  FAILED: {} - {}", name, e);
                    }
                }
            }
        }
        ArchiveFormat::Lz4 => {
            total_files = 1;
            let name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let mut lz4 = Lz4Reader::new(reader)?;
            match lz4.decompress() {
                Ok(_) => {
                    ok_count = 1;
                    if verbose {
                        println!("  OK: {}", name);
                    }
                }
                Err(e) => {
                    error_count = 1;
                    errors.push((name.clone(), e.to_string()));
                    if verbose {
                        println!("  FAILED: {} - {}", name, e);
                    }
                }
            }
        }
        ArchiveFormat::Zstd => {
            total_files = 1;
            let name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let mut zstd = ZstdReader::new(reader)?;
            match zstd.decompress() {
                Ok(_) => {
                    ok_count = 1;
                    if verbose {
                        println!("  OK: {}", name);
                    }
                }
                Err(e) => {
                    error_count = 1;
                    errors.push((name.clone(), e.to_string()));
                    if verbose {
                        println!("  FAILED: {} - {}", name, e);
                    }
                }
            }
        }
        ArchiveFormat::Bzip2 => {
            total_files = 1;
            let name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            let mut bzip2 = Bzip2Reader::new(reader)?;
            match bzip2.decompress() {
                Ok(_) => {
                    ok_count = 1;
                    if verbose {
                        println!("  OK: {}", name);
                    }
                }
                Err(e) => {
                    error_count = 1;
                    errors.push((name.clone(), e.to_string()));
                    if verbose {
                        println!("  FAILED: {} - {}", name, e);
                    }
                }
            }
        }
        ArchiveFormat::SevenZip => {
            let mut sevenz = SevenZReader::new(reader)?;
            let entries: Vec<_> = sevenz.sevenz_entries().to_vec();

            for (i, entry) in entries.iter().enumerate() {
                if entry.is_dir {
                    continue;
                }
                total_files += 1;

                match sevenz.extract(i) {
                    Ok(_) => {
                        ok_count += 1;
                        if verbose {
                            println!("  OK: {}", entry.name);
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        errors.push((entry.name.clone(), e.to_string()));
                        if verbose {
                            println!("  FAILED: {} - {}", entry.name, e);
                        }
                    }
                }
            }
        }
        ArchiveFormat::Cab => {
            let mut cab = CabReader::new(reader)?;
            let entries: Vec<_> = cab.entries().to_vec();

            for entry in &entries {
                if entry.is_dir() {
                    continue;
                }
                total_files += 1;

                match cab.extract(entry) {
                    Ok(_) => {
                        ok_count += 1;
                        if verbose {
                            println!("  OK: {}", entry.name);
                        }
                    }
                    Err(e) => {
                        error_count += 1;
                        errors.push((entry.name.clone(), e.to_string()));
                        if verbose {
                            println!("  FAILED: {} - {}", entry.name, e);
                        }
                    }
                }
            }
        }
        _ => {
            println!("Testing not supported for {}", format);
            return Ok(());
        }
    }

    println!();
    println!("Test results:");
    println!("  Total files: {}", total_files);
    println!("  OK: {}", ok_count);
    println!("  Failed: {}", error_count);

    if !errors.is_empty() && !verbose {
        println!();
        println!("Errors:");
        for (name, err) in &errors {
            println!("  {}: {}", name, err);
        }
    }

    if error_count > 0 {
        std::process::exit(2);
    }

    println!();
    println!("All files OK");
    Ok(())
}

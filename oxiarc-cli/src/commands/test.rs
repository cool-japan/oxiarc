//! Test command implementation.

use crate::style::Styler;
use crate::utils::{input_display_name, open_input};
use oxiarc_archive::{
    ArchiveFormat, BrotliReader, Bzip2Reader, CabReader, Lz4Reader, SevenZReader, SnappyReader,
    ZipReader, ZstdReader,
};
use std::io::{Seek, SeekFrom};

pub fn cmd_test(
    archive: &str,
    verbose: bool,
    quiet: bool,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = open_input(archive)?;

    let (format, _) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    // Name used for single-stream (non-archive) formats that carry no internal
    // entry name. `<stdin>`'s stem is meaningless, so fall back to "stdin".
    let stem = if archive == "-" {
        "stdin".to_string()
    } else {
        std::path::Path::new(archive)
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };

    if !quiet {
        println!("Testing {} ({})", input_display_name(archive), format);
    }

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
            let name = stem.clone();

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
            let name = stem.clone();

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
            let name = stem.clone();

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
            let name = stem.clone();

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
        ArchiveFormat::Brotli => {
            total_files = 1;
            let name = stem.clone();

            let mut brotli = BrotliReader::new(reader)?;
            match brotli.decompress() {
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
        ArchiveFormat::Snappy => {
            total_files = 1;
            let name = stem.clone();

            let mut snappy = SnappyReader::new(reader)?;
            match snappy.decompress() {
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
            return Err(format!(
                "unsupported or unrecognized archive format for {}: {}",
                input_display_name(archive),
                format
            )
            .into());
        }
    }

    if !quiet {
        println!();
        println!("Test results:");
        println!("  Total files: {}", total_files);
        println!("  OK: {}", ok_count);
        println!("  Failed: {}", error_count);
    }

    // Surface the concrete failures whenever they were not already shown
    // inline (verbose mode prints each FAILED line as it happens). These are
    // genuine errors, so they are emitted even under --quiet.
    if !errors.is_empty() && !verbose {
        println!();
        println!("{}", styler.error("Errors:"));
        for (name, err) in &errors {
            println!("  {}: {}", styler.path(name), styler.error(err));
        }
    }

    if error_count > 0 {
        std::process::exit(2);
    }

    if !quiet {
        println!();
        println!("{}", styler.success("All files OK"));
    }
    Ok(())
}

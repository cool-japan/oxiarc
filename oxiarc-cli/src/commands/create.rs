//! Create command implementation.

use oxiarc_archive::{
    Bzip2Writer, Lz4Writer, LzhCompressionLevel, LzhWriter, TarWriter, XzWriter,
    ZipCompressionLevel, ZipWriter, ZstdWriter,
};
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;

/// Compression level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionLevel {
    /// Store without compression
    Store,
    /// Fast compression
    Fast,
    /// Normal compression (default)
    Normal,
    /// Best compression
    Best,
}

/// Output archive format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// ZIP archive
    Zip,
    /// TAR archive
    Tar,
    /// GZIP compressed file
    Gzip,
    /// LZH archive
    Lzh,
    /// XZ compressed file
    Xz,
    /// LZ4 compressed file
    Lz4,
    /// Bzip2 compressed file
    Bz2,
    /// Zstandard compressed file
    Zst,
}

pub fn cmd_create(
    archive: &str,
    files: &[PathBuf],
    format: Option<OutputFormat>,
    compression: CompressionLevel,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let to_stdout = archive == "-";

    // For stdout, format must be specified and only single-file formats are supported
    if to_stdout {
        if format.is_none() {
            return Err("--format is required when writing to stdout".into());
        }
        let fmt = format.ok_or("Format required for stdout")?;
        match fmt {
            OutputFormat::Gzip
            | OutputFormat::Xz
            | OutputFormat::Bz2
            | OutputFormat::Lz4
            | OutputFormat::Zst => {}
            _ => {
                return Err(
                    "Only single-file formats (gzip, xz, bz2, lz4, zst) are supported for stdout"
                        .into(),
                );
            }
        }
    }

    // Validate file input for single-file formats
    let single_file_format = matches!(
        format.unwrap_or(OutputFormat::Zip),
        OutputFormat::Gzip
            | OutputFormat::Xz
            | OutputFormat::Bz2
            | OutputFormat::Lz4
            | OutputFormat::Zst
    );

    // Read input data (either from stdin or from a single file for single-file formats)
    let (input_data, input_name): (Vec<u8>, String) = if files.is_empty() {
        // Read from stdin
        let mut stdin = io::stdin();
        let mut data = Vec::new();
        stdin.read_to_end(&mut data)?;
        (data, "stdin".to_string())
    } else if files.len() == 1 && (single_file_format || to_stdout) {
        // Read from single file for single-file formats or stdout
        let input_path = &files[0];
        if input_path.is_dir() {
            return Err(format!(
                "{:?} cannot compress directories directly. Use TAR first.",
                format.unwrap_or(OutputFormat::Zip)
            )
            .into());
        }
        let data = std::fs::read(input_path)?;
        let filename = input_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("data")
            .to_string();
        (data, filename)
    } else if !single_file_format {
        // For archive formats (ZIP, TAR, LZH), we don't pre-read
        (Vec::new(), String::new())
    } else {
        return Err("Single-file formats only support one file at a time".into());
    };

    // Determine format from extension if not specified
    let format = format.unwrap_or_else(|| {
        if to_stdout {
            OutputFormat::Gzip // Default for stdout if somehow not specified
        } else {
            let ext = PathBuf::from(archive)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            match ext.as_str() {
                "zip" => OutputFormat::Zip,
                "tar" => OutputFormat::Tar,
                "gz" | "gzip" => OutputFormat::Gzip,
                "lzh" | "lha" => OutputFormat::Lzh,
                "xz" => OutputFormat::Xz,
                "lz4" => OutputFormat::Lz4,
                "bz2" | "bzip2" => OutputFormat::Bz2,
                "zst" | "zstd" => OutputFormat::Zst,
                _ => OutputFormat::Zip, // Default to ZIP
            }
        }
    });

    if !to_stdout && verbose {
        eprintln!("Creating {:?} archive: {}", format, archive);
    }

    match format {
        OutputFormat::Zip => {
            if to_stdout {
                return Err(
                    "ZIP format cannot be written to stdout (use single-file formats)".into(),
                );
            }
            let file = File::create(archive)?;
            let writer = BufWriter::new(file);
            let mut zip = ZipWriter::new(writer);

            let level = match compression {
                CompressionLevel::Store => ZipCompressionLevel::Store,
                CompressionLevel::Fast => ZipCompressionLevel::Fast,
                CompressionLevel::Normal => ZipCompressionLevel::Normal,
                CompressionLevel::Best => ZipCompressionLevel::Best,
            };
            zip.set_compression(level);

            for path in files {
                add_path_to_zip(&mut zip, path, path, verbose)?;
            }

            zip.finish()?;
        }
        OutputFormat::Tar => {
            if to_stdout {
                return Err(
                    "TAR format cannot be written to stdout (use single-file formats)".into(),
                );
            }
            let file = File::create(archive)?;
            let writer = BufWriter::new(file);
            let mut tar = TarWriter::new(writer);

            for path in files {
                add_path_to_tar(&mut tar, path, path, verbose)?;
            }

            tar.finish()?;
        }
        OutputFormat::Gzip => {
            let level = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let compressed =
                oxiarc_archive::gzip::compress_with_filename(&input_data, &input_name, level)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&compressed)?;
                writer.flush()?;
            } else {
                std::fs::write(archive, &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
        OutputFormat::Lzh => {
            if to_stdout {
                return Err(
                    "LZH format cannot be written to stdout (use single-file formats)".into(),
                );
            }
            let file = File::create(archive)?;
            let writer = BufWriter::new(file);
            let mut lzh = LzhWriter::new(writer);

            // Note: LZH compression (lh5) is not fully production-ready yet
            // Using Store mode for now
            let level = match compression {
                CompressionLevel::Store => LzhCompressionLevel::Store,
                _ => LzhCompressionLevel::Store, // Fall back to Store for now
            };
            lzh.set_compression(level);

            for path in files {
                add_path_to_lzh(&mut lzh, path, path, verbose)?;
            }

            lzh.finish()?;
        }
        OutputFormat::Xz => {
            let level = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let xz_writer = XzWriter::new(oxiarc_lzma::LzmaLevel::new(level));
            let compressed = xz_writer.compress(&input_data)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&compressed)?;
                writer.flush()?;
            } else {
                std::fs::write(archive, &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
        OutputFormat::Lz4 => {
            let mut output = Vec::new();
            let mut lz4_writer = Lz4Writer::new(&mut output);
            lz4_writer.write_compressed(&input_data)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&output)?;
                writer.flush()?;
            } else {
                std::fs::write(archive, &output)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
        OutputFormat::Bz2 => {
            let level = match compression {
                CompressionLevel::Store => 1,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let bzip2_writer = Bzip2Writer::with_level(level);
            let compressed = bzip2_writer.compress(&input_data)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&compressed)?;
                writer.flush()?;
            } else {
                std::fs::write(archive, &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
        OutputFormat::Zst => {
            let zstd_writer = ZstdWriter::new();
            let compressed = zstd_writer.compress(&input_data)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&compressed)?;
                writer.flush()?;
            } else {
                std::fs::write(archive, &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
    }

    if !to_stdout && verbose {
        eprintln!("Archive created successfully");
    }
    Ok(())
}

fn add_path_to_zip<W: std::io::Write>(
    zip: &mut ZipWriter<W>,
    path: &PathBuf,
    base: &PathBuf,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if path.is_dir() {
        let name = path
            .strip_prefix(base.parent().unwrap_or(base))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        zip.add_directory(&name)?;
        if verbose {
            println!("  Added: {}/", name);
        }

        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            add_path_to_zip(zip, &entry.path(), base, verbose)?;
        }
    } else {
        let name = path
            .strip_prefix(base.parent().unwrap_or(base))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let data = std::fs::read(path)?;
        zip.add_file(&name, &data)?;
        if verbose {
            println!("  Added: {} ({} bytes)", name, data.len());
        }
    }
    Ok(())
}

fn add_path_to_tar<W: std::io::Write>(
    tar: &mut TarWriter<W>,
    path: &PathBuf,
    base: &PathBuf,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if path.is_dir() {
        let name = path
            .strip_prefix(base.parent().unwrap_or(base))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        tar.add_directory(&name)?;
        if verbose {
            println!("  Added: {}/", name);
        }

        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            add_path_to_tar(tar, &entry.path(), base, verbose)?;
        }
    } else {
        let name = path
            .strip_prefix(base.parent().unwrap_or(base))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let data = std::fs::read(path)?;
        tar.add_file(&name, &data)?;
        if verbose {
            println!("  Added: {} ({} bytes)", name, data.len());
        }
    }
    Ok(())
}

fn add_path_to_lzh<W: std::io::Write>(
    lzh: &mut LzhWriter<W>,
    path: &PathBuf,
    base: &PathBuf,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if path.is_dir() {
        let name = path
            .strip_prefix(base.parent().unwrap_or(base))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        lzh.add_directory(&name)?;
        if verbose {
            println!("  Added: {}/", name);
        }

        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            add_path_to_lzh(lzh, &entry.path(), base, verbose)?;
        }
    } else {
        let name = path
            .strip_prefix(base.parent().unwrap_or(base))
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let data = std::fs::read(path)?;
        lzh.add_file(&name, &data)?;
        if verbose {
            println!("  Added: {} ({} bytes)", name, data.len());
        }
    }
    Ok(())
}

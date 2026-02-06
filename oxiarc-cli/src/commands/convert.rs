//! Convert command implementation.

use crate::commands::create::{CompressionLevel, OutputFormat};
use crate::utils::ExtractedEntry;
use oxiarc_archive::{
    ArchiveFormat, Bzip2Reader, Bzip2Writer, CabReader, Lz4Reader, Lz4Writer, LzhCompressionLevel,
    LzhWriter, SevenZReader, TarWriter, XzWriter, ZipCompressionLevel, ZipReader, ZipWriter,
    ZstdReader, ZstdWriter,
};
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub fn cmd_convert(
    input: &PathBuf,
    output: &PathBuf,
    format: Option<OutputFormat>,
    compression: CompressionLevel,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Detect input format
    let file = File::open(input)?;
    let mut reader = BufReader::new(file);
    let (input_format, _) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    // Determine output format
    let output_format = format.unwrap_or_else(|| {
        let ext = output
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
            _ => OutputFormat::Zip,
        }
    });

    println!(
        "Converting {} ({}) to {} ({:?})",
        input.display(),
        input_format,
        output.display(),
        output_format
    );

    // Extract all entries from input archive
    let entries = extract_all_entries(&mut reader, input_format, input)?;

    if verbose {
        println!("  Found {} entries", entries.len());
    }

    // Write to output archive
    match output_format {
        OutputFormat::Zip => {
            let file = File::create(output)?;
            let writer = BufWriter::new(file);
            let mut zip = ZipWriter::new(writer);

            let level = match compression {
                CompressionLevel::Store => ZipCompressionLevel::Store,
                CompressionLevel::Fast => ZipCompressionLevel::Fast,
                CompressionLevel::Normal => ZipCompressionLevel::Normal,
                CompressionLevel::Best => ZipCompressionLevel::Best,
            };
            zip.set_compression(level);

            for (name, is_dir, data) in &entries {
                if *is_dir {
                    zip.add_directory(name)?;
                    if verbose {
                        println!("  Added: {}/", name);
                    }
                } else {
                    zip.add_file(name, data)?;
                    if verbose {
                        println!("  Added: {} ({} bytes)", name, data.len());
                    }
                }
            }

            zip.finish()?;
        }
        OutputFormat::Tar => {
            let file = File::create(output)?;
            let writer = BufWriter::new(file);
            let mut tar = TarWriter::new(writer);

            for (name, is_dir, data) in &entries {
                if *is_dir {
                    tar.add_directory(name)?;
                    if verbose {
                        println!("  Added: {}/", name);
                    }
                } else {
                    tar.add_file(name, data)?;
                    if verbose {
                        println!("  Added: {} ({} bytes)", name, data.len());
                    }
                }
            }

            tar.finish()?;
        }
        OutputFormat::Lzh => {
            let file = File::create(output)?;
            let writer = BufWriter::new(file);
            let mut lzh = LzhWriter::new(writer);

            // LZH compression is stored mode only for now
            lzh.set_compression(LzhCompressionLevel::Store);

            for (name, is_dir, data) in &entries {
                if *is_dir {
                    lzh.add_directory(name)?;
                    if verbose {
                        println!("  Added: {}/", name);
                    }
                } else {
                    lzh.add_file(name, data)?;
                    if verbose {
                        println!("  Added: {} ({} bytes)", name, data.len());
                    }
                }
            }

            lzh.finish()?;
        }
        OutputFormat::Gzip => {
            // GZIP can only compress a single file
            let non_dir_entries: Vec<_> = entries.iter().filter(|(_, is_dir, _)| !is_dir).collect();

            if non_dir_entries.len() != 1 {
                return Err(format!(
                    "GZIP can only compress a single file, but archive contains {} files",
                    non_dir_entries.len()
                )
                .into());
            }

            let (name, _, data) = &non_dir_entries[0];

            let level = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let compressed = oxiarc_archive::gzip::compress_with_filename(data, name, level)?;
            std::fs::write(output, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", name, data.len());
            }
        }
        OutputFormat::Xz => {
            // XZ can only compress a single file
            let non_dir_entries: Vec<_> = entries.iter().filter(|(_, is_dir, _)| !is_dir).collect();

            if non_dir_entries.len() != 1 {
                return Err(format!(
                    "XZ can only compress a single file, but archive contains {} files",
                    non_dir_entries.len()
                )
                .into());
            }

            let (name, _, data) = &non_dir_entries[0];

            let level = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let xz_writer = XzWriter::new(oxiarc_lzma::LzmaLevel::new(level));
            let compressed = xz_writer.compress(data)?;
            std::fs::write(output, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", name, data.len());
            }
        }
        OutputFormat::Lz4 => {
            // LZ4 can only compress a single file
            let non_dir_entries: Vec<_> = entries.iter().filter(|(_, is_dir, _)| !is_dir).collect();

            if non_dir_entries.len() != 1 {
                return Err(format!(
                    "LZ4 can only compress a single file, but archive contains {} files",
                    non_dir_entries.len()
                )
                .into());
            }

            let (name, _, data) = &non_dir_entries[0];

            let mut compressed = Vec::new();
            let mut lz4_writer = Lz4Writer::new(&mut compressed);
            lz4_writer.write_compressed(data)?;
            std::fs::write(output, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", name, data.len());
            }
        }
        OutputFormat::Bz2 => {
            // Bzip2 can only compress a single file
            let non_dir_entries: Vec<_> = entries.iter().filter(|(_, is_dir, _)| !is_dir).collect();

            if non_dir_entries.len() != 1 {
                return Err(format!(
                    "Bzip2 can only compress a single file, but archive contains {} files",
                    non_dir_entries.len()
                )
                .into());
            }

            let (name, _, data) = &non_dir_entries[0];

            let level = match compression {
                CompressionLevel::Store => 1,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let bzip2_writer = Bzip2Writer::with_level(level);
            let compressed = bzip2_writer.compress(data)?;
            std::fs::write(output, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", name, data.len());
            }
        }
        OutputFormat::Zst => {
            // Zstandard can only compress a single file
            let non_dir_entries: Vec<_> = entries.iter().filter(|(_, is_dir, _)| !is_dir).collect();

            if non_dir_entries.len() != 1 {
                return Err(format!(
                    "Zstandard can only compress a single file, but archive contains {} files",
                    non_dir_entries.len()
                )
                .into());
            }

            let (name, _, data) = &non_dir_entries[0];

            let zstd_writer = ZstdWriter::new();
            let compressed = zstd_writer.compress(data)?;
            std::fs::write(output, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", name, data.len());
            }
        }
    }

    println!("Conversion complete");
    Ok(())
}

/// Extract all entries from an archive into memory.
fn extract_all_entries<R: std::io::Read + std::io::Seek>(
    reader: &mut R,
    format: ArchiveFormat,
    input_path: &Path,
) -> Result<Vec<ExtractedEntry>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();

    match format {
        ArchiveFormat::Zip => {
            let mut zip = ZipReader::new(reader)?;
            for entry in zip.entries().to_vec() {
                let is_dir = entry.is_dir();
                let name = entry.name.clone();
                let data = if is_dir {
                    Vec::new()
                } else {
                    zip.extract(&entry)?
                };
                entries.push((name, is_dir, data));
            }
        }
        ArchiveFormat::Tar => {
            let mut tar = oxiarc_archive::TarReader::new(reader)?;
            for entry in tar.entries().to_vec() {
                let is_dir = entry.is_dir();
                let name = entry.name.clone();
                let data = if is_dir {
                    Vec::new()
                } else {
                    tar.extract_to_vec(&entry)?
                };
                entries.push((name, is_dir, data));
            }
        }
        ArchiveFormat::Lzh => {
            let mut lzh = oxiarc_archive::LzhReader::new(reader)?;
            for entry in lzh.entries().to_vec() {
                let is_dir = entry.is_dir();
                let name = entry.name.clone();
                let data = if is_dir {
                    Vec::new()
                } else {
                    lzh.extract_to_vec(&entry)?
                };
                entries.push((name, is_dir, data));
            }
        }
        ArchiveFormat::Gzip => {
            let mut gzip = oxiarc_archive::GzipReader::new(reader)?;
            let data = gzip.decompress()?;

            // Use original filename if available, otherwise use input filename without .gz
            let name = gzip.header().filename.clone().unwrap_or_else(|| {
                input_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });

            entries.push((name, false, data));
        }
        ArchiveFormat::Xz => {
            let data = oxiarc_archive::xz::decompress(reader)?;

            // Use input filename without .xz extension
            let name = input_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            entries.push((name, false, data));
        }
        ArchiveFormat::Lz4 => {
            let mut lz4 = Lz4Reader::new(reader)?;
            let data = lz4.decompress()?;

            // Use input filename without .lz4 extension
            let name = input_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            entries.push((name, false, data));
        }
        ArchiveFormat::Zstd => {
            let mut zstd = ZstdReader::new(reader)?;
            let data = zstd.decompress()?;

            // Use input filename without .zst extension
            let name = input_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            entries.push((name, false, data));
        }
        ArchiveFormat::Bzip2 => {
            let mut bzip2 = Bzip2Reader::new(reader)?;
            let data = bzip2.decompress()?;

            // Use input filename without .bz2 extension
            let name = input_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            entries.push((name, false, data));
        }
        ArchiveFormat::SevenZip => {
            let mut sevenz = SevenZReader::new(reader)?;
            let sevenz_entries: Vec<_> = sevenz.sevenz_entries().to_vec();

            for (i, entry) in sevenz_entries.iter().enumerate() {
                let is_dir = entry.is_dir;
                let name = entry.name.clone();
                let data = if is_dir {
                    Vec::new()
                } else {
                    sevenz.extract(i)?
                };
                entries.push((name, is_dir, data));
            }
        }
        ArchiveFormat::Cab => {
            let mut cab = CabReader::new(reader)?;
            let cab_entries: Vec<_> = cab.entries().to_vec();

            for entry in &cab_entries {
                let is_dir = entry.is_dir();
                let name = entry.name.clone();
                let data = if is_dir {
                    Vec::new()
                } else {
                    cab.extract(entry)?
                };
                entries.push((name, is_dir, data));
            }
        }
        _ => {
            return Err(format!("Cannot read entries from {} format", format).into());
        }
    }

    Ok(entries)
}

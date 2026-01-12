//! OxiArc CLI - The Oxidized Archiver
//!
//! A Pure Rust archive utility supporting ZIP, GZIP, TAR, LZH, XZ, 7z, CAB, LZ4, Zstd, and Bzip2 formats.

mod utils;

use clap::{Parser, Subcommand, ValueEnum};
use oxiarc_archive::{
    ArchiveFormat, Bzip2Reader, Bzip2Writer, CabReader, Lz4Reader, Lz4Writer, LzhCompressionLevel,
    LzhWriter, SevenZReader, TarWriter, XzWriter, ZipCompressionLevel, ZipReader, ZipWriter,
    ZstdReader, ZstdWriter,
};
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use utils::{ExtractedEntry, create_progress_bar, filter_entries, matches_filters, print_entries};

#[derive(Parser)]
#[command(name = "oxiarc")]
#[command(
    author,
    version,
    about = "The Oxidized Archiver - Pure Rust archive utility"
)]
#[command(long_about = "
OxiArc is a Pure Rust implementation of common archive formats.
Supported formats: ZIP, GZIP, TAR, LZH, XZ, 7z, LZ4, Zstd, Bzip2

Examples:
  oxiarc list archive.zip
  oxiarc list archive.7z
  oxiarc extract archive.zip
  oxiarc extract archive.7z
  oxiarc extract data.xz
  oxiarc extract data.lz4
  oxiarc extract data.zst
  oxiarc extract data.bz2
  oxiarc create archive.zip file1.txt file2.txt
  oxiarc create data.xz file.txt
  oxiarc create data.lz4 file.txt
  oxiarc create data.bz2 file.txt
  oxiarc convert archive.lzh output.zip
  oxiarc convert archive.7z output.zip
  oxiarc test archive.lzh
  oxiarc info archive.7z
")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List contents of an archive
    #[command(alias = "l")]
    List {
        /// Archive file to list
        archive: PathBuf,

        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Output as JSON (machine-readable)
        #[arg(short, long)]
        json: bool,

        /// Include only files matching pattern (glob syntax: *.txt, src/**/*)
        #[arg(short = 'I', long)]
        include: Vec<String>,

        /// Exclude files matching pattern (glob syntax)
        #[arg(short = 'X', long)]
        exclude: Vec<String>,
    },

    /// Extract files from an archive
    #[command(alias = "x")]
    Extract {
        /// Archive file to extract
        archive: PathBuf,

        /// Output directory
        #[arg(short, long, default_value = ".")]
        output: PathBuf,

        /// Files to extract (all if empty)
        files: Vec<String>,

        /// Include only files matching pattern (glob syntax: *.txt, src/**/*)
        #[arg(short = 'I', long)]
        include: Vec<String>,

        /// Exclude files matching pattern (glob syntax)
        #[arg(short = 'X', long)]
        exclude: Vec<String>,

        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Show progress bar
        #[arg(short = 'P', long, default_value = "true")]
        progress: bool,
    },

    /// Test archive integrity
    #[command(alias = "t")]
    Test {
        /// Archive file to test
        archive: PathBuf,

        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Create a new archive
    #[command(alias = "c")]
    Create {
        /// Output archive file
        archive: PathBuf,

        /// Files to add to the archive
        files: Vec<PathBuf>,

        /// Archive format (zip, tar, gzip, lzh, xz, lz4)
        #[arg(short, long, value_enum)]
        format: Option<OutputFormat>,

        /// Compression level
        #[arg(short = 'l', long, value_enum, default_value = "normal")]
        compression: CompressionLevel,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Show information about an archive
    #[command(alias = "i")]
    Info {
        /// Archive file to inspect
        archive: PathBuf,
    },

    /// Detect archive format
    Detect {
        /// File to detect
        file: PathBuf,
    },

    /// Convert archive to another format
    Convert {
        /// Input archive file
        input: PathBuf,

        /// Output archive file
        output: PathBuf,

        /// Output format (zip, tar, gzip, lzh, xz, lz4) - auto-detected from extension if not specified
        #[arg(short, long, value_enum)]
        format: Option<OutputFormat>,

        /// Compression level for output
        #[arg(short = 'l', long, value_enum, default_value = "normal")]
        compression: CompressionLevel,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },
}

/// Output archive format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
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

/// Compression level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum CompressionLevel {
    /// Store without compression
    Store,
    /// Fast compression
    Fast,
    /// Normal compression (default)
    #[default]
    Normal,
    /// Best compression
    Best,
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::List {
            archive,
            verbose,
            json,
            include,
            exclude,
        } => cmd_list(&archive, verbose, json, &include, &exclude),
        Commands::Extract {
            archive,
            output,
            files,
            include,
            exclude,
            verbose,
            progress,
        } => cmd_extract(
            &archive, &output, &files, &include, &exclude, verbose, progress,
        ),
        Commands::Test { archive, verbose } => cmd_test(&archive, verbose),
        Commands::Create {
            archive,
            files,
            format,
            compression,
            verbose,
        } => cmd_create(&archive, &files, format, compression, verbose),
        Commands::Info { archive } => cmd_info(&archive),
        Commands::Detect { file } => cmd_detect(&file),
        Commands::Convert {
            input,
            output,
            format,
            compression,
            verbose,
        } => cmd_convert(&input, &output, format, compression, verbose),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

fn cmd_list(
    archive: &PathBuf,
    verbose: bool,
    json: bool,
    include: &[String],
    exclude: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    // Detect format
    let (format, _magic) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    if json {
        // JSON output mode
        return cmd_list_json(archive, format, reader, include, exclude);
    }

    println!("Archive: {} ({})", archive.display(), format);
    println!();

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let filtered = filter_entries(zip.entries(), include, exclude);
            print_entries(&filtered, verbose);
        }
        ArchiveFormat::Tar => {
            let tar = oxiarc_archive::TarReader::new(reader)?;
            let filtered = filter_entries(tar.entries(), include, exclude);
            print_entries(&filtered, verbose);
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?;
            let filtered = filter_entries(&lzh.entries(), include, exclude);
            print_entries(&filtered, verbose);
        }
        ArchiveFormat::Gzip => {
            let gzip = oxiarc_archive::GzipReader::new(reader)?;
            println!("GZIP file");
            if let Some(name) = &gzip.header().filename {
                println!("  Original name: {}", name);
            }
        }
        ArchiveFormat::Xz => {
            println!("XZ file (LZMA2 compressed)");
            println!("  Single compressed stream - use 'extract' to decompress");
        }
        ArchiveFormat::Lz4 => {
            let lz4 = Lz4Reader::new(reader)?;
            println!("LZ4 file (fast compression)");
            if let Some(size) = lz4.original_size() {
                println!("  Original size: {} bytes", size);
            } else {
                println!("  Original size: unknown");
            }
            println!("  Use 'extract' to decompress");
        }
        ArchiveFormat::Zstd => {
            let zstd = ZstdReader::new(reader)?;
            println!("Zstandard file (modern fast compression)");
            if let Some(size) = zstd.content_size() {
                println!("  Original size: {} bytes", size);
            }
            println!("  Use 'extract' to decompress");
        }
        ArchiveFormat::Bzip2 => {
            let bzip2 = Bzip2Reader::new(reader)?;
            println!("Bzip2 file (block-sorting compression)");
            println!(
                "  Block size level: {} ({}KB blocks)",
                bzip2.block_size_level(),
                bzip2.block_size() / 1000
            );
            println!("  Use 'extract' to decompress");
        }
        ArchiveFormat::SevenZip => {
            let sevenz = SevenZReader::new(reader)?;
            let filtered = filter_entries(&sevenz.entries(), include, exclude);
            print_entries(&filtered, verbose);
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let filtered = filter_entries(cab.entries(), include, exclude);
            print_entries(&filtered, verbose);
        }
        _ => {
            println!("Unsupported format: {}", format);
        }
    }

    Ok(())
}

/// Output archive listing as JSON.
fn cmd_list_json<R: std::io::Read + std::io::Seek>(
    archive: &std::path::Path,
    format: ArchiveFormat,
    reader: R,
    include: &[String],
    exclude: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    use oxiarc_core::Entry;

    // Build JSON output
    let mut entries_json: Vec<String> = Vec::new();

    let format_entry = |e: &Entry| -> String {
        format!(
            r#"{{"name":"{}","size":{},"compressed_size":{},"is_dir":{},"method":"{:?}"}}"#,
            e.name.replace('\\', "\\\\").replace('"', "\\\""),
            e.size,
            e.compressed_size,
            e.is_dir(),
            e.method
        )
    };

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let filtered = filter_entries(zip.entries(), include, exclude);
            for entry in &filtered {
                entries_json.push(format_entry(entry));
            }
        }
        ArchiveFormat::Tar => {
            let tar = oxiarc_archive::TarReader::new(reader)?;
            let filtered = filter_entries(tar.entries(), include, exclude);
            for entry in &filtered {
                entries_json.push(format_entry(entry));
            }
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?;
            let filtered = filter_entries(&lzh.entries(), include, exclude);
            for entry in &filtered {
                entries_json.push(format_entry(entry));
            }
        }
        ArchiveFormat::Gzip => {
            let gzip = oxiarc_archive::GzipReader::new(reader)?;
            let name = gzip.header().filename.clone().unwrap_or_default();
            entries_json.push(format!(
                r#"{{"name":"{}","type":"compressed_file"}}"#,
                name.replace('\\', "\\\\").replace('"', "\\\"")
            ));
        }
        ArchiveFormat::Xz => {
            entries_json.push(r#"{"type":"compressed_stream","method":"LZMA2"}"#.to_string());
        }
        ArchiveFormat::Lz4 => {
            let lz4 = Lz4Reader::new(reader)?;
            let size_str = lz4
                .original_size()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "null".to_string());
            entries_json.push(format!(
                r#"{{"type":"compressed_file","method":"LZ4","original_size":{}}}"#,
                size_str
            ));
        }
        ArchiveFormat::Zstd => {
            let zstd = ZstdReader::new(reader)?;
            let size_str = zstd
                .content_size()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "null".to_string());
            entries_json.push(format!(
                r#"{{"type":"compressed_file","method":"Zstandard","original_size":{}}}"#,
                size_str
            ));
        }
        ArchiveFormat::Bzip2 => {
            let bzip2 = Bzip2Reader::new(reader)?;
            entries_json.push(format!(
                r#"{{"type":"compressed_file","method":"Bzip2","block_size":{}}}"#,
                bzip2.block_size()
            ));
        }
        ArchiveFormat::SevenZip => {
            let sevenz = SevenZReader::new(reader)?;
            let filtered = filter_entries(&sevenz.entries(), include, exclude);
            for entry in &filtered {
                entries_json.push(format_entry(entry));
            }
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let filtered = filter_entries(cab.entries(), include, exclude);
            for entry in &filtered {
                entries_json.push(format_entry(entry));
            }
        }
        _ => {}
    }

    // Output JSON
    println!(
        r#"{{"archive":"{}","format":"{}","entries":[{}]}}"#,
        archive
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\""),
        format,
        entries_json.join(",")
    );

    Ok(())
}

/// Filter entries by include/exclude patterns.
fn cmd_extract(
    archive: &Path,
    output: &Path,
    files: &[String],
    include: &[String],
    exclude: &[String],
    verbose: bool,
    progress: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    let (format, _) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    println!("Extracting {} to {}", archive.display(), output.display());

    // Helper to check if entry should be extracted
    let should_extract = |name: &str| -> bool {
        // If specific files are requested, check those first
        if !files.is_empty()
            && !files
                .iter()
                .any(|f| name == f || name.starts_with(&format!("{}/", f)))
        {
            return false;
        }
        // Apply include/exclude filters
        matches_filters(name, include, exclude)
    };

    match format {
        ArchiveFormat::Zip => {
            let mut zip = ZipReader::new(reader)?;
            let entries: Vec<_> = zip.entries().to_vec();

            // Filter entries
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path = output.join(entry.sanitized_name());
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = output.join(entry.sanitized_name());
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    let data = zip.extract(entry)?;
                    std::fs::write(&file_path, data)?;
                    if verbose {
                        pb.println(format!(
                            "  Extracted: {} ({} bytes)",
                            entry.name, entry.size
                        ));
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Gzip => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut gzip = oxiarc_archive::GzipReader::new(reader)?;
            let data = gzip.decompress()?;

            // Use original filename if available, otherwise strip .gz
            let out_name = gzip.header().filename.clone().unwrap_or_else(|| {
                archive
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });

            // For GZIP, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                std::fs::write(&out_path, &data)?;
                if verbose {
                    pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Tar => {
            let mut tar = oxiarc_archive::TarReader::new(reader)?;
            let entries: Vec<_> = tar.entries().to_vec();

            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path = output.join(entry.sanitized_name());
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = output.join(entry.sanitized_name());
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    let data = tar.extract_to_vec(entry)?;
                    std::fs::write(&file_path, data)?;
                    if verbose {
                        pb.println(format!(
                            "  Extracted: {} ({} bytes)",
                            entry.name, entry.size
                        ));
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Lzh => {
            let mut lzh = oxiarc_archive::LzhReader::new(reader)?;
            let entries: Vec<_> = lzh.entries().to_vec();

            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path = output.join(entry.sanitized_name());
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = output.join(entry.sanitized_name());
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    let data = lzh.extract_to_vec(entry)?;
                    std::fs::write(&file_path, data)?;
                    if verbose {
                        pb.println(format!(
                            "  Extracted: {} ({} bytes)",
                            entry.name, entry.size
                        ));
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Xz => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let data = oxiarc_archive::xz::decompress(&mut reader)?;

            // Use input filename without .xz extension
            let out_name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For XZ, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                std::fs::write(&out_path, &data)?;
                if verbose {
                    pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Lz4 => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut lz4 = Lz4Reader::new(reader)?;
            let data = lz4.decompress()?;

            // Use input filename without .lz4 extension
            let out_name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For LZ4, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                std::fs::write(&out_path, &data)?;
                if verbose {
                    pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Zstd => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut zstd = ZstdReader::new(reader)?;
            let data = zstd.decompress()?;

            // Use input filename without .zst extension
            let out_name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For Zstd, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                std::fs::write(&out_path, &data)?;
                if verbose {
                    pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Bzip2 => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut bzip2 = Bzip2Reader::new(reader)?;
            let data = bzip2.decompress()?;

            // Use input filename without .bz2 extension
            let out_name = archive
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For Bzip2, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                std::fs::write(&out_path, &data)?;
                if verbose {
                    pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::SevenZip => {
            let mut sevenz = SevenZReader::new(reader)?;
            let entries: Vec<_> = sevenz.sevenz_entries().to_vec();

            let to_extract: Vec<_> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| should_extract(&e.name))
                .collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for (i, entry) in to_extract {
                if entry.is_dir {
                    let dir_path = output.join(&entry.name);
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = output.join(&entry.name);
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    let data = sevenz.extract(i)?;
                    std::fs::write(&file_path, &data)?;
                    if verbose {
                        pb.println(format!(
                            "  Extracted: {} ({} bytes)",
                            entry.name,
                            data.len()
                        ));
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Cab => {
            let mut cab = CabReader::new(reader)?;
            let entries: Vec<_> = cab.entries().to_vec();

            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path = output.join(&entry.name);
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = output.join(&entry.name);
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    let data = cab.extract(entry)?;
                    std::fs::write(&file_path, &data)?;
                    if verbose {
                        pb.println(format!(
                            "  Extracted: {} ({} bytes)",
                            entry.name,
                            data.len()
                        ));
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
        }
        _ => {
            println!("Extraction not yet implemented for {}", format);
        }
    }

    Ok(())
}

fn cmd_test(archive: &PathBuf, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
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

fn cmd_info(archive: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

fn cmd_create(
    archive: &PathBuf,
    files: &[PathBuf],
    format: Option<OutputFormat>,
    compression: CompressionLevel,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if files.is_empty() {
        return Err("No files specified".into());
    }

    // Determine format from extension if not specified
    let format = format.unwrap_or_else(|| {
        let ext = archive
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
    });

    println!("Creating {:?} archive: {}", format, archive.display());

    match format {
        OutputFormat::Zip => {
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
            let file = File::create(archive)?;
            let writer = BufWriter::new(file);
            let mut tar = TarWriter::new(writer);

            for path in files {
                add_path_to_tar(&mut tar, path, path, verbose)?;
            }

            tar.finish()?;
        }
        OutputFormat::Gzip => {
            if files.len() != 1 {
                return Err("GZIP only supports single file compression".into());
            }

            let input_path = &files[0];
            if input_path.is_dir() {
                return Err("GZIP cannot compress directories directly. Use TAR first.".into());
            }

            let data = std::fs::read(input_path)?;
            let filename = input_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("data");

            let level = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let compressed = oxiarc_archive::gzip::compress_with_filename(&data, filename, level)?;
            std::fs::write(archive, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", filename, data.len());
            }
        }
        OutputFormat::Lzh => {
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
            if files.len() != 1 {
                return Err("XZ only supports single file compression".into());
            }

            let input_path = &files[0];
            if input_path.is_dir() {
                return Err("XZ cannot compress directories directly. Use TAR first.".into());
            }

            let data = std::fs::read(input_path)?;
            let filename = input_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("data");

            let level = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let xz_writer = XzWriter::new(oxiarc_lzma::LzmaLevel::new(level));
            let compressed = xz_writer.compress(&data)?;
            std::fs::write(archive, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", filename, data.len());
            }
        }
        OutputFormat::Lz4 => {
            if files.len() != 1 {
                return Err("LZ4 only supports single file compression".into());
            }

            let input_path = &files[0];
            if input_path.is_dir() {
                return Err("LZ4 cannot compress directories directly. Use TAR first.".into());
            }

            let data = std::fs::read(input_path)?;
            let filename = input_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("data");

            let mut output = Vec::new();
            let mut lz4_writer = Lz4Writer::new(&mut output);
            lz4_writer.write_compressed(&data)?;
            std::fs::write(archive, output)?;

            if verbose {
                println!("  Added: {} ({} bytes)", filename, data.len());
            }
        }
        OutputFormat::Bz2 => {
            if files.len() != 1 {
                return Err("Bzip2 only supports single file compression".into());
            }

            let input_path = &files[0];
            if input_path.is_dir() {
                return Err("Bzip2 cannot compress directories directly. Use TAR first.".into());
            }

            let data = std::fs::read(input_path)?;
            let filename = input_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("data");

            let level = match compression {
                CompressionLevel::Store => 1,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 9,
            };

            let bzip2_writer = Bzip2Writer::with_level(level);
            let compressed = bzip2_writer.compress(&data)?;
            std::fs::write(archive, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", filename, data.len());
            }
        }
        OutputFormat::Zst => {
            if files.len() != 1 {
                return Err("Zstandard only supports single file compression".into());
            }

            let input_path = &files[0];
            if input_path.is_dir() {
                return Err(
                    "Zstandard cannot compress directories directly. Use TAR first.".into(),
                );
            }

            let data = std::fs::read(input_path)?;
            let filename = input_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("data");

            let zstd_writer = ZstdWriter::new();
            let compressed = zstd_writer.compress(&data)?;
            std::fs::write(archive, compressed)?;

            if verbose {
                println!("  Added: {} ({} bytes)", filename, data.len());
            }
        }
    }

    println!("Archive created successfully");
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

fn cmd_detect(file: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
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

fn cmd_convert(
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

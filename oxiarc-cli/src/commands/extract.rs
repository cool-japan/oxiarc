//! Extract command implementation.

use crate::commands::OutputFormat;
use crate::style::Styler;
use crate::utils::{create_progress_bar, matches_filters};
use crate::windows::{long_path_prefix, sanitize_relative_path};
use dialoguer::Confirm;
use filetime::{FileTime, set_file_mtime};
use oxiarc_archive::{
    ArchiveFormat, BrotliReader, Bzip2Reader, CabReader, IsoReader, LenientWarning, Lz4Reader,
    SevenZReader, SnappyReader, ZipReader, ZstdReader,
};
use oxiarc_core::Entry;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Argument bundle for `cmd_extract`.
///
/// Extract grew enough CLI flags that inlining them all in the dispatcher
/// was triggering clippy's `too_many_arguments`. Packing the values here
/// keeps the wire-up explicit while flattening the call site in `main.rs`.
pub struct ExtractArgs<'a> {
    /// Archive file to extract (use `"-"` for stdin).
    pub archive: &'a str,
    /// Output directory (use `"-"` for stdout for single-file formats).
    pub output: &'a str,
    /// Specific entry names to extract; empty means all.
    pub files: &'a [String],
    /// Glob include patterns.
    pub include: &'a [String],
    /// Glob exclude patterns.
    pub exclude: &'a [String],
    /// Verbose logging.
    pub verbose: bool,
    /// Enable progress bar.
    pub progress: bool,
    /// Explicit format hint (required for stdin).
    pub format_hint: Option<OutputFormat>,
    /// Always overwrite (kept for CLI backward-compat; currently a no-op).
    pub overwrite: bool,
    /// Skip existing output files.
    pub skip_existing: bool,
    /// Interactively prompt before overwriting.
    pub prompt: bool,
    /// Preserve mtime.
    pub preserve_timestamps: bool,
    /// Preserve Unix mode bits.
    pub preserve_permissions: bool,
    /// Preserve all metadata (timestamps + permissions).
    pub preserve: bool,
    /// Dry run: report what would happen, write nothing.
    pub dry_run: bool,
    /// Optional password for encrypted entries; prompts interactively if
    /// `None` and an encrypted entry is encountered.
    pub password: Option<String>,
    /// Refuse to sanitize Windows-reserved basenames (error instead).
    pub strict_names: bool,
    /// Continue on corruption (CRC mismatch, bad TAR checksum, etc.)
    /// with warnings instead of errors. Warnings are emitted to stderr
    /// in yellow after extraction completes.
    pub lenient: bool,
    /// Optional per-entry memory cap in bytes. Entries whose uncompressed
    /// size exceeds this limit cause an immediate error rather than
    /// an out-of-memory allocation.
    pub memory_limit: Option<u64>,
}

/// Print accumulated lenient-mode warnings to stderr. No-op for empty
/// slices (common case — lenient is a silent no-op on clean archives).
fn print_warnings(warnings: &[LenientWarning], styler: &Styler) {
    for w in warnings {
        let msg = format!("warning: {} [{}]", w.message, w.format);
        eprintln!("{}", styler.warning(&msg));
    }
}

/// Overwrite mode for file extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteMode {
    /// Always overwrite existing files
    Always,
    /// Never overwrite existing files (skip them)
    Never,
    /// Prompt user for each file
    Prompt,
}

/// Check if we should write a file based on overwrite mode.
/// Returns Ok(true) if we should write, Ok(false) if we should skip.
fn should_write_file(
    path: &Path,
    mode: OverwriteMode,
    verbose: bool,
) -> Result<bool, Box<dyn std::error::Error>> {
    // If file doesn't exist, always write
    if !path.exists() {
        return Ok(true);
    }

    // Check if it's a directory
    if path.is_dir() {
        return Err(format!("Target path exists and is a directory: {}", path.display()).into());
    }

    match mode {
        OverwriteMode::Always => Ok(true),
        OverwriteMode::Never => {
            if verbose {
                eprintln!("  Skipped: {} (already exists)", path.display());
            }
            Ok(false)
        }
        OverwriteMode::Prompt => {
            let prompt = format!("Overwrite {}?", path.display());
            let result = Confirm::new()
                .with_prompt(&prompt)
                .default(false)
                .interact()?;
            Ok(result)
        }
    }
}

/// Apply metadata (timestamps and permissions) to an extracted file.
///
/// # Arguments
/// * `path` - Path to the extracted file
/// * `entry` - Archive entry with metadata
/// * `preserve_timestamps` - Whether to preserve modification time
/// * `preserve_permissions` - Whether to preserve Unix permissions
#[allow(unused_variables)]
fn apply_metadata(
    path: &Path,
    entry: &Entry,
    preserve_timestamps: bool,
    preserve_permissions: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Preserve timestamps
    if preserve_timestamps {
        if let Some(mtime) = entry.modified {
            let filetime = FileTime::from_system_time(mtime);
            set_file_mtime(path, filetime)?;
        }
    }

    // Preserve permissions (Unix only)
    if preserve_permissions {
        if let Some(mode) = entry.attributes.unix_mode {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let permissions = fs::Permissions::from_mode(mode);
                fs::set_permissions(path, permissions)?;
            }
            #[cfg(not(unix))]
            {
                // On non-Unix systems, just ignore permission preservation
            }
        }
    }

    Ok(())
}

/// Check that `entry_size` does not exceed `memory_limit` (if set).
///
/// Returns `Err` with a descriptive message when the limit is exceeded.
fn check_memory_limit(
    entry_name: &str,
    entry_size: u64,
    memory_limit: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(limit) = memory_limit {
        if entry_size > limit {
            return Err(format!(
                "entry '{}' requires {} bytes, exceeds --memory-limit {} bytes",
                entry_name, entry_size, limit
            )
            .into());
        }
    }
    Ok(())
}

/// Filter entries by include/exclude patterns.
pub fn cmd_extract(
    args: ExtractArgs<'_>,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let ExtractArgs {
        archive,
        output,
        files,
        include,
        exclude,
        verbose,
        progress,
        format_hint,
        overwrite: _overwrite,
        skip_existing,
        prompt,
        preserve_timestamps,
        preserve_permissions,
        preserve,
        dry_run,
        password,
        strict_names,
        lenient,
        memory_limit,
    } = args;

    // Determine overwrite mode from flags
    let overwrite_mode = if prompt {
        OverwriteMode::Prompt
    } else if skip_existing {
        OverwriteMode::Never
    } else {
        // Default is Always (for backwards compatibility)
        OverwriteMode::Always
    };

    // Determine what metadata to preserve
    let preserve_timestamps = preserve_timestamps || preserve;
    let preserve_permissions = preserve_permissions || preserve;

    // Check if we're reading from stdin
    let from_stdin = archive == "-";
    let to_stdout = output == "-";

    // Disable progress bar for stdin/stdout
    let progress = progress && !from_stdin && !to_stdout;

    if from_stdin && format_hint.is_none() {
        return Err("--format is required when reading from stdin".into());
    }

    // Dry run for stdin single-file formats: just detect and report
    if dry_run && from_stdin {
        let fmt = format_hint.ok_or("--format is required when reading from stdin")?;
        println!(
            "[DRY RUN] Would extract from stdin (format: {:?}) to {}",
            fmt, output
        );
        println!("[DRY RUN] No files were extracted.");
        return Ok(());
    }

    // Dry run for file-based archives: detect format, list entries, but skip writes
    if dry_run && !from_stdin {
        let archive_path = Path::new(archive);
        let file = File::open(archive_path)?;
        let mut reader = BufReader::new(file);
        let (format, _) = ArchiveFormat::detect(&mut reader)?;
        reader.seek(SeekFrom::Start(0))?;

        return extract_dry_run(
            reader,
            format,
            archive_path,
            output,
            files,
            include,
            exclude,
        );
    }

    // For stdin, we need to read all data into memory first
    let (format, data): (ArchiveFormat, Vec<u8>) = if from_stdin {
        let format =
            match format_hint.ok_or("Format required for stdin")? {
                OutputFormat::Gzip => ArchiveFormat::Gzip,
                OutputFormat::Xz => ArchiveFormat::Xz,
                OutputFormat::Bz2 => ArchiveFormat::Bzip2,
                OutputFormat::Lz4 => ArchiveFormat::Lz4,
                OutputFormat::Zst => ArchiveFormat::Zstd,
                OutputFormat::Br => ArchiveFormat::Brotli,
                OutputFormat::Snappy => ArchiveFormat::Snappy,
                _ => return Err(
                    "Only single-file formats (gzip, xz, bz2, lz4, zst, br, snappy) are supported for stdin"
                        .into(),
                ),
            };

        let mut stdin = io::stdin();
        let mut data = Vec::new();
        stdin.read_to_end(&mut data)?;
        (format, data)
    } else {
        let archive_path = Path::new(archive);
        let file = File::open(archive_path)?;
        let mut reader = BufReader::new(file);
        let (format, _) = ArchiveFormat::detect(&mut reader)?;
        reader.seek(SeekFrom::Start(0))?;

        // Read entire file for single-file formats when outputting to stdout
        if to_stdout {
            let mut data = Vec::new();
            reader.read_to_end(&mut data)?;
            (format, data)
        } else {
            // For archive formats, we'll process below
            drop(reader);
            let file = File::open(archive_path)?;
            let mut reader = BufReader::new(file);
            reader.seek(SeekFrom::Start(0))?;
            return extract_archive_format(ExtractArchiveArgs {
                reader,
                format,
                output: Path::new(output),
                files,
                include,
                exclude,
                verbose,
                progress,
                archive_path,
                overwrite_mode,
                preserve_timestamps,
                preserve_permissions,
                password,
                strict_names,
                lenient,
                memory_limit,
                styler,
            });
        }
    };

    // Only print message if not using stdout (to avoid mixing with output data)
    if !to_stdout && verbose {
        eprintln!("Extracting {} to {}", archive, output);
    }

    // Handle single-file format extraction to stdout or file
    if to_stdout {
        let stdout = io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        extract_single_file_to_writer(&data, format, &mut writer, verbose)?;
        return Ok(());
    }

    // For stdin to file, handle single-file formats
    if from_stdin {
        let output_path = Path::new(output);
        std::fs::create_dir_all(output_path)?;
        let out_name = "output"; // Default name for stdin
        let out_path = output_path.join(out_name);

        let decompressed = decompress_single_file(&data, format)?;

        if should_write_file(&out_path, overwrite_mode, verbose)? {
            std::fs::write(&out_path, &decompressed)?;
            if verbose {
                eprintln!("Extracted: {} ({} bytes)", out_name, decompressed.len());
            }
        }
        return Ok(());
    }

    unreachable!("Should have been handled above");
}

/// Helper that resolves the output filesystem path for an archive entry,
/// applying Windows reserved-name sanitization and long-path prefixing.
fn resolve_output_path(
    output_root: &Path,
    entry_name: &str,
    strict_names: bool,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let sanitized = sanitize_relative_path(entry_name, strict_names)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let joined = output_root.join(&sanitized);
    Ok(long_path_prefix(&joined))
}

/// Resolve a password either from the CLI flag or interactive prompt.
///
/// Returns the password bytes. If the CLI flag is `None`, prompts on the
/// controlling terminal via `rpassword`. Exits with status `2` if the prompt
/// fails (e.g. no TTY available).
fn resolve_password(cli_password: Option<String>) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    if let Some(pw) = cli_password {
        return Ok(pw.into_bytes());
    }
    match rpassword::prompt_password("Password: ") {
        Ok(pw) => Ok(pw.into_bytes()),
        Err(e) => {
            eprintln!(
                "error: could not read password from terminal: {} (use --password=... for non-interactive use)",
                e
            );
            std::process::exit(2);
        }
    }
}

/// Argument bundle passed to `extract_archive_format`. Prevents a tangle of
/// positional arguments.
struct ExtractArchiveArgs<'a, R: Read + Seek> {
    reader: R,
    format: ArchiveFormat,
    output: &'a Path,
    files: &'a [String],
    include: &'a [String],
    exclude: &'a [String],
    verbose: bool,
    progress: bool,
    archive_path: &'a Path,
    overwrite_mode: OverwriteMode,
    preserve_timestamps: bool,
    preserve_permissions: bool,
    password: Option<String>,
    strict_names: bool,
    /// Whether to continue past per-entry corruption, recording
    /// warnings on the reader instead of aborting.
    lenient: bool,
    /// Optional per-entry memory cap in bytes.
    memory_limit: Option<u64>,
    /// Styler used to colorize any warnings emitted after extraction.
    styler: &'a Styler,
}

/// Decompress a single-file format from a byte slice.
fn decompress_single_file(
    data: &[u8],
    format: ArchiveFormat,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut cursor = io::Cursor::new(data);
    let mut reader = BufReader::new(&mut cursor);

    match format {
        ArchiveFormat::Gzip => {
            let mut gzip = oxiarc_archive::GzipReader::new(reader)?;
            Ok(gzip.decompress()?)
        }
        ArchiveFormat::Xz => Ok(oxiarc_archive::xz::decompress(&mut reader)?),
        ArchiveFormat::Lz4 => {
            let mut lz4 = Lz4Reader::new(reader)?;
            Ok(lz4.decompress()?)
        }
        ArchiveFormat::Zstd => {
            let mut zstd = ZstdReader::new(reader)?;
            Ok(zstd.decompress()?)
        }
        ArchiveFormat::Bzip2 => {
            let mut bzip2 = Bzip2Reader::new(reader)?;
            Ok(bzip2.decompress()?)
        }
        ArchiveFormat::Brotli => {
            let mut brotli = BrotliReader::new(reader)?;
            Ok(brotli.decompress()?)
        }
        ArchiveFormat::Snappy => {
            let mut snappy = SnappyReader::new(reader)?;
            Ok(snappy.decompress()?)
        }
        _ => Err("Unsupported format for stdin/stdout".into()),
    }
}

/// Extract a single-file format to a writer.
fn extract_single_file_to_writer<W: Write>(
    data: &[u8],
    format: ArchiveFormat,
    writer: &mut W,
    _verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let decompressed = decompress_single_file(data, format)?;
    writer.write_all(&decompressed)?;
    writer.flush()?;
    Ok(())
}

/// Extract archive formats (non-streaming).
fn extract_archive_format<R: Read + Seek>(
    args: ExtractArchiveArgs<'_, R>,
) -> Result<(), Box<dyn std::error::Error>> {
    let ExtractArchiveArgs {
        mut reader,
        format,
        output,
        files,
        include,
        exclude,
        verbose,
        progress,
        archive_path,
        overwrite_mode,
        preserve_timestamps,
        preserve_permissions,
        password,
        strict_names,
        lenient,
        memory_limit,
        styler,
    } = args;
    println!(
        "Extracting {} to {}",
        archive_path.display(),
        output.display()
    );

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
            let mut zip = ZipReader::new(reader)?.lenient(lenient);
            let entries: Vec<_> = zip.entries().to_vec();

            // Filter entries
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            // Resolve password if any encrypted entries are in the selection.
            let needs_password = to_extract
                .iter()
                .any(|e| ZipReader::<std::io::Cursor<&[u8]>>::is_encrypted(e));
            let password_bytes: Option<Vec<u8>> = if needs_password {
                Some(resolve_password(password)?)
            } else {
                None
            };

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    if should_write_file(&file_path, overwrite_mode, verbose)? {
                        check_memory_limit(&entry.name, entry.size, memory_limit)?;
                        let data = if ZipReader::<std::io::Cursor<&[u8]>>::is_encrypted(entry) {
                            let pw = password_bytes
                                .as_deref()
                                .ok_or("encrypted entry but no password provided")?;
                            match zip.extract_encrypted(entry, pw) {
                                Ok(bytes) => bytes,
                                Err(e) => {
                                    eprintln!(
                                        "error: failed to decrypt {}: {} (likely wrong password)",
                                        entry.name, e
                                    );
                                    std::process::exit(2);
                                }
                            }
                        } else {
                            zip.extract(entry)?
                        };
                        std::fs::write(&file_path, data)?;
                        apply_metadata(
                            &file_path,
                            entry,
                            preserve_timestamps,
                            preserve_permissions,
                        )?;
                        if verbose {
                            pb.println(format!(
                                "  Extracted: {} ({} bytes)",
                                entry.name, entry.size
                            ));
                        }
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
            print_warnings(zip.warnings(), styler);
        }
        ArchiveFormat::Gzip => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut gzip = oxiarc_archive::GzipReader::new(reader)?;
            let data = gzip.decompress()?;

            // Use original filename if available, otherwise strip .gz
            let out_name = gzip.header().filename.clone().unwrap_or_else(|| {
                archive_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });

            // For GZIP, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Tar => {
            // TarReader scans eagerly in `new`, so lenient scanning
            // requires the dedicated `new_lenient` constructor.
            let mut tar = if lenient {
                oxiarc_archive::TarReader::new_lenient(reader)?
            } else {
                oxiarc_archive::TarReader::new(reader)?
            };
            let entries: Vec<_> = tar.entries().to_vec();

            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    if should_write_file(&file_path, overwrite_mode, verbose)? {
                        check_memory_limit(&entry.name, entry.size, memory_limit)?;
                        let data = tar.extract_to_vec(entry)?;
                        std::fs::write(&file_path, data)?;
                        apply_metadata(
                            &file_path,
                            entry,
                            preserve_timestamps,
                            preserve_permissions,
                        )?;
                        if verbose {
                            pb.println(format!(
                                "  Extracted: {} ({} bytes)",
                                entry.name, entry.size
                            ));
                        }
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
            print_warnings(tar.warnings(), styler);
        }
        ArchiveFormat::Lzh => {
            let mut lzh = oxiarc_archive::LzhReader::new(reader)?.lenient(lenient);
            let entries: Vec<_> = lzh.entries().to_vec();

            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                if entry.is_dir() {
                    let dir_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    if should_write_file(&file_path, overwrite_mode, verbose)? {
                        check_memory_limit(&entry.name, entry.size, memory_limit)?;
                        let data = lzh.extract_to_vec(entry)?;
                        std::fs::write(&file_path, data)?;
                        apply_metadata(
                            &file_path,
                            entry,
                            preserve_timestamps,
                            preserve_permissions,
                        )?;
                        if verbose {
                            pb.println(format!(
                                "  Extracted: {} ({} bytes)",
                                entry.name, entry.size
                            ));
                        }
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
            print_warnings(lzh.warnings(), styler);
        }
        ArchiveFormat::Xz => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let data = oxiarc_archive::xz::decompress(&mut reader)?;

            // Use input filename without .xz extension
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For XZ, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
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
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For LZ4, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
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
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For Zstd, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
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
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            // For Bzip2, apply filter to output name
            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Brotli => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut brotli = BrotliReader::new(reader)?;
            let data = brotli.decompress()?;

            // Use input filename without .br extension
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
                }
            } else if verbose {
                pb.println(format!("  Skipped: {} (filtered)", out_name));
            }
            pb.inc(1);
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Snappy => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut snappy = SnappyReader::new(reader)?;
            let data = snappy.decompress()?;

            // Use input filename without .sz extension
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();

            if should_extract(&out_name) {
                let out_path = output.join(&out_name);
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &data)?;
                    if verbose {
                        pb.println(format!("  Extracted: {} ({} bytes)", out_name, data.len()));
                    }
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
                    let dir_path = resolve_output_path(output, &entry.name, strict_names)?;
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = resolve_output_path(output, &entry.name, strict_names)?;
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if should_write_file(&file_path, overwrite_mode, verbose)? {
                        check_memory_limit(&entry.name, entry.size, memory_limit)?;
                        let data = sevenz.extract(i)?;
                        std::fs::write(&file_path, &data)?;
                        let core_entry = entry.to_entry();
                        apply_metadata(
                            &file_path,
                            &core_entry,
                            preserve_timestamps,
                            preserve_permissions,
                        )?;
                        if verbose {
                            pb.println(format!(
                                "  Extracted: {} ({} bytes)",
                                entry.name,
                                data.len()
                            ));
                        }
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
                    let dir_path = resolve_output_path(output, &entry.name, strict_names)?;
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path = resolve_output_path(output, &entry.name, strict_names)?;
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }

                    if should_write_file(&file_path, overwrite_mode, verbose)? {
                        check_memory_limit(&entry.name, entry.size, memory_limit)?;
                        let data = cab.extract(entry)?;
                        std::fs::write(&file_path, &data)?;
                        apply_metadata(
                            &file_path,
                            entry,
                            preserve_timestamps,
                            preserve_permissions,
                        )?;
                        if verbose {
                            pb.println(format!(
                                "  Extracted: {} ({} bytes)",
                                entry.name,
                                data.len()
                            ));
                        }
                    }
                }
                pb.inc(1);
            }
            pb.finish_with_message("Done");
        }
        ArchiveFormat::Iso9660 => {
            let mut iso = IsoReader::new(reader)?;
            let entries: Vec<_> = iso.entries().to_vec();

            let to_extract: Vec<_> = entries
                .iter()
                .filter(|e| !e.is_dir && should_extract(&e.name))
                .collect();
            let total = to_extract.len() as u64;

            let pb = create_progress_bar(total, progress);
            pb.set_message("files");

            for entry in to_extract {
                let file_path = resolve_output_path(output, &entry.name, strict_names)?;
                if let Some(parent) = file_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if should_write_file(&file_path, overwrite_mode, verbose)? {
                    check_memory_limit(&entry.name, entry.size, memory_limit)?;
                    let mut data = Vec::new();
                    iso.extract(entry, &mut data)?;
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
            return Err(format!(
                "Unsupported archive format: {}; supported formats: \
                 zip, gzip, tar, lzh, xz, lz4, zstd, bzip2, brotli, snappy, 7z, cab, iso9660",
                format
            )
            .into());
        }
    }

    Ok(())
}

/// Dry run mode for extract: show what would be extracted without writing files.
#[allow(clippy::too_many_arguments)]
fn extract_dry_run<R: Read + Seek>(
    reader: R,
    format: ArchiveFormat,
    archive_path: &Path,
    output: &str,
    files: &[String],
    include: &[String],
    exclude: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "[DRY RUN] Would extract {} to {}",
        archive_path.display(),
        output
    );

    let should_extract = |name: &str| -> bool {
        if !files.is_empty()
            && !files
                .iter()
                .any(|f| name == f || name.starts_with(&format!("{}/", f)))
        {
            return false;
        }
        matches_filters(name, include, exclude)
    };

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let entries: Vec<_> = zip.entries().to_vec();
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            println!("[DRY RUN] {} entries would be extracted:", to_extract.len());
            let mut total_size = 0u64;
            for entry in &to_extract {
                let kind = if entry.is_dir() { "dir " } else { "file" };
                println!("[DRY RUN]   {} {} ({} bytes)", kind, entry.name, entry.size);
                total_size += entry.size;
            }
            println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
        }
        ArchiveFormat::Tar => {
            let tar = oxiarc_archive::TarReader::new(reader)?;
            let entries: Vec<_> = tar.entries().to_vec();
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            println!("[DRY RUN] {} entries would be extracted:", to_extract.len());
            let mut total_size = 0u64;
            for entry in &to_extract {
                let kind = if entry.is_dir() { "dir " } else { "file" };
                println!("[DRY RUN]   {} {} ({} bytes)", kind, entry.name, entry.size);
                total_size += entry.size;
            }
            println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?;
            let entries: Vec<_> = lzh.entries().to_vec();
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            println!("[DRY RUN] {} entries would be extracted:", to_extract.len());
            let mut total_size = 0u64;
            for entry in &to_extract {
                let kind = if entry.is_dir() { "dir " } else { "file" };
                println!("[DRY RUN]   {} {} ({} bytes)", kind, entry.name, entry.size);
                total_size += entry.size;
            }
            println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
        }
        ArchiveFormat::SevenZip => {
            let sevenz = SevenZReader::new(reader)?;
            let entries: Vec<_> = sevenz.sevenz_entries().to_vec();
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            println!("[DRY RUN] {} entries would be extracted:", to_extract.len());
            let mut total_size = 0u64;
            for entry in &to_extract {
                let kind = if entry.is_dir { "dir " } else { "file" };
                println!("[DRY RUN]   {} {} ({} bytes)", kind, entry.name, entry.size);
                total_size += entry.size;
            }
            println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let entries: Vec<_> = cab.entries().to_vec();
            let to_extract: Vec<_> = entries.iter().filter(|e| should_extract(&e.name)).collect();
            println!("[DRY RUN] {} entries would be extracted:", to_extract.len());
            let mut total_size = 0u64;
            for entry in &to_extract {
                let kind = if entry.is_dir() { "dir " } else { "file" };
                println!("[DRY RUN]   {} {} ({} bytes)", kind, entry.name, entry.size);
                total_size += entry.size;
            }
            println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
        }
        ArchiveFormat::Gzip => {
            let gzip = oxiarc_archive::GzipReader::new(reader)?;
            let out_name = gzip.header().filename.clone().unwrap_or_else(|| {
                archive_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Xz => {
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Lz4 => {
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Zstd => {
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Bzip2 => {
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Brotli => {
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Snappy => {
            let out_name = archive_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            println!("[DRY RUN] Would decompress to: {}", out_name);
        }
        ArchiveFormat::Iso9660 => {
            let iso = IsoReader::new(reader)?;
            let entries: Vec<_> = iso.entries().to_vec();
            let to_extract: Vec<_> = entries
                .iter()
                .filter(|e| !e.is_dir && should_extract(&e.name))
                .collect();
            println!("[DRY RUN] {} entries would be extracted:", to_extract.len());
            let mut total_size = 0u64;
            for entry in &to_extract {
                println!("[DRY RUN]   file {} ({} bytes)", entry.name, entry.size);
                total_size += entry.size;
            }
            println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
        }
        _ => {
            println!("[DRY RUN] Format detection: {}", format);
        }
    }

    println!("[DRY RUN] No files were extracted.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::ColorChoice;
    use std::io::Cursor;

    /// `ArchiveFormat::Unknown` is the only variant that reaches the `_ =>` arm
    /// in `extract_archive_format`. All thirteen named variants (Zip, Gzip, Tar,
    /// Lzh, SevenZip, Xz, Bzip2, Zstd, Lz4, Cab, Brotli, Snappy, Iso9660) are
    /// handled by explicit arms; `Unknown` is the only reachable catch-all
    /// through the CLI.
    ///
    /// This test constructs `ExtractArchiveArgs` directly (bypassing detection)
    /// to verify that the `_ =>` arm returns a clear unsupported-format error.
    #[test]
    fn test_extract_dispatch_unknown_format_errors_clearly() {
        let tmp = std::env::temp_dir().join(format!(
            "oxiarc_extract_test_unknown_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&tmp);

        let data: &[u8] = b"\x00\x01\x02\x03"; // matches no magic
        let cursor = Cursor::new(data);
        let styler = Styler::new(ColorChoice::Never);
        let archive_path = tmp.join("fake.bin");

        let result = extract_archive_format(ExtractArchiveArgs {
            reader: cursor,
            format: ArchiveFormat::Unknown,
            output: &tmp,
            files: &[],
            include: &[],
            exclude: &[],
            verbose: false,
            progress: false,
            archive_path: &archive_path,
            overwrite_mode: OverwriteMode::Always,
            preserve_timestamps: false,
            preserve_permissions: false,
            password: None,
            strict_names: false,
            lenient: false,
            memory_limit: None,
            styler: &styler,
        });

        let _ = std::fs::remove_dir_all(&tmp);

        assert!(result.is_err(), "expected Err for Unknown format");
        let msg = result
            .expect_err("expected error for unknown format")
            .to_string();
        assert!(
            msg.contains("Unsupported archive format"),
            "expected 'Unsupported archive format' in error message, got: {msg}"
        );
        assert!(
            msg.contains("zip"),
            "error message should list supported formats, got: {msg}"
        );
    }
}

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
use oxiarc_core::{Entry, EntryType};
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
    /// Suppress the progress bar and per-file chatter (errors still print).
    pub quiet: bool,
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
        quiet,
    } = args;

    // Quiet mode forces off both the progress bar and per-file verbose chatter.
    let verbose = verbose && !quiet;

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
    let progress = progress && !from_stdin && !to_stdout && !quiet;

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
        // `detect_with_path` falls back to the filename extension for the
        // magic-less formats (raw Brotli `.br`, raw Snappy `.sz`), which plain
        // `detect` can only ever report as Unknown.
        let (format, _) = ArchiveFormat::detect_with_path(&mut reader, archive_path)?;
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
        let (format, _) = ArchiveFormat::detect_with_path(&mut reader, archive_path)?;
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
        extract_single_file_to_writer(&data, format, &mut writer, verbose, memory_limit)?;
        return Ok(());
    }

    // For stdin to file, handle single-file formats
    if from_stdin {
        let output_path = Path::new(output);
        std::fs::create_dir_all(output_path)?;
        let out_name = "output"; // Default name for stdin
        let out_path = output_path.join(out_name);

        let decompressed = decompress_single_file(&data, format, memory_limit)?;

        if should_write_file(&out_path, overwrite_mode, verbose)? {
            std::fs::write(&out_path, &decompressed)?;
            if verbose {
                eprintln!("Extracted: {} ({} bytes)", out_name, decompressed.len());
            }
        }
        return Ok(());
    }

    // The only paths that fall through the `(format, data)` block above without
    // returning are `to_stdout` (handled above) and `from_stdin` (handled just
    // above). Any other case returned early inside that block, so this point is
    // logically unreachable — but we return a defensive error instead of
    // panicking to satisfy the no-panic policy.
    Err("internal error: extract reached an unhandled code path".into())
}

/// Resolve `output_root` to an absolute, symlink-free form when it exists,
/// falling back to a purely lexical absolute path when it does not yet exist
/// on disk (the common case for the first entry of an archive).
fn canonicalize_or_absolute(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Helper that resolves the output filesystem path for an archive entry,
/// applying Windows reserved-name sanitization and long-path prefixing.
///
/// Security: `sanitize_relative_path` already strips `..`/root/drive
/// components (first line of defense against Zip-Slip). As defense in depth,
/// the joined path is verified to remain within the (absolute) output root;
/// any escape is rejected with an error rather than written outside the
/// target directory.
fn resolve_output_path(
    output_root: &Path,
    entry_name: &str,
    strict_names: bool,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let sanitized = sanitize_relative_path(entry_name, strict_names)
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    // Build the candidate path from the absolute root so the containment check
    // below compares like-for-like prefixes.
    let root_abs = canonicalize_or_absolute(output_root);
    let joined = root_abs.join(&sanitized);
    if !joined.starts_with(&root_abs) {
        return Err(format!(
            "refusing to extract '{}': resolved path '{}' escapes output directory '{}'",
            entry_name,
            joined.display(),
            output_root.display()
        )
        .into());
    }
    Ok(long_path_prefix(&joined))
}

/// Determine the symlink target for a core [`Entry`], if it represents a
/// symbolic link. Returns `None` for regular files and directories.
fn core_symlink_target(entry: &Entry) -> Option<PathBuf> {
    if entry.entry_type == EntryType::Symlink || entry.link_target.is_some() {
        entry.link_target.clone()
    } else {
        None
    }
}

/// Create a symbolic link at `link_path` pointing at `target`, honoring the
/// overwrite mode. Returns `Ok(Some(msg))` with a verbose success description
/// when the link was created, or `Ok(None)` when nothing quotable happened
/// (skipped, or a warning already printed to stderr).
///
/// The `link_path` has already passed the containment check in
/// [`resolve_output_path`], so the *location* of the link cannot escape the
/// output directory (the link target itself is written verbatim, matching the
/// behavior of standard extraction tools).
fn write_symlink_entry(
    link_path: &Path,
    target: &Path,
    overwrite_mode: OverwriteMode,
    verbose: bool,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if let Some(parent) = link_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // A symlink cannot be created over an existing path; consult the overwrite
    // policy, then remove the existing entry before recreating it.
    let exists = link_path.symlink_metadata().is_ok();
    if exists {
        match overwrite_mode {
            OverwriteMode::Never => {
                if verbose {
                    eprintln!("  Skipped: {} (already exists)", link_path.display());
                }
                return Ok(None);
            }
            OverwriteMode::Prompt => {
                let prompt = format!("Overwrite {}?", link_path.display());
                let ok = Confirm::new()
                    .with_prompt(&prompt)
                    .default(false)
                    .interact()?;
                if !ok {
                    return Ok(None);
                }
            }
            OverwriteMode::Always => {}
        }
        std::fs::remove_file(link_path)?;
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link_path)?;
        Ok(Some(format!(
            "  Symlink: {} -> {}",
            link_path.display(),
            target.display()
        )))
    }
    #[cfg(windows)]
    {
        // Prefer a directory symlink when the target resolves to an existing
        // directory relative to the link's parent; otherwise a file symlink.
        let resolved = link_path
            .parent()
            .map(|p| p.join(target))
            .unwrap_or_else(|| target.to_path_buf());
        let result = if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, link_path)
        } else {
            std::os::windows::fs::symlink_file(target, link_path)
        };
        match result {
            Ok(()) => Ok(Some(format!(
                "  Symlink: {} -> {}",
                link_path.display(),
                target.display()
            ))),
            Err(e) => {
                // ERROR_PRIVILEGE_NOT_HELD (1314): creating symlinks requires
                // either Developer Mode or the SeCreateSymbolicLink privilege.
                // Degrade gracefully with a warning rather than aborting.
                const ERROR_PRIVILEGE_NOT_HELD: i32 = 1314;
                if e.raw_os_error() == Some(ERROR_PRIVILEGE_NOT_HELD) {
                    eprintln!(
                        "  warning: skipped symlink {} (insufficient privilege; enable Developer Mode)",
                        link_path.display()
                    );
                    Ok(None)
                } else {
                    Err(e.into())
                }
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = target;
        eprintln!(
            "  warning: skipped symlink {} (symlinks unsupported on this platform)",
            link_path.display()
        );
        Ok(None)
    }
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

/// Decoded payload of a single-file compressed stream, plus any original
/// filename recorded inside the container (only gzip carries one).
struct SingleFileOutput {
    data: Vec<u8>,
    original_name: Option<String>,
}

/// Read an XZ multibyte integer (base-128, little-endian, continuation bit in
/// the MSB — xz spec §1.2). Returns `None` for a truncated, over-long, or
/// non-minimal encoding.
fn xz_read_varint(data: &[u8], offset: &mut usize) -> Option<u64> {
    let mut result: u64 = 0;
    for i in 0..9usize {
        let byte = *data.get(offset.checked_add(i)?)?;
        result |= u64::from(byte & 0x7F) << (i * 7);
        if byte & 0x80 == 0 {
            // The spec mandates the shortest encoding: a final byte of 0x00
            // after at least one continuation byte is non-minimal.
            if i > 0 && byte == 0 {
                return None;
            }
            *offset = offset.checked_add(i + 1)?;
            return Some(result);
        }
    }
    None
}

/// Total uncompressed size declared by a `.xz` file's stream index.
///
/// XZ has no single "original size" header field, but every stream ends with
/// an Index listing `(unpadded_size, uncompressed_size)` for each block — this
/// is the declared-size equivalent of gzip's ISIZE or zstd's frame content
/// size, and it is what `--memory-limit` is checked against for xz.
///
/// Returns `None` unless `data` is exactly *one* structurally intact XZ stream
/// (header + blocks + index + footer, sizes all agreeing). Truncated files,
/// malformed indexes, and concatenated multi-stream files therefore yield
/// `None` rather than an untrustworthy number — the caller must refuse to
/// decompress under a memory limit instead of guessing.
fn xz_declared_output_size(data: &[u8]) -> Option<u64> {
    const STREAM_HEADER_SIZE: u64 = 12;
    const STREAM_FOOTER_SIZE: usize = 12;

    let footer_start = data.len().checked_sub(STREAM_FOOTER_SIZE)?;
    let footer = data.get(footer_start..)?;
    if footer.get(10..12) != Some(b"YZ") {
        return None;
    }
    // Stream Footer: CRC32(4) | Backward Size(4) | Stream Flags(2) | "YZ"(2).
    // Real index size = (stored + 1) * 4.
    let backward_size = u64::from(u32::from_le_bytes([
        footer[4], footer[5], footer[6], footer[7],
    ]));
    let index_size_u64 = backward_size.checked_add(1)?.checked_mul(4)?;
    let index_size = usize::try_from(index_size_u64).ok()?;
    let index_start = footer_start.checked_sub(index_size)?;
    if (index_start as u64) < STREAM_HEADER_SIZE {
        return None;
    }
    let index = data.get(index_start..footer_start)?;

    // Index: Indicator(0x00) | Number of Records | Records | Padding | CRC32.
    if index.first() != Some(&0x00) {
        return None;
    }
    let mut offset = 1usize;
    let record_count = xz_read_varint(index, &mut offset)?;
    // Each record is at least two bytes; a count that cannot possibly fit in
    // the index is a malformed (or hostile) header.
    if record_count > (index.len() as u64) / 2 {
        return None;
    }

    let mut total_uncompressed = 0u64;
    let mut blocks_size = 0u64;
    for _ in 0..record_count {
        let unpadded = xz_read_varint(index, &mut offset)?;
        let uncompressed = xz_read_varint(index, &mut offset)?;
        total_uncompressed = total_uncompressed.checked_add(uncompressed)?;
        // Each block is padded out to a 4-byte boundary in the stream.
        let padded = unpadded.checked_add(3)? & !3u64;
        blocks_size = blocks_size.checked_add(padded)?;
    }

    // Single-stream check: the sizes the index declares must account for the
    // whole file. Anything left over means stream padding or a concatenated
    // second stream, in which case this index does not describe everything
    // that would be decoded.
    let expected_len = STREAM_HEADER_SIZE
        .checked_add(blocks_size)?
        .checked_add(index_size_u64)?
        .checked_add(STREAM_FOOTER_SIZE as u64)?;
    if expected_len != data.len() as u64 {
        return None;
    }

    Some(total_uncompressed)
}

/// Decompress a single-file format from a byte slice, enforcing `memory_limit`.
///
/// This is the **one** place single-file decompression happens; both the stdin
/// path and the `oxiarc extract file.gz -o dir` path route through it so the
/// bomb defense advertised by `--memory-limit` cannot drift between them.
///
/// Enforcement per format:
///
/// * **gzip** — the trailing ISIZE field declares the uncompressed size; it is
///   checked before any decompression happens.
/// * **lz4** / **zstd** — the frame header's content size (when present) is
///   checked before decompression.
/// * **xz** — the stream index declares the total uncompressed size (see
///   [`xz_declared_output_size`]); it is checked before decompression, and an
///   xz stream whose index cannot be trusted is *refused* under a memory limit
///   rather than silently decompressed.
/// * **bzip2** — bzip2 declares nothing, so the limit is enforced *during*
///   decoding by `oxiarc_archive::bzip2::decompress_with_limit`, which fails
///   before an over-budget block is ever appended.
/// * **brotli** — the stream declares no total size, but each meta-block
///   declares its exact length (MLEN), so
///   `oxiarc_archive::brotli::decompress_with_limit` checks
///   `produced + MLEN` against the budget *before* decoding that meta-block.
///   An over-budget bomb is rejected without its expansion being allocated.
/// * **snappy** — likewise: the frame declares no total size, but every chunk
///   declares its own (the block varint, or the chunk length), so
///   `oxiarc_archive::snappy::decompress_with_limit` rejects a chunk that
///   would push the total past the budget *before* decoding it.
///
/// Every format additionally gets a final post-decode check, so no path can
/// return more bytes than the caller allowed.
///
/// Residual caveat: the *compressed* input itself is read into memory in full
/// by every path here (it is a `&[u8]` slice by the time it reaches this
/// function), so `--memory-limit` bounds the decompressed payload, not the
/// archive bytes; and a corrupt brotli stream may append at most one
/// transformed dictionary word (< 64 bytes) past the budget before the
/// meta-block length check rejects it.
fn decompress_single_file_full(
    data: &[u8],
    format: ArchiveFormat,
    memory_limit: Option<u64>,
) -> Result<SingleFileOutput, Box<dyn std::error::Error>> {
    let mut cursor = io::Cursor::new(data);
    let mut reader = BufReader::new(&mut cursor);

    let (decompressed, original_name) = match format {
        ArchiveFormat::Gzip => {
            // gzip stores the uncompressed size (mod 2^32) as the trailing
            // ISIZE field; use it as a guard before allocating.
            if data.len() >= 4 {
                let declared_size = u32::from_le_bytes([
                    data[data.len() - 4],
                    data[data.len() - 3],
                    data[data.len() - 2],
                    data[data.len() - 1],
                ]) as u64;
                check_memory_limit("gzip stream", declared_size, memory_limit)?;
            }
            let mut gzip = oxiarc_archive::GzipReader::new(reader)?;
            let name = gzip.header().filename.clone();
            (gzip.decompress()?, name)
        }
        ArchiveFormat::Xz => {
            if let Some(limit) = memory_limit {
                match xz_declared_output_size(data) {
                    Some(declared) => check_memory_limit("xz stream", declared, Some(limit))?,
                    None => {
                        return Err(format!(
                            "refusing to decompress this .xz input under --memory-limit {} bytes: \
                             its stream index (the only field declaring the uncompressed size) is \
                             missing, malformed, or describes a concatenated multi-stream file, so \
                             the limit cannot be enforced before decompression",
                            limit
                        )
                        .into());
                    }
                }
            }
            (oxiarc_archive::xz::decompress(&mut reader)?, None)
        }
        ArchiveFormat::Lz4 => {
            let mut lz4 = Lz4Reader::new(reader)?;
            if let Some(declared) = lz4.original_size() {
                check_memory_limit("lz4 stream", declared, memory_limit)?;
            }
            (lz4.decompress()?, None)
        }
        ArchiveFormat::Zstd => {
            let mut zstd = ZstdReader::new(reader)?;
            if let Some(declared) = zstd.content_size() {
                check_memory_limit("zstd stream", declared, memory_limit)?;
            }
            (zstd.decompress()?, None)
        }
        ArchiveFormat::Bzip2 => {
            // bzip2 declares no output size, but the codec exposes a bounded
            // decoder that errors *before* appending an over-budget block.
            match memory_limit {
                Some(limit) => {
                    let max_out = usize::try_from(limit).unwrap_or(usize::MAX);
                    (
                        oxiarc_archive::bzip2::decompress_with_limit(data, max_out)?,
                        None,
                    )
                }
                None => {
                    let mut bzip2 = Bzip2Reader::new(reader)?;
                    (bzip2.decompress()?, None)
                }
            }
        }
        ArchiveFormat::Brotli => {
            // Brotli declares no output size, but every meta-block declares
            // its own length, so the codec can enforce the budget *before*
            // decoding an over-budget block.
            match memory_limit {
                Some(limit) => {
                    let max_out = usize::try_from(limit).unwrap_or(usize::MAX);
                    (
                        oxiarc_archive::brotli::decompress_with_limit(data, max_out)?,
                        None,
                    )
                }
                None => {
                    let mut brotli = BrotliReader::new(reader)?;
                    (brotli.decompress()?, None)
                }
            }
        }
        ArchiveFormat::Snappy => {
            // Same for Snappy: no total size in the frame, but each chunk
            // declares its own, so the budget is enforced per chunk before
            // that chunk is decoded.
            match memory_limit {
                Some(limit) => (
                    oxiarc_archive::snappy::decompress_with_limit(data, limit)?,
                    None,
                ),
                None => {
                    let mut snappy = SnappyReader::new(reader)?;
                    (snappy.decompress()?, None)
                }
            }
        }
        _ => return Err("Unsupported format for stdin/stdout".into()),
    };

    // Backstop: whatever the container claimed, never hand back more bytes
    // than the caller budgeted for.
    check_memory_limit(
        &format!("{} stream", format),
        decompressed.len() as u64,
        memory_limit,
    )?;

    Ok(SingleFileOutput {
        data: decompressed,
        original_name,
    })
}

/// Decompress a single-file format from a byte slice, discarding any container
/// filename. Thin wrapper over [`decompress_single_file_full`].
fn decompress_single_file(
    data: &[u8],
    format: ArchiveFormat,
    memory_limit: Option<u64>,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    Ok(decompress_single_file_full(data, format, memory_limit)?.data)
}

/// Extract a single-file format to a writer.
fn extract_single_file_to_writer<W: Write>(
    data: &[u8],
    format: ArchiveFormat,
    writer: &mut W,
    _verbose: bool,
    memory_limit: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let decompressed = decompress_single_file(data, format, memory_limit)?;
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

    // Materialize the output directory up front. The archive formats below
    // create it implicitly (per-entry `create_dir_all(parent)`), but the
    // single-file formats write straight into `output`, and without this they
    // failed with a bare `No such file or directory (os error 2)` on a
    // not-yet-existing directory while `-o newdir` worked fine for zip/tar.
    std::fs::create_dir_all(output).map_err(|e| format!("{}: {}", output.display(), e))?;

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
                if let Some(target) = core_symlink_target(entry) {
                    let link_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(msg) =
                        write_symlink_entry(&link_path, &target, overwrite_mode, verbose)?
                    {
                        if verbose {
                            pb.println(msg);
                        }
                    }
                    pb.inc(1);
                    continue;
                }
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
        // Single-file compressed streams (gzip, xz, lz4, zstd, bzip2, brotli,
        // snappy). All seven share one arm so the `--memory-limit` bomb
        // defense — enforced inside `decompress_single_file_full`, the same
        // helper the stdin path uses — cannot drift between the two paths.
        fmt @ (ArchiveFormat::Gzip
        | ArchiveFormat::Xz
        | ArchiveFormat::Lz4
        | ArchiveFormat::Zstd
        | ArchiveFormat::Bzip2
        | ArchiveFormat::Brotli
        | ArchiveFormat::Snappy) => {
            let pb = create_progress_bar(1, progress);
            pb.set_message("Decompressing");

            let mut compressed = Vec::new();
            reader.read_to_end(&mut compressed)?;
            let decoded = decompress_single_file_full(&compressed, fmt, memory_limit)?;

            // gzip may record the original filename; every other format falls
            // back to the archive's stem (`data.xz` -> `data`).
            let out_name = decoded.original_name.unwrap_or_else(|| {
                archive_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });

            if should_extract(&out_name) {
                let out_path = resolve_output_path(output, &out_name, strict_names)?;
                if should_write_file(&out_path, overwrite_mode, verbose)? {
                    std::fs::write(&out_path, &decoded.data)?;
                    if verbose {
                        pb.println(format!(
                            "  Extracted: {} ({} bytes)",
                            out_name,
                            decoded.data.len()
                        ));
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
                if let Some(target) = core_symlink_target(entry) {
                    let link_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(msg) =
                        write_symlink_entry(&link_path, &target, overwrite_mode, verbose)?
                    {
                        if verbose {
                            pb.println(msg);
                        }
                    }
                    pb.inc(1);
                    continue;
                }
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
                if let Some(target) = core_symlink_target(entry) {
                    let link_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(msg) =
                        write_symlink_entry(&link_path, &target, overwrite_mode, verbose)?
                    {
                        if verbose {
                            pb.println(msg);
                        }
                    }
                    pb.inc(1);
                    continue;
                }
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
                let core_entry = entry.to_entry();
                if let Some(target) = core_symlink_target(&core_entry) {
                    let link_path =
                        resolve_output_path(output, &core_entry.sanitized_name(), strict_names)?;
                    if let Some(msg) =
                        write_symlink_entry(&link_path, &target, overwrite_mode, verbose)?
                    {
                        if verbose {
                            pb.println(msg);
                        }
                    }
                    pb.inc(1);
                    continue;
                }
                if entry.is_dir {
                    let dir_path =
                        resolve_output_path(output, &core_entry.sanitized_name(), strict_names)?;
                    std::fs::create_dir_all(&dir_path)?;
                    if verbose {
                        pb.println(format!("  Created: {}", entry.name));
                    }
                } else {
                    let file_path =
                        resolve_output_path(output, &core_entry.sanitized_name(), strict_names)?;
                    if let Some(parent) = file_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    if should_write_file(&file_path, overwrite_mode, verbose)? {
                        check_memory_limit(&entry.name, entry.size, memory_limit)?;
                        let data = sevenz.extract(i)?;
                        std::fs::write(&file_path, &data)?;
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
                if let Some(target) = core_symlink_target(entry) {
                    let link_path =
                        resolve_output_path(output, &entry.sanitized_name(), strict_names)?;
                    if let Some(msg) =
                        write_symlink_entry(&link_path, &target, overwrite_mode, verbose)?
                    {
                        if verbose {
                            pb.println(msg);
                        }
                    }
                    pb.inc(1);
                    continue;
                }
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

    /// Zip-Slip regression: crafted traversal entry names (as used by 7z/CAB/ISO
    /// which historically passed raw `entry.name`) must resolve *inside* the
    /// output root, never above it.
    #[test]
    fn test_resolve_output_path_blocks_traversal() {
        let tmp =
            std::env::temp_dir().join(format!("oxiarc_resolve_traversal_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("create tmp");
        let root_abs = tmp.canonicalize().expect("canonicalize root");

        let malicious = [
            "../../../etc/evil",
            r"..\..\etc\evil",
            "/etc/passwd",
            r"C:\Windows\system32\evil",
            "a/../../b/escape",
        ];
        for name in malicious {
            let resolved = resolve_output_path(&tmp, name, false)
                .unwrap_or_else(|e| panic!("resolve {name:?} failed: {e}"));
            assert!(
                resolved.starts_with(&root_abs),
                "entry {name:?} escaped output root: {resolved:?}"
            );
            assert!(
                !resolved.to_string_lossy().contains(".."),
                "entry {name:?} left `..` in path: {resolved:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The xz index parser is the load-bearing part of the xz `--memory-limit`
    /// enforcement (xz has no other field declaring the uncompressed size), so
    /// it is pinned directly against real `XzWriter` output.
    #[test]
    fn test_xz_declared_output_size_matches_reality() {
        for size in [0usize, 1, 4096, 300_000] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let compressed = oxiarc_archive::XzWriter::new(oxiarc_lzma::LzmaLevel::new(6))
                .compress(&payload)
                .expect("xz compress");
            assert_eq!(
                xz_declared_output_size(&compressed),
                Some(size as u64),
                "xz index declared the wrong uncompressed size for a {size}-byte payload"
            );
        }
    }

    /// Anything that is not one intact XZ stream must yield `None` (the caller
    /// then *refuses* to decompress under a memory limit rather than guessing).
    #[test]
    fn test_xz_declared_output_size_rejects_malformed() {
        let compressed = oxiarc_archive::XzWriter::new(oxiarc_lzma::LzmaLevel::new(6))
            .compress(&vec![7u8; 10_000])
            .expect("xz compress");

        assert_eq!(xz_declared_output_size(&[]), None, "empty input");
        assert_eq!(xz_declared_output_size(b"YZ"), None, "too short");
        // Truncated stream: the footer is gone.
        assert_eq!(
            xz_declared_output_size(&compressed[..compressed.len() - 1]),
            None,
            "truncated stream must not yield a declared size"
        );
        // Concatenated streams: the trailing index describes only the last one,
        // so it must not be trusted.
        let mut concatenated = compressed.clone();
        concatenated.extend_from_slice(&compressed);
        assert_eq!(
            xz_declared_output_size(&concatenated),
            None,
            "concatenated multi-stream xz must not yield a declared size"
        );
        // Footer magic corrupted.
        let mut bad_magic = compressed.clone();
        let last = bad_magic.len() - 1;
        bad_magic[last] = 0x00;
        assert_eq!(
            xz_declared_output_size(&bad_magic),
            None,
            "bad footer magic"
        );
    }

    /// CLI-01 at the unit level: gzip, xz and bzip2 bombs must be refused by
    /// the shared helper both paths use, rather than fully expanding.
    #[test]
    fn test_single_file_memory_limit_rejects_bombs() {
        let bomb = vec![0u8; 2 * 1024 * 1024];
        let limit = Some(64 * 1024u64);

        let gz =
            oxiarc_archive::gzip::compress_with_filename(&bomb, "bomb", 6).expect("gzip compress");
        let xz = oxiarc_archive::XzWriter::new(oxiarc_lzma::LzmaLevel::new(6))
            .compress(&bomb)
            .expect("xz compress");
        let bz2 = oxiarc_archive::Bzip2Writer::with_level(9)
            .compress(&bomb)
            .expect("bzip2 compress");

        for (label, data, format) in [
            ("gzip", gz, ArchiveFormat::Gzip),
            ("xz", xz, ArchiveFormat::Xz),
            ("bzip2", bz2, ArchiveFormat::Bzip2),
        ] {
            assert!(
                decompress_single_file(&data, format, limit).is_err(),
                "{label} bomb was NOT rejected under a 64K memory limit (CLI-01 regression)"
            );
            // ...and the same input must still decode fine with no limit.
            let ok = decompress_single_file(&data, format, None)
                .unwrap_or_else(|e| panic!("{label} failed to decompress without a limit: {e}"));
            assert_eq!(ok.len(), bomb.len(), "{label} decoded the wrong length");
        }
    }

    #[test]
    fn test_core_symlink_target_detection() {
        let file = Entry::file("plain.txt", 0);
        assert!(core_symlink_target(&file).is_none());

        let mut link = Entry::file("link", 0);
        link.entry_type = EntryType::Symlink;
        link.link_target = Some(PathBuf::from("../elsewhere/target"));
        assert_eq!(
            core_symlink_target(&link),
            Some(PathBuf::from("../elsewhere/target"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_symlink_entry_creates_real_symlink() {
        let tmp = std::env::temp_dir().join(format!("oxiarc_symlink_entry_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).expect("create tmp");
        let link_path = tmp.join("mylink");
        let _ = std::fs::remove_file(&link_path);

        let msg = write_symlink_entry(
            &link_path,
            Path::new("target.txt"),
            OverwriteMode::Always,
            false,
        )
        .expect("write symlink");
        assert!(
            msg.is_some(),
            "expected a verbose message for created symlink"
        );

        let meta = std::fs::symlink_metadata(&link_path).expect("symlink metadata");
        assert!(
            meta.file_type().is_symlink(),
            "expected a real symlink, got {:?}",
            meta.file_type()
        );
        let target = std::fs::read_link(&link_path).expect("read_link");
        assert_eq!(target, Path::new("target.txt"));

        // Overwriting an existing symlink with Always must succeed.
        write_symlink_entry(
            &link_path,
            Path::new("other.txt"),
            OverwriteMode::Always,
            false,
        )
        .expect("overwrite symlink");
        assert_eq!(
            std::fs::read_link(&link_path).expect("read_link 2"),
            Path::new("other.txt")
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

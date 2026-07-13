//! Create command implementation.

use crate::style::Styler;
use crate::utils::{
    create_file, mtime_secs, read_dir_for, read_file, read_link_for, symlink_metadata_for,
    unix_mode, write_file,
};
use oxiarc_archive::{
    BrotliWriter, Bzip2Writer, Lz4Writer, LzhCompressionLevel, LzhWriter, SnappyWriter, TarHeader,
    TarWriter, XzWriter, ZipCompressionLevel, ZipWriter, ZstdWriter,
};
use std::collections::HashSet;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

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
    /// Brotli compressed file
    Br,
    /// Snappy compressed file
    Snappy,
}

/// Human-readable list of formats `oxiarc create` can write.
pub(crate) const SUPPORTED_CREATE_FORMATS: &str =
    "zip, tar, gz/gzip, lzh/lha, xz, lz4, bz2/bzip2, zst/zstd, br/brotli, sz/snappy";

/// Resolve the output format from the archive filename extension.
///
/// Returns an error for extensions that have no writer (e.g. `7z`, `cab`,
/// `iso`) instead of silently falling back to ZIP, which would produce a
/// file whose contents contradict its name.
pub(crate) fn output_format_from_extension(archive: &str) -> Result<OutputFormat, String> {
    let ext = PathBuf::from(archive)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "zip" => Ok(OutputFormat::Zip),
        "tar" => Ok(OutputFormat::Tar),
        "gz" | "gzip" => Ok(OutputFormat::Gzip),
        "lzh" | "lha" => Ok(OutputFormat::Lzh),
        "xz" => Ok(OutputFormat::Xz),
        "lz4" => Ok(OutputFormat::Lz4),
        "bz2" | "bzip2" => Ok(OutputFormat::Bz2),
        "zst" | "zstd" => Ok(OutputFormat::Zst),
        "br" | "brotli" => Ok(OutputFormat::Br),
        "sz" | "snappy" => Ok(OutputFormat::Snappy),
        "" => Err(format!(
            "cannot determine output format for '{}' (no file extension); \
             pass --format explicitly. Supported creation formats: {}",
            archive, SUPPORTED_CREATE_FORMATS
        )),
        other => Err(format!(
            "creating '.{}' archives is not supported; refusing to write a \
             different format under that name. Supported creation formats: {}",
            other, SUPPORTED_CREATE_FORMATS
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_create(
    archive: &str,
    files: &[PathBuf],
    format: Option<OutputFormat>,
    compression: CompressionLevel,
    compress_threshold: u64,
    verbose: bool,
    dry_run: bool,
    quiet: bool,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let to_stdout = archive == "-";

    // Dry run mode: show what would be done and exit
    if dry_run {
        return cmd_create_dry_run(archive, files, format, compression);
    }

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
            | OutputFormat::Zst
            | OutputFormat::Br
            | OutputFormat::Snappy => {}
            _ => {
                return Err(
                    "Only single-file formats (gzip, xz, bz2, lz4, zst, br, snappy) are supported for stdout"
                        .into(),
                );
            }
        }
    }

    // Resolve the output format up front so an unsupported target (e.g. .7z)
    // fails before any input is read or any output file is created.
    let format = match format {
        Some(f) => f,
        None => output_format_from_extension(archive)?,
    };

    // Validate file input for single-file formats
    let single_file_format = matches!(
        format,
        OutputFormat::Gzip
            | OutputFormat::Xz
            | OutputFormat::Bz2
            | OutputFormat::Lz4
            | OutputFormat::Zst
            | OutputFormat::Br
            | OutputFormat::Snappy
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
                format
            )
            .into());
        }
        let data = read_file(input_path)?;
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
            let file = create_file(Path::new(archive))?;
            let writer = BufWriter::new(file);
            let mut zip = ZipWriter::new(writer);

            let level = match compression {
                CompressionLevel::Store => ZipCompressionLevel::Store,
                CompressionLevel::Fast => ZipCompressionLevel::Fast,
                CompressionLevel::Normal => ZipCompressionLevel::Normal,
                CompressionLevel::Best => ZipCompressionLevel::Best,
            };
            zip.set_compression(level);

            let mut visited = HashSet::new();
            for path in files {
                add_path_to_zip(
                    &mut zip,
                    path,
                    path,
                    verbose,
                    quiet,
                    compress_threshold,
                    &mut visited,
                )?;
            }

            zip.finish()?;
        }
        OutputFormat::Tar => {
            if to_stdout {
                return Err(
                    "TAR format cannot be written to stdout (use single-file formats)".into(),
                );
            }
            let file = create_file(Path::new(archive))?;
            let writer = BufWriter::new(file);
            let mut tar = TarWriter::new(writer);

            let mut visited = HashSet::new();
            for path in files {
                add_path_to_tar(&mut tar, path, path, verbose, quiet, &mut visited)?;
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
                write_file(Path::new(archive), &compressed)?;
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
            let file = create_file(Path::new(archive))?;
            let writer = BufWriter::new(file);
            let mut lzh = LzhWriter::new(writer);

            let level = match compression {
                CompressionLevel::Store => LzhCompressionLevel::Store,
                _ => LzhCompressionLevel::Lh5,
            };
            lzh.set_compression(level);

            let mut visited = HashSet::new();
            for path in files {
                add_path_to_lzh(&mut lzh, path, path, verbose, quiet, &mut visited)?;
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
                write_file(Path::new(archive), &compressed)?;
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
                write_file(Path::new(archive), &output)?;
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
                write_file(Path::new(archive), &compressed)?;
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
                write_file(Path::new(archive), &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
        OutputFormat::Br => {
            let quality = match compression {
                CompressionLevel::Store => 0,
                CompressionLevel::Fast => 1,
                CompressionLevel::Normal => 6,
                CompressionLevel::Best => 11,
            };

            let brotli_writer = BrotliWriter::with_quality(quality);
            let compressed = brotli_writer.compress(&input_data)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&compressed)?;
                writer.flush()?;
            } else {
                write_file(Path::new(archive), &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
        OutputFormat::Snappy => {
            let snappy_writer = SnappyWriter::new();
            let compressed = snappy_writer.compress(&input_data)?;

            if to_stdout {
                let stdout = io::stdout();
                let mut writer = BufWriter::new(stdout.lock());
                writer.write_all(&compressed)?;
                writer.flush()?;
            } else {
                write_file(Path::new(archive), &compressed)?;
            }

            if verbose {
                eprintln!("  Added: {} ({} bytes)", input_name, input_data.len());
            }
        }
    }

    if !to_stdout && verbose && !quiet {
        eprintln!("{}", styler.success("Archive created successfully"));
    }
    Ok(())
}

/// Dry run mode for create: show what would be archived without creating the file.
fn cmd_create_dry_run(
    archive: &str,
    files: &[PathBuf],
    format: Option<OutputFormat>,
    compression: CompressionLevel,
) -> Result<(), Box<dyn std::error::Error>> {
    let to_stdout = archive == "-";

    // Determine format
    let format = match format {
        Some(f) => f,
        None if to_stdout => OutputFormat::Gzip,
        None => output_format_from_extension(archive)?,
    };

    println!("[DRY RUN] Would create {:?} archive: {}", format, archive);
    println!("[DRY RUN] Compression level: {:?}", compression);

    let mut total_size: u64 = 0;
    let mut file_count: u64 = 0;
    let mut dir_count: u64 = 0;

    let mut visited = HashSet::new();
    for path in files {
        collect_dry_run_stats(
            path,
            &mut total_size,
            &mut file_count,
            &mut dir_count,
            &mut visited,
        )?;
    }

    println!(
        "[DRY RUN] {} file(s), {} directory(ies)",
        file_count, dir_count
    );
    println!("[DRY RUN] Total uncompressed size: {} bytes", total_size);
    println!("[DRY RUN] No archive was created.");
    Ok(())
}

/// Archive entry name for `path`, rooted at the top-level input's basename and
/// always using `/` separators.
fn entry_name(path: &Path, base: &Path) -> String {
    path.strip_prefix(base.parent().unwrap_or(base))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Canonicalize a directory for cycle tracking, falling back to the literal
/// path if canonicalization fails (e.g. permission denied on a parent).
fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Recursively collect stats for dry run output.
///
/// Uses `symlink_metadata` so symlinks are counted as leaf entries and never
/// traversed — a circular symlink therefore cannot make the dry run loop
/// forever, matching the real archiving path.
fn collect_dry_run_stats(
    path: &Path,
    total_size: &mut u64,
    file_count: &mut u64,
    dir_count: &mut u64,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta = symlink_metadata_for(path)?;
    let ftype = meta.file_type();
    if ftype.is_symlink() {
        *file_count += 1;
        println!("[DRY RUN]   {} (symlink)", path.display());
    } else if ftype.is_dir() {
        if !visited.insert(canonical_key(path)) {
            return Ok(());
        }
        *dir_count += 1;
        println!("[DRY RUN]   {}/", path.display());
        for entry in read_dir_for(path)? {
            let entry = entry.map_err(|e| format!("{}: {}", path.display(), e))?;
            collect_dry_run_stats(&entry.path(), total_size, file_count, dir_count, visited)?;
        }
    } else {
        let size = meta.len();
        *total_size += size;
        *file_count += 1;
        println!("[DRY RUN]   {} ({} bytes)", path.display(), size);
    }
    Ok(())
}

fn add_path_to_zip<W: std::io::Write>(
    zip: &mut ZipWriter<W>,
    path: &Path,
    base: &Path,
    verbose: bool,
    quiet: bool,
    compress_threshold: u64,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta = symlink_metadata_for(path)?;
    let ftype = meta.file_type();
    let name = entry_name(path, base);

    if ftype.is_symlink() {
        // ZIP's writer has no dedicated symlink entry type, so the link target
        // is stored as the entry's content. Crucially the link is NOT followed,
        // so a circular symlink cannot recurse and a symlinked tree is not
        // silently dereferenced into a full copy.
        let target = read_link_for(path)?;
        let target_str = target.to_string_lossy();
        zip.add_file(&name, target_str.as_bytes())?;
        if verbose && !quiet {
            println!("  Added (symlink): {} -> {}", name, target_str);
        }
    } else if ftype.is_dir() {
        if !visited.insert(canonical_key(path)) {
            if verbose && !quiet {
                println!("  Skipped (already visited): {}/", name);
            }
            return Ok(());
        }
        zip.add_directory(&name)?;
        if verbose && !quiet {
            println!("  Added: {}/", name);
        }
        for entry in read_dir_for(path)? {
            let entry = entry.map_err(|e| format!("{}: {}", path.display(), e))?;
            add_path_to_zip(
                zip,
                &entry.path(),
                base,
                verbose,
                quiet,
                compress_threshold,
                visited,
            )?;
        }
    } else {
        let data = read_file(path)?;
        if compress_threshold > 0 && (data.len() as u64) < compress_threshold {
            zip.add_file_stored(&name, &data)?;
            if verbose && !quiet {
                println!("  Added: {} ({} bytes, stored)", name, data.len());
            }
        } else {
            zip.add_file(&name, &data)?;
            if verbose && !quiet {
                println!("  Added: {} ({} bytes)", name, data.len());
            }
        }
    }
    Ok(())
}

fn add_path_to_tar<W: std::io::Write>(
    tar: &mut TarWriter<W>,
    path: &Path,
    base: &Path,
    verbose: bool,
    quiet: bool,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta = symlink_metadata_for(path)?;
    let ftype = meta.file_type();
    let name = entry_name(path, base);

    if ftype.is_symlink() {
        // Store a real symlink entry (typeflag '2'); the link is not followed.
        let target = read_link_for(path)?;
        let target_str = target.to_string_lossy();
        let mut header = TarHeader::new_symlink(&name, &target_str);
        header.mtime = mtime_secs(&meta);
        tar.add_entry_from_header(&header, &[])?;
        if verbose && !quiet {
            println!("  Added (symlink): {} -> {}", name, target_str);
        }
    } else if ftype.is_dir() {
        if !visited.insert(canonical_key(path)) {
            if verbose && !quiet {
                println!("  Skipped (already visited): {}/", name);
            }
            return Ok(());
        }
        // Preserve the real directory mode and mtime so `--preserve-*` can
        // round-trip through oxiarc-produced tarballs.
        let dir_name = if name.ends_with('/') {
            name.clone()
        } else {
            format!("{}/", name)
        };
        let mut header = TarHeader::new_directory(&dir_name, unix_mode(&meta));
        header.mtime = mtime_secs(&meta);
        tar.add_entry_from_header(&header, &[])?;
        if verbose && !quiet {
            println!("  Added: {}/", name);
        }
        for entry in read_dir_for(path)? {
            let entry = entry.map_err(|e| format!("{}: {}", path.display(), e))?;
            add_path_to_tar(tar, &entry.path(), base, verbose, quiet, visited)?;
        }
    } else {
        let data = read_file(path)?;
        let mut header = TarHeader::new_file(&name, data.len() as u64, unix_mode(&meta));
        header.mtime = mtime_secs(&meta);
        tar.add_entry_from_header(&header, &data)?;
        if verbose && !quiet {
            println!("  Added: {} ({} bytes)", name, data.len());
        }
    }
    Ok(())
}

fn add_path_to_lzh<W: std::io::Write>(
    lzh: &mut LzhWriter<W>,
    path: &Path,
    base: &Path,
    verbose: bool,
    quiet: bool,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta = symlink_metadata_for(path)?;
    let ftype = meta.file_type();
    let name = entry_name(path, base);

    if ftype.is_symlink() {
        // LZH's writer has no symlink entry type; store the target as content
        // without following the link (no dereference, no recursion cycle).
        let target = read_link_for(path)?;
        let target_str = target.to_string_lossy();
        lzh.add_file(&name, target_str.as_bytes())?;
        if verbose && !quiet {
            println!("  Added (symlink): {} -> {}", name, target_str);
        }
    } else if ftype.is_dir() {
        if !visited.insert(canonical_key(path)) {
            if verbose && !quiet {
                println!("  Skipped (already visited): {}/", name);
            }
            return Ok(());
        }
        lzh.add_directory(&name)?;
        if verbose && !quiet {
            println!("  Added: {}/", name);
        }
        for entry in read_dir_for(path)? {
            let entry = entry.map_err(|e| format!("{}: {}", path.display(), e))?;
            add_path_to_lzh(lzh, &entry.path(), base, verbose, quiet, visited)?;
        }
    } else {
        let data = read_file(path)?;
        lzh.add_file(&name, &data)?;
        if verbose && !quiet {
            println!("  Added: {} ({} bytes)", name, data.len());
        }
    }
    Ok(())
}

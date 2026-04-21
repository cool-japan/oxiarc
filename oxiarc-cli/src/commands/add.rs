//! `add` command — append files to an existing archive.
//!
//! Supports ZIP, TAR, and LZH archive formats. The implementation reads every
//! existing entry into memory, writes them into a temporary archive next to
//! the target, appends the newly-supplied files, and then atomically
//! `std::fs::rename`s the temp file over the original. Single-stream
//! compressed formats (gzip, xz, zstd, bz2, lz4, br, snappy) cannot have
//! entries "added" to them, and 7z/CAB are read-only here — in all those
//! cases a clear error is printed and the process exits with status 2.

use crate::commands::CompressionLevel;
use oxiarc_archive::{
    ArchiveFormat, LzhCompressionLevel, LzhReader, LzhWriter, TarReader, TarWriter,
    ZipCompressionLevel, ZipReader, ZipWriter,
};
use oxiarc_core::EntryType;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// `(entry_name, is_dir, data)` record passed between the read and write
/// phases of the add operation. Simple tuple to avoid a throwaway struct.
type AddEntry = (String, bool, Vec<u8>);

/// Entrypoint invoked by `main.rs` when the user runs `oxiarc add ...`.
pub fn cmd_add(
    archive: &Path,
    files: &[PathBuf],
    compression: CompressionLevel,
    verbose: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !archive.exists() {
        return Err(format!("archive not found: {}", archive.display()).into());
    }

    // Detect format.
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);
    let (format, _magic) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;
    drop(reader);

    match format {
        ArchiveFormat::Zip => add_to_zip(archive, files, compression, verbose, dry_run),
        ArchiveFormat::Tar => add_to_tar(archive, files, verbose, dry_run),
        ArchiveFormat::Lzh => add_to_lzh(archive, files, verbose, dry_run),
        other => {
            eprintln!(
                "error: `oxiarc add` does not support the {} format (only ZIP, TAR, and LZH are appendable).",
                other
            );
            std::process::exit(2);
        }
    }
}

/// Gather all source paths, recursively for directories. Returns
/// `(entry_name, is_dir, data)` tuples with `name` always using `/` separators
/// and rooted at the file's own basename (matching the convention in
/// `create.rs`).
fn collect_input_entries(files: &[PathBuf]) -> Result<Vec<AddEntry>, Box<dyn std::error::Error>> {
    let mut out: Vec<AddEntry> = Vec::new();
    for path in files {
        if !path.exists() {
            return Err(format!("input not found: {}", path.display()).into());
        }
        collect_one(path, path, &mut out)?;
    }
    Ok(out)
}

fn collect_one(
    path: &Path,
    base: &Path,
    out: &mut Vec<AddEntry>,
) -> Result<(), Box<dyn std::error::Error>> {
    let rel = path
        .strip_prefix(base.parent().unwrap_or(base))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");

    if path.is_dir() {
        out.push((rel.clone(), true, Vec::new()));
        for child in std::fs::read_dir(path)? {
            let child = child?;
            collect_one(&child.path(), base, out)?;
        }
    } else {
        let data = std::fs::read(path)?;
        out.push((rel, false, data));
    }
    Ok(())
}

/// Build a sibling temp path: `<archive>.<pid>.tmp` to avoid cross-device
/// rename failures and keep atomicity simple.
fn temp_path_for(archive: &Path) -> PathBuf {
    let mut fname = archive
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("archive"));
    fname.push(format!(".{}.tmp", std::process::id()));
    archive.with_file_name(fname)
}

/// ZIP append — read all existing entries via `ZipReader`, rewrite with a
/// fresh `ZipWriter`, then append the newly-supplied files.
fn add_to_zip(
    archive: &Path,
    files: &[PathBuf],
    compression: CompressionLevel,
    verbose: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read all existing entries into memory.
    let file = File::open(archive)?;
    let reader = BufReader::new(file);
    let mut zip = ZipReader::new(reader)?;
    let existing: Vec<_> = zip.entries().to_vec();

    let mut existing_data: Vec<AddEntry> = Vec::with_capacity(existing.len());
    for entry in &existing {
        if entry.is_dir() {
            existing_data.push((entry.name.clone(), true, Vec::new()));
        } else {
            let data = zip.extract(entry)?;
            existing_data.push((entry.name.clone(), false, data));
        }
    }
    drop(zip);

    let new_entries = collect_input_entries(files)?;

    if dry_run {
        println!("[DRY RUN] Would update ZIP archive: {}", archive.display());
        println!("[DRY RUN] Existing entries: {}", existing_data.len());
        for (name, is_dir, data) in &new_entries {
            if *is_dir {
                println!("[DRY RUN]   + (dir) {}", name);
            } else {
                println!("[DRY RUN]   + {} ({} bytes)", name, data.len());
            }
        }
        println!("[DRY RUN] No archive was modified.");
        return Ok(());
    }

    // Write to temp file.
    let tmp = temp_path_for(archive);
    {
        let out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        let writer = BufWriter::new(out);
        let mut zw = ZipWriter::new(writer);
        let level = match compression {
            CompressionLevel::Store => ZipCompressionLevel::Store,
            CompressionLevel::Fast => ZipCompressionLevel::Fast,
            CompressionLevel::Normal => ZipCompressionLevel::Normal,
            CompressionLevel::Best => ZipCompressionLevel::Best,
        };
        zw.set_compression(level);

        for (name, is_dir, data) in existing_data {
            if is_dir {
                zw.add_directory(&name)?;
            } else {
                zw.add_file(&name, &data)?;
            }
        }
        for (name, is_dir, data) in &new_entries {
            if *is_dir {
                zw.add_directory(name)?;
                if verbose {
                    println!("  Added: {}/", name);
                }
            } else {
                zw.add_file(name, data)?;
                if verbose {
                    println!("  Added: {} ({} bytes)", name, data.len());
                }
            }
        }
        zw.finish()?;
    }

    std::fs::rename(&tmp, archive)?;
    if verbose {
        eprintln!("Updated {}", archive.display());
    }
    Ok(())
}

/// TAR append — read all existing entries, rewrite + append, rename.
fn add_to_tar(
    archive: &Path,
    files: &[PathBuf],
    verbose: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let reader = BufReader::new(file);
    let mut tar = TarReader::new(reader)?;
    let existing: Vec<_> = tar.entries().to_vec();

    let mut existing_data: Vec<AddEntry> = Vec::with_capacity(existing.len());
    for entry in &existing {
        if entry.is_dir() {
            existing_data.push((entry.name.clone(), true, Vec::new()));
        } else if entry.entry_type == EntryType::File {
            let data = tar.extract_to_vec(entry)?;
            existing_data.push((entry.name.clone(), false, data));
        } else {
            // Skip symlinks/specials — add_file API can't represent them.
            if verbose {
                eprintln!(
                    "note: skipping non-file entry {} (type {:?})",
                    entry.name, entry.entry_type
                );
            }
        }
    }
    drop(tar);

    let new_entries = collect_input_entries(files)?;

    if dry_run {
        println!("[DRY RUN] Would update TAR archive: {}", archive.display());
        println!("[DRY RUN] Existing entries: {}", existing_data.len());
        for (name, is_dir, data) in &new_entries {
            if *is_dir {
                println!("[DRY RUN]   + (dir) {}", name);
            } else {
                println!("[DRY RUN]   + {} ({} bytes)", name, data.len());
            }
        }
        println!("[DRY RUN] No archive was modified.");
        return Ok(());
    }

    let tmp = temp_path_for(archive);
    {
        let out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        let writer = BufWriter::new(out);
        let mut tw = TarWriter::new(writer);

        for (name, is_dir, data) in existing_data {
            if is_dir {
                tw.add_directory(&name)?;
            } else {
                tw.add_file(&name, &data)?;
            }
        }
        for (name, is_dir, data) in &new_entries {
            if *is_dir {
                tw.add_directory(name)?;
                if verbose {
                    println!("  Added: {}/", name);
                }
            } else {
                tw.add_file(name, data)?;
                if verbose {
                    println!("  Added: {} ({} bytes)", name, data.len());
                }
            }
        }
        tw.finish()?;
    }

    std::fs::rename(&tmp, archive)?;
    if verbose {
        eprintln!("Updated {}", archive.display());
    }
    Ok(())
}

/// LZH append — read all existing entries, rewrite + append, rename.
///
/// Compression level for the rewrite is forced to `Store` to match the
/// behaviour in `create.rs`, which currently treats `Lh5` as experimental.
fn add_to_lzh(
    archive: &Path,
    files: &[PathBuf],
    verbose: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let reader = BufReader::new(file);
    let mut lzh = LzhReader::new(reader)?;
    let existing: Vec<_> = lzh.entries();

    let mut existing_data: Vec<AddEntry> = Vec::with_capacity(existing.len());
    for entry in &existing {
        if entry.is_dir() {
            existing_data.push((entry.name.clone(), true, Vec::new()));
        } else if entry.entry_type == EntryType::File {
            let data = lzh.extract_to_vec(entry)?;
            existing_data.push((entry.name.clone(), false, data));
        }
    }
    drop(lzh);

    let new_entries = collect_input_entries(files)?;

    if dry_run {
        println!("[DRY RUN] Would update LZH archive: {}", archive.display());
        println!("[DRY RUN] Existing entries: {}", existing_data.len());
        for (name, is_dir, data) in &new_entries {
            if *is_dir {
                println!("[DRY RUN]   + (dir) {}", name);
            } else {
                println!("[DRY RUN]   + {} ({} bytes)", name, data.len());
            }
        }
        println!("[DRY RUN] No archive was modified.");
        return Ok(());
    }

    let tmp = temp_path_for(archive);
    {
        let out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        let writer = BufWriter::new(out);
        let mut lw = LzhWriter::new(writer);
        lw.set_compression(LzhCompressionLevel::Store);

        for (name, is_dir, data) in existing_data {
            if is_dir {
                lw.add_directory(&name)?;
            } else {
                lw.add_file(&name, &data)?;
            }
        }
        for (name, is_dir, data) in &new_entries {
            if *is_dir {
                lw.add_directory(name)?;
                if verbose {
                    println!("  Added: {}/", name);
                }
            } else {
                lw.add_file(name, data)?;
                if verbose {
                    println!("  Added: {} ({} bytes)", name, data.len());
                }
            }
        }
        lw.finish()?;
    }

    std::fs::rename(&tmp, archive)?;
    if verbose {
        eprintln!("Updated {}", archive.display());
    }
    Ok(())
}

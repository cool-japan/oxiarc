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
use oxiarc_archive::zip::{CompressionMethod as ZipMethod, is_entry_encrypted};
use oxiarc_archive::{
    ArchiveFormat, LzhCompressionLevel, LzhMethod, LzhReader, LzhWriter, TarHeader, TarReader,
    TarWriter, ZipCompressionLevel, ZipReader, ZipWriter,
};
use oxiarc_core::EntryType;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// A raw-preserved entry from an existing archive, or new file input data.
///
/// Existing ZIP and LZH entries are stored with their original compressed
/// payload to avoid recompression and preserve byte-for-byte fidelity.
/// TAR entries keep the full `TarHeader` so all metadata (uid/gid/uname/gname/
/// mtime/mode/linkname) round-trips without loss.
enum ArchiveEntry {
    /// Existing ZIP directory entry.
    ZipDir { name: String },
    /// Existing ZIP file entry — pre-compressed bytes preserved verbatim.
    ZipFile {
        name: String,
        method: ZipMethod,
        crc32: u32,
        uncompressed_size: u64,
        mtime: Option<std::time::SystemTime>,
        compressed_data: Vec<u8>,
    },
    /// Existing LZH directory entry.
    LzhDir { name: String },
    /// Existing LZH file entry — pre-compressed bytes preserved verbatim.
    LzhFile {
        name: String,
        method: LzhMethod,
        crc16: u16,
        original_size: u64,
        compressed_data: Vec<u8>,
        mtime: u32,
    },
    /// Existing TAR entry — full header and raw body preserved.
    Tar { header: TarHeader, data: Vec<u8> },
}

/// `(entry_name, is_dir, data)` record used only for newly-added files.
type NewEntry = (String, bool, Vec<u8>);

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
fn collect_input_entries(files: &[PathBuf]) -> Result<Vec<NewEntry>, Box<dyn std::error::Error>> {
    let mut out: Vec<NewEntry> = Vec::new();
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
    out: &mut Vec<NewEntry>,
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
/// fresh `ZipWriter` using raw-preserve for existing entries, then append
/// the newly-supplied files.
fn add_to_zip(
    archive: &Path,
    files: &[PathBuf],
    compression: CompressionLevel,
    verbose: bool,
    dry_run: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Read all existing entries into memory, preserving raw compressed bytes.
    let file = File::open(archive)?;
    let reader = BufReader::new(file);
    let mut zip = ZipReader::new(reader)?;
    let existing: Vec<_> = zip.entries().to_vec();

    let mut existing_entries: Vec<ArchiveEntry> = Vec::with_capacity(existing.len());
    for entry in &existing {
        if entry.is_dir() {
            existing_entries.push(ArchiveEntry::ZipDir {
                name: entry.name.clone(),
            });
        } else {
            // Reject encrypted entries — raw-preserve cannot work without the key.
            if is_entry_encrypted(entry) {
                return Err(format!(
                    "cannot add to archive: entry '{}' is encrypted; raw-preserve requires an unencrypted archive",
                    entry.name
                )
                .into());
            }
            let compressed_data = zip.extract_raw(entry)?;
            existing_entries.push(ArchiveEntry::ZipFile {
                name: entry.name.clone(),
                method: ZipMethod::from_core(&entry.method),
                crc32: entry.crc32.unwrap_or(0),
                uncompressed_size: entry.size,
                mtime: entry.modified,
                compressed_data,
            });
        }
    }
    drop(zip);

    let new_entries = collect_input_entries(files)?;

    if dry_run {
        println!("[DRY RUN] Would update ZIP archive: {}", archive.display());
        println!("[DRY RUN] Existing entries: {}", existing_entries.len());
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

        // Rewrite existing entries verbatim (raw-preserve).
        for entry in existing_entries {
            match entry {
                ArchiveEntry::ZipDir { name } => {
                    zw.add_directory(&name)?;
                }
                ArchiveEntry::ZipFile {
                    name,
                    method,
                    crc32,
                    uncompressed_size,
                    mtime,
                    compressed_data,
                } => {
                    zw.add_file_raw(
                        &name,
                        method,
                        crc32,
                        uncompressed_size,
                        mtime,
                        &compressed_data,
                    )?;
                }
                _ => unreachable!("ZIP path only holds Zip variants"),
            }
        }

        // Append newly-supplied files (re-compressed normally).
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

/// TAR append — read all existing entries preserving full metadata via
/// `TarHeader`, rewrite + append, rename.
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

    // Collect all existing entries with their full headers for metadata preservation.
    let mut existing_entries: Vec<ArchiveEntry> = Vec::with_capacity(existing.len());
    for entry in &existing {
        let header = tar
            .header_for(entry)
            .ok_or_else(|| {
                format!(
                    "internal error: header not found for TAR entry '{}'",
                    entry.name
                )
            })?
            .clone();

        let data = match entry.entry_type {
            EntryType::File => tar.extract_to_vec(entry)?,
            // Directories, symlinks, hard links — no payload.
            _ => Vec::new(),
        };

        existing_entries.push(ArchiveEntry::Tar { header, data });
    }
    drop(tar);

    let new_entries = collect_input_entries(files)?;

    if dry_run {
        println!("[DRY RUN] Would update TAR archive: {}", archive.display());
        println!("[DRY RUN] Existing entries: {}", existing_entries.len());
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

        // Rewrite existing entries via add_entry_from_header to preserve all metadata.
        for entry in existing_entries {
            match entry {
                ArchiveEntry::Tar { header, data } => {
                    tw.add_entry_from_header(&header, &data)?;
                }
                _ => unreachable!("TAR path only holds Tar variants"),
            }
        }

        // Append newly-supplied files.
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

/// LZH append — read all existing entries preserving raw compressed bytes,
/// rewrite + append, rename.
///
/// Existing entries are rewritten verbatim via `add_file_raw` so that the
/// compression method (lh0, lh5, …) and the exact compressed payload are
/// preserved. Newly-added files are compressed with `Store` (`lh0`).
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

    let mut existing_entries: Vec<ArchiveEntry> = Vec::with_capacity(existing.len());
    for entry in &existing {
        if entry.is_dir() {
            existing_entries.push(ArchiveEntry::LzhDir {
                name: entry.name.clone(),
            });
        } else if entry.entry_type == EntryType::File {
            let (method, compressed_data, crc16) = lzh.read_raw_method_data(entry)?;
            let mtime = entry
                .modified
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as u32)
                .unwrap_or(0);
            existing_entries.push(ArchiveEntry::LzhFile {
                name: entry.name.clone(),
                method,
                crc16,
                original_size: entry.size,
                compressed_data,
                mtime,
            });
        }
    }
    drop(lzh);

    let new_entries = collect_input_entries(files)?;

    if dry_run {
        println!("[DRY RUN] Would update LZH archive: {}", archive.display());
        println!("[DRY RUN] Existing entries: {}", existing_entries.len());
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
        // Newly-added files use Store (lh0). Existing files keep their original method.
        lw.set_compression(LzhCompressionLevel::Store);

        // Rewrite existing entries verbatim.
        for entry in existing_entries {
            match entry {
                ArchiveEntry::LzhDir { name } => {
                    lw.add_directory(&name)?;
                }
                ArchiveEntry::LzhFile {
                    name,
                    method,
                    crc16,
                    original_size,
                    compressed_data,
                    mtime,
                } => {
                    lw.add_file_raw(
                        &name,
                        method,
                        crc16,
                        original_size,
                        &compressed_data,
                        mtime,
                        None,
                    )?;
                }
                _ => unreachable!("LZH path only holds Lzh variants"),
            }
        }

        // Append newly-supplied files.
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

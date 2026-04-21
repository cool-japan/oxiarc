use super::SortBy;
use crate::style::Styler;
use crate::utils::{filter_entries, print_entries, print_tree, sort_entries};
use oxiarc_archive::{
    ArchiveFormat, Bzip2Reader, CabReader, LenientWarning, Lz4Reader, SevenZReader, ZipReader,
    ZstdReader,
};
use oxiarc_core::Entry;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
struct EntryJson {
    name: String,
    size: u64,
    compressed_size: u64,
    ratio: f64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    crc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime: Option<i64>,
    is_dir: bool,
}

impl EntryJson {
    fn from_entry(entry: &Entry) -> Self {
        let mtime = entry.modified.and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs() as i64)
        });

        Self {
            name: entry.name.clone(),
            size: entry.size,
            compressed_size: entry.compressed_size,
            ratio: entry.compression_ratio(),
            method: entry.method.name().to_string(),
            crc: entry.crc32,
            mtime,
            is_dir: entry.is_dir(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ArchiveListJson {
    archive: String,
    format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<EntryJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

pub struct ListOptions<'a> {
    pub verbose: bool,
    pub json: bool,
    pub tree: bool,
    pub sort_by: SortBy,
    pub reverse: bool,
    pub include: &'a [String],
    pub exclude: &'a [String],
    /// Continue on corruption (CRC mismatch, bad TAR checksum) with
    /// warnings instead of errors. Warnings are emitted to stderr in
    /// yellow after the listing completes.
    pub lenient: bool,
}

/// Print accumulated lenient-mode warnings to stderr. No-op for empty
/// slices (common case — lenient is a silent no-op on clean archives).
fn print_warnings(warnings: &[LenientWarning], styler: &Styler) {
    for w in warnings {
        let msg = format!("warning: {} [{}]", w.message, w.format);
        eprintln!("{}", styler.warning(&msg));
    }
}

pub fn cmd_list(
    archive: &PathBuf,
    options: &ListOptions<'_>,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    let (format, _magic) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    if options.json {
        return cmd_list_json(archive, format, reader, options, styler);
    }

    println!(
        "Archive: {} ({})",
        styler.path(&archive.display().to_string()),
        format
    );
    println!();

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?.lenient(options.lenient);
            let mut filtered = filter_entries(zip.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree, styler);
            print_warnings(zip.warnings(), styler);
        }
        ArchiveFormat::Tar => {
            // TarReader scans eagerly in `new`, so lenient-header-scan support
            // requires the dedicated `new_lenient` constructor — chaining
            // `.lenient(true)` after `new(reader)?` would not re-run the scan.
            let tar = if options.lenient {
                oxiarc_archive::TarReader::new_lenient(reader)?
            } else {
                oxiarc_archive::TarReader::new(reader)?
            };
            let mut filtered = filter_entries(tar.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree, styler);
            print_warnings(tar.warnings(), styler);
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?.lenient(options.lenient);
            let mut filtered = filter_entries(&lzh.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree, styler);
            print_warnings(lzh.warnings(), styler);
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
            let mut filtered = filter_entries(&sevenz.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree, styler);
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let mut filtered = filter_entries(cab.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree, styler);
        }
        _ => {
            println!("Unsupported format: {}", format);
        }
    }

    Ok(())
}

fn display_entries(entries: &[Entry], verbose: bool, tree: bool, styler: &Styler) {
    if tree {
        print_tree(entries, verbose, styler);
    } else {
        print_entries(entries, verbose, styler);
    }
}

fn cmd_list_json<R: std::io::Read + std::io::Seek>(
    archive: &std::path::Path,
    format: ArchiveFormat,
    reader: R,
    options: &ListOptions<'_>,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = ArchiveListJson {
        archive: archive.display().to_string(),
        format: format!("{}", format),
        entries: None,
        metadata: None,
    };

    // Accumulate lenient-mode warnings separately so we can emit them on
    // stderr after the JSON payload lands on stdout. Keeping them off the
    // JSON document preserves stdout as pure machine-readable output.
    let mut pending_warnings: Vec<LenientWarning> = Vec::new();

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?.lenient(options.lenient);
            let mut filtered = filter_entries(zip.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
            pending_warnings.extend(zip.warnings().iter().cloned());
        }
        ArchiveFormat::Tar => {
            let tar = if options.lenient {
                oxiarc_archive::TarReader::new_lenient(reader)?
            } else {
                oxiarc_archive::TarReader::new(reader)?
            };
            let mut filtered = filter_entries(tar.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
            pending_warnings.extend(tar.warnings().iter().cloned());
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?.lenient(options.lenient);
            let mut filtered = filter_entries(&lzh.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
            pending_warnings.extend(lzh.warnings().iter().cloned());
        }
        ArchiveFormat::Gzip => {
            let gzip = oxiarc_archive::GzipReader::new(reader)?;
            let name = gzip.header().filename.clone().unwrap_or_default();
            output.metadata = Some(serde_json::json!({
                "type": "compressed_file",
                "filename": name
            }));
        }
        ArchiveFormat::Xz => {
            output.metadata = Some(serde_json::json!({
                "type": "compressed_stream",
                "method": "LZMA2"
            }));
        }
        ArchiveFormat::Lz4 => {
            let lz4 = Lz4Reader::new(reader)?;
            output.metadata = Some(serde_json::json!({
                "type": "compressed_file",
                "method": "LZ4",
                "original_size": lz4.original_size()
            }));
        }
        ArchiveFormat::Zstd => {
            let zstd = ZstdReader::new(reader)?;
            output.metadata = Some(serde_json::json!({
                "type": "compressed_file",
                "method": "Zstandard",
                "content_size": zstd.content_size()
            }));
        }
        ArchiveFormat::Bzip2 => {
            let bzip2 = Bzip2Reader::new(reader)?;
            output.metadata = Some(serde_json::json!({
                "type": "compressed_file",
                "method": "Bzip2",
                "block_size": bzip2.block_size(),
                "block_size_level": bzip2.block_size_level()
            }));
        }
        ArchiveFormat::SevenZip => {
            let sevenz = SevenZReader::new(reader)?;
            let mut filtered = filter_entries(&sevenz.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let mut filtered = filter_entries(cab.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
        }
        _ => {
            output.metadata = Some(serde_json::json!({
                "error": "Unsupported format"
            }));
        }
    }

    let json_output = serde_json::to_string_pretty(&output)?;
    println!("{}", json_output);

    print_warnings(&pending_warnings, styler);

    Ok(())
}

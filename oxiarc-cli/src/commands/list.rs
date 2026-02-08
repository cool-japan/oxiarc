//! List command implementation.

use super::SortBy;
use crate::utils::{filter_entries, print_entries, print_tree, sort_entries};
use oxiarc_archive::{
    ArchiveFormat, Bzip2Reader, CabReader, Lz4Reader, SevenZReader, ZipReader, ZstdReader,
};
use oxiarc_core::Entry;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::PathBuf;

/// JSON serializable entry data for archive listings.
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

/// JSON output for archive listing.
#[derive(Debug, Serialize, Deserialize)]
struct ArchiveListJson {
    archive: String,
    format: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<EntryJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<serde_json::Value>,
}

/// Options for listing archive contents.
pub struct ListOptions<'a> {
    pub verbose: bool,
    pub json: bool,
    pub tree: bool,
    pub sort_by: SortBy,
    pub reverse: bool,
    pub include: &'a [String],
    pub exclude: &'a [String],
}

pub fn cmd_list(
    archive: &PathBuf,
    options: &ListOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::open(archive)?;
    let mut reader = BufReader::new(file);

    // Detect format
    let (format, _magic) = ArchiveFormat::detect(&mut reader)?;
    reader.seek(SeekFrom::Start(0))?;

    if options.json {
        // JSON output mode
        return cmd_list_json(archive, format, reader, options);
    }

    println!("Archive: {} ({})", archive.display(), format);
    println!();

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let mut filtered = filter_entries(zip.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree);
        }
        ArchiveFormat::Tar => {
            let tar = oxiarc_archive::TarReader::new(reader)?;
            let mut filtered = filter_entries(tar.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree);
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?;
            let mut filtered = filter_entries(&lzh.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree);
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
            display_entries(&filtered, options.verbose, options.tree);
        }
        ArchiveFormat::Cab => {
            let cab = CabReader::new(reader)?;
            let mut filtered = filter_entries(cab.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            display_entries(&filtered, options.verbose, options.tree);
        }
        _ => {
            println!("Unsupported format: {}", format);
        }
    }

    Ok(())
}

/// Display entries in either table or tree format.
fn display_entries(entries: &[Entry], verbose: bool, tree: bool) {
    if tree {
        print_tree(entries, verbose);
    } else {
        print_entries(entries, verbose);
    }
}

/// Output archive listing as JSON.
fn cmd_list_json<R: std::io::Read + std::io::Seek>(
    archive: &std::path::Path,
    format: ArchiveFormat,
    reader: R,
    options: &ListOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut output = ArchiveListJson {
        archive: archive.display().to_string(),
        format: format!("{}", format),
        entries: None,
        metadata: None,
    };

    match format {
        ArchiveFormat::Zip => {
            let zip = ZipReader::new(reader)?;
            let mut filtered = filter_entries(zip.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
        }
        ArchiveFormat::Tar => {
            let tar = oxiarc_archive::TarReader::new(reader)?;
            let mut filtered = filter_entries(tar.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
        }
        ArchiveFormat::Lzh => {
            let lzh = oxiarc_archive::LzhReader::new(reader)?;
            let mut filtered = filter_entries(&lzh.entries(), options.include, options.exclude);
            sort_entries(&mut filtered, options.sort_by, options.reverse);
            output.entries = Some(filtered.iter().map(EntryJson::from_entry).collect());
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

    // Pretty-print JSON output
    let json_output = serde_json::to_string_pretty(&output)?;
    println!("{}", json_output);

    Ok(())
}

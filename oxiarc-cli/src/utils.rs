use crate::commands::SortBy;
use crate::style::Styler;

/// Parse a human-readable byte size string into a raw `u64` byte count.
///
/// Accepted formats: `"100"`, `"100k"` / `"100K"` (×1 000), `"100m"` / `"100M"` (×1 000 000),
/// `"100g"` / `"100G"` (×1 000 000 000).  The suffixes use decimal (SI) multipliers, not
/// binary (KiB/MiB/GiB).
///
/// # Errors
/// Returns `Err(String)` with a human-readable message when the input cannot be parsed.
pub fn parse_byte_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num_str, mult) = if let Some(rest) = s.strip_suffix(['k', 'K']) {
        (rest, 1_000u64)
    } else if let Some(rest) = s.strip_suffix(['m', 'M']) {
        (rest, 1_000_000u64)
    } else if let Some(rest) = s.strip_suffix(['g', 'G']) {
        (rest, 1_000_000_000u64)
    } else {
        (s, 1u64)
    };
    num_str
        .parse::<u64>()
        .map(|n| n * mult)
        .map_err(|_| format!("invalid byte size: '{s}'"))
}
use glob::Pattern;
use indicatif::{ProgressBar, ProgressStyle};
use oxiarc_core::{Entry, EntryType};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub type ExtractedEntry = (String, bool, Vec<u8>);

/// A seekable byte source: either a file on disk or an in-memory buffer read
/// from standard input. Used by the read-only commands (`list`, `test`,
/// `info`, `detect`) so they can accept `-` as a stand-in for stdin, mirroring
/// what `extract`/`create` already do for their streams.
pub trait ReadSeek: Read + std::io::Seek {}
impl<T: Read + std::io::Seek> ReadSeek for T {}

/// Open an archive input, transparently handling the `-` stdin sentinel.
///
/// For a real path the file is opened lazily and buffered. For `-` the whole
/// of stdin is slurped into memory and wrapped in a [`std::io::Cursor`] so the
/// archive readers (which all require `Seek`) can operate on a non-seekable
/// pipe. Every I/O error names the offending source.
pub fn open_input(path: &str) -> Result<Box<dyn ReadSeek>, Box<dyn std::error::Error>> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| format!("<stdin>: {e}"))?;
        Ok(Box::new(std::io::Cursor::new(buf)))
    } else {
        let file = open_file(Path::new(path))?;
        Ok(Box::new(BufReader::new(file)))
    }
}

/// Human-facing display name for an archive source (`<stdin>` for `-`).
pub fn input_display_name(path: &str) -> String {
    if path == "-" {
        "<stdin>".to_string()
    } else {
        path.to_string()
    }
}

/// `File::open` with the path folded into the error message.
///
/// The bare [`std::io::Error`] returned by the standard library omits the path
/// (e.g. just "No such file or directory"), which is useless in a CLI that may
/// touch many files. These wrappers guarantee every filesystem error names the
/// file it refers to.
pub fn open_file(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// `File::create` with the path folded into the error message.
pub fn create_file(path: &Path) -> Result<File, String> {
    File::create(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// `std::fs::read` with the path folded into the error message.
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// `std::fs::write` with the path folded into the error message.
pub fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("{}: {}", path.display(), e))
}

/// `std::fs::symlink_metadata` (does NOT follow symlinks) with the path folded
/// into the error message.
pub fn symlink_metadata_for(path: &Path) -> Result<std::fs::Metadata, String> {
    std::fs::symlink_metadata(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// `std::fs::read_dir` with the path folded into the error message.
pub fn read_dir_for(path: &Path) -> Result<std::fs::ReadDir, String> {
    std::fs::read_dir(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// `std::fs::read_link` with the path folded into the error message.
pub fn read_link_for(path: &Path) -> Result<std::path::PathBuf, String> {
    std::fs::read_link(path).map_err(|e| format!("{}: {}", path.display(), e))
}

/// Extract the Unix permission bits (masked to `0o7777`) from `metadata`.
///
/// On non-Unix platforms there is no meaningful mode, so a sane default is
/// returned for files (`0o644`) and directories (`0o755`).
#[cfg(unix)]
pub fn unix_mode(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    metadata.mode() & 0o7777
}

#[cfg(not(unix))]
pub fn unix_mode(metadata: &std::fs::Metadata) -> u32 {
    if metadata.is_dir() { 0o755 } else { 0o644 }
}

/// Seconds since the Unix epoch for `metadata`'s modification time, or `0` if
/// unavailable (pre-epoch or unsupported platform).
pub fn mtime_secs(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn create_progress_bar(len: u64, enable: bool) -> ProgressBar {
    if !enable {
        return ProgressBar::hidden();
    }

    let pb = ProgressBar::new(len);
    // Degrade gracefully to the plain default bar if a future template edit
    // introduces a parse error, rather than panicking (no-panic policy).
    let style = ProgressStyle::default_bar()
        .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
        .map(|s| s.progress_chars("█▓▒░ "))
        .unwrap_or_else(|_| ProgressStyle::default_bar());
    pb.set_style(style);
    pb
}

pub fn matches_filters(name: &str, include: &[String], exclude: &[String]) -> bool {
    for pattern_str in exclude {
        if let Ok(pattern) = Pattern::new(pattern_str) {
            if pattern.matches(name) {
                return false;
            }
        }
    }

    if include.is_empty() {
        return true;
    }

    for pattern_str in include {
        if let Ok(pattern) = Pattern::new(pattern_str) {
            if pattern.matches(name) {
                return true;
            }
        }
    }

    false
}

pub fn filter_entries(entries: &[Entry], include: &[String], exclude: &[String]) -> Vec<Entry> {
    if include.is_empty() && exclude.is_empty() {
        return entries.to_vec();
    }

    entries
        .iter()
        .filter(|e| matches_filters(&e.name, include, exclude))
        .cloned()
        .collect()
}

pub fn print_entries(entries: &[Entry], verbose: bool, styler: &Styler) {
    if verbose {
        let header_text = format!(
            "{:>10} {:>10} {:>6} {:>8}  Name",
            "Size", "Compressed", "Ratio", "Method"
        );
        println!("{}", styler.header(&header_text));
        println!("{}", "-".repeat(60));

        let mut total_size = 0u64;
        let mut total_compressed = 0u64;

        for entry in entries {
            let ratio = if entry.size > 0 {
                format!("{:.1}%", entry.space_savings())
            } else {
                "-".to_string()
            };

            let (type_prefix, styled_name) = if entry.is_dir() {
                ("d ", format!("{}", styler.dir_entry(&entry.name)))
            } else if entry.entry_type == EntryType::Symlink {
                ("l ", format!("{}", styler.symlink_entry(&entry.name)))
            } else {
                ("  ", format!("{}", styler.file_entry(&entry.name)))
            };

            let size_str = format!("{:>10}", entry.size);
            let compressed_str = format!("{:>10}", entry.compressed_size);
            let ratio_str = format!("{:>6}", ratio);

            println!(
                "{} {} {} {:>8}  {}{}",
                styler.size(&size_str),
                styler.size(&compressed_str),
                ratio_str,
                entry.method.name(),
                type_prefix,
                styled_name
            );

            total_size += entry.size;
            total_compressed += entry.compressed_size;
        }

        println!("{}", "-".repeat(60));
        let total_ratio = if total_size > 0 {
            (1.0 - total_compressed as f64 / total_size as f64) * 100.0
        } else {
            0.0
        };
        let total_line = format!(
            "{:>10} {:>10} {:>5.1}%          {} files",
            total_size,
            total_compressed,
            total_ratio,
            entries.len()
        );
        println!("{}", styler.size(&total_line));
    } else {
        for entry in entries {
            if entry.is_dir() {
                println!("{}", styler.dir_entry(&entry.name));
            } else if entry.entry_type == EntryType::Symlink {
                println!("{}", styler.symlink_entry(&entry.name));
            } else {
                println!("{}", styler.file_entry(&entry.name));
            }
        }
    }
}

pub fn sort_entries(entries: &mut [Entry], sort_by: SortBy, reverse: bool) {
    match sort_by {
        SortBy::Name => {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
        }
        SortBy::Size => {
            entries.sort_by_key(|a| a.size);
        }
        SortBy::Date => {
            entries.sort_by_key(|a| a.modified);
        }
        SortBy::Ratio => {
            entries.sort_by(|a, b| {
                let ratio_a = if a.size > 0 {
                    a.compressed_size as f64 / a.size as f64
                } else {
                    0.0
                };
                let ratio_b = if b.size > 0 {
                    b.compressed_size as f64 / b.size as f64
                } else {
                    0.0
                };
                ratio_a
                    .partial_cmp(&ratio_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }

    if reverse {
        entries.reverse();
    }
}

#[derive(Debug, Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
    entry: Option<Entry>,
    is_dir: bool,
}

impl TreeNode {
    fn insert(&mut self, path: &str, entry: Entry) {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        self.insert_parts(&parts, 0, entry);
    }

    fn insert_parts(&mut self, parts: &[&str], idx: usize, entry: Entry) {
        if idx >= parts.len() {
            self.entry = Some(entry);
            return;
        }

        let part = parts[idx];
        let child = self.children.entry(part.to_string()).or_default();

        if idx == parts.len() - 1 {
            child.entry = Some(entry.clone());
            child.is_dir = entry.is_dir();
        } else {
            child.is_dir = true;
            child.insert_parts(parts, idx + 1, entry);
        }
    }
}

pub fn print_tree(entries: &[Entry], verbose: bool, styler: &Styler) {
    let mut root = TreeNode::default();
    for entry in entries {
        root.insert(&entry.name, entry.clone());
    }

    print_tree_node(&root, "", verbose, true, styler);
}

fn print_tree_node(node: &TreeNode, prefix: &str, verbose: bool, is_root: bool, styler: &Styler) {
    let mut children: Vec<(&String, &TreeNode)> = node.children.iter().collect();
    children.sort_by(|a, b| match (a.1.is_dir, b.1.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(b.0),
    });

    for (i, (name, child)) in children.iter().enumerate() {
        let is_last_child = i == children.len() - 1;

        let (current_prefix, next_prefix) = if is_root {
            ("".to_string(), "".to_string())
        } else if is_last_child {
            (format!("{}{}── ", prefix, "└"), format!("{}    ", prefix))
        } else {
            (format!("{}{}── ", prefix, "├"), format!("{}│   ", prefix))
        };

        let type_indicator = if child.is_dir { "/" } else { "" };

        let styled_name = if child.is_dir {
            format!("{}", styler.dir_entry(name))
        } else {
            format!("{}", styler.file_entry(name))
        };

        if verbose {
            if let Some(ref entry) = child.entry {
                let size_str = if child.is_dir {
                    "-".to_string()
                } else {
                    format_size(entry.size)
                };
                println!(
                    "{}{}{} [{}]",
                    current_prefix,
                    styled_name,
                    type_indicator,
                    styler.size(&size_str)
                );
            } else {
                println!("{}{}{}", current_prefix, styled_name, type_indicator);
            }
        } else {
            println!("{}{}{}", current_prefix, styled_name, type_indicator);
        }

        if child.is_dir {
            print_tree_node(child, &next_prefix, verbose, false, styler);
        }
    }
}

fn format_size(size: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if size >= GB {
        format!("{:.1} GB", size as f64 / GB as f64)
    } else if size >= MB {
        format!("{:.1} MB", size as f64 / MB as f64)
    } else if size >= KB {
        format!("{:.1} KB", size as f64 / KB as f64)
    } else {
        format!("{size} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_byte_size_plain() {
        assert_eq!(parse_byte_size("100").unwrap(), 100);
        assert_eq!(parse_byte_size("0").unwrap(), 0);
        assert_eq!(parse_byte_size("1048576").unwrap(), 1_048_576);
    }

    #[test]
    fn test_parse_byte_size_kilo() {
        assert_eq!(parse_byte_size("100K").unwrap(), 100_000);
        assert_eq!(parse_byte_size("100k").unwrap(), 100_000);
        assert_eq!(parse_byte_size("1K").unwrap(), 1_000);
    }

    #[test]
    fn test_parse_byte_size_mega() {
        assert_eq!(parse_byte_size("100M").unwrap(), 100_000_000);
        assert_eq!(parse_byte_size("100m").unwrap(), 100_000_000);
        assert_eq!(parse_byte_size("1M").unwrap(), 1_000_000);
    }

    #[test]
    fn test_parse_byte_size_giga() {
        assert_eq!(parse_byte_size("1G").unwrap(), 1_000_000_000);
        assert_eq!(parse_byte_size("1g").unwrap(), 1_000_000_000);
    }

    #[test]
    fn test_parse_byte_size_error() {
        assert!(parse_byte_size("garbage").is_err());
        assert!(parse_byte_size("").is_err());
        assert!(parse_byte_size("-1").is_err());
        assert!(parse_byte_size("1.5M").is_err());
    }

    #[test]
    fn test_parse_byte_size_whitespace() {
        assert_eq!(parse_byte_size("  100M  ").unwrap(), 100_000_000);
    }
}

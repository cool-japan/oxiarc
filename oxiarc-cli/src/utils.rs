//! Utility functions for the CLI.

use glob::Pattern;
use indicatif::{ProgressBar, ProgressStyle};
use oxiarc_core::Entry;

/// An extracted entry: (filename, is_directory, file_contents).
pub type ExtractedEntry = (String, bool, Vec<u8>);

/// Create a progress bar with standard styling.
pub fn create_progress_bar(len: u64, enable: bool) -> ProgressBar {
    if !enable {
        return ProgressBar::hidden();
    }

    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .expect("progress bar template is valid")
            .progress_chars("█▓▒░ "),
    );
    pb
}

/// Check if a filename matches the filter patterns.
/// - If include patterns are specified, the name must match at least one
/// - If exclude patterns are specified, the name must not match any
pub fn matches_filters(name: &str, include: &[String], exclude: &[String]) -> bool {
    // Check exclude patterns first
    for pattern_str in exclude {
        if let Ok(pattern) = Pattern::new(pattern_str) {
            if pattern.matches(name) {
                return false;
            }
        }
    }

    // If no include patterns, include everything (that wasn't excluded)
    if include.is_empty() {
        return true;
    }

    // Check include patterns
    for pattern_str in include {
        if let Ok(pattern) = Pattern::new(pattern_str) {
            if pattern.matches(name) {
                return true;
            }
        }
    }

    false
}

/// Filter entries based on include/exclude patterns.
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

/// Print entries in a formatted table.
pub fn print_entries(entries: &[Entry], verbose: bool) {
    if verbose {
        println!(
            "{:>10} {:>10} {:>6} {:>8}  Name",
            "Size", "Compressed", "Ratio", "Method",
        );
        println!("{}", "-".repeat(60));

        let mut total_size = 0u64;
        let mut total_compressed = 0u64;

        for entry in entries {
            let ratio = if entry.size > 0 {
                format!("{:.1}%", entry.space_savings())
            } else {
                "-".to_string()
            };

            let type_prefix = if entry.is_dir() {
                "d "
            } else if entry.entry_type == oxiarc_core::EntryType::Symlink {
                "l "
            } else {
                "  "
            };

            println!(
                "{:>10} {:>10} {:>6} {:>8}  {}{}",
                entry.size,
                entry.compressed_size,
                ratio,
                entry.method.name(),
                type_prefix,
                entry.name
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
        println!(
            "{:>10} {:>10} {:>5.1}%          {} files",
            total_size,
            total_compressed,
            total_ratio,
            entries.len()
        );
    } else {
        for entry in entries {
            println!("{}", entry.name);
        }
    }
}

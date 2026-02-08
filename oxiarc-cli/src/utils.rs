//! Utility functions for the CLI.

use crate::commands::SortBy;
use glob::Pattern;
use indicatif::{ProgressBar, ProgressStyle};
use oxiarc_core::Entry;
use std::collections::BTreeMap;

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

/// Sort entries by the specified criteria.
pub fn sort_entries(entries: &mut [Entry], sort_by: SortBy, reverse: bool) {
    match sort_by {
        SortBy::Name => {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
        }
        SortBy::Size => {
            entries.sort_by(|a, b| a.size.cmp(&b.size));
        }
        SortBy::Date => {
            entries.sort_by(|a, b| a.modified.cmp(&b.modified));
        }
    }

    if reverse {
        entries.reverse();
    }
}

/// Represents a node in the directory tree.
#[derive(Debug, Default)]
struct TreeNode {
    /// Children indexed by name.
    children: BTreeMap<String, TreeNode>,
    /// Entry data if this is a file/directory leaf.
    entry: Option<Entry>,
    /// Whether this is a directory (has children or ends with /).
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
            // This is the final part
            child.entry = Some(entry.clone());
            child.is_dir = entry.is_dir();
        } else {
            // Intermediate directory
            child.is_dir = true;
            child.insert_parts(parts, idx + 1, entry);
        }
    }
}

/// Print entries as a directory tree.
pub fn print_tree(entries: &[Entry], verbose: bool) {
    // Build the tree structure
    let mut root = TreeNode::default();
    for entry in entries {
        root.insert(&entry.name, entry.clone());
    }

    // Print the tree
    print_tree_node(&root, "", verbose, true);
}

/// Recursively print a tree node.
fn print_tree_node(node: &TreeNode, prefix: &str, verbose: bool, is_root: bool) {
    // Sort children: directories first, then files, both alphabetically
    let mut children: Vec<(&String, &TreeNode)> = node.children.iter().collect();
    children.sort_by(|a, b| match (a.1.is_dir, b.1.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.0.cmp(b.0),
    });

    for (i, (name, child)) in children.iter().enumerate() {
        let is_last_child = i == children.len() - 1;

        // Determine the prefix for this item
        let (current_prefix, next_prefix) = if is_root {
            ("".to_string(), "".to_string())
        } else if is_last_child {
            (format!("{}{}── ", prefix, "└"), format!("{}    ", prefix))
        } else {
            (format!("{}{}── ", prefix, "├"), format!("{}│   ", prefix))
        };

        // Format the entry line
        let type_indicator = if child.is_dir { "/" } else { "" };

        if verbose {
            if let Some(ref entry) = child.entry {
                let size_str = if child.is_dir {
                    "-".to_string()
                } else {
                    format_size(entry.size)
                };
                println!(
                    "{}{}{} [{}]",
                    current_prefix, name, type_indicator, size_str
                );
            } else {
                println!("{}{}{}", current_prefix, name, type_indicator);
            }
        } else {
            println!("{}{}{}", current_prefix, name, type_indicator);
        }

        // Recursively print children
        if child.is_dir {
            print_tree_node(child, &next_prefix, verbose, false);
        }
    }
}

/// Format file size in human-readable format.
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
        format!("{} B", size)
    }
}

use crate::commands::SortBy;
use crate::style::Styler;
use glob::Pattern;
use indicatif::{ProgressBar, ProgressStyle};
use oxiarc_core::{Entry, EntryType};
use std::collections::BTreeMap;

pub type ExtractedEntry = (String, bool, Vec<u8>);

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

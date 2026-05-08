//! Windows-specific path handling: long paths + reserved name sanitization.
//!
//! Even on non-Windows hosts, callers may extract archives that later get
//! copied onto a Windows filesystem. Sanitizing reserved names at extraction
//! time prevents surprise failures on Windows. The long-path prefix
//! (`\\?\`) is only applied on Windows (`#[cfg(windows)]`).

use std::path::{Path, PathBuf};

/// Reserved names that must not appear as the stem of a Windows filename.
const RESERVED_EXACT: &[&str] = &["CON", "PRN", "AUX", "NUL"];

/// Reserved numbered device names (COM1..COM9, LPT1..LPT9).
const RESERVED_PREFIX: &[&str] = &[
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3",
    "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Returns true if `basename` (the filename without directory) is a Windows
/// reserved name.
///
/// Only the stem (the part before the first `.`) is considered, and the match
/// is case-insensitive. So `CON`, `con`, `Con`, `CON.txt`, and `NUL.EXE` are
/// all reserved, while `CONFIG` and `COM10` are not.
pub fn is_reserved_name(basename: &str) -> bool {
    let stem = basename.split('.').next().unwrap_or(basename);
    let upper = stem.to_ascii_uppercase();
    RESERVED_EXACT.iter().any(|r| upper == *r) || RESERVED_PREFIX.iter().any(|r| upper == *r)
}

/// Sanitize a filename by appending `_` to the stem if it collides with a
/// Windows reserved name.
///
/// In `strict` mode, returns an error instead of sanitizing. The underscore is
/// inserted before the extension so the original extension is preserved:
/// `CON.txt` -> `CON_.txt`, `CON` -> `CON_`.
pub fn sanitize_reserved_name(name: &str, strict: bool) -> Result<String, String> {
    if !is_reserved_name(name) {
        return Ok(name.to_string());
    }
    if strict {
        return Err(format!("reserved filename: {}", name));
    }
    match name.find('.') {
        Some(dot) => {
            let (stem, rest) = name.split_at(dot);
            Ok(format!("{}_{}", stem, rest))
        }
        None => Ok(format!("{}_", name)),
    }
}

/// Apply `sanitize_reserved_name` only to the final component of a relative
/// path, preserving the directory components unchanged.
///
/// Used by extraction to rewrite output paths before they are created on disk.
pub fn sanitize_relative_path(rel: &str, strict: bool) -> Result<String, String> {
    if rel.is_empty() {
        return Ok(String::new());
    }
    // Preserve a trailing `/` (directory markers in ZIP/TAR).
    let (body, trailing_slash) = if let Some(stripped) = rel.strip_suffix('/') {
        (stripped, true)
    } else {
        (rel, false)
    };

    let mut parts: Vec<String> = Vec::new();
    for part in body.split('/') {
        parts.push(sanitize_reserved_name(part, strict)?);
    }
    let mut out = parts.join("/");
    if trailing_slash {
        out.push('/');
    }
    Ok(out)
}

/// On Windows, prefix paths longer than 255 chars with `\\?\` for long-path
/// support. On non-Windows, return the path unchanged.
#[cfg(windows)]
pub fn long_path_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.len() > 255 && !s.starts_with(r"\\?\") {
        PathBuf::from(format!(r"\\?\{}", s))
    } else {
        path.to_path_buf()
    }
}

/// On non-Windows, `long_path_prefix` is a no-op that simply clones the path.
#[cfg(not(windows))]
pub fn long_path_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_reserved_name() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("con"));
        assert!(is_reserved_name("Con"));
        assert!(is_reserved_name("CON.txt"));
        assert!(is_reserved_name("NUL"));
        assert!(is_reserved_name("NUL.EXE"));
        assert!(!is_reserved_name("CONFIG"));
        assert!(!is_reserved_name("CONFIG.txt"));
        assert!(is_reserved_name("COM1"));
        assert!(!is_reserved_name("COM10"));
        assert!(is_reserved_name("LPT9"));
        assert!(!is_reserved_name("LPT10"));
    }

    #[test]
    fn test_sanitize_reserved_name_non_strict() {
        assert_eq!(
            sanitize_reserved_name("safe.txt", false).expect("safe"),
            "safe.txt"
        );
        assert_eq!(sanitize_reserved_name("CON", false).expect("CON"), "CON_");
        assert_eq!(
            sanitize_reserved_name("CON.txt", false).expect("CON.txt"),
            "CON_.txt"
        );
    }

    #[test]
    fn test_sanitize_reserved_name_strict() {
        assert!(sanitize_reserved_name("safe.txt", true).is_ok());
        assert!(sanitize_reserved_name("CON", true).is_err());
        assert!(sanitize_reserved_name("CON.txt", true).is_err());
    }

    #[test]
    fn test_sanitize_relative_path_preserves_dirs() {
        assert_eq!(
            sanitize_relative_path("dir1/CON.txt", false).expect("path"),
            "dir1/CON_.txt"
        );
        assert_eq!(
            sanitize_relative_path("dir/sub/NUL", false).expect("path"),
            "dir/sub/NUL_"
        );
        assert_eq!(
            sanitize_relative_path("safe/path/file.txt", false).expect("path"),
            "safe/path/file.txt"
        );
        assert_eq!(
            sanitize_relative_path("some/dir/", false).expect("path"),
            "some/dir/"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn test_long_path_prefix_noop_on_non_windows() {
        let tmp = std::env::temp_dir();
        let p = tmp.join("foo");
        let expected = tmp.join("foo");
        assert_eq!(long_path_prefix(&p), expected);
    }
}

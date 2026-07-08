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

/// If `part` begins with a Windows drive designator (`C:`, `d:foo`, …), return
/// the portion after the `X:` prefix; otherwise return `part` unchanged.
///
/// This strips `Component::Prefix`-style drive markers that would otherwise
/// survive naive `/`-splitting on non-Windows hosts.
fn strip_drive_prefix(part: &str) -> &str {
    let bytes = part.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        &part[2..]
    } else {
        part
    }
}

/// Trailing `.` and space characters are stripped by Windows when creating a
/// file, so `foo.` and `foo ` silently become `foo`, colliding with a sibling
/// named `foo`. Suffix such components with `_` to keep them distinct and
/// creatable. Returns the component unchanged when it has no trailing `.`/` `.
fn sanitize_trailing(name: &str) -> String {
    if name.ends_with('.') || name.ends_with(' ') {
        format!("{}_", name)
    } else {
        name.to_string()
    }
}

/// Sanitize a single path component so it is safe to create on a Windows
/// filesystem.
///
/// Handles two independent hazards:
///   * Reserved device names (`CON`, `NUL`, `COM1`, …): the stem gets a `_`
///     suffix inserted before the extension (`CON.txt` -> `CON_.txt`).
///   * Trailing `.`/space: suffixed with `_` (`foo.` -> `foo._`).
///
/// In `strict` mode, returns an error instead of rewriting.
pub fn sanitize_reserved_name(name: &str, strict: bool) -> Result<String, String> {
    if is_reserved_name(name) {
        if strict {
            return Err(format!("reserved filename: {}", name));
        }
        let renamed = match name.find('.') {
            Some(dot) => {
                let (stem, rest) = name.split_at(dot);
                format!("{}_{}", stem, rest)
            }
            None => format!("{}_", name),
        };
        return Ok(sanitize_trailing(&renamed));
    }

    if name.ends_with('.') || name.ends_with(' ') {
        if strict {
            return Err(format!(
                "invalid trailing '.' or space in filename: {}",
                name
            ));
        }
        return Ok(sanitize_trailing(name));
    }

    Ok(name.to_string())
}

/// Sanitize a relative extraction path so it cannot escape the output
/// directory and is safe on a Windows filesystem.
///
/// This is the first line of defense against Zip-Slip: it treats both `/` and
/// `\` as separators (so `..\..\` traversal is caught on non-Windows hosts
/// too), drops `.`/`..`/root/drive-prefix components, and applies
/// [`sanitize_reserved_name`] to each surviving component. A trailing `/`
/// (directory marker in ZIP/TAR) is preserved when the result is non-empty.
///
/// The returned string is always a clean relative path with no traversal
/// components; callers still apply a defense-in-depth containment check after
/// joining it to the output root.
pub fn sanitize_relative_path(rel: &str, strict: bool) -> Result<String, String> {
    if rel.is_empty() {
        return Ok(String::new());
    }
    // Treat backslashes as separators so Windows-style traversal (`..\..\`) is
    // normalized on every host, not just Windows.
    let normalized = rel.replace('\\', "/");
    let trailing_slash = normalized.ends_with('/');

    let mut parts: Vec<String> = Vec::new();
    for raw in normalized.split('/') {
        // Collapse empty components (leading/duplicate/trailing separators).
        if raw.is_empty() {
            continue;
        }
        // Drop current-dir and parent-dir traversal markers.
        if raw == "." || raw == ".." {
            continue;
        }
        // Drop drive designators (`C:`), keeping any tail (`C:foo` -> `foo`).
        let cleaned = strip_drive_prefix(raw);
        if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
            continue;
        }
        parts.push(sanitize_reserved_name(cleaned, strict)?);
    }

    let mut out = parts.join("/");
    if trailing_slash && !out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// On Windows, prefix paths longer than 255 chars with the `\\?\` extended-length
/// marker. On non-Windows, return the path unchanged.
///
/// The marker is only valid for *absolute* paths, and the exact spelling differs
/// between drive-absolute and UNC paths:
///   * `C:\very\long\path`      -> `\\?\C:\very\long\path`
///   * `\\server\share\long`    -> `\\?\UNC\server\share\long`
///
/// Relative paths are first resolved to absolute (via [`std::path::absolute`]);
/// already-prefixed and short paths are returned unchanged.
#[cfg(windows)]
pub fn long_path_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with(r"\\?\") || s.len() <= 255 {
        return path.to_path_buf();
    }

    // The extended-length prefix requires an absolute path.
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let abs_str = absolute.to_string_lossy();

    if abs_str.starts_with(r"\\?\") {
        absolute.to_path_buf()
    } else if let Some(unc) = abs_str.strip_prefix(r"\\") {
        // UNC: \\server\share\... -> \\?\UNC\server\share\...
        PathBuf::from(format!(r"\\?\UNC\{}", unc))
    } else {
        // Drive-absolute: C:\... -> \\?\C:\...
        PathBuf::from(format!(r"\\?\{}", abs_str))
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

    #[test]
    fn test_sanitize_relative_path_strips_parent_dir() {
        // Unix-style traversal.
        assert_eq!(
            sanitize_relative_path("../../../etc/evil", false).expect("path"),
            "etc/evil"
        );
        // Bare traversal collapses to empty (join keeps it inside the root).
        assert_eq!(sanitize_relative_path("../../..", false).expect("path"), "");
        // Interior `..` is dropped too.
        assert_eq!(
            sanitize_relative_path("a/../../b", false).expect("path"),
            "a/b"
        );
    }

    #[test]
    fn test_sanitize_relative_path_handles_backslashes() {
        // Windows-style traversal must be caught even on non-Windows hosts.
        assert_eq!(
            sanitize_relative_path(r"..\..\etc\evil", false).expect("path"),
            "etc/evil"
        );
        assert_eq!(
            sanitize_relative_path(r"dir\sub\file.txt", false).expect("path"),
            "dir/sub/file.txt"
        );
    }

    #[test]
    fn test_sanitize_relative_path_strips_root_and_drive() {
        assert_eq!(
            sanitize_relative_path("/etc/passwd", false).expect("path"),
            "etc/passwd"
        );
        assert_eq!(
            sanitize_relative_path(r"C:\Windows\system32", false).expect("path"),
            "Windows/system32"
        );
        assert_eq!(
            sanitize_relative_path("C:relative/file", false).expect("path"),
            "relative/file"
        );
    }

    #[test]
    fn test_sanitize_reserved_name_trailing_dot_space() {
        assert_eq!(
            sanitize_reserved_name("foo.", false).expect("trailing dot"),
            "foo._"
        );
        assert_eq!(
            sanitize_reserved_name("bar ", false).expect("trailing space"),
            "bar _"
        );
        assert!(sanitize_reserved_name("foo.", true).is_err());
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

//! Archive entry metadata.
//!
//! This module defines the `Entry` struct that represents a file or directory
//! within an archive, along with its metadata.

use std::path::PathBuf;
use std::time::SystemTime;

/// Compression method used for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionMethod {
    /// No compression (stored).
    #[default]
    Stored,
    /// DEFLATE compression (ZIP, GZIP).
    Deflate,
    /// LZH method lh0 (stored).
    Lh0,
    /// LZH method lh4 (4KB window).
    Lh4,
    /// LZH method lh5 (8KB window).
    Lh5,
    /// LZH method lh6 (32KB window).
    Lh6,
    /// LZH method lh7 (64KB window).
    Lh7,
    /// LZMA compression (7z).
    Lzma,
    /// LZMA2 compression (xz, 7z).
    Lzma2,
    /// Bzip2 compression.
    Bzip2,
    /// Zstandard compression.
    Zstd,
    /// Unknown/unsupported method.
    Unknown(u16),
}

impl CompressionMethod {
    /// Check if this method is "stored" (no compression).
    pub fn is_stored(&self) -> bool {
        matches!(self, Self::Stored | Self::Lh0)
    }

    /// Get the method name as a string.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stored => "Stored",
            Self::Deflate => "Deflate",
            Self::Lh0 => "lh0",
            Self::Lh4 => "lh4",
            Self::Lh5 => "lh5",
            Self::Lh6 => "lh6",
            Self::Lh7 => "lh7",
            Self::Lzma => "LZMA",
            Self::Lzma2 => "LZMA2",
            Self::Bzip2 => "Bzip2",
            Self::Zstd => "Zstd",
            Self::Unknown(_) => "Unknown",
        }
    }
}

impl std::fmt::Display for CompressionMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "Unknown({})", id),
            _ => write!(f, "{}", self.name()),
        }
    }
}

/// Entry type (file, directory, symlink, etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EntryType {
    /// Regular file.
    #[default]
    File,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Hard link.
    Hardlink,
    /// Unknown type.
    Unknown,
}

impl EntryType {
    /// Check if this is a file.
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File)
    }

    /// Check if this is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Check if this is a symlink.
    pub fn is_symlink(&self) -> bool {
        matches!(self, Self::Symlink)
    }
}

/// File attributes/permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileAttributes {
    /// Unix mode bits (rwxrwxrwx).
    pub unix_mode: Option<u32>,
    /// Windows/DOS attributes.
    pub dos_attributes: Option<u8>,
    /// User ID (Unix).
    pub uid: Option<u32>,
    /// Group ID (Unix).
    pub gid: Option<u32>,
}

impl FileAttributes {
    /// Create new empty attributes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set Unix mode.
    pub fn with_mode(mut self, mode: u32) -> Self {
        self.unix_mode = Some(mode);
        self
    }

    /// Set DOS attributes.
    pub fn with_dos(mut self, attrs: u8) -> Self {
        self.dos_attributes = Some(attrs);
        self
    }

    /// Check if the entry is read-only.
    pub fn is_readonly(&self) -> bool {
        if let Some(dos) = self.dos_attributes {
            dos & 0x01 != 0
        } else if let Some(mode) = self.unix_mode {
            mode & 0o222 == 0
        } else {
            false
        }
    }

    /// Check if this is a hidden file.
    pub fn is_hidden(&self) -> bool {
        if let Some(dos) = self.dos_attributes {
            dos & 0x02 != 0
        } else {
            false
        }
    }
}

/// An entry in an archive.
///
/// This represents a single file, directory, or other item within an archive.
/// The struct is format-agnostic and can represent entries from any supported
/// archive format (ZIP, TAR, LZH, etc.).
#[derive(Debug, Clone)]
pub struct Entry {
    /// The name/path of the entry within the archive.
    pub name: String,
    /// The type of entry.
    pub entry_type: EntryType,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Compressed size in bytes.
    pub compressed_size: u64,
    /// Compression method.
    pub method: CompressionMethod,
    /// Last modification time.
    pub modified: Option<SystemTime>,
    /// Creation time (if available).
    pub created: Option<SystemTime>,
    /// Access time (if available).
    pub accessed: Option<SystemTime>,
    /// File attributes.
    pub attributes: FileAttributes,
    /// CRC-32 checksum (if available).
    pub crc32: Option<u32>,
    /// Comment (if available).
    pub comment: Option<String>,
    /// Link target (for symlinks).
    pub link_target: Option<PathBuf>,
    /// Offset in archive (format-specific, for reader use).
    pub offset: u64,
    /// Extra data (format-specific).
    pub extra: Vec<u8>,
}

impl Entry {
    /// Create a new file entry.
    pub fn file(name: impl Into<String>, size: u64) -> Self {
        Self {
            name: name.into(),
            entry_type: EntryType::File,
            size,
            compressed_size: size,
            method: CompressionMethod::Stored,
            modified: None,
            created: None,
            accessed: None,
            attributes: FileAttributes::default(),
            crc32: None,
            comment: None,
            link_target: None,
            offset: 0,
            extra: Vec::new(),
        }
    }

    /// Create a new directory entry.
    pub fn directory(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entry_type: EntryType::Directory,
            size: 0,
            compressed_size: 0,
            method: CompressionMethod::Stored,
            modified: None,
            created: None,
            accessed: None,
            attributes: FileAttributes::default(),
            crc32: None,
            comment: None,
            link_target: None,
            offset: 0,
            extra: Vec::new(),
        }
    }

    /// Check if this is a file.
    pub fn is_file(&self) -> bool {
        self.entry_type.is_file()
    }

    /// Check if this is a directory.
    pub fn is_dir(&self) -> bool {
        self.entry_type.is_dir()
    }

    /// Get the compression ratio (compressed/uncompressed).
    pub fn compression_ratio(&self) -> f64 {
        if self.size == 0 {
            1.0
        } else {
            self.compressed_size as f64 / self.size as f64
        }
    }

    /// Get the space savings as a percentage.
    pub fn space_savings(&self) -> f64 {
        if self.size == 0 {
            0.0
        } else {
            (1.0 - self.compression_ratio()) * 100.0
        }
    }

    /// Builder method to set compression method.
    pub fn with_method(mut self, method: CompressionMethod) -> Self {
        self.method = method;
        self
    }

    /// Builder method to set compressed size.
    pub fn with_compressed_size(mut self, size: u64) -> Self {
        self.compressed_size = size;
        self
    }

    /// Builder method to set modification time.
    pub fn with_modified(mut self, time: SystemTime) -> Self {
        self.modified = Some(time);
        self
    }

    /// Builder method to set CRC-32.
    pub fn with_crc32(mut self, crc: u32) -> Self {
        self.crc32 = Some(crc);
        self
    }

    /// Builder method to set attributes.
    pub fn with_attributes(mut self, attrs: FileAttributes) -> Self {
        self.attributes = attrs;
        self
    }

    /// Builder method to set comment.
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Validate the entry path for security.
    ///
    /// Returns an error if the path contains potentially dangerous components
    /// like ".." (parent directory traversal) or absolute paths.
    pub fn validate_path(&self) -> crate::error::Result<()> {
        use crate::error::OxiArcError;

        let path = std::path::Path::new(&self.name);

        // Check for absolute paths
        if path.is_absolute() {
            return Err(OxiArcError::path_traversal(&self.name));
        }

        // Check for parent directory references
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    return Err(OxiArcError::path_traversal(&self.name));
                }
                std::path::Component::Normal(s) => {
                    // Check for null bytes
                    if s.to_string_lossy().contains('\0') {
                        return Err(OxiArcError::path_traversal(&self.name));
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Get a sanitized path that's safe for extraction.
    ///
    /// This removes dangerous components like ".." and converts absolute
    /// paths to relative ones.
    pub fn sanitized_name(&self) -> String {
        let mut result = String::new();

        for component in std::path::Path::new(&self.name).components() {
            match component {
                std::path::Component::Normal(s) => {
                    if !result.is_empty() && !result.ends_with('/') {
                        result.push('/');
                    }
                    // Remove null bytes
                    result.push_str(&s.to_string_lossy().replace('\0', "_"));
                }
                std::path::Component::CurDir => {
                    // Skip "."
                }
                std::path::Component::ParentDir => {
                    // Skip ".."
                }
                std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                    // Skip absolute path components
                }
            }
        }

        result
    }
}

impl Default for Entry {
    fn default() -> Self {
        Self::file("", 0)
    }
}

impl std::fmt::Display for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let type_char = match self.entry_type {
            EntryType::Directory => 'd',
            EntryType::Symlink => 'l',
            EntryType::Hardlink => 'h',
            _ => '-',
        };
        write!(
            f,
            "{}{:>10} {:>10} {:>6.1}% {}",
            type_char,
            self.size,
            self.compressed_size,
            self.space_savings(),
            self.name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_file() {
        let entry = Entry::file("test.txt", 1000)
            .with_compressed_size(500)
            .with_method(CompressionMethod::Deflate);

        assert!(entry.is_file());
        assert!(!entry.is_dir());
        assert_eq!(entry.size, 1000);
        assert_eq!(entry.compressed_size, 500);
        assert_eq!(entry.compression_ratio(), 0.5);
        assert_eq!(entry.space_savings(), 50.0);
    }

    #[test]
    fn test_entry_directory() {
        let entry = Entry::directory("subdir/");
        assert!(entry.is_dir());
        assert!(!entry.is_file());
    }

    #[test]
    fn test_validate_path_safe() {
        let entry = Entry::file("subdir/file.txt", 100);
        assert!(entry.validate_path().is_ok());
    }

    #[test]
    fn test_validate_path_traversal() {
        let entry = Entry::file("../etc/passwd", 100);
        assert!(entry.validate_path().is_err());

        let entry = Entry::file("subdir/../../etc/passwd", 100);
        assert!(entry.validate_path().is_err());
    }

    #[test]
    fn test_validate_path_absolute() {
        let entry = Entry::file("/etc/passwd", 100);
        assert!(entry.validate_path().is_err());
    }

    #[test]
    fn test_sanitized_name() {
        let entry = Entry::file("../etc/passwd", 100);
        assert_eq!(entry.sanitized_name(), "etc/passwd");

        let entry = Entry::file("/absolute/path/file.txt", 100);
        assert_eq!(entry.sanitized_name(), "absolute/path/file.txt");

        let entry = Entry::file("./current/./path/../file.txt", 100);
        assert_eq!(entry.sanitized_name(), "current/path/file.txt");
    }

    #[test]
    fn test_compression_method_display() {
        assert_eq!(format!("{}", CompressionMethod::Deflate), "Deflate");
        assert_eq!(format!("{}", CompressionMethod::Lh5), "lh5");
        assert_eq!(format!("{}", CompressionMethod::Unknown(99)), "Unknown(99)");
    }

    #[test]
    fn test_file_attributes() {
        let attrs = FileAttributes::new().with_mode(0o755).with_dos(0x01);

        assert!(attrs.is_readonly());
        assert_eq!(attrs.unix_mode, Some(0o755));
    }
}

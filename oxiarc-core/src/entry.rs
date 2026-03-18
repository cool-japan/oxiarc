//! Archive entry metadata.
//!
//! This module defines the `Entry` struct that represents a file or directory
//! within an archive, along with its metadata.

use std::path::PathBuf;
use std::time::SystemTime;

/// Compression method used for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
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
                std::path::Component::Normal(s) if s.to_string_lossy().contains('\0') => {
                    return Err(OxiArcError::path_traversal(&self.name));
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

/// Builder for constructing `Entry` values with method chaining.
///
/// Required fields (`name` and `entry_type`) are set at construction time
/// via [`EntryBuilder::file`], [`EntryBuilder::directory`], or
/// [`EntryBuilder::symlink`]. All other fields are optional and have
/// sensible defaults.
///
/// # Example
///
/// ```
/// use oxiarc_core::entry::{EntryBuilder, CompressionMethod};
///
/// let entry = EntryBuilder::file("readme.txt")
///     .size(1024)
///     .compressed_size(512)
///     .method(CompressionMethod::Deflate)
///     .crc32(0xDEADBEEF)
///     .build();
///
/// assert!(entry.is_file());
/// assert_eq!(entry.size, 1024);
/// ```
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EntryBuilder {
    name: String,
    entry_type: EntryType,
    size: u64,
    compressed_size: u64,
    method: CompressionMethod,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    accessed: Option<SystemTime>,
    attributes: FileAttributes,
    crc32: Option<u32>,
    comment: Option<String>,
    link_target: Option<PathBuf>,
    offset: u64,
    extra: Vec<u8>,
}

impl EntryBuilder {
    /// Create a builder for a file entry.
    pub fn file(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entry_type: EntryType::File,
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

    /// Create a builder for a directory entry.
    pub fn directory(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entry_type: EntryType::Directory,
            ..Self::file("")
        }
    }

    /// Create a builder for a symlink entry.
    pub fn symlink(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entry_type: EntryType::Symlink,
            ..Self::file("")
        }
    }

    /// Create a builder for a hardlink entry.
    pub fn hardlink(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entry_type: EntryType::Hardlink,
            ..Self::file("")
        }
    }

    /// Set the uncompressed size.
    pub fn size(mut self, size: u64) -> Self {
        self.size = size;
        self
    }

    /// Set the compressed size.
    pub fn compressed_size(mut self, size: u64) -> Self {
        self.compressed_size = size;
        self
    }

    /// Set the compression method.
    pub fn method(mut self, method: CompressionMethod) -> Self {
        self.method = method;
        self
    }

    /// Set the last modification time.
    pub fn modified(mut self, time: SystemTime) -> Self {
        self.modified = Some(time);
        self
    }

    /// Set the creation time.
    pub fn created(mut self, time: SystemTime) -> Self {
        self.created = Some(time);
        self
    }

    /// Set the last access time.
    pub fn accessed(mut self, time: SystemTime) -> Self {
        self.accessed = Some(time);
        self
    }

    /// Set the file attributes.
    pub fn attributes(mut self, attributes: FileAttributes) -> Self {
        self.attributes = attributes;
        self
    }

    /// Set the CRC-32 checksum.
    pub fn crc32(mut self, crc: u32) -> Self {
        self.crc32 = Some(crc);
        self
    }

    /// Set the comment.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// Set the link target (for symlinks and hardlinks).
    pub fn link_target(mut self, target: impl Into<PathBuf>) -> Self {
        self.link_target = Some(target.into());
        self
    }

    /// Set the offset in the archive.
    pub fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Set extra data.
    pub fn extra(mut self, extra: Vec<u8>) -> Self {
        self.extra = extra;
        self
    }

    /// Build the `Entry`.
    ///
    /// This always succeeds because the required fields (`name` and
    /// `entry_type`) are provided at builder construction time.
    pub fn build(self) -> Entry {
        Entry {
            name: self.name,
            entry_type: self.entry_type,
            size: self.size,
            compressed_size: self.compressed_size,
            method: self.method,
            modified: self.modified,
            created: self.created,
            accessed: self.accessed,
            attributes: self.attributes,
            crc32: self.crc32,
            comment: self.comment,
            link_target: self.link_target,
            offset: self.offset,
            extra: self.extra,
        }
    }
}

impl Entry {
    /// Create an `EntryBuilder` for a file with the given name.
    ///
    /// This is a convenience shortcut for `EntryBuilder::file(name)`.
    pub fn builder(name: impl Into<String>) -> EntryBuilder {
        EntryBuilder::file(name)
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

    #[test]
    fn test_entry_builder_file() {
        let entry = EntryBuilder::file("test.txt")
            .size(2048)
            .compressed_size(1024)
            .method(CompressionMethod::Deflate)
            .crc32(0xABCD1234)
            .build();

        assert!(entry.is_file());
        assert!(!entry.is_dir());
        assert_eq!(entry.name, "test.txt");
        assert_eq!(entry.size, 2048);
        assert_eq!(entry.compressed_size, 1024);
        assert_eq!(entry.method, CompressionMethod::Deflate);
        assert_eq!(entry.crc32, Some(0xABCD1234));
    }

    #[test]
    fn test_entry_builder_directory() {
        let entry = EntryBuilder::directory("my_dir/").build();

        assert!(entry.is_dir());
        assert!(!entry.is_file());
        assert_eq!(entry.name, "my_dir/");
        assert_eq!(entry.size, 0);
    }

    #[test]
    fn test_entry_builder_symlink() {
        let entry = EntryBuilder::symlink("link.txt")
            .link_target("/usr/bin/target")
            .build();

        assert!(entry.entry_type.is_symlink());
        assert_eq!(entry.name, "link.txt");
        assert_eq!(entry.link_target, Some(PathBuf::from("/usr/bin/target")));
    }

    #[test]
    fn test_entry_builder_all_optional_fields() {
        let now = SystemTime::now();
        let attrs = FileAttributes::new().with_mode(0o644);

        let entry = EntryBuilder::file("full.txt")
            .size(5000)
            .compressed_size(3000)
            .method(CompressionMethod::Lzma)
            .modified(now)
            .created(now)
            .accessed(now)
            .attributes(attrs)
            .crc32(0x12345678)
            .comment("a test comment")
            .link_target("nowhere")
            .offset(4096)
            .extra(vec![0x01, 0x02, 0x03])
            .build();

        assert_eq!(entry.name, "full.txt");
        assert_eq!(entry.size, 5000);
        assert_eq!(entry.compressed_size, 3000);
        assert_eq!(entry.method, CompressionMethod::Lzma);
        assert_eq!(entry.modified, Some(now));
        assert_eq!(entry.created, Some(now));
        assert_eq!(entry.accessed, Some(now));
        assert_eq!(entry.attributes.unix_mode, Some(0o644));
        assert_eq!(entry.crc32, Some(0x12345678));
        assert_eq!(entry.comment, Some("a test comment".to_string()));
        assert_eq!(entry.link_target, Some(PathBuf::from("nowhere")));
        assert_eq!(entry.offset, 4096);
        assert_eq!(entry.extra, vec![0x01, 0x02, 0x03]);
    }

    #[test]
    fn test_entry_builder_convenience_method() {
        let entry = Entry::builder("quick.txt").size(100).build();

        assert!(entry.is_file());
        assert_eq!(entry.name, "quick.txt");
        assert_eq!(entry.size, 100);
    }

    #[test]
    fn test_entry_builder_hardlink() {
        let entry = EntryBuilder::hardlink("hardlink.txt")
            .link_target("/original/file")
            .build();

        assert_eq!(entry.entry_type, EntryType::Hardlink);
        assert_eq!(entry.link_target, Some(PathBuf::from("/original/file")));
    }

    #[test]
    fn test_entry_builder_defaults() {
        let entry = EntryBuilder::file("default.txt").build();

        assert_eq!(entry.size, 0);
        assert_eq!(entry.compressed_size, 0);
        assert_eq!(entry.method, CompressionMethod::Stored);
        assert_eq!(entry.modified, None);
        assert_eq!(entry.created, None);
        assert_eq!(entry.accessed, None);
        assert_eq!(entry.crc32, None);
        assert_eq!(entry.comment, None);
        assert_eq!(entry.link_target, None);
        assert_eq!(entry.offset, 0);
        assert!(entry.extra.is_empty());
    }

    #[cfg(feature = "serde")]
    mod serde_tests {
        use super::*;
        use std::time::{Duration, SystemTime};

        #[test]
        fn test_compression_method_roundtrip() {
            let methods = [
                CompressionMethod::Stored,
                CompressionMethod::Deflate,
                CompressionMethod::Lh0,
                CompressionMethod::Lh4,
                CompressionMethod::Lh5,
                CompressionMethod::Lh6,
                CompressionMethod::Lh7,
                CompressionMethod::Lzma,
                CompressionMethod::Lzma2,
                CompressionMethod::Bzip2,
                CompressionMethod::Zstd,
                CompressionMethod::Unknown(42),
            ];

            for method in &methods {
                let json = serde_json::to_string(method)
                    .unwrap_or_else(|e| panic!("failed to serialize {:?}: {}", method, e));
                let deserialized: CompressionMethod = serde_json::from_str(&json)
                    .unwrap_or_else(|e| panic!("failed to deserialize {:?}: {}", json, e));
                assert_eq!(*method, deserialized);
            }
        }

        #[test]
        fn test_entry_type_roundtrip() {
            let types = [
                EntryType::File,
                EntryType::Directory,
                EntryType::Symlink,
                EntryType::Hardlink,
                EntryType::Unknown,
            ];

            for entry_type in &types {
                let json = serde_json::to_string(entry_type)
                    .unwrap_or_else(|e| panic!("failed to serialize {:?}: {}", entry_type, e));
                let deserialized: EntryType = serde_json::from_str(&json)
                    .unwrap_or_else(|e| panic!("failed to deserialize {:?}: {}", json, e));
                assert_eq!(*entry_type, deserialized);
            }
        }

        #[test]
        fn test_file_attributes_roundtrip() {
            let attrs = FileAttributes {
                unix_mode: Some(0o755),
                dos_attributes: Some(0x20),
                uid: Some(1000),
                gid: Some(1000),
            };

            let json = serde_json::to_string(&attrs)
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            let deserialized: FileAttributes = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize: {}", e));
            assert_eq!(attrs, deserialized);

            // Also test empty attributes
            let empty = FileAttributes::default();
            let json = serde_json::to_string(&empty)
                .unwrap_or_else(|e| panic!("failed to serialize empty: {}", e));
            let deserialized: FileAttributes = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize empty: {}", e));
            assert_eq!(empty, deserialized);
        }

        #[test]
        fn test_entry_roundtrip_minimal() {
            let entry = Entry::file("test.txt", 1024);

            let json = serde_json::to_string(&entry)
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            let deserialized: Entry = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize: {}", e));

            assert_eq!(entry.name, deserialized.name);
            assert_eq!(entry.entry_type, deserialized.entry_type);
            assert_eq!(entry.size, deserialized.size);
            assert_eq!(entry.compressed_size, deserialized.compressed_size);
            assert_eq!(entry.method, deserialized.method);
        }

        #[test]
        fn test_entry_roundtrip_full() {
            let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
            let created = SystemTime::UNIX_EPOCH + Duration::from_secs(1_699_000_000);
            let accessed = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_100_000);

            let entry = Entry {
                name: "subdir/data.bin".to_string(),
                entry_type: EntryType::File,
                size: 8192,
                compressed_size: 4096,
                method: CompressionMethod::Deflate,
                modified: Some(modified),
                created: Some(created),
                accessed: Some(accessed),
                attributes: FileAttributes {
                    unix_mode: Some(0o644),
                    dos_attributes: Some(0x20),
                    uid: Some(1000),
                    gid: Some(100),
                },
                crc32: Some(0xDEAD_BEEF),
                comment: Some("test comment".to_string()),
                link_target: None,
                offset: 512,
                extra: vec![0x01, 0x02, 0x03],
            };

            let json = serde_json::to_string_pretty(&entry)
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            let deserialized: Entry = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize: {}", e));

            assert_eq!(entry.name, deserialized.name);
            assert_eq!(entry.entry_type, deserialized.entry_type);
            assert_eq!(entry.size, deserialized.size);
            assert_eq!(entry.compressed_size, deserialized.compressed_size);
            assert_eq!(entry.method, deserialized.method);
            assert_eq!(entry.modified, deserialized.modified);
            assert_eq!(entry.created, deserialized.created);
            assert_eq!(entry.accessed, deserialized.accessed);
            assert_eq!(entry.attributes, deserialized.attributes);
            assert_eq!(entry.crc32, deserialized.crc32);
            assert_eq!(entry.comment, deserialized.comment);
            assert_eq!(entry.link_target, deserialized.link_target);
            assert_eq!(entry.offset, deserialized.offset);
            assert_eq!(entry.extra, deserialized.extra);
        }

        #[test]
        fn test_entry_directory_roundtrip() {
            let entry = Entry::directory("my_folder/");

            let json = serde_json::to_string(&entry)
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            let deserialized: Entry = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize: {}", e));

            assert_eq!(entry.name, deserialized.name);
            assert_eq!(entry.entry_type, deserialized.entry_type);
            assert!(deserialized.is_dir());
        }

        #[test]
        fn test_entry_with_symlink_roundtrip() {
            let entry = Entry {
                name: "link.txt".to_string(),
                entry_type: EntryType::Symlink,
                size: 0,
                compressed_size: 0,
                method: CompressionMethod::Stored,
                modified: None,
                created: None,
                accessed: None,
                attributes: FileAttributes::default(),
                crc32: None,
                comment: None,
                link_target: Some(std::path::PathBuf::from("target.txt")),
                offset: 0,
                extra: Vec::new(),
            };

            let json = serde_json::to_string(&entry)
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            let deserialized: Entry = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize: {}", e));

            assert_eq!(entry.link_target, deserialized.link_target);
            assert_eq!(entry.entry_type, deserialized.entry_type);
        }

        #[test]
        fn test_entry_builder_serde_roundtrip() {
            let builder = EntryBuilder::file("readme.txt")
                .size(2048)
                .compressed_size(1024)
                .method(CompressionMethod::Lh5)
                .crc32(0xCAFE_BABE);

            let json = serde_json::to_string(&builder)
                .unwrap_or_else(|e| panic!("failed to serialize builder: {}", e));
            let deserialized: EntryBuilder = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("failed to deserialize builder: {}", e));

            // Verify by building both and comparing
            let entry1 = builder.build();
            let entry2 = deserialized.build();
            assert_eq!(entry1.name, entry2.name);
            assert_eq!(entry1.size, entry2.size);
            assert_eq!(entry1.compressed_size, entry2.compressed_size);
            assert_eq!(entry1.method, entry2.method);
            assert_eq!(entry1.crc32, entry2.crc32);
        }

        #[test]
        fn test_compression_method_json_representation() {
            // Verify that the default externally-tagged representation produces readable JSON
            let stored_json = serde_json::to_string(&CompressionMethod::Stored)
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            assert_eq!(stored_json, "\"Stored\"");

            let unknown_json = serde_json::to_string(&CompressionMethod::Unknown(99))
                .unwrap_or_else(|e| panic!("failed to serialize: {}", e));
            assert!(unknown_json.contains("Unknown"));
            assert!(unknown_json.contains("99"));
        }
    }
}

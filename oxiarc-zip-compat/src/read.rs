//! Types for reading ZIP archives.

use std::borrow::Cow;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Take};
use std::marker::PhantomData;
use std::path::{Component, Path, PathBuf};

use oxiarc_archive::zip::{ZipReader, is_entry_encrypted};
use oxiarc_core::Crc32;
use oxiarc_core::entry::{CompressionMethod as CoreMethod, Entry};
use oxiarc_deflate::InflateReader;

use crate::result::{ZipError, ZipResult, from_oxiarc};
use crate::types::{CompressionMethod, DateTime};

/// ZIP archive reader.
///
/// At the moment, this type is cheap to clone if this is the case for the
/// reader it uses. However, this is not guaranteed by this crate and it may
/// change in the future.
pub struct ZipArchive<R: Read + Seek> {
    inner: ZipReader<R>,
}

impl<R: Read + Seek> std::fmt::Debug for ZipArchive<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZipArchive")
            .field("files", &self.len())
            .finish()
    }
}

impl<R: Read + Seek> ZipArchive<R> {
    /// Read a ZIP archive, collecting the files it contains.
    ///
    /// This uses the central directory record of the ZIP file, and ignores
    /// local file headers.
    ///
    /// # Errors
    ///
    /// [`ZipError::InvalidArchive`] when no valid central directory is
    /// found, or an I/O error.
    pub fn new(reader: R) -> ZipResult<ZipArchive<R>> {
        let inner = ZipReader::new(reader).map_err(from_oxiarc)?;
        Ok(ZipArchive { inner })
    }

    /// Number of files contained in this zip.
    pub fn len(&self) -> usize {
        self.inner.entries().len()
    }

    /// Whether this zip archive contains no files.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns an iterator over all the file and directory names in this
    /// archive.
    pub fn file_names(&self) -> impl Iterator<Item = &str> {
        self.inner.entries().iter().map(|e| e.name.as_str())
    }

    /// Get the index of a file entry by name, if it's present.
    pub fn index_for_name(&self, name: &str) -> Option<usize> {
        self.inner.entries().iter().position(|e| e.name == name)
    }

    /// Get the index of a file entry by path, if it's present.
    pub fn index_for_path<T: AsRef<Path>>(&self, path: T) -> Option<usize> {
        let wanted = path_to_zip_name(path.as_ref());
        self.index_for_name(&wanted)
    }

    /// Get the name of a file entry, if it's present.
    pub fn name_for_index(&self, index: usize) -> Option<&str> {
        self.inner.entries().get(index).map(|e| e.name.as_str())
    }

    /// Get the comment of the zip archive (always empty: archive comments
    /// are not surfaced by the underlying reader).
    pub fn comment(&self) -> &[u8] {
        &[]
    }

    /// Search for a file entry by name.
    ///
    /// # Errors
    ///
    /// [`ZipError::FileNotFound`] when absent; see [`ZipArchive::by_index`].
    pub fn by_name(&mut self, name: &str) -> ZipResult<ZipFile<'_, R>> {
        let index = self.index_for_name(name).ok_or(ZipError::FileNotFound)?;
        self.by_index(index)
    }

    /// Search for a file entry by path, decrypting nothing.
    ///
    /// # Errors
    ///
    /// See [`ZipArchive::by_name`].
    pub fn by_path<T: AsRef<Path>>(&mut self, path: T) -> ZipResult<ZipFile<'_, R>> {
        let index = self.index_for_path(path).ok_or(ZipError::FileNotFound)?;
        self.by_index(index)
    }

    /// Get a contained file by index.
    ///
    /// Stored and deflated entries are streamed straight from the
    /// underlying reader (the CRC-32 is verified when the entry has been
    /// read to its end); LZMA entries are decoded up front.
    ///
    /// # Errors
    ///
    /// [`ZipError::FileNotFound`] for an out-of-range index,
    /// [`ZipError::UnsupportedArchive`] for encrypted entries or methods
    /// this implementation cannot decode, or an I/O error.
    pub fn by_index(&mut self, file_number: usize) -> ZipResult<ZipFile<'_, R>> {
        let entry = self
            .inner
            .entries()
            .get(file_number)
            .cloned()
            .ok_or(ZipError::FileNotFound)?;
        if is_entry_encrypted(&entry) {
            return Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED));
        }
        let source = match entry.method {
            CoreMethod::Stored | CoreMethod::Deflate => {
                let reader = self.inner.get_mut();
                reader.seek(SeekFrom::Start(entry.offset))?;
                let limited = (reader as &mut R).take(entry.compressed_size);
                if entry.method == CoreMethod::Stored {
                    Source::Stored(limited)
                } else {
                    Source::Deflated(Box::new(InflateReader::raw(limited)))
                }
            }
            CoreMethod::Lzma => {
                let data = self.inner.extract(&entry).map_err(from_oxiarc)?;
                Source::Buffered(Cursor::new(data))
            }
            _ => {
                return Err(ZipError::UnsupportedArchive(
                    "Compression method not supported",
                ));
            }
        };
        Ok(ZipFile {
            data: entry,
            source,
            crc: Crc32::new(),
            read_total: 0,
            _marker: PhantomData,
        })
    }

    /// Get a contained file by index without decompressing it: reads yield
    /// the raw (compressed) bytes.
    ///
    /// # Errors
    ///
    /// See [`ZipArchive::by_index`].
    pub fn by_index_raw(&mut self, file_number: usize) -> ZipResult<ZipFile<'_, R>> {
        let entry = self
            .inner
            .entries()
            .get(file_number)
            .cloned()
            .ok_or(ZipError::FileNotFound)?;
        let reader = self.inner.get_mut();
        reader.seek(SeekFrom::Start(entry.offset))?;
        let limited = (reader as &mut R).take(entry.compressed_size);
        Ok(ZipFile {
            data: entry,
            source: Source::Raw(limited),
            crc: Crc32::new(),
            read_total: 0,
            _marker: PhantomData,
        })
    }

    /// Extract a Zip archive into a directory, overwriting files if they
    /// already exist. Paths are sanitized with [`ZipFile::enclosed_name`];
    /// entries that would escape `directory` are rejected.
    ///
    /// # Errors
    ///
    /// Any decode or filesystem error.
    pub fn extract<P: AsRef<Path>>(&mut self, directory: P) -> ZipResult<()> {
        for i in 0..self.len() {
            let mut file = self.by_index(i)?;
            let rel = file
                .enclosed_name()
                .ok_or(ZipError::InvalidArchive(Cow::Borrowed("Invalid file path")))?;
            let target = directory.as_ref().join(rel);
            if file.is_dir() {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&target)?;
            io::copy(&mut file, &mut out)?;
            #[cfg(unix)]
            if let Some(mode) = file.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode & 0o7777))?;
            }
        }
        Ok(())
    }

    /// Unwrap and return the inner reader object.
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

fn path_to_zip_name(path: &Path) -> String {
    let mut parts = Vec::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            parts.push(part.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

/// Where a [`ZipFile`]'s bytes come from.
enum Source<'a, R: Read> {
    Stored(Take<&'a mut R>),
    Deflated(Box<InflateReader<Take<&'a mut R>>>),
    Buffered(Cursor<Vec<u8>>),
    Raw(Take<&'a mut R>),
}

/// A struct for reading a zip file.
pub struct ZipFile<'a, R: Read> {
    data: Entry,
    source: Source<'a, R>,
    crc: Crc32,
    read_total: u64,
    _marker: PhantomData<&'a mut R>,
}

impl<R: Read> std::fmt::Debug for ZipFile<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZipFile")
            .field("name", &self.data.name)
            .field("size", &self.data.size)
            .field("compressed_size", &self.data.compressed_size)
            .finish()
    }
}

impl<R: Read> ZipFile<'_, R> {
    /// Get the version of the file (always `(2, 0)`, the DEFLATE-capable
    /// PKZIP version).
    pub fn version_made_by(&self) -> (u8, u8) {
        (2, 0)
    }

    /// Get the name of the file.
    pub fn name(&self) -> &str {
        &self.data.name
    }

    /// Get the name of the file, in the raw (internal) byte representation
    /// (UTF-8 re-encoded).
    pub fn name_raw(&self) -> &[u8] {
        self.data.name.as_bytes()
    }

    /// Get the name of the file in a sanitized form (lossy, may not be
    /// safe for extraction; prefer [`ZipFile::enclosed_name`]).
    pub fn mangled_name(&self) -> PathBuf {
        PathBuf::from(path_to_zip_name(Path::new(&self.data.name)))
    }

    /// Ensure the file path is safe to use as a [`Path`]: no absolute
    /// paths, no `..` escaping the extraction directory.
    pub fn enclosed_name(&self) -> Option<PathBuf> {
        let name = self.data.name.replace('\\', "/");
        if name.contains('\0') || name.starts_with('/') {
            return None;
        }
        let path = PathBuf::from(&name);
        let mut depth = 0usize;
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => return None,
                Component::ParentDir => depth = depth.checked_sub(1)?,
                Component::Normal(_) => depth += 1,
                Component::CurDir => {}
            }
        }
        Some(path)
    }

    /// Get the comment of the file.
    pub fn comment(&self) -> &str {
        self.data.comment.as_deref().unwrap_or("")
    }

    /// Get the compression method used to store the file.
    pub fn compression(&self) -> CompressionMethod {
        match self.data.method {
            CoreMethod::Stored => CompressionMethod::Stored,
            CoreMethod::Deflate => CompressionMethod::Deflated,
            CoreMethod::Lzma => CompressionMethod::Lzma,
            CoreMethod::Bzip2 => CompressionMethod::Bzip2,
            CoreMethod::Zstd => CompressionMethod::Zstd,
            _ => CompressionMethod::parse_from_u16(0xffff),
        }
    }

    /// Get the size of the file, in bytes, in the archive.
    pub fn compressed_size(&self) -> u64 {
        self.data.compressed_size
    }

    /// Get the size of the file, in bytes, when uncompressed.
    pub fn size(&self) -> u64 {
        self.data.size
    }

    /// Get the time the file was last modified.
    pub fn last_modified(&self) -> Option<DateTime> {
        let modified = self.data.modified?;
        let secs = modified
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(DateTime::from_unix_seconds(secs))
    }

    /// Returns whether the file is actually a directory.
    pub fn is_dir(&self) -> bool {
        self.data.is_dir() || self.data.name.ends_with('/')
    }

    /// Returns whether the file is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.data.entry_type == oxiarc_core::entry::EntryType::Symlink
    }

    /// Returns whether the file is a regular file.
    pub fn is_file(&self) -> bool {
        !self.is_dir() && !self.is_symlink()
    }

    /// Get unix mode for the file.
    pub fn unix_mode(&self) -> Option<u32> {
        self.data.attributes.unix_mode
    }

    /// Get the CRC32 hash of the original file.
    pub fn crc32(&self) -> u32 {
        self.data.crc32.unwrap_or(0)
    }

    /// Get the extra data of the zip header for this file.
    pub fn extra_data(&self) -> Option<&[u8]> {
        if self.data.extra.is_empty() {
            None
        } else {
            Some(&self.data.extra)
        }
    }

    /// Get the starting offset of the data of the compressed file.
    pub fn data_start(&self) -> u64 {
        self.data.offset
    }
}

impl<R: Read> Read for ZipFile<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = match &mut self.source {
            Source::Stored(r) => r.read(buf)?,
            Source::Deflated(r) => r.read(buf)?,
            Source::Buffered(r) => return r.read(buf),
            Source::Raw(r) => return r.read(buf),
        };
        self.crc.update(&buf[..n]);
        self.read_total += n as u64;
        if n == 0 && !buf.is_empty() {
            if self.read_total != self.data.size {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "zip entry shorter than its recorded size",
                ));
            }
            if let Some(expected) = self.data.crc32 {
                if self.crc.value() != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Invalid checksum",
                    ));
                }
            }
        }
        Ok(n)
    }
}

/// Methods for retrieving information on zip files (shared by read-side
/// file types in the zip crate).
pub trait HasZipMetadata {
    /// Get the name of the file.
    fn file_name(&self) -> &str;
    /// Get the size of the file, in bytes, when uncompressed.
    fn file_size(&self) -> u64;
}

impl<R: Read> HasZipMetadata for ZipFile<'_, R> {
    fn file_name(&self) -> &str {
        self.name()
    }
    fn file_size(&self) -> u64 {
        self.size()
    }
}

/// Options for reading a file from an archive (accepted for signature
/// compatibility; passwords are not supported).
#[derive(Default, Debug, Clone)]
pub struct ZipReadOptions<'a> {
    _password: Option<&'a [u8]>,
}

impl<'a> ZipReadOptions<'a> {
    /// Create default options.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the password (encrypted entries are still rejected).
    pub fn password(mut self, password: Option<&'a [u8]>) -> Self {
        self._password = password;
        self
    }
}

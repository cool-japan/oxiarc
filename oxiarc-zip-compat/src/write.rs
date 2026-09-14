//! Types for creating ZIP archives.

use std::io::{self, Seek, Write};
use std::marker::PhantomData;
use std::sync::Arc;

use oxiarc_archive::zip::{ZipCompressionLevel, ZipWriter as OxiZipWriter};

use crate::result::{ZipError, ZipResult, from_oxiarc};
use crate::types::{CompressionMethod, DateTime, System};

mod sealed {
    pub trait Sealed {}
}

/// Extension point of [`FileOptions`]: `()` for simple options,
/// [`ExtendedFileOptions`] for extra-field / comment support.
pub trait FileOptionExtension: Default + sealed::Sealed {
    /// Extra data for the local header.
    fn extra_data(&self) -> Option<&Arc<Vec<u8>>>;
    /// Extra data for the central directory.
    fn central_extra_data(&self) -> Option<&Arc<Vec<u8>>>;
    /// File comment.
    fn file_comment(&self) -> Option<&str>;
    /// Take the file comment.
    fn take_file_comment(&mut self) -> Option<Box<str>>;
}

impl sealed::Sealed for () {}
impl FileOptionExtension for () {
    fn extra_data(&self) -> Option<&Arc<Vec<u8>>> {
        None
    }
    fn central_extra_data(&self) -> Option<&Arc<Vec<u8>>> {
        None
    }
    fn file_comment(&self) -> Option<&str> {
        None
    }
    fn take_file_comment(&mut self) -> Option<Box<str>> {
        None
    }
}

/// Adds Extra Data and Central Extra Data. Accepted for API compatibility;
/// the extra fields and comments are not written by this implementation.
#[derive(Clone, Default, Debug)]
pub struct ExtendedFileOptions {
    extra_data: Arc<Vec<u8>>,
    central_extra_data: Arc<Vec<u8>>,
    file_comment: Option<Box<str>>,
}

impl sealed::Sealed for ExtendedFileOptions {}
impl FileOptionExtension for ExtendedFileOptions {
    fn extra_data(&self) -> Option<&Arc<Vec<u8>>> {
        Some(&self.extra_data)
    }
    fn central_extra_data(&self) -> Option<&Arc<Vec<u8>>> {
        Some(&self.central_extra_data)
    }
    fn file_comment(&self) -> Option<&str> {
        self.file_comment.as_deref()
    }
    fn take_file_comment(&mut self) -> Option<Box<str>> {
        self.file_comment.take()
    }
}

/// Metadata for a file to be written.
#[derive(Clone, Debug, Copy, Eq, PartialEq)]
pub struct FileOptions<'k, T: FileOptionExtension> {
    pub(crate) compression_method: CompressionMethod,
    pub(crate) compression_level: Option<i64>,
    pub(crate) last_modified_time: DateTime,
    pub(crate) permissions: Option<u32>,
    pub(crate) large_file: bool,
    pub(crate) extended_options: T,
    pub(crate) system: Option<System>,
    _lifetime: PhantomData<&'k ()>,
}

/// Simple File Options. Can be copied and good for simple writing zip
/// files.
pub type SimpleFileOptions = FileOptions<'static, ()>;

/// Adds Extra Data and Central Extra Data. It does not implement copy.
pub type FullFileOptions<'k> = FileOptions<'k, ExtendedFileOptions>;

impl<T: FileOptionExtension> FileOptions<'_, T> {
    /// Indicates whether this file will be encrypted (never, here).
    pub const fn has_encryption(&self) -> bool {
        false
    }

    /// Set the compression method for the new file.
    ///
    /// The default is `CompressionMethod::Deflated`.
    #[must_use]
    pub const fn compression_method(mut self, method: CompressionMethod) -> Self {
        self.compression_method = method;
        self
    }

    /// Sets the system (OS) that created the file.
    #[must_use]
    pub const fn system(mut self, system: System) -> Self {
        self.system = Some(system);
        self
    }

    /// Set the compression level for the new file (`Some(0..=9)` for
    /// Deflated, `None` for the default).
    #[must_use]
    pub const fn compression_level(mut self, level: Option<i64>) -> Self {
        self.compression_level = level;
        self
    }

    /// Set the last modified time.
    #[must_use]
    pub const fn last_modified_time(mut self, mod_time: DateTime) -> Self {
        self.last_modified_time = mod_time;
        self
    }

    /// Set the Unix permissions for the new file (masked to `0o777`).
    #[must_use]
    pub const fn unix_permissions(mut self, mode: u32) -> Self {
        self.permissions = Some(mode & 0o777);
        self
    }

    /// Set whether the new file's compressed and uncompressed size is
    /// less than 4 GiB (Zip64 is chosen automatically either way).
    #[must_use]
    pub const fn large_file(mut self, large: bool) -> Self {
        self.large_file = large;
        self
    }
}

impl FileOptions<'_, ExtendedFileOptions> {
    /// Set the file comment (accepted, not written).
    #[must_use]
    pub fn with_file_comment<S: Into<Box<str>>>(mut self, comment: S) -> Self {
        self.extended_options.file_comment = Some(comment.into());
        self
    }

    /// Adds an extra data field (accepted, not written).
    ///
    /// # Errors
    ///
    /// Never.
    pub fn add_extra_data<D: AsRef<[u8]>>(
        &mut self,
        header_id: u16,
        data: D,
        central_only: bool,
    ) -> ZipResult<()> {
        let mut field = header_id.to_le_bytes().to_vec();
        field.extend_from_slice(&(data.as_ref().len() as u16).to_le_bytes());
        field.extend_from_slice(data.as_ref());
        let target = if central_only {
            &mut self.extended_options.central_extra_data
        } else {
            &mut self.extended_options.extra_data
        };
        Arc::make_mut(target).extend_from_slice(&field);
        Ok(())
    }

    /// Removes the extra data fields.
    #[must_use]
    pub fn clear_extra_data(mut self) -> Self {
        self.extended_options.extra_data = Arc::new(Vec::new());
        self.extended_options.central_extra_data = Arc::new(Vec::new());
        self
    }
}

impl FileOptions<'static, ()> {
    /// The default options (Deflated, 1980-01-01 timestamp).
    pub const DEFAULT: Self = Self {
        compression_method: CompressionMethod::DEFAULT,
        compression_level: None,
        last_modified_time: DateTime::DEFAULT,
        permissions: None,
        large_file: false,
        extended_options: (),
        system: None,
        _lifetime: PhantomData,
    };
}

impl<'k> FileOptions<'k, ()> {
    /// Convert to full options.
    #[must_use]
    pub fn into_full_options(self) -> FullFileOptions<'k> {
        FileOptions {
            compression_method: self.compression_method,
            compression_level: self.compression_level,
            last_modified_time: self.last_modified_time,
            permissions: self.permissions,
            large_file: self.large_file,
            extended_options: ExtendedFileOptions::default(),
            system: self.system,
            _lifetime: PhantomData,
        }
    }
}

impl<T: FileOptionExtension> Default for FileOptions<'_, T> {
    /// Construct a new FileOptions object (Deflated, modified now).
    fn default() -> Self {
        Self {
            compression_method: CompressionMethod::default(),
            compression_level: None,
            last_modified_time: DateTime::default_for_write(),
            permissions: None,
            large_file: false,
            extended_options: T::default(),
            system: None,
            _lifetime: PhantomData,
        }
    }
}

/// An entry being written: the data is buffered until the next
/// `start_file` / `finish` / drop, then emitted with its sizes and CRC in
/// the local header (no data descriptor needed).
struct PendingFile {
    name: String,
    data: Vec<u8>,
    method: CompressionMethod,
    level: ZipCompressionLevel,
    dos: (u16, u16),
    mode: Option<u32>,
}

/// ZIP archive generator.
///
/// Handles the bookkeeping involved in building an archive, and provides an
/// API to edit its contents. The archive is finalized (central directory
/// written) by [`ZipWriter::finish`] or on drop.
pub struct ZipWriter<W: Write + Seek> {
    inner: Option<OxiZipWriter<W>>,
    pending: Option<PendingFile>,
    names: std::collections::HashSet<String>,
}

impl<W: Write + Seek> std::fmt::Debug for ZipWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZipWriter")
            .field("pending", &self.pending.as_ref().map(|p| p.name.as_str()))
            .finish()
    }
}

fn level_for(method: CompressionMethod, level: Option<i64>) -> ZipResult<ZipCompressionLevel> {
    match method {
        CompressionMethod::Stored => Ok(ZipCompressionLevel::Store),
        CompressionMethod::Deflated => Ok(match level {
            None => ZipCompressionLevel::Normal,
            Some(0) => ZipCompressionLevel::Store,
            Some(1..=3) => ZipCompressionLevel::Fast,
            Some(4..=8) => ZipCompressionLevel::Normal,
            Some(9..=264) => ZipCompressionLevel::Best,
            Some(_) => {
                return Err(ZipError::UnsupportedArchive(
                    "Unsupported compression level",
                ));
            }
        }),
        CompressionMethod::Lzma => Ok(ZipCompressionLevel::Normal),
        _ => Err(ZipError::UnsupportedArchive(
            "Compression method not supported",
        )),
    }
}

impl<W: Write + Seek> ZipWriter<W> {
    /// Initializes the archive.
    ///
    /// Before writing to this object, the [`ZipWriter::start_file`]
    /// function should be called. After a successful write, the file
    /// remains open for writing. After a failed write, call
    /// [`ZipWriter::start_file`] or [`ZipWriter::finish`].
    pub fn new(inner: W) -> ZipWriter<W> {
        ZipWriter {
            inner: Some(OxiZipWriter::new(inner)),
            pending: None,
            names: std::collections::HashSet::new(),
        }
    }

    fn writer(&mut self) -> ZipResult<&mut OxiZipWriter<W>> {
        self.inner.as_mut().ok_or(ZipError::UnsupportedArchive(
            "ZipWriter was already finished",
        ))
    }

    /// Emit the entry currently being written, if any.
    fn finish_file(&mut self) -> ZipResult<()> {
        let Some(file) = self.pending.take() else {
            return Ok(());
        };
        let writer = self.writer()?;
        let result = if file.method == CompressionMethod::Lzma {
            writer.add_file_lzma(&file.name, &file.data)
        } else {
            writer.add_file_with_attributes(
                &file.name,
                &file.data,
                file.level,
                Some(file.dos),
                file.mode,
            )
        };
        result.map_err(from_oxiarc)
    }

    /// Create a file in the archive and start writing its contents. The
    /// file must not have the same name as a file already in the archive.
    ///
    /// # Errors
    ///
    /// An unsupported compression method or level, a duplicate name, or an
    /// error emitting the previous file.
    pub fn start_file<S: ToString, T: FileOptionExtension>(
        &mut self,
        name: S,
        options: FileOptions<'_, T>,
    ) -> ZipResult<()> {
        self.finish_file()?;
        let name = name.to_string();
        if !self.names.insert(name.clone()) {
            return Err(ZipError::InvalidArchive(std::borrow::Cow::Owned(format!(
                "Duplicate filename: {name}"
            ))));
        }
        let level = level_for(options.compression_method, options.compression_level)?;
        let dt = options.last_modified_time;
        self.pending = Some(PendingFile {
            name,
            data: Vec::new(),
            method: options.compression_method,
            level,
            dos: (dt.timepart(), dt.datepart()),
            mode: options.permissions,
        });
        Ok(())
    }

    /// Create a file in the archive from a path.
    ///
    /// # Errors
    ///
    /// See [`ZipWriter::start_file`].
    pub fn start_file_from_path<E: FileOptionExtension, P: AsRef<std::path::Path>>(
        &mut self,
        path: P,
        options: FileOptions<'_, E>,
    ) -> ZipResult<()> {
        let name = path
            .as_ref()
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
        self.start_file(name, options)
    }

    /// Add a directory entry.
    ///
    /// # Errors
    ///
    /// An error emitting the previous file or this entry.
    pub fn add_directory<S: Into<String>, T: FileOptionExtension>(
        &mut self,
        name: S,
        _options: FileOptions<'_, T>,
    ) -> ZipResult<()> {
        self.finish_file()?;
        let mut name: String = name.into();
        if !name.ends_with('/') {
            name.push('/');
        }
        self.writer()?.add_directory(&name).map_err(from_oxiarc)
    }

    /// Finish the last file and write all other zip-structures.
    ///
    /// This will return the writer, but one should normally not append any
    /// data to the end of the file.
    ///
    /// # Errors
    ///
    /// Any error writing the pending file or the central directory.
    pub fn finish(mut self) -> ZipResult<W> {
        self.finish_file()?;
        let writer = self.inner.take().ok_or(ZipError::UnsupportedArchive(
            "ZipWriter was already finished",
        ))?;
        writer.into_inner().map_err(from_oxiarc)
    }

    /// Returns whether a file is currently open for writing.
    pub fn is_writing_file(&self) -> bool {
        self.pending.is_some()
    }

    /// Abort the file currently being written, discarding its data.
    ///
    /// # Errors
    ///
    /// Never.
    pub fn abort_file(&mut self) -> ZipResult<()> {
        self.pending = None;
        Ok(())
    }
}

impl<W: Write + Seek> Write for ZipWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.pending.as_mut() {
            Some(file) => {
                file.data.extend_from_slice(buf);
                Ok(buf.len())
            }
            None => Err(io::Error::other("No file has been started")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<W: Write + Seek> Drop for ZipWriter<W> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            // Errors can't be reported from drop; the inner writer then
            // writes the central directory in its own Drop.
            let _ = self.finish_file();
        }
    }
}

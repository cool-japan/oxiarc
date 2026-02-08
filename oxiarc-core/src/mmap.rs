//! Memory-mapped file support for OxiArc.
//!
//! This module provides memory-mapped file access for efficient reading of large
//! archive files. Memory mapping allows the operating system to handle file I/O
//! through virtual memory, which can be more efficient for large files and random
//! access patterns.
//!
//! # Features
//!
//! - [`MmapReader`]: A memory-mapped file reader implementing [`std::io::Read`]
//! - Zero-copy access to file contents
//! - Automatic memory management by the OS
//!
//! # Example
//!
//! ```no_run
//! use oxiarc_core::mmap::MmapReader;
//! use std::io::Read;
//!
//! let mut reader = MmapReader::open("archive.zip").unwrap();
//! let mut buffer = [0u8; 1024];
//! let bytes_read = reader.read(&mut buffer).unwrap();
//! ```
//!
//! # Safety
//!
//! Memory-mapped files can be dangerous if the underlying file is modified by
//! another process while mapped. This implementation uses read-only mappings
//! to minimize risks.
//!
//! # Performance Considerations
//!
//! Memory mapping is typically faster for:
//! - Large files where the OS can efficiently page in data
//! - Random access patterns
//! - Multiple reads of the same data
//!
//! Regular file I/O may be faster for:
//! - Small files
//! - Sequential reads where buffering is more important
//! - Systems with limited virtual address space

use crate::error::Result;
use memmap2::Mmap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

/// A memory-mapped file reader.
///
/// This struct wraps a memory-mapped file and provides a [`Read`] interface
/// for sequential access to the file contents. It also supports seeking via
/// the [`Seek`] trait.
///
/// # Thread Safety
///
/// The underlying memory map is wrapped in an [`Arc`], making it safe to
/// clone and share between threads. Each clone maintains its own read position.
///
/// # Example
///
/// ```no_run
/// use oxiarc_core::mmap::MmapReader;
/// use std::io::{Read, Seek, SeekFrom};
///
/// let mut reader = MmapReader::open("archive.zip").unwrap();
///
/// // Read first 4 bytes (e.g., magic number)
/// let mut magic = [0u8; 4];
/// reader.read_exact(&mut magic).unwrap();
///
/// // Seek to a specific position
/// reader.seek(SeekFrom::Start(100)).unwrap();
///
/// // Read more data
/// let mut buffer = vec![0u8; 256];
/// let bytes_read = reader.read(&mut buffer).unwrap();
/// ```
#[derive(Debug)]
pub struct MmapReader {
    /// The memory-mapped file data.
    mmap: Arc<Mmap>,
    /// Current read position.
    position: usize,
}

impl MmapReader {
    /// Open a file and create a memory-mapped reader.
    ///
    /// This function opens the file at the specified path and creates a
    /// read-only memory mapping of its contents.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to open
    ///
    /// # Returns
    ///
    /// Returns a new `MmapReader` on success, or an error if the file
    /// cannot be opened or mapped.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::OxiArcError::Io`] if:
    /// - The file does not exist
    /// - The file cannot be opened
    /// - Memory mapping fails
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    ///
    /// let reader = MmapReader::open("archive.zip")?;
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        Self::from_file(&file)
    }

    /// Create a memory-mapped reader from an open file.
    ///
    /// This function creates a memory-mapped reader from an already-open
    /// [`File`] handle. The file must be opened for reading.
    ///
    /// # Arguments
    ///
    /// * `file` - A reference to an open file
    ///
    /// # Returns
    ///
    /// Returns a new `MmapReader` on success.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::OxiArcError::Io`] if memory mapping fails.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the file is not modified while the
    /// memory mapping is active.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    /// use std::fs::File;
    ///
    /// let file = File::open("archive.zip")?;
    /// let reader = MmapReader::from_file(&file)?;
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    pub fn from_file(file: &File) -> Result<Self> {
        // SAFETY: We create a read-only mapping, and the caller is responsible
        // for ensuring the file is not modified while mapped.
        let mmap = unsafe { Mmap::map(file)? };
        Ok(Self {
            mmap: Arc::new(mmap),
            position: 0,
        })
    }

    /// Get the total length of the mapped file.
    ///
    /// # Returns
    ///
    /// The total size of the file in bytes.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    ///
    /// let reader = MmapReader::open("archive.zip")?;
    /// println!("File size: {} bytes", reader.len());
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    /// Check if the mapped file is empty.
    ///
    /// # Returns
    ///
    /// `true` if the file has zero length, `false` otherwise.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    ///
    /// let reader = MmapReader::open("archive.zip")?;
    /// if reader.is_empty() {
    ///     println!("File is empty!");
    /// }
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    /// Get the current read position.
    ///
    /// # Returns
    ///
    /// The current offset from the beginning of the file.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    /// use std::io::Read;
    ///
    /// let mut reader = MmapReader::open("archive.zip")?;
    /// let mut buffer = [0u8; 100];
    /// reader.read(&mut buffer)?;
    /// assert_eq!(reader.position(), 100);
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn position(&self) -> usize {
        self.position
    }

    /// Get the remaining bytes available for reading.
    ///
    /// # Returns
    ///
    /// The number of bytes from the current position to the end of the file.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    ///
    /// let reader = MmapReader::open("archive.zip")?;
    /// println!("Remaining: {} bytes", reader.remaining());
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn remaining(&self) -> usize {
        self.len().saturating_sub(self.position)
    }

    /// Get a slice of the underlying memory-mapped data.
    ///
    /// This provides direct, zero-copy access to the file contents.
    ///
    /// # Returns
    ///
    /// A byte slice representing the entire file contents.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    ///
    /// let reader = MmapReader::open("archive.zip")?;
    /// let data = reader.as_slice();
    /// println!("First byte: {:#x}", data[0]);
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap
    }

    /// Get a slice of the remaining unread data.
    ///
    /// This provides direct access to the data from the current position
    /// to the end of the file.
    ///
    /// # Returns
    ///
    /// A byte slice of the remaining data.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    /// use std::io::{Read, Seek, SeekFrom};
    ///
    /// let mut reader = MmapReader::open("archive.zip")?;
    /// reader.seek(SeekFrom::Start(100))?;
    /// let remaining = reader.remaining_slice();
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn remaining_slice(&self) -> &[u8] {
        if self.position >= self.len() {
            &[]
        } else {
            &self.mmap[self.position..]
        }
    }

    /// Reset the read position to the beginning.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    /// use std::io::Read;
    ///
    /// let mut reader = MmapReader::open("archive.zip")?;
    /// let mut buffer = [0u8; 100];
    /// reader.read(&mut buffer)?;
    /// reader.reset();
    /// assert_eq!(reader.position(), 0);
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    #[inline]
    pub fn reset(&mut self) {
        self.position = 0;
    }

    /// Create a clone of this reader with an independent position.
    ///
    /// The clone shares the underlying memory mapping but has its own
    /// read position, initially set to the beginning of the file.
    ///
    /// # Returns
    ///
    /// A new `MmapReader` sharing the same memory mapping.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    /// use std::io::{Read, Seek, SeekFrom};
    ///
    /// let mut reader1 = MmapReader::open("archive.zip")?;
    /// reader1.seek(SeekFrom::Start(100))?;
    ///
    /// // Clone starts at position 0
    /// let reader2 = reader1.clone_with_reset_position();
    /// assert_eq!(reader2.position(), 0);
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    pub fn clone_with_reset_position(&self) -> Self {
        Self {
            mmap: Arc::clone(&self.mmap),
            position: 0,
        }
    }

    /// Create a clone of this reader preserving the current position.
    ///
    /// The clone shares the underlying memory mapping and has the same
    /// read position as the original.
    ///
    /// # Returns
    ///
    /// A new `MmapReader` sharing the same memory mapping and position.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use oxiarc_core::mmap::MmapReader;
    /// use std::io::{Read, Seek, SeekFrom};
    ///
    /// let mut reader1 = MmapReader::open("archive.zip")?;
    /// reader1.seek(SeekFrom::Start(100))?;
    ///
    /// // Clone preserves position
    /// let reader2 = reader1.clone_with_position();
    /// assert_eq!(reader2.position(), 100);
    /// # Ok::<(), oxiarc_core::error::OxiArcError>(())
    /// ```
    pub fn clone_with_position(&self) -> Self {
        Self {
            mmap: Arc::clone(&self.mmap),
            position: self.position,
        }
    }
}

impl Clone for MmapReader {
    fn clone(&self) -> Self {
        self.clone_with_position()
    }
}

impl Read for MmapReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.len() {
            return Ok(0);
        }

        let available = self.remaining();
        let to_read = buf.len().min(available);
        buf[..to_read].copy_from_slice(&self.mmap[self.position..self.position + to_read]);
        self.position += to_read;
        Ok(to_read)
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        if buf.len() > self.remaining() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "read_exact: requested {} bytes but only {} available",
                    buf.len(),
                    self.remaining()
                ),
            ));
        }
        buf.copy_from_slice(&self.mmap[self.position..self.position + buf.len()]);
        self.position += buf.len();
        Ok(())
    }
}

impl Seek for MmapReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let new_pos = match pos {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::End(offset) => self.len() as i64 + offset,
            SeekFrom::Current(offset) => self.position as i64 + offset,
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to negative position",
            ));
        }

        let new_pos = new_pos as usize;
        // Allow seeking past end (consistent with std::io::Cursor behavior)
        self.position = new_pos;
        Ok(new_pos as u64)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position as u64)
    }
}

/// Options for creating a memory-mapped reader.
///
/// This struct provides a builder pattern for configuring memory-mapped
/// file access with additional options.
///
/// # Example
///
/// ```no_run
/// use oxiarc_core::mmap::MmapOptions;
///
/// let reader = MmapOptions::new()
///     .populate(true)
///     .open("archive.zip")?;
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
#[derive(Debug, Default, Clone)]
pub struct MmapOptions {
    /// Whether to populate (prefault) the memory mapping.
    populate: bool,
}

impl MmapOptions {
    /// Create a new `MmapOptions` with default settings.
    ///
    /// # Returns
    ///
    /// A new `MmapOptions` instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set whether to populate the memory mapping.
    ///
    /// When enabled, the operating system will attempt to prefault the
    /// memory mapping, reading the file contents into memory immediately.
    /// This can improve performance for files that will be read entirely,
    /// but increases initial memory usage.
    ///
    /// # Arguments
    ///
    /// * `populate` - Whether to populate the mapping
    ///
    /// # Returns
    ///
    /// Self for method chaining.
    pub fn populate(mut self, populate: bool) -> Self {
        self.populate = populate;
        self
    }

    /// Open a file with the configured options.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the file to open
    ///
    /// # Returns
    ///
    /// Returns a new `MmapReader` on success.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::OxiArcError::Io`] if the file cannot be opened or mapped.
    pub fn open<P: AsRef<Path>>(self, path: P) -> Result<MmapReader> {
        let file = File::open(path.as_ref())?;
        self.open_file(&file)
    }

    /// Open a file handle with the configured options.
    ///
    /// # Arguments
    ///
    /// * `file` - A reference to an open file
    ///
    /// # Returns
    ///
    /// Returns a new `MmapReader` on success.
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::OxiArcError::Io`] if memory mapping fails.
    pub fn open_file(self, file: &File) -> Result<MmapReader> {
        let mmap = if self.populate {
            // SAFETY: Read-only mapping, caller responsible for file stability
            unsafe { memmap2::MmapOptions::new().populate().map(file)? }
        } else {
            // SAFETY: Read-only mapping, caller responsible for file stability
            unsafe { Mmap::map(file)? }
        };

        Ok(MmapReader {
            mmap: Arc::new(mmap),
            position: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OxiArcError;
    use std::io::Write;

    /// Create a temporary file with the given contents and return its path.
    fn create_temp_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir();
        let path = temp_dir.join(format!("oxiarc_mmap_test_{}", name));
        let mut file = File::create(&path).expect("Failed to create temp file");
        file.write_all(contents)
            .expect("Failed to write to temp file");
        file.sync_all().expect("Failed to sync temp file");
        path
    }

    /// Remove a temporary file.
    fn remove_temp_file(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_open_and_read() {
        let contents = b"Hello, memory-mapped world!";
        let path = create_temp_file("read_test", contents);

        let result = MmapReader::open(&path);
        assert!(result.is_ok(), "Failed to open file: {:?}", result);

        let mut reader = result.expect("Reader creation failed");
        let mut buffer = vec![0u8; contents.len()];
        let bytes_read = reader.read(&mut buffer).expect("Read failed");

        assert_eq!(bytes_read, contents.len());
        assert_eq!(&buffer, contents);

        remove_temp_file(&path);
    }

    #[test]
    fn test_empty_file() {
        let path = create_temp_file("empty_test", b"");

        let result = MmapReader::open(&path);
        assert!(result.is_ok());

        let mut reader = result.expect("Reader creation failed");
        assert!(reader.is_empty());
        assert_eq!(reader.len(), 0);
        assert_eq!(reader.remaining(), 0);

        let mut buffer = [0u8; 10];
        let bytes_read = reader.read(&mut buffer).expect("Read failed");
        assert_eq!(bytes_read, 0);

        remove_temp_file(&path);
    }

    #[test]
    fn test_large_file() {
        // Create a 1MB file
        let size = 1024 * 1024;
        let mut contents = vec![0u8; size];
        for (i, byte) in contents.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        let path = create_temp_file("large_test", &contents);

        let result = MmapReader::open(&path);
        assert!(result.is_ok());

        let mut reader = result.expect("Reader creation failed");
        assert_eq!(reader.len(), size);

        // Read in chunks
        let mut buffer = vec![0u8; size];
        let bytes_read = reader.read(&mut buffer).expect("Read failed");
        assert_eq!(bytes_read, size);
        assert_eq!(buffer, contents);

        remove_temp_file(&path);
    }

    #[test]
    fn test_compare_with_regular_read() {
        let contents = b"Testing comparison between mmap and regular file read";
        let path = create_temp_file("compare_test", contents);

        // Read with mmap
        let mut mmap_reader = MmapReader::open(&path).expect("Mmap open failed");
        let mut mmap_buffer = vec![0u8; contents.len()];
        mmap_reader
            .read_exact(&mut mmap_buffer)
            .expect("Mmap read failed");

        // Read with regular file
        let mut regular_file = File::open(&path).expect("File open failed");
        let mut regular_buffer = vec![0u8; contents.len()];
        regular_file
            .read_exact(&mut regular_buffer)
            .expect("Regular read failed");

        // Compare results
        assert_eq!(mmap_buffer, regular_buffer);
        assert_eq!(mmap_buffer, contents.to_vec());

        remove_temp_file(&path);
    }

    #[test]
    fn test_seek() {
        let contents = b"0123456789ABCDEF";
        let path = create_temp_file("seek_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");

        // Seek to middle
        let pos = reader.seek(SeekFrom::Start(8)).expect("Seek failed");
        assert_eq!(pos, 8);
        assert_eq!(reader.position(), 8);

        let mut buffer = [0u8; 4];
        reader.read_exact(&mut buffer).expect("Read failed");
        assert_eq!(&buffer, b"89AB");

        // Seek from current
        reader.seek(SeekFrom::Current(-2)).expect("Seek failed");
        reader.read_exact(&mut buffer).expect("Read failed");
        assert_eq!(&buffer, b"ABCD");

        // Seek from end
        reader.seek(SeekFrom::End(-4)).expect("Seek failed");
        reader.read_exact(&mut buffer).expect("Read failed");
        assert_eq!(&buffer, b"CDEF");

        remove_temp_file(&path);
    }

    #[test]
    fn test_seek_negative_position() {
        let contents = b"Test data";
        let path = create_temp_file("seek_neg_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");
        let result = reader.seek(SeekFrom::Current(-1));

        assert!(result.is_err());

        remove_temp_file(&path);
    }

    #[test]
    fn test_remaining() {
        let contents = b"ABCDEFGHIJ";
        let path = create_temp_file("remaining_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");

        assert_eq!(reader.remaining(), 10);

        let mut buffer = [0u8; 3];
        reader.read_exact(&mut buffer).expect("Read failed");

        assert_eq!(reader.remaining(), 7);
        assert_eq!(reader.position(), 3);

        remove_temp_file(&path);
    }

    #[test]
    fn test_reset() {
        let contents = b"Reset test data";
        let path = create_temp_file("reset_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");

        // Read some data
        let mut buffer = [0u8; 5];
        reader.read_exact(&mut buffer).expect("Read failed");
        assert_eq!(reader.position(), 5);

        // Reset
        reader.reset();
        assert_eq!(reader.position(), 0);

        // Read again from beginning
        reader.read_exact(&mut buffer).expect("Read failed");
        assert_eq!(&buffer, b"Reset");

        remove_temp_file(&path);
    }

    #[test]
    fn test_clone_with_reset_position() {
        let contents = b"Clone test data";
        let path = create_temp_file("clone_reset_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");
        reader.seek(SeekFrom::Start(6)).expect("Seek failed");

        let cloned = reader.clone_with_reset_position();
        assert_eq!(cloned.position(), 0);
        assert_eq!(reader.position(), 6);

        remove_temp_file(&path);
    }

    #[test]
    fn test_clone_with_position() {
        let contents = b"Clone test data";
        let path = create_temp_file("clone_pos_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");
        reader.seek(SeekFrom::Start(6)).expect("Seek failed");

        let cloned = reader.clone_with_position();
        assert_eq!(cloned.position(), 6);
        assert_eq!(reader.position(), 6);

        remove_temp_file(&path);
    }

    #[test]
    fn test_as_slice() {
        let contents = b"Slice access test";
        let path = create_temp_file("slice_test", contents);

        let reader = MmapReader::open(&path).expect("Open failed");
        let slice = reader.as_slice();

        assert_eq!(slice, contents);

        remove_temp_file(&path);
    }

    #[test]
    fn test_remaining_slice() {
        let contents = b"Remaining slice";
        let path = create_temp_file("remaining_slice_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");
        reader.seek(SeekFrom::Start(10)).expect("Seek failed");

        let remaining = reader.remaining_slice();
        assert_eq!(remaining, b"slice");

        remove_temp_file(&path);
    }

    #[test]
    fn test_from_file() {
        let contents = b"From file test";
        let path = create_temp_file("from_file_test", contents);

        let file = File::open(&path).expect("File open failed");
        let mut reader = MmapReader::from_file(&file).expect("from_file failed");

        let mut buffer = vec![0u8; contents.len()];
        reader.read_exact(&mut buffer).expect("Read failed");
        assert_eq!(&buffer, contents);

        remove_temp_file(&path);
    }

    #[test]
    fn test_mmap_options() {
        let contents = b"Options test data";
        let path = create_temp_file("options_test", contents);

        let reader = MmapOptions::new()
            .populate(true)
            .open(&path)
            .expect("Open with options failed");

        assert_eq!(reader.len(), contents.len());
        assert_eq!(reader.as_slice(), contents);

        remove_temp_file(&path);
    }

    #[test]
    fn test_mmap_options_open_file() {
        let contents = b"Options file test";
        let path = create_temp_file("options_file_test", contents);

        let file = File::open(&path).expect("File open failed");
        let reader = MmapOptions::new()
            .populate(false)
            .open_file(&file)
            .expect("Open file with options failed");

        assert_eq!(reader.len(), contents.len());
        assert_eq!(reader.as_slice(), contents);

        remove_temp_file(&path);
    }

    #[test]
    fn test_read_exact_insufficient_data() {
        let contents = b"Short";
        let path = create_temp_file("read_exact_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");
        let mut buffer = [0u8; 100];

        let result = reader.read_exact(&mut buffer);
        assert!(result.is_err());

        remove_temp_file(&path);
    }

    #[test]
    fn test_stream_position() {
        let contents = b"Stream position test";
        let path = create_temp_file("stream_pos_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");

        assert_eq!(reader.stream_position().expect("stream_position failed"), 0);

        reader.seek(SeekFrom::Start(7)).expect("Seek failed");
        assert_eq!(reader.stream_position().expect("stream_position failed"), 7);

        remove_temp_file(&path);
    }

    #[test]
    fn test_file_not_found() {
        let result = MmapReader::open("/nonexistent/path/to/file.dat");
        assert!(result.is_err());

        if let Err(OxiArcError::Io(io_err)) = result {
            assert_eq!(io_err.kind(), io::ErrorKind::NotFound);
        } else {
            panic!("Expected Io error with NotFound kind");
        }
    }

    #[test]
    fn test_multiple_reads() {
        let contents = b"AABBCCDDEE";
        let path = create_temp_file("multi_read_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");

        let mut buf1 = [0u8; 2];
        let mut buf2 = [0u8; 2];
        let mut buf3 = [0u8; 2];

        reader.read_exact(&mut buf1).expect("Read 1 failed");
        reader.read_exact(&mut buf2).expect("Read 2 failed");
        reader.read_exact(&mut buf3).expect("Read 3 failed");

        assert_eq!(&buf1, b"AA");
        assert_eq!(&buf2, b"BB");
        assert_eq!(&buf3, b"CC");
        assert_eq!(reader.position(), 6);

        remove_temp_file(&path);
    }

    #[test]
    fn test_seek_past_end() {
        let contents = b"Short";
        let path = create_temp_file("seek_past_end_test", contents);

        let mut reader = MmapReader::open(&path).expect("Open failed");

        // Seeking past end should be allowed (like std::io::Cursor)
        let pos = reader.seek(SeekFrom::Start(100)).expect("Seek failed");
        assert_eq!(pos, 100);
        assert_eq!(reader.position(), 100);

        // Reading should return 0 bytes
        let mut buffer = [0u8; 10];
        let bytes_read = reader.read(&mut buffer).expect("Read failed");
        assert_eq!(bytes_read, 0);

        remove_temp_file(&path);
    }
}

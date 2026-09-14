//! LZ4 frame format (`Read` / `Write` adapters), lz4_flex-shaped, over
//! [`oxiarc_lz4::Lz4FrameReader`] and [`oxiarc_lz4::Lz4FrameWriter`].

use std::fmt;
use std::io::{self, BufRead, Read, Write};

use oxiarc_lz4::{BlockMaxSize, FrameDescriptor, Lz4FrameReader, Lz4FrameWriter};

/// Errors that can occur when de/compressing lz4.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// Compression error.
    CompressionError(crate::block::CompressError),
    /// Decompression error.
    DecompressionError(crate::block::DecompressError),
    /// An io::Error was encountered.
    IoError(io::Error),
    /// Invalid block size.
    UnsupportedBlocksize(u8),
    /// Unsupported frame version.
    UnsupportedVersion(u8),
    /// Wrong magic number for the LZ4 frame format.
    WrongMagicNumber,
    /// Reserved bits set.
    ReservedBitsSet,
    /// Block header is malformed.
    InvalidBlockInfo,
    /// Read a block larger than specified in the Frame header.
    BlockTooBig,
    /// The Frame header checksum doesn't match.
    HeaderChecksumError,
    /// The block checksum doesn't match.
    BlockChecksumError,
    /// The content checksum doesn't match.
    ContentChecksumError,
    /// Read an skippable frame.
    SkippableFrame(u32),
    /// External dictionaries are not supported.
    DictionaryNotSupported,
    /// Content length differs.
    ContentLengthError {
        /// Expected content length.
        expected: u64,
        /// Actual content length.
        actual: u64,
    },
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        match e {
            Error::IoError(e) => e,
            Error::CompressionError(_)
            | Error::DecompressionError(_)
            | Error::SkippableFrame(_)
            | Error::DictionaryNotSupported => io::Error::other(e),
            Error::WrongMagicNumber
            | Error::UnsupportedBlocksize(..)
            | Error::UnsupportedVersion(..)
            | Error::ReservedBitsSet
            | Error::InvalidBlockInfo
            | Error::BlockTooBig
            | Error::HeaderChecksumError
            | Error::ContentChecksumError
            | Error::BlockChecksumError
            | Error::ContentLengthError { .. } => io::Error::new(io::ErrorKind::InvalidData, e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        let is_ours = e.get_ref().is_some_and(|inner| inner.is::<Error>());
        if is_ours {
            if let Some(inner) = e.into_inner() {
                if let Ok(ours) = inner.downcast::<Error>() {
                    return *ours;
                }
            }
            return Error::IoError(io::Error::other("lz4 error"));
        }
        Error::IoError(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Error {}

/// Different predefined block sizes to choose when compressing data.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum BlockSize {
    /// Will detect optimal frame size based on the size of the first write
    /// call.
    #[default]
    Auto = 0,
    /// The default block size.
    Max64KB = 4,
    /// 256KB block size.
    Max256KB = 5,
    /// 1MB block size.
    Max1MB = 6,
    /// 4MB block size.
    Max4MB = 7,
    /// 8MB block size (written as 4MB, the largest the LZ4 frame spec
    /// defines).
    Max8MB = 8,
}

/// The two `BlockMode` operations that can be set on (`FrameInfo`).
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum BlockMode {
    /// Every block is compressed independently.
    #[default]
    Independent,
    /// Blocks can reference data from previous blocks (decoding supported;
    /// the encoder writes independent blocks, which every decoder accepts).
    Linked,
}

/// The metadata for de/compressing with lz4 frame format.
#[derive(Debug, Default, Clone)]
pub struct FrameInfo {
    /// If set, includes the total uncompressed size of data in the frame.
    pub content_size: Option<u64>,
    /// The maximum uncompressed size of each data block.
    pub block_size: BlockSize,
    /// The block mode.
    pub block_mode: BlockMode,
    /// If set, includes a checksum for each data block in the frame.
    pub block_checksums: bool,
    /// If set, includes a content checksum to verify that the full frame
    /// contents have been decoded correctly.
    pub content_checksum: bool,
    /// Use the legacy frame format (not produced; a standard frame is
    /// written instead).
    pub legacy_frame: bool,
}

impl FrameInfo {
    /// Create a new `FrameInfo`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether to include the total uncompressed size of data in the frame.
    pub fn content_size(mut self, content_size: Option<u64>) -> Self {
        self.content_size = content_size;
        self
    }

    /// The maximum uncompressed size of each data block.
    pub fn block_size(mut self, block_size: BlockSize) -> Self {
        self.block_size = block_size;
        self
    }

    /// The block mode.
    pub fn block_mode(mut self, block_mode: BlockMode) -> Self {
        self.block_mode = block_mode;
        self
    }

    /// If set, includes a checksum for each data block in the frame.
    pub fn block_checksums(mut self, block_checksums: bool) -> Self {
        self.block_checksums = block_checksums;
        self
    }

    /// If set, includes a content checksum.
    pub fn content_checksum(mut self, content_checksum: bool) -> Self {
        self.content_checksum = content_checksum;
        self
    }

    /// Use the legacy frame format.
    pub fn legacy_frame(mut self, legacy_frame: bool) -> Self {
        self.legacy_frame = legacy_frame;
        self
    }

    fn to_descriptor(&self) -> FrameDescriptor {
        let size = match self.block_size {
            BlockSize::Auto | BlockSize::Max64KB => BlockMaxSize::Size64KB,
            BlockSize::Max256KB => BlockMaxSize::Size256KB,
            BlockSize::Max1MB => BlockMaxSize::Size1MB,
            BlockSize::Max4MB | BlockSize::Max8MB => BlockMaxSize::Size4MB,
        };
        let mut desc = FrameDescriptor::new()
            .with_block_max_size(size)
            .with_block_checksum(self.block_checksums)
            .with_content_checksum(self.content_checksum)
            .with_block_independence(true);
        if let Some(n) = self.content_size {
            desc = desc.with_content_size(n);
        }
        desc
    }
}

/// A writer for compressing a LZ4 stream.
///
/// Remember to call [`FrameEncoder::finish`] (or use
/// [`FrameEncoder::auto_finish`]): as in lz4_flex, dropping the encoder does
/// not terminate the frame.
pub struct FrameEncoder<W: Write> {
    inner: Lz4FrameWriter<W>,
    frame_info: FrameInfo,
}

impl<W: fmt::Debug + Write> fmt::Debug for FrameEncoder<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameEncoder")
            .field("w", self.inner.get_ref())
            .field("frame_info", &self.frame_info)
            .finish()
    }
}

impl<W: Write> FrameEncoder<W> {
    /// Creates a new Encoder with the specified FrameInfo.
    pub fn with_frame_info(frame_info: FrameInfo, wtr: W) -> Self {
        FrameEncoder {
            inner: Lz4FrameWriter::with_descriptor(wtr, frame_info.to_descriptor()),
            frame_info,
        }
    }

    /// Creates a new Encoder with the default settings.
    pub fn new(wtr: W) -> Self {
        Self::with_frame_info(FrameInfo::default(), wtr)
    }

    /// Returns a wrapper around `self` that will finish the stream on drop.
    pub fn auto_finish(self) -> AutoFinishEncoder<W> {
        AutoFinishEncoder {
            encoder: Some(self),
        }
    }

    /// The frame information used by this Encoder.
    pub fn frame_info(&mut self) -> &FrameInfo {
        &self.frame_info
    }

    /// Consumes this encoder, flushing internal buffer and writing stream
    /// terminator.
    ///
    /// # Errors
    ///
    /// Any I/O error from the underlying writer.
    pub fn finish(mut self) -> Result<W, Error> {
        self.try_finish()?;
        Ok(self.inner.into_inner())
    }

    /// Attempt to finish this output stream, flushing internal buffer and
    /// writing stream terminator.
    ///
    /// # Errors
    ///
    /// Any I/O error from the underlying writer.
    pub fn try_finish(&mut self) -> Result<(), Error> {
        self.inner.try_finish().map_err(Error::from)
    }

    /// Returns the underlying writer _without_ flushing the stream. This
    /// may leave the output in an unfinished state.
    pub fn into_inner(self) -> W {
        self.inner.into_inner()
    }

    /// Gets a reference to the underlying writer in this encoder.
    pub fn get_ref(&self) -> &W {
        self.inner.get_ref()
    }

    /// Gets a mutable reference to the underlying writer in this encoder.
    ///
    /// Note that mutating the output/input state of the stream may corrupt
    /// this encoder, so care must be taken when using this method.
    pub fn get_mut(&mut self) -> &mut W {
        self.inner.get_mut()
    }
}

impl<W: Write> Write for FrameEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A wrapper around an [`FrameEncoder<W>`] that finishes the stream on drop.
///
/// Errors on drop get silently ignored.
pub struct AutoFinishEncoder<W: Write> {
    encoder: Option<FrameEncoder<W>>,
}

impl<W: Write> Drop for AutoFinishEncoder<W> {
    fn drop(&mut self) {
        if let Some(mut encoder) = self.encoder.take() {
            let _ = encoder.try_finish();
        }
    }
}

impl<W: Write> Write for AutoFinishEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.encoder.as_mut() {
            Some(e) => e.write(buf),
            None => Err(io::Error::other("encoder already finished")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.encoder.as_mut() {
            Some(e) => e.flush(),
            None => Ok(()),
        }
    }
}

/// A reader for decompressing the LZ4 frame format.
///
/// Decodes concatenated frames, skips skippable frames, and supports linked
/// and independent blocks.
pub struct FrameDecoder<R: Read> {
    inner: Lz4FrameReader<R>,
}

impl<R: fmt::Debug + Read> fmt::Debug for FrameDecoder<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecoder")
            .field("r", self.inner.get_ref())
            .finish()
    }
}

impl<R: Read> FrameDecoder<R> {
    /// Creates a new Decoder for the specified reader.
    pub fn new(rdr: R) -> FrameDecoder<R> {
        FrameDecoder {
            inner: Lz4FrameReader::new(rdr),
        }
    }

    /// Gets a reference to the underlying reader in this decoder.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref()
    }

    /// Gets a mutable reference to the underlying reader in this decoder.
    ///
    /// Note that mutation of the stream may result in surprising results if
    /// this decoder is continued to be used.
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut()
    }

    /// Consumes the FrameDecoder and returns the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

impl<R: Read> Read for FrameDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read> BufRead for FrameDecoder<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.inner.fill_buf()
    }

    fn consume(&mut self, amt: usize) {
        self.inner.consume(amt);
    }
}

//! gzip encoder and decoders over a `Read` (buffered internally).

use std::io::{self, Read, Write};

use crate::bufreader::BufReader;

use super::{GzBuilder, GzHeader, bufread};
use crate::Compression;

pub(crate) fn gz_encoder<R: Read>(inner: bufread::GzEncoder<BufReader<R>>) -> GzEncoder<R> {
    GzEncoder { inner }
}

/// A gzip streaming encoder.
///
/// This structure implements a [`Read`] interface. When read from, it reads
/// uncompressed data from the underlying [`Read`] and provides the
/// compressed data.
#[derive(Debug)]
pub struct GzEncoder<R> {
    inner: bufread::GzEncoder<BufReader<R>>,
}

impl<R: Read> GzEncoder<R> {
    /// Creates a new encoder which will use the given compression level.
    ///
    /// The encoder is not configured specially for the emitted header. For
    /// header configuration, see the [`GzBuilder`] type.
    pub fn new(r: R, level: Compression) -> GzEncoder<R> {
        GzBuilder::new().read(r, level)
    }
}

impl<R> GzEncoder<R> {
    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying reader.
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut().get_mut()
    }

    /// Returns the underlying stream, consuming this encoder.
    pub fn into_inner(self) -> R {
        self.inner.into_inner().into_inner()
    }
}

impl<R: Read> Read for GzEncoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.inner.read(into)
    }
}

impl<R: Read + Write> Write for GzEncoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

/// A decoder for a single member of a gzip file.
///
/// This structure implements a [`Read`] interface. When read from, it reads
/// compressed data from the underlying [`Read`] and provides the
/// uncompressed data. Only the first member is decoded; use
/// [`MultiGzDecoder`] for concatenated members.
#[derive(Debug)]
pub struct GzDecoder<R> {
    inner: bufread::GzDecoder<BufReader<R>>,
}

impl<R: Read> GzDecoder<R> {
    /// Creates a new decoder from the given reader, immediately parsing the
    /// gzip header.
    pub fn new(r: R) -> GzDecoder<R> {
        GzDecoder {
            inner: bufread::GzDecoder::new(BufReader::new(r)),
        }
    }
}

impl<R> GzDecoder<R> {
    /// Returns the header associated with this stream, if it was valid.
    pub fn header(&self) -> Option<&GzHeader> {
        self.inner.header()
    }

    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut().get_mut()
    }

    /// Consumes this decoder, returning the underlying reader. Buffered but
    /// not yet decoded bytes are lost.
    pub fn into_inner(self) -> R {
        self.inner.into_inner().into_inner()
    }

    /// Resets the state of this decoder entirely, swapping out the input
    /// stream for another. Returns the old stream.
    pub fn reset(&mut self, r: R) -> R {
        self.inner.reset(BufReader::new(r)).into_inner()
    }
}

impl<R: Read> Read for GzDecoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.inner.read(into)
    }
}

impl<R: Read + Write> Write for GzDecoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

/// A gzip streaming decoder that decodes all members of a multistream.
#[derive(Debug)]
pub struct MultiGzDecoder<R> {
    inner: bufread::MultiGzDecoder<BufReader<R>>,
}

impl<R: Read> MultiGzDecoder<R> {
    /// Creates a new decoder from the given reader, immediately parsing the
    /// (first) gzip header.
    pub fn new(r: R) -> MultiGzDecoder<R> {
        MultiGzDecoder {
            inner: bufread::MultiGzDecoder::new(BufReader::new(r)),
        }
    }
}

impl<R> MultiGzDecoder<R> {
    /// Returns the current header associated with this stream, if it's
    /// valid.
    pub fn header(&self) -> Option<&GzHeader> {
        self.inner.header()
    }

    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut().get_mut()
    }

    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.inner.into_inner().into_inner()
    }
}

impl<R: Read> Read for MultiGzDecoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.inner.read(into)
    }
}

impl<R: Read + Write> Write for MultiGzDecoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

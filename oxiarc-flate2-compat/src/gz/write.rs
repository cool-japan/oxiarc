//! gzip encoder and decoders over a `Write`.

use std::io::{self, Read, Write};

use super::{GzBuilder, GzHeader, HeaderReader, corrupt};
use crate::crc::{Crc, CrcWriter};
use crate::zio;
use crate::{Compress, Compression, Decompress, Status};

const CRC_BYTES_LEN: usize = 8;

pub(crate) fn gz_encoder<W: Write>(header: Vec<u8>, w: W, lvl: Compression) -> GzEncoder<W> {
    GzEncoder {
        inner: zio::Writer::new(w, Compress::new(lvl, false)),
        crc: Crc::new(),
        header,
        crc_bytes_written: 0,
    }
}

fn write_nonzero<W: Write>(w: &mut W, buf: &[u8]) -> io::Result<usize> {
    match w.write(buf)? {
        0 => Err(io::ErrorKind::WriteZero.into()),
        n => Ok(n),
    }
}

/// A gzip streaming encoder.
///
/// This structure exposes a [`Write`] interface that will emit compressed
/// data to the underlying writer `W`. The stream (final block plus CRC-32 /
/// ISIZE trailer) is finished when the encoder is dropped, ignoring errors;
/// call [`GzEncoder::finish`] to observe them.
#[derive(Debug)]
pub struct GzEncoder<W: Write> {
    inner: zio::Writer<W, Compress>,
    crc: Crc,
    crc_bytes_written: usize,
    header: Vec<u8>,
}

impl<W: Write> GzEncoder<W> {
    /// Creates a new encoder which will use the given compression level.
    ///
    /// The encoder is not configured specially for the emitted header. For
    /// header configuration, see the [`GzBuilder`] type.
    pub fn new(w: W, level: Compression) -> GzEncoder<W> {
        GzBuilder::new().write(w, level)
    }

    /// Acquires a reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        self.inner.get_ref()
    }

    /// Acquires a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        self.inner.get_mut()
    }

    /// Attempt to finish this output stream, writing out final chunks of
    /// data.
    ///
    /// # Errors
    ///
    /// Any I/O error from the underlying writer.
    pub fn try_finish(&mut self) -> io::Result<()> {
        self.write_header()?;
        self.inner.finish()?;
        while self.crc_bytes_written < CRC_BYTES_LEN {
            let (sum, amt) = (self.crc.sum(), self.crc.amount());
            let mut buf = [0u8; CRC_BYTES_LEN];
            buf[..4].copy_from_slice(&sum.to_le_bytes());
            buf[4..].copy_from_slice(&amt.to_le_bytes());
            let n = write_nonzero(self.inner.get_mut(), &buf[self.crc_bytes_written..])?;
            self.crc_bytes_written += n;
        }
        Ok(())
    }

    /// Finish encoding this stream, returning the underlying writer once
    /// the encoding is done.
    ///
    /// # Errors
    ///
    /// Any I/O error from the underlying writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.try_finish()?;
        self.inner.take_inner()
    }

    fn write_header(&mut self) -> io::Result<()> {
        while !self.header.is_empty() {
            let n = write_nonzero(self.inner.get_mut(), &self.header)?;
            self.header.drain(..n);
        }
        Ok(())
    }
}

impl<W: Write> Write for GzEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.crc_bytes_written != 0 {
            return Err(io::Error::other("write after gzip stream was finished"));
        }
        self.write_header()?;
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.crc_bytes_written != 0 {
            return Err(io::Error::other("flush after gzip stream was finished"));
        }
        self.write_header()?;
        self.inner.flush()
    }
}

impl<R: Read + Write> Read for GzEncoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.get_mut().read(buf)
    }
}

impl<W: Write> Drop for GzEncoder<W> {
    fn drop(&mut self) {
        if self.inner.is_present() {
            let _ = self.try_finish();
        }
    }
}

/// A decoder for a single member of a gzip file.
///
/// This structure exposes a [`Write`] interface, receiving compressed data
/// and writing uncompressed data to the underlying writer.
#[derive(Debug)]
pub struct GzDecoder<W: Write> {
    inner: zio::Writer<CrcWriter<W>, Decompress>,
    crc_bytes: Vec<u8>,
    header_parser: HeaderReader,
    header: Option<GzHeader>,
}

impl<W: Write> GzDecoder<W> {
    /// Creates a new decoder which will write uncompressed data to the
    /// stream.
    pub fn new(w: W) -> GzDecoder<W> {
        GzDecoder {
            inner: zio::Writer::new(CrcWriter::new(w), Decompress::new(false)),
            crc_bytes: Vec::with_capacity(CRC_BYTES_LEN),
            header_parser: HeaderReader::new(),
            header: None,
        }
    }

    /// Returns the header associated with this stream.
    pub fn header(&self) -> Option<&GzHeader> {
        self.header.as_ref()
    }

    /// Acquires a reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        self.inner.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        self.inner.get_mut().get_mut()
    }

    /// Attempt to finish this output stream, writing out final chunks of
    /// data and verifying the trailer.
    ///
    /// # Errors
    ///
    /// An I/O error, or a missing / mismatching CRC-32 or ISIZE trailer.
    pub fn try_finish(&mut self) -> io::Result<()> {
        self.finish_and_check_crc()
    }

    /// Consumes this decoder, flushing the output stream.
    ///
    /// # Errors
    ///
    /// See [`GzDecoder::try_finish`].
    pub fn finish(mut self) -> io::Result<W> {
        self.finish_and_check_crc()?;
        Ok(self.inner.take_inner()?.into_inner())
    }

    fn finish_and_check_crc(&mut self) -> io::Result<()> {
        self.inner.finish()?;
        if self.crc_bytes.len() != CRC_BYTES_LEN {
            return Err(corrupt());
        }
        let b = &self.crc_bytes;
        let crc = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let amt = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        if crc != self.inner.get_ref().crc().sum() || amt != self.inner.get_ref().crc().amount() {
            return Err(corrupt());
        }
        Ok(())
    }
}

impl<W: Write> Write for GzDecoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let buflen = buf.len();
        if self.header.is_none() {
            let mut rest = buf;
            match self.header_parser.parse(&mut rest) {
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => Ok(buflen),
                Err(err) => Err(err),
                Ok(header) => {
                    self.header = Some(header);
                    Ok(buflen - rest.len())
                }
            }
        } else {
            let (n, status) = self.inner.write_with_status(buf)?;
            if status == Status::StreamEnd && n < buf.len() && self.crc_bytes.len() < CRC_BYTES_LEN
            {
                let remaining = buf.len() - n;
                let take = remaining.min(CRC_BYTES_LEN - self.crc_bytes.len());
                self.crc_bytes.extend_from_slice(&buf[n..n + take]);
                return Ok(n + take);
            }
            Ok(n)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Read + Write> Read for GzDecoder<W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.get_mut().get_mut().read(buf)
    }
}

/// A gzip streaming decoder that decodes all members of a multistream.
#[derive(Debug)]
pub struct MultiGzDecoder<W: Write> {
    inner: GzDecoder<W>,
}

impl<W: Write> MultiGzDecoder<W> {
    /// Creates a new decoder which will write uncompressed data to the
    /// stream. If the gzip stream contains multiple members all will be
    /// decoded.
    pub fn new(w: W) -> MultiGzDecoder<W> {
        MultiGzDecoder {
            inner: GzDecoder::new(w),
        }
    }

    /// Returns the header associated with the current member.
    pub fn header(&self) -> Option<&GzHeader> {
        self.inner.header()
    }

    /// Acquires a reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        self.inner.get_ref()
    }

    /// Acquires a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        self.inner.get_mut()
    }

    /// Attempt to finish this output stream.
    ///
    /// # Errors
    ///
    /// See [`GzDecoder::try_finish`].
    pub fn try_finish(&mut self) -> io::Result<()> {
        self.inner.try_finish()
    }

    /// Consumes this decoder, flushing the output stream.
    ///
    /// # Errors
    ///
    /// See [`GzDecoder::try_finish`].
    pub fn finish(self) -> io::Result<W> {
        self.inner.finish()
    }
}

impl<W: Write> Write for MultiGzDecoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.inner.write(buf) {
            Ok(0) => {
                self.inner.try_finish()?;
                let w = self.inner.inner.take_inner()?.into_inner();
                self.inner = GzDecoder::new(w);
                self.inner.write(buf)
            }
            res => res,
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

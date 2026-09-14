//! gzip encoder and decoders over a `BufRead`.

use std::io::{self, BufRead, Read, Write};

use super::{GzBuilder, GzHeader, HeaderReader, corrupt};
use crate::Compression;
use crate::crc::CrcReader;
use crate::deflate;

/// Build an encoder from pre-serialised header bytes.
pub(crate) fn gz_encoder<R: BufRead>(header: Vec<u8>, r: R, lvl: Compression) -> GzEncoder<R> {
    let crc = CrcReader::new(r);
    GzEncoder {
        inner: deflate::bufread::DeflateEncoder::new(crc, lvl),
        header,
        pos: 0,
        eof: false,
    }
}

/// A gzip streaming encoder.
///
/// This structure implements a [`Read`] interface. When read from, it reads
/// uncompressed data from the underlying [`BufRead`] and provides the
/// compressed data.
#[derive(Debug)]
pub struct GzEncoder<R> {
    inner: deflate::bufread::DeflateEncoder<CrcReader<R>>,
    header: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: BufRead> GzEncoder<R> {
    /// Creates a new encoder which will use the given compression level.
    ///
    /// The encoder is not configured specially for the emitted header. For
    /// header configuration, see the [`GzBuilder`] type.
    pub fn new(r: R, level: Compression) -> GzEncoder<R> {
        GzBuilder::new().buf_read(r, level)
    }

    fn read_footer(&mut self, into: &mut [u8]) -> io::Result<usize> {
        if self.pos == 8 {
            return Ok(0);
        }
        let crc = self.inner.get_ref().crc();
        let mut footer = [0u8; 8];
        footer[..4].copy_from_slice(&crc.sum().to_le_bytes());
        footer[4..].copy_from_slice(&crc.amount().to_le_bytes());
        let n = into.len().min(8 - self.pos);
        into[..n].copy_from_slice(&footer[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
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

impl<R: BufRead> Read for GzEncoder<R> {
    fn read(&mut self, mut into: &mut [u8]) -> io::Result<usize> {
        let mut amt = 0;
        if self.eof {
            return self.read_footer(into);
        } else if self.pos < self.header.len() {
            let n = into.len().min(self.header.len() - self.pos);
            into[..n].copy_from_slice(&self.header[self.pos..self.pos + n]);
            self.pos += n;
            amt += n;
            if amt == into.len() {
                return Ok(amt);
            }
            into = &mut into[n..];
        }
        match self.inner.read(into)? {
            0 => {
                self.eof = true;
                self.pos = 0;
                self.read_footer(into).map(|n| n + amt)
            }
            n => Ok(amt + n),
        }
    }
}

impl<R: BufRead + Write> Write for GzEncoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

#[derive(Debug)]
enum GzState {
    Header(HeaderReader),
    Body(GzHeader),
    Finished(GzHeader, usize, [u8; 8]),
    Err(io::Error),
    End(Option<GzHeader>),
}

/// A decoder for a single member of a gzip file.
///
/// This structure consumes a [`BufRead`] interface, reading compressed data
/// from the underlying reader, and emitting uncompressed data. Bytes after
/// the first member are left unread in the underlying reader; use
/// [`MultiGzDecoder`] to decode every member.
#[derive(Debug)]
pub struct GzDecoder<R> {
    state: GzState,
    reader: CrcReader<deflate::bufread::DeflateDecoder<R>>,
    multi: bool,
}

impl<R: BufRead> GzDecoder<R> {
    /// Creates a new decoder from the given reader, immediately parsing the
    /// gzip header.
    pub fn new(mut r: R) -> GzDecoder<R> {
        let mut parser = HeaderReader::new();
        let state = match parser.parse(&mut r) {
            Ok(header) => GzState::Body(header),
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => GzState::Header(parser),
            Err(err) => GzState::Err(err),
        };
        GzDecoder {
            state,
            reader: CrcReader::new(deflate::bufread::DeflateDecoder::new(r)),
            multi: false,
        }
    }

    fn multi(mut self, flag: bool) -> GzDecoder<R> {
        self.multi = flag;
        self
    }
}

impl<R> GzDecoder<R> {
    /// Returns the header associated with this stream, if it was valid.
    pub fn header(&self) -> Option<&GzHeader> {
        match &self.state {
            GzState::Body(header) | GzState::Finished(header, _, _) => Some(header),
            GzState::End(header) => header.as_ref(),
            _ => None,
        }
    }

    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.reader.get_ref().get_ref()
    }

    /// Acquires a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut R {
        self.reader.get_mut().get_mut()
    }

    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader.into_inner().into_inner()
    }

    /// Resets the state of this decoder entirely, swapping out the input
    /// stream for another. Returns the old stream.
    pub fn reset(&mut self, r: R) -> R {
        self.state = GzState::Header(HeaderReader::new());
        self.reader.reset();
        self.reader.get_mut().reset(r)
    }
}

impl<R: BufRead> Read for GzDecoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        loop {
            match &mut self.state {
                GzState::Header(parser) => {
                    let header = parser.parse(self.reader.get_mut().get_mut())?;
                    self.state = GzState::Body(header);
                }
                GzState::Body(header) => {
                    if into.is_empty() {
                        return Ok(0);
                    }
                    match self.reader.read(into)? {
                        0 => {
                            self.state = GzState::Finished(std::mem::take(header), 0, [0; 8]);
                        }
                        n => return Ok(n),
                    }
                }
                GzState::Finished(header, pos, buf) => {
                    if *pos < buf.len() {
                        let n = self.reader.get_mut().get_mut().read(&mut buf[*pos..])?;
                        if n == 0 {
                            return Err(io::ErrorKind::UnexpectedEof.into());
                        }
                        *pos += n;
                    } else {
                        let crc = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
                        let amt = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        if crc != self.reader.crc().sum() || amt != self.reader.crc().amount() {
                            self.state = GzState::End(Some(std::mem::take(header)));
                            return Err(corrupt());
                        } else if self.multi {
                            let is_eof = self
                                .reader
                                .get_mut()
                                .get_mut()
                                .fill_buf()
                                .map(|buf| buf.is_empty())?;
                            if is_eof {
                                self.state = GzState::End(Some(std::mem::take(header)));
                            } else {
                                self.reader.reset();
                                self.reader.get_mut().reset_data();
                                self.state = GzState::Header(HeaderReader::new());
                            }
                        } else {
                            self.state = GzState::End(Some(std::mem::take(header)));
                        }
                    }
                }
                GzState::Err(err) => {
                    let result = Err(std::mem::replace(err, io::ErrorKind::Other.into()));
                    self.state = GzState::End(None);
                    return result;
                }
                GzState::End(_) => return Ok(0),
            }
        }
    }
}

impl<R: BufRead + Write> Write for GzDecoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

/// A gzip streaming decoder that decodes all members of a multistream.
///
/// A gzip member consists of a header, compressed data and a trailer. The
/// gzip specification allows multiple members to be joined in a single
/// stream; this decoder emits their concatenated contents.
#[derive(Debug)]
pub struct MultiGzDecoder<R>(GzDecoder<R>);

impl<R: BufRead> MultiGzDecoder<R> {
    /// Creates a new decoder from the given reader, immediately parsing the
    /// (first) gzip header.
    pub fn new(r: R) -> MultiGzDecoder<R> {
        MultiGzDecoder(GzDecoder::new(r).multi(true))
    }
}

impl<R> MultiGzDecoder<R> {
    /// Returns the current header associated with this stream, if it's
    /// valid.
    pub fn header(&self) -> Option<&GzHeader> {
        self.0.header()
    }

    /// Acquires a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        self.0.get_ref()
    }

    /// Acquires a mutable reference to the underlying stream.
    pub fn get_mut(&mut self) -> &mut R {
        self.0.get_mut()
    }

    /// Consumes this decoder, returning the underlying reader.
    pub fn into_inner(self) -> R {
        self.0.into_inner()
    }
}

impl<R: BufRead> Read for MultiGzDecoder<R> {
    fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        self.0.read(into)
    }
}

impl<R: BufRead + Write> Write for MultiGzDecoder<R> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.get_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.get_mut().flush()
    }
}

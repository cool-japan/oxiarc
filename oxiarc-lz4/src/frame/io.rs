//! `std::io` adapters for the LZ4 frame format: [`Lz4FrameReader`] (a
//! `Read`/`BufRead` decoder that handles concatenated and skippable frames)
//! and [`Lz4FrameWriter`] (a `Write` encoder with an explicit
//! [`Lz4FrameWriter::finish`]).

use std::io::{self, BufRead, Read, Write};

use oxiarc_core::error::OxiArcError;
use oxiarc_core::traits::{CompressStatus, Compressor, Decompressor, FlushMode};

use super::streaming::{Lz4Compressor, Lz4Decompressor};
use super::types::{FrameDescriptor, LZ4_FRAME_MAGIC};

/// Size of the reads issued to the inner reader and of the staging buffers.
const CHUNK: usize = 64 * 1024;

/// Skippable frames use magics `0x184D2A50..=0x184D2A5F`.
fn is_skippable_magic(magic: u32) -> bool {
    magic & 0xFFFF_FFF0 == 0x184D_2A50
}

fn to_io(err: OxiArcError) -> io::Error {
    match err {
        OxiArcError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// Where the reader is between frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    /// Expecting a frame magic (or end of stream).
    FrameStart,
    /// Skipping the remaining bytes of a skippable frame.
    Skipping(u64),
    /// Inside an LZ4 frame.
    InFrame,
    /// The inner reader is exhausted at a frame boundary.
    Eof,
}

/// A streaming LZ4 frame decoder over any [`Read`].
///
/// Decodes every concatenated frame until the inner reader is exhausted,
/// skipping skippable frames, and supports both independent and linked
/// blocks. A stream that ends inside a frame is an
/// [`io::ErrorKind::UnexpectedEof`] error.
///
/// ```
/// use std::io::Read;
/// use oxiarc_lz4::{Lz4FrameReader, compress};
///
/// let mut two = compress(b"first ")?;
/// two.extend(compress(b"second")?);
/// let mut text = String::new();
/// Lz4FrameReader::new(&two[..]).read_to_string(&mut text)?;
/// assert_eq!(text, "first second");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Lz4FrameReader<R> {
    inner: R,
    decoder: Lz4Decompressor,
    /// Raw bytes read from `inner` and not yet handed to `decoder`.
    pending: Vec<u8>,
    pending_pos: usize,
    /// Decoded bytes not yet returned.
    out: Vec<u8>,
    out_pos: usize,
    state: ReadState,
    frames: u64,
}

impl<R: Read> Lz4FrameReader<R> {
    /// Create a decoder reading frames from `inner`.
    pub fn new(inner: R) -> Self {
        Lz4FrameReader {
            inner,
            decoder: Lz4Decompressor::new(),
            pending: Vec::new(),
            pending_pos: 0,
            out: Vec::with_capacity(CHUNK),
            out_pos: 0,
            state: ReadState::FrameStart,
            frames: 0,
        }
    }
}

impl<R> Lz4FrameReader<R> {
    /// Borrow the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrow the inner reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Return the inner reader. Bytes already read from it but not yet
    /// decoded are lost.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Number of LZ4 frames fully decoded so far.
    pub fn frames_decoded(&self) -> u64 {
        self.frames
    }
}

impl<R: Read> Lz4FrameReader<R> {
    /// Make sure at least `n` unread raw bytes are pending; `Ok(false)` if
    /// the inner reader ended first.
    fn need(&mut self, n: usize) -> io::Result<bool> {
        if self.pending_pos > 0 && self.pending_pos == self.pending.len() {
            self.pending.clear();
            self.pending_pos = 0;
        }
        while self.pending.len() - self.pending_pos < n {
            let start = self.pending.len();
            self.pending.resize(start + CHUNK, 0);
            let got = loop {
                match self.inner.read(&mut self.pending[start..]) {
                    Ok(got) => break got,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.pending.truncate(start);
                        return Err(e);
                    }
                }
            };
            self.pending.truncate(start + got);
            if got == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Produce more decoded bytes into `self.out`; `Ok(false)` at the end of
    /// the stream.
    fn refill(&mut self) -> io::Result<bool> {
        self.out.clear();
        self.out_pos = 0;
        loop {
            match self.state {
                ReadState::Eof => return Ok(false),
                ReadState::FrameStart => {
                    if !self.need(4)? {
                        if self.pending_pos == self.pending.len() {
                            self.state = ReadState::Eof;
                            return Ok(false);
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated LZ4 frame magic",
                        ));
                    }
                    let p = &self.pending[self.pending_pos..self.pending_pos + 4];
                    let magic = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                    if is_skippable_magic(magic) {
                        if !self.need(8)? {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "truncated skippable frame header",
                            ));
                        }
                        let p = &self.pending[self.pending_pos + 4..self.pending_pos + 8];
                        let size = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
                        self.pending_pos += 8;
                        self.state = ReadState::Skipping(u64::from(size));
                    } else if magic == LZ4_FRAME_MAGIC {
                        self.decoder = Lz4Decompressor::new();
                        self.state = ReadState::InFrame;
                    } else {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid LZ4 frame magic {magic:#010x}"),
                        ));
                    }
                }
                ReadState::Skipping(left) => {
                    if left == 0 {
                        self.state = ReadState::FrameStart;
                        continue;
                    }
                    if !self.need(1)? {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "truncated skippable frame",
                        ));
                    }
                    let avail = (self.pending.len() - self.pending_pos) as u64;
                    let skip = avail.min(left);
                    self.pending_pos += skip as usize;
                    self.state = ReadState::Skipping(left - skip);
                }
                ReadState::InFrame => {
                    if self.decoder.frame_complete() {
                        let rest = self.decoder.take_remaining_input();
                        if !rest.is_empty() {
                            let tail = self.pending.split_off(self.pending_pos);
                            self.pending = rest;
                            self.pending.extend_from_slice(&tail);
                            self.pending_pos = 0;
                        }
                        self.frames += 1;
                        self.state = ReadState::FrameStart;
                        continue;
                    }
                    if self.pending_pos >= self.pending.len() && !self.need(1)? {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "LZ4 stream ended inside a frame",
                        ));
                    }
                    let fed = self.pending.len() - self.pending_pos;
                    let mut first = true;
                    let mut scratch = [0u8; 8192];
                    loop {
                        let feed: &[u8] = if first {
                            &self.pending[self.pending_pos..]
                        } else {
                            &[]
                        };
                        let (_, written, status) =
                            self.decoder.decompress(feed, &mut scratch).map_err(to_io)?;
                        first = false;
                        self.out.extend_from_slice(&scratch[..written]);
                        if !matches!(status, oxiarc_core::traits::DecompressStatus::NeedsOutput) {
                            break;
                        }
                    }
                    self.pending_pos += fed;
                    if !self.out.is_empty() {
                        return Ok(true);
                    }
                }
            }
        }
    }
}

impl<R: Read> Read for Lz4FrameReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let available = self.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl<R: Read> BufRead for Lz4FrameReader<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.out_pos == self.out.len() {
            self.refill()?;
        }
        Ok(&self.out[self.out_pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.out_pos = (self.out_pos + amt).min(self.out.len());
    }
}

/// A streaming LZ4 frame encoder over any [`Write`].
///
/// The frame is only terminated (end mark and optional content checksum)
/// by [`Lz4FrameWriter::finish`] / [`Lz4FrameWriter::try_finish`]; dropping
/// the writer without finishing leaves an incomplete frame, the same
/// contract as `lz4_flex::frame::FrameEncoder`.
///
/// ```
/// use std::io::{Read, Write};
/// use oxiarc_lz4::{Lz4FrameReader, Lz4FrameWriter};
///
/// let mut w = Lz4FrameWriter::new(Vec::new());
/// w.write_all(b"streamed lz4")?;
/// let frame = w.finish()?;
/// let mut back = Vec::new();
/// Lz4FrameReader::new(&frame[..]).read_to_end(&mut back)?;
/// assert_eq!(back, b"streamed lz4");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug)]
pub struct Lz4FrameWriter<W> {
    inner: W,
    encoder: Lz4Compressor,
    scratch: Vec<u8>,
    finished: bool,
}

impl<W: Write> Lz4FrameWriter<W> {
    /// Create an encoder with the default frame descriptor.
    pub fn new(inner: W) -> Self {
        Lz4FrameWriter::with_descriptor(inner, FrameDescriptor::new())
    }

    /// Create an encoder with an explicit frame descriptor (block size,
    /// checksums, content size).
    pub fn with_descriptor(inner: W, desc: FrameDescriptor) -> Self {
        Lz4FrameWriter {
            inner,
            encoder: Lz4Compressor::with_options(desc),
            scratch: vec![0u8; CHUNK],
            finished: false,
        }
    }

    fn pump(&mut self, input: &[u8], flush: FlushMode) -> io::Result<()> {
        let mut feed = input;
        loop {
            let (_, written, status) = self
                .encoder
                .compress(feed, &mut self.scratch, flush)
                .map_err(to_io)?;
            feed = &[];
            self.inner.write_all(&self.scratch[..written])?;
            if !matches!(status, CompressStatus::NeedsOutput) {
                return Ok(());
            }
        }
    }

    /// Terminate the frame, flushing everything to the inner writer.
    ///
    /// # Errors
    ///
    /// Any I/O error from the inner writer.
    pub fn try_finish(&mut self) -> io::Result<()> {
        if !self.finished {
            self.pump(&[], FlushMode::Finish)?;
            self.finished = true;
        }
        self.inner.flush()
    }

    /// Terminate the frame and return the inner writer.
    ///
    /// # Errors
    ///
    /// Any I/O error from the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.try_finish()?;
        Ok(self.inner)
    }
}

impl<W> Lz4FrameWriter<W> {
    /// Borrow the inner writer.
    pub fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Mutably borrow the inner writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Return the inner writer without terminating the frame.
    pub fn into_inner(self) -> W {
        self.inner
    }

    /// Whether the frame has been terminated.
    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

impl<W: Write> Write for Lz4FrameWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("write after LZ4 frame was finished"));
        }
        self.pump(buf, FlushMode::None)?;
        Ok(buf.len())
    }

    /// Flushes the inner writer. Bytes of a not-yet-complete block stay
    /// buffered: an LZ4 frame cannot emit a partial block without ending it
    /// early.
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{BlockMaxSize, compress_with_options};

    fn data(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| {
                if i % 1000 < 700 {
                    (i % 13) as u8
                } else {
                    (i * 7919 % 251) as u8
                }
            })
            .collect()
    }

    #[test]
    fn writer_reader_roundtrip_tiny_reads() -> io::Result<()> {
        let input = data(1_000_000);
        let mut w = Lz4FrameWriter::new(Vec::new());
        for piece in input.chunks(3333) {
            w.write_all(piece)?;
        }
        let frame = w.finish()?;
        let mut r = Lz4FrameReader::new(&frame[..]);
        let mut out = Vec::new();
        let mut buf = [0u8; 97];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, input);
        assert_eq!(r.frames_decoded(), 1);
        Ok(())
    }

    #[test]
    fn concatenated_skippable_and_linked() -> Result<(), Box<dyn std::error::Error>> {
        let a = data(200_000);
        let b = data(90_000);
        let linked = compress_with_options(
            &b,
            FrameDescriptor::new()
                .with_block_independence(false)
                .with_block_max_size(BlockMaxSize::Size64KB)
                .with_content_checksum(true),
        )?;
        let mut stream = crate::frame::compress(&a)?;
        // Skippable frame: magic, size 5, payload.
        stream.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
        stream.extend_from_slice(&5u32.to_le_bytes());
        stream.extend_from_slice(b"skip!");
        stream.extend_from_slice(&linked);
        let mut out = Vec::new();
        let mut r = Lz4FrameReader::new(&stream[..]);
        r.read_to_end(&mut out)?;
        let mut expected = a.clone();
        expected.extend_from_slice(&b);
        assert_eq!(out, expected);
        assert_eq!(r.frames_decoded(), 2);
        Ok(())
    }

    #[test]
    fn truncated_frame_is_unexpected_eof() -> io::Result<()> {
        let frame = crate::frame::compress(&data(100_000)).map_err(to_io)?;
        let cut = &frame[..frame.len() - 10];
        let err = Lz4FrameReader::new(cut).read_to_end(&mut Vec::new());
        assert!(matches!(err, Err(e) if e.kind() == io::ErrorKind::UnexpectedEof));
        let err = Lz4FrameReader::new(&b"not lz4"[..]).read_to_end(&mut Vec::new());
        assert!(err.is_err());
        assert_eq!(
            Lz4FrameReader::new(&b""[..]).read_to_end(&mut Vec::new())?,
            0
        );
        Ok(())
    }
}

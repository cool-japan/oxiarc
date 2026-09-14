//! A `Write` adapter that emits the Snappy framing format.

use std::fmt;
use std::io::{self, Write};

pub use crate::error::IntoInnerError;
use crate::{MAX_BLOCK_SIZE, STREAM_IDENTIFIER};

/// Encode one framed chunk (compressed when that saves at least 1/8, as
/// snap does, otherwise stored) into `out`.
pub(crate) fn encode_chunk(src: &[u8], out: &mut Vec<u8>) {
    let checksum = oxiarc_snappy::crc32c::masked_crc32c(src);
    let compressed = oxiarc_snappy::compress(src);
    let (chunk_type, body): (u8, &[u8]) = if compressed.len() < src.len() - src.len() / 8 {
        (0x00, &compressed)
    } else {
        (0x01, src)
    };
    let len = (body.len() + 4) as u32;
    out.push(chunk_type);
    out.extend_from_slice(&len.to_le_bytes()[..3]);
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(body);
}

/// A writer for compressing a Snappy stream.
///
/// This `FrameEncoder` wraps any other writer that implements `io::Write`.
/// Bytes written to this writer are compressed using the Snappy frame
/// format. Writes are buffered automatically, so there's no need to wrap
/// the underlying writer in a `BufWriter`. Buffered data is flushed when
/// the encoder is dropped (errors ignored) or by [`FrameEncoder::into_inner`].
pub struct FrameEncoder<W: Write> {
    inner: Option<W>,
    src: Vec<u8>,
    wrote_stream_ident: bool,
    scratch: Vec<u8>,
}

impl<W: Write + fmt::Debug> fmt::Debug for FrameEncoder<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameEncoder")
            .field("inner", &self.inner)
            .field("buffered", &self.src.len())
            .finish()
    }
}

impl<W: Write> FrameEncoder<W> {
    /// Create a new writer for streaming Snappy compression.
    pub fn new(wtr: W) -> FrameEncoder<W> {
        FrameEncoder {
            inner: Some(wtr),
            src: Vec::with_capacity(MAX_BLOCK_SIZE),
            wrote_stream_ident: false,
            scratch: Vec::new(),
        }
    }

    /// Returns the underlying stream, consuming and flushing this writer.
    ///
    /// # Errors
    ///
    /// If flushing fails, the error is returned together with the encoder.
    pub fn into_inner(mut self) -> Result<W, IntoInnerError<FrameEncoder<W>>> {
        match self.flush() {
            Ok(()) => match self.inner.take() {
                Some(w) => Ok(w),
                None => {
                    let err = io::Error::other("encoder already consumed");
                    Err(IntoInnerError::new(self, err))
                }
            },
            Err(err) => Err(IntoInnerError::new(self, err)),
        }
    }

    /// Gets a reference to the underlying writer in this encoder.
    pub fn get_ref(&self) -> &W {
        match self.inner.as_ref() {
            Some(w) => w,
            None => unreachable!("FrameEncoder used after into_inner"),
        }
    }

    /// Gets a mutable reference to the underlying writer in this encoder.
    ///
    /// Note that mutating the output/input state of the stream may corrupt
    /// this encoder, so care must be taken when using this method.
    pub fn get_mut(&mut self) -> &mut W {
        match self.inner.as_mut() {
            Some(w) => w,
            None => unreachable!("FrameEncoder used after into_inner"),
        }
    }

    fn write_block(&mut self, block: &[u8]) -> io::Result<()> {
        let Some(w) = self.inner.as_mut() else {
            return Err(io::Error::other("encoder already consumed"));
        };
        self.scratch.clear();
        if !self.wrote_stream_ident {
            self.scratch.extend_from_slice(STREAM_IDENTIFIER);
            self.wrote_stream_ident = true;
        }
        for chunk in block.chunks(MAX_BLOCK_SIZE) {
            encode_chunk(chunk, &mut self.scratch);
        }
        w.write_all(&self.scratch)
    }
}

impl<W: Write> Drop for FrameEncoder<W> {
    fn drop(&mut self) {
        if self.inner.is_some() {
            let _ = self.flush();
        }
    }
}

impl<W: Write> Write for FrameEncoder<W> {
    fn write(&mut self, mut buf: &[u8]) -> io::Result<usize> {
        let mut total = 0;
        // If there isn't enough room to add buf to src, then add only a
        // piece of it, flush it and mush on.
        loop {
            let free = self.src.capacity().max(MAX_BLOCK_SIZE) - self.src.len();
            if buf.len() <= free {
                break;
            }
            if self.src.is_empty() {
                // Big write with nothing buffered: compress straight from
                // the caller's buffer.
                let n = buf.len() - buf.len() % MAX_BLOCK_SIZE;
                let n = if n == 0 {
                    MAX_BLOCK_SIZE.min(buf.len())
                } else {
                    n
                };
                self.write_block(&buf[..n])?;
                total += n;
                buf = &buf[n..];
            } else {
                self.src.extend_from_slice(&buf[..free]);
                buf = &buf[free..];
                total += free;
                self.flush()?;
            }
        }
        self.src.extend_from_slice(buf);
        total += buf.len();
        Ok(total)
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.src.is_empty() {
            return Ok(());
        }
        let src = std::mem::take(&mut self.src);
        let result = self.write_block(&src);
        self.src = src;
        result?;
        self.src.clear();
        Ok(())
    }
}

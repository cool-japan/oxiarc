//! `Read` adapters for the Snappy framing format.

use std::fmt;
use std::io::{self, Read};

use crate::write::encode_chunk;
use crate::{MAX_BLOCK_SIZE, STREAM_IDENTIFIER};

/// A reader for decompressing a Snappy stream.
///
/// This `FrameDecoder` wraps any other reader that implements `io::Read`.
/// Bytes read from this reader are decompressed using the Snappy frame
/// format (CRC-32C verified; padding and skippable chunks ignored).
pub struct FrameDecoder<R: Read> {
    inner: oxiarc_snappy::FrameDecoder<R>,
}

impl<R: Read + fmt::Debug> fmt::Debug for FrameDecoder<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameDecoder")
            .field("inner", self.inner.get_ref())
            .finish()
    }
}

impl<R: Read> FrameDecoder<R> {
    /// Create a new reader for streaming Snappy decompression.
    pub fn new(rdr: R) -> FrameDecoder<R> {
        FrameDecoder {
            // snap imposes no total-output cap.
            inner: oxiarc_snappy::FrameDecoder::new(rdr).with_max_output_size(u64::MAX),
        }
    }

    /// Gets a reference to the underlying reader in this decoder.
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref()
    }

    /// Gets a mutable reference to the underlying reader in this decoder.
    ///
    /// Note that mutation of the stream may result in surprising results
    /// if this decoder is continued to be used.
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut()
    }

    /// Gets the underlying reader of this decoder.
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

impl<R: Read> Read for FrameDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

/// An encoder that reads uncompressed bytes from a `Read` and emits the
/// Snappy framing format when read from.
pub struct FrameEncoder<R: Read> {
    inner: R,
    src: Vec<u8>,
    dst: Vec<u8>,
    dst_pos: usize,
    wrote_stream_ident: bool,
    eof: bool,
}

impl<R: Read + fmt::Debug> fmt::Debug for FrameEncoder<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameEncoder")
            .field("inner", &self.inner)
            .finish()
    }
}

impl<R: Read> FrameEncoder<R> {
    /// Create a new reader for streaming Snappy compression.
    pub fn new(rdr: R) -> FrameEncoder<R> {
        FrameEncoder {
            inner: rdr,
            src: vec![0; MAX_BLOCK_SIZE],
            dst: Vec::new(),
            dst_pos: 0,
            wrote_stream_ident: false,
            eof: false,
        }
    }

    /// Gets a reference to the underlying reader in this encoder.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Gets a mutable reference to the underlying reader in this encoder.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Read a full block (or up to EOF) from the inner reader.
    fn fill_block(&mut self) -> io::Result<usize> {
        let mut filled = 0;
        while filled < self.src.len() {
            match self.inner.read(&mut self.src[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }
}

impl<R: Read> Read for FrameEncoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.dst_pos < self.dst.len() {
                let n = buf.len().min(self.dst.len() - self.dst_pos);
                buf[..n].copy_from_slice(&self.dst[self.dst_pos..self.dst_pos + n]);
                self.dst_pos += n;
                return Ok(n);
            }
            if self.eof || buf.is_empty() {
                return Ok(0);
            }
            self.dst.clear();
            self.dst_pos = 0;
            if !self.wrote_stream_ident {
                self.dst.extend_from_slice(STREAM_IDENTIFIER);
                self.wrote_stream_ident = true;
            }
            let n = self.fill_block()?;
            if n == 0 {
                self.eof = true;
            } else {
                let block = &self.src[..n];
                encode_chunk(block, &mut self.dst);
            }
        }
    }
}

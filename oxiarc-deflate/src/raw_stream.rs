//! Async streaming raw-DEFLATE wrappers for RFC 4978 IMAP COMPRESS=DEFLATE.
//!
//! [`RawDeflateWriter`] transparently compresses writes using sync-flush DEFLATE.
//! [`RawInflateReader`] transparently decompresses a sync-flushed raw-DEFLATE stream.
//!
//! Both preserve the LZ77 sliding window across flush boundaries (RFC 4978 §3).

use crate::async_reader::AsyncInflateReader;
use crate::deflate::Deflater;
use crate::wrapper::{InflateWrapper, TrailingPolicy, WrappedInflate};
use oxiarc_core::traits::FlushMode;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ── RawDeflateWriter ─────────────────────────────────────────────────────────

/// An [`AsyncWrite`] adapter that sync-flush-compresses data for RFC 4978.
///
/// Each `poll_flush` call compresses buffered plaintext with `deflate_sync()`
/// and writes the compressed bytes to the underlying writer.
pub struct RawDeflateWriter<W> {
    inner: W,
    deflater: Deflater,
    /// Plaintext buffered since last flush.
    pending: Vec<u8>,
    /// Compressed bytes queued for writing to `inner`.
    write_buf: Vec<u8>,
    /// Write cursor into `write_buf`.
    write_pos: usize,
}

impl<W> RawDeflateWriter<W> {
    /// Wrap `inner` with a compressor at level (0–9; 6 is a balanced default).
    pub fn new(inner: W, level: u8) -> Self {
        Self {
            inner,
            deflater: Deflater::new(level),
            pending: Vec::new(),
            write_buf: Vec::new(),
            write_pos: 0,
        }
    }

    /// Unwrap, returning the inner writer. Buffered data is discarded.
    pub fn into_inner(self) -> W {
        self.inner
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for RawDeflateWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.pending.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();

        // Compress pending bytes when write_buf is exhausted.
        if this.write_pos >= this.write_buf.len() && !this.pending.is_empty() {
            let mut compressed = Vec::new();
            if let Err(e) = this.deflater.deflate_sync(&this.pending, &mut compressed) {
                return Poll::Ready(Err(io::Error::other(e)));
            }
            this.pending.clear();
            this.write_buf = compressed;
            this.write_pos = 0;
        }

        // Drain write_buf into inner.
        while this.write_pos < this.write_buf.len() {
            let remaining = &this.write_buf[this.write_pos..];
            match Pin::new(&mut this.inner).poll_write(cx, remaining) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write returned 0",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    this.write_pos += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }

        // Forward flush to inner.
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Flush first.
        match self.as_mut().poll_flush(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// ── RawInflateReader ─────────────────────────────────────────────────────────

/// An [`AsyncRead`] adapter that decompresses a sync-flushed raw-DEFLATE stream
/// as produced by the RFC 4978 compressor.
///
/// Decoding is resumable at the bit level, so a sync-flush unit split across
/// several TCP segments costs nothing extra: bytes are handed to the caller
/// as soon as they decode, rather than being withheld until a whole unit has
/// arrived, and a partial segment is never re-decoded.
///
/// The LZ77 window is carried across flush boundaries (RFC 4978 §3), and
/// end-of-stream on the socket is **not** treated as truncation: an IMAP peer
/// may fall silent between commands and resume later, so the decoder stays in
/// [`FlushMode::None`] throughout and simply reports EOF.
///
/// # Example
///
/// ```
/// use oxiarc_deflate::{Deflater, RawInflateReader};
/// use tokio::io::AsyncReadExt;
///
/// # fn main() {
/// let mut deflater = Deflater::new(6);
/// let mut wire = Vec::new();
/// deflater.deflate_sync(b"* OK ready\r\n", &mut wire).expect("deflate_sync");
///
/// let runtime = tokio::runtime::Builder::new_current_thread()
///     .build()
///     .expect("runtime");
/// runtime.block_on(async move {
///     let mut reader = RawInflateReader::new(std::io::Cursor::new(wire));
///     let mut plain = Vec::new();
///     reader.read_to_end(&mut plain).await.expect("inflate");
///     assert_eq!(plain, b"* OK ready\r\n");
/// });
/// # }
/// ```
pub struct RawInflateReader<R> {
    inner: AsyncInflateReader<R>,
}

impl<R> RawInflateReader<R> {
    /// Wrap `inner` with a decompressor.
    pub fn new(inner: R) -> Self {
        Self {
            inner: AsyncInflateReader::with_core(
                inner,
                WrappedInflate::new(InflateWrapper::Raw)
                    .multi_member(false)
                    .trailing_policy(TrailingPolicy::Stop),
            )
            // RFC 4978: the peer may send more at any time, so a closed
            // connection ends the stream without being truncation.
            .with_eof_flush(FlushMode::None),
        }
    }

    /// Unwrap, returning the inner reader.
    ///
    /// Compressed bytes already staged inside the decoder are discarded.
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }

    /// Decompressed bytes produced so far.
    pub fn total_out(&self) -> u64 {
        self.inner.total_out()
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for RawInflateReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.as_mut().get_mut().inner).poll_read(cx, buf)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::Deflater;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_writer_produces_sync_flush() {
        let plain = b"Hello, COMPRESS=DEFLATE!";
        let mut buf = Vec::<u8>::new();
        {
            let mut writer = RawDeflateWriter::new(&mut buf, 6);
            writer.write_all(plain).await.unwrap();
            writer.flush().await.unwrap();
        }
        assert!(!buf.is_empty());
        // Sync flush always ends with LEN=0/NLEN=0xFFFF
        assert!(
            buf.windows(4).any(|w| w == [0x00, 0x00, 0xFF, 0xFF]),
            "sync-flush marker must be present in output"
        );
    }

    #[tokio::test]
    async fn test_reader_single_chunk() {
        let plain = b"IMAP COMPRESS=DEFLATE single-chunk test.";
        let compressed = {
            let mut d = Deflater::new(6);
            let mut c = Vec::new();
            d.deflate_sync(plain, &mut c).unwrap();
            c
        };
        let mut reader = RawInflateReader::new(std::io::Cursor::new(compressed));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        assert_eq!(&out, plain);
    }

    #[tokio::test]
    async fn test_reader_multi_chunk_lz77() {
        let plains: [&[u8]; 3] = [b"First IMAP line\r\n", b"* 42 EXISTS\r\n", b"A001 OK\r\n"];
        let compressed = {
            let mut d = Deflater::new(6);
            let mut c = Vec::new();
            for p in &plains {
                d.deflate_sync(p, &mut c).unwrap();
            }
            c
        };
        let mut reader = RawInflateReader::new(std::io::Cursor::new(compressed));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();
        let expected: Vec<u8> = plains.iter().flat_map(|s| s.iter().copied()).collect();
        assert_eq!(out, expected);
    }

    #[tokio::test]
    async fn test_writer_reader_roundtrip() {
        let messages: [&[u8]; 4] = [
            b"* OK Greetings\r\n",
            b"A001 LOGIN user password\r\n",
            b"A001 OK LOGIN completed\r\n",
            b"A002 SELECT INBOX\r\n",
        ];

        // Compress with RawDeflateWriter
        let mut compressed = Vec::<u8>::new();
        {
            let mut writer = RawDeflateWriter::new(&mut compressed, 6);
            for msg in &messages {
                writer.write_all(msg).await.unwrap();
                writer.flush().await.unwrap();
            }
        }

        // Decompress with RawInflateReader
        let mut reader = RawInflateReader::new(std::io::Cursor::new(compressed));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.unwrap();

        let expected: Vec<u8> = messages.iter().flat_map(|s| s.iter().copied()).collect();
        assert_eq!(out, expected);
    }
}

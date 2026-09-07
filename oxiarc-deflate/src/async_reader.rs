//! [`AsyncRead`] adapter over the resumable inflate core (feature `async-io`).
//!
//! [`AsyncInflateReader`] is the async twin of
//! [`InflateReader`](crate::InflateReader): the same `InflatePump`, the
//! same 64 KiB staging buffers, the same end-of-stream rules — only the
//! source reads differ. Feeding it a growing prefix of a compressed stream
//! is exactly what it is for, which is why
//! `AsyncDecompressorWrapper<Inflater>` (a whole-remaining-input contract)
//! must be replaced by this type rather than fixed.
//!
//! # Example
//!
//! ```
//! use oxiarc_deflate::{AsyncInflateReader, gzip_compress};
//! use tokio::io::AsyncReadExt;
//!
//! # fn main() {
//! let compressed = gzip_compress(b"async body", 6).expect("gzip");
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .build()
//!     .expect("runtime");
//! runtime.block_on(async move {
//!     let mut reader = AsyncInflateReader::gzip(&compressed[..]);
//!     let mut plain = Vec::new();
//!     reader.read_to_end(&mut plain).await.expect("inflate");
//!     assert_eq!(plain, b"async body");
//! });
//! # }
//! ```
//!
//! # I/O contract
//!
//! * `Poll::Pending` from the source is returned unchanged — the waker
//!   registered by the source drives the retry, and decoded bytes staged
//!   inside the adapter are always served before the source is polled, so a
//!   pending source never hides readable output.
//! * `Ok(0)` from the source switches the flush mode to
//!   [`FlushMode::Finish`](oxiarc_core::traits::FlushMode), so a truncated
//!   member is an [`io::Error`] rather than a short read.
//! * A decoder that still asks for input after that switch is
//!   [`io::ErrorKind::UnexpectedEof`] rather than a clean end of stream.
//! * A decoder that makes no progress twice in a row is an error, never a
//!   spin.
//! * Errors are sticky: a reader that has failed keeps failing, and never
//!   decays into an end-of-stream `Poll::Ready(Ok(()))` with nothing filled.
//!
//! Every one of those rules lives in the `InflatePump` this shares with
//! [`InflateReader`](crate::InflateReader), so the two adapters cannot drift
//! apart on end-of-stream semantics.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use oxiarc_core::traits::FlushMode;
use tokio::io::{AsyncRead, ReadBuf};

use crate::reader::InflatePump;
use crate::wrapper::{GzipHeaderInfo, InflateWrapper, TrailingPolicy, WrappedInflate};

/// An [`AsyncRead`] adapter that decompresses gzip, zlib or raw DEFLATE
/// from an async source.
///
/// See the [module documentation](self) for the polling contract and a
/// worked example. Defaults match
/// [`InflateReader`](crate::InflateReader): strict framing, concatenated
/// members decoded, both checksums verified.
#[derive(Debug)]
pub struct AsyncInflateReader<R> {
    inner: R,
    pump: InflatePump,
}

impl<R> AsyncInflateReader<R> {
    /// A strict reader for `wrapper`.
    ///
    /// Concatenated members are decoded for gzip and zlib framing; raw
    /// DEFLATE has no header to find a second member by, so there the
    /// setting only decides how bytes after the final block are treated
    /// (rejected by default). See
    /// [`InflateReader::new`](crate::InflateReader::new).
    pub fn new(inner: R, wrapper: InflateWrapper) -> Self {
        Self::with_core(inner, WrappedInflate::new(wrapper).multi_member(true))
    }

    /// A reader for `Content-Encoding: gzip` (and `x-gzip`).
    pub fn gzip(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Gzip)
    }

    /// A reader for zlib-framed DEFLATE (RFC 1950).
    pub fn zlib(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Zlib)
    }

    /// A reader for raw DEFLATE (RFC 1951) with no container.
    pub fn raw(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Raw)
    }

    /// A reader that picks the framing from the first two bytes.
    ///
    /// The sniff happens at offset 0 only: gzip magic wins, then a
    /// structurally valid zlib header, then raw.
    pub fn auto(inner: R) -> Self {
        Self::new(inner, InflateWrapper::Auto)
    }

    /// Wrap a fully configured decoder.
    pub(crate) fn with_core(inner: R, core: WrappedInflate) -> Self {
        Self {
            inner,
            pump: InflatePump::new(core),
        }
    }

    /// Whether a concatenated stream of members is decoded (default `true`).
    #[must_use]
    pub fn multi_member(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.multi_member(yes));
        self
    }

    /// What to do with bytes after the last member (default
    /// [`TrailingPolicy::Reject`]).
    #[must_use]
    pub fn trailing_policy(mut self, policy: TrailingPolicy) -> Self {
        self.pump.replace_core(|core| core.trailing_policy(policy));
        self
    }

    /// Fail once more than `limit` bytes would be produced.
    ///
    /// Enforced inside a DEFLATE block, so a single-block bomb is stopped
    /// at the limit rather than after full expansion.
    #[must_use]
    pub fn with_max_output(mut self, limit: u64) -> Self {
        self.pump.replace_core(|core| core.with_max_output(limit));
        self
    }

    /// Fail once the output exceeds `ratio` times the input, ignoring the
    /// first `min_output` bytes.
    #[must_use]
    pub fn with_ratio_guard(mut self, ratio: f64, min_output: u64) -> Self {
        self.pump
            .replace_core(|core| core.with_ratio_guard(ratio, min_output));
        self
    }

    /// Supply the preset dictionary a zlib `FDICT` stream asks for.
    #[must_use]
    pub fn with_dictionary(mut self, dictionary: &[u8]) -> Self {
        self.pump
            .replace_core(|core| core.with_dictionary(dictionary));
        self
    }

    /// Whether a gzip `FHCRC` header checksum is verified (default `true`).
    #[must_use]
    pub fn verify_header_crc(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.verify_header_crc(yes));
        self
    }

    /// Whether the trailer checksum is verified (default `true`).
    #[must_use]
    pub fn verify_checksum(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.verify_checksum(yes));
        self
    }

    /// Whether a bad header at offset 0 is an error (default `true`).
    #[must_use]
    pub fn strict_first_member(mut self, yes: bool) -> Self {
        self.pump.replace_core(|core| core.strict_first_member(yes));
        self
    }

    /// Never treat source EOF as the end of the compressed stream (RFC 4978).
    pub(crate) fn with_eof_flush(mut self, flush: FlushMode) -> Self {
        self.pump.set_eof_flush(flush);
        self
    }

    /// Borrow the inner reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrow the inner reader.
    ///
    /// Reading from it directly loses bytes already staged in the decoder.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Consume the adapter and return the inner reader.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// Compressed bytes consumed by the decoder so far.
    pub fn total_in(&self) -> u64 {
        self.pump.core().total_in()
    }

    /// Decompressed bytes produced so far.
    pub fn total_out(&self) -> u64 {
        self.pump.core().total_out()
    }

    /// Members fully decoded so far.
    pub fn members_decoded(&self) -> u32 {
        self.pump.core().members_decoded()
    }

    /// gzip header fields of the member most recently started.
    pub fn gzip_header(&self) -> Option<&GzipHeaderInfo> {
        self.pump.core().gzip_header()
    }

    /// Whether the stream has ended and every decoded byte has been read.
    pub fn is_finished(&self) -> bool {
        self.pump.is_finished()
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for AsyncInflateReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.as_mut().get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            // 1. Serve staged output first, so a pending source can never
            //    hide bytes that are already decoded.
            let ready = this.pump.staged().len().min(buf.remaining());
            if ready > 0 {
                if let Some(chunk) = this.pump.staged().get(..ready) {
                    buf.put_slice(chunk);
                }
                this.pump.consume_staged(ready);
                return Poll::Ready(Ok(()));
            }
            // 2. End of stream: leave `buf` untouched, which is async EOF.
            if this.pump.is_done() {
                return Poll::Ready(Ok(()));
            }
            // 3. Top up compressed input.
            if this.pump.wants_input() {
                let mut source = ReadBuf::new(this.pump.input_slot());
                match Pin::new(&mut this.inner).poll_read(cx, &mut source) {
                    Poll::Ready(Ok(())) => {
                        let filled = source.filled().len();
                        this.pump.note_read(filled);
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
                continue;
            }
            // 4. Decode one staging buffer's worth.
            if let Err(error) = this.pump.step() {
                return Poll::Ready(Err(error));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{deflate, gzip_compress, zlib_compress};
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn async_reader_round_trips_every_framing() {
        let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();

        let gzipped = gzip_compress(&payload, 6).expect("gzip");
        let mut reader = AsyncInflateReader::gzip(&gzipped[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("gzip inflate");
        assert_eq!(out, payload);
        assert_eq!(reader.total_in(), gzipped.len() as u64);

        let zlibbed = zlib_compress(&payload, 6).expect("zlib");
        let mut reader = AsyncInflateReader::zlib(&zlibbed[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("zlib inflate");
        assert_eq!(out, payload);

        let raw = deflate(&payload, 6).expect("deflate");
        let mut reader = AsyncInflateReader::raw(&raw[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("raw inflate");
        assert_eq!(out, payload);

        let mut reader = AsyncInflateReader::auto(&gzipped[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("auto inflate");
        assert_eq!(out, payload);
    }

    #[tokio::test]
    async fn async_reader_rejects_truncation() {
        let payload = vec![7u8; 60_000];
        let gzipped = gzip_compress(&payload, 6).expect("gzip");
        let cut = gzipped.len() - 40;
        let mut reader = AsyncInflateReader::gzip(&gzipped[..cut]);
        let mut out = Vec::new();
        let error = reader
            .read_to_end(&mut out)
            .await
            .expect_err("a truncated member must be an error");
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ),
            "unexpected kind {:?}",
            error.kind()
        );
    }
}

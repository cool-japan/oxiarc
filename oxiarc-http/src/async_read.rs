//! [`AsyncDecodedBody`]: the `tokio::io::AsyncRead` twin of
//! [`DecodedBody`](crate::DecodedBody) (feature `async-io`).
//!
//! # Polling contract
//!
//! * Decoded bytes already staged inside the adapter are served **before**
//!   the source is polled, so a `Poll::Pending` source can never hide
//!   readable output.
//! * `Poll::Pending` is returned unchanged; the waker the source registered
//!   drives the retry.
//! * `Ok(())` with nothing filled — async EOF — switches the flush mode to
//!   [`FlushMode::Finish`], so a truncated body is an [`io::Error`] rather
//!   than a short read, and [`Decoder::close`] then verifies every trailer
//!   before the adapter reports its own EOF.
//! * `Unpin` plus `Pin::new(&mut this.inner)` rather than a projection
//!   crate: this keeps the dependency list at `tokio` alone, exactly as
//!   `oxiarc-deflate`'s `AsyncInflateReader` does.
//!
//! For `reqwest`, the push [`Decoder`] is usually the better fit — there is
//! no `AsyncRead` to wrap, since `bytes_stream()`/`chunk()` yield `Bytes`.
//! `AsyncDecodedBody` targets `tokio::net::TcpStream`, an `oxihttp`/`hyper`
//! body adapted through `StreamReader`, and `tokio::fs::File`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use oxiarc_core::traits::FlushMode;
use tokio::io::{AsyncRead, ReadBuf};

use crate::coding::ContentCoding;
use crate::decode::{DecodeStatus, Decoder, TrailingData};
use crate::error::Result;
use crate::limits::DecodeLimits;

/// Wire bytes read per poll, and decoded bytes staged for the caller.
///
/// Mandatory, not an optimisation — see [`DecodedBody`](crate::DecodedBody).
const STAGING: usize = 64 * 1024;

/// An [`AsyncRead`] adapter that decodes a content-coded body as it is read.
///
/// Memory is bounded by the two 64 KiB staging buffers plus each codec's own
/// window, whatever the body's size or expansion ratio.
///
/// # Example
///
/// ```
/// use oxiarc_http::{AsyncDecodedBody, DecodeLimits};
/// use tokio::io::AsyncReadExt;
///
/// # fn main() {
/// let gz = oxiarc_deflate::gzip_compress(b"async body", 6).expect("compress");
/// let runtime = tokio::runtime::Builder::new_current_thread()
///     .build()
///     .expect("runtime");
/// runtime.block_on(async move {
///     let mut body = AsyncDecodedBody::new(&gz[..], "gzip", &DecodeLimits::default())
///         .expect("gzip is compiled in");
///     let mut plain = Vec::new();
///     body.read_to_end(&mut plain).await.expect("decode");
///     assert_eq!(plain, b"async body");
/// });
/// # }
/// ```
#[derive(Debug)]
pub struct AsyncDecodedBody<R> {
    inner: R,
    decoder: Decoder,
    wire: Box<[u8]>,
    wire_start: usize,
    wire_end: usize,
    out: Box<[u8]>,
    out_start: usize,
    out_end: usize,
    src_eof: bool,
    closed: bool,
}

impl<R> AsyncDecodedBody<R> {
    /// Wrap `inner`, decoding per a `Content-Encoding` header value.
    ///
    /// Pass `"identity"` (or `""`) when the response carried no such header.
    ///
    /// # Errors
    ///
    /// As [`Decoder::from_header`].
    pub fn new(inner: R, content_encoding: &str, limits: &DecodeLimits) -> Result<Self> {
        Ok(Self::with_decoder(
            inner,
            Decoder::from_header(content_encoding, limits)?,
        ))
    }

    /// Wrap `inner` with an already-parsed coding list.
    ///
    /// # Errors
    ///
    /// As [`Decoder::new`].
    pub fn with_codings(
        inner: R,
        codings: &[ContentCoding],
        limits: &DecodeLimits,
    ) -> Result<Self> {
        Ok(Self::with_decoder(inner, Decoder::new(codings, limits)?))
    }

    /// Wrap `inner` with a decoder built elsewhere.
    pub fn with_decoder(inner: R, decoder: Decoder) -> Self {
        Self {
            inner,
            decoder,
            wire: vec![0u8; STAGING].into_boxed_slice(),
            wire_start: 0,
            wire_end: 0,
            out: vec![0u8; STAGING].into_boxed_slice(),
            out_start: 0,
            out_end: 0,
            src_eof: false,
            closed: false,
        }
    }

    /// Wrap `inner` when there is no `Content-Encoding` header at all.
    pub fn identity(inner: R) -> Self {
        Self::with_decoder(inner, Decoder::identity())
    }

    /// The wrapped wire-byte reader.
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The wrapped wire-byte reader, mutably.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Unwrap, discarding any buffered decoded bytes.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The decoder driving this body.
    pub fn decoder(&self) -> &Decoder {
        &self.decoder
    }

    /// Decoded bytes produced so far.
    pub fn output_len(&self) -> u64 {
        self.decoder.output_len()
    }

    /// Whether the encoded stream ended and its trailers verified.
    pub fn is_finished(&self) -> bool {
        self.closed
    }

    /// Decode one staging buffer's worth from whatever wire bytes are held.
    ///
    /// Never touches the source: the caller polls it.
    fn step(&mut self) -> io::Result<()> {
        self.out_start = 0;
        self.out_end = 0;
        let flush = if self.src_eof {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = self.decoder.decode(
            &self.wire[self.wire_start..self.wire_end],
            &mut self.out,
            flush,
        )?;
        self.wire_start += progress.consumed;
        self.out_end = progress.produced;
        if progress.produced > 0 {
            return Ok(());
        }
        match progress.status {
            // Reaching the end of the *coded* stream is not the same
            // as reaching the end of the body's bytes. Closing here while
            // the source may still hold data would make the trailing-data
            // verdict depend on where the read boundary happened to fall:
            // the four codings differ in whether they need a further byte
            // to report `StreamEnd` at all, so `XXXX` appended to a `br`
            // body was rejected when the stream and the garbage landed in
            // one read and silently ACCEPTED when the body's last byte
            // arrived on its own. Under a policy that inspects what
            // follows, keep the body open until the source is spent — the
            // next round refills from the source first, so it is not the
            // repeat of an identical round the no-progress guard below
            // exists to catch — and hand whatever arrives to the finished
            // decoder, which applies `check_trailing` to it. Under
            // `TrailingData::Ignore` there is nothing to check, so close
            // at once and never read a byte the caller did not ask for.
            DecodeStatus::StreamEnd
                if self.src_eof || self.decoder.trailing_policy() == TrailingData::Ignore =>
            {
                self.decoder.close()?;
                self.closed = true;
            }
            DecodeStatus::StreamEnd => {}
            // The source is spent and the stream never ended: `close`
            // turns that into the truncation error it is.
            DecodeStatus::NeedInput if self.src_eof => {
                self.decoder.close()?;
                self.closed = true;
            }
            // The staging buffer is 64 KiB, so `NeedOutput` always comes with
            // output for every coding this crate ships. Fall through to the
            // no-progress guard below, which is what stops a stage that
            // claims otherwise from spinning inside a single `poll_read`.
            _ => {}
        }

        // `poll_read` only tops the wire buffer up when it is empty, so a
        // round that consumed nothing and produced nothing would be repeated
        // verbatim, forever, inside one poll — starving the whole executor
        // thread rather than merely failing this body. Report it instead.
        //
        // Not when the body just closed: the last round of a clean decode is
        // legitimately `0/0`, because everything was already handed over and
        // all that remained was to verify the trailers.
        // Nor when the stream has ended and the body is deliberately still
        // open (above): the next round polls the source rather than repeating
        // this one.
        if !self.closed
            && progress.consumed == 0
            && progress.produced == 0
            && progress.status != DecodeStatus::StreamEnd
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "content-coding decoder made no progress: it neither consumed wire bytes \
                 nor produced decoded bytes with a full staging buffer available",
            ));
        }
        Ok(())
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for AsyncDecodedBody<R> {
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
            let ready = (this.out_end - this.out_start).min(buf.remaining());
            if ready > 0 {
                buf.put_slice(&this.out[this.out_start..this.out_start + ready]);
                this.out_start += ready;
                return Poll::Ready(Ok(()));
            }
            // 2. End of body: leave `buf` untouched, which is async EOF.
            if this.closed {
                return Poll::Ready(Ok(()));
            }
            // 3. Top up wire bytes when the buffer is dry and the source is
            //    still live.
            if this.wire_start == this.wire_end && !this.src_eof {
                this.wire_start = 0;
                this.wire_end = 0;
                let mut source = ReadBuf::new(&mut this.wire);
                match Pin::new(&mut this.inner).poll_read(cx, &mut source) {
                    Poll::Ready(Ok(())) => {
                        let filled = source.filled().len();
                        if filled == 0 {
                            this.src_eof = true;
                        } else {
                            this.wire_end = filled;
                        }
                    }
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => return Poll::Pending,
                }
                continue;
            }
            // 4. Decode one staging buffer's worth.
            if let Err(error) = this.step() {
                return Poll::Ready(Err(error));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
    }

    #[test]
    fn identity_body_round_trips() {
        runtime().block_on(async {
            let mut body = AsyncDecodedBody::identity(&b"plain bytes"[..]);
            let mut out = Vec::new();
            body.read_to_end(&mut out).await.expect("identity");
            assert_eq!(out, b"plain bytes");
            assert!(body.is_finished());
        });
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_body_round_trips() {
        let payload: Vec<u8> = (0..90_000u32).map(|i| (i % 251) as u8).collect();
        let gz = oxiarc_deflate::gzip_compress(&payload, 6).expect("compress");
        runtime().block_on(async {
            let mut body =
                AsyncDecodedBody::new(&gz[..], "gzip", &DecodeLimits::default()).expect("gzip");
            let mut out = Vec::new();
            body.read_to_end(&mut out).await.expect("decode");
            assert_eq!(out, payload);
            assert_eq!(body.output_len(), payload.len() as u64);
            assert!(body.decoder().is_finished());
        });
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn truncated_gzip_body_is_an_error() {
        let gz = oxiarc_deflate::gzip_compress(&vec![b'x'; 4096], 6).expect("compress");
        runtime().block_on(async {
            let mut body =
                AsyncDecodedBody::new(&gz[..gz.len() - 5], "gzip", &DecodeLimits::default())
                    .expect("gzip");
            let mut out = Vec::new();
            let err = body
                .read_to_end(&mut out)
                .await
                .expect_err("a truncated body must not read short");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        });
    }

    /// The async twin of `read.rs`'s guard test: a stage that claims pending
    /// output and never writes must not spin inside one `poll_read`, which
    /// would starve the executor thread rather than merely fail this body.
    struct NeverProgresses;

    impl crate::decode::coding::CodingDecoder for NeverProgresses {
        fn decode(
            &mut self,
            _input: &[u8],
            _output: &mut [u8],
            _flush: FlushMode,
        ) -> Result<crate::decode::coding::CodingProgress> {
            Ok(crate::decode::coding::CodingProgress {
                consumed: 0,
                produced: 0,
                status: crate::decode::coding::CodingStatus::NeedOutput,
            })
        }

        fn finish(&mut self) -> Result<()> {
            Ok(())
        }

        fn coding(&self) -> &ContentCoding {
            static IDENTITY: ContentCoding = ContentCoding::Identity;
            &IDENTITY
        }
    }

    #[test]
    fn a_stage_that_never_progresses_is_an_error_not_a_hang() {
        runtime().block_on(async {
            let decoder = Decoder::from_stage(Box::new(NeverProgresses), &DecodeLimits::default());
            let mut body =
                AsyncDecodedBody::with_decoder(&b"wire bytes that go nowhere"[..], decoder);
            let mut out = Vec::new();
            let error = body
                .read_to_end(&mut out)
                .await
                .expect_err("a stage that makes no progress must be reported, not looped on");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("made no progress"));
        });
    }

    #[test]
    fn accessors_expose_the_inner_reader() {
        let body = AsyncDecodedBody::identity(&b"abc"[..]);
        assert_eq!(body.get_ref().len(), 3);
        let mut body = body;
        assert_eq!(body.get_mut().len(), 3);
        assert!(body.decoder().codings().is_empty());
        assert_eq!(body.into_inner().len(), 3);
    }
}

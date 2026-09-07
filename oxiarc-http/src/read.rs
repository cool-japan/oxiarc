//! [`DecodedBody`]: a `Read`/`BufRead` adapter that decodes a content-coded
//! response body on the fly.

use std::io::{self, BufRead, Read};

use oxiarc_core::traits::FlushMode;

use crate::coding::ContentCoding;
use crate::decode::{DecodeStatus, Decoder};
use crate::error::Result;
use crate::limits::DecodeLimits;

/// Wire bytes read from the source per round, and decoded bytes staged for
/// the caller.
///
/// 64 KiB each, and **mandatory, not an optimisation**: a caller reading
/// three bytes at a time (`BufReader`'s `read_line` does exactly that in the
/// worst case) would otherwise cost one sliding-window update per three
/// bytes. Both buffers are allocated once, at construction.
const STAGING: usize = 64 * 1024;

/// A `Read` adapter that decodes a content-coded body as it is read.
///
/// Wrap a reader of **wire** bytes — `ureq`'s `BodyReader`, a
/// `std::net::TcpStream`, a `File` of a captured response — and read decoded
/// bytes out. Memory is bounded by the two 64 KiB staging buffers plus each
/// codec's own window, whatever the body's size or expansion ratio.
///
/// # End of body
///
/// Reaching EOF on the inner reader ends the encoded stream, and
/// [`Decoder::close`] then runs automatically: checksums, trailers and
/// truncation are verified **before** the final `Ok(0)`. A body that is
/// truncated, corrupt, over-budget or followed by trailing garbage
/// therefore surfaces as an `io::Error` from the last `read` call, never as
/// a silently short result. `io::ErrorKind::InvalidData` wraps the
/// underlying [`HttpCodingError`](crate::HttpCodingError); recover it with
/// [`io::Error::get_ref`] and `downcast_ref`.
///
/// # `BufRead`
///
/// The [`BufRead`] impl is real, not a `BufReader` wrapper: `fill_buf`
/// exposes the decoded staging buffer directly. That makes `.lines()` work
/// on a gzip body, which is what an NDJSON or log endpoint needs.
///
/// # Example
///
/// ```
/// use oxiarc_http::{DecodeLimits, DecodedBody};
///
/// let gz = oxiarc_deflate::gzip_compress(b"hello, world", 6).expect("compress");
/// let mut body = DecodedBody::new(&gz[..], "gzip", &DecodeLimits::default())
///     .expect("gzip is compiled in");
///
/// let text = body.read_to_string().expect("decode");
/// assert_eq!(text, "hello, world");
/// ```
#[derive(Debug)]
pub struct DecodedBody<R> {
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

impl<R: Read> DecodedBody<R> {
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
    ///
    /// The way to use a non-default [`TrailingData`](crate::TrailingData)
    /// policy or a `dcz` shared dictionary.
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

    /// Read the whole body into a `Vec`, bounded by the decoder's
    /// [`DecodeLimits::max_output`].
    ///
    /// The convenience form of [`Read::read_to_end`] that hands back the
    /// `Vec` instead of filling one. Growth is the same amortised doubling
    /// either way — `max_output` is what bounds the result, not the
    /// buffering strategy — but every growth here goes through
    /// [`Vec::try_reserve`], so an allocation failure on a large body is a
    /// reported `io::Error` rather than an abort.
    ///
    /// # Errors
    ///
    /// Any I/O error from the inner reader, a decode failure as
    /// `io::ErrorKind::InvalidData`, or `io::ErrorKind::OutOfMemory` if the
    /// buffer cannot grow.
    pub fn read_to_vec(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let chunk = self.fill_buf()?;
            if chunk.is_empty() {
                return Ok(out);
            }
            let n = chunk.len();
            if out.try_reserve(n).is_err() {
                return Err(io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "cannot grow the decoded-body buffer",
                ));
            }
            out.extend_from_slice(chunk);
            self.consume(n);
        }
    }

    /// [`read_to_vec`](Self::read_to_vec), then validate UTF-8.
    ///
    /// # This shadows [`Read::read_to_string`]
    ///
    /// An inherent method wins over a trait method, so `body.read_to_string()`
    /// is *this* one — it takes no argument and hands back the `String`,
    /// matching `ureq`'s `Body::read_to_string()` and the design report's
    /// shape. The `std` trait method, which appends into a `&mut String`, is
    /// still reachable as `Read::read_to_string(&mut body, &mut buffer)`.
    /// The mismatch is a compile error, never a silent change of meaning.
    ///
    /// # Errors
    ///
    /// As [`read_to_vec`](Self::read_to_vec), plus
    /// `io::ErrorKind::InvalidData` when the decoded body is not UTF-8.
    pub fn read_to_string(&mut self) -> io::Result<String> {
        let bytes = self.read_to_vec()?;
        String::from_utf8(bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.utf8_error()))
    }

    /// Refill the decoded staging buffer, if it is empty and more is coming.
    fn fill(&mut self) -> io::Result<()> {
        while self.out_start == self.out_end {
            if self.closed {
                return Ok(());
            }
            self.out_start = 0;
            self.out_end = 0;

            if self.wire_start == self.wire_end && !self.src_eof {
                self.wire_start = 0;
                self.wire_end = 0;
                loop {
                    match self.inner.read(&mut self.wire) {
                        Ok(0) => {
                            self.src_eof = true;
                            break;
                        }
                        Ok(n) => {
                            self.wire_end = n;
                            break;
                        }
                        // A signal interrupted the read: retry, as every
                        // well-behaved `Read` consumer must.
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        // Everything else, `WouldBlock` included, propagates
                        // verbatim. Turning `WouldBlock` into `Ok(0)` would
                        // report a non-blocking socket's empty moment as end
                        // of body — the exact defect the audit found in
                        // `Lzma2StreamDecoder`.
                        Err(e) => return Err(e),
                    }
                }
            }

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
                DecodeStatus::StreamEnd => {
                    self.decoder.close()?;
                    self.closed = true;
                    return Ok(());
                }
                // The source is spent and the stream never ended:
                // `close` turns that into the truncation error it is.
                DecodeStatus::NeedInput if self.src_eof => {
                    self.decoder.close()?;
                    self.closed = true;
                    return Ok(());
                }
                // The staging buffer is 64 KiB, so `NeedOutput` always comes
                // with output for every coding this crate ships. Fall
                // through to the no-progress guard below, which is what
                // stops a stage that claims otherwise from spinning here.
                _ => {}
            }

            // A round that consumed nothing and produced nothing cannot be
            // followed by a different one: the source is only polled when
            // the wire buffer is empty, so the next pass would hand the
            // decoder exactly the same bytes and get exactly the same
            // answer. Report it instead of looping forever — a read loop
            // that never returns is worse than a failed response, and a
            // hostile body must not be able to hang a client.
            if progress.consumed == 0 && progress.produced == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "content-coding decoder made no progress: it neither consumed wire bytes \
                     nor produced decoded bytes with a full staging buffer available",
                ));
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for DecodedBody<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.fill()?;
        let available = self.out_end - self.out_start;
        let n = available.min(buf.len());
        buf[..n].copy_from_slice(&self.out[self.out_start..self.out_start + n]);
        self.out_start += n;
        Ok(n)
    }
}

impl<R: Read> BufRead for DecodedBody<R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.fill()?;
        Ok(&self.out[self.out_start..self.out_end])
    }

    fn consume(&mut self, amt: usize) {
        self.out_start = (self.out_start + amt).min(self.out_end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A reader that hands out at most `step` bytes per call and injects an
    /// `Interrupted` before every real read, so the adapter's retry loop is
    /// exercised on every fixture rather than only in a dedicated test.
    #[cfg(feature = "gzip")]
    struct Trickle<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
        interrupt_next: bool,
    }

    #[cfg(feature = "gzip")]
    impl<'a> Trickle<'a> {
        fn new(data: &'a [u8], step: usize) -> Self {
            Self {
                data,
                pos: 0,
                step,
                interrupt_next: true,
            }
        }
    }

    #[cfg(feature = "gzip")]
    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.interrupt_next = true;
            let n = self.step.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn identity_body_round_trips() {
        let mut body = DecodedBody::identity(&b"plain bytes"[..]);
        let out = body.read_to_vec().expect("identity");
        assert_eq!(out, b"plain bytes");
        assert!(body.is_finished());
        assert_eq!(body.output_len(), 11);
    }

    #[test]
    fn empty_identity_body_is_empty_not_an_error() {
        let mut body = DecodedBody::identity(&b""[..]);
        assert_eq!(body.read_to_vec().expect("identity"), b"");
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_body_survives_a_trickling_interrupting_reader() {
        let payload: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let gz = oxiarc_deflate::gzip_compress(&payload, 6).expect("compress");
        let mut body = DecodedBody::with_decoder(
            Trickle::new(&gz, 3),
            Decoder::from_header("gzip", &DecodeLimits::default()).expect("gzip"),
        );
        assert_eq!(body.read_to_vec().expect("decode"), payload);
        assert!(body.is_finished());
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn truncated_gzip_body_is_an_io_error() {
        let gz = oxiarc_deflate::gzip_compress(&vec![b'x'; 4096], 6).expect("compress");
        let mut body =
            DecodedBody::new(&gz[..gz.len() - 5], "gzip", &DecodeLimits::default()).expect("gzip");
        let err = body
            .read_to_vec()
            .expect_err("a truncated body must not read short");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn buf_read_gives_lines_over_a_gzip_body() {
        let text = "alpha\nbeta\ngamma\n";
        let gz = oxiarc_deflate::gzip_compress(text.as_bytes(), 6).expect("compress");
        let body = DecodedBody::new(&gz[..], "gzip", &DecodeLimits::default()).expect("gzip");
        let lines: Vec<String> = body.lines().map(|l| l.expect("line")).collect();
        assert_eq!(lines, ["alpha", "beta", "gamma"]);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn would_block_is_propagated_never_reported_as_end_of_body() {
        struct Blocking;
        impl Read for Blocking {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
        }
        let mut body = DecodedBody::new(Blocking, "gzip", &DecodeLimits::default()).expect("gzip");
        let mut buf = [0u8; 16];
        let err = body.read(&mut buf).expect_err("WouldBlock must propagate");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    /// A stage that always claims "more output pending" and never writes a
    /// byte. No shipped codec behaves this way; the point is that if one ever
    /// did — or a future refactor made one — the adapter must report it, not
    /// spin forever inside a single `read`.
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
        let decoder = Decoder::from_stage(Box::new(NeverProgresses), &DecodeLimits::default());
        let mut body = DecodedBody::with_decoder(&b"wire bytes that go nowhere"[..], decoder);
        let mut buf = [0u8; 32];
        let error = body
            .read(&mut buf)
            .expect_err("a stage that makes no progress must be reported, not looped on");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("made no progress"));
    }

    #[test]
    fn accessors_expose_the_inner_reader() {
        let body = DecodedBody::identity(&b"abc"[..]);
        assert_eq!(body.get_ref().len(), 3);
        let mut body = body;
        assert_eq!(body.get_mut().len(), 3);
        assert!(body.decoder().codings().is_empty());
        assert_eq!(body.into_inner().len(), 3);
    }
}

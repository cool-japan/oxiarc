//! Server-side response encoding: [`encode_body`] (one-shot) and
//! [`Encoder`] (streaming).

use std::io::{self, Write};

use crate::coding::{ContentCoding, unsupported_coding_error};
use crate::error::Result;
// Kept imported (rather than named through a full path at each use site) so
// the rustdoc intra-doc links throughout this file resolve, and the
// per-codec-feature `cfg` blocks below that construct it read naturally.
// It is genuinely unused only when every one of `gzip`/`deflate`/`brotli`/
// `zstd` is off, in which case every arm falls through to
// `unsupported_coding_error` instead.
#[allow(unused_imports)]
use crate::error::HttpCodingError;

/// Options for encoding a response body.
///
/// Borrows its (optional) dictionary rather than owning it, so the type
/// stays [`Copy`] and a caller can reuse one `EncodeOptions` across many
/// [`encode_body`]/[`Encoder::new`] calls without cloning anything.
///
/// # Construction
///
/// This struct is `#[non_exhaustive]`, so a *downstream* crate cannot build
/// one with struct-expression syntax (not even `..EncodeOptions::default()`
/// — `#[non_exhaustive]` rejects that form outright, and only code inside
/// `oxiarc-http` itself is exempt). Use [`new`](Self::new) or
/// [`default`](Default::default) plus the `with_*` builders below, exactly
/// as with [`DecodeLimits`](crate::DecodeLimits); the fields stay `pub` for
/// reading and for in-place tweaks, but the builders are the supported —
/// and the only warning-free — way to set them from outside this crate.
///
/// ```
/// use oxiarc_http::EncodeOptions;
///
/// let dictionary = b"a shared dictionary both ends already have";
/// let opts = EncodeOptions::new()
///     .with_level(9)
///     .with_brotli_quality(5)
///     .with_zstd_level(10)
///     .with_dictionary(Some(dictionary));
///
/// assert_eq!(opts.level, 9);
/// assert_eq!(opts.brotli_quality, 5);
/// assert_eq!(opts.zstd_level, 10);
/// assert_eq!(opts.dictionary, Some(&dictionary[..]));
/// ```
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct EncodeOptions<'a> {
    /// DEFLATE-family (gzip, deflate) level, `0..=9`. Default **6**.
    ///
    /// A larger value is **clamped** to 9 by `oxiarc-deflate` itself
    /// (`Deflater::new`/`Lz77Encoder::with_level` both `min(9)`), so an
    /// out-of-range level never fails — it silently means "maximum".
    pub level: u8,
    /// Brotli quality, `0..=11`. Default **4** — quality 11 is far too slow
    /// for per-response encoding; 4 is a typical server-side choice.
    ///
    /// Unlike [`level`](Self::level) and [`zstd_level`](Self::zstd_level),
    /// this one is **not** clamped: `oxiarc-brotli` validates its own
    /// parameter range and rejects anything above 11, which surfaces here
    /// as [`HttpCodingError::Corrupt`] naming the offending value. That
    /// asymmetry is each codec crate's own policy, deliberately not
    /// papered over by this dispatch layer — silently turning a typo'd
    /// `20` into quality 11 would make every response pay the slowest
    /// brotli setting there is.
    pub brotli_quality: u32,
    /// Zstd level. Default **3**.
    ///
    /// Clamped to `0..=22` by `oxiarc-zstd` itself (`ZstdEncoder::set_level`),
    /// so, as with [`level`](Self::level), an out-of-range value never
    /// fails.
    pub zstd_level: i32,
    /// Shared dictionary for [`ContentCoding::Dcz`] (RFC 9842 Compression
    /// Dictionary Transport, zstd variant). Ignored by every other coding.
    /// `None` makes `Dcz` fail with
    /// [`HttpCodingError::MissingDictionary`] — see that coding's docs for
    /// why `Dcz`, unlike `Dcb`, is implemented at all.
    pub dictionary: Option<&'a [u8]>,
}

impl Default for EncodeOptions<'_> {
    fn default() -> Self {
        Self {
            level: 6,
            brotli_quality: 4,
            zstd_level: 3,
            dictionary: None,
        }
    }
}

impl<'a> EncodeOptions<'a> {
    /// The default options: `level: 6`, `brotli_quality: 4`,
    /// `zstd_level: 3`, no dictionary. Identical to
    /// [`Default::default`], spelled as a constructor so the `with_*`
    /// builders read as one chain.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`level`](Self::level), the DEFLATE-family (gzip, deflate)
    /// compression level.
    #[must_use]
    pub fn with_level(mut self, level: u8) -> Self {
        self.level = level;
        self
    }

    /// Set [`brotli_quality`](Self::brotli_quality).
    #[must_use]
    pub fn with_brotli_quality(mut self, quality: u32) -> Self {
        self.brotli_quality = quality;
        self
    }

    /// Set [`zstd_level`](Self::zstd_level).
    #[must_use]
    pub fn with_zstd_level(mut self, level: i32) -> Self {
        self.zstd_level = level;
        self
    }

    /// Set (or, with `None`, clear) the [`dictionary`](Self::dictionary)
    /// used by [`ContentCoding::Dcz`].
    ///
    /// Takes an `Option` rather than a bare slice so the dictionary can be
    /// cleared again, mirroring
    /// [`DecodeLimits::with_max_ratio`](crate::DecodeLimits::with_max_ratio).
    ///
    /// ```
    /// # #[cfg(feature = "zstd")] {
    /// use oxiarc_http::{ContentCoding, EncodeOptions, encode_body};
    ///
    /// let dictionary = b"a shared dictionary both ends already have";
    /// let body = b"a small body that leans on the shared dictionary";
    ///
    /// // `dcz` (RFC 9842) is encodable only when a dictionary is supplied.
    /// let with_dict = EncodeOptions::new().with_dictionary(Some(dictionary));
    /// assert!(encode_body(&ContentCoding::Dcz, body, with_dict).is_ok());
    /// assert!(encode_body(&ContentCoding::Dcz, body, EncodeOptions::new()).is_err());
    /// # }
    /// ```
    #[must_use]
    pub fn with_dictionary(mut self, dictionary: Option<&'a [u8]>) -> Self {
        self.dictionary = dictionary;
        self
    }
}

/// Wrap an underlying codec error as [`HttpCodingError::Corrupt`] (the name
/// covers both encode- and decode-time codec failures; see that variant's
/// docs).
///
/// Only referenced by the codec-specific match arms below, each gated on
/// its own Cargo feature; with every one of `gzip`/`deflate`/`brotli`/`zstd`
/// off, nothing calls it, hence the matching `cfg`.
#[cfg(any(
    feature = "gzip",
    feature = "deflate",
    feature = "brotli",
    feature = "zstd"
))]
fn wrap_codec_error(
    coding: &ContentCoding,
    source: impl std::error::Error + Send + Sync + 'static,
) -> HttpCodingError {
    HttpCodingError::Corrupt {
        coding: coding.clone(),
        source: Box::new(source),
    }
}

/// Encode a response body with one content coding.
///
/// [`ContentCoding::Identity`] is a trivial copy (no transformation). Every
/// coding this build cannot produce — including
/// [`Compress`](ContentCoding::Compress), [`Dcb`](ContentCoding::Dcb), and
/// [`Unknown`](ContentCoding::Unknown) unconditionally, and any coding whose
/// Cargo feature is off — fails with
/// [`HttpCodingError::UnsupportedCoding`]; check
/// [`ContentCoding::is_encodable`] first if you need to decide that ahead of
/// time.
///
/// # The caller's header obligations
///
/// This function only produces bytes. On success, the caller must still:
/// 1. Set `Content-Encoding: {coding.as_str()}`.
/// 2. Update (or remove) `Content-Length` to match the returned length.
/// 3. Add `Accept-Encoding` to the response's `Vary` header — omitting this
///    is a real, damaging cache-poisoning bug (a shared cache that doesn't
///    vary on it can serve a compressed body to a client that never asked
///    for one).
///
/// # Errors
/// [`HttpCodingError::UnsupportedCoding`] if this build cannot produce
/// `coding`; [`HttpCodingError::MissingDictionary`] for
/// [`ContentCoding::Dcz`] without `opts.dictionary`;
/// [`HttpCodingError::Corrupt`] if the underlying codec itself reports an
/// error (out-of-range parameters aside, this should not normally happen for
/// well-formed input).
///
/// # Examples
/// ```
/// # #[cfg(feature = "gzip")] {
/// use oxiarc_http::{ContentCoding, EncodeOptions, encode_body};
///
/// let body = b"hello, hello, hello!";
/// let encoded = encode_body(&ContentCoding::Gzip, body, EncodeOptions::default())
///     .expect("gzip is always encodable with the default features");
/// assert_ne!(encoded, body); // it's compressed
/// # }
/// ```
pub fn encode_body(
    coding: &ContentCoding,
    body: &[u8],
    opts: EncodeOptions<'_>,
) -> Result<Vec<u8>> {
    // `opts` is unused only when every codec feature is off, in which case
    // every arm below falls through to `unsupported_coding_error`.
    let _ = &opts;
    match coding {
        ContentCoding::Identity => Ok(body.to_vec()),

        #[cfg(feature = "gzip")]
        ContentCoding::Gzip => {
            oxiarc_deflate::gzip_compress(body, opts.level).map_err(|e| wrap_codec_error(coding, e))
        }
        #[cfg(not(feature = "gzip"))]
        ContentCoding::Gzip => Err(unsupported_coding_error(coding)),

        #[cfg(feature = "deflate")]
        ContentCoding::Deflate => {
            oxiarc_deflate::zlib_compress(body, opts.level).map_err(|e| wrap_codec_error(coding, e))
        }
        #[cfg(not(feature = "deflate"))]
        ContentCoding::Deflate => Err(unsupported_coding_error(coding)),

        #[cfg(feature = "brotli")]
        ContentCoding::Brotli => oxiarc_brotli::compress(body, opts.brotli_quality)
            .map_err(|e| wrap_codec_error(coding, e)),
        #[cfg(not(feature = "brotli"))]
        ContentCoding::Brotli => Err(unsupported_coding_error(coding)),

        #[cfg(feature = "zstd")]
        ContentCoding::Zstd => oxiarc_zstd::compress_with_level(body, opts.zstd_level)
            .map_err(|e| wrap_codec_error(coding, e)),
        #[cfg(not(feature = "zstd"))]
        ContentCoding::Zstd => Err(unsupported_coding_error(coding)),

        #[cfg(feature = "zstd")]
        ContentCoding::Dcz => {
            let Some(dictionary) = opts.dictionary else {
                return Err(HttpCodingError::MissingDictionary {
                    coding: coding.clone(),
                });
            };
            let mut encoder = oxiarc_zstd::ZstdEncoder::new();
            encoder.set_level(opts.zstd_level);
            encoder.set_dictionary(dictionary);
            encoder
                .compress(body)
                .map_err(|e| wrap_codec_error(coding, e))
        }
        #[cfg(not(feature = "zstd"))]
        ContentCoding::Dcz => Err(unsupported_coding_error(coding)),

        ContentCoding::Compress | ContentCoding::Dcb | ContentCoding::Unknown(_) => {
            Err(unsupported_coding_error(coding))
        }
    }
}

/// A streaming, incremental response encoder: wraps a writer `W` and applies
/// `coding` to every byte written through it.
///
/// Mirrors the shape of the sibling codec crates' own `Write` wrappers
/// (`oxiarc_deflate::GzipStreamEncoder`, `ZlibStreamEncoder`,
/// `oxiarc_brotli::BrotliCompressor`, `oxiarc_zstd::ZstdStreamEncoder`) —
/// this type is a thin, coding-selecting dispatch over exactly those, not a
/// reimplementation.
///
/// **[`finish`](Self::finish) is mandatory**: dropping an `Encoder` without
/// calling it may leave the compressed stream unterminated (missing final
/// blocks, trailers, or checksums), matching every wrapped encoder's own
/// contract.
///
/// # Framing, [`flush`](std::io::Write::flush), and the zstd multi-frame rule
///
/// The dispatch is uniform; the *framing* underneath it is not, because each
/// wrapped encoder has its own idea of what an incremental boundary is. This
/// matters for what a peer needs in order to decode the result, so it is
/// spelled out rather than left to the sibling crates' docs:
///
/// | Coding | What `flush()` emits | Resulting stream |
/// |---|---|---|
/// | `identity` | nothing of its own; flushes `W` | the bytes as written |
/// | `gzip` | a DEFLATE sync flush inside the **same** gzip member (~5 bytes) | one gzip member |
/// | `deflate` | a DEFLATE sync flush inside the **same** zlib stream | one zlib stream |
/// | `br` | every complete meta-block; a ≤7-bit residue can stay buffered until the next block or `finish` | one brotli stream |
/// | `zstd` / `dcz` | a **complete Zstandard frame** | a *sequence* of frames |
///
/// So for gzip, deflate and brotli, `flush()` is cheap and invisible to the
/// peer — the output is still one member/stream, and a plain one-shot
/// decoder handles it.
///
/// **Zstd is different, and it is not only about `flush`.**
/// `oxiarc_zstd::ZstdStreamEncoder` closes a frame every `flush()` *and*
/// automatically every 128 KiB of buffered input, so **any** body that is
/// streamed in chunks past that boundary — no explicit `flush()` required —
/// comes out as several concatenated frames. That is a perfectly legal
/// Zstandard stream (RFC 8878 §3: "multiple frames can be appended"), and it
/// is what makes bounded-memory zstd streaming possible at all, but the peer
/// must use a **multi-frame-capable** decoder. A single-frame entry point
/// stops after the first frame: `oxiarc_zstd::decompress` on such a body
/// returns `Ok` with only the first frame's bytes — a *silent truncation*,
/// not an error. Decode with `oxiarc_zstd::decompress_multi_frame` (or
/// `decompress_multi_frame_with_dict` for `dcz`, or this crate's future
/// `Decoder`, which will handle it), never with the single-frame one.
///
/// [`encode_body`] does **not** have this property: it compresses in one
/// shot and always emits exactly one frame. If a single-frame body matters
/// more than bounded memory, use it instead of `Encoder`.
///
/// Every non-identity variant boxes its inner encoder: with only one codec
/// feature enabled, `Identity(W)` (as small as `W` itself) would otherwise
/// sit next to a ~230-byte codec state struct in the same enum, tripping
/// `clippy::large_enum_variant` and wasting that much stack on every
/// `Encoder`, most of it padding for a coding that isn't in use.
#[non_exhaustive]
pub enum Encoder<W: Write> {
    /// [`ContentCoding::Identity`] — writes straight through, unmodified.
    Identity(W),
    /// [`ContentCoding::Gzip`].
    #[cfg(feature = "gzip")]
    Gzip(Box<oxiarc_deflate::GzipStreamEncoder<W>>),
    /// [`ContentCoding::Deflate`].
    #[cfg(feature = "deflate")]
    Deflate(Box<oxiarc_deflate::ZlibStreamEncoder<W>>),
    /// [`ContentCoding::Brotli`].
    #[cfg(feature = "brotli")]
    Brotli(Box<oxiarc_brotli::BrotliCompressor<W>>),
    /// [`ContentCoding::Zstd`] and, with a dictionary, [`ContentCoding::Dcz`].
    #[cfg(feature = "zstd")]
    Zstd(Box<oxiarc_zstd::ZstdStreamEncoder<W>>),
}

impl<W: Write> Encoder<W> {
    /// Start a new streaming encoder over `writer` for `coding`.
    ///
    /// Same coverage and errors as [`encode_body`], with one addition:
    /// [`ContentCoding::Dcz`] streams through `oxiarc_zstd`'s own
    /// dictionary-aware streaming encoder (`ZstdStreamEncoder::with_dictionary`)
    /// rather than the one-shot API `encode_body` uses, so a dictionary
    /// passed here is copied once up front (the streaming encoder must hold
    /// it for the writer's whole lifetime) rather than merely borrowed for
    /// one call.
    pub fn new(writer: W, coding: &ContentCoding, opts: EncodeOptions<'_>) -> Result<Self> {
        // See the matching comment in `encode_body`.
        let _ = &opts;
        match coding {
            ContentCoding::Identity => Ok(Self::Identity(writer)),

            #[cfg(feature = "gzip")]
            ContentCoding::Gzip => Ok(Self::Gzip(Box::new(
                oxiarc_deflate::GzipStreamEncoder::new(writer, opts.level),
            ))),
            #[cfg(not(feature = "gzip"))]
            ContentCoding::Gzip => Err(unsupported_coding_error(coding)),

            #[cfg(feature = "deflate")]
            ContentCoding::Deflate => Ok(Self::Deflate(Box::new(
                oxiarc_deflate::ZlibStreamEncoder::new(writer, opts.level),
            ))),
            #[cfg(not(feature = "deflate"))]
            ContentCoding::Deflate => Err(unsupported_coding_error(coding)),

            #[cfg(feature = "brotli")]
            ContentCoding::Brotli => {
                let params = oxiarc_brotli::BrotliParams {
                    quality: opts.brotli_quality,
                    ..oxiarc_brotli::BrotliParams::default()
                };
                Ok(Self::Brotli(Box::new(
                    oxiarc_brotli::BrotliCompressor::new(writer, params),
                )))
            }
            #[cfg(not(feature = "brotli"))]
            ContentCoding::Brotli => Err(unsupported_coding_error(coding)),

            #[cfg(feature = "zstd")]
            ContentCoding::Zstd => Ok(Self::Zstd(Box::new(oxiarc_zstd::ZstdStreamEncoder::new(
                writer,
                opts.zstd_level,
            )))),
            #[cfg(not(feature = "zstd"))]
            ContentCoding::Zstd => Err(unsupported_coding_error(coding)),

            #[cfg(feature = "zstd")]
            ContentCoding::Dcz => {
                let Some(dictionary) = opts.dictionary else {
                    return Err(HttpCodingError::MissingDictionary {
                        coding: coding.clone(),
                    });
                };
                Ok(Self::Zstd(Box::new(
                    oxiarc_zstd::ZstdStreamEncoder::with_dictionary(
                        writer,
                        opts.zstd_level,
                        dictionary.to_vec(),
                    ),
                )))
            }
            #[cfg(not(feature = "zstd"))]
            ContentCoding::Dcz => Err(unsupported_coding_error(coding)),

            ContentCoding::Compress | ContentCoding::Dcb | ContentCoding::Unknown(_) => {
                Err(unsupported_coding_error(coding))
            }
        }
    }

    /// Finish the stream and return the underlying writer.
    ///
    /// See the type docs: for every non-identity coding, this writes final
    /// framing (and, for gzip, the trailer checksum) that a bare `Drop`
    /// cannot.
    pub fn finish(self) -> io::Result<W> {
        match self {
            Self::Identity(w) => Ok(w),
            #[cfg(feature = "gzip")]
            Self::Gzip(e) => e.finish(),
            #[cfg(feature = "deflate")]
            Self::Deflate(e) => e.finish(),
            #[cfg(feature = "brotli")]
            Self::Brotli(e) => e.finish(),
            #[cfg(feature = "zstd")]
            Self::Zstd(e) => e.finish(),
        }
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Identity(w) => w.write(buf),
            #[cfg(feature = "gzip")]
            Self::Gzip(e) => e.write(buf),
            #[cfg(feature = "deflate")]
            Self::Deflate(e) => e.write(buf),
            #[cfg(feature = "brotli")]
            Self::Brotli(e) => e.write(buf),
            #[cfg(feature = "zstd")]
            Self::Zstd(e) => e.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Identity(w) => w.flush(),
            #[cfg(feature = "gzip")]
            Self::Gzip(e) => e.flush(),
            #[cfg(feature = "deflate")]
            Self::Deflate(e) => e.flush(),
            #[cfg(feature = "brotli")]
            Self::Brotli(e) => e.flush(),
            #[cfg(feature = "zstd")]
            Self::Zstd(e) => e.flush(),
        }
    }
}

/// A [`negotiate_and_encode`] failure: either nothing was acceptable to the
/// client, or the chosen coding could not be produced.
///
/// Two errors rather than one because they call for two different HTTP
/// responses: [`NotAcceptable`](Self::NotAcceptable) is a
/// `415 Unsupported Media Type`, while [`Coding`](Self::Coding) is a server
/// fault (a `500`), since `available` is the server's own list.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NegotiateEncodeError {
    /// The client excluded identity and every coding the server has. Answer
    /// `415`.
    #[error(transparent)]
    NotAcceptable(#[from] crate::negotiate::NotAcceptable),
    /// The chosen coding could not be encoded by this build.
    #[error(transparent)]
    Coding(#[from] HttpCodingError),
}

/// Negotiate a content coding against the client's `Accept-Encoding`, then
/// encode `body` with it.
///
/// `Ok(None)` means **send `body` unchanged, with no `Content-Encoding`
/// header** — either the client wanted no coding, or identity won.
///
/// # The caller still owns three response headers
///
/// On `Ok(Some((coding, bytes)))`:
///
/// 1. set `Content-Encoding: <coding>`;
/// 2. set `Content-Length` to `bytes.len()`, or remove it;
/// 3. **add `Accept-Encoding` to `Vary`** — forgetting this is a real and
///    damaging cache-poisoning bug, because a shared cache will otherwise
///    serve the compressed representation to a client that cannot decode it.
///
/// This crate deliberately does not mutate headers for you: it has no
/// dependency on the `http` crate and no opinion about your framework.
///
/// # Small bodies
///
/// There is no minimum-size short-circuit here. Compressing a 40-byte JSON
/// response makes it *larger*, so a server should skip negotiation entirely
/// below its own threshold (`oxihttp` uses 1024 bytes); doing that in the
/// caller keeps the decision — and the `Vary` consequences — where the
/// response policy already lives.
///
/// # Errors
///
/// [`NegotiateEncodeError::NotAcceptable`] when the client excluded identity
/// and every available coding (answer `415`);
/// [`NegotiateEncodeError::Coding`] when the chosen coding cannot be encoded
/// by this build — which can only happen if `available` was not filtered by
/// [`ContentCoding::is_encodable`].
///
/// # Example
///
/// ```
/// use oxiarc_http::{ContentCoding, EncodeOptions, negotiate_and_encode};
///
/// let available: Vec<ContentCoding> = [ContentCoding::Gzip, ContentCoding::Deflate]
///     .into_iter()
///     .filter(ContentCoding::is_encodable)
///     .collect();
/// let body = b"a response body long enough that compressing it is worthwhile; \
///              a response body long enough that compressing it is worthwhile";
///
/// match negotiate_and_encode(Some("gzip, deflate"), &available, body, EncodeOptions::default())
///     .expect("identity is acceptable, so this cannot be 415")
/// {
///     Some((coding, bytes)) => {
///         assert_eq!(coding, ContentCoding::Gzip);
///         assert!(bytes.len() < body.len());
///         // Content-Encoding: gzip / Content-Length: bytes.len() / Vary: Accept-Encoding
///     }
///     None => { /* send `body` as-is */ }
/// }
///
/// // A client that wants nothing compressed:
/// let plain = negotiate_and_encode(Some(""), &available, body, EncodeOptions::default())
///     .expect("empty value means identity");
/// assert!(plain.is_none());
/// ```
pub fn negotiate_and_encode(
    accept_encoding: Option<&str>,
    available: &[ContentCoding],
    body: &[u8],
    opts: EncodeOptions<'_>,
) -> std::result::Result<Option<(ContentCoding, Vec<u8>)>, NegotiateEncodeError> {
    let Some(coding) = crate::negotiate::negotiate(accept_encoding, available)? else {
        return Ok(None);
    };
    let encoded = encode_body(&coding, body, opts)?;
    Ok(Some((coding, encoded)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "deflate")]
    #[test]
    fn negotiate_and_encode_picks_and_encodes() {
        let available: Vec<ContentCoding> = [ContentCoding::Gzip, ContentCoding::Deflate]
            .into_iter()
            .filter(ContentCoding::is_encodable)
            .collect();
        let body = vec![b'a'; 4096];

        match negotiate_and_encode(
            Some("gzip;q=0.5, deflate;q=0.9"),
            &available,
            &body,
            EncodeOptions::default(),
        )
        .expect("identity is acceptable")
        {
            Some((coding, bytes)) => {
                // Highest q wins, not server order: deflate's 0.9 beats
                // gzip's 0.5 even though gzip is first in `available`.
                assert_eq!(coding, ContentCoding::Deflate);
                assert!(bytes.len() < body.len());
            }
            None => assert!(available.is_empty(), "a real coding was available"),
        }
    }

    #[test]
    fn negotiate_and_encode_returns_none_for_identity() {
        let available: Vec<ContentCoding> = [ContentCoding::Gzip]
            .into_iter()
            .filter(ContentCoding::is_encodable)
            .collect();
        assert!(
            negotiate_and_encode(Some(""), &available, b"body", EncodeOptions::default())
                .expect("empty value is identity")
                .is_none()
        );
        assert!(
            negotiate_and_encode(Some("br"), &available, b"body", EncodeOptions::default())
                .expect("nothing matches, identity is acceptable")
                .is_none()
        );
    }

    #[test]
    fn negotiate_and_encode_reports_not_acceptable() {
        let error = negotiate_and_encode(
            Some("*;q=0, identity;q=0"),
            &[],
            b"body",
            EncodeOptions::default(),
        )
        .expect_err("nothing is acceptable");
        assert!(matches!(error, NegotiateEncodeError::NotAcceptable(_)));
        assert!(error.to_string().contains("acceptable"));
    }

    #[test]
    fn negotiate_and_encode_reports_a_coding_this_build_cannot_produce() {
        // `available` deliberately unfiltered, which is the mistake the
        // rustdoc warns about.
        let error = negotiate_and_encode(
            Some("compress"),
            &[ContentCoding::Compress],
            b"body",
            EncodeOptions::default(),
        )
        .expect_err("compress cannot be produced");
        assert!(matches!(error, NegotiateEncodeError::Coding(_)));
    }

    #[test]
    fn identity_is_a_trivial_copy() {
        let body = b"unchanged";
        let out = encode_body(&ContentCoding::Identity, body, EncodeOptions::default())
            .expect("identity always succeeds");
        assert_eq!(out, body);
    }

    #[test]
    fn always_unsupported_codings_error() {
        for coding in [
            ContentCoding::Compress,
            ContentCoding::Dcb,
            ContentCoding::Unknown("x".to_string()),
        ] {
            let err = encode_body(&coding, b"x", EncodeOptions::default())
                .expect_err("must be unsupported");
            assert!(matches!(err, HttpCodingError::UnsupportedCoding { .. }));
        }
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_round_trips_through_the_oxiarc_decoder() {
        let body = b"the quick brown fox jumps over the lazy dog, repeatedly, repeatedly";
        let encoded =
            encode_body(&ContentCoding::Gzip, body, EncodeOptions::default()).expect("gzip encode");
        let decoded = oxiarc_deflate::gzip_decompress(&encoded).expect("gzip decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "deflate")]
    #[test]
    fn deflate_round_trips_through_the_oxiarc_decoder() {
        let body = b"the quick brown fox jumps over the lazy dog, repeatedly, repeatedly";
        let encoded = encode_body(&ContentCoding::Deflate, body, EncodeOptions::default())
            .expect("deflate encode");
        let decoded = oxiarc_deflate::zlib_decompress(&encoded).expect("zlib decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn brotli_round_trips_through_the_oxiarc_decoder() {
        let body = b"the quick brown fox jumps over the lazy dog, repeatedly, repeatedly";
        let encoded = encode_body(&ContentCoding::Brotli, body, EncodeOptions::default())
            .expect("brotli encode");
        let decoded = oxiarc_brotli::decompress(&encoded).expect("brotli decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_round_trips_through_the_oxiarc_decoder() {
        let body = b"the quick brown fox jumps over the lazy dog, repeatedly, repeatedly";
        let encoded =
            encode_body(&ContentCoding::Zstd, body, EncodeOptions::default()).expect("zstd encode");
        let decoded = oxiarc_zstd::decompress(&encoded).expect("zstd decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn dcz_without_dictionary_is_missing_dictionary() {
        let err = encode_body(&ContentCoding::Dcz, b"x", EncodeOptions::default())
            .expect_err("must require a dictionary");
        assert!(matches!(err, HttpCodingError::MissingDictionary { .. }));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn dcz_round_trips_through_the_oxiarc_decoder_with_dictionary() {
        let dictionary = b"common repeated preamble used across many small payloads";
        let body = b"a small payload sharing the common repeated preamble";
        let opts = EncodeOptions {
            dictionary: Some(dictionary),
            ..EncodeOptions::default()
        };
        let encoded = encode_body(&ContentCoding::Dcz, body, opts).expect("dcz encode");
        let decoded = oxiarc_zstd::decompress_with_dict(&encoded, dictionary).expect("dcz decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn streaming_encoder_round_trips() {
        let body = b"streamed, one write() call at a time, streamed, one write() call at a time";
        let mut encoder = Encoder::new(Vec::new(), &ContentCoding::Gzip, EncodeOptions::default())
            .expect("gzip streaming encoder");
        for chunk in body.chunks(7) {
            encoder.write_all(chunk).expect("write");
        }
        let compressed = encoder.finish().expect("finish");
        let decoded = oxiarc_deflate::gzip_decompress(&compressed).expect("gzip decode");
        assert_eq!(decoded, body);
    }

    #[test]
    fn streaming_identity_is_a_trivial_copy() {
        let body = b"unchanged, streamed";
        let mut encoder = Encoder::new(
            Vec::new(),
            &ContentCoding::Identity,
            EncodeOptions::default(),
        )
        .expect("identity streaming encoder");
        encoder.write_all(body).expect("write");
        let out = encoder.finish().expect("finish");
        assert_eq!(out, body);
    }

    #[cfg(feature = "deflate")]
    #[test]
    fn streaming_deflate_round_trips() {
        let body = b"streamed deflate, streamed deflate, streamed deflate".repeat(8);
        let mut encoder = Encoder::new(
            Vec::new(),
            &ContentCoding::Deflate,
            EncodeOptions::default(),
        )
        .expect("deflate streaming encoder");
        encoder.write_all(&body).expect("write");
        let compressed = encoder.finish().expect("finish");
        let decoded = oxiarc_deflate::zlib_decompress(&compressed).expect("zlib decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn streaming_brotli_round_trips() {
        let body = b"streamed brotli, streamed brotli, streamed brotli".repeat(8);
        let mut encoder =
            Encoder::new(Vec::new(), &ContentCoding::Brotli, EncodeOptions::default())
                .expect("brotli streaming encoder");
        encoder.write_all(&body).expect("write");
        let compressed = encoder.finish().expect("finish");
        let decoded = oxiarc_brotli::decompress(&compressed).expect("brotli decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_zstd_round_trips() {
        let body = b"streamed zstd, streamed zstd, streamed zstd".repeat(8);
        let mut encoder = Encoder::new(Vec::new(), &ContentCoding::Zstd, EncodeOptions::default())
            .expect("zstd streaming encoder");
        encoder.write_all(&body).expect("write");
        let compressed = encoder.finish().expect("finish");
        let decoded = oxiarc_zstd::decompress(&compressed).expect("zstd decode");
        assert_eq!(decoded, body);
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_dcz_round_trips_with_a_dictionary() {
        // `Encoder`'s Dcz arm goes through `ZstdStreamEncoder::with_dictionary`,
        // a different entry point from the one-shot `ZstdEncoder::set_dictionary`
        // that `encode_body` uses — Phase 8 owner decision #8 covers both, so
        // both need a round trip.
        let dictionary = b"common repeated preamble used across many small payloads";
        let body = b"a small payload sharing the common repeated preamble";
        let opts = EncodeOptions::new().with_dictionary(Some(dictionary));
        let mut encoder =
            Encoder::new(Vec::new(), &ContentCoding::Dcz, opts).expect("dcz streaming encoder");
        encoder.write_all(body).expect("write");
        let compressed = encoder.finish().expect("finish");
        let decoded =
            oxiarc_zstd::decompress_with_dict(&compressed, dictionary).expect("dcz decode");
        assert_eq!(decoded, body);

        // Flushed mid-body, `dcz` inherits zstd's multi-frame rule (see the
        // framing section of `Encoder`'s docs): every frame is coded against
        // the same dictionary, and only a multi-frame decoder sees all of it.
        let mut encoder =
            Encoder::new(Vec::new(), &ContentCoding::Dcz, opts).expect("dcz streaming encoder");
        encoder.write_all(&body[..10]).expect("write");
        encoder.flush().expect("flush");
        encoder.write_all(&body[10..]).expect("write");
        let flushed = encoder.finish().expect("finish");
        assert!(zstd_frame_magics(&flushed) >= 2);
        assert_eq!(
            oxiarc_zstd::decompress_multi_frame_with_dict(&flushed, dictionary)
                .expect("dcz multi-frame decode"),
            body
        );
    }

    /// Count RFC 8878 §3.1.1 frame magic numbers (`0xFD2FB528`, little-endian
    /// on the wire) in a zstd stream. A naive window scan can only
    /// *over*-count (a compressed payload may contain the same four bytes),
    /// so it is sound to assert "at least two frames" with it.
    #[cfg(feature = "zstd")]
    fn zstd_frame_magics(stream: &[u8]) -> usize {
        stream
            .windows(4)
            .filter(|w| *w == [0x28, 0xB5, 0x2F, 0xFD])
            .count()
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_zstd_is_multi_frame_and_needs_a_multi_frame_decoder() {
        // Pins the "Framing, flush, and the zstd multi-frame rule" section of
        // `Encoder`'s docs. Two independent ways to reach a multi-frame body:
        // an explicit flush, and simply streaming past the wrapped encoder's
        // 128 KiB automatic block boundary with no flush at all.
        let parts: [&[u8]; 3] = [b"first chunk ", b"second chunk ", b"third chunk "];
        let joined: Vec<u8> = parts.concat();
        let mut encoder = Encoder::new(Vec::new(), &ContentCoding::Zstd, EncodeOptions::default())
            .expect("zstd streaming encoder");
        for part in parts {
            encoder.write_all(part).expect("write");
            encoder.flush().expect("flush");
        }
        let flushed = encoder.finish().expect("finish");
        assert!(
            zstd_frame_magics(&flushed) >= 2,
            "an explicitly flushed zstd stream must be multi-frame"
        );
        assert_eq!(
            oxiarc_zstd::decompress_multi_frame(&flushed).expect("multi-frame decode"),
            joined,
            "a multi-frame decoder must recover the body exactly"
        );

        // No explicit flush: 10 x 64 KiB crosses the 128 KiB block boundary.
        let chunk = vec![b'x'; 64 * 1024];
        let mut encoder = Encoder::new(Vec::new(), &ContentCoding::Zstd, EncodeOptions::default())
            .expect("zstd streaming encoder");
        let mut expected = Vec::new();
        for _ in 0..10 {
            encoder.write_all(&chunk).expect("write");
            expected.extend_from_slice(&chunk);
        }
        let streamed = encoder.finish().expect("finish");
        assert!(
            zstd_frame_magics(&streamed) >= 2,
            "streaming past 128 KiB must produce a multi-frame zstd stream even without flush"
        );
        assert_eq!(
            oxiarc_zstd::decompress_multi_frame(&streamed).expect("multi-frame decode"),
            expected
        );
        // The hazard the docs warn about, asserted without depending on a
        // sibling crate keeping its current single-frame behaviour: a
        // single-frame decoder may stop early, but whatever it returns must
        // be a genuine *prefix* of the body — never corruption, and never a
        // superset. If `oxiarc-zstd` later grows multi-frame handling here,
        // this still holds (the prefix becomes the whole body).
        let single = oxiarc_zstd::decompress(&streamed).expect("single-frame decode");
        assert!(
            expected.starts_with(&single),
            "a single-frame decode must yield a prefix of the body, not garbage"
        );

        // `encode_body`, by contrast, always emits exactly one frame.
        let one_shot = encode_body(&ContentCoding::Zstd, &expected, EncodeOptions::default())
            .expect("one-shot zstd");
        assert_eq!(
            oxiarc_zstd::decompress(&one_shot).expect("single-frame decode"),
            expected,
            "encode_body must stay single-frame and decode with the plain entry point"
        );
    }

    #[cfg(any(feature = "gzip", feature = "deflate", feature = "brotli"))]
    #[test]
    fn streaming_flush_keeps_one_member_for_the_deflate_family_and_brotli() {
        // The other half of the framing table: for these codings a flush is
        // invisible to the peer, so the plain one-shot decoder still works.
        let parts: [&[u8]; 3] = [b"first chunk ", b"second chunk ", b"third chunk "];
        let joined: Vec<u8> = parts.concat();
        #[cfg(feature = "gzip")]
        {
            let mut e = Encoder::new(Vec::new(), &ContentCoding::Gzip, EncodeOptions::default())
                .expect("gzip streaming encoder");
            for part in parts {
                e.write_all(part).expect("write");
                e.flush().expect("flush");
            }
            let out = e.finish().expect("finish");
            assert_eq!(
                oxiarc_deflate::gzip_decompress(&out).expect("gzip decode"),
                joined
            );
        }
        #[cfg(feature = "deflate")]
        {
            let mut e = Encoder::new(
                Vec::new(),
                &ContentCoding::Deflate,
                EncodeOptions::default(),
            )
            .expect("deflate streaming encoder");
            for part in parts {
                e.write_all(part).expect("write");
                e.flush().expect("flush");
            }
            let out = e.finish().expect("finish");
            assert_eq!(
                oxiarc_deflate::zlib_decompress(&out).expect("zlib decode"),
                joined
            );
        }
        #[cfg(feature = "brotli")]
        {
            let mut e = Encoder::new(Vec::new(), &ContentCoding::Brotli, EncodeOptions::default())
                .expect("brotli streaming encoder");
            for part in parts {
                e.write_all(part).expect("write");
                e.flush().expect("flush");
            }
            let out = e.finish().expect("finish");
            assert_eq!(
                oxiarc_brotli::decompress(&out).expect("brotli decode"),
                joined
            );
        }
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn streaming_dcz_without_dictionary_is_missing_dictionary() {
        match Encoder::new(Vec::new(), &ContentCoding::Dcz, EncodeOptions::default()) {
            Err(e) => assert!(matches!(e, HttpCodingError::MissingDictionary { .. })),
            Ok(_) => panic!("Dcz must require a dictionary in the streaming encoder too"),
        }
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn streaming_empty_body_still_produces_a_complete_stream() {
        // Nothing is ever written; `finish()` alone must emit a header and
        // trailer that the decoder accepts, not an empty buffer.
        let encoder = Encoder::new(Vec::new(), &ContentCoding::Gzip, EncodeOptions::default())
            .expect("gzip streaming encoder");
        let compressed = encoder.finish().expect("finish");
        assert!(!compressed.is_empty());
        let decoded = oxiarc_deflate::gzip_decompress(&compressed).expect("gzip decode");
        assert!(decoded.is_empty());
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn streaming_one_byte_at_a_time_round_trips() {
        let body = b"one byte at a time, one byte at a time, one byte at a time";
        let mut encoder = Encoder::new(Vec::new(), &ContentCoding::Gzip, EncodeOptions::default())
            .expect("gzip streaming encoder");
        for byte in body.iter() {
            encoder.write_all(&[*byte]).expect("write");
            // An interleaved empty write must not disturb the stream.
            encoder.write_all(&[]).expect("empty write");
        }
        let compressed = encoder.finish().expect("finish");
        let decoded = oxiarc_deflate::gzip_decompress(&compressed).expect("gzip decode");
        assert_eq!(decoded, body);
    }

    #[test]
    fn encode_body_handles_an_empty_body_for_every_encodable_coding() {
        for coding in [
            ContentCoding::Identity,
            ContentCoding::Gzip,
            ContentCoding::Deflate,
            ContentCoding::Brotli,
            ContentCoding::Zstd,
        ] {
            if !coding.is_encodable() {
                continue;
            }
            let out = encode_body(&coding, b"", EncodeOptions::default())
                .unwrap_or_else(|e| panic!("{coding} must encode an empty body: {e}"));
            if coding != ContentCoding::Identity {
                assert!(!out.is_empty(), "{coding}: framing must still be emitted");
            }
        }
    }

    #[test]
    fn options_builders_are_chainable_and_match_the_fields() {
        let dictionary = b"dict";
        let opts = EncodeOptions::new()
            .with_level(1)
            .with_brotli_quality(11)
            .with_zstd_level(-5)
            .with_dictionary(Some(dictionary));
        assert_eq!(opts.level, 1);
        assert_eq!(opts.brotli_quality, 11);
        assert_eq!(opts.zstd_level, -5);
        assert_eq!(opts.dictionary, Some(&dictionary[..]));
        // `with_dictionary(None)` clears it again.
        assert_eq!(opts.with_dictionary(None).dictionary, None);
        assert_eq!(
            EncodeOptions::new().level,
            EncodeOptions::default().level,
            "new() must equal default()"
        );
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn out_of_range_deflate_level_is_clamped_not_an_error() {
        // Documented on `EncodeOptions::level`: oxiarc-deflate clamps to 9.
        let body = b"clamped level, clamped level, clamped level";
        let max = encode_body(
            &ContentCoding::Gzip,
            body,
            EncodeOptions::new().with_level(9),
        )
        .expect("level 9");
        let over = encode_body(
            &ContentCoding::Gzip,
            body,
            EncodeOptions::new().with_level(255),
        )
        .expect("an out-of-range level must not fail");
        assert_eq!(max, over);
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn out_of_range_brotli_quality_is_a_named_error_not_a_panic() {
        // Documented on `EncodeOptions::brotli_quality`: unlike `level` and
        // `zstd_level`, this one is validated by oxiarc-brotli, surfacing as
        // `Corrupt` rather than being silently clamped.
        let err = encode_body(
            &ContentCoding::Brotli,
            b"x",
            EncodeOptions::new().with_brotli_quality(200),
        )
        .expect_err("quality 200 is out of brotli's range");
        assert!(matches!(err, HttpCodingError::Corrupt { .. }));
        assert!(err.to_string().contains("200"));
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn out_of_range_zstd_level_is_clamped_not_an_error() {
        // Documented on `EncodeOptions::zstd_level`: oxiarc-zstd clamps to 0..=22.
        let body = b"clamped zstd level, clamped zstd level";
        let max = encode_body(
            &ContentCoding::Zstd,
            body,
            EncodeOptions::new().with_zstd_level(22),
        )
        .expect("level 22");
        let over = encode_body(
            &ContentCoding::Zstd,
            body,
            EncodeOptions::new().with_zstd_level(i32::MAX),
        )
        .expect("an out-of-range level must not fail");
        assert_eq!(max, over);
        let under = encode_body(
            &ContentCoding::Zstd,
            body,
            EncodeOptions::new().with_zstd_level(i32::MIN),
        )
        .expect("an out-of-range level must not fail");
        let min = encode_body(
            &ContentCoding::Zstd,
            body,
            EncodeOptions::new().with_zstd_level(0),
        )
        .expect("level 0");
        assert_eq!(under, min);
    }

    #[test]
    fn streaming_always_unsupported_codings_error() {
        // `Encoder<W>` cannot derive `Debug` (the wrapped sibling-crate
        // stream types don't), so `expect_err`/`unwrap_err` aren't available
        // here — match manually instead.
        match Encoder::new(
            Vec::new(),
            &ContentCoding::Compress,
            EncodeOptions::default(),
        ) {
            Err(e) => assert!(matches!(e, HttpCodingError::UnsupportedCoding { .. })),
            Ok(_) => panic!("must be unsupported"),
        }
    }
}

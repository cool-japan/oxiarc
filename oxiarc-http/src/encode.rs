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
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct EncodeOptions<'a> {
    /// DEFLATE-family (gzip, deflate) level, `0..=9`. Default **6**.
    pub level: u8,
    /// Brotli quality, `0..=11`. Default **4** — quality 11 is far too slow
    /// for per-response encoding; 4 is a typical server-side choice.
    pub brotli_quality: u32,
    /// Zstd level. Default **3**.
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

#[cfg(test)]
mod tests {
    use super::*;

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

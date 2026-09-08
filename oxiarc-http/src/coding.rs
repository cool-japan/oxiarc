//! [`ContentCoding`]: the RFC 9110 §8.4.1 content-coding token.

use std::fmt;
use std::str::FromStr;

use crate::error::{HttpCodingError, UnsupportedReason};

/// A single HTTP content coding (RFC 9110 §8.4.1).
///
/// # Ordering contract — load-bearing, do not reorder casually
///
/// Variants are declared from **least to most preferred**. This is not
/// incidental: the derived [`Ord`] lets [`AcceptEncoding::all_supported`]
/// (client side) list every coding this build can decode from least to most
/// preferred by simply sorting, which is exactly "best first" once reversed.
/// Server-side [`negotiate`] does **not** use this derived order to break
/// q-value ties — it uses the caller's own `available` slice order instead
/// (see that function's docs) — so this ordering is purely the crate's own
/// opinion about which codings are generally "better", used only where no
/// caller-supplied preference exists.
///
/// Inserting a new coding means placing it at the correct preference
/// position, never appending it blindly.
///
/// [`AcceptEncoding::all_supported`]: crate::AcceptEncoding::all_supported
/// [`negotiate`]: crate::negotiate
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ContentCoding {
    /// `identity` — no transformation (RFC 9110 §8.4.1). Reserved for
    /// `Accept-Encoding`; RFC 9110 §8.4 says it "SHOULD NOT" appear in a
    /// `Content-Encoding` response header.
    Identity,
    /// `compress` / `x-compress` (RFC 9110 §8.4.1.1) — legacy UNIX LZW,
    /// the `.Z` container (`oxiarc_lzw::z`). Decodable/encodable exactly
    /// when the `compress` feature is on — see
    /// [`is_decodable`](Self::is_decodable).
    ///
    /// `.Z` has no end-of-information code: a body cut short decodes to a
    /// plausible, silently short prefix rather than an error, the same as
    /// `gzip -dc`/BSD `uncompress`. See
    /// [`TrailingData`](crate::TrailingData)'s docs for how the *other*
    /// codings here detect truncation, and why this one structurally
    /// cannot.
    Compress,
    /// `deflate` (RFC 9110 §8.4.1.2) — an RFC 1950 zlib wrapper around an
    /// RFC 1951 DEFLATE stream. RFC 9110 itself sanctions accepting a raw
    /// (unwrapped) DEFLATE stream too, since some servers send that under
    /// this name. [`Decoder`](crate::Decoder) sniffs for a zlib header and
    /// falls back to raw DEFLATE (and accepts a gzip stream mislabelled
    /// `deflate`, which browsers do too).
    Deflate,
    /// `gzip` / `x-gzip` (RFC 9110 §8.4.1.3) — RFC 1952.
    Gzip,
    /// `br` — RFC 7932 Brotli.
    Brotli,
    /// `zstd` — RFC 8878 Zstandard.
    Zstd,
    /// `dcb` — Compression Dictionary Transport (RFC 9842), Brotli variant.
    ///
    /// Decodable/encodable when the `brotli` feature is on **and** a
    /// dictionary is supplied
    /// ([`Decoder::with_dictionary`](crate::Decoder::with_dictionary),
    /// [`EncodeOptions::dictionary`](crate::EncodeOptions)) — a body is a
    /// 36-byte preamble (magic + the dictionary's SHA-256) followed by a
    /// shared-dictionary Brotli stream (`oxiarc_brotli::dcb`); a wrong or
    /// absent dictionary is [`HttpCodingError::MissingDictionary`] /
    /// [`HttpCodingError::Corrupt`], never a silent fallback to plain `br`.
    /// [`Encoder`](crate::Encoder) (the *streaming* encoder) still refuses
    /// this one specifically — see its own doc comment.
    Dcb,
    /// `dcz` — Compression Dictionary Transport (RFC 9842), Zstandard variant.
    ///
    /// Decodable/encodable when the `zstd` feature is on **and** a
    /// dictionary is supplied — the same shape as [`Dcb`](Self::Dcb), a
    /// 40-byte preamble (an RFC 8878 skippable frame carrying the
    /// dictionary's SHA-256) followed by an ordinary dictionary-referencing
    /// Zstandard frame.
    Dcz,
    /// Any other token, preserved verbatim (lowercased) for round-tripping
    /// and for matching a server's own custom/experimental coding.
    ///
    /// **Build it with [`parse`](Self::parse), not by hand.** Content
    /// codings are case-insensitive (RFC 9110 §8.4.1), and this crate
    /// normalizes that by holding the token already lowercased; equality —
    /// and therefore [`negotiate`]'s matching of an `available` entry
    /// against a client's token — is a plain string comparison. A
    /// hand-written `Unknown("X-Custom".to_string())` will never match the
    /// `Unknown("x-custom")` a parsed header yields.
    ///
    /// [`is_decodable`](Self::is_decodable) and
    /// [`is_encodable`](Self::is_encodable) are always `false`. Ranked most
    /// preferred: an `Unknown` value only ever reaches [`negotiate`]'s
    /// candidate pool when a caller explicitly places it in `available`
    /// (a server never gets one from parsing its own configuration), so
    /// unlike an unrecognized *client* token it represents a deliberate,
    /// specific server choice.
    ///
    /// [`negotiate`]: crate::negotiate
    Unknown(String),
}

impl ContentCoding {
    /// The canonical token, as emitted in a header.
    ///
    /// Always the non-`x-` spelling: [`Gzip`](Self::Gzip) is `"gzip"`, never
    /// `"x-gzip"`.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Identity => "identity",
            Self::Compress => "compress",
            Self::Deflate => "deflate",
            Self::Gzip => "gzip",
            Self::Brotli => "br",
            Self::Zstd => "zstd",
            Self::Dcb => "dcb",
            Self::Dcz => "dcz",
            Self::Unknown(token) => token,
        }
    }

    /// Parse one content-coding token, case-insensitively (RFC 9110 §8.4.1),
    /// accepting the `x-gzip` / `x-compress` aliases (§8.4.1.1, §8.4.1.3).
    ///
    /// Infallible: any token this crate does not recognize becomes
    /// [`Unknown`](Self::Unknown) (lowercased), rather than an error — a
    /// header may legitimately name a coding this build (or any build) has
    /// never heard of, and that is not, by itself, a protocol violation.
    /// Whitespace must already be trimmed by the caller; this function does
    /// not skip leading or trailing OWS.
    ///
    /// # Examples
    /// ```
    /// use oxiarc_http::ContentCoding;
    /// assert_eq!(ContentCoding::parse("GZIP"), ContentCoding::Gzip);
    /// assert_eq!(ContentCoding::parse("x-gzip"), ContentCoding::Gzip);
    /// assert_eq!(ContentCoding::parse("x-compress"), ContentCoding::Compress);
    /// assert_eq!(ContentCoding::parse("Br"), ContentCoding::Brotli);
    /// assert_eq!(
    ///     ContentCoding::parse("shrink-o-matic"),
    ///     ContentCoding::Unknown("shrink-o-matic".to_string())
    /// );
    /// ```
    pub fn parse(token: &str) -> Self {
        // `eq_ignore_ascii_case` avoids allocating a lowercased copy just to
        // compare against a fixed set of ASCII literals.
        if token.eq_ignore_ascii_case("identity") {
            Self::Identity
        } else if token.eq_ignore_ascii_case("compress") || token.eq_ignore_ascii_case("x-compress")
        {
            Self::Compress
        } else if token.eq_ignore_ascii_case("deflate") {
            Self::Deflate
        } else if token.eq_ignore_ascii_case("gzip") || token.eq_ignore_ascii_case("x-gzip") {
            Self::Gzip
        } else if token.eq_ignore_ascii_case("br") {
            Self::Brotli
        } else if token.eq_ignore_ascii_case("zstd") {
            Self::Zstd
        } else if token.eq_ignore_ascii_case("dcb") {
            Self::Dcb
        } else if token.eq_ignore_ascii_case("dcz") {
            Self::Dcz
        } else {
            Self::Unknown(token.to_ascii_lowercase())
        }
    }

    /// Whether this build can actually decode this coding.
    ///
    /// Tracks real, implemented capability — not merely "the dependency
    /// happens to be compiled in". [`Identity`] is always `true`;
    /// [`Unknown`](Self::Unknown) is always `false`.
    ///
    /// # What this predicate is for
    ///
    /// It answers "will this build's [`Decoder`](crate::Decoder) handle
    /// this coding?" — the question a client building an `Accept-Encoding`
    /// header must answer at request time, and the one
    /// [`AcceptEncoding`](crate::AcceptEncoding) uses it for.
    /// [`Decoder::new`](crate::Decoder::new) refuses exactly the codings
    /// this reports `false` for, with
    /// [`HttpCodingError::UnsupportedCoding`](crate::HttpCodingError::UnsupportedCoding).
    ///
    /// One caveat: [`Dcb`](Self::Dcb) and [`Dcz`](Self::Dcz) are decodable
    /// only *with* a caller-supplied dictionary, so each reports `true`
    /// whenever its underlying feature (`brotli`, `zstd`) is on, while
    /// `Decoder::new` still directs you to
    /// [`Decoder::with_dictionary`](crate::Decoder::with_dictionary).
    ///
    /// [`Identity`]: Self::Identity
    pub const fn is_decodable(&self) -> bool {
        match self {
            Self::Identity => true,
            Self::Unknown(_) => false,
            Self::Compress => cfg!(feature = "compress"),
            Self::Deflate => cfg!(feature = "deflate"),
            Self::Gzip => cfg!(feature = "gzip"),
            Self::Brotli | Self::Dcb => cfg!(feature = "brotli"),
            Self::Zstd | Self::Dcz => cfg!(feature = "zstd"),
        }
    }

    /// Whether this build can encode this coding (server side).
    ///
    /// See [`is_decodable`](Self::is_decodable) for the same "real
    /// capability, not just a compiled-in dependency" caveat.
    pub const fn is_encodable(&self) -> bool {
        self.is_decodable()
    }
}

impl FromStr for ContentCoding {
    /// Parsing a content-coding token never fails; see [`ContentCoding::parse`].
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

impl fmt::Display for ContentCoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a coding this crate cannot currently produce should report as
/// "the feature is off" versus "this crate has no such capability at all".
pub(crate) fn unsupported_reason(coding: &ContentCoding) -> UnsupportedReason {
    match coding {
        ContentCoding::Identity => {
            // Identity is always supported; callers should not reach this.
            UnsupportedReason::Unknown
        }
        ContentCoding::Compress => UnsupportedReason::FeatureDisabled("compress"),
        ContentCoding::Deflate => UnsupportedReason::FeatureDisabled("deflate"),
        ContentCoding::Gzip => UnsupportedReason::FeatureDisabled("gzip"),
        ContentCoding::Brotli | ContentCoding::Dcb => UnsupportedReason::FeatureDisabled("brotli"),
        ContentCoding::Zstd | ContentCoding::Dcz => UnsupportedReason::FeatureDisabled("zstd"),
        ContentCoding::Unknown(_) => UnsupportedReason::Unknown,
    }
}

/// Build the [`HttpCodingError::UnsupportedCoding`] for a coding that
/// [`ContentCoding::is_encodable`] (or `is_decodable`) reported `false` for.
pub(crate) fn unsupported_coding_error(coding: &ContentCoding) -> HttpCodingError {
    HttpCodingError::UnsupportedCoding {
        token: coding.as_str().to_string(),
        reason: unsupported_reason(coding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_is_case_insensitive() {
        for (input, expected) in [
            ("gzip", ContentCoding::Gzip),
            ("GZIP", ContentCoding::Gzip),
            ("GzIp", ContentCoding::Gzip),
            ("br", ContentCoding::Brotli),
            ("Br", ContentCoding::Brotli),
            ("BR", ContentCoding::Brotli),
            ("deflate", ContentCoding::Deflate),
            ("DEFLATE", ContentCoding::Deflate),
            ("zstd", ContentCoding::Zstd),
            ("ZSTD", ContentCoding::Zstd),
            ("identity", ContentCoding::Identity),
            ("IDENTITY", ContentCoding::Identity),
            ("compress", ContentCoding::Compress),
            ("dcb", ContentCoding::Dcb),
            ("dcz", ContentCoding::Dcz),
            ("DCZ", ContentCoding::Dcz),
        ] {
            assert_eq!(ContentCoding::parse(input), expected, "input: {input:?}");
        }
    }

    #[test]
    fn aliases_map_to_canonical() {
        assert_eq!(ContentCoding::parse("x-gzip"), ContentCoding::Gzip);
        assert_eq!(ContentCoding::parse("X-GZIP"), ContentCoding::Gzip);
        assert_eq!(ContentCoding::parse("x-compress"), ContentCoding::Compress);
        assert_eq!(ContentCoding::parse("X-Compress"), ContentCoding::Compress);
    }

    #[test]
    fn unknown_token_round_trips_lowercased() {
        let c = ContentCoding::parse("Shrink-O-Matic");
        assert_eq!(c, ContentCoding::Unknown("shrink-o-matic".to_string()));
        assert_eq!(c.as_str(), "shrink-o-matic");
        assert_eq!(c.to_string(), "shrink-o-matic");
    }

    #[test]
    fn display_matches_as_str() {
        for c in [
            ContentCoding::Identity,
            ContentCoding::Compress,
            ContentCoding::Deflate,
            ContentCoding::Gzip,
            ContentCoding::Brotli,
            ContentCoding::Zstd,
            ContentCoding::Dcb,
            ContentCoding::Dcz,
        ] {
            assert_eq!(c.to_string(), c.as_str());
        }
    }

    #[test]
    fn ordering_is_least_to_most_preferred() {
        assert!(ContentCoding::Identity < ContentCoding::Compress);
        assert!(ContentCoding::Compress < ContentCoding::Deflate);
        assert!(ContentCoding::Deflate < ContentCoding::Gzip);
        assert!(ContentCoding::Gzip < ContentCoding::Brotli);
        assert!(ContentCoding::Brotli < ContentCoding::Zstd);
        assert!(ContentCoding::Zstd < ContentCoding::Dcb);
        assert!(ContentCoding::Dcb < ContentCoding::Dcz);
        assert!(ContentCoding::Dcz < ContentCoding::Unknown(String::new()));
    }

    #[test]
    fn identity_is_always_capable() {
        assert!(ContentCoding::Identity.is_decodable());
        assert!(ContentCoding::Identity.is_encodable());
    }

    #[test]
    fn permanently_unsupported_codings_report_false() {
        // True regardless of which features happen to be on in this build —
        // unlike `Compress`/`Dcb`/`Dcz`, which are conditionally decodable
        // (see the feature-gated tests elsewhere in this module and in
        // `tests/dictionary.rs`).
        assert!(!ContentCoding::Unknown("x".to_string()).is_decodable());
        assert!(!ContentCoding::Unknown("x".to_string()).is_encodable());
    }

    #[cfg(not(feature = "compress"))]
    #[test]
    fn compress_is_undecodable_without_its_feature() {
        assert!(!ContentCoding::Compress.is_decodable());
        assert!(!ContentCoding::Compress.is_encodable());
    }

    #[cfg(feature = "compress")]
    #[test]
    fn compress_is_decodable_with_its_feature() {
        assert!(ContentCoding::Compress.is_decodable());
        assert!(ContentCoding::Compress.is_encodable());
    }

    #[cfg(not(feature = "brotli"))]
    #[test]
    fn dcb_is_undecodable_without_brotli() {
        assert!(!ContentCoding::Dcb.is_decodable());
        assert!(!ContentCoding::Dcb.is_encodable());
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn dcb_is_decodable_with_brotli() {
        // "Decodable" here means "with a dictionary in hand" — see the
        // type's own doc comment; `Decoder::new` alone still needs
        // `with_dictionary`.
        assert!(ContentCoding::Dcb.is_decodable());
        assert!(ContentCoding::Dcb.is_encodable());
    }

    #[test]
    fn from_str_delegates_to_parse() {
        let c: ContentCoding = "gzip".parse().expect("infallible");
        assert_eq!(c, ContentCoding::Gzip);
    }
}

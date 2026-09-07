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
    /// `compress` / `x-compress` (RFC 9110 §8.4.1.1) — legacy UNIX LZW.
    ///
    /// Token recognition only in this build: [`is_decodable`](Self::is_decodable)
    /// and [`is_encodable`](Self::is_encodable) are unconditionally `false`.
    /// See the `compress` Cargo feature's doc comment for why.
    Compress,
    /// `deflate` (RFC 9110 §8.4.1.2) — an RFC 1950 zlib wrapper around an
    /// RFC 1951 DEFLATE stream. RFC 9110 itself sanctions accepting a raw
    /// (unwrapped) DEFLATE stream too, since some servers send that under
    /// this name; that sniffing logic lives in the decoder wave.
    Deflate,
    /// `gzip` / `x-gzip` (RFC 9110 §8.4.1.3) — RFC 1952.
    Gzip,
    /// `br` — RFC 7932 Brotli.
    Brotli,
    /// `zstd` — RFC 8878 Zstandard.
    Zstd,
    /// `dcb` — Compression Dictionary Transport (RFC 9842), Brotli variant.
    ///
    /// Always unsupported in this build: `oxiarc-brotli` has no shared-dictionary
    /// support (Phase 8 owner decision #8). [`is_decodable`](Self::is_decodable)
    /// and [`is_encodable`](Self::is_encodable) are unconditionally `false`.
    Dcb,
    /// `dcz` — Compression Dictionary Transport (RFC 9842), Zstandard variant.
    ///
    /// Supported when a dictionary is supplied (Phase 8 owner decision #8):
    /// [`encode_body`](crate::encode_body) accepts `Dcz` when the `zstd`
    /// feature is on and [`EncodeOptions::dictionary`](crate::EncodeOptions)
    /// is `Some`, and fails with
    /// [`HttpCodingError::MissingDictionary`] otherwise.
    Dcz,
    /// Any other token, preserved verbatim (lowercased) for round-tripping
    /// and for matching a server's own custom/experimental coding.
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
    /// happens to be compiled in" — so [`Compress`](Self::Compress) and
    /// [`Dcb`](Self::Dcb) are `false` unconditionally (see their doc
    /// comments) even when their Cargo features are enabled. [`Identity`]
    /// is always `true`.
    ///
    /// [`Identity`]: Self::Identity
    pub const fn is_decodable(&self) -> bool {
        match self {
            Self::Identity => true,
            Self::Compress | Self::Dcb | Self::Unknown(_) => false,
            Self::Deflate => cfg!(feature = "deflate"),
            Self::Gzip => cfg!(feature = "gzip"),
            Self::Brotli => cfg!(feature = "brotli"),
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
        ContentCoding::Compress => UnsupportedReason::Unknown, // P-4, not a feature flag away
        ContentCoding::Dcb => UnsupportedReason::Unknown,      // needs upstream brotli work
        ContentCoding::Deflate => UnsupportedReason::FeatureDisabled("deflate"),
        ContentCoding::Gzip => UnsupportedReason::FeatureDisabled("gzip"),
        ContentCoding::Brotli => UnsupportedReason::FeatureDisabled("brotli"),
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
        // True regardless of which features happen to be on in this build.
        assert!(!ContentCoding::Compress.is_decodable());
        assert!(!ContentCoding::Compress.is_encodable());
        assert!(!ContentCoding::Dcb.is_decodable());
        assert!(!ContentCoding::Dcb.is_encodable());
        assert!(!ContentCoding::Unknown("x".to_string()).is_decodable());
        assert!(!ContentCoding::Unknown("x".to_string()).is_encodable());
    }

    #[test]
    fn from_str_delegates_to_parse() {
        let c: ContentCoding = "gzip".parse().expect("infallible");
        assert_eq!(c, ContentCoding::Gzip);
    }
}

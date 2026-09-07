//! [`AcceptEncoding`]: client-side `Accept-Encoding` header builder.

use std::fmt;

use crate::coding::ContentCoding;
use crate::header::QValue;

/// Builds an `Accept-Encoding` request header from the codings this build
/// can actually decode.
///
/// # The RFC 9110 trap: `""` and an absent header are opposites
///
/// Per §12.5.3, an **absent** `Accept-Encoding` header means "any content
/// coding is acceptable", while a **present but empty** value means "the
/// user agent does not want any content coding in response". Sending
/// `Accept-Encoding: ` (empty) is therefore not a harmless no-op — it is a
/// request for an uncompressed body, the opposite of what an empty builder
/// usually means to a caller. [`to_header_value`](Self::to_header_value)
/// returns `Option<String>` specifically so this mistake is hard to make by
/// accident: `None` means "send no header at all" (do not set the header),
/// and only [`to_header_value_or_empty`](Self::to_header_value_or_empty)
/// (or an explicit empty builder) ever produces the empty string.
///
/// # "Can decode" means this build, not necessarily your decode path
///
/// Every method here filters on [`ContentCoding::is_decodable`], which
/// reports what `oxiarc-http` compiled with these Cargo features will
/// decode; [`Decoder`](crate::Decoder) accepts exactly that set, so a
/// header built here and a decoder built by [`Decoder::from_header`] agree
/// by construction. Advertising a coding is a promise about whatever
/// actually decodes the response body — if that is not this crate's
/// `Decoder`, the promise is about *that* decoder and only you can vouch
/// for it. The filtering keeps a coding whose Cargo feature is off out of
/// the header, which is the mistake worth preventing.
///
/// [`Decoder::from_header`]: crate::Decoder::from_header
#[derive(Debug, Clone, PartialEq)]
pub struct AcceptEncoding {
    /// `(coding, weight)`; `coding: None` is the `*` wildcard.
    codings: Vec<(Option<ContentCoding>, QValue)>,
}

impl Default for AcceptEncoding {
    /// Same as [`all_supported`](Self::all_supported) — the useful default
    /// for a client is "everything I can decode", not an empty builder
    /// (which [`new`](Self::new) already spells unambiguously).
    fn default() -> Self {
        Self::all_supported()
    }
}

impl AcceptEncoding {
    /// Start from an empty set (no header content at all).
    pub fn new() -> Self {
        Self {
            codings: Vec::new(),
        }
    }

    /// Every coding this build can decode, best first, no explicit
    /// `;q=` values.
    ///
    /// With `default` features (`gzip`, `deflate`) this is `"gzip, deflate"`;
    /// with `brotli` and `zstd` also enabled it is `"zstd, br, gzip, deflate"`
    /// (`dcz` joins that list once its underlying `zstd` feature is on, but
    /// only once a dictionary makes it actually usable — see
    /// [`ContentCoding::Dcz`]; it is therefore **not** included here even
    /// when `zstd` is on, since advertising it would be the exact footgun
    /// [`add`](Self::add) exists to avoid).
    pub fn all_supported() -> Self {
        // Best-to-worst: mirrors ContentCoding's declared least-to-most-preferred
        // Ord, reversed. Compress is never decodable in this build (see its
        // doc comment) so it is not worth listing here; Dcb/Dcz/Unknown are
        // deliberately excluded too — see the doc comment above.
        let mut out = Self::new();
        for coding in [
            ContentCoding::Zstd,
            ContentCoding::Brotli,
            ContentCoding::Gzip,
            ContentCoding::Deflate,
        ] {
            out = out.add(coding);
        }
        out
    }

    /// Add a coding at the default weight (`q=1`), if this build can decode
    /// it.
    ///
    /// A coding whose feature is off — or, like [`ContentCoding::Compress`],
    /// not yet implemented at all — is **silently skipped**: advertising a
    /// coding you cannot decode is the one mistake that turns a working
    /// client into one that receives an undecodable body. See the
    /// type-level docs for exactly which "cannot decode" this is: the
    /// feature-gated capability of the finished crate, not of this
    /// decoder-less version.
    // `add` is a builder method, not an arithmetic operator — there is no
    // sensible `std::ops::Add` for `AcceptEncoding`, so the name collision
    // clippy flags here is spurious.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn add(self, coding: ContentCoding) -> Self {
        self.with_q(coding, QValue::ONE)
    }

    /// Add a coding with an explicit weight. Skipped (same as
    /// [`add`](Self::add)) if this build cannot decode it.
    #[must_use]
    pub fn with_q(mut self, coding: ContentCoding, q: QValue) -> Self {
        if coding.is_decodable() {
            self.codings.push((Some(coding), q));
        }
        self
    }

    /// Remove every entry for `coding` (added via [`add`](Self::add),
    /// [`with_q`](Self::with_q), or [`with_identity`](Self::with_identity)).
    #[must_use]
    pub fn remove(mut self, coding: &ContentCoding) -> Self {
        self.codings.retain(|(c, _)| c.as_ref() != Some(coding));
        self
    }

    /// Append `identity;q=<q>`. Use [`QValue::ZERO`] to demand a compressed
    /// response (RFC 9110 §12.5.3 rule 2) or omit this entirely to leave
    /// identity's usual "acceptable by default" behaviour alone.
    #[must_use]
    pub fn with_identity(mut self, q: QValue) -> Self {
        self.codings.push((Some(ContentCoding::Identity), q));
        self
    }

    /// Append a `*` wildcard entry with the given weight; [`QValue::ZERO`]
    /// rejects every coding not explicitly listed elsewhere in this builder.
    #[must_use]
    pub fn with_wildcard(mut self, q: QValue) -> Self {
        self.codings.push((None, q));
        self
    }

    /// Render the header value, e.g. `"gzip, deflate;q=0.5, *;q=0"`.
    ///
    /// Returns `None` when no entries are present: send **no**
    /// `Accept-Encoding` header at all in that case (RFC 9110 §12.5.3 rule
    /// 1 — "any content coding is acceptable" — not the empty-value rule).
    /// See the type-level docs for why this distinction matters, and use
    /// [`to_header_value_or_empty`](Self::to_header_value_or_empty) if you
    /// specifically mean "send `Accept-Encoding: ` (empty)".
    pub fn to_header_value(&self) -> Option<String> {
        if self.codings.is_empty() {
            None
        } else {
            Some(self.render())
        }
    }

    /// As [`to_header_value`](Self::to_header_value), but renders the empty
    /// set as `""` instead of `None` — i.e. this always produces a value you
    /// can hand straight to a header setter, deliberately signalling
    /// "no content coding wanted" when the builder is empty.
    pub fn to_header_value_or_empty(&self) -> String {
        self.render()
    }

    /// The codings advertised, in the order they were added. Useful for
    /// asserting in tests.
    pub fn codings(&self) -> &[(Option<ContentCoding>, QValue)] {
        &self.codings
    }

    fn render(&self) -> String {
        self.codings
            .iter()
            .map(|(coding, q)| {
                let token = match coding {
                    Some(c) => c.as_str().to_string(),
                    None => "*".to_string(),
                };
                if *q == QValue::ONE {
                    token
                } else {
                    format!("{token};q={}", format_qvalue(*q))
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl fmt::Display for AcceptEncoding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_header_value_or_empty())
    }
}

/// Render a [`QValue`] the way RFC 9110 §12.4.2 examples do: as few
/// fractional digits as needed, never a trailing `.` or trailing zeros.
fn format_qvalue(q: QValue) -> String {
    let thousandths = q.as_thousandths();
    if thousandths == 0 {
        return "0".to_string();
    }
    if thousandths == 1000 {
        return "1".to_string();
    }
    let mut rendered = format!("0.{:03}", thousandths % 1000);
    while rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.pop();
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::parse_accept_encoding;
    use proptest::prelude::*;

    #[test]
    fn default_is_all_supported() {
        assert_eq!(AcceptEncoding::default(), AcceptEncoding::all_supported());
    }

    #[test]
    fn all_supported_renders_exactly_what_the_docs_claim() {
        // Pins `all_supported`'s rustdoc, which names two concrete header
        // values. Written so it asserts something on every feature combo:
        // the rendered list is exactly the decodable subset of the declared
        // best-first order, and never contains a coding this build cannot
        // decode.
        let expected: Vec<&str> = [
            ContentCoding::Zstd,
            ContentCoding::Brotli,
            ContentCoding::Gzip,
            ContentCoding::Deflate,
        ]
        .iter()
        .filter(|c| c.is_decodable())
        .map(|c| c.as_str())
        .collect();
        let rendered = AcceptEncoding::all_supported().to_header_value();
        if expected.is_empty() {
            assert_eq!(rendered, None, "no decodable coding => send no header");
        } else {
            assert_eq!(rendered, Some(expected.join(", ")));
        }
        // The two combinations the rustdoc spells out verbatim.
        #[cfg(all(
            feature = "gzip",
            feature = "deflate",
            not(feature = "brotli"),
            not(feature = "zstd")
        ))]
        assert_eq!(rendered, Some("gzip, deflate".to_string()));
        #[cfg(all(
            feature = "gzip",
            feature = "deflate",
            feature = "brotli",
            feature = "zstd"
        ))]
        assert_eq!(rendered, Some("zstd, br, gzip, deflate".to_string()));
        // Never advertises a coding with no dictionary story (Dcz), a
        // permanently-unsupported one (Compress/Dcb), or identity.
        for banned in ["dcz", "dcb", "compress", "identity", "*"] {
            assert!(
                !rendered
                    .as_deref()
                    .unwrap_or("")
                    .split(", ")
                    .any(|t| t == banned),
                "all_supported must not advertise {banned:?}"
            );
        }
    }

    #[test]
    fn empty_builder_yields_none_for_to_header_value() {
        assert_eq!(AcceptEncoding::new().to_header_value(), None);
    }

    #[test]
    fn empty_builder_yields_empty_string_for_or_empty() {
        assert_eq!(AcceptEncoding::new().to_header_value_or_empty(), "");
    }

    #[test]
    fn undecodable_coding_is_silently_skipped() {
        // `compress` is never decodable in this build regardless of features.
        let ae = AcceptEncoding::new().add(ContentCoding::Compress);
        assert_eq!(ae.to_header_value(), None);
    }

    #[test]
    fn add_uses_q1_with_no_explicit_param() {
        // Identity is decodable regardless of which codec features are
        // enabled, so this test (which is about the builder's rendering,
        // not about any one coding) holds across the whole feature matrix.
        let ae = AcceptEncoding::new().add(ContentCoding::Identity);
        assert_eq!(ae.to_header_value(), Some("identity".to_string()));
    }

    #[test]
    fn with_q_renders_fraction() {
        let ae =
            AcceptEncoding::new().with_q(ContentCoding::Identity, QValue::from_thousandths(500));
        assert_eq!(ae.to_header_value(), Some("identity;q=0.5".to_string()));
    }

    #[test]
    fn with_identity_and_wildcard_render() {
        let ae = AcceptEncoding::new()
            .with_identity(QValue::ZERO)
            .with_wildcard(QValue::ZERO);
        assert_eq!(
            ae.to_header_value(),
            Some("identity;q=0, *;q=0".to_string())
        );
    }

    // Needs two distinct, unconditionally-real codings to prove `remove`
    // filters one and keeps the other; `gzip` and `deflate` are both this
    // crate's default features, so this runs under the common builds
    // (default, all-features) without depending on `brotli`/`zstd`.
    #[cfg(all(feature = "gzip", feature = "deflate"))]
    #[test]
    fn remove_drops_matching_entries() {
        let ae = AcceptEncoding::new()
            .add(ContentCoding::Gzip)
            .add(ContentCoding::Deflate)
            .remove(&ContentCoding::Gzip);
        assert_eq!(ae.to_header_value(), Some("deflate".to_string()));
    }

    #[test]
    fn display_matches_to_header_value_or_empty() {
        let ae = AcceptEncoding::new().add(ContentCoding::Gzip);
        assert_eq!(ae.to_string(), ae.to_header_value_or_empty());
    }

    #[test]
    fn round_trips_through_parse_accept_encoding() {
        let ae = AcceptEncoding::new()
            .with_q(ContentCoding::Gzip, QValue::from_thousandths(500))
            .add(ContentCoding::Brotli)
            .with_wildcard(QValue::ZERO);
        let rendered = ae.to_header_value_or_empty();
        let entries = parse_accept_encoding(&rendered).expect("valid");
        assert_eq!(entries.len(), ae.codings().len());
        for (entry, (coding, q)) in entries.iter().zip(ae.codings()) {
            assert_eq!(&entry.coding, coding);
            assert_eq!(&entry.qvalue, q);
        }
    }

    proptest::proptest! {
        #[test]
        fn round_trip_holds_for_generated_weights(
            gzip_q in 0u16..=1000,
            br_q in 0u16..=1000,
            wildcard_q in 0u16..=1000,
        ) {
            let ae = AcceptEncoding::new()
                .with_q(ContentCoding::Gzip, QValue::from_thousandths(gzip_q))
                .with_q(ContentCoding::Brotli, QValue::from_thousandths(br_q))
                .with_wildcard(QValue::from_thousandths(wildcard_q));
            let rendered = ae.to_header_value_or_empty();
            let entries = parse_accept_encoding(&rendered).expect("valid");
            let expected = ae.codings();
            prop_assert_eq!(entries.len(), expected.len());
            for (entry, (coding, q)) in entries.iter().zip(expected) {
                prop_assert_eq!(&entry.coding, coding);
                prop_assert_eq!(&entry.qvalue, q);
            }
        }
    }
}

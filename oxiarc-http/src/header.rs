//! Header-value parsing: [`QValue`], `Accept-Encoding` and `Content-Encoding`.

use crate::coding::ContentCoding;
use crate::error::{HttpCodingError, LimitKind, Result};
use crate::limits::DEFAULT_MAX_CODINGS;

/// Bound on the number of raw comma-separated segments scanned in one header
/// value (RFC 9110 §5.6.1.2: tolerate a reasonable number of empty list
/// elements, but "not so much that they could be used as a denial-of-service
/// mechanism"). Applied to the *raw* segment count, before empty elements
/// are dropped, so a value with an unreasonable number of commas is rejected
/// before any per-element work — including allocating a
/// [`ContentCoding::Unknown`] — is done on it.
const MAX_HEADER_ELEMENTS: usize = 64;

/// A quality value (RFC 9110 §12.4.2), stored as thousandths (`0..=1000`) so
/// ordering is exact and no float comparison — and no `NaN` — is involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct QValue(u16);

impl QValue {
    /// `q=0` — not acceptable.
    pub const ZERO: QValue = QValue(0);
    /// `q=1` (or no `;q=` at all, which defaults to this) — fully acceptable.
    pub const ONE: QValue = QValue(1000);

    /// Parse a bare qvalue such as `"0.8"`, `"1"`, `"0.001"` (no `q=`
    /// prefix). Exact per RFC 9110 §12.4.2's grammar:
    /// `("0" ["." 0*3DIGIT]) / ("1" ["." 0*3("0")])` — rejects more than
    /// three fractional digits, a fractional part after `1` that isn't all
    /// zeros, and anything not starting with `0` or `1`.
    ///
    /// # Examples
    /// ```
    /// use oxiarc_http::QValue;
    /// assert_eq!(QValue::parse("0"), Some(QValue::ZERO));
    /// assert_eq!(QValue::parse("0.0"), Some(QValue::ZERO));
    /// assert_eq!(QValue::parse("0.123"), Some(QValue::from_thousandths(123)));
    /// assert_eq!(QValue::parse("1"), Some(QValue::ONE));
    /// assert_eq!(QValue::parse("1.000"), Some(QValue::ONE));
    /// assert_eq!(QValue::parse("1.5"), None); // out of range
    /// assert_eq!(QValue::parse("0.0001"), None); // too many fractional digits
    /// assert_eq!(QValue::parse("abc"), None);
    /// ```
    pub fn parse(s: &str) -> Option<QValue> {
        let bytes = s.as_bytes();
        let whole = match bytes.first()? {
            b'0' => 0u16,
            b'1' => 1u16,
            _ => return None,
        };
        let rest = &bytes[1..];
        if rest.is_empty() {
            return Some(if whole == 0 { Self::ZERO } else { Self::ONE });
        }
        if rest[0] != b'.' {
            return None;
        }
        let digits = &rest[1..];
        if digits.len() > 3 || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        if whole == 1 && digits.iter().any(|&d| d != b'0') {
            return None; // "1.001".."1.999" exceed the qvalue ceiling of 1.000
        }
        let mut thousandths = whole * 1000;
        for (&digit, place) in digits.iter().zip([100u16, 10, 1]) {
            thousandths += u16::from(digit - b'0') * place;
        }
        Some(QValue(thousandths))
    }

    /// Parse a `q=<qvalue>` parameter; the `q` is case-insensitive
    /// (RFC 9110 §12.4.2: the parameter is "named \"q\" (case-insensitive)").
    /// Leading/trailing whitespace around the whole parameter, and around
    /// the `=`, is tolerated.
    pub fn parse_param(s: &str) -> Option<QValue> {
        try_parse_q_param(s).flatten()
    }

    /// Build a `QValue` directly from a thousandths value (`0..=1000`),
    /// clamping anything out of range. Mainly useful for tests and for
    /// constructing values that didn't come from header text.
    pub const fn from_thousandths(thousandths: u16) -> QValue {
        QValue(if thousandths > 1000 {
            1000
        } else {
            thousandths
        })
    }

    /// Thousandths, `0..=1000`.
    pub const fn as_thousandths(self) -> u16 {
        self.0
    }

    /// As a fraction in `0.0..=1.0`.
    pub fn as_f32(self) -> f32 {
        f32::from(self.0) / 1000.0
    }

    /// `true` for any weight greater than zero (RFC 9110 §12.4.2: "a qvalue
    /// of 0 means \"not acceptable\"").
    pub const fn is_acceptable(self) -> bool {
        self.0 > 0
    }
}

/// Distinguish "not a `q=` parameter at all" (`None`, ignore it — Accept-Encoding
/// has no other standard parameter, but real senders occasionally attach one)
/// from "a `q=` parameter whose value failed to parse" (`Some(None)` — RFC
/// 9110 §12.4.2 grammar violation; callers must drop the whole entry, not
/// silently default it to `q=1`) from "a valid `q=` parameter" (`Some(Some(q))`).
fn try_parse_q_param(param: &str) -> Option<Option<QValue>> {
    let rest = param.trim().strip_prefix(|c: char| c == 'q' || c == 'Q')?;
    let rest = rest.trim_start().strip_prefix('=')?;
    Some(QValue::parse(rest.trim()))
}

/// A parsed `Accept-Encoding` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptEntry {
    /// The coding, or `None` for the `*` wildcard.
    pub coding: Option<ContentCoding>,
    /// The weight; [`QValue::ONE`] when no `;q=` was present.
    pub qvalue: QValue,
}

/// Split a header value on commas per RFC 9110 §5.6.1.2's `#element` list
/// rule: trim optional whitespace (OWS) around each element and drop empty
/// ones, while bounding the number of raw segments scanned by
/// [`MAX_HEADER_ELEMENTS`].
fn split_list_bounded<'a>(value: &'a str, field: &'static str) -> Result<Vec<&'a str>> {
    let mut out = Vec::new();
    for (index, raw) in value.split(',').enumerate() {
        if index >= MAX_HEADER_ELEMENTS {
            return Err(HttpCodingError::HeaderSyntax {
                field,
                message: format!("more than {MAX_HEADER_ELEMENTS} comma-separated elements"),
            });
        }
        let trimmed = raw.trim_matches(|c: char| c == ' ' || c == '\t');
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
    }
    Ok(out)
}

/// Parse an `Accept-Encoding` field value (RFC 9110 §12.5.3).
///
/// Unknown codings are **not** an error — represented as
/// [`ContentCoding::Unknown`] — since a client may legitimately advertise a
/// coding this build, or any build, has never heard of. An entry whose
/// `;q=` parameter fails to parse is dropped entirely (defensive: real-world
/// proxies emit things like `q=1.00000`); an entry with no `;q=` defaults to
/// [`QValue::ONE`].
///
/// Returns entries in the order they appeared. `Ok(vec![])` for an empty
/// value — note this is meaningful and distinct from an *absent* header
/// (RFC 9110 §12.5.3: an empty value means the user agent wants no content
/// coding at all). [`negotiate`](crate::negotiate) encodes that distinction
/// correctly; do not infer it yourself by testing whether this Vec is empty.
///
/// # Errors
/// [`HttpCodingError::HeaderSyntax`] if the value has more than 64
/// comma-separated segments (an internal, non-configurable bound — this
/// function takes no [`DecodeLimits`](crate::DecodeLimits)).
///
/// # Examples
/// ```
/// use oxiarc_http::{AcceptEntry, ContentCoding, QValue, parse_accept_encoding};
///
/// let entries = parse_accept_encoding("gzip;q=0.5, br, *;q=0").expect("valid");
/// assert_eq!(
///     entries,
///     vec![
///         AcceptEntry { coding: Some(ContentCoding::Gzip), qvalue: QValue::from_thousandths(500) },
///         AcceptEntry { coding: Some(ContentCoding::Brotli), qvalue: QValue::ONE },
///         AcceptEntry { coding: None, qvalue: QValue::ZERO },
///     ]
/// );
/// ```
pub fn parse_accept_encoding(value: &str) -> Result<Vec<AcceptEntry>> {
    let elements = split_list_bounded(value, "Accept-Encoding")?;
    let mut entries = Vec::with_capacity(elements.len());
    for element in elements {
        let mut parts = element.split(';');
        // `element` came from `split_list_bounded`, which only yields
        // non-empty (post-trim) segments, so `parts.next()` is always `Some`.
        let token = parts.next().unwrap_or_default().trim();
        if token.is_empty() {
            continue; // e.g. a bare ";q=0.5" with no coding name
        }

        let mut qvalue = QValue::ONE;
        let mut malformed = false;
        for param in parts {
            let param = param.trim();
            if param.is_empty() {
                continue; // tolerate a stray repeated `;`
            }
            match try_parse_q_param(param) {
                Some(Some(q)) => qvalue = q,
                Some(None) => {
                    malformed = true;
                    break;
                }
                None => {} // not a `q=` parameter; ignore it
            }
        }
        if malformed {
            continue;
        }

        let coding = if token == "*" {
            None
        } else {
            Some(ContentCoding::parse(token))
        };
        entries.push(AcceptEntry { coding, qvalue });
    }
    Ok(entries)
}

/// Parse a `Content-Encoding` field value into the codings **in the order
/// they were applied** (RFC 9110 §8.4).
///
/// To decode, apply the returned codings in **reverse**: the last-listed
/// coding was applied last and must be undone first.
///
/// Multiple `Content-Encoding` header lines are equivalent to one
/// comma-joined value; use [`parse_content_encoding_all`] for that case.
///
/// # Behaviour
/// - Case-insensitive; `x-gzip` and `x-compress` map to their canonical
///   codings (RFC 9110 §8.4.1.1, §8.4.1.3).
/// - Empty list elements are skipped (RFC 9110 §5.6.1.2), bounded by 64
///   total raw segments (the same internal bound as
///   [`parse_accept_encoding`]).
/// - `identity` is accepted and **dropped** from the result: it is a no-op
///   transformation, and RFC 9110 §8.4 says it "SHOULD NOT" appear here.
/// - An unknown token becomes [`ContentCoding::Unknown`] rather than an
///   error — whether an unrecognized *content-coding* is fatal is a decode-time
///   question (raised only once something actually tries to undo it), not a
///   parse-time one.
/// - An empty or all-whitespace value yields an empty `Vec` (no coding applied).
///
/// # Errors
/// [`HttpCodingError::HeaderSyntax`] for too many elements;
/// [`HttpCodingError::LimitExceeded`] (`kind:` [`LimitKind::Codings`]) if
/// more than 4 real codings chain together (the same default as
/// [`DecodeLimits`](crate::DecodeLimits)`::max_codings`) — a
/// `Content-Encoding: gzip, gzip, gzip, ...` amplification guard.
///
/// # Examples
/// ```
/// use oxiarc_http::{ContentCoding, parse_content_encoding};
///
/// // RFC 9110 §8.4: apply in reverse — decode br first, then gzip.
/// assert_eq!(
///     parse_content_encoding("gzip, br").expect("valid"),
///     vec![ContentCoding::Gzip, ContentCoding::Brotli]
/// );
/// assert_eq!(parse_content_encoding("identity").expect("valid"), vec![]);
/// ```
pub fn parse_content_encoding(value: &str) -> Result<Vec<ContentCoding>> {
    let elements = split_list_bounded(value, "Content-Encoding")?;
    let mut codings = Vec::with_capacity(elements.len());
    for element in elements {
        // RFC 9110 §8.4 gives Content-Encoding no parameters at all; a
        // trailing `;...` from a non-conformant sender is defensively
        // ignored rather than folded into the token.
        let token = element.split(';').next().unwrap_or(element).trim();
        if token.is_empty() {
            continue;
        }
        let coding = ContentCoding::parse(token);
        if coding == ContentCoding::Identity {
            continue;
        }
        codings.push(coding);
    }
    if codings.len() > DEFAULT_MAX_CODINGS {
        return Err(HttpCodingError::LimitExceeded {
            limit: DEFAULT_MAX_CODINGS as f64,
            kind: LimitKind::Codings {
                count: codings.len(),
            },
        });
    }
    Ok(codings)
}

/// As [`parse_content_encoding`], over several header lines (multiple
/// `Content-Encoding:` lines are equivalent to one comma-joined value,
/// RFC 9110 §5.3).
pub fn parse_content_encoding_all<'a, I>(values: I) -> Result<Vec<ContentCoding>>
where
    I: IntoIterator<Item = &'a str>,
{
    let joined = values.into_iter().collect::<Vec<_>>().join(", ");
    parse_content_encoding(&joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- QValue -------------------------------------------------------

    #[test]
    fn qvalue_exact_parses() {
        assert_eq!(QValue::parse("0"), Some(QValue::ZERO));
        assert_eq!(QValue::parse("0.0"), Some(QValue::ZERO));
        assert_eq!(QValue::parse("0.123"), Some(QValue::from_thousandths(123)));
        assert_eq!(QValue::parse("1"), Some(QValue::ONE));
        assert_eq!(QValue::parse("1.000"), Some(QValue::ONE));
        assert_eq!(QValue::parse("1.0"), Some(QValue::ONE));
    }

    #[test]
    fn qvalue_rejects_out_of_range() {
        assert_eq!(QValue::parse("1.5"), None);
        assert_eq!(QValue::parse("1.001"), None);
        assert_eq!(QValue::parse("2"), None);
        assert_eq!(QValue::parse("-0.5"), None);
    }

    #[test]
    fn qvalue_rejects_too_many_fractional_digits() {
        assert_eq!(QValue::parse("0.0001"), None);
        assert_eq!(QValue::parse("1.0001"), None);
    }

    #[test]
    fn qvalue_rejects_garbage() {
        assert_eq!(QValue::parse("abc"), None);
        assert_eq!(QValue::parse(""), None);
        assert_eq!(QValue::parse("."), None);
        assert_eq!(QValue::parse("0."), Some(QValue::ZERO));
    }

    #[test]
    fn qvalue_parse_param_case_insensitive() {
        assert_eq!(
            QValue::parse_param("q=0.5"),
            Some(QValue::from_thousandths(500))
        );
        assert_eq!(
            QValue::parse_param("Q=0.5"),
            Some(QValue::from_thousandths(500))
        );
        assert_eq!(
            QValue::parse_param(" q = 0.5 "),
            Some(QValue::from_thousandths(500))
        );
        assert_eq!(QValue::parse_param("x=0.5"), None);
    }

    #[test]
    fn qvalue_never_panics_on_arbitrary_bytes() {
        // Non-ASCII / malformed-looking input must not panic (byte slicing
        // on `&[u8]`, never `&str`, so char-boundary panics are impossible —
        // this just re-asserts that at the API level).
        for s in ["０", "0.\u{0}", "1.😀", "0.-1", "q=q=q", ";;;"] {
            let _ = QValue::parse(s);
            let _ = QValue::parse_param(s);
        }
    }

    // --- parse_accept_encoding -----------------------------------------

    #[test]
    fn accept_encoding_rfc_examples() {
        let entries = parse_accept_encoding("compress, gzip").expect("valid");
        assert_eq!(
            entries,
            vec![
                AcceptEntry {
                    coding: Some(ContentCoding::Compress),
                    qvalue: QValue::ONE
                },
                AcceptEntry {
                    coding: Some(ContentCoding::Gzip),
                    qvalue: QValue::ONE
                },
            ]
        );
    }

    #[test]
    fn accept_encoding_empty_value_is_meaningful_empty_vec() {
        assert_eq!(parse_accept_encoding("").expect("valid"), vec![]);
    }

    #[test]
    fn accept_encoding_wildcard() {
        let entries = parse_accept_encoding("*").expect("valid");
        assert_eq!(
            entries,
            vec![AcceptEntry {
                coding: None,
                qvalue: QValue::ONE
            }]
        );
    }

    #[test]
    fn accept_encoding_q_values() {
        let entries = parse_accept_encoding("compress;q=0.5, gzip;q=1.0").expect("valid");
        assert_eq!(
            entries,
            vec![
                AcceptEntry {
                    coding: Some(ContentCoding::Compress),
                    qvalue: QValue::from_thousandths(500)
                },
                AcceptEntry {
                    coding: Some(ContentCoding::Gzip),
                    qvalue: QValue::ONE
                },
            ]
        );
    }

    #[test]
    fn accept_encoding_space_before_q_and_wildcard_zero() {
        // "gzip;q=1.0, identity; q=0.5, *;q=0" — note the space before `q`.
        let entries = parse_accept_encoding("gzip;q=1.0, identity; q=0.5, *;q=0").expect("valid");
        assert_eq!(
            entries,
            vec![
                AcceptEntry {
                    coding: Some(ContentCoding::Gzip),
                    qvalue: QValue::ONE
                },
                AcceptEntry {
                    coding: Some(ContentCoding::Identity),
                    qvalue: QValue::from_thousandths(500)
                },
                AcceptEntry {
                    coding: None,
                    qvalue: QValue::ZERO
                },
            ]
        );
    }

    #[test]
    fn accept_encoding_empty_elements_skipped() {
        let entries = parse_accept_encoding("gzip,,,br").expect("valid");
        assert_eq!(
            entries,
            vec![
                AcceptEntry {
                    coding: Some(ContentCoding::Gzip),
                    qvalue: QValue::ONE
                },
                AcceptEntry {
                    coding: Some(ContentCoding::Brotli),
                    qvalue: QValue::ONE
                },
            ]
        );
    }

    #[test]
    fn accept_encoding_unknown_tokens_are_not_errors() {
        let entries = parse_accept_encoding("foo , ,bar,charlie").expect("valid");
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0].coding,
            Some(ContentCoding::Unknown("foo".to_string()))
        );
    }

    #[test]
    fn accept_encoding_case_insensitive_and_alias() {
        let entries = parse_accept_encoding("GZIP, Br, X-GZIP").expect("valid");
        assert_eq!(
            entries.iter().map(|e| e.coding.clone()).collect::<Vec<_>>(),
            vec![
                Some(ContentCoding::Gzip),
                Some(ContentCoding::Brotli),
                Some(ContentCoding::Gzip)
            ]
        );
    }

    #[test]
    fn accept_encoding_q_case_insensitive_param_name() {
        let entries = parse_accept_encoding("gzip;Q=0.5").expect("valid");
        assert_eq!(entries[0].qvalue, QValue::from_thousandths(500));
    }

    #[test]
    fn accept_encoding_malformed_q_drops_entry_silently() {
        for header in ["gzip;q=0.0001", "gzip;q=1.5", "gzip;q=abc"] {
            let entries = parse_accept_encoding(header).expect("valid");
            assert_eq!(entries, vec![], "header: {header:?}");
        }
    }

    #[test]
    fn accept_encoding_dos_bound() {
        let hostile = ",".repeat(65);
        let err = parse_accept_encoding(&hostile).expect_err("must reject");
        assert!(matches!(err, HttpCodingError::HeaderSyntax { .. }));
    }

    #[test]
    fn accept_encoding_exactly_at_bound_is_fine() {
        // 64 elements (63 commas would give 64 segments); use 64 commas ->
        // 65 segments, one over, still must fail; 63 commas -> 64 segments, ok.
        let ok = "gzip,".repeat(63) + "gzip"; // 64 segments
        assert!(parse_accept_encoding(&ok).is_ok());
        let bad = "gzip,".repeat(64) + "gzip"; // 65 segments
        assert!(parse_accept_encoding(&bad).is_err());
    }

    // --- parse_content_encoding ------------------------------------------

    #[test]
    fn content_encoding_reverse_order_is_preserved_by_the_caller() {
        // parse_content_encoding preserves *application* order; the caller
        // reverses it to decode (RFC 9110 §8.4).
        let codings = parse_content_encoding("deflate, gzip").expect("valid");
        assert_eq!(codings, vec![ContentCoding::Deflate, ContentCoding::Gzip]);
    }

    #[test]
    fn content_encoding_identity_is_dropped() {
        assert_eq!(
            parse_content_encoding("identity").expect("valid"),
            Vec::new()
        );
    }

    #[test]
    fn content_encoding_unknown_token_is_not_an_error() {
        let codings = parse_content_encoding("gzip, unknown").expect("valid");
        assert_eq!(
            codings,
            vec![
                ContentCoding::Gzip,
                ContentCoding::Unknown("unknown".to_string())
            ]
        );
    }

    #[test]
    fn content_encoding_repeated_coding_is_legal() {
        let codings = parse_content_encoding("gzip, gzip").expect("valid");
        assert_eq!(codings, vec![ContentCoding::Gzip, ContentCoding::Gzip]);
    }

    #[test]
    fn content_encoding_too_many_codings_errors() {
        let header = ["gzip"; DEFAULT_MAX_CODINGS + 1].join(", ");
        let err = parse_content_encoding(&header).expect_err("must reject");
        assert!(matches!(
            err,
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Codings { .. },
                ..
            }
        ));
    }

    #[test]
    fn content_encoding_at_the_limit_is_fine() {
        let header = ["gzip"; DEFAULT_MAX_CODINGS].join(", ");
        assert!(parse_content_encoding(&header).is_ok());
    }

    #[test]
    fn content_encoding_all_joins_lines() {
        let codings = parse_content_encoding_all(["gzip", "br"]).expect("valid");
        assert_eq!(codings, vec![ContentCoding::Gzip, ContentCoding::Brotli]);
    }

    // --- fuzzable: no panics on any input --------------------------------

    proptest::proptest! {
        #[test]
        fn parse_accept_encoding_never_panics(s in ".{0,200}") {
            let _ = parse_accept_encoding(&s);
        }

        #[test]
        fn parse_content_encoding_never_panics(s in ".{0,200}") {
            let _ = parse_content_encoding(&s);
        }

        #[test]
        fn qvalue_parse_never_panics(s in ".{0,32}") {
            let _ = QValue::parse(&s);
            let _ = QValue::parse_param(&s);
        }

        #[test]
        fn content_coding_parse_never_panics(s in ".{0,64}") {
            let _ = ContentCoding::parse(&s);
        }
    }
}

//! [`negotiate`]: RFC 9110 §12.5.3 server-side content-coding selection.

use std::collections::HashMap;

use thiserror::Error;

use crate::coding::ContentCoding;
use crate::header::{QValue, parse_accept_encoding};

/// Every acceptable coding was excluded, including identity.
///
/// RFC 9110 §12.5.3: "Servers that fail a request due to an unsupported
/// content coding ought to respond with a 415 (Unsupported Media Type)
/// status and include an Accept-Encoding header field in that response."
/// Note the status code is **415**, not 406.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("no acceptable content coding: client excluded identity and all server codings")]
pub struct NotAcceptable;

/// Choose a content coding for a response, per RFC 9110 §12.5.3.
///
/// `accept_encoding` is the raw request header value, or `None` if the
/// header was absent — the two are **not** interchangeable (rule 1 vs. the
/// empty-value rule). `available` is the server's preference-ordered list of
/// codings it can produce, best first; [`ContentCoding::Identity`] need not
/// be listed and is always implicitly available unless excluded.
///
/// # Returns
/// - `Ok(Some(coding))` — send the body encoded with this coding, and set
///   `Content-Encoding: coding.as_str()`.
/// - `Ok(None)` — send the body uncompressed (`identity`); emit no
///   `Content-Encoding` header.
/// - `Err(`[`NotAcceptable`]`)` — the client excluded identity *and* every
///   coding the server can produce. Per §12.5.3 the server should respond
///   **415** (Unsupported Media Type) with an `Accept-Encoding` response
///   header listing what it does support.
///
/// # Rules implemented (RFC 9110 §12.5.3)
/// 1. Header **absent** (`None`) → any coding acceptable; `available`'s
///    first entry is chosen.
/// 2. Header **present but empty** (`Some("")`, after trimming OWS) →
///    `Ok(None)`; the client wants no coding at all. This is the opposite of
///    rule 1 — see [`AcceptEncoding`](crate::AcceptEncoding)'s docs.
/// 3. A coding named with `q=0` is never chosen, unconditionally, even if a
///    `*` entry would otherwise admit it — an explicit entry always wins
///    over the wildcard for that one coding.
/// 4. `*` supplies the weight for any *ordinary* (non-identity) coding not
///    explicitly named — but never for identity itself; identity's
///    acceptability is governed **solely** by rule 5, independent of any
///    wildcard (this is the one place a literal reading of §12.5.3's prose
///    is ambiguous; see the "Identity and the wildcard" section below for
///    the reasoning and the RFC 9110 §12.5.3 test table that pins it).
/// 5. Identity is acceptable by default unless explicitly excluded
///    (`identity;q=0`). If nothing else in the header addresses real
///    codings at all — no wildcard, no explicit non-identity entry — and
///    identity itself is explicitly excluded, the server falls back to
///    `available`'s own order, exactly as under rule 1 (this is rule 2's own
///    qualifier: "unless the identity coding is indicated as unacceptable").
///    An unlisted ordinary coding with **no** wildcard present and **no**
///    such identity-refusal fallback in play is simply not acceptable — it
///    was never mentioned and nothing broadens the request to cover it.
/// 6. Among acceptable codings the highest non-zero q-value wins; ties break
///    by `available`'s order (the *caller's* preference, not
///    [`ContentCoding`]'s derived [`Ord`] — see that type's docs). Identity
///    never wins an exact tie against a real coding in `available` — it is
///    considered only after every entry of `available` has been — but an
///    *explicit* `identity;q=`, given a strictly higher weight than
///    anything else, is honoured like any other explicit entry.
///
/// # Identity and the wildcard
///
/// RFC 9110 §12.5.3's own text is genuinely ambiguous read in isolation:
/// "acceptable by default unless specifically excluded ... stating either
/// `identity;q=0` or `*;q=0` without a more specific entry for identity"
/// could be parsed as "`*;q=0` alone excludes identity". It does not: a bare
/// `*;q=0` (no explicit `identity` entry at all) leaves identity acceptable,
/// while `*;q=0, identity;q=0` excludes it and `*;q=0, identity;q=1` leaves
/// it acceptable at `q=1`. In other words, **the wildcard never reaches
/// identity** — identity is governed exclusively by its own explicit entry
/// (if any) or by its own §12.5.3 default. This implementation is pinned
/// against that reading with the full table in `tests/negotiation.rs`
/// (report §10.6), which is authoritative over the prose here.
///
/// # oxihttp deviations
///
/// This corrects two behaviours `oxihttp`'s own `negotiate` (pre-`oxiarc-http`)
/// gets wrong: it inverts precedence by preferring the *server's* fixed order
/// over the *client's* stated q-value (rule 6 requires the opposite — the
/// client's q-value dominates and the server's order only breaks exact
/// ties), and it does not implement the `*` wildcard at all.
pub fn negotiate(
    accept_encoding: Option<&str>,
    available: &[ContentCoding],
) -> Result<Option<ContentCoding>, NotAcceptable> {
    let Some(raw) = accept_encoding else {
        // Rule 1: header absent => any coding acceptable; use the server's
        // own first preference (or identity, if `available` is empty).
        return Ok(available.first().cloned());
    };
    if raw.trim().is_empty() {
        // Rule 2 (empty-value case): the client wants no coding, full stop.
        return Ok(None);
    }

    // A malformed Accept-Encoding value (only possible cause: too many
    // comma-separated elements) is treated the same as it being absent of
    // useful information — fall through to "nothing acceptable" rather than
    // propagating a parse error through a signature that has none; a
    // hostile header should not get special treatment over one that simply
    // named nothing available.
    let entries = parse_accept_encoding(raw).unwrap_or_default();

    // Max q per explicitly-named coding (including `identity`, if present);
    // max q carried by the `*` wildcard, if present. RFC 9110 doesn't spell
    // out what happens with a duplicated token or duplicated wildcard; take
    // the max of the duplicates, matching the duplicate-explicit-coding rule
    // the report's test table pins (`"gzip;q=0.5, gzip;q=0.9"` -> q=0.9).
    let mut explicit: HashMap<ContentCoding, QValue> = HashMap::new();
    let mut wildcard_q: Option<QValue> = None;
    for entry in &entries {
        match &entry.coding {
            Some(coding) => {
                let slot = explicit.entry(coding.clone()).or_insert(QValue::ZERO);
                if entry.qvalue > *slot {
                    *slot = entry.qvalue;
                }
            }
            None => {
                wildcard_q = Some(match wildcard_q {
                    Some(current) if current >= entry.qvalue => current,
                    _ => entry.qvalue,
                });
            }
        }
    }

    let mut best: Option<(ContentCoding, QValue)> = None;

    // Ordinary (non-identity) codings: explicit entry, else the wildcard,
    // else not acceptable (rule 3 only ever makes a *listed* coding
    // acceptable; nothing broadens that except the wildcard, rule 4).
    for coding in available {
        if *coding == ContentCoding::Identity {
            continue; // identity is always resolved once, below
        }
        let q = explicit
            .get(coding)
            .copied()
            .or(wildcard_q)
            .unwrap_or(QValue::ZERO);
        consider(&mut best, coding.clone(), q);
    }

    // Identity: rule 5, never reached by the wildcard (see the doc section
    // above). An explicit entry competes numerically like anything else; the
    // absence of one makes identity a pure fallback, chosen only if nothing
    // in `available` was otherwise acceptable — never displacing an already
    // -accepted ordinary coding regardless of relative q magnitude.
    match explicit.get(&ContentCoding::Identity).copied() {
        Some(q) => consider(&mut best, ContentCoding::Identity, q),
        None => {
            if best.is_none() {
                best = Some((ContentCoding::Identity, QValue::ONE));
            }
        }
    }

    if best.is_none() {
        // Reachable only when identity carried an explicit `q=0` (the branch
        // above never leaves `best` empty otherwise) and no ordinary coding
        // in `available` was acceptable. Rule 2's own qualifier — "unless
        // the identity coding is indicated as unacceptable" — means the
        // usual "SHOULD send uncompressed" fallback no longer applies; the
        // server must send *something*. If the header made no statement at
        // all about real codings (no wildcard), it fell back to the
        // server's own preference exactly as under rule 1. A wildcard, by
        // contrast, *is* a statement about real codings — if it excluded
        // them too, there is genuinely nothing left to send.
        if wildcard_q.is_none() {
            best = available
                .iter()
                .find(|c| **c != ContentCoding::Identity)
                .cloned()
                .map(|c| (c, QValue::ONE));
        }
    }

    match best {
        Some((ContentCoding::Identity, _)) => Ok(None),
        Some((coding, _)) => Ok(Some(coding)),
        None => Err(NotAcceptable),
    }
}

/// Replace `best` with `(coding, q)` only if `q` is acceptable and strictly
/// higher than `best`'s current weight — so, on an exact tie, whichever
/// candidate was considered **first** keeps the slot. Candidates are always
/// considered in `available`'s order (rule 6: ties break by the server's
/// stated preference).
fn consider(best: &mut Option<(ContentCoding, QValue)>, coding: ContentCoding, q: QValue) {
    if !q.is_acceptable() {
        return;
    }
    match best {
        Some((_, current_q)) if q <= *current_q => {}
        _ => *best = Some((coding, q)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GZIP_DEFLATE: [ContentCoding; 2] = [ContentCoding::Gzip, ContentCoding::Deflate];

    #[test]
    fn absent_header_picks_servers_first_choice() {
        assert_eq!(
            negotiate(None, &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Gzip))
        );
    }

    #[test]
    fn absent_header_with_empty_available_is_identity() {
        assert_eq!(negotiate(None, &[]), Ok(None));
    }

    #[test]
    fn empty_value_means_no_coding() {
        assert_eq!(negotiate(Some(""), &GZIP_DEFLATE), Ok(None));
    }

    #[test]
    fn tie_breaks_by_available_order() {
        assert_eq!(
            negotiate(Some("gzip, deflate"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Gzip))
        );
    }

    #[test]
    fn single_listed_coding_wins_over_unlisted() {
        assert_eq!(
            negotiate(Some("deflate"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Deflate))
        );
    }

    #[test]
    fn unavailable_codings_fall_back_to_identity() {
        assert_eq!(negotiate(Some("br, zstd"), &GZIP_DEFLATE), Ok(None));
    }

    #[test]
    fn explicit_zero_excludes_even_with_others_present() {
        assert_eq!(
            negotiate(Some("gzip;q=0, deflate"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Deflate))
        );
    }

    #[test]
    fn highest_q_wins_not_server_order() {
        // This is the row that catches oxihttp's actual bug.
        assert_eq!(
            negotiate(Some("deflate;q=0.9, gzip;q=0.8"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Deflate))
        );
    }

    #[test]
    fn bare_wildcard_admits_all() {
        assert_eq!(
            negotiate(Some("*"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Gzip))
        );
    }

    #[test]
    fn wildcard_zero_alone_still_leaves_identity_acceptable() {
        assert_eq!(negotiate(Some("*;q=0"), &GZIP_DEFLATE), Ok(None));
    }

    #[test]
    fn wildcard_zero_and_identity_zero_excludes_both() {
        assert_eq!(
            negotiate(Some("*;q=0, identity;q=0"), &GZIP_DEFLATE),
            Err(NotAcceptable)
        );
    }

    #[test]
    fn wildcard_zero_with_explicit_identity_one_is_more_specific() {
        assert_eq!(
            negotiate(Some("*;q=0, identity;q=1"), &GZIP_DEFLATE),
            Ok(None)
        );
    }

    #[test]
    fn identity_refused_alone_falls_back_to_server_preference() {
        assert_eq!(
            negotiate(Some("identity;q=0"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Gzip))
        );
    }

    #[test]
    fn identity_refused_with_nothing_available_is_not_acceptable() {
        assert_eq!(negotiate(Some("identity;q=0"), &[]), Err(NotAcceptable));
    }

    #[test]
    fn wildcard_weight_applies_to_unlisted_over_a_lower_explicit_weight() {
        assert_eq!(
            negotiate(Some("*;q=0.5, gzip;q=0.1"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Deflate))
        );
    }

    #[test]
    fn duplicate_token_takes_the_max() {
        assert_eq!(
            negotiate(Some("gzip;q=0.5, gzip;q=0.9"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Gzip))
        );
    }

    #[test]
    fn available_order_is_honoured_even_against_declared_preference() {
        // ContentCoding's own Ord ranks Gzip above Deflate, but the caller's
        // `available` order must win the tie, not the derived Ord.
        let reversed = [ContentCoding::Deflate, ContentCoding::Gzip];
        assert_eq!(
            negotiate(Some("gzip, deflate"), &reversed),
            Ok(Some(ContentCoding::Deflate))
        );
    }

    #[test]
    fn identity_never_wins_an_exact_tie_against_a_real_coding() {
        assert_eq!(
            negotiate(Some("identity;q=0.5, gzip;q=0.5"), &GZIP_DEFLATE),
            Ok(Some(ContentCoding::Gzip))
        );
    }

    #[test]
    fn explicit_identity_with_higher_q_wins_outright() {
        assert_eq!(
            negotiate(Some("identity;q=0.9, gzip;q=0.5"), &GZIP_DEFLATE),
            Ok(None)
        );
    }

    #[test]
    fn unknown_server_coding_can_be_matched_by_token() {
        let available = [ContentCoding::Unknown("x-custom".to_string())];
        assert_eq!(
            negotiate(Some("x-custom;q=1"), &available),
            Ok(Some(ContentCoding::Unknown("x-custom".to_string())))
        );
    }

    proptest::proptest! {
        #[test]
        fn never_panics_on_arbitrary_headers(s in ".{0,200}") {
            let _ = negotiate(Some(&s), &GZIP_DEFLATE);
        }

        #[test]
        fn result_is_always_from_available_or_none(s in ".{0,120}") {
            let result = negotiate(Some(&s), &GZIP_DEFLATE);
            if let Ok(Some(coding)) = result {
                prop_assert!(GZIP_DEFLATE.contains(&coding));
            }
        }
    }
}

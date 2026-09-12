//! [`DecodeLimits`]: bounds applied while decoding a response body.

/// Default [`DecodeLimits::max_codings`], and the internal bound
/// [`crate::header::parse_content_encoding`] enforces on its own (that
/// function takes no `DecodeLimits`, so it uses this constant directly —
/// see its rustdoc).
pub(crate) const DEFAULT_MAX_CODINGS: usize = 4;

/// Bounds applied while decoding a response body.
///
/// `max_output` is enforced **during** decoding by
/// [`Decoder`](crate::Decoder)'s push decoders — checked on every write
/// into the output sink, including mid-block — so a
/// decompression bomb is rejected before its bytes are materialised, not
/// after. It is the crate's **load-bearing** bomb control; every other field
/// here is defense-in-depth.
///
/// # `max_ratio` is a heuristic, not a security boundary
///
/// Measured legitimate output:input ratios (`oxiarc-http` design report §6.4):
///
/// ```text
/// html   100 MiB (repeated div)      ratio =  343.5x   <- legitimate
/// CSV     10 MiB (20 distinct rows)  ratio =  411.4x   <- legitimate
/// XML     10 MiB                     ratio =  411.4x   <- legitimate
/// log     10 MiB identical lines     ratio =  342.7x   <- legitimate
/// BOMB   gzip -9 of 100 MiB zeros    ratio = 1028.6x   <- hostile
/// ```
///
/// Legitimate traffic reaches 411x and the classic bomb is 1029x — a gap of
/// only 2.5x, and a bomb can trivially be tuned to any ratio below a chosen
/// threshold by padding the input. `max_ratio` **cannot** reliably
/// distinguish a 400x legitimate body from a 400x hostile one; it only
/// bounds wasted work per input byte in the narrow "slow drip" case where a
/// tiny input produces a large output before `max_output` trips. Lower
/// `max_output` for real protection; lowering only `max_ratio` buys very
/// little.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct DecodeLimits {
    /// Hard ceiling on total decoded bytes. Default **64 MiB**. Set
    /// `u64::MAX` to disable (not advised for untrusted peers). This is the
    /// bound that actually stops a decompression bomb.
    pub max_output: u64,
    /// Maximum output:input expansion ratio, or `None` to disable this
    /// (secondary, heuristic) guard entirely. Default `Some(1000.0)` —
    /// above every legitimate figure measured above and below the classic
    /// bomb's ~1029x, but a heuristic, not a boundary; see the type docs.
    pub max_ratio: Option<f64>,
    /// Maximum number of chained codings a *decoder* will accept from one
    /// `Content-Encoding` value. Default **4**. Guards against
    /// `gzip, gzip, gzip, ...` amplification chains.
    ///
    /// [`Decoder::new`](crate::Decoder::new) checks it before building a
    /// single stage, so an over-long chain costs nothing to refuse. Note
    /// that [`parse_content_encoding`](crate::parse_content_encoding) takes
    /// no `DecodeLimits` — its signature is fixed — and enforces the same
    /// **default** of 4 as a fixed internal bound, so raising this field
    /// does not raise what that function, or
    /// [`Decoder::from_header`](crate::Decoder::from_header) which calls
    /// it, will parse.
    pub max_codings: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_output: 64 * 1024 * 1024,
            max_ratio: Some(1000.0),
            max_codings: DEFAULT_MAX_CODINGS,
        }
    }
}

impl DecodeLimits {
    /// No limits at all. Only for trusted, local, or already-bounded input —
    /// removes the load-bearing `max_output` control along with everything
    /// else.
    pub fn unlimited() -> Self {
        Self {
            max_output: u64::MAX,
            max_ratio: None,
            max_codings: usize::MAX,
        }
    }

    /// Set [`max_output`](Self::max_output).
    #[must_use]
    pub fn with_max_output(mut self, n: u64) -> Self {
        self.max_output = n;
        self
    }

    /// Set [`max_ratio`](Self::max_ratio).
    #[must_use]
    pub fn with_max_ratio(mut self, r: Option<f64>) -> Self {
        self.max_ratio = r;
        self
    }

    /// Set [`max_codings`](Self::max_codings).
    #[must_use]
    pub fn with_max_codings(mut self, n: usize) -> Self {
        self.max_codings = n;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_documented_values() {
        let d = DecodeLimits::default();
        assert_eq!(d.max_output, 64 * 1024 * 1024);
        assert_eq!(d.max_ratio, Some(1000.0));
        assert_eq!(d.max_codings, 4);
    }

    #[test]
    fn unlimited_disables_everything() {
        let d = DecodeLimits::unlimited();
        assert_eq!(d.max_output, u64::MAX);
        assert_eq!(d.max_ratio, None);
    }

    #[test]
    fn builders_are_chainable() {
        let d = DecodeLimits::default()
            .with_max_output(1024)
            .with_max_ratio(None)
            .with_max_codings(1);
        assert_eq!(d.max_output, 1024);
        assert_eq!(d.max_ratio, None);
        assert_eq!(d.max_codings, 1);
    }
}

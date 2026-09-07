# oxiarc-http - Development Status (v0.4.2, 2026-09-07)

Part of the root `TODO.md` "Phase 8: HTTP Content-Coding, Incremental
Inflate, and Image Codecs" program — track **W1-H0 / H0** (headers,
negotiation, limits, error type, server-side encoding). Design source:
`scratchpad/reports/http-content-coding.md` + `scratchpad/reports/critique.md`
(P0-4, §3.3, §4), reconciled against the root `TODO.md` Phase 8 section,
which wins on any disagreement.

## Completed Features (Wave 1 / track H0)

### `ContentCoding` (`src/coding.rs`)
- [x] `Identity, Compress, Deflate, Gzip, Brotli, Zstd, Dcb, Dcz, Unknown(String)`,
      declared least-to-most-preferred (derived `Ord` is the contract —
      inserting a coding means placing it at the right position, not
      appending it)
- [x] `parse` (case-insensitive, infallible — an unrecognized token becomes
      `Unknown`, never an error) with the `x-gzip`/`x-compress` aliases, and
      `Display`
- [x] `is_decodable`/`is_encodable`: real, implemented capability, not just
      "the Cargo feature happens to be on" — `Compress` and `Dcb` are
      unconditionally `false` (see below)

### Header parsing (`src/header.rs`)
- [x] `QValue(u16)` thousandths — exact `parse`/`parse_param`, no `f32`,
      no `NaN`; `0`, `0.0`, `0.123`, `1`, `1.000` parse exactly; out-of-range
      and >3-fractional-digit values rejected
- [x] `parse_content_encoding`/`parse_content_encoding_all` — preserves
      application order (RFC 9110 §8.4: decode in reverse); `identity`
      dropped; unknown tokens become `ContentCoding::Unknown` rather than an
      error (see "Deviations" below); bounded against `Content-Encoding: gzip,
      gzip, gzip, ...` amplification (`LimitExceeded { kind: Codings }`)
- [x] `parse_accept_encoding` — RFC 9110 §5.6.1.2 empty-element tolerance,
      bounded (`HeaderSyntax`) against a hostile comma count; malformed
      `;q=` drops the entry, not a default `q=1`
- [x] Fuzzable: proptest-based no-panic checks over arbitrary input for
      every parser in this file

### `AcceptEncoding` (`src/accept.rs`)
- [x] `all_supported`/`new`/`add`/`with_q`/`remove`/`with_identity`/`with_wildcard`
- [x] `to_header_value() -> Option<String>` vs `to_header_value_or_empty()`,
      documented against the RFC 9110 trap that an empty value and an
      absent header mean opposites
- [x] Round-trips through `parse_accept_encoding` (proptest)

### `negotiate` (`src/negotiate.rs`)
- [x] RFC 9110 §12.5.3 implemented exactly: q=0 forbids, `*` wildcard
      (never reaching identity — see the function's rustdoc for the RFC
      prose's genuine ambiguity here and how the test table resolves it),
      identity acceptable by default unless excluded, ties broken by the
      *caller's* `available` order (not `ContentCoding`'s own `Ord`),
      absent header ⇒ any coding acceptable
- [x] Full negotiation table from the design report §10.6, all 15 rows,
      as tests — plus extras (available-order-vs-Ord tie-break, identity-tie
      cases, a caller-defined `Unknown` coding matched by token)
- [x] `Err(NotAcceptable)` for the 415 case (identity *and* every server
      coding excluded)

### `DecodeLimits` (`src/limits.rs`)
- [x] `max_output` (default 64 MiB, the load-bearing bomb control),
      `max_ratio: Option<f64>` (default `Some(1000.0)`, defense-in-depth —
      documented with the measured legitimate-vs-hostile ratios, 411x vs
      1029x, that justify it), `max_codings` (default 4)

### `HttpCodingError` (`src/error.rs`)
- [x] `UnsupportedCoding { token, reason }`, `LimitExceeded { limit, kind: LimitKind }`,
      `Corrupt { coding, source }`, `TrailingGarbage`, `HeaderSyntax`,
      `MissingDictionary` — `#[non_exhaustive]`, `thiserror`-derived,
      `Corrupt.source` is `Box<dyn Error + Send + Sync>` so this crate needs
      no direct dependency on every optional codec crate's own error type
- [x] `TrailingGarbage` and the `Output`/`Ratio` arms of `LimitKind` are
      constructed only by the future decoder wave — deliberately, so that
      wave needs no changes to this file (verified empirically: an
      unconstructed `#[non_exhaustive]` public enum variant in a lib crate
      is not a `dead_code` warning)

### Server-side encoding (`src/encode.rs`)
- [x] `EncodeOptions<'a>` (`level`, `brotli_quality`, `zstd_level`,
      `dictionary: Option<&'a [u8]>` — borrowed, not owned, so the type
      stays `Copy`)
- [x] `encode_body(coding, body, opts) -> Result<Vec<u8>>` — gzip/deflate
      via `oxiarc-deflate`, brotli/zstd via `oxiarc-brotli`/`oxiarc-zstd`,
      identity as a trivial copy; `Dcz` via `oxiarc_zstd::ZstdEncoder::set_dictionary`
      when a dictionary is supplied (Phase 8 owner decision #8), else
      `MissingDictionary`
- [x] `Encoder<W: Write>` — streaming wrapper dispatching to each sibling
      crate's own `*StreamEncoder`/`BrotliCompressor` (never reimplemented);
      `Dcz` streams through `ZstdStreamEncoder::with_dictionary`; every
      non-identity variant is boxed (a single unboxed codec-feature-alone
      build trips `clippy::large_enum_variant` against the tiny `Identity(W)`
      variant otherwise)
- [x] Round-trips through the sibling crates' own one-shot decoders for
      every coding (gzip, deflate, brotli, zstd, dcz)

### Cargo features
- [x] `default = ["gzip", "deflate"]`; `brotli`, `zstd`, `compress`,
      `async-io` all opt-in; every combination — `--no-default-features`,
      each feature alone, all-features — builds and is
      `clippy --all-targets -D warnings` clean (verified individually, not
      just for the two combinations the root Definition of Done names)

### Tests
- [x] 90 unit tests (co-located `#[cfg(test)] mod tests` per file,
      confirmed by grep to reference no private item — they exercise the
      `pub` surface exactly as an external integration test would) + 6
      doctests, all green under default, all-features and
      `--no-default-features`

## Deviations from the design report (and why)

The design report (`http-content-coding.md`) predates the P0 critique and
this track's own instructions in a few places; where they disagree, the
instructions given to this track win, and are recorded here so a future
reader does not "fix" them back:

- **`ContentCoding` gained `Dcb`/`Dcz`/`Unknown(String)`** (report §6.1 has
  6 variants; this crate has 9). `Unknown(String)` in particular changes
  `parse_content_encoding`'s contract: an unrecognized *Content-Encoding*
  token is no longer fatal (report: `Error::UnsupportedCoding`) — it
  becomes `ContentCoding::Unknown`, and whether that is fatal is left to
  whatever later tries to decode it. This is a deliberate, load-bearing
  consequence of the richer enum, not an oversight.
- **`ContentCoding` is not `Copy`** (report: `#[derive(..., Copy, ...)]`).
  Structurally forced by `Unknown(String)`; `as_str` correspondingly takes
  `&self` and returns `&str`, not `&'static str`.
- **`negotiate` returns `Result<Option<ContentCoding>, NotAcceptable>`**,
  matching the report (§6.5) and the negotiation table (§10.6, which
  contains `Err(NotAcceptable)` rows this track was explicitly told to
  reproduce as tests) — not the bare `Option<ContentCoding>` this track's
  own dispatch summary paraphrased it as. The table cannot be reproduced
  under the bare-`Option` signature, so the more specific, explicitly-cited
  source (the report section + its test table) won over the summary.
- **`encode_body`'s signature is `(coding: &ContentCoding, body: &[u8], opts: EncodeOptions) -> Result<Vec<u8>>`**,
  matching this track's literal instructions — not the report's
  `(body, coding, opts) -> Result<Option<Vec<u8>>>` (min-size short-circuit
  folded into the return type). No `negotiate_and_encode`/min-size
  convenience wrapper was added; it was not asked for, and adding one would
  have required inventing an error-type composition this track's contract
  does not specify.
- **`DecodeLimits` has exactly the three fields this track specified**
  (`max_output`, `max_ratio: Option<f64>`, `max_codings`) rather than the
  report's six (`ratio_grace_bytes`, `max_header_elements`,
  `trailing: TrailingData`). `max_header_elements`'s bound still exists —
  as an internal constant in `header.rs`, since `parse_content_encoding`'s
  specified signature takes no `DecodeLimits` parameter to carry it in.
  `TrailingData`/trailing-bytes policy is a decode-time concept with no
  construction site in this wave (see `HttpCodingError::TrailingGarbage`
  above) and is left for the decoder wave to add, plumbing included.

## Out of scope for this wave (the decoder track)

Everything below needs the resumable push decoders
(`oxiarc_deflate::stream::InflateStream`, `oxiarc_brotli::BrotliStream`,
`oxiarc_zstd::ZstdStream`) landing first, per this track's own brief. The
module boundary above is drawn so adding it touches only new files plus a
handful of `pub use` lines in `lib.rs`:

- [ ] `Decoder` (push-style) / `CodingDecoder` private trait chaining
      gzip → deflate(sniffed) → brotli → zstd → dcz in RFC 9110 §8.4 reverse
      order
- [ ] `DecodedBody<R: Read + BufRead>` and, behind `async-io`,
      `AsyncDecodedBody`
- [ ] `decode_body` one-shot convenience
- [ ] `compress`/`x-compress` LZW decode of `.Z` bodies — blocked on
      `oxiarc-lzw` growing 16-bit codes (`LzwConfig::validate` still hard-caps
      `max_bits` at 12 as of this wave — reconfirmed empirically, not just
      cited from the design report); `ContentCoding::Compress::is_decodable()`
      stays `false` until that lands, regardless of the `compress` feature
- [ ] `dcb` real decode — blocked on `oxiarc-brotli` growing shared-dictionary
      support (Phase 8 owner decision #8); stays `Unsupported`
- [ ] Recipes as `examples/` for ureq/reqwest/oxihttp (report §11) — every
      one of them needs `Decoder`/`DecodedBody`, so none could be written
      truthfully yet
- [ ] `fuzz_http_decode`/`fuzz_http_headers`/... cargo-fuzz targets — listed
      under the root TODO's Wave 3, not this track

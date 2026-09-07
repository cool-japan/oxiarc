# oxiarc-http - Development Status (v0.4.2, 2026-09-07)

Part of the root `TODO.md` "Phase 8: HTTP Content-Coding, Incremental
Inflate, and Image Codecs" program — tracks **W1-H0** (headers, negotiation,
limits, error type, server-side encoding) and **W2-HTTP** (the decoders and
body adapters). Both are complete. Design source:
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
      cases, a caller-defined `Unknown` coding matched by token, and an
      `Identity`-listed-explicitly-in-`available` pair covering the fix
      below)
- [x] `Err(NotAcceptable)` for the 415 case (identity *and* every server
      coding excluded)
- [x] Rule-1 (absent header) fix: the early return used to hand back
      `available.first().cloned()` unnormalized, so a caller who listed
      [`ContentCoding::Identity`] explicitly as `available`'s first entry
      (allowed — the docs only say it "need not" be listed) got a literal
      `Ok(Some(ContentCoding::Identity))`, breaking the "`Some(_)` always
      names something to put in a `Content-Encoding` header" contract every
      other return path in the function upholds. Now routed through the
      same `Identity -> None` normalization; regression tests
      `absent_header_with_identity_first_in_available_normalizes_to_none`/
      `..._not_first_in_available_uses_first`. A `debug_assert` right before
      the final `Err(NotAcceptable)` fallback block formalizes the
      neighbouring comment's invariant (`best` is unset at that point only
      when identity itself was explicitly refused).
- [x] **Verifier addition:** the rule-1 fix above created an asymmetry
      (`Identity` first in `available` decides the absent-header case but
      is still considered last under every other path, so `"*"` picks the
      first *non*-identity entry). Both halves are now stated in
      `available`'s rustdoc and pinned by
      `identity_in_available_only_wins_when_the_client_stated_no_preference`,
      so neither can be "fixed" into inverting the other. Also pinned: an
      OWS-only header value is the empty-value case, and a header over the
      64-segment DoS bound degrades to `Ok(None)` (send it uncompressed) —
      never to a coding the client did not ask for

### `DecodeLimits` (`src/limits.rs`)
- [x] `max_output` (default 64 MiB, the load-bearing bomb control),
      `max_ratio: Option<f64>` (default `Some(1000.0)`, defense-in-depth —
      documented with the measured legitimate-vs-hostile ratios, 411x vs
      1029x, that justify it), `max_codings` (default 4)
- [x] **Verifier doc fix:** `max_codings` used to read as if it configured
      `parse_content_encoding`, which takes no `DecodeLimits` and enforces
      the same default of 4 as a fixed internal bound. Raising the field
      does not raise what that function parses, and the field docs now say
      so rather than leaving a caller to discover it. (Every field is now
      read for real by the W2-HTTP decoder; `max_output` is enforced inside
      a DEFLATE block, not between blocks — see `decode/sink.rs`.)

### `HttpCodingError` (`src/error.rs`)
- [x] `UnsupportedCoding { token, reason }`, `LimitExceeded { limit, kind: LimitKind }`,
      `Corrupt { coding, source }`, `TrailingGarbage`, `HeaderSyntax`,
      `MissingDictionary` — `#[non_exhaustive]`, `thiserror`-derived,
      `Corrupt.source` is `Box<dyn Error + Send + Sync>` so this crate needs
      no direct dependency on every optional codec crate's own error type
- [x] `TrailingGarbage` and the `Output`/`Ratio` arms of `LimitKind` were
      declared here by track H0 ahead of the decoder that constructs them,
      so that wave needed no changes to this file — and it needed exactly
      one: W2-HTTP added `LimitKind::Window { declared }` for a stream that
      declares an over-large sliding window, which is refused before
      allocation and is not an output overrun (additive; the enum is
      `#[non_exhaustive]`)

### Server-side encoding (`src/encode.rs`)
- [x] `EncodeOptions<'a>` (`level`, `brotli_quality`, `zstd_level`,
      `dictionary: Option<&'a [u8]>` — borrowed, not owned, so the type
      stays `Copy`), with `new()` + `with_level`/`with_brotli_quality`/
      `with_zstd_level`/`with_dictionary` builders. **Verifier fix:** the
      struct is `#[non_exhaustive]` and originally had *no* constructor
      beyond `Default`, so a downstream crate could not use
      struct-expression syntax at all (E0639 — not even
      `..EncodeOptions::default()`) and the only remaining path,
      `let mut o = EncodeOptions::default(); o.dictionary = …`, trips
      `clippy::field_reassign_with_default` in the caller. That made the
      `dcz` support Phase 8 owner decision #8 mandates unreachable from
      idiomatic downstream code; the crate's own tests missed it because
      *inside* the crate the struct-expression form is legal. Guarded now
      by doctests (rustdoc compiles each as an external crate), not only
      unit tests
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
      every coding (gzip, deflate, brotli, zstd, dcz), one-shot **and**
      streaming, plus empty bodies, byte-at-a-time writes with interleaved
      empty writes, and out-of-range `level`/`brotli_quality`/`zstd_level`
- [x] **Verifier fix — `Encoder` framing is documented, not assumed
      uniform.** `flush()` means something different per coding, and for
      zstd it is a silent-truncation hazard: `ZstdStreamEncoder` closes a
      complete Zstandard frame on every `flush()` *and* automatically every
      128 KiB of buffered input, so a body streamed in chunks past that
      boundary is multi-frame **with no explicit flush at all**. Measured:
      10 x 64 KiB through `Encoder(Zstd)` yields a stream from which
      `oxiarc_zstd::decompress` returns `Ok` with exactly 131072 of 655360
      bytes — the first frame — with no error, while
      `decompress_multi_frame` recovers all of it. Legal per RFC 8878 §3
      (frames may be appended) and unavoidable inside this crate without
      giving up bounded-memory streaming, so it is now a documented
      framing table on `Encoder` plus regression tests that pin both the
      multi-frame property and the exact multi-frame round-trip.
      `encode_body` is unaffected — it always emits one frame

### Cargo features
- [x] `default = ["gzip", "deflate"]`; `brotli`, `zstd`, `compress`,
      `async-io` all opt-in; every combination — `--no-default-features`,
      each feature alone, all-features — builds and is
      `clippy --all-targets -D warnings` clean (verified individually, not
      just for the two combinations the root Definition of Done names)

### Tests
- [x] 111 unit tests (co-located `#[cfg(test)] mod tests` per file,
      referencing no private item) + 8 doctests, all green under
      `--no-default-features` (90), default (99), `brotli` alone (94),
      `zstd` alone (99) and `--all-features` (111)
- [x] **Verifier correction:** a co-located test is *not* equivalent to an
      external integration test, and this crate was bitten by exactly that
      (see `EncodeOptions` above) — code inside the crate may build a
      `#[non_exhaustive]` struct with struct-expression syntax and a
      downstream crate may not. Doctests are the guard for that class,
      because rustdoc compiles each one as its own external crate; the
      claim in the previous revision of this file and of `README.md` that
      the unit tests cover the external surface "exactly as an external
      integration test would" was wrong and has been corrected in both. The
      replacement rule is narrower than a blanket "everything gets a
      doctest": anything whose correctness depends on being *constructible
      or callable from outside* must carry one, as every `EncodeOptions`
      builder now does

## Deviations from the design report (and why)

**W2-HTTP additions to this list:**

1. **The bench gate is measured against `gzip_decompress`, not raw
   `inflate()`.** The track's literal gate was ">= 0.95x raw `inflate()`
   throughput". That is unachievable by *any* gzip decoder: `oxiarc-deflate`'s
   own one-shot `gzip_decompress` reaches only **0.84x** of `inflate()` on the
   same payload, because it walks the container and computes a CRC-32 over the
   whole output. Measured with an interleaved A/B harness (criterion runs its
   groups minutes apart, which on a loaded machine swings by 30%), medians of
   40 rounds over a 4 MiB body: raw `inflate()` 1.609 GiB/s, `gzip_decompress`
   1.349 GiB/s, `decode_body` 1.297 GiB/s (**0.96x** of `gzip_decompress`,
   0.81x of `inflate()`), `DecodedBody` 0.96x, `feed_into` at 64 KiB **0.98x**.
   The container-matched baseline is the one that measures this crate, and by
   it the 0.95x gate is met with margin.
2. **`ureq` and `reqwest` are not dev-dependencies.** The track allowed
   compile-checking the ureq recipe with `ureq` as a dev-dep *only if* it
   passes `cargo deny check bans`. It would not reliably: `cargo deny` walks
   dev-dependencies, and `Cargo.lock` is feature-agnostic — it records a
   crate's optional dependencies whether or not their feature is enabled — so
   `ureq`'s optional `flate2` would land in this workspace's lockfile even
   under `default-features = false`. The recipes ship as `examples/` with the
   real wiring in module docs and a runnable canned-response demo, which is
   the fallback the track named.
3. **`TrailingData` has no effect on `br`.** `oxiarc-brotli` detects trailing
   data inside the codec (RFC 7932 has no length field or checksum, so the
   final zero-padding check is the only end-of-stream signal), so a `br` body
   with a tail is `Corrupt`, not `TrailingGarbage`, under every policy. That
   is the strict direction, so no policy is weakened. Documented on
   `TrailingData`, pinned by a test.
4. **`TrailingGarbage.count` is 1 for the DEFLATE family**, not the true
   total. The container reports the first offending byte; it never counted a
   total. The field's docs already say the count is "not necessarily the true
   total".
5. **`LimitKind::Window { declared }` is a new variant** (additive, the enum
   is `#[non_exhaustive]`). A stream declaring an over-large sliding window is
   refused before allocation, and calling that "decoded output exceeds the
   limit" would have been misleading.
6. **`Decoder::identity()` is unlimited.** A byte-for-byte copy cannot be a
   decompression bomb, and capping it at the 64 MiB default would make a plain
   100 MB download fail for no reason. `Decoder::new(&[], &limits)` is the
   bounded pass-through.
7. **`decode_body` takes `&[ContentCoding]`** (the track's literal signature),
   with `decode_body_from_header(&str, ..)` alongside it — the shape the
   `oxihttp` recipe and every real call site actually wants, since a response
   header value is a `&str`.
8. **`DecodedBody::read_to_string()` shadows `Read::read_to_string`.** It
   takes no argument and returns the `String`, matching `ureq`'s
   `Body::read_to_string()` and the design report. The mismatch with the trait
   method is a compile error, never a silent change of meaning, and both the
   method's docs and the crate docs say so.

### Track H0's original list

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
  folded into the return type). *Wave 2 update:* the
  `negotiate_and_encode` convenience wrapper now exists (W2-HTTP asked for
  it explicitly) with its own `NegotiateEncodeError`; `encode_body`'s
  signature is unchanged.
- **`DecodeLimits` has exactly the three fields this track specified**
  (`max_output`, `max_ratio: Option<f64>`, `max_codings`) rather than the
  report's six (`ratio_grace_bytes`, `max_header_elements`,
  `trailing: TrailingData`). `max_header_elements`'s bound still exists —
  as an internal constant in `header.rs`, since `parse_content_encoding`'s
  specified signature takes no `DecodeLimits` parameter to carry it in.
  `TrailingData`/trailing-bytes policy was a decode-time concept with no
  construction site in wave 1 (see `HttpCodingError::TrailingGarbage`
  above). *Wave 2 update:* `TrailingData` now exists in `decode`, selected
  per-decoder by `Decoder::trailing_data` rather than as a fourth
  `DecodeLimits` field, so the limits type still has exactly three.
- **`compress` stays a real, buildable Cargo feature, not a `compile_error!`**
  (report §7.6: "should fail to build with a `compile_error!` pointing at
  P-4 until `oxiarc-lzw` grows 16-bit codes"). A `compile_error!` would
  make `cargo build -p oxiarc-http --features compress` (and the
  feature-alone matrix this track's own Definition of Done and root TODO
  Wave-3 gates require to stay green) fail outright — the wrong trade for
  a feature whose only present effect is "the `dep:oxiarc-lzw` optional
  dependency compiles in," since `ContentCoding::Compress::is_decodable()`/
  `is_encodable()` are unconditionally `false` regardless of this feature
  (see `coding.rs`) until `oxiarc-lzw` actually grows `.Z` support. The
  reasoning lived only as a `Cargo.toml` comment until this line.

## Completed Features (Wave 2 / track W2-HTTP: the decoders)

### The decode chain (`src/decode/`)
- [x] Private `CodingDecoder` trait (`decode/coding.rs`) — the one place the
      three codec crates are unified (Phase 8 owner decision #10: no
      `oxiarc-core::stream`, no blanket `Decompressor` impl). Its shape
      mirrors `InflateStream::inflate` / `BrotliStream::decode` /
      `ZstdStream::decode` field for field, so no stage buffers a body
- [x] `gzip` / `x-gzip` (`decode/deflate.rs`) — `WrappedInflate(Gzip)`,
      `multi_member(true)` (RFC 1952 §2.2), `strict_first_member(true)`
      (unlike the legacy `GzipStreamDecoder`, which must keep returning
      `Ok(0)` — Phase 8 owner decision #2), `FHCRC` verified, trailer
      CRC-32 + `ISIZE` verified, reserved `FLG` bits rejected
- [x] `deflate` — `WrappedInflate(Auto)`: gzip magic, else a valid zlib
      header, else raw RFC 1951, decided once at offset 0. All three
      spellings servers actually send are accepted, including a whole gzip
      stream mislabelled `deflate`; `FDICT` is refused cleanly (it cannot be
      satisfied over HTTP)
- [x] `br` (`decode/brotli.rs`) — `BrotliStream`, window ceiling 16 MiB
      checked before allocation
- [x] `zstd` / `dcz` (`decode/zstd.rs`) — `ZstdStream`, multi-frame,
      skippable frames ignored, 8 MiB window ceiling (the largest an HTTP
      `zstd` decoder must support) refused *before* the ring is sized, an
      over-cap declared `Frame_Content_Size` refused before decoding, and
      `dcz` against a caller-supplied dictionary
- [x] `identity` as a real stage, so a pass-through body shares one code
      path with every other coding
- [x] `Decoder` (`decode/mod.rs`) — codings applied in RFC 9110 §8.4 reverse
      order, one fixed 64 KiB buffer per intermediate stage (a single coding
      allocates none), slice-primary `decode(input, output, flush)`,
      `feed_into`/`feed`/`finish_into`/`finish`/`close`, `output_len`,
      `input_len`, `is_finished`, `codings`, `limits`
- [x] `decode_body` / `decode_body_from_header` one-shot helpers
- [x] `TrailingData::{Reject (default), AllowZeros, Ignore}`

### `LimitedSink` (`src/decode/sink.rs`)
- [x] `max_output` enforced by **truncating the output slice** given to the
      codec, so `oxiarc-deflate`'s own bounded sink enforces the HTTP budget
      on every literal and match copy — i.e. *inside* a single DEFLATE block.
      Overflow is unambiguous: a stage that answers `NeedOutput` at budget 0
      with real space offered has output it cannot place
- [x] One sink **per stage**: an intermediate stage of `gzip, gzip` can be a
      bomb even when the final body is small; `max_codings` bounds total work
- [x] `max_ratio` as defense-in-depth, with a 1 MiB grace window (critique
      §H-6 resolved the report's disagreement in favour of 1 MiB)

### Body adapters (`src/read.rs`, `src/async_read.rs`)
- [x] `DecodedBody<R: Read>` — `Read` **and** a real `BufRead` (so `.lines()`
      works on a gzip body), 64 KiB wire + 64 KiB decoded staging (mandatory,
      not an optimisation), `Interrupted` retried, `WouldBlock` propagated
      and never turned into `Ok(0)`, inner `Ok(0)` switches the flush to
      `Finish`, EOF runs `close()` so a truncated body is an `io::Error` from
      the final read
- [x] `AsyncDecodedBody<R: AsyncRead>` (feature `async-io`) — same contract,
      `Unpin` + `Pin::new` rather than a projection crate; staged output is
      served before the source is polled, so a `Poll::Pending` source can
      never look like an empty body

### Server side (`src/encode.rs`)
- [x] `negotiate_and_encode` + `NegotiateEncodeError` — negotiation and
      encoding in one call, with the three header obligations
      (`Content-Encoding`, `Content-Length`, **`Vary: Accept-Encoding`**)
      spelled out in the rustdoc

### Tests, examples, benches
- [x] 249 tests all-features (198 default, 139 `--no-default-features`) +
      17 doctests; clippy-clean in **every** feature combination
- [x] `tests/limits.rs` regenerates the design report's 812 KB → 123 MiB
      **single-block** bomb from a committed Rust generator
      (`tests/common/single_block_bomb`) and proves a 1 MiB cap stops it
      mid-block — the claim the critique (§5) flagged as resting on an
      ephemeral script
- [x] `tests/allocations.rs` — counting `#[global_allocator]`: streaming a
      16 MiB body peaks at **214,946 bytes**; `feed_into` performs **zero**
      allocations per call after warm-up; a refused 123 MiB bomb peaks at
      1.15 MB
- [x] `tests/chunking.rs` — byte-at-a-time, ten chunk sizes, a one-byte
      output slice, and proptests over arbitrary split points and arbitrary
      payloads
- [x] `tests/http_oracle.rs` (feature `http-oracle`) — CPython `zlib`/`gzip`
      and the reference `brotli`/`zstd` CLIs, self-skipping. All seven ran
      for real on the development machine
- [x] `examples/` — `ureq3_manual_gzip`, `reqwest_bytes_stream`,
      `oxihttp_client`, `fuzz_seeds` (129 seeds across the seven wave-3 fuzz
      targets)
- [x] `benches/decode_bench.rs` — **0.96–0.98x** of
      `oxiarc_deflate::gzip_decompress` (see "Deviations")

## Out of scope (still blocked upstream)

- [ ] `compress`/`x-compress` LZW decode of `.Z` bodies — blocked on
      `oxiarc-lzw` growing 16-bit codes (`LzwConfig::validate` still hard-caps
      `max_bits` at 12 — reconfirmed empirically, not just cited from the
      design report); `ContentCoding::Compress::is_decodable()` stays `false`
      until that lands, regardless of the `compress` feature
- [ ] `dcb` real decode — blocked on `oxiarc-brotli` growing shared-dictionary
      support (Phase 8 owner decision #8); stays `Unsupported`, and never
      falls back to plain `br`
- [ ] `fuzz_http_*` cargo-fuzz targets — the root TODO's Wave 3. The seed
      generator (`examples/fuzz_seeds.rs`) and the invariants each target
      should assert (`tests/fuzz_seeds.rs`) are done here; `fuzz/Cargo.toml`
      needs `oxiarc-http` added to its `[dependencies]`
- [ ] `Transfer-Encoding` — deliberately out of scope (Phase 8 owner
      decision #8); the transport owns chunked framing

# Changelog

All notable changes to the OxiArc project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.2] - Unreleased

### Added

- **`oxiarc-tiff`: CCITT Group 3/4 uncompressed mode (ITU-T T.4 §4.2.1.3.2,
  Table 5/T.4), both directions.** The mode transmits pixels at about one bit
  each instead of Huffman-coding runs, which is the difference between shrinking
  and *expanding* a dithered or halftoned bilevel page. Decode is unconditional
  — the entrance code (`0000001111` on a two-dimensionally coded line,
  `000000001111` on a one-dimensionally coded one) is unambiguous, so a file
  that carries the mode without setting `T4Options` bit 1 / `T6Options` bit 1
  still decodes, where before it was a named error. Encode is opt-in through the
  new `ImageSpec::with_ccitt_uncompressed(bool)`, which also writes the option
  bit into whichever of tags 292/293 the codec owns; the encoder prices the
  Huffman coding and the uncompressed coding of every row exactly and takes the
  mode only where it is strictly smaller, so enabling it can only shrink a page.
  A 512x64 dithered page more than halves in all three dialects, and a page of
  long runs comes out byte-identical to one written with the mode off.
  **Interoperability**: libtiff 4.7.1 *parses* these files (`tiffinfo` prints
  "Group 4 Options: uncompressed data") but cannot decode them
  (`Fax4Decode: Uncompressed data (not supported)`), which is why the mode is
  never written unless asked for.
- **`oxiarc-tiff`: `ImageSpec::with_jpeg_restart_rows(u16)` and
  `CodecContext::jpeg_restart_rows`**, writing a `DRI` segment and `RSTn`
  markers every *n* MCU rows (`cjpeg -restart n`). `0`, the default, writes
  none, which is what libtiff writes. libtiff reads the result.
- **`oxiarc-jpeg`: two-component frames (`ColorSpace::Unknown(2)`), libjpeg's
  `JCS_UNKNOWN` layout.** Sequential identifiers `1`/`2`, one shared
  quantisation and Huffman slot, no subsampling, no colour transform, and
  neither a `JFIF` nor an Adobe marker (libjpeg writes neither for
  `JCS_UNKNOWN`). `InputColor::LumaAlpha` reaches it by asking for it
  explicitly — its default target is still one-component `Luma`, which
  keeps dropping the alpha channel exactly as before. The decoder already
  handled any component count outside `1..=4` generically; only the
  encoder's `component_template` needed the one new row. No external
  libjpeg tool can *decode* a two-component stream (none has an output path
  for a colour space it cannot map to grayscale or RGB — checked against
  `djpeg`, `tjbench` and Pillow), so this is verified by decomposing a
  two-component source into its two channels and checking each alone
  against real `cjpeg`/`djpeg`, by `djpeg -verbose`'s frame/scan header
  trace, and — closing the remaining gap — by embedding one in a TIFF page
  and confirming libtiff's *own* embedded libjpeg decodes the interleaved
  scan byte-identically to this crate's decoder (`tiffcp -c none`; see the
  `oxiarc-tiff` entry below). This is what
  `oxiarc-tiff`'s two-channel chunky JPEG refusal, added earlier in this
  same unreleased version, was waiting on.

### Changed

- **`oxiarc-zstd`: decode throughput rebuilt around the reference decoder's
  data layout.** The speed-up is strongly shape-dependent — it is large exactly
  where the decoder was doing per-byte work, and small where it was already
  bound by `memcpy` (see the table below). The motivating
  measurement came from the TIFF track: on a 4096x4096 RGB8 page in 16-row
  strips, our ZSTD strip decode delivered ~110 MB/s where `libzstd` inside
  `tiffcp` was an order of magnitude ahead, the largest gap in the whole codec
  matrix. Profiling found the cost was almost never in the entropy decoding
  itself but in how the decoder reached the bits and moved the bytes:
  `FseBitReader` gathered up to five bounds-checked byte loads *per bit-field
  read* (a Huffman literals stream calls it once per output byte, a sequence
  stream six to nine times per sequence); an overlapping match copied
  `out[i % offset]` one byte at a time, an integer division per output byte;
  the window ring reduced every index with `% cap`, and `cap` is not a power of
  two, so each was a real `udiv`; short literal and match runs went through
  `memcpy`/`memmove` **calls** whose overhead dwarfed the 3-20 bytes they moved
  (`_platform_memmove` was **51 %** of a 50 MB text decode); the window
  re-allocated and re-zeroed on every growth step (`2x` the final capacity in
  `bzero` over a doubling sequence); `xxhash`'s `read_u64_le` was eight
  bounds-checked byte loads and was not being inlined; and the legacy
  `ZstdDecoder` allocated a fresh literals and sequences `Vec` for **every
  block**. All of that is gone. What replaces it: a 64-bit bit container with
  bulk refills (`src/backward_bits.rs`, modelled on the reference
  `BIT_DStream_t`), split into a register-resident `BitCursor` a decode loop
  keeps out of memory; **reloads on a fixed schedule instead of a
  data-dependent test** (four Huffman symbols per stream per reload; two
  reloads per sequence), because that test is a mispredicting branch; the four
  Huffman literal streams decoded **in lockstep**, which is what RFC 8878's
  four-stream layout exists for — four independent `peek -> table load -> skip`
  chains instead of one; pattern-doubling overlapping-match copies on both
  decode paths (`offset`, `2*offset`, `4*offset`, ...); a new `src/short_copy.rs`
  of call-free fixed-width copies for short runs; one conditional subtraction
  in place of every ring `%`; in-place ring growth that zeroes only the new
  tail; and scratch buffers reused across blocks on the legacy path too.
  **No output changed**: the `zstd-oracle` differential suite (reference `zstd`
  1.5.7, both directions), the embedded OxiGDAL corpus, the mutation and
  truncation sweeps and every ZSTD4 accept/refuse test are unchanged and green,
  and two new differentials pin the fast paths against the implementations they
  replace — `literals::tests::the_interleaved_pass_agrees_with_the_checked_decoder`
  (interleaved vs checked, over real encoder-produced four-stream sections) and
  `frame`/`window`'s `..._matches_the_byte_at_a_time_definition` (pattern
  doubling vs `out.push(out[len - offset])`, every offset x length combination),
  plus a differential oracle in `backward_bits` that replays the byte-gathering
  reader's exact `peek`/`bits_remaining`/`is_overflowed`/`is_finished` at every
  step. Public API unchanged. New `examples/decode_throughput.rs` measures the
  whole matrix against `zstd -b -d` in interleaved rounds; `benches/stream_bench.rs`
  gained a `zstd_shape/*` group over the same shapes.

- **`oxiarc-zstd`: `decompress_multi_frame` no longer tolerates *leading*
  garbage, and a recognised-but-truncated frame is an error wherever it sits.**
  Trailing tolerance is unchanged and now stated exactly in the doc comment:
  bytes that start no recognisable frame end the stream gracefully **after at
  least one complete frame has been decoded**; the same bytes before any frame
  are an error. A truncated skippable frame (magic with no size field, or a
  payload that runs off the end) is an error on both paths — a recognised frame
  start is never trailing garbage. A complete skippable frame still does not
  count as a decoded frame, so `[skippable]` alone remains an empty stream
  while `[skippable][2 stray bytes]` is now an error. Callers that relied on
  `Ok(vec![])` for non-zstd input must check the error instead.
- **`oxiarc-zstd`: the declared-`Window_Size` policy is now explicit, and
  differs by entry point on purpose.** `ZstdStream` keeps a real sliding-window
  ring, so it refuses a declaration above `with_max_window` (8 MiB by default)
  before allocating; the unbounded `decompress` / `decompress_multi_frame` keep
  no ring — their output `Vec` *is* the window — so they accept any declaration,
  as they always have; and `decompress_with_limit` /
  `decompress_multi_frame_with_limit` now refuse a declaration above
  `max(max_output, 128 MiB)`, 128 MiB being the reference decoder's own
  `ZSTD_WINDOWLOG_MAX_DEFAULT` (`zstd -d` rejects a 2 GiB-window frame with
  "Window size larger than maximum : 2147483648 > 134217728"). The ceiling is
  deliberately **not** the caller's output limit: measured against `zstd` 1.5.7,
  a payload piped through `-3` declares a 2 MiB window and one piped through
  `--long=24 -6` declares 16 MiB — whatever the payload's length, and with no
  `Frame_Content_Size` — so `Window_Size > limit` would reject ordinary
  reference frames. A declared `Frame_Content_Size` past the limit is still
  refused before anything is decoded. `decompress_into` stays unrestricted: a
  container's chunk already bounds it.
- **`oxiarc-tiff` now encodes `Compression = 7` (JPEG) through
  `oxiarc_jpeg::Encoder`** instead of the baseline encoder it carried while
  `oxiarc-jpeg` had none. `compression/jpeg/encode.rs` went from 659 lines of
  DCT, Huffman and downsampling code to 334 lines that map a TIFF
  `CodecContext` onto `oxiarc_jpeg::EncodeOptions` and drive
  `write_tables_only` / `encode_scan_only` / `encode`. `JPEGTables` (tag 347)
  is unchanged — verified byte-identical to the old encoder's for gray, YCbCr
  4:2:2, RGB and CMYK at qualities 1, 10, 25, 50, 75, 95 and 100 before the
  swap, and now checked byte-identical to `tiffcp -c jpeg`'s in the
  `tiff-oracle` suite. The strips' marker order follows libjpeg's
  (`SOI DQT SOF DHT SOS`) rather than the old `SOI DQT DHT SOF SOS`; both are
  conformant abbreviated datastreams and libtiff, Pillow and `tifffile` read
  either. A chunk with exactly two channels was a named error for part of this
  same unreleased version and never in a published one — the swap's local
  encoder had produced a two-component frame that `oxiarc-jpeg` could not —
  and is fixed below, in the same version, rather than shipped and documented
  as a regression: see the `oxiarc-jpeg` two-component entry above.
  `ImageSpec::validate`'s refusal (and the `plan()` arm that produced it) are
  both gone; a greyscale-plus-alpha *chunky* JPEG page now round-trips
  (`tests/roundtrip.rs::a_two_channel_jpeg_page_round_trips_chunky_through_
  jcs_unknown`), and so does `PlanarConfiguration::Planar` (each channel its
  own single-component frame), which remains a legitimate way to write the
  same page. Verified against real libtiff: `tiffinfo` parses the chunky
  file cleanly, and `tiffcp -c none` makes libtiff's own embedded libjpeg
  decode the two-component scan byte-identically to this crate's decoder.
- **`oxiarc-tiff` per-image codec scratch is now pooled rather than held in a
  single slot.** The reusable `WrappedInflate`, `ZstdStream`, `XzDecoder` and
  CCITT changing-element buffers used to live behind one `Mutex` each, held for
  the length of a chunk's decode — correct, but it made a `rayon` decode of a
  Deflate, CCITT, ZSTD or LZMA page serialise every worker behind the codec. A
  pool hands each worker a decoder of its own and locks only around the
  hand-off. Measured, interleaved, 4096x4096, medians of nine rounds: Deflate
  went from 0.98x to **2.24x**, Group 4 from no gain to **3.04x**, Group 3 2D
  to **2.03x**, LZW to 2.13x. Serial decode is unchanged (the pool holds
  exactly one entry) and output is byte-identical either way, which
  `tests/codec_reuse.rs` now asserts for all four codecs, including eight
  threads decoding one page through one shared `CodecState`.

### Fixed

- **`oxiarc-zstd`: the legacy one-shot decoders accepted three classes of frame
  the RFC forbids.** Differential fuzzing of `decompress_multi_frame` against
  the new `ZstdStream` (`fuzz/fuzz_targets/fuzz_zstd_stream.rs`) found four
  independently-rooted inputs, in about ten cumulative minutes, that the legacy
  `ZstdDecoder::decode_frame` core accepted and the hardened push decoder
  refused. Three were real defects, now fixed *in shared code* so the two paths
  cannot drift again:
  1. **`Dictionary_ID` was never validated.** A frame naming a dictionary the
     caller never supplied (RFC 8878 §3.1.1.1.1.6) decoded to silently wrong
     bytes — its matches reach into content the decoder does not have. Every
     entry point (`decompress`, `decompress_frame`, `decompress_multi_frame`,
     `ZstdDecoder::decode_frame`, `decompress_with_limit`) now refuses it with
     the same `InvalidHeader` the streaming decoder already used;
     `Dictionary_ID` 0 still means "no dictionary", and the `*_with_dict`
     entries are unaffected.
  2. **`Block_Maximum_Decompressed_Size` was never enforced.** A block may not
     regenerate more than `min(Window_Size, 128 KiB)`, further bounded by a
     declared `Frame_Content_Size`. `Raw`/`RLE` blocks are now charged from the
     block header — before an RLE block expands — and `Compressed` blocks
     inside the sequence executor, so an over-large block is refused without
     first materialising it.
  3. **Leading garbage was tolerated.** `decompress_multi_frame`'s loop stopped
     and returned `Ok(accumulated)` on fewer than four bytes, an unknown magic,
     or a truncated skippable frame *at any position, including the very
     first* — so `decompress_multi_frame(b"not zstd at all")` was `Ok(vec![])`.
     Those are now errors before any frame has been decoded, matching
     `ZstdStream` byte for byte.
  The fourth finding, an ~11 MB declared `Window_Size`, is a deliberate split
  and is now documented as one (see *Changed*). Pinned by
  `oxiarc-zstd/tests/legacy_hardening.rs` — 16 tests that drive **both** paths
  over every case, including the fuzzer's own minimised artifacts, and assert
  identical accept/refuse plus identical bytes whenever both accept, and by
  `oxiarc-zstd/tests/legacy_verify.rs` (below).

- **`oxiarc-zstd`: a reused `ZstdDecoder` carried a failed frame's partial
  output into the next one.** `ZstdDecoder::decode_frame` took its output
  buffer only on success, so after a truncated or corrupt frame the buffer
  still held whatever that frame had already regenerated, and the *next*
  `decode_frame` on the same decoder returned it prepended to the new frame's
  content. With a checksum on the following frame the symptom was a bogus
  `CrcMismatch`; with neither a checksum nor a `Frame_Content_Size` — what
  `zstd --no-check` writes from a pipe — it was silent: 131 076 bytes returned
  as `Ok` where 4 were expected. `decode_frame` now resets the per-frame state
  (output buffer plus the literals Huffman table and the three sequence FSE
  tables, so a `Treeless`/`Repeat` block at the start of a new frame is
  rejected rather than decoded with the previous frame's tables) at the start
  of every call, exactly as `ZstdStream::begin_frame` does; a configured
  dictionary survives. `ZstdDecoder::reset` stays public and is no longer
  something a caller has to remember. The one-shot free functions were never
  affected — they build a fresh decoder per frame.

- **`oxiarc-zstd`: the legacy one-shot decoders refused a skippable frame
  placed in front of a Zstandard frame.** `zstd -d` decodes
  `[skippable][frame]` exactly like `[frame]`, and so do `ZstdStream`,
  `decompress_into` and `decompress_with_limit` — but `decompress`,
  `decompress_frame` and `ZstdDecoder::decode_frame` stopped at the skippable
  magic with `InvalidMagic`, so a container that prefixes its payload with
  metadata decoded on one path and failed on the other. All of them now walk
  past a complete skippable-frame prefix (RFC 8878 §3.1.2); `decompress_frame`
  reports it in the byte count it returns, so walking a concatenated stream
  still lands on the next frame. A *truncated* skippable frame remains an
  error wherever it sits, and leading bytes that are not a recognised frame
  start remain an error. Found by the adversarial sweep in
  `oxiarc-zstd/tests/legacy_verify.rs`, which now pins the whole matrix: 8000+
  crafted frames over every `Window_Descriptor` byte and 5500+ truncated,
  mutated and spliced inputs, asserting the legacy and streaming families
  reach the same verdict with no carve-out beyond the documented
  declared-window split.

- **`oxiarc-tiff`: CCITT Group 4 decode was quadratic in the number of runs
  per row.** The two-dimensional row decoder re-scanned the reference line's
  changing elements from element zero for every code word. Since `a0` never
  moves backwards inside a row, the search can resume where the previous one
  stopped. Timed against the restarting search directly, interleaved, medians
  of five, on three 4096x4096 Group 4 pages: **1.03x** on a page with a few
  long runs per row, **4.8x** on the benches' bilevel fixture and **24.6x** on
  a halftone page with hundreds of runs per row — free where fax coding is at
  home, decisive where it is not. `tests/tiff_oracle_codecs.rs`'s byte-identity
  checks against `tiffcp -c g3` and `-c g4` are unchanged. `BitReader::peek` was also a byte-at-a-time loop and
  is now one shift-and-mask over a three-byte window.
- **`oxiarc-tiff`: `cargo nextest run --no-default-features` passes again.**
  Eight tests in `tests/corrupt_no_panic.rs` and `tests/proptest_roundtrip.rs`
  built their fixtures with `Compression::Lzw` / `CcittGroup4` / `Deflate`
  unconditionally and panicked on `FeatureNotCompiled` before testing anything.
  Each fixture is now gated on the feature that compiles its codec, with
  `PackBits` (which needs no feature) keeping the corpus non-empty. The default
  and `--all-features` corpora are unchanged.

## [0.4.1] - 2026-08-06

**Security hardening (ZIP CSPRNG, constant-time AES, x86 CRC-32, 7z
coder-chain), two long-standing encoder-ratio limitations closed (Zstandard
`FSE_Compressed_Mode` sequence tables, Brotli literal block splitting), plus a
hygiene/infrastructure pass** (`deny.toml` bans, `rustfmt.toml`/`clippy.toml`,
`ManuallyDrop` removal, docs-truth fixes). No archive/stream wire format
changed and no public API was removed for any read/decode path; the ZIP
write-side salt/header API changed from infallible to fallible (see
Security below), which is the one intentional breaking change in this
release. The two encoder changes alter the *bytes produced* by
`oxiarc-zstd` and by `oxiarc-brotli` at quality 10-11 — both remain
RFC-conformant and reference-decodable, and both are strictly smaller or
equal, never larger.

### Security

- **ZIP encryption salts and ZipCrypto headers now always come from a real
  OS CSPRNG.** `zip::encryption::generate_salt` and
  `zip::crypto::ZipCrypto::generate_header_random` previously used
  `/dev/urandom` on Unix with a `#[cfg(not(unix))]` arm that unconditionally
  returned `false`, silently falling back — on every non-Unix target, and on
  Unix if `/dev/urandom` could not be opened (fd exhaustion, chroot,
  seccomp) — to a low-entropy seed (wall-clock nanos, PID, a per-process
  counter, a stack address, a heap address, and a thread-id hash) expanded
  with SHA-1, despite the module documentation claiming salts were "sourced
  from the operating system CSPRNG." A predictable WinZip-AES salt is the
  sole input differentiating PBKDF2-SHA1 key derivation between archives; a
  collision under AES-CTR is direct keystream reuse. Fixed by a new internal
  `zip::csprng` module with a real, unconditional platform binding
  (`/dev/urandom` on Unix, `BCryptGenRandom` with
  `BCRYPT_USE_SYSTEM_PREFERRED_RNG` on Windows, pure-Rust `#[link]`
  declarations with no external crate) and **no software fallback**: if the
  OS source cannot be reached, generation now fails with a typed
  `OxiArcError::Io` instead of silently emitting weak material.
  `generate_salt`/`generate_header_random` are therefore now fallible
  (`Result<..>`), which is a breaking change to those two write-side
  signatures — callers must handle the new `Result`. New tests assert
  distinctness and non-constant-pattern output across many draws
  (`test_generate_salt_is_fallible_and_os_sourced`,
  `os_csprng_produces_distinct_high_entropy_output`); a new
  `tests/zip_encryption_e2e.rs` covers full AES-256/ZipCrypto write→read
  round-trips including wrong-password rejection.
- **Fixed silently-wrong x86_64 CRC-32 SIMD arithmetic; enabled dispatch.**
  `oxiarc_core::crc_simd::x86::crc32_pclmulqdq` mixed the non-reflected
  Intel whitepaper fold constants with reflected-mode folding and extracted
  the result from the wrong dword lane, so it returned incorrect CRC-32
  values — but shipped as a public `unsafe fn` with dispatch hardcoded off
  specifically because it was unverified, so the wrong arithmetic was
  reachable only by a downstream caller doing its own feature detection.
  The constants are now a direct translation of the already-validated
  aarch64 PMULL path (both architectures now share one
  `reflected_constants` module), and `SimdCrc32Dispatcher` now dispatches to
  it on x86_64 when PCLMULQDQ + SSE4.1 are detected. New tests
  (`dispatched_crc32_matches_software_reference`, a 0–4096-byte length
  sweep, and 100 randomized-input vectors including non-zero seed CRCs)
  cross-check the dispatched implementation against the scalar
  slicing-by-8 reference on every architecture.
- **Bounded 7z folder coder-chain amplification.** A crafted 7z bind-pair
  graph that revisits coders could chain decompression stages up to
  `folder.coders.len()` deep, each stage expanding the previous stage's
  output with no cumulative budget — a nested decompression bomb from a
  tiny input. `decode_folder_data`'s coder-chain walk (extracted into
  `decode_coder_chain`) now tracks per-folder visited-coder state (each
  coder decodes at most once) and enforces a cumulative output budget
  (`folder_decode_budget`: ratio-capped at 10^6× the packed size, floored at
  64 MiB, ceilinged at 16 GiB) checked both against declared per-stage sizes
  before decoding and against actual bytes produced after.

### Fixed

- **`BrotliCompressor` (`oxiarc-brotli`) is now genuinely streaming**
  instead of buffering the entire input in memory until `finish()`. Written
  data is buffered only up to a bounded threshold
  (`with_max_input`, default one meta-block); `Write::flush` now actually
  compresses and pushes every complete byte toward the inner writer instead
  of being a silent `Ok(())` no-op (the only such trivial `flush` body found
  anywhere in the workspace); a new `BitWriter::drain_complete_bytes`
  primitive drains whole bytes while leaving a sub-byte residue buffered
  until the next meta-block or `finish()` completes it.
- **`repair_zip`/`repair_tar` (`oxiarc-archive`):** fixed a `usize`
  overflow in the truncated/corrupt-archive scanner where
  `local_file_header.data_start + declared_compressed_size` — the latter an
  attacker-controlled `u32` — could wrap on a 32-bit target before the
  buffer-length clamp applied; both call sites now use `saturating_add`.
  Added a deterministic bit-flip/byte-mutation regression suite
  (`tests/iso_sevenz_mutation.rs`) targeting the two previously
  least-covered untrusted-input parsers (ISO 9660, 7z: 0 B and 8 KB fuzz
  corpora respectively, against 1.7–9.4 MB for every other target) — not a
  substitute for a real `cargo fuzz` campaign, but a cheap permanent check
  that both parsers return `Ok`/`Err` and never panic or allocate unbounded
  memory across thousands of reproducible mutations of the embedded
  fixtures. Also expanded inline test coverage in `repair_zip.rs`/
  `repair_tar.rs` for oversized/malformed declared sizes.
- **XZ stream-footer Backward Size validation** (`xz::header::read_footer`)
  cast `usize` to `u32` with a plain `as`, which wraps rather than
  saturates; an index exceeding ~16 GiB on a 64-bit target could therefore
  alias a forged footer value and defeat the consistency check. Now uses
  `u32::try_from` and rejects the stream when the real size cannot be
  represented in the field.
- **XZ stream-footer Backward Size construction** (`xz::header::write_stream_footer`)
  had the same `as u32` truncation on the write side: an implausibly large
  (but not impossible, on a 64-bit target) `index_size` would silently wrap
  and produce a footer whose Backward Size does not describe the Index this
  crate just wrote — an archive that corrupts itself in the act of being
  created, which this crate's own `read_footer` guard above would then
  reject. Now uses the same `u32::try_from` guard as the read side instead
  of a differently-shaped bug in the mirror-image code path.

### Changed

- **Removed four redundant `unsafe impl Send`/`Sync` blocks**
  (`LzmaPool`, `SnappyPool`, `BrotliPool`, `DeflatePool`): every backing
  field is `Mutex`/`Arc`/`AtomicUsize`-based and therefore already
  auto-`Send`+`Sync`. The manual impls added no capability but permanently
  suppressed the compiler's auto-trait derivation, so a future non-`Send`
  field addition would have silently compiled into an unsound type with no
  diagnostic. Replaced with a `const _: fn() = || { fn
  assert_send_sync<T: Send + Sync>() {} assert_send_sync::<T>(); };`
  compile-time check in each module — same guarantee, but it now fails to
  *compile* instead of failing to *warn* if it is ever violated.
- **Replaced `ManuallyDrop` + `ptr::read` + hand-enumerated `drop_in_place`
  with `Option<W>` + `.take()`** in `into_inner` for `ZipWriter`,
  `TarWriter`, `LzhWriter` (`oxiarc-archive`) and `oxiarc-core`'s
  `BitWriter`. All four implementations were already sound, but the
  drop-field list was a hand-maintained duplicate of the struct's field
  list with nothing tying the two together, so a future non-`Copy` field
  addition would have leaked silently. Every other field now drops through
  the ordinary safe path with no enumeration needed. As a side effect this
  also closes a latent double-finish hazard in `ZipWriter`/
  `LzhWriter::into_inner`: previously, calling the public `finish()` before
  the (then-)`ManuallyDrop` wrap meant a partial failure (e.g. the central
  directory writes out but the final flush fails) left `self.finished ==
  false` and `self` to drop normally, and `Drop` would re-invoke `finish()`
  a second time — silently re-writing however much output it got through
  before hitting an error again, since `Drop` discards it. `into_inner` now
  takes the writer unconditionally *before* propagating any error, so
  `Drop` never re-enters `finish()` on any path.
- **Six of eight non-test `.expect()` calls converted to `Result`
  propagation** (the other two are blocked by an unstable API, see below).
  The three `BinaryHeap::pop()` calls in `oxiarc-zstd`'s Huffman tree
  builder (`HuffmanEncoder::from_frequencies`) now use `?` on the `Option`
  the function already returns. The three RFC 8878 predefined-FSE-table
  constructors in `oxiarc-zstd::sequences` (`predefined_ll_table`/
  `predefined_of_table`/`predefined_ml_table`) now return
  `Result<FseTable>`, propagated with `?` from their (already-`Result`
  -returning) callers. All six operated on a provably-safe input (a loop
  invariant or a compile-time constant) with zero behavior change on the
  path that actually runs — this closes the "safe today, silently breaks on
  a future refactor" gap rather than fixing a live bug.
- **`deny.toml` gained `[[bans.deny]]` entries** for the full COOLJAPAN
  replacement table (`zip`, `flate2`, `miniz_oxide`, `zstd`(-safe/-sys),
  `bzip2`(-sys), `lz4`(-sys)/`lz4_flex`, `tar`, `snap`, `brotli`
  `-decompressor`, plus the wider-ecosystem `bincode`, `rustfft`,
  `quick-xml`, `openblas-src`). Previously `[bans]` had `multiple-versions`
  and `wildcards` but zero `deny` entries, so `cargo deny check bans`
  passed vacuously for the one workspace whose entire purpose is replacing
  those crates; verified live with a temporary positive-control entry
  (denying `thiserror`, a real dependency) that correctly failed the check
  before being removed.
- **Added `rustfmt.toml`** (`edition = "2024"`, `max_width = 100` — both
  already rustfmt's stable defaults, pinned explicitly so `cargo fmt`
  behavior cannot silently drift with a future rustfmt release) **and
  `clippy.toml`** (`msrv = "1.85"`, matching `[workspace.package]
  .rust-version`). Neither config triggers a repo-wide reformat or new lint
  hit on its own — a one-time `cargo fmt --all` was still needed for four
  files touched by the security/hardening fixes above
  (`sevenz/header.rs`, `zip/crypto.rs`, `zip/csprng.rs`, `crc_simd.rs`)
  that had never been run through `cargo fmt` after being hand-edited;
  all four diffs were whitespace/line-wrap only, no logic change.
  `cargo fmt --all --check` and `cargo clippy --workspace --all-targets`
  are both clean as of this entry.

### Added

- **LZH legacy methods: `-lh2-`, `-lh3-`, `-lzs-`, `-lz4-`, `-lz5-`, `-pm0-`**
  (`oxiarc-lzhuf/src/legacy/`, ~1,900 lines across six new modules). Until now
  everything outside the LHarc `-lh0-`..`-lh7-` line was reported as
  `LzhMethod::Unknown` and refused at extraction time. All six now decode
  **and** encode, and `CompressionMethod`/`LzhMethod` name them honestly in
  archive listings instead of "unknown".

  - `-lh2-` (`legacy/lh2.rs` + `legacy/dynhuff.rs`): 8 KiB LZSS over the
    LHarc 2.x *adaptive* Huffman of `dhuf.c` — a flat frequency-sorted node
    array with equal-frequency blocks (so a code bit is the parity of a node
    index), plus a match-position tree that starts as a single leaf and grafts
    on one 64-distance group every time the output passes another 64-byte
    boundary. Structurally unrelated to the Vitter-style tree `-lh1-` uses.
  - `-lh3-` (`legacy/lh3.rs` + `legacy/huffcode.rs`): the same window with
    `shuf.c` block-static tables — a 16-bit block size, 286 x (1 + 4)-bit
    literal/length lengths, an optional 128 x 4-bit position table, the
    three-consecutive-1-lengths degenerate escape for both, and LArc's built-in
    `ready_made` position table as the fallback when transmitting one does not
    pay. This needed a *different* Huffman length builder from the one
    `encode.rs` uses: `-lh3-`'s `make_table` rejects an incomplete code
    outright, whereas the existing clamp-and-reinflate limiter satisfies Kraft
    only as an inequality. `huffcode.rs` instead re-runs an exact (always
    complete) merge on progressively flattened frequencies until the deepest
    code fits the field.
  - `-lzs-`/`-lz5-` (`legacy/larc.rs`): LArc LZSS with no entropy coding.
    These address history by **absolute ring index**, not by distance back —
    the single most dangerous difference from every other method here, since
    confusing the two yields a codec that round-trips perfectly against itself
    and is unreadable by every real LHA implementation. `legacy/ring.rs`
    therefore exposes only absolute indexing, with one conversion point.
  - `-lz4-`/`-pm0-`: genuine stored formats (the reference decoders route both
    to a null decoder), now handled as such.

  **Verification.** `-lzs-`/`-lz5-`/`-lz4-`/`-pm0-` are gated by the real `lha`
  CLI (Lhasa 0.6.0) in a new `oxiarc-archive` `lha-oracle` suite: this crate
  builds a `.lzh` through the public `LzhWriter`, `lha t` CRC-tests it, `lha x`
  extracts it, and the extraction must equal the input byte for byte — 14
  payload/method/header-level combinations. Lhasa implements **no** `-lh2-` or
  `-lh3-` decoder (there is no `lh2_decoder.c`/`lh3_decoder.c` in its source
  tree at all) and neither does `delharc`, so those two have no oracle that a
  test can shell out to. Conformance was instead established against the
  canonical *LHa for UNIX* `dhuf.c`/`shuf.c` decode path compiled standalone:
  10 payloads x 2 methods = 20 streams decoded byte-identically, plus 10 more
  with the `ready_made` position table forced. Five payloads x 2 methods are
  frozen into `oxiarc-lzhuf/tests/lzh_legacy_vectors.rs`, asserted in both
  directions, so the in-repo gate stays hermetic and pure Rust.

  **Not implemented: `-pm1-`/`-pm2-`.** PMarc's compressed variants have no
  published format description; the only specification is Lhasa's
  `pm2_decoder.c`/`pm1_decoder.c`, which are GPL-2.0 and cannot be ported into
  this Apache-2.0/MIT crate. A clean-room derivation would need fixtures, and
  `lha` can decode `-pm2-` but cannot create it, so there is no way to generate
  them. Such entries stay listed with a typed `unsupported_method` error at
  extraction, never a silent mis-decode.
- **Constant-time AES for WinZip AES ZIP encryption**
  (`oxiarc-archive/src/zip/aes_ct.rs`, new module). `SubBytes`, the key
  schedule's `SubWord` and the GF(2^8) doubling inside `MixColumns` used to be
  a 256-byte S-box lookup and a branch on the high bit — the classic
  cache-timing / `prime+probe` target, since both the table index and the
  branch depend on key material. They are now a bitsliced Boyar-Peralta S-box
  circuit (113 `AND`/`XOR`/`NOT` gates evaluated over eight 16-bit bit planes,
  substituting all 16 state bytes at once) and a mask-based `xtime`. No table
  is indexed with secret data and no branch is taken on it anywhere in the
  cipher. The change is provably behaviour-preserving — the new circuit is
  checked against the FIPS 197 table for **all 256 inputs in all 16 lanes**,
  and the branchless `xtime` against the textbook branching definition for all
  256 inputs — so ciphertext, and therefore wire compatibility with
  WinZip/7-Zip/WinRAR, is unchanged. New known-answer tests add the NIST
  SP 800-38A F.1.1/F.1.3/F.1.5 `ECB-AES128/192/256.Encrypt` vectors (an
  unstructured key, unlike FIPS 197 Appendix C's `000102...`, so key-schedule
  bugs cannot hide) plus WinZip little-endian CTR counter and carry tests
  derived from the now-NIST-verified block cipher.
  Delegation of these primitives to the sibling `oxicrypto` crates was
  evaluated and is not possible today: `oxicrypto-cipher` exposes only
  AES-128/256 single-block ECB (no AES-192), and no `oxicrypto` crate ships
  SHA-1, HMAC-SHA1 or PBKDF2-HMAC-SHA1 — all four of which WinZip AE-1/AE-2
  requires. A partial delegation would have left AES-192 on the vulnerable
  table path, which is worse than a uniform in-repo fix.

- **Zstandard `FSE_Compressed_Mode` sequence tables** (resolves TODO Known
  Issue #2). `oxiarc-zstd/src/fse_encoder.rs` gained reference-faithful ports
  of `FSE_normalizeCount` (with the `FSE_normalizeM2` fallback) and
  `FSE_writeNCount`, and `compressed_block.rs` now chooses per symbol category
  whichever of RLE, the RFC 8878 predefined table, and a table built from the
  block's own distribution costs fewest bits *including* the table
  description. Custom tables also raise the offset-code ceiling from the
  predefined table's 0..=28 to the format's full 0..=31. Measured on a 1.5 MB
  structured-record corpus: 173,521 → 109,359 bytes at level 1 (-37%).
  `fse.rs` gained a `read_ncount` split out of `read_fse_table_description` so
  the writer can be validated against the exact parser that reads real `zstd`
  output. The `zstd-oracle` suite gained an independent frame walker that
  asserts a block really used mode 2 before requiring `zstd -d` to reproduce
  the input byte for byte — without that assertion the oracle check would pass
  vacuously on predefined-table frames.

- **Brotli literal block splitting** (`oxiarc-brotli/src/block_split.rs`, new
  module; partially resolves TODO Known Issue #3). The encoder previously
  hardcoded `NBLTYPESL = 1`, so it could never use the block-type switching
  and context maps the decoder has supported all along. It now segments the
  literal stream, clusters segment histograms by merge cost, collapses
  adjacent equal labels into runs, and emits up to 8 literal block types, each
  bound to its own prefix code through a move-to-front + zero-run-length-coded
  context map. The meta-block is encoded **both** ways and the smaller kept, so
  a poor split can cost encode time but never compression ratio. Active at
  quality 10-11 only; quality 0-9 output is byte-identical to the previously
  reference-verified path, asserted by a test. Measured on a 180 KB
  two-population input: 145,638 → 125,264 bytes (-14%). The `brotli-oracle`
  suite gained an independent stream-header walker that asserts
  `NBLTYPESL > 1` before requiring `brotli -d` to reproduce the input.
- **Brotli insert-and-copy + distance block splitting and per-context
  histogram assignment** — the remainder of the block-splitting work. The
  encoder now splits all three symbol categories, not just literals, and
  exploits the `LSB6` context mode it already declared instead of binding one
  prefix code per block type. `block_split.rs` grew a category-generic
  clustering core (`cluster_histograms` / `split_symbols`) plus two plan
  builders (`plan_literals`, `plan_distances`) that also cluster
  per-`(block type, context)` histograms into prefix codes and emit the
  binding context map, so `NTREESL`/`NTREESD` may now exceed their block-type
  counts. `compress.rs` gained `SymbolStreams` (the three streams flattened in
  decoder order with their Section 7.1/7.2 context IDs) and `BlockSwitcher`
  (a mirror of the decoder's `BlockCategory::tick`, shared by all three
  categories).

  The three categories are searched by **coordinate ascent**: each category's
  candidates are measured against the current best plan for the other two, and
  a change is kept only when the fully-written meta-block gets smaller. A
  single joint on/off switch was tried first and was strictly worse — it
  coupled the categories, so data whose literal statistics are uniform but
  whose command statistics change halfway had to buy literal splitting (pure
  cost) to get insert-and-copy splitting, and the measured comparison then
  correctly rejected the whole bundle, leaving both features unused on exactly
  the inputs they were built for. Relatedly, every cluster-opening threshold is
  now expressed **per symbol** rather than as an absolute bit count: merge cost
  scales linearly with segment length, so absolute bars silently meant
  different things per category and would silently disable a feature if a
  segment length were ever tuned.

  Measured: a two-regime 162 KB corpus 62,719 → 60,659 bytes; 240 KB of
  context-dependent text 146,769 → 142,703. Still quality 10-11 only, and
  quality 0-9 is now asserted byte-frozen for *every* category, not just
  literals.

  Verification needed a new tool. The existing oracle's independent header
  walker cannot reach `NBLTYPESI`/`NBLTYPESD` — skipping past `NBLTYPESL`
  requires decoding Huffman-coded block counts, i.e. reimplementing the
  decoder inside the test. Instead `oxiarc-brotli` gained
  `decompress_reporting_shapes`, which reports the `MetaBlockShape` each
  meta-block header actually declared as a side effect of a normal decode
  through the reference-validated decoder (608/608 reference streams). Five new
  `brotli-oracle` tests use it to assert each feature genuinely reached the
  wire — a round-trip alone is also true of a stream that silently declined to
  split, so without this the tests would pass vacuously — and then require
  reference `brotli -d` to reproduce the input byte for byte.
- **TAR PAX 1.0 sparse format support** (`GNU.sparse.major=1`/
  `GNU.sparse.minor=0`) in both `TarReader` (seekable) and
  `TarStreamReader` (streaming) — the one sparse variant this crate
  previously did not read (old-format `'S'` and PAX 0.1 were already fully
  supported in both readers). Unlike PAX 0.1, where the offset/numbytes map
  lives in `GNU.sparse.map` pax-attribute text, PAX 1.0's map is a
  newline-terminated decimal-ASCII preamble at the very start of the data
  entry's own payload (`SparseMap::parse_pax_1_0_preamble`,
  `oxiarc-archive/src/tar/sparse.rs`); it is consumed directly off the
  stream rather than seeked over, so both the seekable and streaming
  readers support it with the same code path (no `Seek` requirement added).
  `GNU.sparse.realsize` and the `GNU.sparse.name` shadow-name convention
  are shared with 0.1. A non-1.0 archive is unaffected: detection requires
  both `GNU.sparse.major == "1"` and `GNU.sparse.minor == "0"` to be
  present; previously such an archive failed cleanly with "missing
  GNU.sparse.map" (additive change, no prior behavior removed). 10 new
  tests: 8 unit tests (round-trip, zero-entry, exact-padding-consumption,
  and five malformed-preamble cases — missing realsize, a non-digit byte,
  a truncated stream, an excessive declared entry count, and digit
  overflow) plus one end-to-end test per reader, cross-checked against
  each other. Writer-side sparse emission remains out of scope for all
  three variants, unchanged from before.
- `oxiarc-szip` gained a `TODO.md` (previously the only member crate
  without one), documenting its real entropy-coding-encoder gap (`encode()`
  currently emits only no-compression blocks — spec-valid and
  libaec-round-trippable, but not actual compression), the unimplemented
  CCSDS restricted option set, and unimplemented `AEC_DATA_SIGNED` support.
- `oxiarc-lzhuf` and `oxiarc-lzw` gained runnable `examples/`
  (`lzh_roundtrip.rs` round-trips every implemented LZH method;
  `lzw_roundtrip.rs` round-trips both the TIFF and GIF bitstream
  configurations) — both previously had none, unlike the other ten member
  crates.

### Docs

- Corrected two stale per-crate `TODO.md` "Fuzzing tests" checkboxes
  (`oxiarc-snappy`, `oxiarc-deflate`) that were still shown unchecked
  despite the corresponding `fuzz/fuzz_targets/` harnesses and
  multi-megabyte corpora already existing.
- Marked the root `TODO.md`'s zstd-release and OxiGDAL-downstream checklist
  items as stale/out of this repo's scope: the branch has shipped multiple
  releases (0.3.6 → 0.4.0 → 0.4.1) since that checklist was written, and
  the OxiGDAL item was always a separate project's backlog.
- Documented, in `CONTRIBUTING.md` and root `TODO.md`, that `cargo bench` /
  any `--all-targets` build requires a C compiler on `PATH` because of
  `criterion` 0.8+'s mandatory (non-optional, non-feature-gated) `alloca`
  dependency. This is dev-only and does not affect the shipped libraries:
  every member crate's default features remain 100% Pure Rust, verified by
  walking the full `Cargo.lock` for any other C/C++/Fortran build
  dependency (none found). `CONTRIBUTING.md` previously stated flatly that
  "no external C/Fortran toolchain is required," which was true for
  `cargo build`/`cargo test` but not for the benchmark surface.
- `oxiarc-cli` intentionally still has no `examples/` directory: it is a
  `[[bin]]`-only crate with no library target, so there is no public API
  for an example to demonstrate; its usage is documented via `README.md`,
  `man/`, and `completions/` instead.

## [0.4.0] - 2026-07-30

**DEFLATE/zlib decoder performance rewrite.** No archive/stream wire format
changed and no public API was removed — every addition below is opt-in or
internal; existing callers of `inflate`, `Inflater::new`, `zlib_decompress`,
etc. see only a speed-up.

### Added
- `oxiarc_deflate::inflate_into(src, dst) -> Result<usize>` and
  `zlib::zlib_decompress_into` — decompress DEFLATE/zlib payloads directly
  into a caller-supplied buffer with no intermediate `Vec` and no
  output-size guessing; a stream that would overflow `dst` is rejected with
  `BufferTooSmall` rather than truncated.
- `oxiarc_core::BitReader::buffered` / `with_buffer_capacity` — a
  buffered/prefetch reader mode that refills the bit accumulator with bulk
  64-bit little-endian loads instead of one `Read::read` per few bits.
  `BitReader::new` (exact mode) is unchanged and still required wherever the
  reader must not advance past the bits actually consumed (e.g. ZIP's
  byte-aligned data descriptor immediately following a DEFLATE member).
- `oxiarc_core::BitCache`, plus `BitReader::detach` / `reattach` /
  `refill_cache` — a register-resident bit accumulator a decoder's inner
  loop can detach, decode many symbols against, and reattach, removing the
  store/load-forwarding stall a memory-resident accumulator costs on every
  symbol.
- `BitReader::into_parts` / `buffered_len` — recover prefetched-but-unconsumed
  bytes so a buffered `BitReader` can hand a shared stream back to other code
  without losing data.
- `Inflater::with_output_capacity(size_hint)` and `MAX_OUTPUT_CAPACITY_HINT`
  — pre-size the decoder's output buffer from an untrusted size hint
  (clamped to 64 MiB); GZIP decoding now seeds this automatically from the
  trailing ISIZE field.
- Fuzz target `fuzz_inflate_into`, cross-checking the growable-`Vec` and
  slice-sink decode paths byte-for-byte against each other.
- `oxiarc-deflate/tests/inflate_differential.rs` — a differential suite
  proving the buffered fast path, the exact-mode path, and
  `inflate_into`/`zlib_decompress_into` all agree across stored/fixed/dynamic
  blocks, maximum-distance (32 KiB) back-references, and
  hostile/truncated/corrupted input; adds an optional CPython `zlib` oracle
  comparison behind the pre-existing `zlib-oracle` feature.

### Changed
- **DEFLATE/zlib decoding rewritten for throughput.** `HuffmanTree` now
  decodes through a two-level root+sub-table layout (root widened from a
  single-level 9-bit table to a 10-bit root table), in the style of zlib's
  `inflate_table`/libdeflate. The LZ77 history is now the output buffer
  itself (`InflateWindow`, backed by `Vec::extend_from_within`) rather than
  a separate ring buffer that required writing every decoded byte twice.
  Combined with the buffered `BitReader`/`BitCache` above, this is a
  substantial decode speed-up with unchanged output.
- `Adler32::update` now folds 32-byte groups through a closed-form reduction
  (`b' = b + 32*a + Σ(32-i)·xᵢ`) instead of one add-pair per byte, letting
  the compiler auto-vectorize it — matching zlib's `DO16` unrolling. Output
  is bit-identical to the previous byte-at-a-time version.
- `zlib_decompress` internals split into `zlib_payload` (header validation)
  and `verify_zlib_trailer` (Adler-32 check), now shared with
  `zlib_decompress_into`.
- The Huffman fast-decode path's last `unsafe`/`get_unchecked` table access
  is now safe, bounds-checked code.

## [0.3.6] - 2026-07-13

This release bundles two hardening passes from the same 0.3.6 development cycle.

**2026-07-08 — security-hardening and API-stabilization pass:** a broad pass
across every codec/container crate closing memory-safety, path-traversal, and
panic issues found by internal audit, plus a documentation/CLI-ergonomics pass
and a large expansion of test/example/fuzz coverage. No archive/stream wire
format changed; all fixes are either defensive (reject malformed/hostile input
instead of panicking or over-allocating) or additive (new opt-in flags, APIs,
and docs).

**2026-07-13 — reference-interoperability and hostile-input hardening
campaign:** root problem — the test suites only exercised oxiarc→oxiarc
round-trips, which masked total interoperability failure — several codecs
were self-consistent **private dialects** that passed their own tests while
failing ~100% against the reference implementation in both directions (zstd,
brotli, LZMA2/.xz multi-chunk, TIFF-LZW, SZIP), and several decoders returned
`Ok` with silently wrong or truncated data. A 22-auditor differential+audit
investigation produced a 75-item remediation plan (P0–P3); all 75 items were
implemented. Every codec is now validated by reference-tool differential
testing, in both directions, with permanent regression gates. Two codec wire
formats necessarily changed (see Changed).

### Fixed — codec interoperability (private dialects eliminated)

- **oxiarc-zstd** (flagship; resolves the OxiGDAL-reported FSE decoder
  panic): the FSE backward bitstream was read FIFO/LSB-first instead of
  RFC 8878 LIFO/MSB-first — and the writer mirrored the same wrong layout,
  which is why self-round-trips passed while **0/64** real zstd frames
  decoded (62 panics, 2 silent corruptions). Also fixed: the 4-stream
  Huffman jump table was read as offsets instead of sizes; Huffman weight
  tables lacked validation and the RFC 4.2.1.1 implied-last-weight
  deduction; FSE tables lacked probability-sum validation and bounds-checked
  state indexing (the reported `fse.rs` index-OOB panic site); a truncated
  1-byte FCS corrupted `set_content_size(false)` frames ≥ 256 bytes;
  raw-content dictionary frames carried a fabricated Dictionary_ID that
  reference zstd could never match. Now: **64/64** corpus + **101/101** wide
  reference frames decode byte-identical; **85/85** oxiarc frames accepted
  by `zstd -d`; **9/9** dictionary frames; 60,000 fuzz cases, 0 panics. The
  Huffman literals encoder is wired in (used when it beats Raw/RLE);
  sequence sections remain predefined/RLE FSE — RFC-valid, a ratio
  limitation only, documented honestly.
- **oxiarc-brotli**: full RFC 7932 rewrite of decoder AND encoder —
  window-bits tree (§9.2), the 704-symbol insert-and-copy table (§5) with
  implicit distance-code-0, block-type switching (§9.3), the RFC
  code-length VLC with Kraft-complete stop, context maps + the exact §7.1
  context LUTs, distance short-code ring semantics, metadata meta-blocks,
  two-level `O(1)` Huffman decode tables, strict trailing-garbage/padding
  rejection, and the byte-exact **122,784-byte Appendix A static
  dictionary** (previously an empty stub) with all 121 transforms
  (UTF-8-aware ferment casing). Baseline: 141/172 reference-stream decode
  failures plus 21 silently-wrong results, and `brotli -d` rejected
  **441/441** oxiarc outputs. Now: **608/608** reference streams decode
  byte-identical (zero silent mismatches); **588/588** oxiarc streams
  accepted by `brotli -d`, with real compressed meta-blocks. The encoder's
  ratio trails the reference at q10–11 and on structured binary (no
  block-splitting/context modeling on the encode side) — ratio only, not
  correctness.
- **oxiarc-lzma / XZ**: the LZMA2 decoder reset the uncompressed position
  per chunk, desyncing `pos_state`/`lit_state`, so standard multi-chunk
  `.xz` (i.e. most real files over one chunk) was undecodable — oxiarc's
  own encoder reset every chunk, masking it. The position is now a
  persistent decoder field reset only on dictionary reset. A stateful
  chunked encoder (cross-chunk matching, reset-1/2 continuation chunks) was
  added, and `LzmaProperties` now validates lc/lp/pb (`new(20, 20, 4)`
  previously aborted the process via a multi-TiB allocation). Verified vs
  `xz 5.8.3`: **60/60** `.xz` decode + **8/8** encode byte-identical,
  multi-block OK.
- **oxiarc-bzip2**: multi-stream/concatenated `.bz2` (pbzip2, lbzip2,
  `cat a.bz2 b.bz2`) was silently truncated to the first stream with an
  `Ok` return — now all streams decode and trailing garbage is an error.
  Legacy randomised blocks (bzip2 ≤ 0.9.0) are de-randomised (new
  `src/rand.rs`). The encoder now implements libbz2's multi-Huffman-table
  `sendMTFValues` clustering (previously 1 effective table; output up to
  +50% larger than reference) reaching **99.9%** of the reference ratio.
  New `decompress_with_limit(reader, max_out)` bounded API. Verified vs
  `bzip2 1.0.8`: **324/324** both directions.
- **oxiarc-szip**: the AEC/CCSDS-121 framing placed the RSI reference
  sample outside the block option-ID field, breaking libaec interop in both
  directions including silent wrong output on genuine libaec streams.
  Rewritten per CCSDS-121.0-B-2 §5.2 (option ID precedes all
  `pixels_per_block` samples; zero-block/ROS/second-extension options;
  typed validation errors replace silent truncation). Verified against
  **live libaec 1.1.4: 2450/2450 decode + 4900/4900 encode
  byte-identical.**
- **oxiarc-lzw**: TIFF LZW had no Clear Code support — TIFF 6.0 mandates
  one as the first code of every strip and at table entry 4094 — making it
  100% incompatible with real TIFF (libtiff/Pillow/GDAL) in both
  directions. Fixed to libtiff `tif_lzw.c` semantics. Verified:
  **125/125** both directions vs Pillow/libtiff; oxiarc's encoded output is
  **byte-identical to libtiff's**.
- **oxiarc-deflate**: the core codec was already bit-exact vs CPython
  zlib/gzip and the gzip CLI — but the streaming/trait wrappers were not:
  `Inflater::decompress`/`decompress_all` silently truncated output past
  32 KiB and reported `Done`; `Deflater` broke DEFLATE bit-continuity
  across `deflate(_, false)` calls (its own inflate rejected the output)
  and discarded compressed bytes that overflowed the caller's buffer;
  `GzipDecoder` could not decode concatenated multi-member gzip (including
  oxiarc's own parallel-gzip output). All wrappers now stream correctly and
  are reference-verified.
- **oxiarc-lz4**: encoders violated the LASTLITERALS(5) end-of-block
  invariant (reference lz4 rejected the frames for common repetitive
  inputs), and the frame decoder ignored the block-independence flag
  (`lz4 -BD` linked frames failed at block 2; oxiarc could not emit them
  either — new `FrameDescriptor::with_block_independence` + rolling
  dictionary). Verified vs `lz4 1.10.0`: **11/11** encode + **44/44**
  decode + **3/3** linked-block frames.
- **oxiarc-lzhuf**: lh4–lh7 decoders returned `Ok` with silently truncated
  output on truncated streams (zero-padded reads past EOF read as
  end-of-block); all decode paths now detect exhaustion — **4,442/4,442**
  truncation trials return `Err` (previously 4,440 returned a silent
  short `Ok`). The streaming decoder was hardened the same way, and
  `LzhStreamReader` now honors the 64-bit uncompressed-size extension
  header (0x42).

### Security — 2026-07-08 pass

- **oxiarc-cli** (Zip-Slip / path traversal): hardened `sanitize_relative_path`
  to treat both `/` and `\` as separators and strip `.`/`..`/root/drive-prefix
  components (previously a bare `..` component could pass through). All
  archive readers (ZIP, 7z, CAB, LZH, ISO 9660) now route extracted names
  through this sanitizer, and `resolve_output_path` independently rejects any
  join that would resolve outside the output root.
- **oxiarc-cli** (symlink handling): extraction now creates real symlinks for
  archive entries that declare one (currently only TAR reads populate
  symlink metadata) instead of silently following/overwriting through them;
  Windows falls back to a warning when `SeCreateSymbolicLinkPrivilege` is
  unavailable.
- **oxiarc-cli** (Windows paths): fixed long-path (`\\?\`) and reserved
  device-name (`CON`, `NUL`, `AUX`, ...) sanitization, including a
  previously-unhandled trailing `.`/space on the final path component.
- **oxiarc-archive** (ZIP reader): fixed an integer underflow in AES
  compressed-size accounting that could panic on a crafted header; central
  directory and per-entry reads now validate declared lengths against actual
  remaining stream bytes and use `try_reserve`/`try_reserve_exact` instead of
  unconditional `Vec::with_capacity`/`vec![0; n]`, turning malicious
  oversized-length headers into a clean error instead of an allocator
  abort/OOM.
- **oxiarc-archive** (ZIP reader): fixed a second, unrelated integer-underflow
  panic in `LocalFileHeader::modified_time` — a crafted local-file header
  encoding a zero DOS month field underflowed the `month - 1` term of the
  epoch-offset calculation (panicking in debug, wrapping to a huge day count
  in release); DOS date fields are 1-based, so a zero month or day is now
  clamped to the minimum valid value (1) instead.
- **oxiarc-archive** (ZIP reader): classic and Zip64 end-of-central-directory
  records that declare more than one disk are now rejected — spanned/
  multi-volume ZIP archives are explicitly unsupported rather than silently
  misread.
- **oxiarc-archive** (TAR): fixed a slice-index panic in the PAX extended-
  header parser on malformed short records (e.g. `b"1 X=Y\n"`); bounded the
  untrusted declared sizes read for PAX/GNU long-name payloads and whole-
  entry extraction against actual remaining stream/extension-data length.
- **oxiarc-archive** (LZH, 7z, ZIP stream reader): bounded three more
  header-driven allocation sites (`zip::stream`, `lzh::reader`,
  `sevenz::header`) the same way — declared length checked against bytes
  actually available, then `try_reserve`/`try_reserve_exact`.
- **oxiarc-archive** (ISO 9660): `walk_directory` now tracks visited
  directory LBAs (rejecting a directory record that points back at itself or
  an ancestor), enforces a maximum recursion depth, and caps + bounds-checks
  the declared directory-extent size before allocating — closing a crafted-
  image cyclic-directory and unbounded-allocation DoS.
- **oxiarc-zstd**: capped the untrusted 8-byte `Frame_Content_Size` frame-
  header field against the window size before reserving output capacity
  (`Vec::try_reserve` instead of `Vec::reserve`), fixing a capacity-overflow
  panic/OOM risk on a crafted frame header (e.g. content size = `u64::MAX`).
- **oxiarc-lzma**: capped the LZMA/LZMA2 dictionary-size allocation at 1.5 GiB
  and switched to lazy, incrementally-grown dictionary buffers instead of
  eagerly zero-filling a header-declared size; the XZ container reader now
  rejects any LZMA2 filter whose declared dictionary size exceeds the cap
  before constructing a decoder, and validates the XZ index CRC-32 (never
  checked before) plus the footer Backward-Size field against the parsed
  index.
- **oxiarc-archive/zip** (encryption correctness): AES-128/192 encryption was
  previously silently downgraded to an AES-256 code path with a zero-padded
  key (producing output incompatible with WinZip/7-Zip/WinRAR and weaker
  than advertised); replaced with a genuine FIPS-197 AES cipher whose key
  schedule and round count are derived from the actual key length. The
  AE-2 (HMAC-SHA1) authentication-tag comparison now runs in constant time, and
  salts are drawn from the OS CSPRNG (`/dev/urandom`, with a strong
  entropy-mixing fallback) instead of a weaker PRNG; ZipCrypto's header
  randomization was aligned to the same CSPRNG source.
- **oxiarc-core** (CRC SIMD): fixed `is_simd_available()`/`implementation_name()`
  to report the actually-dispatched CRC-32 code path (previously x86_64
  could misreport PCLMULQDQ while the runtime dispatch had silently fallen
  back to software). Verified the aarch64 PMULL constants against the
  scalar reference over thousands of inputs/lengths — the implementation was
  already correct; only the diagnostics were wrong.
- **oxiarc-lzhuf** (`-lh1-` decode): a malformed or truncated `-lh1-`
  compressed stream paired with a large declared uncompressed size could
  loop indefinitely, manufacturing zero-padding output until memory was
  exhausted (decompression-bomb DoS); the bit reader now flags end-of-input
  exhaustion and `decode_lh1` returns an error as soon as decoding would
  read past the real compressed data instead of continuing to fabricate
  output.
- **oxiarc-lzhuf / oxiarc-core**: fixed `LzssDecoder::new`/`RingBuffer::new`
  panicking on a non-power-of-two or zero window size; added non-panicking
  `RingBuffer::try_new`/`OutputRingBuffer::try_new` alternatives and
  documented the `# Panics` contract of the existing infallible
  constructors.
- **oxiarc-lzma**: replaced two `.lock().expect(...)` calls on a
  `LzmaPool` mutex with poison-recovering `unwrap_or_else` — a panic in one
  worker thread while holding the pool lock no longer poisons the pool for
  every other thread.

### Security — 2026-07-13 pass

- **oxiarc-cli**: `--memory-limit` — the advertised decompression-bomb
  defense — was silently unenforced for file-based gzip/xz/bzip2
  extraction: a 50 MB bomb expanded fully under `--memory-limit 1M` and
  exited 0. It is now enforced **during** decode for every format (gzip
  ISIZE, xz stream-index declared size, lz4/zstd frame content sizes,
  bzip2/brotli/snappy bounded `decompress_with_limit` decoders). Measured:
  a brotli bomb under `--memory-limit 1M` peaks at **3.3 MB RSS vs
  72.9 MB** unbounded and exits non-zero.
- **oxiarc-lz4**: decompression-bomb fix — `decompress_block` only checked
  `max_output` between sequences, never inside one, so a single crafted
  sequence overshot the cap (measured: 64 B input → 1,020,020 B output,
  15,937x). The projected size is now checked before every literal/match
  copy, matching liblz4's destination-capacity discipline.
- **oxiarc-snappy**: the 64 KiB per-chunk uncompressed cap was not
  enforced in the frame decoder (one crafted chunk decoded to ~200 MiB,
  21x amplification); now rejected up front, with bounded total-output
  APIs added.
- **oxiarc-archive (XZ)**: a 28-byte crafted `.xz` triggered an unbounded
  allocation abort (SIGABRT) from the block-header compressed-size field;
  an out-of-bounds index panic in LZMA2 filter-props parsing; and the
  block-header CRC32 — written but never checked on read — is now
  validated before any parsed field is used.
- **oxiarc-archive (7z)**: complex-coder stream counts were read as
  unbounded varints feeding `Vec::with_capacity` (capacity-overflow panic
  or 16 TiB reservation from one crafted archive), and the `kCrc`
  aggregate materialized `vec![true; count]` (a 175-byte archive reserved
  256 MB). Both bounded.
- **oxiarc-archive (CAB/LZH/ISO)**: CFFILE filename reads capped;
  `LzhStreamReader` no longer eagerly allocates the untrusted u32
  `compressed_size` (`try_reserve` + remaining-bytes check); the ISO 9660
  directory-record parser no longer panics on a non-zero `LEN_DR` < 34
  (index-OOB reachable from any untrusted `.iso`).
- **oxiarc-deflate**: `ZlibStreamDecoder`'s concatenation fallback re-ran
  full, bomb-amplifiable decompression per candidate split offset (a
  ~22 KB adversarial stream hung > 60 s); now O(n) via exact
  consumed-length tracking, plus a `with_max_output` cap.

### Fixed — containers and CLI

- **oxiarc-archive (ZIP)**: externally-encrypted archives (`zip -e`,
  7-Zip, WinRAR, Python) were reported as **unencrypted** and silently
  mis-extracted — detection used a homegrown 0xEE,0xEE extra-field marker
  only oxiarc's own writer emits, instead of general-purpose bit 0; the GP
  flags are now persisted per entry and drive detection. Also fixed: a
  DOS-date month=0 underflow panic in `read_central_dir_entry` (a missed
  duplicate of the 0.3.6 `modified_time` fix, now factored into one shared
  helper); AE-2 entries now write CRC=0 per the WinZip AES spec (was the
  plaintext CRC — a spec violation and a plaintext-checksum leak); the
  Info-ZIP data-descriptor password-check byte (high byte of the DOS mtime
  when GP bit 3 is set) is accepted, so `zip -e` archives decrypt with the
  correct password; and written DOS timestamps use a proper civil-date
  algorithm (the naive 365/30-day math drifted ~14 days and could emit
  month=13).
- **oxiarc-archive (CAB)**: the MSZIP LZ77 window is now carried across
  CFDATA blocks within a folder (spec-valid multi-block cabinets from
  cabarc/makecab/libmspack previously failed with InvalidDistance); CFDATA
  per-block checksums — parsed but never validated — are now verified;
  extraction caches the decoded folder instead of re-decompressing it for
  every file (was O(files × folder size)); unknown compression-method
  codes now return `unsupported_method` instead of being silently treated
  as stored.
- **oxiarc-archive (TAR)**: `TarStreamReader` silently mis-decoded GNU
  old-format sparse ('S') entries — wrong size and content with no error
  (a realsize=16384 entry returned 600 raw bytes with `Ok`). The streaming
  reader now fully supports GNU old-format and PAX 0.1 sparse maps
  (bsdtar-verified byte-identical). PAX 1.0 sparse remains unsupported
  (documented).
- **oxiarc-cli**: raw Brotli has no magic bytes, so `.br` files were
  unusable via any file-path command including oxiarc's own output (only
  the stdin `--format br` path worked); an extension-based fallback fixes
  `detect`/`list`/`extract`/`test`/`convert` for `.br`. Extracting a
  single-file format to a non-existent output directory now creates it
  (previously a raw OS error, inconsistent with ZIP/TAR).
- Fixed a pre-existing flaky test race (async_lzh/async_tar suites shared
  a deletable temp path across parallel tests); each test now uses a
  unique path.

### Fixed — CLI and encoder correctness (2026-07-08 pass)

- **oxiarc-cli**: the `--progress`/`-P` flag was inert — it defaulted to
  `true` regardless of whether it was passed. It is now a plain opt-in
  boolean flag, off by default.
- **oxiarc-cli**: `oxiarc create`/`add` directory traversal could infinite-
  loop or dereference-and-copy through a circular or malicious symlink;
  traversal is now symlink-aware (`symlink_metadata`, never follows links)
  with a visited-canonical-path set guarding directory cycles.
- **oxiarc-cli**: `oxiarc convert` no longer silently clobbers an existing
  output file.
- **oxiarc-cli**: filesystem errors during `create`/`add`/`extract` now name
  the offending path instead of a bare I/O error.
- **oxiarc-cli**: replaced an `unreachable!()` at the end of `cmd_extract`
  with a proper `Err(...)` return, per the no-panic policy.
- **oxiarc-lzma**: fixed an LZMA2 multi-chunk encoder/decoder desync that
  corrupted varied (non-repeated-byte) data spanning more than one chunk.
  `Lzma2ChunkedEncoder` only reset the decoder's dictionary on the first
  chunk even though every chunk is compressed with a fresh, history-less
  `LzmaEncoder`; the decoder then seeded its literal-coder context from the
  previous chunk's dictionary tail while the encoder assumed empty history,
  desyncing the range coder on the first colliding literal. Every chunk (and
  sub-chunk) now resets the dictionary to match, fixing the default
  `encode_lzma2`/`decode_lzma2` path for inputs above the 2 MiB chunk-size
  threshold.
- **oxiarc-zstd**: one-shot compression always framed its output as
  `Single_Segment`, so the frame's implicit window size equaled the full
  content size; reference decoders enforce a default `windowLogMax` (128 MiB
  for libzstd's simple decompression API) and could reject an
  oxiarc-produced frame larger than that. Content above the internal 8 MiB
  window cap now instead gets an explicit, bounded 8 MiB `Window_Descriptor`
  — always sufficient since every block is compressed independently and
  match offsets never cross a block boundary — so the implicit window never
  approaches the reference-decoder limit regardless of input size.

### Changed — 2026-07-08 pass

- **Breaking (pre-1.0) API freeze**: `oxiarc-core::FlushMode`,
  `oxiarc-core::CompressStatus`/`DecompressStatus`, `oxiarc-zstd::BlockType`/
  `LiteralsBlockType`, `oxiarc-lz4::Lz4Level`, and the `OxiArcError`/
  `BrotliError`/`SnappyError`/`LzwError`/`SzipError` error enums are now
  `#[non_exhaustive]` for SemVer stability ahead of 1.0. Downstream code
  matching on these must include a wildcard arm (existing in-workspace call
  sites already did).
- **oxiarc-core**: removed the unused `CompressionLevel(u8)` newtype (dead
  code — every codec crate already has its own, differently-ranged
  `CompressionLevel` type); `Compressor`/`Decompressor` trait docs now
  correctly describe them as optional, DEFLATE-family-only traits rather
  than "implemented by all algorithms".
- **oxiarc-zstd**: advanced internal re-exports (`lz77::{LevelConfig,
  Lz77Sequence, MatchFinder}`, `bitwriter::{ForwardBitWriter,
  BackwardBitWriter}`) are demoted to `#[doc(hidden)]` — no longer part of
  the crate's documented/SemVer-covered public API surface.
- **oxiarc-lzma**: the `match_finder` and `model` modules are demoted from
  `pub mod` to `pub(crate) mod`, and their crate-root re-exports —
  `match_finder::{Bt4MatchFinder, HashChainMatchFinder, MatchFinder}` and
  `model::{LzmaModel, State}` — are removed entirely. Breaking (pre-1.0)
  change: `Bt4MatchFinder`, `HashChainMatchFinder`, `MatchFinder`,
  `LzmaModel`, and `State` are no longer publicly reachable from
  `oxiarc_lzma` (`LzmaProperties`, previously re-exported alongside
  `LzmaModel`/`State`, remains public via its own
  `pub use model::LzmaProperties;`).
- **oxiarc-cli**: added a global `--quiet`/`-q` flag; `list`/`test`/`info`/
  `detect`'s `archive` argument now accepts `-` for stdin.
- Numerous `#[must_use]` additions on by-value builder setters across
  `oxiarc-core`, `oxiarc-deflate`, `oxiarc-lz4`, and `oxiarc-lzhuf`
  (setters that consume and return `self` now warn if the return value is
  discarded).

### Changed — 2026-07-13 pass

- **Wire-format corrections (breaking for oxiarc-only streams)**:
  oxiarc-szip and the crate-private LZW stream framing moved from private
  dialects to the standard formats. Streams produced by earlier oxiarc
  versions of these two codecs are not readable by the fixed code (and
  vice versa) — intended, since the old bytes interoperated with nothing
  else. The brotli/zstd/lzma/bzip2/lz4 fixes change emitted bytes too, but
  all outputs old and new remain decodable — the new ones now also by the
  reference tools.
- **API stability (pre-1.0)**: 18 public enums marked `#[non_exhaustive]`
  (`CompressionMethod`, `EntryType`, `ArchiveFormat`, `RecoveryStatus`,
  `LenientWarningKind`, and the remaining format/method/status/error
  enums); 106 `#[must_use]` attributes added on consuming builder setters;
  internal types no longer leaked through public signatures.
- **Fallible constructors**: `LzwConfig::new` now returns `Result`
  (invalid bit widths previously panicked); `LzmaProperties` validates
  lc/lp/pb. New error variants: `SampleOutOfRange`/`InputTooShort`/
  `InvalidBlockOption` (szip), `ChunkTooLarge`/`TotalOutputExceeded`
  (snappy).
- **oxiarc-core**: `BitReader::fill_buffer` now loops on short reads
  instead of failing with `UnexpectedEof` on valid streams read from
  pipes/sockets; ring-buffer back-reference copies are length-bounded.

### Added — 2026-07-08 pass

- **SECURITY.md** and **CONTRIBUTING.md** at the workspace root.
- **oxiarc-cli**: `man/` troff man pages for every subcommand, and shell
  completions regenerated for bash/zsh/fish/PowerShell to match the new
  flags.
- Expanded regression-test coverage for every fix above (path sanitization,
  bounded-allocation, cyclic-ISO-directory, AES known-answer/CSPRNG,
  zstd/LZMA crafted-header, TAR PAX panic, symlink extraction, and more).
- `proptest`-based round-trip tests for `oxiarc-deflate`, `oxiarc-lz4`,
  `oxiarc-lzma`, `oxiarc-lzhuf`, `oxiarc-lzw`, `oxiarc-brotli`,
  `oxiarc-bzip2`, `oxiarc-snappy`, and `oxiarc-zstd`.
- `fuzz/` targets and new integration-test fixtures/corpora (CAB, ISO 9660
  interop) under `oxiarc-archive/tests/`.
- `examples/` directories added across most crates; new CLI integration
  tests (`cli_flags`, `cli_convert`, `cli_completion`).
- Compiling rustdoc doctests added/repaired throughout (previously several
  crates had `ignore`-fenced or otherwise non-compiling examples).
- **Packaging**: every workspace crate now has a per-crate `LICENSE` (a
  symlink to the root `LICENSE`) so `cargo package`/crates.io/docs.rs render
  the license file without relying on the workspace-level file alone, plus a
  `[package.metadata.docs.rs]` section (`all-features = true`) so docs.rs
  builds the full, feature-gated API surface.
- **`deny.toml`** at the workspace root: `cargo-deny` configuration
  (advisories: deny yanked crates; an explicit license allow-list covering
  MIT/Apache-2.0/BSD-2/3-Clause/ISC/Zlib/CC0-1.0/Unicode-3.0; documented
  `[bans]` exceptions for duplicate transitive dependency versions).

### Added — 2026-07-13 pass

- **Reference-differential oracle features** (opt-in, self-skipping when
  the reference tool is absent, so CI stays hermetic): `zstd-oracle`
  (oxiarc-zstd), `brotli-oracle` (oxiarc-brotli), `xz-oracle` (oxiarc-lzma
  and oxiarc-archive), `bzip2-oracle` (oxiarc-bzip2), `lz4-oracle`
  (oxiarc-lz4), `snappy-oracle` (oxiarc-snappy), `zlib-oracle`
  (oxiarc-deflate), `tiff-oracle` (oxiarc-lzw), `libaec-oracle`
  (oxiarc-szip), `zip-oracle` (oxiarc-archive) — joining 0.3.5's
  `lha-oracle`. Each is paired with always-run embedded golden corpora.
- New public APIs: `oxiarc_bzip2::decompress_with_limit`,
  `oxiarc_lzma::Lzma2Decoder::decode_chunk`,
  `oxiarc_lz4::FrameDescriptor::with_block_independence`,
  `ZlibStreamDecoder::with_max_output`, bounded snappy decode APIs.
- **oxiarc-brotli**: `src/tables.rs` (RFC 7932 tables) and
  `src/dict_data.bin` (the 122,784-byte Appendix A dictionary,
  CRC-verified, included in `cargo package`).
- CLI end-to-end matrix test suite: 13 formats × 7 subcommands against
  both oxiarc-produced and reference-tool-produced inputs (**303/303**,
  byte-identical), plus corrupt/truncated-input sweeps (0 panics, 0
  exit-101) and path-traversal checks.

### Documentation

- **README.md**: corrected the per-crate line-count table (previous figures
  were stale by roughly 1.7-3x), the archive-formats/format-support-matrix
  list (ISO 9660 was missing from the former, SZIP was miscategorized as an
  archive format rather than a standalone codec), the SIMD-CRC32 claim
  (actual gate is PCLMULQDQ + SSE4.1, not "SSE 4.2"), the LZH method list
  (lh0, lh1, lh4, lh5, lh6, lh7, lhd are implemented; lh2/lh3 are not), the Zstandard
  entropy-coding claim (Huffman literals are decode-only; the encoder emits
  Raw/RLE literals with predefined-FSE sequences), and documented ZIP
  AES/ZipCrypto encryption support, spanned-ZIP rejection, the actual
  semantics/limits of `--memory-limit`, and the always-on `try_reserve`
  allocation bounds that protect against malicious headers independently of
  `--memory-limit`.
- **oxiarc-cli/README.md**: documented exit code `2` (integrity failures,
  password/decryption failures, non-appendable `add` targets) and current
  Ctrl-C/partial-file behavior.
- **CONTRIBUTING.md**: documented the existing `fuzz/` cargo-fuzz harnesses
  and the opt-in `lha-oracle` feature (real-`lha`-CLI interop validation).

### Quality

- **2,426 tests passing, 0 failed, 0 ignored** (2,289 via `cargo nextest
  run --workspace --all-features` across 100 binaries + 137 doctests; 112
  suites) — up from 2,004 + 1 skipped.
- Zero clippy warnings (`--all-features --all-targets`, and with
  `--no-default-features`); `cargo build --workspace
  --no-default-features` green (Pure Rust default preserved);
  `cargo fmt --all --check` clean; rustdoc clean.
- ~114,394 lines across 336 Rust files (tokei).
- Acid test: the 64 real-world zstd frames (OxiGDAL Zarr v3 chunks) that
  triggered this campaign decode **64/64 byte-identical** through the
  `oxiarc` CLI with 0 panics; once this ships, OxiGDAL can bump the dep
  and remove the `#[ignore]` on `test_compute_zarr_stats_demo_fixture`.
- All COOLJAPAN policies compliant (no `unwrap`/`expect`/panics on
  untrusted-input paths, pure Rust, workspace deps, snake_case, <2000
  LoC/file).

## [0.3.5] - 2026-07-07

LZH/LHA interoperability release: the `-lh4-`/`-lh5-`/`-lh6-`/`-lh7-` codec is rewritten from OxiArc's private, non-canonical bitstream format to genuine canonical LHA wire format, closing the interoperability gap left open by the 0.3.4 hardening pass (which covered LZMA, bzip2, 7z, ZIP, TAR, and XZ). Validated bidirectionally against a corpus of real third-party `.lzh` archives and a live `lha` (Lhasa) CLI oracle. Also fixes a Miri-flagged undefined-behavior class in the CRC fast paths, resource leaks in three archive writers' `into_inner()`, and a CLI exit-code bug on unrecognized archive formats.

### Fixed
- **oxiarc-lzhuf**: `-lh4-`/`-lh5-`/`-lh6-`/`-lh7-` archives produced by OxiArc were unreadable by real LHA implementations (`lha`, LHarc, and compatible tools) despite round-tripping correctly through OxiArc's own reader — the codec spoke a private, non-canonical bitstream. The encoder and decoder (`encode.rs`, `decode.rs`, `huffman.rs`, `methods.rs`, `optimal.rs`, `streaming/{decoder,huffman}.rs`) were rewritten against the reference `lhasa` decoder to genuine canonical LHA format:
  1. **Bit order** — bits were packed LSB-first with every Huffman code bit-reversed to fake MSB semantics; canonical LHA is natively MSB-first. New `oxiarc_core::msb_bitstream::{MsbBitReader, MsbBitWriter}` module (mirroring LHA's `getbits`/`putbits`, including zero-bit padding past end-of-input) replaces the reversal hack.
  2. **Block command-count field** — the 16-bit per-block field was treated as a byte count; canonically it counts *commands* (literals or copies), so a copy-heavy block can cover far more output bytes than its count suggests.
  3. **Code-table length encoding** — temp-tree symbol values were mapped to code lengths as `v - 3` (with an always-unused `v == 3` slot); canonical LHA uses `v - 2`, and its zero-run skip mechanism is independent of (not layered onto) the temp table's own index-2 skip-count field.
  4. **Offset-table field widths** — the offset (P-tree) code-count field and history-buffer size are method-dependent (4 bits / 16 KiB for `-lh4-`/`-lh5-`; 5 bits / 64 KiB for `-lh6-`; 5 bits / 128 KiB for `-lh7-`), now centralized in new `LzhMethod::offset_bits`/`history_bits`/`max_offset_codes` helpers.

  Validated against 6 genuine third-party `.lzh` fixtures spanning header levels 0/1/2 (including a 1.24 MB multi-block archive) in `oxiarc-lzhuf/tests/data/`, plus a live `lha` (Lhasa) CLI oracle gated behind a new opt-in `lha-oracle` feature. `oxiarc-archive`'s `LzhWriter`/`LzhReader` needed no changes — the default level-2 header format was already spec-conformant.
- **oxiarc-lzhuf**: `parallel::lzh_compress_parallel`'s level-1 header builder computed its header-size byte as `20 + fname_len`, five bytes short of the spec value `25 + fname_len` (it omits the CRC-16(2) + OS-ID(1) + next-extension-size(2) fields the header does write). Archives it produced were internally self-consistent but real `lha` reported **zero entries** (`lha l`) in them. Fixed to `25 + fname_len`, matching `oxiarc-archive`'s `LzhWriter` and confirmed byte-exact against a real `-lh5-` level-1 fixture.
- **oxiarc-core**: Fixed undefined behavior, flagged by Miri, in the scalar and SIMD CRC-32/CRC-64 fast paths (`crc.rs`, `crc_simd.rs`; 7 call sites across the slice-by-8 scalar loop, the x86 PCLMULQDQ fold loop, and the aarch64 PMULL fold loop). Each loop guard computed `ptr.add(n)` speculatively to compare it against `end`, but `ptr.add` is itself UB once the result lands more than one byte past the end of the allocation — even when the pointer is only compared, never dereferenced. Replaced with address subtraction (`(end as usize) - (ptr as usize) >= n`), which never constructs an out-of-bounds pointer. No behavioral or performance change.
- **oxiarc-archive**: Fixed a resource leak in `ZipWriter::into_inner`, `TarWriter::into_inner`, and `LzhWriter::into_inner`. Each wrapped the *entire* writer struct in `ManuallyDrop` to suppress its `Drop` impl (which would otherwise re-run `finish()`), which also silently leaked every other owned field — most notably the `Arc<dyn ProgressSink>` progress-handle clone in all three (its refcount was never decremented), plus `ZipWriter`'s `entries` Vec. `TarWriter::into_inner` additionally leaked the writer itself if an I/O error occurred while writing the two end-of-archive zero blocks. All three now read `writer` out via `ptr::read` and explicitly drop the remaining owned fields; `TarWriter` computes the finish-write result before disposing of resources so an I/O error can no longer leak the writer. Regression tests added for all three, asserting the progress `Arc`'s strong count drops to 1 after `into_inner()`.
- **oxiarc-cli**: `oxiarc test` and `oxiarc list` (including `--json` mode) on a file with an unrecognized or corrupt archive format previously printed a message and exited `0`; both now return a non-zero exit code with a clean `unsupported or unrecognized archive format for <path>: <format>` error. (`oxiarc detect`, whose job is reporting `Format: Unknown` at exit 0, is intentionally unaffected.)

### Changed
- Dependency bumps: `glob` 0.3 → 0.3.3, `clap_complete` 4.6.6 → 4.6.7 (root `[workspace.dependencies]`).

### Added
- **oxiarc-core**: `msb_bitstream::{MsbBitReader, MsbBitWriter}` — public most-significant-bit-first bit I/O (re-exported at the crate root and in `prelude`), the canonical-LZH/LHA-oriented sibling of the existing LSB-first `bitstream` module used by DEFLATE.
- **oxiarc-lzhuf** / **oxiarc-archive**: opt-in `lha-oracle` Cargo feature (off by default; `oxiarc-lzhuf`'s implies `parallel`) that shells out to a real `lha` (Lhasa) CLI to validate OxiArc-produced archives — codec-level `lha t`/`x` round-trips in `oxiarc-lzhuf`, archive-level `lha l`/`t`/`x`/`p` round-trips in `oxiarc-archive`; self-skips cleanly when `lha` is not on `PATH`.
- Real-world LZH interop corpus (`oxiarc-lzhuf/tests/data/`: 6 genuine third-party `.lzh` archives across header levels 0/1/2, plus expected-plaintext goldens) and the suites exercising it: `oxiarc-lzhuf/tests/corpus_fixtures.rs`, `oxiarc-lzhuf/tests/lha_oracle.rs`, `oxiarc-archive/tests/lzh_corpus_reader.rs`, `oxiarc-archive/tests/lzh_lha_oracle.rs`, plus expanded chunked/incremental-decode coverage in `oxiarc-lzhuf/tests/streaming_integration.rs`.
- **oxiarc-deflate**: decoder-only regression test for a hand-built fixed-Huffman, length-258 (maximum-length) back-reference — closes a coverage gap; this case was previously only exercised indirectly via encode-then-decode roundtrips.
- **oxiarc-cli**: `tests/cli_unrecognized_format.rs` regression coverage for the exit-code fix above.

### Quality
- 1878 tests passing (all features, 0 skipped — 79 more than 0.3.4); zero clippy, check, and rustdoc warnings across the workspace.
- **oxiarc-snappy**: re-audited max-size-block (64 KiB) chunking handling — confirmed already correct and exhaustively covered by existing tests; no code changes needed.
- All COOLJAPAN policies compliant (no `unwrap` in production, pure Rust, workspace deps, snake_case, <2000 LoC/file).

## [0.3.4] - 2026-07-06

Interoperability hardening release: a batch of spec-conformance defects found via downstream FVRS integration testing was root-caused and fixed across the LZMA, bzip2, LZH, 7z, ZIP, TAR, and XZ stacks. All codecs were validated bidirectionally against reference implementations (liblzma, libbz2, bsdtar/libarchive, CPython stdlib) during development; the committed test suites are fully hermetic (golden vectors embedded, no external tools invoked at test time).

### Fixed
- **oxiarc-lzma**: Four LZMA/LZMA2 spec deviations that made oxiarc streams mutually incompatible with liblzma:
  1. **Distance-slot special probability table layout** — the table used a custom overlapping layout instead of the spec's `PosDecoders + dist - posSlot` indexing (LzmaSpec.cpp); fixed consistently in the decoder, LZMA2 decoder, encoder, and optimal-parser pricing, with the table resized 114 → 115 (`1 + FULL_DISTANCES - END_POS_MODEL_INDEX`, exposed as `SPEC_POS_PROBS`).
  2. **`State::update_literal` state mapping** — states ≥ 10 mapped 10 → 6 instead of the spec's `state - 6` (10 → 4), corrupting decode of real liblzma streams past ~64 KB.
  3. **LZMA2 control-byte reset field** — parsed bit 5 as a dictionary-reset flag instead of the spec's 2-bit field `(control >> 5) & 3` (1 = state reset, 2 = + props, 3 = + dict reset), so state-reset chunks wrongly discarded the dictionary.
  4. **End-of-stream marker inside LZMA2 chunks** — chunk payloads embedded the LZMA EOS marker, which liblzma rejects as corrupt because chunk compressed sizes must be consumed exactly; all three LZMA2 chunk writers now use the new additive `LzmaEncoder::compress_chunk` API.
- **oxiarc-lzma**: `Lzma2Encoder::encode` no longer silently truncates the 21-bit uncompressed / 16-bit compressed chunk-header size fields — inputs over 2 MiB, compressed payloads over 64 KiB, or incompressible inputs over 64 KiB now delegate to the chunked encoder internally (dict size, progress, and cancellation forwarded).
- **oxiarc-bzip2**: Complete bidirectional interop fix — oxiarc could previously neither decode real bzip2 streams nor produce streams bzip2 could decode. Root causes, all corrected:
  1. **Bit order** — the codec used LSB-first (DEFLATE-style) bit I/O while the bzip2 format is an MSB-first bit stream; new private MSB-first bit I/O module.
  2. **CRC-32 variant** — block and combined stream CRCs used the reflected ZIP/GZIP CRC-32 (0xEDB88320) instead of bzip2's non-reflected MSB-first CRC-32 (poly 0x04C11DB7, init 0xFFFFFFFF, final complement).
  3. **Pipeline layering** — MTF ran over the full 256-byte alphabet with a "compact remap" of MTF positions, instead of a symbol map of used byte values with MTF over the used-byte list (symbol `s` → MTF index `s-1`, EOB = `nUsed+1`).
  4. **Huffman alphabet off-by-one** — the decoder read `alpha_size + 1` code lengths and treated EOB as `num_symbols`; the encoder used `used + 3` symbols.
  5. **Format minimums** — the encoder could emit a single Huffman table (format minimum is 2) and undercounted selectors by excluding EOB from the symbol count.
  Decoding now follows the libbz2 limit/base/perm scheme; encoding uses weight-halving length-limited (17-bit) code construction with canonical `hbAssignCodes` assignment, and input is chunked per level (4/5 of `blockSize - 20`) so RLE1 expansion never exceeds the block limit. Corrupt-input handling hardened: `orig_ptr` bounds check, zero-run length cap, block-size overflow checks, selector-exhaustion/MTF-range errors, randomized-block rejection, RLE1 truncation errors — malformed streams now return errors instead of panicking or allocating unboundedly. Public API unchanged.
- **oxiarc-archive**: 7z reader spec-conformance overhaul (ported from a liblzma/bsdtar-validated implementation):
  - Listings report real per-entry sizes via proper `kSubStreamsInfo` size/CRC parsing (previously a size-zeroing bug left solid-folder members listed as 0 bytes).
  - 0-byte members, directories, and anti-items extract as empty data instead of aborting extraction with an error.
  - Variable-length numbers decode little-endian per 7zFormat.txt (values ≥ 16384 were previously misread).
  - Encoded (compressed) headers no longer contaminate main-streams state; pack-CRC digests are parsed per defined-bitmap; `kWinAttributes` external byte consumed per spec; backslash path separators normalized.
  - Extraction now verifies folder and per-entry CRC-32 with linear coder-chain support (Copy/LZMA/LZMA2/Deflate/BZip2/Delta/BCJ-x86) and errors on out-of-bounds entry ranges instead of silently returning truncated data.
- **oxiarc-archive**: ZIP name decoding data loss — non-EFS entry names were decoded with `String::from_utf8_lossy`, collapsing distinct Shift-JIS names (e.g. `あ.txt` / `い.txt`) into identical U+FFFD strings so extraction silently overwrote files. New `zip/name_codec` module implements the chain: strict UTF-8 (mandatory when EFS bit 11 is set) → Shift_JIS (only when EFS absent) → injective CP437 fallback (never emits U+FFFD; distinct raw names always decode distinct), wired into both the central-directory path (names and comments) and the local-file-header path used by `ZipStreamReader`.
- **oxiarc-archive**: ZIP writer now sets the EFS language-encoding flag (bit 11, 0x0800) for non-ASCII UTF-8 names in all entry paths (files, LZMA, AES/traditional encryption, raw append, directories), in both local headers and the central directory — previously python/bsdtar decoded oxiarc's Japanese names as cp437 mojibake.
- **oxiarc-archive**: TAR writer panicked on a char boundary (`byte index 155 is not a char boundary`) when splitting multibyte names for the UStar prefix field; `TarHeader::to_block` now splits on raw bytes at a `/` (always a UTF-8 boundary) and `write_string` floors truncation to a char boundary. Short-name (≤ 100 byte) and ASCII prefix/name-split blocks remain byte-identical to the previous serialization (locked by golden tests).
- **oxiarc-archive**: XZ container fixes (both directions were incompatible with liblzma despite correct LZMA2 payloads):
  - Writer: block-header CRC32 now covers the Block Header Size byte plus padded content per xz spec §3.1 (previously content-only, causing liblzma to reject all oxiarc `.xz` output as corrupt); index-record Unpadded Size now excludes block padding (previously off by up to 3 bytes).
  - Reader: blocks without a declared compressed size (i.e. every real liblzma stream) are now parsed via the self-describing LZMA2 chunk framing instead of scanning for the first 0x00 byte, which truncated at the first zero byte inside compressed data; block-padding read errors and invalid control bytes are now rejected instead of silently swallowed.
- **oxiarc-lzhuf**: lh5 (and lh4/lh6/lh7) streams were corrupt for inputs beyond the window size; three independent root causes fixed:
  1. `LzssEncoder::encode` pre-wrote the entire input into the circular window before matching, clobbering both history and lookahead for inputs larger than the window; the encoder now consumes input incrementally, keeping only true history in the window.
  2. The 16-bit per-block uncompressed-size field silently overflowed for blocks covering > 65535 bytes (blocks were split by token count); blocks are now capped at 0xFFFF bytes.
  3. lh6/lh7: the p-tree count field was written/read as 4 bits though np = 16/17 needs 5; lh7 full-window distance 65536 overflowed the u16 token (now capped, with guards in both decoders).
  Also fixed while auditing: Huffman lookup tables could not decode codes longer than `table_bits`, and the streaming decoder desynchronized when resuming mid-block in multi-block streams.
- **oxiarc-archive**: LZH archives containing `-lh1-` or `-lhd-` entries, or any unrecognized method, previously aborted the whole listing; unknown methods now list and skip per entry, `-lhd-` entries list as directories, level-1 extension chains (skip-size semantics) and spec-correct level-2 headers are now parsed.
- **oxiarc-archive**: LZH writer now encodes filenames as Shift_JIS (the LHA convention) at all header levels instead of raw UTF-8, so Japanese names are readable by standard LHA tools; the level-1 header-size byte was corrected to the spec value `25 + name_len`.
- **oxiarc-cli**: `oxiarc create` / `oxiarc convert` with an unwritable or unknown output extension (`.7z`, `.cab`, `.iso`, extension-less) silently fell back to writing ZIP data under the requested name; both now error up front (before any input is read or output created) listing the supported creation formats.
- **oxiarc-cli**: `create`/`convert` no longer force LZH entries to Store — non-Store compression levels now map to lh5 (the workaround for the encoder window bug above was removed; `convert` previously ignored its compression setting entirely for LZH output).

### Changed
- **oxiarc-lzhuf**: `LzhMethod` gained `Lh1`, `Lhd`, and `Unknown([u8; 5])` variants, and `LzhMethod::id()` now returns `[u8; 5]` by value; **oxiarc-core** `CompressionMethod` gained `Lh1`/`Lhd` (additive).
- **oxiarc-archive**: `LzhWriter` default header level changed 1 → 2 (the LHA 2.x/Lhaplus standard, required for spec-conformant Shift_JIS dirname/basename extension blocks); levels 1 and 3 remain selectable via `with_header_level`. `LzhWriter` also writes `-lhd-` entries for directories.
- **oxiarc-lzma**: `DistanceModel::special` array size changed 114 → 115 to match the spec layout (public field; no external users).
- **oxiarc-archive**: 7z `extract()` now errors on CRC mismatch instead of returning unverified data, and returns `Ok` with empty data for directories/0-byte files/anti-items.
- Dependency bumps: `clap_complete` 4.6.5 → 4.6.6, `indicatif` 0.18.4 → 0.18.6, `memmap2` 0.9.10 → 0.9.11.

### Added
- **oxiarc-lzhuf**: `-lh1-` (LZHUF: 4 KB window + adaptive Huffman) decoder and spec-conformant greedy encoder, ported from a validated implementation.
- **oxiarc-archive**: TAR write-side PAX long-name support — all `TarWriter` paths emit PAX `path`/`linkpath` records for names/linknames exceeding 100 bytes, with a char-boundary-safe trailing-suffix fallback name in the UStar block for non-PAX readers; round-trips exactly through `TarReader`'s existing PAX path and extracts byte-exact under bsdtar.
- **oxiarc-archive**: `ZipReader::entry_name_bytes(index)` accessor exposing per-entry raw name bytes, and `LocalFileHeader::filename_raw` (additive).
- **oxiarc-lzma**: `LzmaEncoder::compress_chunk` — encodes a chunk without the end-of-stream marker, for exact-size container framing (used by all LZMA2 chunk writers).
- Hermetic interop regression suites (golden vectors generated once with liblzma/libbz2/bsdtar/CPython during development, embedded as test data; no external tools at test time):
  - **oxiarc-lzma**: 12 tests decoding real liblzma raw LZMA1/`.lzma`/LZMA2 streams (small and > 64 KB) and liblzma-verified oxiarc outputs.
  - **oxiarc-bzip2**: 13 tests covering real libbz2 streams (incl. a 1.2 MB two-block stream crossing the 900 KB boundary), blessed encoder bytes, and corrupt-CRC rejection.
  - **oxiarc-archive**: 7z suite (bsdtar Copy/LZMA1/LZMA2 fixtures incl. LZMA-encoded headers, solid-folder substream sizes, 0-byte members), ZIP name-encoding suite (Shift-JIS no-EFS, EFS UTF-8, CP437 fixtures), and TAR PAX Japanese long-name suite (incl. a python3-tarfile golden and pre-fix UStar byte-compatibility goldens).
  - **oxiarc-lzhuf** / **oxiarc-archive**: lh5 beyond-window regression tests (8/16/64/100 KB, compressible and incompressible, CRC-16 verified) and an LHA level-1 fixture with `-lhd-`, Japanese-named `-lh1-`, and unknown-method entries.

### Quality
- 1799 tests passing (all features, 0 skipped); zero clippy, check, and rustdoc warnings across the workspace
- Bidirectional byte-exact interop verified at development time: xz/bz2 vs CPython liblzma/libbz2, 7z/tar vs bsdtar, ZIP Japanese names vs python zipfile and bsdtar
- All COOLJAPAN policies compliant (no `unwrap` in production, pure Rust, workspace deps, snake_case, <2000 LoC/file)

## [0.3.3] - 2026-06-06

### Fixed
- **oxiarc-brotli**: High-entropy / incompressible data now round-trips byte-for-byte across all quality levels (1–11); previously near-uniform or incompressible inputs failed to decode. Two distinct underlying bugs were fixed:
  1. **Incomplete length-limited Huffman codes** — for near-uniform, all-symbols-present literal distributions, the old `compute_code_lengths` heuristic (ideal `ceil(-log2 p)` lengths plus a Kraft-inequality fix-up loop) could emit a code-length table whose Kraft sum was strictly below `2^15`, i.e. an *incomplete* prefix code; the decoder then hit bit patterns that decoded to no symbol and failed with "invalid Huffman code: no matching code found".
  2. **Insert lengths above 319 silently truncated** — a single incompressible meta-block is emitted as one insert-and-copy command whose insert length spans the whole block, but the encoder only had insert-length categories 0–15 (max base 192, i.e. insert length ≤ 319) and wrote the excess in a 7-bit field that wrapped around, so the decoder ended the literal run early and desynchronised (content mismatch); the old decoder's extended-insert branch also did not invert the encoder.

### Changed
- **oxiarc-brotli**: `compute_code_lengths` now uses the **package-merge algorithm** (Larmore–Hirschberg) instead of the `ceil(-log2 p)` heuristic, always producing a *complete* and length-optimal (minimum-redundancy) length-limited code; adds `package_merge_lengths` and `is_complete_code` (a Kraft-sum invariant used in a debug assertion).
- **oxiarc-brotli**: Insert-length code table unified into a single source of truth `insert_length_code_info(cat) -> (base, extra_bits)` shared by encoder and decoder; insert categories extended from 15 up to `MAX_INSERT_LENGTH_CATEGORY = 40` (covering inserts up to ~4 MiB) via the extended insert-and-copy symbols 128–703; the decoder's split `decode_insert_length_short` / `decode_insert_length_extended` functions are collapsed into a single `decode_insert_length` driven by that shared table, guaranteeing encoder/decoder agreement across the full insert-length range.

### Added
- **oxiarc-brotli**: `high_entropy_roundtrip.rs` regression suite — byte-for-byte round-trip assertions across quality levels 1–11 for random 4 KiB and 64 KiB data, an incompressible counter sequence, all-distinct-byte and all-same-byte blocks, the empty input, varied sizes, and mixed compressible/incompressible content; plus a `decode_insert_length` ↔ `insert_length_code_info` inverse-check unit test over categories 0–40.

### Quality
- 1679 tests passing, 2 skipped (13 new); zero clippy warnings (`-D warnings`), zero rustdoc warnings
- All COOLJAPAN policies compliant (no `unwrap` in production, pure Rust, workspace deps, snake_case, <2000 LoC/file)

## [0.3.2] - 2026-05-31

### Added
- **oxiarc-szip**: Full AEC/SZIP encoding and decoding compliant with CCSDS-121.0-B-2 — `BitReader`/`BitWriter` for efficient bit manipulation; `encode` compresses sample arrays into AEC/SZIP bit streams; `decode` decompresses AEC/SZIP byte streams into raw sample bytes; `SzipParams` struct manages encoding/decoding parameters; `SzipError` enum for error handling. Round-trip tests cover various sample scenarios.

### Quality
- All COOLJAPAN policies compliant (no `unwrap` in production, pure Rust, workspace deps, snake_case, <2000 LoC/file)

## [0.3.1] - 2026-05-30

### Added
- **oxiarc-lzhuf**: Custom dictionary support — `LzhEncoder::with_dictionary(method, dict)` / `set_dictionary(&mut self, dict)` and `LzhDecoder::with_dictionary(method, size, dict)` / `set_dictionary` mirror the DEFLATE template; `LzssEncoder::preload_dictionary` / `LzssDecoder::preload_dictionary` seed hash chains and ring buffer from the dict tail; improves compression ratio when encoder and decoder share a known corpus prefix.
- **oxiarc-lzma**: Custom dictionary support — `LzmaEncoder::with_dictionary(level, dict_size, dict)` / `set_dictionary` fast-forwards the match finder through the dict prefix; `LzmaDecoder::with_dictionary(reader, props, dict_size, dict)` / `set_dictionary` seeds the circular dict ring from the dict tail; dict larger than `dict_size` is silently truncated to the last `dict_size` bytes.
- **oxiarc-lzma**: Thread-safe memory pool (`LzmaPool`) for amortizing large dict buffer allocations — power-of-two capacity buckets, configurable max buffers per bucket, `PooledBuf<'a>` RAII wrapper, `LzmaDecoderPooled<'p, R>` decoder backed by pooled buffer, `LzmaPool::decode` / `decode_from_header` convenience constructors.
- **oxiarc-archive**: Archive repair/recovery — `repair_zip<R: Read+Seek>(reader)` and `repair_tar<R: Read>(reader)` scan archives front-to-back independent of central directory / trailer integrity; ZIP scanner rolls over LFH signatures (`PK\x03\x04`), decompresses stored/deflated payloads, verifies CRC; TAR scanner walks 512-byte UStar blocks, recovers regular files and skips corrupt headers; results in `RepairReport { recovered_entries, skipped_ranges, warnings }` with per-entry `RecoveryStatus` (Verified/Recovered/RawOnly). Also available as `ZipRepair` / `TarRepair` builder structs with `RepairOptions`.
- **oxiarc-snappy**: 16 interop integration tests against Google Snappy wire-format golden vectors — empty, single-byte, 64 KiB boundary, 64 KiB+1, `max_compress_len` invariant, arbitrary-data roundtrip, crafted-stream decode, truncated/oversized-varint rejection.
- **oxiarc-brotli**: 19 interop integration tests covering all quality levels 0–11, empty/single-byte/binary/text/large-input roundtrips, `compress_with_params` variations, minimum-window (lgwin=16), compression-is-beneficial assertion, invalid parameter rejection.

### Quality
- 1647 tests, 2 skipped, zero warnings
- All COOLJAPAN policies compliant (no `unwrap` in production, pure Rust, workspace deps, snake_case, <2000 LoC/file)

## [0.3.0] - 2026-05-17

### Added
- **oxiarc-deflate**: Zopfli-style graph-based optimal DEFLATE parser (`OptimalParser`) — iterative shortest-path DP with per-pass Huffman cost retraining; opt-in via `Deflater::with_optimal_parsing(level)`; produces smaller output than greedy/lazy at the cost of extra CPU time. Adds `cost_table_from_lengths`, `cost_of_match`, `find_all_matches` helpers.
- **oxiarc-snappy**: Parallel frame compression (`compress_parallel`) via new `parallel` feature flag — rayon-based chunk-level parallelism mirroring the LZ4 parallel encoder; output is fully compatible with serial `FrameDecoder`.
- **oxiarc-lz4**: True bounded-memory streaming compressor/decompressor — `Lz4Compressor` now emits complete blocks on the fly (no full-input buffering); `Lz4Decompressor` uses a state-machine parser that processes one block at a time; both gain `with_memory_budget(usize)` builder.
- **oxiarc-lzhuf**: 4-byte multiplicative hash function replacing the old 3-byte hash (better avalanche, fewer collisions); new `LzssOptimalParser` two-pass optimal LZSS parser with Huffman-cost retraining; `LzhEncoder::with_optimal()` builder.
- **oxiarc-lzma**: BT4 binary tree match finder (`Bt4MatchFinder`) with 3-table hash (h2/h3/h4), cyclic-buffer BST, and configurable `cut_value` depth limit; `MatchFinder` trait abstracts both `HashChainMatchFinder` (levels 0–8) and `Bt4MatchFinder` (level 9); level 9 now delivers superior compression quality matching LZMA SDK.
- **oxiarc-core**: `MappedFile` — read-only memory-mapped file primitive (`memmap2`-backed, `mmap` feature flag); `Deref<Target=[u8]>` + `AsRef<[u8]>` for zero-copy archive access.

### Quality
- 1325 tests (44 new), 3 skipped, zero warnings
- All COOLJAPAN policies compliant (no `unwrap` in production, pure Rust, workspace deps, snake_case, <2000 LoC/file)

## [0.2.8] - 2026-05-08

### Added
- **oxiarc-core**: SIMD CRC32 via aarch64 PMULL — hardware-accelerated CRC32 computation on Apple Silicon / aarch64 using PMULL instructions; constants pinned from crc32fast reference; bitwise-identical to scalar path
- **oxiarc-lz4**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builders on `Lz4Compressor`, `Lz4Decompressor`, `Lz4DictFrameEncoder`, and `Lz4DictFrameDecoder`
- **oxiarc-zstd**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builders on `ZstdEncoder`, `ZstdStreamEncoder`, and `ZstdStreamDecoder`
- **oxiarc-lzma**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builders on `Lzma2Encoder`, `Lzma2Decoder`, and `Lzma2ChunkedEncoder`
- **oxiarc-archive**: Raw-preserve append in `oxiarc add` — ZIP and LZH entries are now preserved byte-for-byte when appending new entries, eliminating the decompress→recompress round-trip; added `ZipWriter::add_file_raw`, `LzhReader::read_raw_method_data`, and `LzhWriter::add_file_raw`
- **oxiarc-archive**: ISO 9660 read support via new `IsoReader` with PVD + Joliet UCS-2 filename support; format detection via magic bytes at LBA 16
- **oxiarc-cli**: `list`, `extract`, `info`, and `detect` commands now support `.iso` images
- **oxiarc-snappy**: Snappy CRC32C SSE 4.2 — hardware-accelerated CRC32C for x86_64 using SSE 4.2 intrinsics (`_mm_crc32_u64`) with runtime dispatch via `OnceLock`
- **oxiarc-cli**: `--memory-limit <BYTES>` option for `extract` and `list` subcommands (accepts suffixes such as `100M`, `1G`) to cap peak allocation per entry

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.8:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.7] - 2026-04-21

### Added
- **oxiarc-cli**: `oxiarc add` command for appending files to existing archives (ZIP, TAR, LZH formats); supports `--dry-run` and `--verbose` options
- **oxiarc-archive**: `lenient` mode enhancements — robust handling of malformed/partial archives in list and extract operations
- **oxiarc-archive**: `LzhExtensions` module for extended LZH archive manipulation (appending, rewriting entries)
- **oxiarc-archive**: Async LZH and TAR streaming support (`async_lzh.rs`, `async_tar.rs`)
- **oxiarc-core**: `CancellationToken` cooperative cancellation for archive operations
- **oxiarc-core**: `ProgressHandle` / `ProgressSink` progress reporting infrastructure
- **oxiarc-cli**: Man page generation (`man` subcommand via `clap_mangen`)
- **oxiarc-cli**: Colored output with ANSI support (`style.rs`, respects `NO_COLOR`/`--no-color`)
- **oxiarc-cli**: Windows long path support and reserved filename handling during extraction

### Testing
- End-to-end tests for ZIP AES-256 and ZipCrypto encryption (`zip_encryption_e2e.rs`)
- Progress callbacks and cancellation tests for Brotli (`progress_cancel.rs`)
- CLI integration tests: add command, color output, man page generation, password extraction, lenient mode, compression threshold, tree listing, Windows filenames

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)
- Updated: `clap` 4.6.1, `tokio` 1.52.1

### Crates in This Release
All crates published at version 0.2.7:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.6] - 2026-03-21

### Added
- **oxiarc-brotli**: `write_prefix_code_and_build_tree()` — unified prefix code writing and Huffman tree construction for encoder use
- **oxiarc-brotli**: Comprehensive roundtrip tests for compress/decompress (simple, binary pattern, uniform data)

### Fixed
- **oxiarc-brotli**: `is_single_symbol()` now correctly identifies true single-symbol Huffman trees (all code lengths must be 0); previously returned true for trees with exactly one non-zero code length, causing incorrect decoding
- **oxiarc-brotli**: Kraft inequality tracker changed to `i32` to prevent potential overflow in complex prefix code reading

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings (strict mode with all lint checks)
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.6:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.5] - 2026-03-18

### Added
- **oxiarc-brotli**: New crate — Brotli compression (RFC 7932)
  - Quality levels 0-11 with static dictionary support
  - LZ77 and context-dependent Huffman coding
  - Streaming compression/decompression API
- **oxiarc-snappy**: New crate — Snappy compression
  - Block format and framed format with CRC32C checksums
  - Streaming Write/Read API
- **oxiarc-deflate**: Streaming compression/decompression
  - `GzipStreamEncoder`/`GzipStreamDecoder` with flush modes (sync_flush, full_flush, partial_flush)
  - `ZlibStreamEncoder`/`ZlibStreamDecoder` with configurable block sizes
- **oxiarc-lz4**: Acceleration parameter (`compress_block_with_accel`) with adaptive skip scaling
- **oxiarc-lzw**: Streaming encoder/decoder (`LzwStreamEncoder`/`LzwStreamDecoder`, TIFF and GIF modes)
- **oxiarc-core**: `EntryBuilder` pattern with fluent API; Serde serialization for Entry types (optional `serde` feature)
- **oxiarc-archive**: Brotli/Snappy archive integration (`BrotliReader`/`BrotliWriter`, `SnappyReader`/`SnappyWriter` with format detection)
- **oxiarc-cli**: Dry-run mode (`--dry-run`/`-n`), sort by ratio, Brotli/Snappy format support

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings (strict mode with all lint checks)
- 100% test pass rate (1038 tests)
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.5:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.4] - 2026-03-16

### Changed
- Updated dependencies: `clap` 4.5→4.6, `clap_complete` 4.5→4.6
- Clippy fixes: collapsible match guards, `sort_by` → `sort_by_key`, removed redundant `.max(0)`

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings (strict mode with all lint checks)
- 100% test pass rate (799 tests)
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.4:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-archive, oxiarc-cli

## [0.2.3] - 2026-03-11

### Added
- `oxiarc-archive`: Async ZIP support (`async_zip` module)
- `oxiarc-deflate`: Async deflate support (`async_deflate` module) and GZip module (`gzip`)
- `oxiarc-lzw`: New GIF LZW codec (`gif_lzw` module) and LSB bitstream support (`bitstream_lsb` module)

### Changed
- `oxiarc-deflate`: Various improvements to LZ77 match-finding, deflate engine, and lib interface
- `oxiarc-lz4`: Dictionary and HC (high-compression) improvements
- `oxiarc-lzma`: Encoder optimizations, model refinements, and optimal parsing improvements
- `oxiarc-zstd`: Frame, streaming, and lib improvements
- `oxiarc-lzhuf`: LZSS improvements
- `oxiarc-bzip2`: BWT improvements
- `oxiarc-archive`: ZIP header reader and module-level improvements

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- 100% test pass rate
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.3:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-archive, oxiarc-cli

## [0.2.2] - 2026-03-10

### Added

#### oxiarc-zstd: Full Zstandard Encoder Implementation

- **`bitwriter` module** — two bitstream writers required by the Zstandard encoding pipeline:
  - `ForwardBitWriter`: LSB-first bit packing; `write_bits(value: u32, num_bits: u8)` (up to 25 bits), `write_bit(bool)`, `finish() -> Vec<u8>`, `bit_position()`, `byte_len()`, `is_empty()`, `as_bytes()`, `with_capacity()`; used for FSE table description headers
  - `BackwardBitWriter`: sentinel-marked reversed bitstream compatible with `FseBitReader`; `write_bits(value: u64, num_bits: u8)`, `finish() -> Vec<u8>` (empty input yields `[0x01]` sentinel), `len()`, `is_empty()`, `with_capacity()`; used for FSE sequence encoding

- **`lz77` module** — LZ77 match-finder for compressed block production:
  - `LevelConfig`: 22 compression levels mapping level index to `hash_log` (17–20 bits), `chain_log` (0–20 bits), `search_depth` (1 to level×32), `lazy_matching` flag, `lazy_min_gain`, and `target_block_size` (128 KB)
  - `Lz77Sequence { literals: Vec<u8>, offset: usize, match_length: usize }` — public parsed-sequence type
  - `MatchFinder`: hash-chain algorithm using multiply-shift hashing (`HASH_PRIME = 0x9E3779B1`); `find_sequences(&[u8], dict: &[u8]) -> Result<Vec<Lz77Sequence>>`; `reset()`; internal `CombinedBuffer` avoids copying dictionary data; 8-byte-at-a-time comparison via `get_u64()` fast path; constants `MIN_MATCH=3`, `MAX_MATCH=65539`
  - New public re-exports: `LevelConfig`, `Lz77Sequence`, `MatchFinder`

- **`huffman_encoder` module** — canonical Huffman coding for Zstandard literals:
  - `HuffmanEncoder::from_frequencies(frequencies: &[u64; 256]) -> Option<Self>` — returns `None` for ≤1 distinct symbol; constructs min-heap tree via `BinaryHeap`
  - `limit_code_lengths(code_lengths: &mut [u8], max_length: u8)` — Kraft inequality rebalancing to enforce `MAX_CODE_LENGTH = 11`
  - `serialize_table() -> Vec<u8>` — header byte = `127 + num_weight_symbols`, 4-bit weights packed two per byte, high nibble first
  - `encode_literals(literals: &[u8]) -> Vec<u8>` — produces backward-compatible sentinel byte stream
  - `get_code(symbol: u8) -> (u32, u8)`, `max_bits()`, `num_symbols()`, `weights()`

- **`fse_encoder` module** — FSE (Finite State Entropy) encoding tables and state machine:
  - `FseEncodeTable::from_frequencies(frequencies: &[u32], accuracy_log: u8) -> Option<Self>` — returns `None` for ≤1 distinct symbol; `normalize_frequencies()` with probability spreading; `spread_remainder()` for residual probability assignment; `serialize() -> Vec<u8>` (4-bit `accuracy_log - 5` header, then variable-length probability encoding); `reset_counters()`, `initial_state_for(symbol: u8) -> u16`, `get_encoding_info()`, `state_symbol()`, `encode_symbol()`
  - `FseStateEncoder<'a>`: `init(table, symbol: u8) -> Self`; `encode(symbol: u8) -> (u8, u32)` (returns bits to flush and their count); `flush() -> (u8, u32)`; `state() -> u16`
  - Standalone functions: `ll_code(literal_length: usize) -> (u8, u8, u32)`, `ml_code(match_length: usize) -> (u8, u8, u32)`, `of_code(offset: usize) -> (u8, u8, u32)` — encode Zstandard literal-length, match-length, and offset codes with baseline/extra-bits; `choose_mode(frequencies: &[u32], total: u32) -> SequenceCompressionMode`, `choose_accuracy_log(total: u32, distinct: usize) -> u8`
  - `pub enum SequenceCompressionMode { Predefined, Rle(u8), Fse(FseEncodeTable) }` — public encoding mode selector

- **`compressed_block` module** — Zstandard compressed block assembly:
  - `pub fn encode_compressed_block(sequences: &[Lz77Sequence]) -> Result<Vec<u8>>` — assembles a complete Zstandard compressed block from LZ77 sequences
  - Internal: literals-section encoding choosing Raw, RLE, or Compressed (Huffman) headers; `encode_sequences_section()` with variable-count encoding (1–3 bytes); `encode_sequences_bitstream()` using `BackwardBitWriter` and backward-order FSE state encoding; `compute_fse_states_backward()` traverses sequences in reverse; predefined FSE table probabilities for LL (accuracy_log=6, 36 symbols), OF (accuracy_log=5, 29 symbols), ML (accuracy_log=6, 53 symbols)

- **`streaming` module (public)** — `std::io` trait adapters for Zstandard:
  - `ZstdStreamEncoder<W: Write>`: `new(writer: W, level: i32)`, `with_dictionary(writer, level, dict: Vec<u8>)`, `finish() -> io::Result<W>` (must be called to flush), `buffered_bytes() -> usize`, `is_finished() -> bool`; implements `Write` buffering data until `finish()`
  - `ZstdStreamDecoder<R: Read>`: `new(reader: R)`, `with_dictionary(reader, _dict: Vec<u8>)`, `decompressed_size() -> usize`, `is_finished() -> bool`; implements `Read` with eager full-decompression on first call
  - New re-exports: `ZstdStreamEncoder`, `ZstdStreamDecoder`

- **`dict` module (public)** — Zstandard dictionary support:
  - `pub const MAX_DICT_SIZE: usize = 1_048_576` (1 MB limit)
  - `ZstdDict`: `new(data: Vec<u8>) -> Result<Self>` (rejects oversized data), `id() -> u32` (lower 32 bits of XXH64 with seed 0), `data() -> &[u8]`, `len()`, `is_empty()`, `into_data() -> Vec<u8>`
  - `pub fn train_dictionary(samples: &[&[u8]], dict_size: usize) -> Result<ZstdDict>` — n-gram extraction (lengths 4–16 bytes, `MIN_FREQUENCY = 2`), frequency×length scoring, descending-score sort, greedy substring deduplication; falls back to raw sample concatenation when no common n-grams exist; output capped at `dict_size` bytes
  - New re-exports: `ZstdDict`, `train_dictionary`

- **`ZstdEncoder` — new public API**:
  - `set_level(level: i32)` and `set_dictionary(dict_data: Vec<u8>)` mutating methods
  - `write_compressed_blocks(&[u8]) -> Result<Vec<u8>>` — dispatches to `MatchFinder` and `encode_compressed_block()`
  - Free functions: `compress_with_level(data: &[u8], level: i32) -> Result<Vec<u8>>`, `encode_all(data: &[u8], level: i32) -> Result<Vec<u8>>`, `decode_all(data: &[u8]) -> Result<Vec<u8>>`
  - Feature-gated `compress_parallel(data: &[u8], level: i32, num_threads: usize) -> Result<Vec<u8>>` (Rayon, `parallel` feature)
  - New re-exports: `compress_with_level`, `encode_all`, `decode_all`, `BackwardBitWriter`, `ForwardBitWriter`; feature-gated `compress_parallel`

#### oxiarc-lz4: Dictionary Frame Support

- **`frame_dict` submodule** — dictionary-aware LZ4 frame encoding and decoding:
  - Free functions: `compress_frame_with_dict(input: &[u8], dict: &Lz4Dict) -> Result<Vec<u8>>` (stores dict ID in FLG byte), `compress_frame_with_dict_options(input, dict, desc: FrameDescriptor) -> Result<Vec<u8>>`, `decompress_frame_with_dict(input, max_output, dict) -> Result<Vec<u8>>` (verifies dict ID matches frame header), `get_frame_dict_id(input: &[u8]) -> Result<Option<u32>>`
  - `Lz4DictFrameEncoder { dict, desc }`: `new(dict: Lz4Dict)`, `with_options(dict, desc)`, `encode(input: &[u8]) -> Result<Vec<u8>>`, `encode_with_size()`, `dict()`, `dict_id() -> u32`
  - `Lz4DictFrameDecoder { dict }`: `new(dict: Lz4Dict)`, `decode(input, max_output)`, `can_decode(input) -> bool` (checks dict ID), `dict()`, `dict_id() -> u32`
  - `Lz4DictCompressor`: implements `Compressor` trait; `new(dict)`, `with_options(dict, desc)`, `dict()`; full `reset()` support
  - `Lz4DictDecompressor`: implements `Decompressor` trait; `new(dict)`, `dict()`; full `reset()` support
- `FrameDescriptor::with_dict_id(id: u32)` — new builder method for setting dictionary ID in frame headers

### Refactored

#### oxiarc-lz4: `frame` Module Split

The monolithic `frame/mod.rs` was split into five dedicated submodules with no public API breakage:

- `frame/types.rs` — `BlockMaxSize` enum, `FrameDescriptor` struct, `LZ4_FRAME_MAGIC`, `LZ4_LEGACY_MAGIC`
- `frame/compress.rs` — `compress()`, `compress_with_options()`, `compress_with_options_parallel()`, `compress_parallel()` (feature-gated)
- `frame/decompress.rs` — `decompress()` supporting both `LZ4_FRAME_MAGIC` and `LZ4_LEGACY_MAGIC`; `decompress_frame()`, `decompress_legacy()`; adds legacy LZ4 format decoding
- `frame/streaming.rs` — `Lz4Compressor` and `Lz4Decompressor` (implementing `Compressor`/`Decompressor` core traits)
- `frame/frame_dict.rs` — new dictionary compression logic (see Added section above)

#### oxiarc-archive: ZIP `header` Module Split

The ZIP `header/mod.rs` was split into three dedicated submodules with no public API breakage:

- `header/types.rs` — enhanced type definitions:
  - `DataDescriptor` struct with `read<R: Read>(reader, is_zip64: bool) -> Result<(Self, usize)>`: handles optional `0x08074B50` signature detection and ZIP64 8-byte size fields
  - `CentralDirEntry`: new methods `needs_zip64() -> bool`, `build_zip64_extra() -> Vec<u8>`, `write<W: Write>()`, `written_size() -> usize`
  - `LocalFileHeader`: added `uncompressed_size_64: Option<u64>` and `compressed_size_64: Option<u64>` fields; new methods `parse_zip64_extra()`, `actual_uncompressed_size() -> u64`, `actual_compressed_size() -> u64`, `has_data_descriptor() -> bool`
  - New constants: `ZIP64_MARKER_16: u16 = 0xFFFF`, `FLAG_DATA_DESCRIPTOR: u16 = 0x0008`, `METHOD_AES_ENCRYPTED: u16 = 99`
  - New free functions: `is_entry_encrypted()`, `get_entry_aes_encryption_info()`, `is_entry_traditional_encrypted()`
- `header/reader.rs` — `ZipReader<R: Read + Seek>`:
  - Primary path `read_from_central_directory()` with ZIP64 EOCD64 locator support (`0x07064B50` signature); fallback `read_from_local_headers()`
  - `extract()`, `extract_with_password()` (ZipCrypto/PKWARE), `extract_with_password_aes()` (WinZip AE-2 with HMAC-SHA1 authentication tag verification), `extract_encrypted()` (auto-detects encryption method)
  - Static helpers: `is_encrypted()`, `get_aes_encryption_info()`, `is_traditional_encrypted()`; `entry_by_name()`
- `header/writer.rs` — `ZipWriter<W: Write>`:
  - `new()`, `set_compression()`, `add_file()`, `add_file_with_options()`, `add_encrypted_file()` (AES-256 CTR + PBKDF2-SHA1 default), `add_encrypted_file_with_options()`, `add_encrypted_file_traditional()`, `add_encrypted_file_traditional_with_options()`, `add_directory()`, `finish()`, `into_inner()`
  - Automatic ZIP64 upgrade: local headers, central directory entries, and EOCD all promote to ZIP64 when `compressed_size`, `uncompressed_size`, or file offset exceeds `0xFFFFFFFF`; extra field ID `0x0001`
  - Implements `Drop` calling `finish()`

### Changed

- **oxiarc-zstd**: `ZstdEncoder` internal structure extended with `level: i32` (0–22) field, `dictionary: Option<Vec<u8>>`, and `dict_id: Option<u32>`; dictionary ID written as 4-byte little-endian field in Zstandard frame header when present; `Single_Segment_flag` always set
- **oxiarc-zstd/fse.rs**: Added `FseTable::from_entries(accuracy_log: u8, entries: Vec<FseTableEntry>) -> Self` constructor; added `test_backward_writer_reader_roundtrip` test verifying `BackwardBitWriter` ↔ `FseBitReader` round-trip correctness
- **Dependencies**: Updated to latest versions
  - clap: 4.5.57 → 4.5.60
  - clap_complete: 4.5.65 → 4.5.66
  - indicatif: 0.18.3 → 0.18.4
  - dialoguer: 0.11.0 → 0.12.0
  - tokio: 1.49.0 → 1.50.0
  - memmap2: 0.9.9 → 0.9.10

### Documentation
- Updated README.md files for all subcrates

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- 100% test pass rate
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)
- Security audit passed

### Crates in This Release
All crates published at version 0.2.2:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-archive, oxiarc-cli

## [0.2.1] - 2026-02-09

### Added
- **oxiarc-archive**: ZIP encryption support
  - Traditional ZIP encryption (ZipCrypto) implementation
  - Encryption and decryption modules for password-protected archives
  - Comprehensive crypto primitives for secure archive handling
- **oxiarc-core**: Advanced I/O capabilities
  - Async I/O support for non-blocking operations
  - SIMD-accelerated CRC implementations for faster checksums
  - Memory-mapped I/O (mmap) support for efficient large file handling
  - Enhanced CRC benchmarks and performance testing
- **oxiarc-lz4**: Dictionary support for improved compression
  - LZ4 dictionary compression for better ratios on similar data
  - Dictionary API for streaming compression scenarios
- **oxiarc-lzhuf**: Streaming support
  - Streaming compression and decompression API
  - Comprehensive streaming integration tests
- **oxiarc-lzma**: LZMA2 chunking improvements
  - Enhanced LZMA2 chunk handling for better performance
  - Optimal parsing improvements for compression efficiency
- **oxiarc-deflate**: Enhanced compression capabilities
  - Improved LZ77 implementation with better match finding
  - Enhanced zlib support with more compression options
- **oxiarc-cli**: Enhanced utilities and command improvements
  - New utility modules for better file handling
  - Improved list and extract commands

### Changed
- **oxiarc-core**: Enhanced ring buffer implementation
- **oxiarc-deflate**: Optimized Huffman coding
- **CLI**: Improved error handling and user feedback

### Fixed
- Multi-file archive handling edge cases
- DEFLATE compression edge cases in simple scenarios

### Tests
- Added comprehensive ZIP encryption tests
- Added streaming integration tests for LZHUF
- Added multi-file bug regression tests
- Added simple DEFLATE test cases

## [0.2.0] - 2026-02-06

### Added
- **oxiarc-lzw**: Complete LZW compression implementation for TIFF and GIF formats
  - MSB-first and LSB-first bitstream support
  - Variable bit width encoding (9-12 bits for TIFF, 2-12 bits for GIF)
  - Configurable for TIFF and GIF compatibility modes
  - Comprehensive test suite with 427 total tests
- **Documentation**: Added comprehensive README.md files for all codec crates:
  - oxiarc-bzip2: BZip2 compression guide with examples
  - oxiarc-lz4: LZ4 compression guide with parallel compression examples
  - oxiarc-lzw: LZW compression guide for TIFF/GIF formats
  - oxiarc-zstd: Zstandard compression guide with FSE/Huffman details
- **Tests**: Marked resource-intensive stress tests with `#[ignore]` attribute
  - Reduced default test suite runtime from 137s to 32s
  - Stress tests can still be run with `cargo test -- --ignored`

### Changed
- **Dependencies**: Updated to latest versions
  - clap: 4.5.56 → 4.5.57
  - clap_complete: 4.5.56 → 4.5.65
  - criterion: 0.8.1 → 0.8.2
- **Workspace**: Improved workspace dependency management
  - Fixed oxiarc-lzw to use `workspace = true` for oxiarc-core dependency
  - All subcrates now consistently use workspace version references
- **Testing**: Optimized test performance without sacrificing coverage
  - Default `cargo test` now runs in ~32s (76% faster)
  - Parallel stress tests moved to optional ignored tests

### Fixed
- Version synchronization across all 10 workspace crates
- Workspace dependency references in oxiarc-lzw
- Publish script version updated to 0.2.0

### Quality
- ✓ Zero clippy warnings (strict mode with `-D warnings`)
- ✓ Zero rustdoc warnings
- ✓ 100% test pass rate (427/427 tests)
- ✓ All policies compliant (no unwrap, pure Rust, latest crates, workspace)
- ✓ Security audit passed (0 vulnerabilities, 131 dependencies scanned)

### Crates in This Release
All crates published at version 0.2.0:
- oxiarc-core: Core traits and utilities
- oxiarc-deflate: DEFLATE/GZIP compression
- oxiarc-lzhuf: LZHUF compression (LZH format)
- oxiarc-lzw: LZW compression (TIFF/GIF) **[NEW]**
- oxiarc-lzma: LZMA compression
- oxiarc-bzip2: BZip2 compression
- oxiarc-lz4: LZ4/LZ4-HC compression
- oxiarc-zstd: Zstandard compression
- oxiarc-archive: Multi-format archive support
- oxiarc-cli: Command-line interface

## [0.1.0] - 2026-01-17

### Added
- Initial release of OxiArc - Pure Rust Archive/Compression Library
- **oxiarc-core**: Foundation crate with core traits and utilities
  - `Compressor` and `Decompressor` traits
  - CRC32, CRC64, CRC16 implementations
  - Bitstream utilities
- **oxiarc-deflate**: DEFLATE compression implementation
  - RFC 1951 compliant
  - GZIP support (RFC 1952)
  - Huffman coding and LZ77 compression
- **oxiarc-lzhuf**: LZHUF compression
  - LZH archive format support
  - Sliding dictionary with static Huffman
- **oxiarc-lzma**: LZMA compression
  - LZMA1 and LZMA2 support
  - Range coding and LZ dictionary
  - XZ format support
- **oxiarc-bzip2**: BZip2 compression
  - Burrows-Wheeler Transform
  - Parallel compression with Rayon
  - Compression levels 1-9
- **oxiarc-lz4**: LZ4 compression
  - LZ4 frame format
  - LZ4-HC (high compression)
  - XXHash checksum support
  - Parallel compression
- **oxiarc-zstd**: Zstandard compression
  - FSE (Finite State Entropy) coding
  - Huffman coding
  - Parallel compression support
- **oxiarc-archive**: Multi-format archive handling
  - Format detection
  - ZIP, LZH, CAB, GZIP, BZIP2, LZ4, XZ, ZSTD support
- **oxiarc-cli**: Command-line interface
  - Compress/decompress commands
  - Archive extraction and creation
  - Multiple format support

### Quality Standards
- Pure Rust implementation (no C/Fortran dependencies)
- Zero unwrap() in production code
- Comprehensive test coverage
- Full documentation with examples
- Workspace-based dependency management

[Unreleased]: https://github.com/cool-japan/oxiarc/compare/v0.4.0...HEAD
[0.4.1]: https://github.com/cool-japan/oxiarc/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/cool-japan/oxiarc/compare/v0.3.6...v0.4.0
[0.3.6]: https://github.com/cool-japan/oxiarc/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/cool-japan/oxiarc/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/cool-japan/oxiarc/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/cool-japan/oxiarc/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/cool-japan/oxiarc/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/cool-japan/oxiarc/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/cool-japan/oxiarc/compare/v0.2.6...v0.3.0
[0.2.6]: https://github.com/cool-japan/oxiarc/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/cool-japan/oxiarc/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/cool-japan/oxiarc/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/cool-japan/oxiarc/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/cool-japan/oxiarc/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/cool-japan/oxiarc/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/cool-japan/oxiarc/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/cool-japan/oxiarc/releases/tag/v0.1.0

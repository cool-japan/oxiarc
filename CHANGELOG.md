# Changelog

All notable changes to the OxiArc project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.2] - Unreleased

**The P2/P3 program: HTTP `Content-Encoding` decoding and three new image
container formats, all Pure Rust, closing the two routes (`flate2` via
`ureq`'s default `gzip` feature, and `png`/`tiff` via the `image` facade)
that accounted for `flate2` in 39 of 93 `~/work` project lockfiles.** Five
new crates — `oxiarc-http`, `oxiarc-png`, `oxiarc-jpeg`, `oxiarc-tiff`,
`oxiarc-image` — bring the workspace from 13 to **18 member crates**. The
keystone underneath all of them is a rewritten, genuinely resumable
DEFLATE/zlib/gzip core (`InflateStream`, `WrappedInflate`, `InflateReader`/
`AsyncInflateReader`) that replaces the `read_to_end`-then-decode decoders
every streaming consumer used to be built on; `oxiarc-zstd` and
`oxiarc-brotli` gained the equivalent push decoders (`ZstdStream`,
`BrotliStream`). Alongside the new crates: a from-scratch DEFLATE
*encoder* rewrite (`oxiarc-deflate`, now byte-identical to CPython's
`zlib.compress` at every level 1-9; level 0's stored blocks are cut at a
different, RFC-legal boundary, so its bytes differ — never by being
larger), a decode-throughput rebuild of
`oxiarc-zstd` around the reference decoder's data layout, a TIFF/GIF LZW
decoder rebuilt to libtiff's own algorithm shape, Brotli shared-dictionary
support and `Content-Encoding: dcb`/`dcz` (RFC 9842), the legacy UNIX
`compress`/`.Z` container (`oxiarc-lzw::z`, `Content-Encoding: compress`),
and CCITT Group 3/4 uncompressed mode. **No archive/stream wire format
changed and no public read/decode-path API was removed.** Four narrow
breaking changes, all scoped and named at their own entries below: (1)
`LzwConfig::GIF` is now genuinely LSB-first (it was accidentally identical
to `TIFF_OLD_STYLE`'s MSB-first value before this release — no published
version ever carried the old, wrong value); (2) `LzwConfig` gained a
`bit_order` field, so any external struct-literal construction of it no
longer compiles without setting one (`LzwConfig::new` and the named
constants are unaffected — `oxiarc-lzw` has been published since before
this cycle, unlike the five brand-new crates below, so this one reaches
real downstream callers); (3) `oxiarc-zstd`'s `decompress_multi_frame` and
its `_with_dict` twin no longer tolerate *leading* garbage or a
truncated-but-recognised skippable-frame prefix (callers relying on
`Ok(vec![])` for non-Zstandard input must check the error instead —
trailing-garbage tolerance is unchanged); (4) the `dcz` wire preamble
gained a dictionary-binding header within this same unreleased cycle, so a
`dcz` body produced by an *earlier* unreleased build of `oxiarc-http` is
no longer readable by the version shipping in this release (no published
consumer is affected either way). Final state,
measured 2026-09-08 on the full workspace: **5,501 tests passing, 0
failed, 0 skipped** (5,155 via `cargo nextest run --workspace
--all-features` + 346 doctests via `cargo test --doc --workspace
--all-features`); zero clippy warnings on every crate individually
(`--all-features --all-targets -D warnings`, also with
`--no-default-features`) as reported by each track, **and** workspace-wide:
`cargo fmt --all -- --check`, `cargo clippy --workspace` in both feature
sets, `cargo build --workspace --no-default-features`, `RUSTDOCFLAGS='-D
warnings' cargo doc --workspace --no-deps --all-features` and a real
`cargo +1.85.0 check --workspace` MSRV compile all ran green on the
quiescent tree on 2026-09-08 (GATES, re-run independently by the FINALGATE
gatekeeper and again after its findings were applied); `cargo deny
check bans` clean with the new PNG/JPEG/TIFF/`image` bans in place; ~225,700
Rust lines across 714 files (tokei, workspace-wide including `fuzz/`).

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
  `oxiarc-tiff`'s two-channel chunky JPEG refusal — a named error for part
  of this same unreleased version and never in a published one — was
  waiting on.
- **A resumable, truly-incremental DEFLATE/zlib/gzip core, replacing the
  read-to-end decoders that used to serve every consumer.** The root
  problem this closes: `GzipStreamDecoder`/`ZlibStreamDecoder::fill_buffer`
  called `read_to_end` and decoded the *whole* stream before serving the
  first byte, `ZlibStreamDecoder::with_max_output` was enforced only after
  full expansion, and `Inflater as Decompressor` could not accept partial
  input (a second call after a mid-stream EOF returned `Ok((0, n, Done))`
  with the output silently truncated) — the same shape of bug `BrotliDecompressor<R>`
  and `ZstdStreamDecoder<R>` had, and `oxiarc-zstd` had no output cap at
  all. New `oxiarc_deflate::stream::InflateStream` —
  `new`/`with_window_capacity(bytes)`, `inflate(&mut self, input: &[u8],
  output: &mut [u8], flush: FlushMode) -> Result<InflateProgress { consumed,
  produced, status }>` (`InflateStatus::{NeedInput, NeedOutput, StreamEnd}`),
  `set_dictionary`, `reset`/`reset_keep_history`/`reset_for_next_member`
  (preserves the bit accumulator across gzip members), `align_to_byte`,
  `at_sync_flush`, `take_buffered_byte`, `bits_consumed`, `total_in`,
  `total_out`, `with_max_output`, `with_ratio_guard` — a fast cached-symbol
  loop while `input_remaining >= 8 && output_space >= 258`, a careful
  per-symbol resumable path otherwise, 32 KiB linear history, and a sticky
  fault latch (cleared only by `reset()`) that turns "decoder already
  failed" into an error on every later call instead of quietly returning
  short output. New `oxiarc_deflate::wrapper::WrappedInflate` layers zlib
  and gzip framing on top: `new(InflateWrapper::{Raw, Zlib, Gzip, Auto})`
  (`Auto` sniffs the 2-byte zlib header at offset 0 only, no Adler-32
  retry), `multi_member`, `trailing_policy(TrailingPolicy::{Reject,
  AllowZeros, Stop})`, `with_max_output`, `with_ratio_guard`,
  `with_dictionary`, `verify_header_crc` (gzip FHCRC, **default `true`**;
  the legacy one-shot `gzip_decompress`/`GzipDecoder` *were* re-based on
  this core and now verify FHCRC too — a member whose FHCRC is corrupt,
  which 0.4.1 silently accepted, is now rejected exactly as `gzip -d`
  rejects it; see **Changed** below), `verify_checksum`
  (default `true`; PNG's IDAT chain sets it `false` and relies on its own
  CRC-per-chunk instead), `strict_first_member` (default `true` on every
  new consumer; the legacy `GzipStreamDecoder` keeps returning `Ok(0)` on
  non-gzip leading bytes, unchanged), `gzip_header() -> Option<&GzipHeaderInfo>`,
  `members_decoded`, `reset`. New `oxiarc_deflate::{InflateReader<R: Read>,
  AsyncInflateReader<R: AsyncRead>}` wrap either core behind a mandatory
  64 KiB output staging buffer, retry `Interrupted`, propagate `WouldBlock`,
  turn an inner `Ok(0)` into `FlushMode::Finish` so a truncated stream is an
  error rather than a silent short read, replace the previous
  `debug_assert` no-progress guard with a real `Err`, and implement the
  zlib "fewer than 6 unconsumed bytes at EOF after a complete member ⇒
  stop" rule in the adapter, not the core. `GzipStreamDecoder`,
  `ZlibStreamDecoder`, `RawInflateReader` (RFC 4978, `FlushMode::None`
  always), `async_deflate`, and `Inflater as Decompressor`
  (`FlushMode::Finish`) are all re-based on this core with every existing
  public item and guarantee preserved; `decompressed_size()` is
  re-documented as "produced so far", not a final total. **One push API
  serves both P2 and P3**: PNG's `IDAT` chain, an HTTP chunked/streamed
  body and a self-contained TIFF strip are the same "feed bytes, get bytes,
  ask again" shape, and `oxiarc-png` / `oxiarc-http` both consume
  `InflateStream`/`WrappedInflate` directly rather than each growing its
  own partial decoder. `Decompressor::decompress` keeps its documented
  whole-remaining-input (`FlushMode::Finish`) contract; there is no new
  `oxiarc-core::stream` module and no blanket `Decompressor` impl, by
  design — `AsyncDecompressorWrapper<Inflater>` is documented unsupported
  and points callers at `AsyncInflateReader` instead. Extensive new test
  coverage (byte-at-a-time feeding, 1-byte output buffers, proptest split
  points, truncation at every offset under bounded call-count budgets,
  multi-member gzip, every gzip header flag, zlib concatenation with 1-3
  byte accumulator residue, cap/ratio enforcement mid-block including a
  committed 812 KB→123 MiB single-block bomb generator, CAB-style
  reset/`set_dictionary` cycles) plus a new `tests/cross_crate_inflate.rs`
  proving a PNG `IDAT` chain split across chunks, an HTTP gzip body split
  across arbitrary reads, and a TIFF deflate strip all decode to
  byte-identical output through this one shared core.
- New **`oxiarc-zstd::stream::ZstdStream`** push decoder: `decode(&mut
  self, input, output, flush) -> Result<ZstdProgress>` (fields mirror
  `InflateProgress`), `finish()`, `with_max_output(u64)`,
  `with_max_window(usize)`, `with_multi_frame(bool)`,
  `with_dictionary(Vec<u8>)` — a real sliding-window ring rather than
  "the output `Vec` is the window", incremental XXH64, and a memory
  ceiling enforced *before* a block is decoded (a compressed block is
  conservatively charged 128 KiB then re-charged its real size, an
  RLE/raw block is charged from its header before it is materialised).
  New `decompress_into`/`decompress_with_limit`/
  `decompress_multi_frame_with_limit`; `ZstdStreamDecoder<R>` and the
  async adapter are re-based on `ZstdStream` so they are genuinely
  incremental rather than `read_to_end`-then-decode, with the output cap
  honoured pre-decode instead of after. Fixed in the same pass: stale
  literals-Huffman/FSE tables surviving `ZstdDecoder::reset` (a reused
  decoder could decode a `Treeless`/`Repeat` block at the start of a new
  frame against the *previous* frame's tables).
- New **`oxiarc-brotli::stream::BrotliStream`** push decoder: meta-block
  headers are parsed atomically with a bit-cursor rollback (a header that
  straddles a `decode()` call boundary is retried whole, never
  half-applied), the command loop is symbol-resumable, the window ring is
  real and lazily allocated (`with_max_window`), and a
  `decompress_reporting_shapes` API exposes `MetaBlockShape` for
  differential testing against the one-shot decoder.
  `BrotliDecompressor<R>` and its async adapter are re-based on it.
- **`oxiarc_lzw::decompress_tiff_into(src, dst) -> Result<usize>`** — a
  prefix/suffix table TIFF-LZW decoder with no per-code `Vec` allocation
  (the old-style `early_change = false` fallback is still available for
  pre-1993 files). `.xz` container framing (`XzReader`/`XzWriter`,
  `CheckType`, `compress`/`decompress`) moved from `oxiarc-archive` into
  `oxiarc_lzma::xz`, re-exported unchanged from `oxiarc-archive::xz` so no
  downstream import breaks.
- **`oxiarc-http`'s headers layer** (`ContentCoding` — an `Ord` from least-
  to most-preferred, `parse_content_encoding`; `QValue`, a thousandths-
  precision `u16`; `AcceptEncoding` builder,
  `to_header_value() -> Option<String>`; RFC 9110-conformant `negotiate`
  honouring `*` and `q=0`; `DecodeLimits { max_output, max_ratio, .. }`;
  server-side `encode_body`), the foundation the decoders below are built
  on. RFC 9110 example tables from the spec text are runnable tests, not
  paraphrased.
- **New crate `oxiarc-jpeg`** — a from-scratch Pure Rust JPEG (ITU-T T.81 /
  ISO/IEC 10918-1) decoder and encoder. Decoder: all four marker-defined
  processes (baseline, extended sequential, progressive with Annex G AC
  refinement/EOB runs, and lossless SOF3 in 2-16 bit precision), a 9-bit
  fast Huffman lookup, the exact-integer islow IDCT and libjpeg-exact fancy
  upsampling, fixed-point YCbCr/YCCK/CMYK colour conversion, 1-4 components
  at every sampling factor libjpeg supports, restart markers, `DNL`,
  APPn/COM passthrough (JFIF/EXIF/Adobe/ICC), a `TableSet`/`load_tables`/
  `decode_abbreviated_into`/`frame_header`/`decode_into_strided` surface
  built specifically for TIFF's `JPEGTables` (compression 7) to reuse
  without ever concatenating raw JPEG buffers, resource limits, and a
  no-panic corpus — verified byte-parity against `djpeg -dct int`.
  Encoder: baseline and 12-bit extended sequential (`SOF1`, dynamic
  quantisation-table precision), standard and optimized (libjpeg
  tie-breaking-exact) Huffman, quality-scaled quantisation tables
  byte-identical to libjpeg's, box downsampling at 4:4:4/4:2:2/4:2:0 with
  edge replication, restart intervals, JFIF/Adobe headers, gray/YCbCr/
  RGB/CMYK, progressive encoding with libjpeg's default scan script,
  `write_tables_only`/`encode_scan_only` (the other half of the TIFF
  `JPEGTables` mode) and lossless encoding — verified **byte-identical**
  to `cjpeg`, including progressive, optimized and 12-bit output. Also
  ships arithmetic coding (SOF9/10/11, `DAC`; feature `arithmetic`,
  **default-on**) verified byte-identical to libjpeg both directions, and
  OJPEG (old-style, `Compression = 6`) reconstruction helpers for TIFF's
  benefit. Hierarchical JPEG (SOF5/6/7/13/14/15) has no reference encoder
  to test against and returns a named `Unsupported` error rather than
  best-effort output, per the Phase 8 owner decision. `#![forbid(unsafe_code)]`.
- **`oxiarc-jpeg`: reduced- and enlarged-scale decoding** — `Scale`
  (numerator 1-16 over denominator 8, i.e. libjpeg's `-scale M/8`;
  `Scale::{FULL, ONE_HALF, ONE_QUARTER, ONE_EIGHTH}` constants),
  `DecodeOptions::scale` (default `Scale::FULL`, a byte-for-byte no-op;
  ignored for lossless frames, matching libjpeg), and `ImageInfo::{scaled_width,
  scaled_height}` (`ceil(dim * M / 8)`), which every decode entry point now
  sizes its output from. `M` in `{1, 2, 4, 8}` is byte-identical to `djpeg
  -dct int -scale M/8` (ported `jidctred.c` reduced-size IDCT kernels for
  1x1/2x2/4x4 plus the pre-existing full-size one); the other twelve values
  use one general kernel verified to a numeric tolerance (peak error 3,
  MSE 0.0531 over 2.1M samples, 216 configurations) against `djpeg`,
  including restart-marker streams and twelve-bit precision. `rayon`'s
  parallel band splitter honours the scaled geometry at every `M`.
- **`oxiarc-jpeg`: migration-aid compat facades**, always compiled (no
  Cargo feature) — `compat::zune` (a `zune_jpeg`-shaped `JpegDecoder`:
  `new`/`new_with_options`/`decode_headers`/`info`/`dimensions`/
  `output_buffer_size`/`input_colorspace`/`output_colorspace`/
  `set_options`/`icc_profile`/`exif`/`decode`, output colour space
  `Rgb`/`Rgba`/`Luma`/`YCbCr`) and `compat::jpeg_decoder` (a
  `jpeg-decoder`-shaped `Decoder<R: Read>`: `new`/`read_info`/`info`/
  `decode`/`icc_profile`/`exif_data`/`inner_mut`, matching the real
  crate's `ImageInfo`/`PixelFormat`/`CodingProcess` shapes), alongside the
  pre-existing `compat::JpegEncoder`. One deliberate deviation: real
  `jpeg_decoder::Decoder::info()` panics for a component count outside
  `{1, 3, 4}`; this crate's no-panic policy turns that into
  `Result<Option<ImageInfo>, JpegError>` instead.
- **New crate `oxiarc-png`** — a from-scratch Pure Rust PNG (ISO/IEC
  15948) decoder and encoder built on `oxiarc-deflate`. Decoder: a chunk
  reader with per-chunk CRC-32 (reusing `oxiarc-core`'s PNG-compatible
  CRC-32 table, not a second one), every colour type × bit depth, filter
  kernels specialised per bytes-per-pixel, Adam7 interlacing with its
  sub-byte edge cases handled correctly (1×1, width < 5, height == 1
  empty passes), `expected_raw` computed exactly once so allocation never
  guesses, incremental `IDAT` decode via `WrappedInflate(Zlib)`
  (`verify_checksum(false)` by default, matching the reference `png`
  crate's own leniency; strict mode available), both a pull `Decoder<R:
  Read>`/`Reader` (`read_info`, `next_row`, `next_frame`,
  `Transformations::{EXPAND, STRIP_16, ALPHA, ..}` matching `png` 0.18's
  output-colour-type table exactly) and a push `StreamingDecoder`, lenient
  defaults matching `png` 0.18 (ancillary-chunk CRC skip, out-of-range
  palette index treated as opaque black, `tRNS` truncation tolerated) with
  a strict mode, Apple `CgBI` PNGs (raw-deflate `IDAT`, BGR(A) swap,
  premultiplied-alpha flag exposed) decoded under the lenient default and
  named-error-rejected under strict, and untrusted-input limits. Encoder:
  filter strategies (None/Sub/Up/Avg/Paeth/adaptive MSAD/entropy),
  interlaced encoding, `Deflater` driven directly rather than through
  `ZlibStreamEncoder` (which sync-flushes every 128 KiB and would fragment
  every scanline run), `Encoder`/`Writer`/`StreamWriter`, every ancillary
  chunk (`PLTE`, `tRNS`, `gAMA`, `cHRM`, `sRGB`, `iCCP`, `cICP`, `mDCv`,
  `cLLi`, `sBIT`, `bKGD`, `hIST`, `pHYs`, `sPLT`, `tIME`, `tEXt`/`zTXt`/
  `iTXt` keyword rules, `eXIf`, `oFFs`/`sCAL`/`pCAL`, `sTER`, unknown-chunk
  retention), and full APNG (`acTL`/`fcTL`/`fdAT` sequence numbers,
  dispose/blend compositing to RGBA8/16). A `png`-0.18-shaped compat facade
  lives at the crate root plus a `v017` module for the older API shape.
  Optional `parallel` (per-band filtering; DEFLATE itself stays serial)
  and `async-io` features. `#![forbid(unsafe_code)]`.
- **New crate `oxiarc-tiff`** — a from-scratch Pure Rust TIFF 6.0 (+
  BigTIFF) decoder and encoder. Core: a byte-order layer for both
  endiannesses, classic and BigTIFF headers, IFD parsing across all 18 tag
  value types (inline and offset-indirected, `LONG8`/`SLONG8`/`IFD8`,
  count-overflow guards, lazy value loading, IFD-loop detection, `SubIFD`
  trees), a tag table spanning baseline, extension, GeoTIFF, EXIF, GPS,
  XMP, ICC, IPTC, Photoshop and DNG numbers, strip/tile geometry, sample
  unpacking at 1/2/4/8/12/16/24/32/64-bit depths and `FillOrder 2`,
  predictors 1/2/3 (correct file-order arithmetic under planar striding),
  and photometric conversions (`MinIsWhite`, palette, the YCbCr matrix,
  CMYK passthrough). Codecs: None, PackBits and CCITT RLE/G3-1D/G3-2D/G4
  (private modules — no independent second consumer exists for these two
  in the ecosystem, so they are not a separate crate; T4/T6 options,
  `FillOrder`, and now uncompressed mode, see below), Deflate/Adobe
  Deflate (8/32946, via `WrappedInflate(Zlib)` per strip with an explicit
  `TrailingPolicy`), LZW (5, via `oxiarc_lzw::decompress_tiff_into`),
  ZSTD (50000), LZMA (34925, via `oxiarc_lzma::xz`), JPEG (7, via
  `oxiarc-jpeg`'s `TableSet`/`decode_abbreviated_into` — a photometric ×
  APP14 × chroma-subsampling table, `RowsPerStrip` rounded to
  `8·Vmax` on write) and old-style JPEG (`Compression = 6`, OJPEG flavour
  (a) via libtiff-compatible reconstruction, flavours (b)/(c)
  constructively); WebP/JXL/LERC return a named `Unsupported`/
  `FeatureNotCompiled` rather than a silent stored fallback, and a
  `CodecRegistry` plugin trait lets a caller add its own. A full writer
  exists for every codec above (strips and tiles, multi-page, arbitrary
  tags including GeoTIFF passthrough, classic/BigTIFF chosen
  automatically, offsets patched after the fact). A `tiff`-0.11-shaped
  `compat` feature (`Decoder`/`DecodingResult` with the real crate's exact
  11/6 variant counts, `TiffEncoder`/`ImageEncoder`; `half` is a
  dependency only here), `rayon` (parallel strip/tile decode — see the
  pooling entry below) and `mmap` round it out.
  `#![forbid(unsafe_code)]`.
- **`oxiarc-brotli`: shared (custom LZ77) dictionary support, both
  directions.** `compress_with_dictionary`, `decompress_with_dictionary`,
  `decompress_with_dictionary_and_limit`, and `with_dictionary` on
  `BrotliStream` (surviving `reset()`), `BrotliDecompressor` and
  `BrotliAsyncDecompressor`. The distance space a dictionary opens up
  (`shared_dict`: ordinary output, then the dictionary, then the Appendix
  A static words, all relative to `max_backward = min(window, produced)`)
  was established by probes against `brotli 1.1.0 -D` rather than
  assumed. A shared dictionary is a *compound history block*: a copy may
  not run past its end, and one that would is rejected as corrupt — which
  is what the reference decoder does, re-derived every oracle run from a
  pair of streams differing in one copy length (this replaces an earlier
  "straddle" behaviour that turned out to be a fabrication — see Fixed,
  below). The encoder seeds its match finder with the dictionary and
  encodes every meta-block both with and without it, keeping the smaller,
  so attaching a dictionary can never cost ratio; `prefix_len == 0` stays
  byte-identical to the dictionary-free encoder. Verified against the
  reference CLI: **72/72** reference `-D` streams decode byte-identically
  and **96/96** of ours are accepted by `brotli -d -D` (70 of them smaller
  than the dictionary-free encoding); a 20 KiB slice of a 78 KB dictionary
  goes from 594 bytes to **27**.
- **`oxiarc-brotli`: `Content-Encoding: dcb` framing (RFC 9842).** New
  `dcb` module — `DCB_MAGIC` (`FF 44 43 42`), `DCB_HEADER_LEN` (36),
  `dictionary_id` (SHA-256, via `oxiarc_core::sha256`), `write_header`,
  `parse_header`, `verify_header`, `compress`, `decompress`,
  `decompress_with_limit` — re-exported at the crate root as
  `write_dcb_header`/`parse_dcb_header`/`verify_dcb_header`/`compress_dcb`/
  `decompress_dcb`/`decompress_dcb_with_limit`. A body from the wire is
  treated as untrusted: truncation at every offset, a flip of every header
  byte and 1,098 payload mutations are all covered, with both decoders
  required to agree on every one. This is what lets `oxiarc-http` stop
  refusing `dcb` (see below).
- **`oxiarc-lzw`: UNIX `compress(1)` / `.Z` container support**
  (`oxiarc_lzw::z`). `1F 9D` header with the block-mode flag and 9-16 bit
  code widths, LSB-first 8-code groups with the reference's exact
  group-alignment and table-reset semantics, KwKwK, and no
  end-of-information code (a truncated stream decodes to a *prefix*,
  exactly as `gzip -dc` does — the format has no way to signal "the body
  was cut short"; a transport-level check like `Content-Length` has to
  catch that). One-shot `decompress`/`decompress_with_limit`/
  `decompress_into`/`compress`/`compress_with_block_mode`, plus
  `ZReader<R: Read>` (a genuinely incremental reader with
  `with_max_output`, its compressed working set bounded to one 16 KiB
  chunk plus under `max_bits` bytes of carry plus the ~193 KiB code table
  — the decoded working set is *not* bounded unless `with_max_output` is
  set, since one 16 KiB chunk of 16-bit codes can legitimately expand to
  hundreds of MB) and `ZWriter<W: Write>` (batches of about 32 KiB;
  `finish()` must be called or the tail is lost). `compress(data, n)` is
  byte-identical to `compress -b n -c`, and `gzip -dc`/`uncompress -c`
  reproduce this crate's streams; gated by the new self-skipping
  `z-oracle` feature plus committed fixtures in
  `oxiarc-lzw/tests/data/z/`. This is what `Content-Encoding: compress`
  needs (see the `oxiarc-http` entry below).
- **`oxiarc-lzw`: explicit bit order.** `LzwConfig` gains a `bit_order`
  field (`LzwBitOrder::{Msb, Lsb}`) honoured by every generic entry point,
  plus `LzwConfig::with_bit_order` and `LzwConfig::TIFF_COMPAT_LSB` for
  libtiff's pre-1993 `LZWDecodeCompat` strips (late width change + LSB
  packing), which this crate previously could not decode at all — checked
  in both directions against `weezl` (an existing dev-dependency oracle;
  never a banned crate).
- **`oxiarc-lzw`: code widths up to 16 bits.** `LzwConfig::max_bits` now
  accepts 9-16 (`MAX_SUPPORTED_BITS`), with the dictionary counter, the
  width-growth rule and the encoder's reset trigger all computed in `u32`
  so the 65,536-entry exhausted state is representable (12 remains the
  TIFF/GIF default). `LzwStreamMode::Config(LzwConfig)` lets the streaming
  adapters use any bit order and code width; `Config(LzwConfig::TIFF)` is
  byte-identical to `LzwStreamMode::Tiff`.
- **`oxiarc-lzw`: GIF decode is now validated against a reference for the
  first time.** New self-skipping `gif-oracle` feature drives Pillow's own
  GIF decoder in both directions: 20 Pillow-written GIFs (interlaced and
  not, five payload shapes, sides 1-200) decode byte-identically, 20 GIFs
  built around `gif_compress` output read back byte-identically in
  Pillow, and every minimum code size 2-8 round-trips through it. The GIF
  codec had previously only ever been round-tripped against itself.
- **`oxiarc-lzma`: `xz::XzDecoder`** — a reusable decoder context (`new`,
  `with_max_output`, `decompress_into`, `reset`) that keeps its LZMA2
  dictionary buffer, probability model and coder state allocated across
  calls instead of rebuilding them per stream; targets TIFF
  `Compression = 34925`, where one image can be thousands of independent
  same-dictionary-size `.xz` strips. `xz::decompress_into` is a one-line
  wrapper over it; `xz::decompress_with_limit` is unchanged (a growable
  `Vec` from one call has no caller-held state to reuse in the first
  place). A guard (`block_opener_permits_decoder_reuse`) restores the
  "every independent XZ block must reset its dictionary" enforcement that
  blind decoder reuse would otherwise weaken; every already-accepted
  stream is still accepted. `XzWriter` gained multi-block output
  (`with_block_size`, default 64 MiB) with a per-block `CheckType`
  (`None`/`Crc32`/`Crc64`/`Sha256`).
- **`oxiarc-core`: new `sha256` module** (`Sha256::{new, update, finalize,
  compute}`, `hex32`) — dependency-free FIPS 180-4, moved here from
  `oxiarc-lzma`'s `.xz` reader/writer (same implementation, same bytes,
  independently re-verified against Python's `hashlib` including the
  56-byte and 112-byte NIST multi-block vectors and one million `'a'`
  bytes) so other crates — `oxiarc-brotli`'s `dcb` framing and
  `oxiarc-http`'s `dcz` preamble, both above — can share it without
  depending on `oxiarc-lzma`.
- **`oxiarc-http` decoders** (the `Decoder`/`DecodedBody` layer): a
  private `CodingDecoder` trait over `WrappedInflate` (gzip multi-member,
  `x-gzip`, and `deflate` sniffed `Auto` at offset 0 per RFC 9110
  §8.4.1.2, since some servers emit raw DEFLATE under that name),
  `BrotliStream`, and `ZstdStream` (+ `dcz`, RFC 9842's Zstandard
  dictionary variant, when a dictionary is supplied); chained codings
  (`Content-Encoding: gzip, br`) apply in reverse order; push
  `Decoder::feed_into`; a pull `DecodedBody<R: Read + BufRead>` and, under
  `async-io`, `AsyncDecodedBody`; `identity` and `compress` token
  recognition; `finish()` failing is documented as "the response failed",
  not a soft warning. Recipes for `ureq` 3 (default-features = false plus
  a manual `Accept-Encoding`), `reqwest` (`bytes_stream`), and `oxihttp`
  ship as runnable `examples/`. Every RFC example is a test, alongside
  per-coding round-trips against the matching oxiarc encoder and against
  python-produced fixtures, byte-at-a-time feeding, chunk invariance,
  `DecodeLimits` enforcement, and the negotiation table.
- **`oxiarc-http`: `Content-Encoding: compress`/`x-compress` (legacy UNIX
  `.Z`) is now fully functional, both directions**, behind the new
  `compress` Cargo feature. `decode/compress.rs`'s `CompressCodingDecoder`
  bridges `oxiarc_lzw::z::ZReader`'s pull `Read` shape onto this crate's
  push `CodingDecoder` seam through a small `Arc<Mutex<VecDeque<u8>>>`
  queue, whose read end reports `WouldBlock` rather than `Ok(0)` when
  starved so end-of-body is never confused with "no bytes yet". The
  bridge **meters how much compressed input it hands the inner reader**:
  `ZReader` decodes a whole pull to completion, into a buffer of its own,
  before serving the first byte of it, and at its native 16 KiB pull a
  body that expands 4992:1 would otherwise materialise ~128 MiB inside one
  `read` call. The bridge retunes its pull size after every completed
  fill from the worst per-fill expansion ratio measured so far, targeting
  a 64 KiB fill: streaming a 128 MiB, 4992:1 `.Z` body through
  `DecodedBody` with `DecodeLimits::unlimited()` and 4 KiB reads went from
  **134,605,718 bytes** of peak live allocation to **893,668** (an
  ordinary 32 MiB text body: 2,508,454 → 542,439), with no throughput
  cost — interleaved A/B (best-of-9) makes the metered path **~8 % faster**,
  since the smaller working set fits cache. `encode_body`/`Encoder<W>`
  gain a real streaming `Compress` arm (`oxiarc_lzw::z::ZWriter`); new
  `EncodeOptions::compress_max_bits` (default 16). `.Z` has no
  end-of-information code and no checksum, so a truncated/corrupted body
  decodes to a plausible, silently short prefix — exactly like
  `gzip -dc`/BSD `uncompress` — now a documented, tested exception on
  every one of this crate's cross-coding invariant tests. Gated by
  `tests/allocations.rs`'s new Gate 4.
- **`oxiarc-http`: `Content-Encoding: dcb` (RFC 9842, Brotli variant) is
  now fully functional, decode and encode**, now that `oxiarc-brotli` has
  shared-dictionary support (above) — the exact condition the original
  Phase 8 owner decision named. `decode/dcb.rs`'s `DcbCodingDecoder`
  buffers `oxiarc_brotli::dcb`'s 36-byte preamble (magic + the
  dictionary's SHA-256) across as many calls as it takes, verifies it
  against the caller's dictionary before a single Brotli byte decodes,
  then hands the rest to `BrotliStream::with_dictionary`; a wrong or
  absent dictionary is refused before any decoding, never a silent
  fallback to plain `br`. `encode_body` produces one via
  `oxiarc_brotli::compress_dcb`. The *streaming* `Encoder<W>` refuses
  `Dcb` by name (`UnsupportedReason::StreamingUnsupported`) —
  `oxiarc-brotli` has no dictionary-aware *streaming* encoder to wrap,
  only the one-shot `compress_with_dictionary`/`dcb::compress` — so
  `encode_body`'s one-shot path is the only way to produce a `dcb` body
  from this crate. `ContentCoding::{Compress, Dcb}::is_decodable`/
  `is_encodable` now track real, feature-gated capability instead of
  being unconditionally `false`.
- **New crate `oxiarc-image`** — a thin, `image`-0.25-crate-shaped facade
  over `oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`, for the ~10 of 17
  actionable ecosystem projects that depend on the `image` facade rather
  than on `png`/`tiff` directly. Magic-byte and extension format
  sniffing, `ImageReader`, `DynamicImage` (10 variants: `Luma8/16`,
  `LumaA8/16`, `Rgb8/16/32F`, `Rgba8/16/32F`), `ImageBuffer<P>` with
  `Pixel`/`Primitive` traits, and `codecs::{png, jpeg, tiff}::{Decoder,
  Encoder}` shaped to match `image` 0.25's own API, for projects
  migrating off `image` without adopting each codec crate's native API
  directly. TIFF's 32-bit-per-channel IEEE-float round trip
  (`ColorType::Rgb32F`/`Rgba32F`) is supported end to end, including
  straight/premultiplied-alpha handling at float precision, and
  `DynamicImage::from_decoder` mirrors `image` 0.25's own constructor.
  Deliberately **not** an image-processing library: no
  resize/blur/rotate/crop, no `imageops`, no `GenericImage(View)`, no
  animation — a migration aid for format I/O only, documented as such.
  `#![forbid(unsafe_code)]`.
- **`oxiarc detect`/`oxiarc info` now recognise PNG, JPEG and TIFF images
  by magic bytes** as a CLI-layer fallback when the input is not a
  recognised archive format (deliberately never added to `ArchiveFormat`
  itself, per the cost/benefit analysis in the Phase 8 design report —
  that would need five new "recognised, but not an archive" error paths
  across `list`/`extract`/`test`/`convert`/`add`, for a status `detect`/
  `info` can already report through a small standalone sniffing table
  instead): `detect` prints a brief summary, `info` prints dimensions,
  colour type, bit depth, compression and a chunk/segment/IFD summary.
  Every other subcommand now appends a short, specific hint to its
  existing "unsupported format" error ("this looks like a PNG, not an
  archive — try `oxiarc info`...") instead of a generic message, computed
  from a bounded-length read (at most 16 bytes) rather than buffering the
  whole input just to check a magic number. Man pages (11 × 2 directories)
  and shell completions (4 shells × 2 directories) regenerated to match.
- **15 new `cargo-fuzz` targets** under `fuzz/fuzz_targets/`:
  `fuzz_inflate_stream`, `fuzz_wrapped_inflate`, `fuzz_inflate_reader`,
  `fuzz_zstd_stream`, `fuzz_brotli_stream`, `fuzz_http_decode`,
  `fuzz_http_headers`, `fuzz_png_decode`, `fuzz_png_limits`,
  `fuzz_png_streaming`, `fuzz_jpeg_decode`, `fuzz_jpeg_tables`,
  `fuzz_tiff_read`, `fuzz_tiff_ifd`, `fuzz_image_open` — a superset of the
  12 the Phase 8 program named, plus a reusable
  `fuzz/support/counting_alloc.rs` peak-tracking global allocator for
  allocation-bound assertions. `fuzz_zstd_stream` differentially fuzzes
  `ZstdStream` against the legacy `decompress_multi_frame` and, after the
  `oxiarc-zstd` legacy-hardening fixes above landed, asserts genuine
  two-directional agreement (not just "if both accept, bytes match") with
  one precisely-scoped, evidence-backed carve-out for the permanent
  declared-window-ceiling split — 983,701 fuzz executions found none.
  `oxiarc-image`'s `tests/adversarial.rs` (truncation/corruption/short-read
  hardening across all three codecs) is the always-on substitute for a
  16th target, `fuzz_image_open`, which is recorded but not yet built.

### Changed

- **gzip `FHCRC` is now verified on every decode path, not only the new
  types.** `oxiarc-deflate`'s legacy one-shot `gzip_decompress` /
  `GzipDecoder` and `oxiarc-archive`'s `GzipReader` were both re-based on
  the new `WrappedInflate` core, whose `verify_header_crc` defaults to
  `true`. A gzip member that sets `FLG.FHCRC` and carries a corrupt CRC-16
  over its own header — silently accepted by 0.4.1, which skipped the field
  — is now rejected with a checksum-mismatch error, exactly as `gzip -d`
  rejects it. Streams without the flag (the overwhelming majority, including
  everything `gzip`, `pigz` and oxiarc itself write by default) are
  unaffected. Opt out with `WrappedInflate::verify_header_crc(false)` or
  `InflateReader::verify_header_crc(false)`.
- **`OxiArcError::CrcMismatch`'s message now reads "checksum mismatch"
  rather than "CRC mismatch".** The variant has always carried *every*
  whole-stream checksum this workspace verifies, including zlib's
  **Adler-32** trailer (RFC 1950 §8.2) — the one PNG `IDAT` chains, TIFF
  Deflate strips and HTTP `Content-Encoding: deflate` bodies all decode
  through — so a corrupt Adler-32 used to be reported as a "CRC mismatch",
  sending anyone debugging it to look for a field that is not there. The
  variant, its `expected`/`computed` fields and every raise site are
  unchanged (renaming or splitting the variant would be a breaking change on
  a published crate); only the `Display` text changed, and each raise site
  now names its own algorithm in its rustdoc.
- **`oxiarc-deflate`: the inflate fast loop rewritten around packed decode
  tables — decode throughput up across every data shape, with no change to
  any decoder guarantee.** The symbol loop used to decode through
  `HuffmanTree` and then look the symbol's meaning up in four more tables
  (`LENGTH_EXTRA_BITS`, `LENGTH_BASE`, `DISTANCE_EXTRA_BITS`,
  `DISTANCE_BASE`) behind a `< 256 / == 256 / > 285` comparison chain. A new
  crate-private `decode_table::DecodeTable` folds all of that into the table
  entry: one `u32` carries the symbol *kind* (literal / end-of-block /
  sub-table / invalid), the code length, the extra-bit count and the payload
  (literal byte, length base, distance base, sub-table offset), so a literal
  is one masked load and a match needs no side tables at all. The table is
  built by **zlib's one-pass `inflate_table` algorithm** — symbols counting
  sorted into canonical order, the reversed code maintained by a backwards
  increment — instead of reversing every symbol's code with a per-bit loop
  twice per block. Around it: one bulk refill per iteration (>= 56 bits,
  checked once as a single invariant, so no inner step re-checks
  availability), up to three literals decoded from one 32-bit peek with a
  single `consume`, the bit accumulator and both cursors in locals, the loop
  `#[inline(never)]` so it does not share a register allocation with the
  resumable path (which was spilling the input cursor to the stack on every
  refill), and match copies in machine words — 8-byte chunks inline for the
  short non-overlapping case, a byte fill for distance 1, word tiling with
  pattern doubling for distances 2-7, and `memmove` only above 64 bytes
  where its vector width wins. The growable entry points (`inflate`,
  `InflateStream::inflate_to_vec`) now decode into the tail of the buffer
  they are filling, so the window is written once and back-references
  resolve in place, and the first buffer is sized from the input instead of
  always starting at 64 KiB.
  **Nothing observable changed**: the same bytes come out (the differential
  suite, the CPython `zlib` oracle and the PNG/TIFF/HTTP/archive suites all
  pass unchanged), the sticky-fault, `NeedInput`/`NeedOutput`, `FlushMode`
  and no-silent-truncation semantics are untouched, no byte outside the
  output a call reports is ever written, and `HuffmanTree` keeps its public
  API (it is still the encoder's, and still the bit-at-a-time fallback's).
  Resident memory *fell*: the two per-tree root-sized scratch buffers are
  gone, and `oxiarc-http`'s streaming allocation peak went from ~216 KiB to
  178 KiB against its 224 KiB budget. Measured with
  `cargo run --release --example inflate_ab` (new: an interleaved A/B of all
  four decode entry points against `zlib.decompress` over six data shapes x
  three sizes x levels 1/6/9, every arm's output verified before it is
  timed) and `benches/deflate_bench.rs::inflate_shapes` (new).
  Measured by running the pre-change binary and the current one alternately
  on the same corpora (so neither gets a quieter machine), 1 MiB payloads,
  MB/s of output through `inflate_into` — with the worst of the five decode
  arms over CPython's `zlib.decompress` in the same rounds. Every figure is
  a median of medians (the median across three interleaved rounds of each
  round's own median); ranges span levels 1/6/9, and the last column is the
  same ratio computed from each round's best iteration instead, which on a
  machine at load ~45 is the load-robust figure:

  | Shape | before -> after | worst arm vs python | best-of-round |
  |---|---:|---:|---:|
  | PNG-filtered scanlines | 1008-1032 -> 1649-1701 (1.6x) | 0.52-0.61x -> 0.86-0.87x | 0.77-0.83x |
  | text (HTML-like) | 576-989 -> 910-1625 (1.6x) | 0.41-0.54x -> 0.65-0.78x | 0.63-0.74x |
  | a full flush every 8 KiB | 478-572 -> 696-868 (1.5x) | 0.45-0.48x -> 0.66-0.72x | 0.64-0.70x |
  | long-match JSON | 2964-3298 -> 3773-3812 (1.1-1.3x) | 0.96-0.97x -> 1.10-1.13x | 1.01-1.02x |
  | RGB8 image rows | 278-281 -> 307-310 (1.1x) | 0.56-0.60x -> 0.65-0.67x | 0.65-0.66x |
  | incompressible (stored) | 30848-32832 -> 31048-34090 | 2.54-2.80x -> 2.75-2.92x | 2.38-2.49x |

  The reference is CPython 3.14 linking `/usr/lib/libz.1.dylib` 1.2.12 —
  Apple's *tuned* system zlib, not stock zlib. Stated exactly against the
  ">= 0.60x on every shape" requirement, on medians: **1 MiB is met on every
  shape and level** (0.65x-2.92x); **64 KiB is met on four of six** (RGB8
  image rows 0.46x at level 6 and the full-flush shape 0.59x at level 9 are
  short on medians, 0.60x and 0.71x on best-of-round, where every 64 KiB
  shape then clears the line); **16 MiB is met on three of six**
  (PNG-filtered 0.51x-0.60x, RGB8 0.58x-0.67x, text 0.58x-0.77x, five of
  six >= 0.61x on best-of-round). Re-measured 2026-09-08 across three more
  full 54-row passes at load 60-71: PNG-filtered at 16 MiB lands at
  0.47x-0.67x on medians and 0.48x-0.57x on best-of-round — the same band and
  the same verdict, marginally lower at the top because those passes ran at a
  higher load than the originals'. None of that is a regression: on the four
  decode-bound shapes the python ratio improves in all 36 measured rows and
  `inflate_into` is 1.04x-3.3x faster, while the two `memcpy`-bound shapes
  (stored blocks, long-match JSON) scatter in both directions within noise.
  64 KiB decodes are 20-40 microsecond measurements on a loaded shared
  machine, and the 16 MiB PNG corpus is only 2.6x compressible (against 7.9x
  at 1 MiB), i.e. a second literal-heavy shape at that size. The full matrix, the method and the
  changes that did *not* pay off are in `oxiarc-deflate/README.md` and
  `oxiarc-deflate/TODO.md`.

- **`oxiarc-zstd`: decode throughput rebuilt around the reference decoder's
  data layout.** The speed-up is strongly shape-dependent — it is large exactly
  where the decoder was doing per-byte work, and small where it was already
  bound by `memcpy` (see the table below). The motivating
  measurement came from the TIFF track: on a 4096x4096 RGB8 page in 16-row
  strips, our ZSTD strip decode delivered ~110 MB/s where `libzstd` inside
  `tiffcp` was an order of magnitude ahead, the largest gap in the whole codec
  matrix. **That gap is closed**: re-measured 2026-09-08 on the same geometry
  (4096x4096, 16-row strips, medians of 15 interleaved rounds, three runs at
  load 110 down to 20), the whole-image ZSTD ratio against `tiffcp -c none` is
  **0.96x on Gray16 and 1.65x on RGB8**, from 5.07x and 6.24x — the Gray16 row
  now meets the crate's `<= 1.25x` gate outright, and ZSTD is no longer the
  outlier of that matrix. Profiling found the cost was almost never in the entropy decoding
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

  Before/after, `decompress_into`, MB/s of output, best of two 7-round runs of
  each build **alternated back to back** on the same machine at load averages
  27-37 on 8 cores (the two builds' rounds are therefore comparable to each
  other; the absolute numbers are not comparable to an idle machine):

  | shape (level) | before | after | |
  |---|---|---|---|
  | RGB8 TIFF strip 288 KiB (1) | 127.9 | 926.4 | **7.2x** |
  | RGB8 TIFF strip 288 KiB (3) | 227.3 | 921.2 | **4.1x** |
  | RGB8 TIFF strip 288 KiB (9) | 235.8 | 975.3 | **4.1x** |
  | RGB8 TIFF strip 288 KiB (19) | 94.5 | 193.4 | 2.0x |
  | RGB8 TIFF strip 1 MiB (1) | 139.5 | 944.9 | **6.8x** |
  | RGB8 TIFF strip 1 MiB (3) | 126.1 | 561.3 | **4.5x** |
  | RGB8 TIFF strip 1 MiB (9) | 137.4 | 624.4 | **4.5x** |
  | RGB8 TIFF strip 1 MiB (19) | 87.4 | 171.0 | 2.0x |
  | text corpus 50 MB (3) | 315.8 | 701.8 | 2.2x |
  | text corpus 50 MB (19) | 598.5 | 1235.5 | 2.1x |
  | incompressible 8 MiB (3) | 8552.2 | 9507.3 | 1.1x |
  | highly repetitive 8 MiB (3) | 1238.7 | 10220.2 | **8.3x** |

  The two shapes that barely move are the ones that were already bound by
  `memcpy` rather than by per-byte work, which is the point: nothing was slow
  there to begin with. Against the reference decoder the same frames now run at
  0.5x-0.75x of `zstd -b -d` on every shape except the 50 MB text corpus at
  level 3 (0.43x-0.58x depending on the round; see `oxiarc-zstd/README.md`),
  and 3.6x on highly repetitive data. `decompress_into` also gained a
  caller-buffer-sized first window allocation (`ZstdStream::with_window_hint`,
  crate-internal) so a one-shot decode into a known-size buffer no longer walks
  the ring's doubling sequence — memory the caller has *already allocated* is
  the one size that may drive an allocation, and the lazy growth that protects
  against a declared `Window_Size` is untouched.

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
- **`oxiarc-deflate`: the DEFLATE *encoder* rewritten as a faithful port of
  zlib's `deflate.c`/`trees.c`.** `Deflater` output is now **byte-identical
  to CPython's `zlib.compress(data, level)` at every level 1-9** (source
  text, HTML, log lines, binary records, runs, random data, PNG-filtered
  scanlines — 90 of 90 corpus×level pairs). Previously output was up to
  **179 % larger** than zlib's on the same bytes: `find_match` never
  terminated an empty hash-chain bucket correctly (burning the whole
  match-search budget on nothing), levels 1-4 could only ever emit
  fixed-Huffman blocks, the lazy-matching rule diverged from zlib's, there
  was no `TOO_FAR` distance-cost rule, and — the most consequential bug —
  every `deflate()` call emitted its own block and its own Huffman tree
  instead of tracking window/hash-chain/tree state across calls, so a 2 MiB
  stream fed as 1 KiB calls cost **13x more** than one 1 MiB call and
  produced *different, larger* output. All of that is gone: levels now
  select a row of zlib's `configuration_table` (greedy `deflate_fast` at
  1-3, lazy `deflate_slow` at 4-9 including `TOO_FAR` and `good_length`
  chain quartering), a block's type (stored/fixed/dynamic) is chosen on
  its real bit cost with the tree description included, blocks are cut at
  16,383 symbols as zlib does, and the call-size no longer changes the
  output or the cost — the same 2 MiB stream now costs 34.4 ms at 1 KiB
  calls against 33.0 ms for one 1 MiB call, byte-identical either way.
  Level-6 throughput is **0.77x-1.53x of CPython zlib** (was 0.10x-0.40x).
  New `Deflater::with_strategy(Strategy)` exposes zlib's
  `Z_DEFAULT_STRATEGY`/`Z_FILTERED`/`Z_HUFFMAN_ONLY`/`Z_RLE`/`Z_FIXED`
  (`Filtered` suits predictor output such as PNG scanlines and TIFF
  horizontal differencing; `Rle` restricts matching to distance 1); all
  five are now byte-identical to CPython `zlib.compressobj(level,
  DEFLATED, -15, 8, strategy)` — `Z_FIXED` against zlib >= 1.2.13, whose
  block-type rule it follows (see the `Strategy::Fixed` entry under
  Fixed). `Deflater::with_optimal_parsing(level)`
  (graph-based DP parsing) is now **never larger than the default ladder
  at the same level** (it used to be up to 2 % *larger* on noisy image
  rows, because its candidate set bought rare long-distance codes for
  3-byte matches without the `TOO_FAR` filter) and is now **call-size
  invariant** with no per-call cost cliff (it used to be up to 25x slower
  and 9.5 % larger fed one byte at a time than fed as one call — a DP span
  used to start as soon as 262 bytes were buffered, so a byte-at-a-time
  caller re-ran a 259-position candidate collection to emit one byte).
  Levels 1-9 stay exact; level 0's all-stored output depends on the same
  caller-buffering behaviour zlib's own `deflate_stored` does, and is
  documented as never larger than CPython's rather than byte-identical to
  it. New `tests/zlib_encoder_oracle.rs` (byte-identity at every level and
  strategy, behind the self-skipping `zlib-oracle` feature) and
  `tests/encoder_behaviour.rs`/`encoder_adversarial.rs` (hermetic:
  call-size invariance, cross-call matching, block-type selection through
  an independent block walker that is deliberately not built on this
  crate's own inflater); new `examples/zlib_ab.rs` prints the ratio,
  optimal-parser, throughput and per-call tables without `criterion`. A
  steady-state `Decoder::feed_into` loop over a gzip body used to allocate
  79 times in 64 calls (two fresh scratch `Vec`s per Huffman-table
  rebuild, despite the method's own doc claiming reuse) and a 32 KiB
  inflate history buffer transiently overshot to ~125 KB before
  truncating on every large decode — both are fixed (decode throughput
  unchanged, ±2%), so the encoder becoming spec-correct did not regress
  `oxiarc-http`'s allocation-bound gate.
- **`oxiarc-lzw`: the TIFF/GIF LZW decoder is 1.25x-2.6x faster and now
  runs at 0.73-0.80x of the *throughput* of libtiff 4.7.1's own
  `LZWDecode`** (i.e. 1.25x-1.37x of its decode *time* — stated as both
  numbers because the bare ratio reads ambiguously as either; re-measured
  2026-09-08 over three runs of seven interleaved rounds at load 56-66, the
  band widens to 1.00x-1.56x of libtiff's decode time — a noisier measurement,
  not a slower decoder, since rows that agreed to +-0.01x on a quiet machine
  scattered by +-0.3x at that load, so the figure above remains the better
  estimate of the code's cost). Measured on
  4096x4096 TIFF pages written by `tiffcp -c lzw`, strips of 60 KiB to
  1 MiB, three arms (pre-rewrite / now / libtiff) interleaved per round:
  RGB8 rows 233→156 ms, 16-bit grayscale 103→69 ms, text 38→30 ms,
  incompressible data 107→41 ms. Rebuilt to libtiff's `LZWDecode` shape:
  the code table is one packed `u64` per entry (prefix, length, first
  byte, last byte, an all-bytes-equal bit) so a code costs one table load
  and one table store where a five-field struct cost four loads and five
  stores; codes are shifted out of a four-byte window at a bit position
  the loop keeps in a register instead of a stateful reader written back
  every code; a new table entry is created *before* the current code is
  emitted, turning KwKwK into one comparison; all-equal runs are emitted
  with a fill instead of a chain walk; and the chain walk stops one step
  above the root, so a three-byte string costs one table load and a
  two-byte string none. `gif_decompress` shares the same loop now instead
  of cloning a `Vec<u8>` per emitted code (5.8x-24x faster on 1 MiB
  payloads). Output is byte-identical for every dialect (`TIFF`,
  `TIFF_OLD_STYLE`, `TIFF_COMPAT_LSB`, `GIF`) and both bit orders, checked
  against libtiff, Pillow, `compress(1)`, `uncompress` and `gzip -dc`
  (668,091 differential comparisons against the pre-rewrite decoder, zero
  mismatches). New `examples/lzw_vs_libtiff.rs` reproduces the `tiffcp`
  half of the comparison with only PATH tools (self-skips without
  `tiffcp`; needs a quiet machine — above roughly load 20 a *single*
  round's ratio can swing 0.91x-1.79x, though the median/min-of-many-round
  estimators this crate's own gate uses stay accurate through load 46 in
  testing).
- **Breaking (within this still-unreleased version): `LzwConfig::GIF` is
  now genuinely LSB-first.** Before this cycle `LzwConfig` had no
  bit-order field, so `LzwConfig::GIF` and `LzwConfig::TIFF_OLD_STYLE`
  were literally the same value and any caller passing `LzwConfig::GIF`
  into the generic decode/encode entry points silently got MSB-first
  behaviour. Callers who used `LzwConfig::GIF` that way must switch to
  `LzwConfig::TIFF_OLD_STYLE`. The dedicated `gif_compress`/
  `gif_decompress` codec functions were always correct and are
  unaffected; no published release ever shipped the old, wrong value.
- **Breaking: `LzwConfig` gained a field.** The new `bit_order:
  LzwBitOrder` field (see Added, above) means any external struct-literal
  construction of `LzwConfig` — `oxiarc-lzw` has been a published crate
  since before this cycle — no longer compiles without setting it.
  `LzwConfig::new` and the existing named constants (`GIF`, `TIFF`,
  `TIFF_OLD_STYLE`, and the new `TIFF_COMPAT_LSB`) are unaffected; only a
  direct `LzwConfig { .. }` literal is.
- **`oxiarc-zstd`: the crate-internal decoded-sequence record narrowed
  from three `usize` fields to three `u32` (24 → 12 bytes)**, which the
  RFC 8878 format's own bounds allow (literal length ≤ 131071, match
  length ≤ 131074, offset ≤ 2^32 - 4) and which is not a public API change
  (`mod sequences;` is private). The worst-case reservation an attacker
  can buy with a crafted `Number_of_Sequences` halves, from ~2.35 MB to
  ~1.2 MB. Landed alongside a decoder rewrite that lifts the sequence loop
  out of `&mut self` so the three FSE tables and the repeat-offset stack
  are plain locals copied back on every exit, success and error alike.
- **`oxiarc-image`: `DynamicImage::to_luma8`/`to_luma16` (and the two
  `to_luma_alpha*` built on them) now use `image` 0.25's own sRGB/Rec. 709
  luma weights** (`(2126 R + 7152 G + 722 B) / 10000`) instead of BT.601's
  `0.299/0.587/0.114` — pure red now grayscales to 54, not 76,
  byte-identical to `image` for the four 8-bit `DynamicImage` variants,
  which is the entire point of a drop-in facade. No JPEG output byte is
  affected: `write_to(.., ImageFormat::Jpeg)` routes only grayscale
  sources through `to_luma8`, and both coefficient sets sum to exactly
  their divisor, so `luma(l, l, l) == l` under either.
- **`oxiarc-image`: the `DynamicImage` colour conversions were rewritten**
  from per-pixel `get_pixel`/`put_pixel` with `f64` scaling to per-variant
  slice loops with exact integer scaling (`u8 -> u16` is `* 257`; `u16 ->
  u8` is `(v * 255 + 32767) / 65535`): **5.8x faster** `Rgb8 -> to_rgba8`,
  **4.3x** `Luma8 -> to_rgba8`, **3.0x** `Rgb16 -> to_rgba8` (1024×1024,
  interleaved A/B, best of 25). Output is byte-identical apart from the
  luma-weight change above, proven by a new golden table
  (`tests/conversions.rs`) pinning every `to_*`/`into_*` method for all
  ten `DynamicImage` variants, and by exhaustive equivalence tests over
  all 256 `u8` and all 65,536 `u16` sample values.
- **`oxiarc-image`: `ImageDecoder::read_image` now returns
  `ImageError::Parameter` when `buf.len() != total_bytes()`**, in both
  directions and for all three codecs. Its doc previously promised a
  panic — matching `image::ImageDecoder`'s own contract — that no
  implementation actually performed: PNG silently returned `Ok(())` from
  an over-sized buffer, leaving the tail unwritten with no error at all.
  Documented as a deliberate deviation from `image` (an error, not a
  panic) rather than a promise nothing kept.

### Security

- **`oxiarc-cli`: `--memory-limit` accepted an SI byte-size string whose
  multiply overflows `u64` (e.g. `18446744073709551g`) — a debug-build
  panic, or, in release, a silent wraparound to a far-too-small limit.**
  This flag is the CLI's advertised decompression-bomb defense (see
  v0.3.6's "CLI" paragraph), so a limit that silently becomes tiny (or a
  crash on an untrusted invocation) is a defense-in-depth regression, not
  a cosmetic parsing bug. Fixed with `checked_mul` and a named "byte size
  out of range" error. Pre-existing since the flag was introduced (v0.2.8);
  found by the Wave 3 fuzz/CLI-hardening pass, not by fuzzing the flag
  itself — `oxiarc-cli/src/utils.rs::parse_byte_size`'s own unit tests
  now cover it directly.
- **`oxiarc-zstd`: a Zstandard frame using offset code 31 (RFC 8878's
  maximum, reachable through an RLE or custom offset FSE table) computed
  `Offset_Value = (1 << 31) + readBits(31)` — up to 2^32 - 1 — in `usize`.**
  On a 32-bit target (wasm32, armv7, i686) that overflows: a panic in a
  debug build, a silently wrong decoded offset in a release one. 64-bit
  hosts were never affected, which is why this went unnoticed since the
  crate's original sequence decoder. The intermediate is now computed in
  `u64`; the result, at most 2^32 - 4, always fits.
- *(See also, under Fixed below: `oxiarc-zstd`'s legacy one-shot decoders —
  `decompress`, `decompress_multi_frame`, `ZstdDecoder::decode_frame`,
  present and public since well before this release — accepted several
  classes of malformed/adversarial Zstandard frame that the streaming
  `ZstdStream` decoder already refused (an unvalidated `Dictionary_ID`,
  an unenforced per-block decompressed-size maximum, tolerated leading
  garbage, a refused-instead-of-skipped skippable-frame prefix) and could
  carry a failed frame's partially-regenerated output into the next
  `decode_frame` call on a reused decoder. All are fixed in shared code so
  the two decode paths cannot drift apart again; see the `oxiarc-zstd`
  entries below for the full detail and the differential-fuzzing
  provenance.)*

### Fixed

- **`oxiarc-archive`/`oxiarc-cli`: a valid multi-member `.gz` failed with
  "CRC mismatch" — `oxiarc extract`, `test`, `list`, `info` and `convert`
  could not read any concatenated gzip file.** `GzipReader::decompress` read
  the whole input into memory, treated its **last 8 bytes** as *the* member
  trailer, and inflated everything before them as one member. Every RFC 1952
  §2.2 concatenated file — `cat a.gz b.gz`, `pigz`, `bgzip`, rsyncable
  gzips, and oxiarc's own `compress_gzip_parallel` output — was therefore
  rejected outright (a hard error and exit 1, not a first-member result),
  even though `oxiarc_deflate::gzip_decompress`, `GzipStreamDecoder` and
  `InflateReader::gzip` all decoded the same bytes correctly. `GzipReader`
  is now built on the shared resumable core
  (`WrappedInflate(InflateWrapper::Gzip).multi_member(true)
  .trailing_policy(AllowZeros)`) and streams the input in 64 KiB buffers
  instead of buffering the whole compressed file, so: every member decodes
  and their contents concatenate; trailing `0x00` padding is tolerated while
  other trailing garbage is still rejected; each member's CRC-32, `ISIZE`
  **and `FHCRC`** are verified (see **Changed**); and the `ISIZE` mismatch
  message is now the core's `gzip ISIZE mismatch: stored N, decoded M`.
  `GzipReader::header()` still reports the *first* member's header.
- **`oxiarc-cli`: `--memory-limit` could be evaded by a multi-member gzip
  bomb.** The gzip path pre-checked only the trailing `ISIZE` field, which
  for a concatenated stream describes just the *last* member, and otherwise
  relied on a post-decode backstop — so a bomb split across members was
  fully expanded in memory before being refused. The new
  `GzipReader::with_max_output(u64)` is now wired to `--memory-limit`, so
  the cap is enforced **inside a DEFLATE block, across the running total of
  every member**, and the bomb is never materialised.
- **`oxiarc-brotli`: `BrotliStream` could stall for ever on a complete stream,
  and report it as truncated.** The geometric retry schedule that keeps atomic
  meta-block prelude re-parsing linear waits for the buffered input to grow by
  at least 64 bytes before re-attempting a prelude — but it measured that
  growth with a counter relative to the internal carry, which is compacted and
  rebased between calls and can hold a whole byte inside the bit accumulator
  instead. Two consequences: a stream whose prelude parse ran short with fewer
  than 64 bytes still to come never got its retry, and a byte that *did* arrive
  could be gated and then look like "nothing new" for ever. A caller feeding
  with `FlushMode::None` and calling `finish()` at the end therefore saw
  `NeedInput` for ever and `finish()` reported a **complete** stream as
  `UnexpectedEof`, while the identical bytes fed in one call decoded fine. The
  schedule now measures arrival against the monotone `total_in`, and a
  `decode(&[], ..)` drain call — the caller saying "this is all I have right
  now" — retries immediately instead of waiting for input that may never come.
  Every shipping adapter (`BrotliDecompressor`, the async twin,
  `oxiarc-http`'s `br` stage) switches to `FlushMode::Finish` at source EOF and
  so was never affected; a caller driving `BrotliStream` directly was. Found by
  `fuzz_brotli_stream` once its window and output caps made it ~16x faster
  (448 → 7,000+ exec/s); both libFuzzer fixtures are pinned as unit tests, and
  a 1,513,723-execution campaign on the fixed decoder found nothing further.
- **`oxiarc-http`: trailing garbage after a body could be accepted, depending
  on the read granularity.** `DecodedBody` and `AsyncDecodedBody` closed the
  body the moment the coded stream reported `StreamEnd`, without first
  establishing that the *source* was spent. The four codings differ in whether
  they need a further byte to report `StreamEnd` at all, so `XXXX` appended to
  a `br` body was rejected when the stream and the garbage landed in one read
  and silently **accepted** when the body's last byte arrived on its own —
  `read_to_end` returning `Ok` on a response with 4 unexplained bytes after it.
  Both adapters now keep the body open until the source is exhausted whenever
  the trailing policy inspects what follows (the default `Reject`, and
  `AllowZeros`); the finished decoder applies `check_trailing` to whatever
  arrives. `TrailingData::Ignore` still closes at once and never reads a byte
  the caller did not ask for. gzip, deflate and zstd were unaffected in
  practice — their decoders already need the extra byte — but the guarantee is
  now structural rather than incidental, and is pinned at one-byte granularity
  for every coding in both adapters.
- **`oxiarc-lzma`: `XzDecoder::with_max_output` reported a tripped budget as
  a self-contradictory `BufferTooSmall { needed: n, available: n }`.** The
  effective cap is `min(configured, dst.len())`, but every
  `MemoryBudgetExceeded` from the inner reader was mapped to
  `BufferTooSmall` against `dst.len()` — so `with_max_output(1000)` into a
  100,000-byte `dst` produced `Buffer too small: need 100000 bytes, have
  100000`, and a caller could not tell "your buffer is short" from "your
  configured limit tripped". The mapping now applies only when `dst` is the
  binding cap; a *tighter* configured cap passes `MemoryBudgetExceeded`
  through unchanged. `oxiarc-tiff`, the only in-workspace consumer, never
  sets `with_max_output` and is unaffected.
- **MSRV: a multi-line let-chain in `oxiarc-zstd` broke the declared
  `rust-version = "1.85"`.** `oxiarc-zstd/src/compressed_block.rs`'s
  `choose_mode` wrote `if let Some((symbol, &count)) = first` on one line and
  `&& count == total` on the next — a let-chain, which needs rustc 1.88. It
  compiled without complaint on the development toolchain (rustc 1.95) and
  `cargo +1.85.0 check --workspace` failed on it with
  ``error[E0658]: `let` expressions in this position are unstable``. Because the
  chain spans two lines, the single-line `rg 'if let .*=.*&& |&& let '` scan used
  throughout the cycle returned empty on the file. De-sugared to nested `if`s
  (identical semantics — `clippy.toml` pins `msrv = "1.85"`, so `collapsible_if`
  will not suggest the chain back), and the whole workspace now passes
  `cargo +1.85.0 check --workspace`.
- **Twelve rustdoc errors under `RUSTDOCFLAGS="-D warnings"`, in four crates.**
  `cargo doc --workspace --no-deps --all-features` is now exit 0 for all 18
  crates. Doc text only — no API, signature or behaviour change.
  - `oxiarc-lzma`: `xz/mod.rs`'s module docs linked to `decompress_into`,
    `decompress_with_limit` and `XzDecoder` unqualified. Because `lib.rs` puts an
    outer `///` comment on `pub mod xz;` *and* the module carries `//!` inner
    docs, rustdoc resolves the merged fragments in the crate root's scope: those
    three did not resolve at all, and `[`decompress`]` silently pointed at the
    unrelated crate-root `oxiarc_lzma::decompress` (the LZMA one-shot, not the XZ
    one). Every link is now written as `crate::xz::…`. Also fixed:
    `[module documentation](self)` in `xz/decoder.rs` (the private `xz::decoder`
    module) and a link to the private `DEFAULT_BLOCK_SIZE` in `xz/writer.rs`.
  - `oxiarc-jpeg`: `compat/zune.rs` linked to `JpegError` and
    `JpegError::Unsupported`, which the file imports under `#[cfg(test)]` and are
    therefore out of scope in a doc build (now `crate::JpegError…`); `tiff/mod.rs`
    linked to the private `ojpeg` module (now points at its public re-exports).
  - `oxiarc-lzw`: `z::ZReader::into_inner` linked to the private `READ_CHUNK`.
  - `oxiarc-deflate`: the `deflate` module docs linked to the private
    `crate::encoder`.

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
  crafted frames over every `Window_Descriptor` byte and 5501+ truncated,
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
- **`oxiarc-deflate`: `Strategy::Fixed` (zlib's `Z_FIXED`) follows zlib
  1.2.13's block-type rule.** A block is stored whenever a stored block
  beats the *static* code (`stored + 4 <= static`), as in zlib >= 1.2.13,
  whose `_tr_flush_block` narrows `opt_lenb` to the static cost under
  `Z_FIXED` before the stored test. zlib <= 1.2.12 — including macOS's
  system zlib 1.2.12, which CPython links there — applies `Z_FIXED` only
  after that test, still weighing the dynamic cost, and so writes a fixed
  block wherever `dynamic < stored + 4 <= static`. Within this unreleased
  cycle the rule briefly followed 1.2.12, because the CPython oracle it
  was first checked against linked 1.2.12. It is now pinned hermetically
  (`tests/encoder_behaviour.rs`), and the oracle observes which rule its
  reference applies: against zlib >= 1.2.13 all five strategies are
  byte-identical to CPython over ten corpora at four levels; against an
  older zlib the `Z_FIXED` comparisons that differ are held block by block
  to exactly that rule change instead. `with_strategy` was new API in this
  same unreleased cycle with zero committed coverage, which is why the
  first version shipped untested.
- **`oxiarc-deflate`: two defects in the new resumable inflate core
  (introduced and fixed within this same unreleased cycle).** A match
  straddling the 32 KiB history window's boundary could, when its tail
  landed within 8 bytes of a small (≤ ~32 KiB) caller-supplied output
  buffer, silently drop the last 1-7 bytes of the match in a release
  build (a debug build hit a `debug_assert` instead) — reachable through
  the public `InflateStream::inflate` via any of `oxiarc-png`,
  `oxiarc-tiff` or `oxiarc-http` decoding into a buffer that size, though
  no PNG/TIFF/HTTP test or CPython oracle ever happened to hit the narrow
  `(cursor, distance, length)` alignment needed; `inflate()`/
  `inflate_to_vec` were unaffected (their buffer is never below 64 KiB).
  Separately, a dynamic Huffman block whose alphabet's *shortest* code is
  longer than the decode table's root index (e.g. `HDIST = 1` with one
  15-bit distance code — a shape zlib itself rejects as an incomplete
  code, which is exactly why the port's missing root-width clamp never
  showed up against the CPython oracle, but this crate deliberately
  tolerates incomplete codes) built a table whose fill loop's stride
  arithmetic underflowed: a panic in debug, and in release **the decode
  loop never terminated** (confirmed hung past 120 s). Both fixed with no
  output or throughput change on any shape actually exercised by the
  existing suite (interleaved A/B: every shape within ±1.3% of the
  pre-fix binary); the LZ77 word-copy `memmove` threshold is 64 bytes
  (`WORD_COPY_MAX`), and the `HuffmanTree`/`DecodeTable` differential now
  covers 460 shapes and 12.5M bit patterns.
- **`oxiarc-brotli`: a shared-dictionary copy that ran past the end of the
  dictionary could decode to the wrong bytes without an error.** Both
  decoders used to continue such a copy in the produced output (a
  "straddle"); `brotli 1.1.0` rejects the stream instead — re-derived from
  a pair of hand-built streams differing in exactly one copy length, at
  two window sizes. The incremental decoder resolved the continuation
  against its bounded ring, so on a stream crafted to trigger it the
  answer depended on the caller's output-buffer size: correct at some
  sizes, `InvalidDistance` at others, `Ok` with silently wrong bytes at
  others again — a hostile `dcb` body could make a proxy emit silently
  wrong bytes. Both decoders now reject the overrun where the distance is
  resolved, before a byte of the copy is produced, so they agree with
  each other and with the reference whatever chunking the caller uses.
  Only hand-built streams can reach this — neither this crate's encoder
  nor the reference's ever emits such a command. Removing the straddle
  machinery also narrowed the decoder's per-command state from 40 to 24
  bytes, which measurably helped the one throughput shape (copy-dense
  streams at large windows) that had been short of its target. That shape is
  still short: 0.80x when BROTLI3-verify measured it, and 0.71x-0.76x (min) /
  0.70x-0.79x (paired) when re-measured 2026-09-08 at load 60-76, against a
  0.85x target. The other three shapes clear it comfortably.
- **`oxiarc-lzw`: `z::ZReader` (the `.Z`/`Content-Encoding: compress`
  push decoder) buffered the whole compressed body instead of a bounded
  window.** Its bit position was measured from the last code-width
  change, so it retained every compressed byte since that event — and a
  16-bit stream stops changing width early (a non-block-mode stream has
  no `CLEAR` codes at all), making the retained tail the *entire* stream
  and the decode quadratic. `with_max_output` did not help (it bounds
  output, not the compressed carry). Measured on a 5.2 MB stream: 20.5 MB
  of live heap before, 230 KB after; an 8 MiB payload went from 762 ms to
  107 ms. The decoder now rolls its group origin forward every eight
  codes, exactly as the reference `getcode()` does.
- **`oxiarc-lzw`: a crafted 303-byte `.Z` stream could panic the
  decoder.** At `max_bits = 9` the reference's width rule takes the code
  width to 10 bits, so a code naming a table slot one past the end of a
  *full* table was accepted as KwKwK and then used as an array index —
  an out-of-bounds panic on attacker-controlled input. Such a code is now
  `LzwError::InvalidCode`, matching the guard the generic (non-`.Z`)
  decode engine already had.
- **`oxiarc-lzw`: `z::ZWriter` could report success (`finish() -> Ok`) for
  a stream a failed inner write had silently corrupted.** `.Z` has no
  checksum and no end-of-information code, so the result decoded to
  plausible garbage rather than failing loudly. An inner-writer error is
  now sticky: every later `write`/`flush`/`finish` fails; a failed header
  write is no longer recorded as done; and one large `write` call no
  longer stages its entire compressed output in memory before flushing
  (batches of about 32 KiB now, matching the crate's documented "small
  staging buffer" claim, which was not true before this fix).
- **`oxiarc-jpeg`: the `rayon` parallel decode band merge placed every
  band after the first at the *unscaled* block pitch**, corrupting every
  `DecodeOptions::scale` other than `Scale::FULL` on a restart-marker
  stream — invisible at full scale, where the unscaled and scaled pitch
  are the same number, and invisible to every existing test because
  `plan_bands` needs ≥ 256 MCUs and a restart interval, which no
  scaled-decode fixture had. Measured before the fix on a 256×256
  4:2:0 image with 16-MCU restarts: **2,686 of 3,072** bytes per row-group
  differed from the same image decoded serially at `M = 1`; the same
  magnitude held for 4:4:4, 4:2:2, grayscale, CMYK and arithmetic coding.
  Fixed by having the band planner read its destination's own per-component
  block size instead of recomputing it, so the two cannot disagree; the
  crate's "`rayon` output is byte-identical with and without it" guarantee
  now holds at every scale, pinned by a serial-vs-parallel differential
  over `M` in `1..=16` and by a `djpeg -restart` byte-parity oracle.
- **`oxiarc-jpeg`: `compat::jpeg_decoder` and `compat::zune` each had one
  accessor that disagreed with what `decode()` actually produced.**
  `compat::jpeg_decoder::Decoder::info()` reported `PixelFormat::Rgb24`
  (3 bytes/pixel) for a 12-bit *colour* frame while `decode()` returned
  6-byte-per-pixel big-endian `u16` samples — a caller sizing a buffer the
  way the module's own doctest does would read half the image. Such
  frames now return a named `Unsupported(SamplePrecision)` from both
  methods instead; twelve-bit *grayscale* is unaffected
  (`PixelFormat::L16`, already correct), and the frame remains fully
  decodable via `Decoder::inner_mut()` plus `oxiarc_jpeg::Decoder::decode_u16`.
  `compat::zune::JpegDecoder::output_buffer_size()` under-reported by 25%
  for a CMYK source read as `ColorSpace::YCbCr` (`output_buffer_size()`
  used the color space's nominal component count instead of the source's
  real one); it now reports what `decode()` actually produces, for every
  source, and `output_colorspace()` returns the requested space rather
  than a value that does not match `decode()` either — matching real
  `zune_jpeg` 0.5.15's own behaviour, verified by reading its source.
- **`oxiarc-http`: a `dcz` (RFC 9842, Zstandard variant) body carried no
  binding to the dictionary it claimed to need.** The original decoder
  decoded *any* ordinary dictionary-referencing Zstandard frame that
  happened to arrive under `Content-Encoding: dcz`, with no check that it
  named the caller's own dictionary — a frame built against any
  dictionary, or none, decoded so long as its sequences did not reference
  an out-of-range offset. `dcz` bodies now open with an RFC 8878 §3.1.2
  skippable frame (magic `5E 2A 4D 18`) carrying the dictionary's
  SHA-256, verified before a single real frame byte decodes; `encode_body`/
  `Encoder<W>`'s `Dcz` arms write it first. Verified against the
  reference decoder: a genuine `encode_body`-produced `dcz` body, fed
  straight to `zstd -D <dict> -d`, has the preamble silently skipped (as
  any conformant Zstandard decoder must) and the frame after it decoded
  byte-identically. **Breaking within this same, still-unreleased 0.4.2
  cycle** (no released consumer is affected): a `dcz` body produced by an
  earlier `[0.4.2]` build of this crate does not carry this preamble and
  is now refused as too short.
- **`oxiarc-http`: every `dcb` decode error named `br` instead of `dcb`.**
  `HttpCodingError::Corrupt`'s public `coding` field — matched on by
  callers, and printed by `Display` — was hard-wired to
  `ContentCoding::Brotli` in the error mapper `dcb` shared with the plain
  `br` stage, so a short body, a bad magic, a wrong-dictionary digest and
  a corrupt meta-block all reported a coding the response never sent.
  `dcz` already reported `dcz` correctly; `dcb` now matches.
- **`oxiarc-cli`: `list`/`extract`/`test`/`convert`/`add`'s "not an
  archive, did you mean an image" hint, and `detect`/`info`'s own
  sniffing, used to read the *entire* input just to check up to 8 magic
  bytes** — an unbounded allocation driven by file size on what is meant
  to be a fast rejection path. Peak RSS on a 300 MB unrecognised file:
  303 MiB before, 3.06 MiB after (a bounded 16-byte-prefix read,
  retrying on `ErrorKind::Interrupted`). `oxiarc add` also computed this
  hint *eagerly, before* dispatching on the archive format, so a
  successful `add` to a ZIP/TAR/LZH archive paid a second full pass over
  it purely to phrase an error it never reached; now computed only in the
  fallback arm. `detect`/`info` now sniff the bounded prefix before
  deciding whether to read the file in full, rather than always
  buffering it first.
- **`oxiarc-core`: two panic messages had roughly 30 embedded spaces
  from a botched line continuation** (`traits.rs`'s `decompress_all` and
  `async_io.rs`'s async-compress final-flush error) — cosmetic, no
  behavioural change.
- **`oxiarc-image`: `guess_format` never recognised AVIF.** The magic-byte
  table's AVIF row padded its comparison mask to the full 12-byte
  signature length with zero bytes, which makes the `ftypavif` brand
  comparison at offset 4 unsatisfiable by any input (`byte & 0x00 == 'f'`
  is never true) — matched nothing, ever. A real AVIF file was reported
  as "format could not be determined" instead of the named
  `Unsupported(Avif)` the crate's own docs promised for all 15 `ImageFormat`
  variants. Fixed to the real `image` crate's own 4-byte mask (confirmed
  against its literal source), with a comment on why the mask is
  deliberately shorter than the signature so it is not "corrected" back.
- **`oxiarc-image`: decoding a premultiplied-alpha 32-bit-float TIFF
  clamped un-premultiplied colour samples to 1.0**, silently discarding
  any HDR highlight above full scale — the straight-alpha path right
  beside it never clamped, so a file's `ExtraSamples` tag alone decided
  whether values above 1.0 survived the decode. Neither `image` nor
  `tiff` un-premultiplies at all, so there was no upstream behaviour to
  match; the clamp was this crate's own invention and is now removed.
- **`oxiarc-image`: a JPEG whose declared sample precision is 9-15 bits
  decoded at up to 1/16th of the `u16` range its `ColorType::L16`/
  `Rgb16` label promises**, i.e. up to sixteen times too dark once
  converted to 8-bit. `oxiarc-jpeg` correctly writes such a frame's
  samples *unscaled* (0..2^P-1); this crate's own `ColorType::L16` label
  is a *range* contract (`Primitive::<u16>::DEFAULT_MAX_VALUE == 65535`)
  that every other conversion in the crate assumes, so the samples are
  now rescaled to the full 16-bit range on decode (`v * 65535 / (2^P -
  1)`, exact at the endpoints and at P = 16).
- **`oxiarc-image`: `codecs::png::CompressionType::Level(n)` for `n > 9`
  was passed straight through to `oxiarc-deflate`**, which has no defined
  meaning above level 9; now clamped to 9.

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

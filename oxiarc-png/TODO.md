# oxiarc-png - Development Status (v0.4.2, 2026-09-07)

Part of the OxiArc P2/P3 program; see the root `TODO.md` section "Phase 8".

## Completed Features (COMPLETE)

### Chunk layer
- [x] Signature, chunk framing, `2^31-1` length cap with overflow-safe arithmetic
- [x] CRC-32 over type + data, fed incrementally through `oxiarc_core::Crc32`
- [x] `ChunkType` with `is_critical`/`is_private`/`reserved_set`/`safe_to_copy`
- [x] `write_chunk`/`write_chunk_parts` (streaming CRC, no intermediate buffer)
- [x] `ChunkIter`, a borrowing walker for in-memory files
- [x] Per-chunk length table; wrong length fatal for `IHDR`/`PLTE`/`IEND`/`fcTL`, recoverable otherwise

### Header and geometry
- [x] `IHDR` parsing with the full Table 11.1 validity matrix
- [x] `BytesPerPixel` as the six reachable filter strides
- [x] `expected_raw_bytes()` computed a priori, including the Adam7 pass sum

### Filtering
- [x] `unfilter` for None/Sub/Up/Average/Paeth, monomorphised per stride, in place
- [x] First-row degeneration (`Up` -> identity, `Paeth` -> `Sub`, `Avg` with no upper row)
- [x] Forward filters with the same specialisation
- [x] Three Paeth formulations proved equal over all 2^24 inputs
- [x] `select_filter`: fixed, adaptive (MSAD) and minimum-entropy strategies
- [x] Rayon-backed parallel per-row filtering behind the `parallel` feature
      (`src/encoder/parallel.rs`); serial fallback below `MIN_ROWS_FOR_PARALLEL_FILTER`
      rows so small frames never regress

### Interlacing
- [x] Adam7 pass table, `pass_dimensions`, `Adam7Iterator` skipping empty passes
- [x] `expand_pass` / `expand_pass_splat`, bit-exact for sub-byte depths
- [x] 1x1, `width < 5` and `height == 1` empty-pass handling
- [x] Write-side `extract_pass_row` (encoder), proven to round-trip against the
      read-side expansion for every colour type / bit depth combination

### Decoding
- [x] Push `StreamingDecoder` with `update(buf, Option<&mut UnfilterBuf>) -> (usize, Decoded)`
- [x] Pull `Decoder<R: Read>` / `Reader<R>` layered on the same state machine
- [x] Incremental `IDAT` decoding; zero-length chunks legal, boundaries carry no semantics
- [x] `WrappedInflate(Zlib)` with `verify_checksum(false)` by default
- [x] PNG's own zlib header rules: `CM == 8`, `CINFO <= 7`, `FDICT == 0`, check bits
- [x] Two-row scratch buffer; the inflater never sees more space than one scanline
- [x] Transformations `EXPAND`/`ALPHA`/`STRIP_16` with the `png`-identical output table
- [x] Palette expansion through a memoised 256-entry LUT
- [x] Chunk ordering state machine; unknown critical chunks fatal, unknown ancillary retained
- [x] CRC policy: critical fatal, ancillary dropped by default
- [x] Missing `IEND` is an error; trailing image data is `ExtraImageData` in strict mode
- [x] `Limits` budget and `DecodeLimits` for every attacker-controlled dimension, including
      `Reader::check_alloc` and its use in the APNG compositor's canvas allocations
      (`ApngDecoder::new_with_limits`) — not just the per-frame scratch buffer

### Ancillary chunks
- [x] `PLTE`, `tRNS` (per colour type, with `png`-compatible truncation and the
      oversized-indexed rule), `gAMA`, `cHRM`, `sRGB`, `iCCP` (bounded inflate),
      `cICP`, `mDCv`, `cLLi`, `sBIT`, `bKGD`, `hIST`, `pHYs`, `sPLT`, `tIME`,
      `tEXt`/`zTXt`/`iTXt` with the full keyword grammar, `eXIf`, `oFFs`, `sCAL`,
      `pCAL`, `sTER`
- [x] Unknown ancillary chunk retention with a byte budget

### Animation and vendor extensions
- [x] `acTL`/`fcTL`/`fdAT` parsing, one shared sequence counter, bounds checks
- [x] Per-frame inflater reset plus a file-level decompressed-byte budget
- [x] Apple `CgBI`: raw DEFLATE, BGR(A) swap, premultiplied flag, strict-mode rejection
- [x] **APNG compositor** (`src/apng.rs`): `ApngDecoder::next_subframe` /
      `next_composed`, `DisposeOp`/`BlendOp` semantics to RGBA8/16 (including
      Porter-Duff `Over` with correct `out_a = src_a + dst_a*(1-src_a)`), first
      frame's `Previous` treated as `Background` per spec, canvas allocation
      budget-checked via `DecodeLimits::check_alloc`

### Encoder (track PNG2)
- [x] `Encoder`/`Writer`/`StreamWriter`, mirroring `png` 0.18's shapes
      (`tests/compat_surface.rs` exercises every §1.2 call-site pattern under
      `use oxiarc_png as png;`)
- [x] Interlaced encoding (Adam7 write-side), all ancillary chunks on write
- [x] `Deflater` driven directly across one continuous zlib stream per frame
      (never `ZlibStreamEncoder`, which auto-`sync_flush`es every 128KiB and
      would fragment the stream at content-arbitrary boundaries)
- [x] `ApngEncoder`: `write_frame`, `fcTL`/`fdAT` sequencing, default `IDAT`-as-first-frame
- [x] Filter strategies on write: NoFilter/Sub/Up/Avg/Paeth/Adaptive/MinEntropy
- [x] `parallel` feature (`dep:rayon`, off by default): parallel per-row filter
      *selection* on encode; DEFLATE itself stays strictly serial by design (see
      "Triaged out" below for why `oxiarc-deflate/parallel` was not wired in)
- [x] `png`-0.18 compatibility surface: the native API already satisfies every
      §1.2 call site with `use oxiarc_png as png;` (`tests/compat_surface.rs`,
      16 tests) plus the closed §1.3 name set (`closed_set_names_resolve`);
      no separate compat module was needed for the 0.18 shape
- [x] `png`-0.17 compatibility surface (`src/v017.rs`, `use oxiarc_png::v017 as
      png;`): `FilterType` + `AdaptiveFilterType` folded onto this crate's
      single `Filter`, `Reader::output_buffer_size() -> usize` (saturating,
      never wrapping), `Decoder`/`Reader`/`Encoder`/`Writer` shims, and the
      0.17-only export set. Deliberately a **module, not a Cargo feature**, so
      feature unification cannot reshape one crate's API because another crate
      in the graph wanted the older shape (design doc §9.3). Tested by four
      unit tests plus `tests/compat_surface.rs`'s `v017_shapes` module behind
      the `compat-017` feature (design doc §11.6), which pins the two filter
      knobs against the *emitted filter bytes*, not just the fold function
- [x] DEFLATE input-batching (`encoder::zlib::DEFLATE_BATCH_SIZE = 32KiB`):
      `FrameEncoder` accumulates filtered rows into ~32KiB batches before each
      `Deflater::deflate()` call instead of calling it once per scanline — see
      "Known measured gaps" below for why this mattered and by how much

## Triaged out (deliberate, justified omissions — not silent gaps)

- **Fuzz targets** (`fuzz_png_decode`, `fuzz_png_decode_limits`,
  `fuzz_png_streaming`, `fuzz_png_roundtrip`, `fuzz_png_apng`,
  `fuzz_png_unfilter` — design doc §11.5). These belong in the shared
  workspace `fuzz/` crate alongside the existing ~20 targets, which is
  cross-track shared infrastructure, not `oxiarc-png`-owned. `tests/props.rs`
  carries the same properties as `proptest` unit-level checks in the
  meantime (`prop_encode_decode_roundtrip`,
  `prop_apng_compose_never_panics_and_canvas_is_always_full_size`, plus the
  five pre-existing decode-side properties), which is real but weaker
  coverage than a corpus-driven fuzzer.
- **`oxiarc-deflate/parallel` inside the `parallel` feature.** Read
  `oxiarc-deflate::parallel::compress_deflate_parallel`'s source before
  deciding: it produces a concatenation of independently-terminated raw
  DEFLATE streams (split-then-parallel-compress-then-concatenate), which is
  structurally incompatible with a PNG `IDAT`/`fdAT` stream — that has to be
  *one* continuous zlib stream with one Adler-32 trailer. This crate's
  `parallel` feature therefore only parallelizes row filter *selection*
  (`src/encoder/parallel.rs`), which is embarrassingly parallel because
  filter selection only ever reads raw, unfiltered samples. DEFLATE itself
  stays serial. Not a shortcut — verified by reading the source, not assumed.

## Known measured gaps (open issues, out of this crate's ownership)

Two gaps were measured against real references in external scratch Cargo
projects (never a dependency of `oxiarc-png` itself — `deny.toml` bans `png`
here). Both point at `oxiarc_deflate::Deflater`'s internals, which are out of
`oxiarc-png`'s ownership to fix; `oxiarc-png` fixed what was in scope
(the input-batching issue below) and is reporting the rest rather than
routing around `oxiarc-deflate`.

### Compression ratio: ~37% larger than Python's `zlib.compress(level=6)`

Fixture: 256x192 RGB8, smooth gradient + mild noise, `Filter::Adaptive`
row-filtered, compared as raw filtered bytes (isolating DEFLATE quality from
PNG framing and from filter-choice differences). Measured in
`tests/pillow_oracle.rs::compression_ratio_does_not_regress_past_the_measured_gap_to_pythons_zlib_level6`:
`oxiarc_deflate::zlib_compress(_, 6)` produces 19,397 bytes vs. Python's
14,189 bytes on the same input — a 36.7% gap. The design doc's prescribed
response to a >5% gap was "wire `OptimalParser` more aggressively"; this was
tried (lowering `encoder::zlib::use_optimal_parsing`'s threshold from
`level >= 9` to `level >= 6`) and *measured* to roughly double the gap
instead (121% over Python's output), so it was reverted — see that
function's doc comment for the full account, including that the underlying
reason `with_optimal_parsing` doesn't help at level 6 was left as an open
question rather than a guessed-at diagnosis (a plausible but *unverified*
lead: `Deflater::with_optimal_parsing` still uses
`Lz77Encoder::with_level(level)` for match-finding, so a shallower level-6
search may starve the DP of candidates — not confirmed by reading
`Lz77Encoder` or testing intermediate levels). The regression-guard test
asserts a 1.45 ceiling (above the measured 1.37 baseline), not the original
1.05 (5%) design target. Fixing the real gap belongs in `oxiarc-deflate`.

### Encode speed: 0.30-0.45x of the real `png` 0.18 crate (target was >=0.85x)

Fixture: 1024x1024 RGBA8, default settings, measured end-to-end
(`Encoder`/`Writer`/`finish`) against `png` 0.18.1 + default `flate2`
backend in an external scratch project. Before the input-batching fix
below, oxiarc-png was 0.09-0.15x the speed of `png` (495.1ms vs. 74.6ms at
1024x1024). Diagnostic breakdown that found the fixable part: filtering
alone cost 1.4ms (0.3% of the total, not the bottleneck); one-shot
`oxiarc_deflate::zlib_compress` on the same filtered bytes cost 200.9ms; the
full `Encoder`/`Writer` path cost 495.1ms — an unaccounted ~293ms of
overhead between "compress the bytes once" and "encode the image". A
batch-size sweep isolated that overhead to the *calling pattern*:
`Writer::write_image_data` was calling `Deflater::deflate()` once per PNG
scanline (1024 calls for this fixture), and `Deflater` has a sharp per-call
throughput cliff below roughly 32KiB of input (batch=1 row/call: 476.1ms;
batch=4: 448.3ms; **batch=8: 175.3ms**, the cliff; batch=16-1024: 165-175ms,
parity with the one-shot number). This part *was* fixable from
`oxiarc-png` alone (LZ77/entropy-coder state is provably identical whether
fed via many small calls or fewer large ones) and was fixed:
`encoder::zlib::FrameEncoder` now batches filtered rows into 32KiB chunks
before each `Deflater::deflate()` call (`DEFLATE_BATCH_SIZE`,
`flush_input_batch`). Re-measured after the fix: 1024x1024 now 171.951ms
(2.9x faster than before, speed ratio 0.408, up from 0.146); 256x256 now
13.599ms (ratio 0.295, up from 0.091); 4096x4096 now 2898.2ms (ratio 0.454).
Output size also improved slightly (~0.937x of `png`'s own default output
at every size tested, i.e. smaller). **The remaining 0.30-0.45x gap is not
a calling-pattern problem** — a fair one-shot head-to-head on identical
filtered bytes (same fixture) still showed `flate2::ZlibEncoder` finishing
in 69.8ms against `oxiarc_deflate`'s 158-201ms, 2.3-2.9x — i.e. raw
per-byte DEFLATE throughput, not something `oxiarc-png` can change from its
side of the API. Out of this track's ownership; reported here as the
cleanest evidence available for whoever next optimizes `oxiarc-deflate`.

### Informational: Pillow 12.1.0 has an APNG alpha-blending bug

Not an `oxiarc-png` bug. `tests/pillow_oracle.rs`'s
`apng_frames_composite_identically_by_pillow` found Pillow 12.1.0's APNG
plugin miscomputes the alpha channel under `blend_op = Over` with
intermediate source alpha — it applies the same linear-blend coefficient to
the alpha channel that it uses for RGB, instead of the correct Porter-Duff
`out_a = src_a + dst_a*(1 - src_a)`. Verified two independent ways: (1)
`PIL.Image.alpha_composite` (Pillow's own correct compositing primitive, used
directly rather than through the APNG plugin) agrees with `oxiarc-png`'s
alpha output; (2) a Pillow-only encode-then-decode round trip (no
`oxiarc-png` involved at all) reproduces the same wrong value, proving it is
internal to Pillow's APNG codec. Worked around in that test by comparing RGB
channels only (+/-1 tolerance) against the APNG-plugin round trip, plus a
second test (`alpha_channel_matches_pillows_alpha_composite_primitive`) that
cross-checks alpha against the *correct* Pillow primitive directly. Recorded
here because it is genuinely useful to anyone else using Pillow as an APNG
oracle, not because it blocks anything in this crate.

## Future Enhancements

- [ ] `portable_simd` unfilter kernels behind a nightly feature
- [ ] `TrySmallest` filter strategy (deflate each candidate) behind `parallel`
- [ ] Re-run the compression-ratio and encode-speed measurements above once
      `oxiarc-deflate` changes its match-finding depth or per-call
      throughput, to see whether the regression-guard ceilings (1.45 ratio,
      >=0.85x speed target) can tighten

## Known Issues

See "Known measured gaps" above (compression ratio, encode speed) and
"Triaged out" (fuzz targets, `oxiarc-deflate/parallel`). No other known
issues.

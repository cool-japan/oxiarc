# oxiarc-tiff - Development Status (v0.4.2, 2026-09-08)

Program context: root `TODO.md`, "Phase 8", items **W1-G0** (core),
**W2-TIFF1** (codecs) and **W2-TIFF2** (completion: `compat`, `rayon`,
`mmap`, fuzz seeds) are all complete.

**Cross-crate blocker hit and cleared during this track (recorded so the
transient failure is not mistaken for a regression later):** for part of this
session, `cargo clippy -p oxiarc-tiff --all-features --all-targets --
-D warnings` failed on dead-code findings inside the `oxiarc-jpeg`
*dependency* (`encoder/arith.rs`'s `LosslessCoder`/`encode_lossless_diff`,
`encoder/markers.rs`'s `dac`, `encoder/plan.rs`'s `EncodePlan.arithmetic`),
confirmed independent of any change in this crate
(`cargo clippy -p oxiarc-jpeg --all-features --all-targets -- -D warnings`,
run in isolation, failed identically) and consistent with in-progress
concurrent work on track **W1-F3** (arithmetic coding) in the same working
tree. **As of the final gate run for this track, `oxiarc-jpeg`'s own clippy
gate is clean and `cargo clippy -p oxiarc-tiff --all-features --all-targets
-- -D warnings` passes with zero warnings.** `cargo build --all-features`,
`cargo nextest run --all-features` (434 tests) and
`cargo test --doc --all-features` (36 doctests) passed throughout regardless
(dead-code is a lint, not a compile error, outside of `-D warnings`).
`oxiarc-jpeg` is still being actively developed in this same working tree by
another track; if this gate is red again later, it is that crate's
regression, not this one's — verify with `cargo clippy -p oxiarc-jpeg
--all-features --all-targets -- -D warnings` in isolation before assuming
otherwise.

## Completed (W1-G0, core)

### Container
- [x] `byteorder.rs` — `Endian` (II/MM) with `EndianReader`/`EndianWriter`
      over any `Read + Seek` / `Write + Seek`, offset patching, 2-byte
      alignment, native-order conversion helpers. No `byteorder` crate.
- [x] `header.rs` — classic TIFF and BigTIFF (`Variant`), both byte orders,
      the BigTIFF 8/0 constant field, first-IFD pointer, `inline_value_bytes`
- [x] IFD chain walk with a visited set (loops terminate), `max_ifds`
- [x] SubIFD trees (tag 330) with their **own** visited set and a depth cap;
      `sub_ifds()` for one level and `sub_ifd_tree()` for the whole tree
- [x] EXIF (34665), GPS (34853) and Interoperability sub-IFDs

### IFD and tags
- [x] All 18 field types incl. `LONG8` / `SLONG8` / `IFD8`
- [x] Inline versus out-of-line values decided from the byte length, never
      from the type alone; recomputed on write
- [x] `count * size` overflow guards before any allocation
- [x] Lazy value loading: an entry keeps its `ValueSource` until read
- [x] `Value::Unknown { ty_raw, bytes }` retains unknown *field types* so an
      unrecognised entry round-trips byte-identically
- [x] `tags.rs` — one documented `Tag` enum plus a raw `u16` fallback:
      baseline, extensions, GeoTIFF (33550, 33922, 34264, 34735-34737),
      EXIF 34665, GPS 34853, XMP 700, ICC 34675, IPTC 33723, Photoshop 34377,
      DNG basics, ImageJ/OME `ImageDescription` passthrough
- [x] `Type`, `CompressionMethod`, `PhotometricInterpretation`,
      `PlanarConfiguration`, `SampleFormat`, `Predictor`, `ResolutionUnit`,
      `FillOrder`, `Orientation`, `ExtraSamples`, `T4Options`, `T6Options` —
      every one with a raw fallback variant
- [x] Compression 32766 / 32771 / 32895-32898 / 32947 / 34661 and photometric
      32803 / 34892 have named variants (critique T-8 / T-9)

### Geometry and sampling
- [x] `ImageInfo` with per-channel `BitsPerSample`, `expected_total_bytes()`,
      strip and tile geometry, tile padding (coded versus valid dimensions),
      `RowsPerStrip` absent = 2^32-1, missing `StripByteCounts` recovery,
      `SHORT` and `LONG` offset arrays widened transparently
- [x] `Samples` enum: `U8/U16/U32/U64/I8/I16/I32/I64/F16/F32/F64`
      (`F16` holds raw `u16` bits with in-crate `f16` <-> `f32`)
- [x] Unpacking of 1/2/4/8/12/16/24/32/64-bit samples, MSB-first, per-row
      padding, heterogeneous `BitsPerSample`
- [x] `FillOrder` 2 — bits of every byte of the **compressed** chunk reversed,
      at every bit depth, before the codec on read and after it on write;
      skipped for the CCITT family (`compression::handles_fill_order`)
- [x] Endian swap **after** the predictor, never before

### Transforms and colour
- [x] Predictor 1 (none)
- [x] Predictor 2 — horizontal differencing for 8/16/32/64-bit integers in the
      **file's** byte order with whole-sample carry propagation; stride is
      `SamplesPerPixel` for chunky and 1 for planar
- [x] Predictor 3 — floating-point byte-plane transpose
- [x] `MinIsWhite` inversion, palette expansion through `ColorMap`,
      YCbCr -> RGB with `YCbCrCoefficients` / `ReferenceBlackWhite` and
      subsampling reconstruction, CMYK/Separated and CIELab passthrough,
      `ExtraSamples` exposure with associated-alpha un-premultiplication

### Codecs (core)
- [x] `compression/none.rs`, `compression/packbits.rs` (decode into a slice
      with bounds checks, encode)
- [x] Dispatch with **no wildcard**: every registered value has a variant
- [x] `Codec` trait + `CodecRegistry` for out-of-tree codecs (LERC, WebP, JXL)
- [x] `UnsupportedError::FeatureNotCompiled { feature }`

### Pipeline and API
- [x] `decode/{strip,tile}.rs` — fetch into a reusable buffer, decompress into
      a pre-sized chunk buffer, fill order, predictor undo in place, endian
      swap, unpack, planar interleave, sub-rectangle copy. No per-chunk `Vec`.
- [x] `Decoder<R: Read + Seek>` — `image_count`, `next_image`, `seek_to_image`,
      `info`, `read_image`, `read_image_bytes`, `read_region`,
      `read_strip`/`read_tile` raw and decoded, typed tag accessors,
      `all_tags`, `icc_profile`, `xmp`, `iptc`, `photoshop`, `geo_tags`
- [x] `Encoder<W: Write + Seek>` — `new_image` builder, streaming
      `write_rows`/`write_chunk`, `write_image`, multi-page, arbitrary extra
      tags (GeoTIFF passthrough, ICC, XMP, DocumentName/PageName/PageNumber),
      classic versus BigTIFF with auto-promotion past 4 GiB, offsets patched
      after the data is written
- [x] `error.rs` (thiserror, five `#[non_exhaustive]` groups), `limits.rs`
      (`Limits`, `OutputBudget`, `Leniency`, `Warning`)

### Tests (W1-G0 baseline: 264 all-features, 254 default, + 23 doctests)
- [x] The 40-row edge-case register, one named test per row (`e1`..`e40`)
- [x] Round-trip matrix: every colour type, bit depth, sample format, layout,
      container, byte order, predictor, fill order
- [x] `proptest`: uncompressed/PackBits, predictors, tiles, regions, and an
      arbitrary-bytes no-panic property
- [x] Corrupt-input sweeps: single-bit flips, byte substitutions, zeroed
      four-byte windows — none panic
- [x] `tiff-oracle`: 24 `tifffile` fixtures decoded byte-identically, 14
      `tiffcp` re-codings of each, `tiffinfo` accepts what we write with no
      warnings, Pillow and `tifffile` read our pixels, multi-page both ways,
      sub-byte and palette images survive libtiff, `FillOrder` 2 round-trips
      through libtiff, libtiff accepts our subsampled YCbCr
- [x] Benches: decode/encode/predictor/region groups with recorded numbers

## Completed (W2-TIFF1, codecs)

### Codecs, both directions
- [x] **LZW (5)** — `oxiarc_lzw::decompress_tiff_into` straight into the chunk
      buffer; the pre-1993 `early_change = false` rule is retried **only on an
      error**, into scratch, and the winning rule is cached in `CodecState` for
      the rest of the image (a short decode is a short strip, not a reason to
      retry — measured in `oxiarc-lzw`, track D)
- [x] **Deflate (8 and 32946)** — `WrappedInflate(Zlib)` with
      `TrailingPolicy::Stop` and `verify_checksum(!lenient)`, cached in
      `CodecState` and `reset()` per chunk so the 32 KiB window is allocated
      once per image. Padded strips are normal, never an error (critique P0-9);
      `zlib_decompress_into` is deliberately not used
- [x] **ZSTD (50000)** and **LZMA (34925)** — `decompress_into` into the chunk
      buffer, one complete `.xz` stream per chunk through `oxiarc_lzma::xz`;
      lenient reads salvage a prefix
- [x] **CCITT RLE (2), Group 3 (3), Group 4 (4), word-aligned RLE (32771)** —
      T.4 tables with a 13-bit lookup, a changing-element engine shared by the
      decoder and the encoders, `T4Options`/`T6Options`, byte- and word-aligned
      rows, byte-aligned EOLs, the 2D tag bit, `FillOrder` consumed inside the
      codec, encoders for every dialect. The changing-element buffers live in
      `CodecState` too, so a fax image allocates them once and not per chunk
- [x] **JPEG (7)** — `TableSet` + `decode_abbreviated_into` with
      `raw_components` (never a merged buffer, critique P0-10), the
      photometric x `APP14` x subsampling table, `SOF` sampling factors
      authoritative, `JPEGTables` (347) written once per page, `RowsPerStrip`
      multiple of `8 * Vmax` enforced on write, 9..=16-bit frames packed
      through `pack_row`
- [x] **Old-style JPEG (6)**, read: self-contained chunks, an interchange
      prefix spliced with the scan, and a frame synthesised from tags 512-521
      (whose table *offsets* are resolved by the new `ValueSource::read_raw`)
- [x] Explicit `FeatureNotCompiled` when a codec's feature is off and
      `Unsupported::Compression(n)` for WebP / JXL / LERC / JBIG / NeXT / IT8;
      `NotYetAvailable` is now unreachable and a sweep of compression values
      0..=1024 keeps it that way

### Tests (W2-TIFF1: 267 lib, 103 integration, 33 doctests, 14 oracle)
- [x] `tiffcp` matrix extended to 22 variants (`lzw`, `lzw:2`, `zip`, `zip:2`,
      `zip:3`, `lzma`, `zstd`, `zstd:2` added) with assertions that libtiff
      really produced each codec and predictors 2 and 3
- [x] `ccitt_recodings_decode_byte_identically` — `g3:1d`, `g3`, `g3:2d`,
      `g3:2d:fill`, `g3:fill`, `g4`, both photometrics
- [x] `jpeg_recodings_decode_close_to_the_reference` — our JPEG decode against
      **libtiff's own decode** of the same file (strips and tiles)
- [x] `libtiff_reads_every_codec_we_write` and
      `pillow_and_tifffile_read_every_codec_we_write`
- [x] `tifffile`-written zlib / lzma / zstd fixtures (an independent *writer*
      for those three) and a zlib+predictor fixture
- [x] `every_codec_survives_truncation_and_bit_flips` (> 2000 damaged inputs,
      bounded in time), per-codec proptest round trips, a lenient-recovery test
- [x] Benches: per-codec whole-image decode and encode, and the LZW gate
      measured against a reconstruction of the dictionary-of-`Vec` path
      (**18.7x**, gate was 3x)

## Deliberately deferred (with reasons)

- ~~**Group 3/4 uncompressed mode is reported, not decoded.**~~ **Done
  2026-09-08 (TIFFPOLISH).** The code table was checked against the primary
  source — ITU-T T.4 (07/2003) Table 5/T.4, not Table 3, which is the black
  run-length table — and both directions are implemented in
  `compression/ccitt/uncompressed.rs`. Decode is unconditional (the entrance
  code is unambiguous); encode is opt-in through
  `ImageSpec::with_ccitt_uncompressed`, sets `T4Options` bit 1 for Group 3 and
  `T6Options` bit 1 for Group 4, and is taken per row only when it is strictly
  smaller than the Huffman coding of the same row. libtiff parses the files
  (`tiffinfo` reports "Group 4 Options: uncompressed data") and, as documented,
  still cannot decode them.
- **Four codecs now meet the "whole-image decode <= 1.25x `tiffcp`" gate;
  ZSTD, LZMA, JPEG and the fax codecs still miss it.** *Earlier figures, taken
  before `oxiarc-lzw` / `oxiarc-deflate` / `oxiarc-zstd` were rewritten:* LZMA
  passed (1.15x RGB8, 1.16x Gray16), Deflate 1.39x-1.58x, LZW 2.13x-2.41x,
  JPEG 2.37x, ZSTD 5.07x-6.24x, "ZSTD the outlier and the first place to look".
  *Re-measured 2026-09-08* on the same geometry (4096x4096, 16-row strips,
  `tiffcp`-written fixtures), medians of 15 **interleaved** rounds, three
  independent runs at load average 110 down to 20, taking the median of the
  three run medians:

  | codec | before | now | gate |
  |---|---|---|---|
  | uncompressed | 0.11x | 0.19x / 0.13x | met |
  | PackBits | 0.20x | 0.16x | met |
  | LZW | 2.13x-2.41x | **1.00x-1.07x** | **met** (was missed) |
  | Deflate | 1.39x-1.58x | 1.14x RGB8, 1.33x Gray16 | RGB8 **met**, Gray16 missed |
  | ZSTD | 5.07x-6.24x | **0.96x Gray16**, 1.65x RGB8 | Gray16 **met**, RGB8 missed |
  | LZMA | 1.15x-1.16x | 1.31x-1.32x | **missed** (was met) |
  | JPEG | 2.37x | 1.90x | missed |
  | CCITT G3 / G3-2D / G4 | 2.68x-3.40x | 4.80x-4.96x | missed (unlike work, below) |

  The ZSTD and LZW movements are the codec rewrites landing. The LZMA row moved
  the wrong way although nothing in `oxiarc-lzma` changed — and the reference
  arm says why: `tiffcp`'s own LZMA time **halved** (2026→952 ms) where it moved
  by under 15% on the three rewritten codecs, so that row measures a different
  fixture, not a regression. Read it as "not comparable", not as "LZMA
  regressed"; recorded as measured, not explained away. The controls (PackBits,
  JPEG, and LZMA/CCITT modulo the fixture) reproduce their earlier band, which
  is what makes the movers believable at this load. Remaining follow-up belongs
  to `oxiarc-zstd` and `oxiarc-lzma`, which this crate only calls.
- **The bilevel rows of that table compare unlike work.** `tiffcp -c none`
  copies 2 MB of packed bits; this crate expands them to 16 MB of
  one-byte-per-pixel samples, which is the whole 5.30x uncompressed row
  (29.3 ms against 5.5 ms, re-measured 2026-09-08; 28.7 ms when first measured).
  Net of that expansion the fax codecs run at 1.87x-1.90x, reproducing the
  earlier "roughly 1.5-2x". A packed-output decode path would close it, and is
  not in any track's scope yet.
- ~~**The JPEG *encoder* lives in this crate**~~ **Done 2026-09-08
  (TIFFPOLISH).** `compression/jpeg/encode.rs` now maps a TIFF `CodecContext`
  onto `oxiarc_jpeg::EncodeOptions` and calls `oxiarc_jpeg::Encoder`; see the
  "Deferred" note below for the details and for the behaviour change it
  introduced (a two-channel JPEG chunk was a named `Unsupported` error for
  part of this same unreleased version, never in a published one, until the
  item directly below closed it).
- ~~**A two-component JPEG frame (libjpeg's `JCS_UNKNOWN` layout) cannot be
  written**~~ **Done 2026-09-08 (JPEG2C).** `oxiarc-jpeg` gained
  `ColorSpace::Unknown(2)` — sequential ids `1`/`2`, one shared quantisation
  and Huffman slot, no subsampling, no colour transform, no `JFIF`/Adobe
  marker — exactly as the fix was scoped below. A greyscale-plus-alpha
  *chunky* JPEG page now round-trips; the `ImageSpec::validate` refusal and
  the `plan()` arm that produced it are both gone.
  `tests/roundtrip.rs::a_two_channel_jpeg_page_round_trips_chunky_through_jcs_unknown`
  is the regression test, and checks `PlanarConfiguration::Planar` still
  works too. Verified against real libtiff, not merely against this crate's
  own decoder: `tiffinfo` reports the page cleanly, and `tiffcp -c none`
  makes libtiff's own libjpeg actually decode the two-component scan —
  byte-identical to this crate's decode of the same file. Original scoping,
  left here for the record: *"the fix belongs in `oxiarc-jpeg` — one
  `component_template` row and one `ColorSpace` variant for two
  components — and there is no correct workaround inside `oxiarc-tiff`,
  because the component count comes from the colour space and the entropy
  coding cannot be spliced after the fact."*
- **2-, 4-, 12- and 24-bit external fixtures.** `tifffile` needs `imagecodecs`
  to *write* non-byte-aligned depths and Pillow cannot write them at all.
  Covered internally by `e12`/`e13` and, in the encode direction, by
  `sub_byte_and_palette_images_survive_libtiff` (libtiff re-codes ours).
- **The real `tiff` crate as a dev-dependency.** Rejected by critique P0-7: it
  pulls `flate2` and `zune-jpeg`, which turns `cargo deny check bans` red.

## Corrections the codec wave made to the design report

Both were measured against libtiff 4.7.1 rather than assumed, and both now have
regression tests:

1. **The CCITT codes are photometric-agnostic** (§3.6.6 said photometric 0 vs 1
   flips which bit is white). `tiffcp -c g3` and `-c g4` write byte-identical
   strips for `MinIsWhite` and `MinIsBlack` pages holding the same bits, and a
   `tiffcp -c none` round trip returns those bits for both.
2. **libtiff writes no RTC** for Group 3 in TIFF (T.4 asks for six EOLs);
   Group 4 does get `EOFB`. We write what libtiff writes and read both.

Also: `Compression::Deflate` now writes tag value **8** rather than 32946,
because libtiff, GDAL and `tifffile` all write the Adobe registration.

## Completed (W2-TIFF2, completion)

- [x] `compat` feature: a `tiff`-0.11.3-shaped `compat::{decoder, encoder,
      tags}` facade over the native API (thin adapters only, no duplicated
      decode/encode logic). `DecodingResult` has exactly the eleven upstream
      variants (`F16(Vec<half::f16>)`, converted from the native raw-`u16`
      `Samples::F16` only at this boundary) and is not `#[non_exhaustive]`;
      same for `ColorType` (ten variants) and `TiffError` (six variants).
      `TiffEncoder<W, K: TiffKind = TiffKindStandard>` /
      `TiffKindBig` (BigTIFF) / `ImageEncoder` / `DirectoryEncoder` /
      all thirty `encoder::colortype::*` marker types. `tests/compat_api.rs`
      reproduces `image-0.25.10/src/codecs/tiff.rs`'s exact call sequence
      (decoder and encoder sides), with exhaustive, wildcard-free matches on
      `DecodingResult`/`ColorType`/`TiffError` so a shape break is a compile
      error. `half` is a `compat`-only dependency; the native API never
      needs it.
- [x] `rayon` feature: `Decoder::{read_image_parallel, read_image_bytes_parallel,
      read_region_parallel, read_region_bytes_parallel}` and
      `Encoder::write_image_parallel`, byte-identical to the serial paths
      (`tests/rayon_parallel.rs`: strips, tiles, a non-tile-multiple size,
      planar, Deflate — the one codec whose scratch lives behind a shared
      `Mutex` — a windowed region, a budget-overrun parity check, and a
      dedicated JPEG `shared_tables: true` case verified non-vacuous by
      sabotage: commenting out the parallel driver's `set_jpeg_tables` call
      makes the byte-identity assertion fail immediately).
      Architecture: fetch/gather is serial (one `Read`/`Write` handle),
      decode/encode is parallel (one scratch buffer per chunk, shared
      read-only `CodecState`), place/stream is serial. Refactored the serial
      pipeline so there is exactly one implementation of "what a chunk
      decodes/encodes to" either way: `decode::chunk_output_len`,
      `strip::chunk_indices_for_rect` / `tile::chunk_indices_for_rect`,
      `writer::image::{gather_chunk_into, encode_chunk_pure,
      build_shared_jpeg_tables_tag}`. Measured (4096x4096 Gray8 PackBits,
      256x256 tiles): decode 1.7x, encode 3.1x.
- [x] `mmap` feature: `Decoder::from_path` / `MmapDecoder`, a
      `Decoder<Cursor<MappedFile>>` built on `oxiarc_core::mmap::MappedFile`
      (already `Read + Seek` via `Cursor`, so no new reader type was
      needed).
- [x] `CodecRegistry` write-side completion: `writer::Compression::Registered(u16)`
      (the registry was already consulted first on decode via
      `CompressionMethod::Unknown`/named-but-unimplemented values; the
      writer had no way to *select* a plugin codec until this). Documented,
      tested end to end in both directions
      (`tests/codec_registry_example.rs`): a trivial codec taking over
      `CompressionMethod::Webp` (thematically the D-5 use case), and a
      second test proving a registered codec can override a built-in method
      too (the registry is consulted before the built-in dispatch).
- [x] ICC/XMP/IPTC/Photoshop tag *type* preservation asserted directly
      (`Value::Undefined` vs `Value::Byte`), not only the decoded bytes,
      in `tests/roundtrip.rs`.
- [x] Fuzz seed generator: `examples/tiff_fuzz_seeds.rs` (mirrors
      `oxiarc-http/examples/http_fuzz_seeds.rs`'s shape) generates and documents
      the byte layout for five targets — `tiff_decode`, `tiff_roundtrip`,
      `ccitt_decode`, `packbits_decode`, `lzw_tiff_decode` — with the actual
      `fuzz_targets/*.rs` left to the workspace's Wave 3 track (which owns
      `fuzz/`); `tests/fuzz_seeds.rs` re-derives the same seeds and decodes
      them through the documented header layout, so the doc comment and the
      generator can never silently drift.
- [x] `benches/tiff_bench.rs`: a `rayon` on/off pair (`tiff_rayon_decode`,
      `tiff_rayon_encode`), gated `#[cfg(feature = "rayon")]`.
- [x] README: `compat`/`rayon`/`mmap` feature rows, a
      "Migrating from the `tiff` crate" section, parallel-decode and
      mmap quick-start examples, measured `rayon` numbers.
- [x] `tests/geotiff_passthrough.rs`: an external oracle (not only the
      internal round trip `tests/roundtrip.rs` already had) — a `tifffile`
      fixture carrying the six-tag GeoTIFF core set (`extratags=`), read
      with our `Decoder::geo_tags`, re-emitted through our writer's generic
      `extra_tags` (no GeoTIFF special case), and `tiffinfo -D` run on
      *both* files, asserting the four `Tag <n>: ...` lines it prints
      (libtiff does not know these tags by name, so it prints raw values —
      exactly what a byte-for-byte comparison wants) match exactly, with a
      floor on how many lines must appear so the comparison cannot pass
      vacuously if `tiffinfo`'s output ever changes shape.

## Deferred (out of this track's scope, recorded so it is not re-discovered)

- ~~**Move the JPEG encoder to `oxiarc-jpeg`**~~ **Done 2026-09-08
  (TIFFPOLISH).** `compression/jpeg/encode.rs` is now a 334-line mapping from a
  TIFF `CodecContext` onto `oxiarc_jpeg::EncodeOptions`, driving
  `Encoder::{write_tables_only, encode_scan_only, encode}`; the 659-line local
  baseline encoder is gone. Tag 347 was checked byte-identical against the old
  encoder at qualities 1/10/25/50/75/95/100 for gray, YCbCr 4:2:2, RGB and CMYK
  *before* the swap, and is now checked byte-identical against `tiffcp -c jpeg`
  in `tiff_oracle_codecs.rs`. `compression::jpeg::{encode, shared_tables}`
  is unchanged as the seam.
- **`compat::decoder::IfdDecoder` / `compat::decoder::ifd::{Value, Entry}`**
  (critique.md section 2.11's export list) were not built as separate types:
  `compat::tags::ValueBuffer` (`= crate::Value`) and `Decoder::tag_iter`
  cover the same ground (`image` 0.25.10 never uses `IfdDecoder` directly),
  and duplicating the native `Value`/`Entry` decode logic behind a second
  type would have broken this module's "thin adapter only" rule for no
  concrete call site.
- **`compat::decoder::TiffCodingUnit`** exists (`Strip(u32)` / `Tile(u32)`,
  per critique's export list) but `image_coding_unit_layout` /
  `read_coding_unit_bytes` were not wired to it — no concrete caller needs
  them and the crate's own `read_chunk`/`chunk_data_dimensions` already cover
  per-chunk access.
- **Corrupt-input sweeps for BigTIFF/IFD**: already present before this
  track started (`tests/corrupt_no_panic.rs`'s `gray8_bigtiff` fixture is
  exercised by every sweep; `e18`/`e20` cover IFD count overflow and loop
  termination) — verified, not re-built.

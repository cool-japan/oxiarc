# oxiarc-jpeg - Development Status (v0.4.2, in progress)

Program context: root `TODO.md`, "Phase 8", item **W1-F1** (decoder). The
encoder (**W1-F2**) and the arithmetic entropy coder (**W1-F3**) are separate
items in the same wave and land in this crate afterwards.

## Completed (W1-F1, decoder)

### Parsing
- [x] Marker table (T.81 B.1) with fill-byte skipping, stuffed-byte
      tolerance between segments and standalone-marker handling
- [x] Segment scanner shared by the decoder and `TableSet::parse`
- [x] `SOF0`/`1`/`2`/`3`/`9`/`10`/`11` classification; `SOF5`/`6`/`7`/`13`/
      `14`/`15` and `DHP`/`EXP` report `Unsupported::Hierarchical`
- [x] `DQT` (`Pq` 0 and 1), `DHT`, `DRI`, `DNL`, `SOS`, `DAC`
- [x] `DAC` parse-and-store with the T.81 defaults (`L = 0`, `U = 1`,
      `Kx = 5`) and byte-exact re-emission, so W1-F3 needs no parser change
- [x] `APP0` JFIF/JFXX, `APP1` EXIF and XMP, `APP2` ICC chunk reassembly with
      ordering/duplicate/gap validation and a 64 MiB cap, `APP14` Adobe,
      `COM`; every `APPn`/`COM` retained verbatim for passthrough
- [x] `DNL`-resolved heights: a `SOF` with `Y == 0` is resolved by looking
      ahead past the first scan's entropy data

### Entropy decoding
- [x] 64-bit-accumulator bit reader with `0xFF 0x00` de-stuffing, fill-byte
      runs, marker detection and libjpeg's zero-fill past the segment end,
      with a count of *consumed* fabricated bits so truncation is detectable
- [x] Canonical Huffman tables (Annex C.2) with a 9-bit fast lookup;
      over-subscribed tables rejected, under-subscribed tables accepted
- [x] Baseline and extended sequential (`SOF0`/`SOF1`), 8- and 12-bit,
      interleaved and non-interleaved scans
- [x] Progressive (`SOF2`), Annex G: DC first, DC refine, AC first with EOB
      runs, AC refine with the full correction-bit / zero-run / EOB-run
      interaction
- [x] Lossless (`SOF3`), Annex H: predictors 1-7, point transform,
      precision 2..=16, `Nf > 1` with both interleaved and non-interleaved
      scans, MCU padding samples decoded and discarded
- [x] Restart markers: any `RSTn` index accepted, DC predictors and EOB runs
      reset, forward resync when the expected marker is missing
- [x] One to four components, every sampling factor in `1..=4`

### Reconstruction
- [x] `jpeg_idct_islow`-exact inverse DCT with both DC-only shortcuts and
      `PASS1_BITS` switching to 1 above 8-bit samples
- [x] libjpeg's three fancy upsamplers (`h2v1`, `h1v2`, `h2v2`) with their
      asymmetric rounding and the `downsampled_width > 2` guard; replication
      for every other ratio, including non-integer ones
- [x] Fixed-point YCbCr to RGB with libjpeg-turbo's five-decimal constants,
      CMYK passthrough with the Adobe-keyed inversion
- [x] YCCK to CMYK (`APP14` transform 2), verified end to end over a real
      four-component stream. **Diverges from libjpeg deliberately** -- see
      the note below; this is the one colour path with no reference encoder
      to settle the convention
- [x] Row-at-a-time output: one plane allocation per decode, no per-row
      allocation, `u8` and `u16` destinations, arbitrary row stride

### API
- [x] `Decoder<R: Read>`: `read_info`, `info`, `frame_header`, `decode`,
      `decode_into`, `decode_into_strided`, `decode_u16`, `decode_into_u16`,
      `decode_into_u16_strided`, `output_buffer_size`, `pixel_format`,
      `load_tables`, `was_truncated`, `jfif`, `adobe`, `exif`, `xmp`,
      `icc_profile`, `comments`, `app_segments`, `into_inner`
- [x] `TableSet::{parse, emit, merge_from, is_empty}`, `TablesMode`
- [x] `decode_abbreviated`, `decode_abbreviated_into`,
      `decode_abbreviated_into_u16`
- [x] `tiff::{parse_jpeg_tables, build_jpeg_tables, merge_jpeg_tables}`
- [x] `DecodeOptions` (limits, output colour space, `raw_components`,
      upsampling mode, `tolerate_truncated`), `DecodeLimits`
- [x] `ImageInfo` with per-component `(id, h, v, quant_table)`, JFIF/Adobe
      flags and `(Hmax, Vmax)` subsampling
- [x] `From<JpegError> for OxiArcError`

### Testing
- [x] `tests/jpeg_oracle.rs` (`jpeg-oracle`): byte identity with
      `djpeg -dct int` across grayscale, 4:4:4, seven subsampling ratios in
      fancy and box modes, progressive, an AC-refinement scan script,
      restart intervals, optimized Huffman tables and 12-bit frames;
      exact lossless round-trips for predictors 1-7 and point transforms,
      interleaved and non-interleaved (`Ns = 1` per component over an
      `Nf = 3` frame) colour scans;
      an assertion that `djpeg`'s SIMD and `JSIMD_FORCENONE=1` C paths agree
      (otherwise every parity claim would be vacuous); Pillow tolerance
      cross-check
- [x] `tests/tiff_abbreviated.rs`: real `tiffcp -c jpeg`/`jpeg:r` fixtures
      pulled apart with `tifffile`, every strip decoded against the
      `JPEGTables` tag and checked against Pillow; `JPEGTables` reproduced
      byte for byte; component-ID colour heuristics; a 1-row strip with
      `V = 2` chroma; a four-component `photometric='separated'` fixture
      decoded end to end, proving libtiff's CMYK strips carry no `APP14`
      and must **not** be inverted, plus the `APP14` half of the same rule
- [x] `tests/corrupt_no_panic.rs`: truncation at every offset, dense
      single-byte corruption, segment-length mutation, structural attacks,
      a scan bomb and 2 000 pseudo-random buffers
- [x] Vacuity guards: every parity test counts its comparisons and fails
      rather than passing when `cjpeg` produced no fixture, and the libtiff
      tests treat a fixture failure as an error once the tools are known
      present. Both rules exist because a silent skip once hid a real gap
- [x] `tests/decode_api.rs`, `tests/proptest_decode.rs`
- [x] `benches/decode_bench.rs` (criterion)

## Not in W1-F1 (owned by other items)

- [ ] **W1-F2** encoder: baseline and extended 12-bit, standard and
      optimized Huffman, quality-scaled tables, box downsampling, edge
      replication, restart intervals, JFIF/Adobe headers, progressive,
      lossless, `write_tables_only` / `encode_scan_only`.
      `QuantTable::scaled_for_quality`, `quality_scaling_factor`,
      `natural_to_zigzag`, `TableSet::emit` and the Annex K tables in
      `tables` are already in place for it.
- [ ] **W1-F3** arithmetic coding (`SOF9`/`10`/`11`): the `DAC` segment is
      parsed and stored, `EntropyCoding::Arithmetic` is reported by
      `ImageInfo`, and `decode` returns
      `Unsupported::ArithmeticCoding` until the entropy coder lands.
- [ ] **W1-F3** OJPEG (TIFF `Compression = 6`) reconstruction helpers.

## Deliberately out of scope

- Hierarchical JPEG (`SOF5`/`6`/`7`/`13`/`14`/`15`). No reference encoder
  produces it, so there is nothing to test against; it returns a named
  `Unsupported` error.
- Reduced-scale decode (`scale_denom`, `djpeg -scale M/N`). Not named in the
  W1-F1 contract; the scaled IDCT variants are follow-on work.
- EXIF interpretation, ICC application, orientation, resizing. This crate
  decodes JPEG and hands metadata back verbatim.

## Known behaviour worth writing down

- `Decoder<R: Read>` buffers its source before parsing, bounded by
  `DecodeLimits::max_input_bytes` (1 GiB by default). Progressive frames
  revisit every block once per scan, so a streaming parse would have to
  buffer the same bytes anyway; slice callers can use
  `decode_abbreviated_into` and skip the copy.
- `pub mod sample` is deliberate public API, not leftover test scaffolding:
  it carries the embedded abbreviated pair that the doctests, the benches
  and `oxiarc-tiff`'s own tests decode. Removing it breaks those doctests.
- Component planes are stored as `u16` regardless of precision. That costs
  one extra byte per sample for 8-bit frames and buys one code path instead
  of two; the output stage is specialised for `u8` and `u16` destinations.
- The progressive coefficient buffer is `i32` per coefficient rather than
  `i16` at 8-bit precision. It is bounded by
  `DecodeLimits::max_coefficient_bytes`.
- Out-of-range IDCT reconstructions are clamped rather than wrapped through
  libjpeg's range-limit table (see `idct::islow`'s module documentation).
- **YCCK output is the complement of libjpeg's.** `ycck_cmyk_convert` emits
  `(MAX - R, MAX - G, MAX - B, K)` -- CMY complemented, `K` passed through.
  This crate applies the Adobe inversion uniformly to all four channels, so
  it emits `(R, G, B, MAX - K)`. libjpeg's own output is internally
  inconsistent (Adobe stores all four channels inverted, so passing `K`
  through leaves it on the opposite convention from `CMY`), which is why
  Pillow post-inverts everything through its `CMYK;I` raw mode. Uniform
  treatment is the self-consistent choice, but no reference encoder produces
  a YCCK file with known ink values, so this is a reasoned decision rather
  than a measured one. `raw_components` bypasses it entirely and is what
  `oxiarc-tiff` uses. Pinned by
  `tiff_abbreviated::oracle::ycck_transform_2_converts_all_four_planes`,
  which asserts the K channel is inverted so the code and this note cannot
  drift apart silently.
- CMYK output applies the Adobe inversion **iff** an `APP14` marker was seen,
  which is what makes `libtiff`'s non-inverted `Compression = 7` CMYK strips
  and Photoshop's inverted standalone CMYK JPEGs both come out right.
  `raw_components` bypasses the question entirely.

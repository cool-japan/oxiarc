# oxiarc-jpeg - Development Status (v0.4.2, in progress)

Program context: root `TODO.md`, "Phase 8", items **W1-F1** (decoder),
**W1-F2** (encoder) and **W1-F3** (arithmetic coding, OJPEG, `rayon`, fuzz
seeds, benches). All three have landed in this crate.

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
      fancy and box modes, the odd ratios 3x1/1x3/3x2/2x3, per-component
      asymmetric sampling (`-sample 2x2,1x1,2x1` and five more, where Cb and
      Cr select different upsamplers in one image), progressive, progressive
      grayscale, progressive with restart markers, an AC-refinement scan
      script, restart intervals, optimized Huffman tables and 12-bit frames
      in grayscale **and** colour (plus 12-bit with restarts, 12-bit 3x1 and
      12-bit `-optimize`);
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
      a scan bomb and 2 000 pseudo-random buffers, over a corpus of the six
      embedded samples **plus** the five multi-MCU streams in `tests/data`
      (progressive, baseline + `DRI`, progressive + `DRI`, 12-bit, lossless).
      The embedded samples are single-MCU 8-bit baseline, so without those
      five the progressive coefficient buffer, the restart resynchroniser,
      the 12-bit path and the lossless predictors never saw a malformed byte
- [x] `tests/fixture_streams.rs`: the five `tests/data` streams decoded
      intact and asserted to be the frames `tests/data/README.md` claims (so
      the sweep above cannot go vacuous); restart resynchronisation asserted
      by requiring that damage confined to restart interval 0 leaves the last
      MCU bit-identical to a clean decode, for both a sequential and a
      progressive frame; and a hand-built `SOF2` stream that puts an `EOBRUN`
      across a restart marker, which is the only way to test T.81 G.1.2.2's
      reset rule -- a conforming encoder always flushes its EOB run before
      `RST`, so no `cjpeg` output can distinguish the two behaviours
- [x] Vacuity guards: every parity test counts its comparisons and fails
      rather than passing when `cjpeg` produced no fixture, and the libtiff
      tests treat a fixture failure as an error once the tools are known
      present. Both rules exist because a silent skip once hid a real gap
- [x] `tests/decode_api.rs`, `tests/proptest_decode.rs`
- [x] `benches/decode_bench.rs` (criterion)

## Completed (W1-F2, encoder)

- [x] `jpeg_fdct_islow`-exact forward DCT, with libjpeg's `PASS1_BITS` switch
      (2 at eight bits, 1 above) and its `jcdctmgr` quantiser: divide by
      `q << 3`, round half away from zero, `DIVIDE_BY`'s zero shortcut.
- [x] Forward RGB to YCbCr with the asymmetric `ONE_HALF - 1` chroma rounding,
      RGB to grayscale, and CMYK to YCCK.
- [x] MCU edge replication (last real row and column), and the separate
      dummy-block rule: whole blocks past the real grid are zero with the DC
      **copied** from the previous block in MCU order.
- [x] Chroma decimation at 4:4:4 / 4:2:2 / 4:4:0 / 4:2:0 / 4:1:1 and any exact
      integer ratio, with libjpeg's **alternating** rounding bias (0,1 for
      `h2v1`; 1,2 for `h2v2`), plus `-smooth N` input smoothing and its
      context-rows padding rule.
- [x] Quality 1..100 through `jpeg_quality_scaling`, `force_baseline`, and the
      dynamic `Pq = 1` switch when any quantiser exceeds 255.
- [x] Standard Annex K.3 tables and generated ones: libjpeg's
      `jpeg_gen_optimal_table` including the frequency-1 pseudo-symbol and the
      larger-index tie-break. Generated tables are forced for progressive,
      lossless and precisions above eight, as libjpeg forces them.
- [x] Bit writer: 1-bit tail padding, `0xFF 0x00` stuffing, `RSTn` cycling.
- [x] Baseline (`SOF0`) and extended (`SOF1`) sequential, 8- and 12-bit, with
      the real `is_baseline` predicate rather than a precision test.
- [x] Progressive (`SOF2`): DC first, DC refine, AC first with EOB runs, AC
      refine with buffered correction bits; libjpeg's default scan scripts
      (both the ten-scan YCbCr one and the general one, whose DC refinement
      libjpeg-turbo moved before the final AC pass) and a custom scan-script
      API with full validation.
- [x] Lossless (`SOF3`): predictors 1-7, point transform, 2..=16 bit,
      restart-interval prediction reset, `SSSS = 16`.
- [x] Restart intervals in MCUs or MCU rows, resolved per scan.
- [x] `JFIF` `APP0` with density units, Adobe `APP14`, EXIF / XMP / ICC
      (auto-chunked `APP2`) / `COM` passthrough.
- [x] Grayscale, YCbCr, RGB (no transform), CMYK and YCCK, with libjpeg's
      component identifiers and table assignments per colour space.
- [x] `Encoder<W: Write>` with builder setters, `encode`, `encode_u16`,
      `encode_planar`, `write_tables_only`, `encode_scan_only`, `finish`;
      `encode_to_vec`, `encode_to_vec_with_options`,
      `encode_u16_to_vec_with_options` and `table_set` free functions.
- [x] `compat::JpegEncoder`, shaped like `image::codecs::jpeg::JpegEncoder`.
- [x] `tests/encode_oracle.rs` (`jpeg-oracle`): byte identity with `cjpeg` for
      every subsampling ratio, quality, restart spelling, `-optimize`,
      `-smooth`, `-progressive` (default and custom scripts), `-precision 12`
      and `-lossless`; `djpeg` agreement on our own output; Pillow reads it;
      and the `JPEGTables` tag generated from scratch matching `tiffcp`'s.
- [x] `tests/encode_api.rs`: every process x colour space x awkward shape,
      every subsampling ratio, lossless exactness at 2/4/8/12/16 bits,
      restart cycling, marker policies, the Adobe-keyed CMYK inversion, and
      three proptest properties.
- [x] `benches/encode_bench.rs` (criterion), with a `reference/` group that
      times `cjpeg` on the same machine.

## Completed (W1-F3: arithmetic coding, OJPEG, rayon, fuzz seeds, benches)

- [x] **Arithmetic coding**, feature `arithmetic` (default on): the QM coder
      of T.81 Annex D in both directions, `DAC` conditioning, restart
      handling, and all three processes. `src/arith/{qm,decoder,encoder}.rs`
      plus `src/decoder/arith.rs` and `src/encoder/arith.rs`.
  - `SOF9` and `SOF10`: **byte-identical to libjpeg both ways** — our decode
    equals `djpeg -dct int`'s across every ratio, quality, restart spelling,
    grayscale, 12-bit and progressive script, and our encode equals
    `cjpeg -arithmetic -dct int`'s including the per-scan `DAC` segments.
  - `SOF11`: T.81 H.1.2.3's two-dimensional model (158 bins,
    `S0 = 20*cat(Da) + 4*cat(Db)`, `X1 = 100` or `129`). **No reference
    implementation exists** — libjpeg-turbo refuses `-lossless -arithmetic`
    and its `jdarith.c` has no lossless path — so the gate is exact
    round-tripping across predictors 1..7, precisions 8/12/16, point
    transforms, restart intervals and custom conditioning. Said plainly
    rather than dressed up as verification.
- [x] **OJPEG** (TIFF `Compression = 6`) read support in
      `oxiarc_jpeg::tiff`: `OJpegTags`, `OJpegGeometry`, `reconstruct_ojpeg`,
      `decode_ojpeg`, `decode_ojpeg_into`. Scan-then-fill over all three
      spellings; flavour (a) checked against libtiff, (b) and (c)
      constructively against `djpeg` (libtiff cannot read them at all).
      Writing `Compression = 6` is out of scope, as it is for libtiff.
- [x] **`rayon` feature**: entropy coding across restart intervals, in
      parallel, on both sides and for both coders. Byte-identical either way,
      pinned by a digest in `tests/parallel.rs` measured in the serial
      configuration and asserted in both. Measured on 1024x1024 4:2:0 with a
      marker per MCU row, eight threads, fastest of a hundred samples:
      decode 1.65x (Huffman) and 2.66x (arithmetic), encode 1.39x and 2.22x.
      Arithmetic scans reached the parallel decoder only after the scan
      dispatch was reordered — the `arithmetic` branch of `decode_one_scan`
      returned before the `rayon` branch was ever consulted, so `SOF9` was
      silently serial. `tests/parallel.rs` now covers a partial last MCU row
      (320x250 and 314x250), which is the one band geometry the merge has to
      clip.
- [x] **Fuzz seeds**: `tests/fuzz_seeds.rs` builds ~130 seeds covering every
      process and both coders, decodes every one, and writes them out only
      when `OXIARC_JPEG_FUZZ_SEEDS` is set. Nothing is committed.
- [x] **No-panic sweeps** extended to five arithmetic fixtures the test
      builds itself. Two real defects were found this way and fixed: a
      missing `validate_dct_scan` on the arithmetic path (a corrupt `Se`
      indexed the zig-zag table out of bounds) and a missing modulo-2^16
      reduction in lossless arithmetic encoding (a 16-bit frame with
      neighbouring 0 and 65535 samples ran the magnitude chain off the end of
      its statistics area).

## Not in W1-F1 / W1-F2 / W1-F3 (owned by other items)

- [ ] Parallelise the output stage (upsampling and colour conversion). They
      are per-row and need no new synchronisation, and they are why the
      `rayon` Huffman decode gain is 1.65x rather than something closer to
      the thread count: entropy decoding is only about a third of a baseline
      decode, and the band merge copies one plane. The arithmetic rows gain
      more (2.66x) because the QM coder is most of that decode.
- [ ] Encoder performance: a large baseline frame runs at 0.42x of `cjpeg`
      because libjpeg-turbo uses NEON for colour conversion, the forward DCT
      and Huffman encoding. Driving the coefficient pipeline one MCU row at a
      time, so a large frame's coefficients never leave cache, is the obvious
      next step; the whole-image coefficient buffer is required only for
      progressive and for generated tables.

## Deliberately out of scope

- Hierarchical JPEG (`SOF5`/`6`/`7`/`13`/`14`/`15`). No reference encoder
  produces it, so there is nothing to test against; it returns a named
  `Unsupported` error.
- Reduced-scale decode (`scale_denom`, `djpeg -scale M/N`). Not named in the
  W1-F1 contract; the scaled IDCT variants are follow-on work.
- EXIF interpretation, ICC application, orientation, resizing. This crate
  decodes JPEG and hands metadata back verbatim.

## Known behaviour worth writing down

- **A truncated arithmetic scan decodes to noise, not to an error.** T.81
  D.2.6 lets the decoder read zeros past the last coded byte, and measured
  over the whole `cjpeg -arithmetic` corpus a *conforming* stream can need as
  many as 94 of those fabricated bytes — so the count is not a truncation
  signal. Only an impossible decision sequence
  (`JpegError::InvalidArithmeticCode`) or a missing `RSTn` is reported.
  libjpeg has exactly the same property.
- **libtiff's own YCbCr conversion for an OJPEG strip is not libjpeg's.**
  Measured on a 64x48 checkerboard: peak difference 104, with 90 % of samples
  more than 2 apart. Grayscale agrees within 2. `oxiarc_jpeg::tiff` therefore
  applies no colour transform at all and leaves the question to the
  container.


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
- `ImageInfo::restart_interval` reports the most recent `DRI` seen *so far*.
  libjpeg and libtiff write `DRI` in the scan header, after `SOF`, and
  `read_info` stops at `SOF`, so the value is normally 0 there and correct in
  the `ImageInfo` that `Decoder::info()` returns after `decode()`. Callers
  holding tables out of band (TIFF tag 347) should read
  `TableSet::restart_interval` instead.
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

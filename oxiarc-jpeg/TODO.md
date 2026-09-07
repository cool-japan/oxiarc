# oxiarc-jpeg - Development Status (v0.4.2, in progress)

Program context: root `TODO.md`, "Phase 8", items **W1-F1** (decoder),
**W1-F2** (encoder) and **W1-F3** (arithmetic coding, OJPEG, `rayon`, fuzz
seeds, benches). All three have landed in this crate. Reduced-/enlarged-scale
decode and the `zune_jpeg`/`jpeg-decoder` compat facades (design report
§7.4/§8, tracked as **JPEGSCALE**, not a numbered Phase 8 wave item) have
landed as follow-on work in the same cycle — see below.

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

## Completed (JPEGSCALE: reduced-/enlarged-scale decode, compat facades)

- [x] **`DecodeOptions::scale: Scale`** — libjpeg's `-scale M/N` with `N`
      fixed at 8; `Scale::new(M)` for `M` in `1..=16`, plus `FULL`/
      `ONE_HALF`/`ONE_QUARTER`/`ONE_EIGHTH` constants. Ignored for lossless
      frames (`SOF3`/`SOF11`), matching libjpeg's own hardwired "no scaling"
      there. `ImageInfo::scaled_width`/`scaled_height` (`ceil(dim * M / 8)`,
      libjpeg's own formula) drive `Decoder::output_buffer_size` and every
      decode entry point; `width`/`height` keep their pre-existing
      SOF/DNL-resolved, unscaled meaning.
- [x] **`idct/scaled.rs`**: `idct_1x1_into`/`idct_2x2_into`/`idct_4x4_into`,
      bit-identical ports of libjpeg-turbo's `jidctred.c`
      (`jpeg_idct_1x1`/`_2x2`/`_4x4`), sharing `idct/islow.rs`'s
      `coefficient()` narrowing and `descale()`. `idct_general_into`: this
      crate's own kernel for the twelve `M` values libjpeg gives a dedicated
      hand-derived fixed-point routine and this crate does not (`{3, 5, 6, 7,
      9..=16}`) — a direct `f64` evaluation of T.81 A.3.3's cosine basis at
      `n` output positions per axis, truncating to the lowest `n` frequencies
      for `n < 8` (matching `jidctint.c`'s own separate `n < 8` family, *not*
      `jidctred.c`'s full-spectrum-with-cancellation approach — the two
      disagree on an AC-heavy block at `n` in `{1, 2, 4}` by construction,
      pinned by a test so the difference cannot be "fixed" away by accident).
      `idct_scaled_into` dispatches on output size: `8` → the untouched,
      unaffected `idct_islow_into`; `1`/`2`/`4` → the ported kernels;
      otherwise → the general kernel.
- [x] Per-component **`_DCT_scaled_size` bump** (`decoder/planes.rs`
      `component_output_size`, `component_downsampled_dim`, ported from
      `jdmaster.c::jpeg_calc_output_dimensions`): a subsampled component's own
      output block size grows past the requested `M` when doing so brings it
      to the frame's full resolution, letting the upsampler skip a real
      resample. `OutputPlan::new`'s fancy-upsampling gate
      (`do_fancy = fancy_upsampling && planes.min_output_size() > 1`) and
      per-component `h_in_group`/`v_in_group` ratio are `jdsample.c`'s exact
      rules, empirically confirmed against `djpeg -scale M/8` vs
      `-scale M/8 -nosmooth` before any kernel was written.
- [x] **Byte parity for `M` in `{1, 2, 4, 8}`**: `tests/scale_oracle.rs`
      (feature `jpeg-oracle`, self-skipping, records `cjpeg -version`) checks
      `djpeg -dct int -scale M/8` against baseline **and** progressive
      frames, 4:4:4 / 4:2:0 / 4:2:2 sampling and both 8x8-aligned and odd
      (37x29, 18x11) dimensions. Verified green on this machine
      (libjpeg-turbo 3.1.4.1) with every comparison counted
      (`require_comparisons`, the vacuity guard the F1 review pass added).
- [x] **Tolerance check for the other twelve `M` values**, same file: measured
      over 216 encoder/scale/sampling configurations and 2 141 046 samples,
      **peak error 3, MSE 0.0531** against `djpeg`. (Before the general
      kernel's `jidctint.c`-style truncation was found to be the right
      convention for `n < 8`, the same matrix measured peak error 134, MSE
      55.5 — recorded so nobody re-derives that mistake.)
- [x] Self-consistency tests in `idct/scaled.rs` (no external tool needed):
      `idct_general_into` at `n = 8` agrees with `idct_islow_into` within
      ±1 LSB; a DC-only block reconstructs to the flat level
      `dc_dequantised / 8` for every `n` in `1..=16` (pins the general
      kernel's normalisation, independent of `n`, exactly); the ported and
      general kernels are pinned to agree on a DC-only block at `n` in
      `{1, 2, 4}` and pinned to *not* necessarily agree on an AC-heavy one
      (so the algorithmic difference cannot regress silently in either
      direction); every kernel clamps rather than panics at the most extreme
      coefficient/quantiser the narrowing admits, at every `n` in `1..=16`.
- [x] `tests/scale_api.rs`: `ceil(w * M / 8)` for odd dimensions (down to
      1x1) at every `M` in `1..=16`, cross-checked against an
      independently-written `expected_dim` rather than the crate's own
      private `scaled_dim` helper; `output_buffer_size`/
      `decode_into_strided` agreement, including that a stride wider than
      the row leaves the inter-row padding untouched; `Scale::FULL` byte-
      identical to no scale option at all (the regression guard for "the
      whole geometry change is a no-op at the default scale"); `scale` is a
      no-op on a lossless frame; every `M` in `1..=16` decodes without
      panicking on both processes and all three common sampling ratios.
- [x] Adversarial coverage: `corrupt_no_panic.rs` gained `probe_scaled` and
      two sweeps (truncation at every offset, and dense single-byte
      corruption) that decode at `M` in `{1, 2, 3, 8}` or the full `1..=16`
      range rather than only at native resolution; `adversarial.rs`'s
      byte-insertion/deletion fuzzer now also decodes each mutated buffer at
      one of `{1, 2, 5, 8}` per iteration (an exact-kernel, a
      component-bump, a general-kernel and the unscaled case).
- [x] **`compat::zune`** (new; `compat.rs` split into `compat/{mod,zune,
      jpeg_decoder}.rs` to stay well under the line-count target):
      `JpegDecoder::new(bytes: &[u8])`, `new_with_options`, `decode`,
      `decode_headers`, `info`, `dimensions`, `output_buffer_size`,
      `input_colorspace`, `output_colorspace`, `set_options`, `icc_profile`,
      `exif` — read directly from real zune-jpeg 0.5.15's source
      (`~/.cargo/registry/src/*/zune-jpeg-0.5.15`), not from memory.
      Output-colourspace enum: `Rgb`/`Rgba`/`Luma`/`YCbCr`. `Rgba` synthesises
      an opaque alpha byte (this crate's `ColorSpace` has no native alpha);
      `YCbCr` maps to `DecodeOptions::raw()`; a single-component source
      expands to whatever multi-channel space was requested rather than
      erroring, matching `zune_jpeg`'s own behaviour; a colour-space request
      this crate's pipeline cannot satisfy (e.g. `Rgb` from a CMYK source) is
      a named `Unsupported(ColorTransform(_))` error.
- [x] **`compat::jpeg_decoder`**: `Decoder::new(reader: R)`, `read_info`,
      `info`, `decode`, `icc_profile`, `exif_data`, with `ImageInfo {width,
      height, pixel_format, coding_process}` and `PixelFormat::{L8, L16,
      Rgb24, Cmyk32}` / `CodingProcess::{DctSequential, DctProgressive,
      Lossless}` matching real jpeg-decoder 0.3.2's shapes exactly (read
      from `~/.cargo/registry/src/*/jpeg-decoder-0.3.2`). One deliberate
      signature change: real `jpeg_decoder::Decoder::info()` is infallible
      and panics for a component count outside `{1, 3, 4}`; this crate's
      no-panic policy means `Decoder::info` returns
      `Result<Option<ImageInfo>, JpegError>` instead, reporting
      `Unsupported(ComponentCount(_))` where the real crate would abort.
- [x] Both facades are feature-free (always compiled) and every method named
      in the task's list is exercised by at least one test, not merely
      constructed and decoded once.
- [x] `benches/decode_bench.rs` gained a `scaled_decode` group: `M` in
      `{1, 2, 4, 8}` over 4:2:0 and 4:4:4 512x512 sources, with a
      `reference/…` row timing `djpeg -dct int -scale M/8` on the same
      encoded file (mirrors `encode_bench.rs`'s `reference/…` convention for
      `cjpeg`). No perf table is published in the README this cycle: the
      session's machine measured a system load average above 80 from
      unrelated concurrent builds, and two runs of the same case disagreed by
      more than an order of magnitude — noise, not signal. See the README's
      "Reduced- and enlarged-scale decode" performance note.

## Completed (JPEGSCALE-verify: adversarial verification pass)

An independent verification pass over the JPEGSCALE work above found and
fixed three real defects; each has a regression test that was confirmed to
fail on the pre-fix code.

- [x] **`rayon` band merge ignored the scaled block size (correctness, all
      component counts, both entropy coders).** `decoder/parallel.rs`'s
      `plan_bands` placed each band's rows at `mcu_row * Vi * 8` instead of
      `mcu_row * Vi * _DCT_scaled_size`, so with the `rayon` feature on,
      every band but the first landed in the wrong plane rows at every
      `Scale` except `Scale::FULL`. Measured before the fix on a 256x256
      4:2:0 frame with `RestartInterval::Mcus(16)`: **2686 of 3072 output
      bytes wrong at `M = 1`**, 10736/12288 at `M = 2`, 42947/49152 at
      `M = 4`, 0/196608 at `M = 8` — the same magnitude for grayscale, CMYK
      and arithmetic coding, and 0 everywhere in a `--no-default-features`
      build, which isolated it to the parallel path. This also made
      `lib.rs`'s feature-table claim that the `rayon` output "is
      byte-identical with and without it" false; it is true again.
      `plan_bands` now reads the destination `Planes`' own `output_sizes()`
      rather than recomputing from `Scale`, so the two cannot disagree.
      Why every existing gate missed it: `tests/scale_oracle.rs`'s fixtures
      are 64x64 or smaller and carry no `DRI`, so the band planner declined
      all of them, and `parallel.rs`'s own differential suite ran only at
      `Scale::FULL`, where the wrong multiplier is also `8`. Regression
      tests: `parallel.rs`'s
      `bands_land_on_the_right_plane_rows_at_every_scale` (serial vs
      parallel planes, `M` in `1..=16` x 3 ratios x both coders, asserting
      all 16 scales really split), `scale_api.rs`'s
      `restart_markers_do_not_change_a_scaled_decode` (hermetic, always
      runs), `scale_oracle.rs`'s
      `restart_marker_streams_are_byte_identical_to_djpeg_at_every_exact_scale`
      (320x256 `cjpeg -restart` fixtures, colour and grayscale), and the
      pre-existing `a_truncated_scan_reports_the_same_thing_either_way`
      extended to three reduced scales.
- [x] **`compat::jpeg_decoder` reported a pixel format its own `decode()`
      did not produce.** `PixelFormat`'s only wide variant is `L16`, which is
      single-component, but `info()` answered `Rgb24` for a twelve-bit
      *colour* frame while `decode()` returned big-endian `u16` samples —
      measured on a 9x7 twelve-bit RGB frame, `decode()` gave 378 bytes where
      `info()` implied 189, so a caller sizing a buffer the way the module's
      own example does read half the image. Both now report
      `Unsupported(SamplePrecision(_))` in step with each other; the frame is
      still decodable through `Decoder::inner_mut` +
      `oxiarc_jpeg::Decoder::decode_u16`, and twelve-bit *grayscale* still
      resolves to `L16` and matches `pixel_bytes()`. `inner_mut`'s rustdoc
      also claimed it reached scale/limits/raw-components settings, which it
      cannot (those are fixed at construction) — corrected to name the entry
      points it does reach.
- [x] **`compat::zune`'s three accessors disagreed with each other.**
      `output_buffer_size()` computed its width from the requested
      `ColorSpace`'s nominal component count, but `ColorSpace::YCbCr` is a
      raw passthrough of *the source's* components: measured on a 9x7
      fixture, it reported 189 bytes where a grayscale decode produced 63,
      and 189 where a CMYK decode produced **252** — i.e. it told a caller to
      allocate less than `decode()` returns, the one thing that method
      exists to prevent. It now reports exactly what `decode()` produces, and
      a new test pins `output_buffer_size() == decode()?.len()` over
      grayscale/RGB/CMYK sources x all four requests. Separately,
      `output_colorspace()` answered `Luma` for any one-component source
      "mirroring `zune_jpeg`" — it does not: real zune-jpeg 0.5.15 returns
      `self.options.jpeg_get_out_colorspace()` unmodified (the
      `jpeg_set_out_colorspace(Luma)` line in its `headers.rs` is commented
      out) and its `worker.rs` carries `(Luma, RGB)`/`(Luma, RGBA)` expansion
      arms, exactly like this facade's own `decode()`, which was returning
      three bytes per pixel while the accessor said `Luma`. Corrected, with
      the existing test strengthened to assert the accessor and `decode()`'s
      real length together.
- [x] **`idct_general_into` rebuilt its cosine basis for every 8x8 block**
      (performance): `8 * M` `f64::cos` evaluations plus a 1 KiB zeroed stack
      array per block — 24 per block at `M = 3`, 128 at `M = 16`, i.e. about
      147 000 transcendental calls for one 512x512 4:2:0 frame at `M = 3`,
      orders of magnitude more work than the transform they feed. Hoisted
      into a process-wide `OnceLock` table (17 x 8 x 16 `f64`, ~17 KiB).
      Bit-identical by construction and confirmed as such: the `djpeg`
      tolerance oracle reproduces **peak error 3, MSE 0.0531 over 2 141 046
      samples** exactly, unchanged, and a new unit test asserts the table
      equals `cosine_basis(n)` with `assert_eq!` on `f64` (bit equality, not
      an epsilon) for every `n` in `1..=16`. No timing figure is published,
      for the machine-contention reason recorded above.
- [x] **Twelve-bit scaled decode was never checked against `djpeg`.** The
      ported `jidctred.c` kernels take `PASS1_BITS = 1` above eight-bit
      samples, changing five shift amounts, and every fixture in
      `tests/scale_oracle.rs` was eight-bit — a wrong branch there would have
      produced a systematically wrong twelve-bit image with the whole suite
      green. `twelve_bit_scale_1_2_4_8_is_byte_identical_to_djpeg` closes it
      (grayscale, 4:2:0 colour and 4:4:4 progressive, `M` in `{1, 2, 4, 8}`,
      asserting `djpeg` really emitted `maxval 4095` so it cannot degrade to
      an eight-bit comparison). It passes as written — the kernels were
      already correct — and was confirmed non-vacuous by forcing
      `pass1_bits = 2`, which fails it immediately
      (`gray_12bit scale 2/8: sample 9 differs (ours 271, djpeg 272)`).
- [x] Coverage the verification pass added on top: `raw_components` decodes
      follow the scaled geometry at every `M` (the `Mode::Passthrough` arm of
      `OutputPlan`, which the colour-transform tests never reach);
      `decode_into_u16_strided` is bounded by the scaled geometry, leaves
      inter-row padding untouched and refuses a buffer one sample short; and
      `decode_abbreviated_into` (the TIFF `JPEGTables` entry point) threads
      `scale` through and reports it back.

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

# oxiarc-image - Development Status (v0.4.2, 2026-09-07)

See the root `TODO.md` section "Phase 8: HTTP Content-Coding, Incremental Inflate, and
Image Codecs" — track `W2-IMG`. This crate's own status below is the authoritative
per-crate detail; the root file only tracks the one-line summary.

## Completed Features (COMPLETE)

### Format sniffing and dispatch
- [x] `ImageFormat` — 15 variants (`image`-shaped); `can_decode`/`can_encode` name the
      three this crate actually implements
- [x] Magic-byte sniffing (`guess_format`) — PNG, JPEG, classic TIFF (both byte orders),
      BigTIFF (both byte orders), plus GIF/WebP/PNM/TGA/DDS/BMP/ICO/HDR/AVIF/OpenEXR/
      farbfeld/QOI recognised by name (not decoded)
  - [x] **Fixed in verification**: the AVIF row's mask was padded out to the signature's
        full 12 bytes with zeros, which makes the `ftypavif` brand test read
        `byte & 0x00 == b'f'` — unsatisfiable, so the row matched nothing and a real
        AVIF file was reported as "format could not be determined". Now `image` 0.25's
        own 4-byte `\xFF\xFF\0\0` mask (bytes past a mask's end match with an implicit
        `0xFF`). Both masked rows (WebP and AVIF) are pinned in both directions by
        `guess_format_sniffs_the_masked_rows_webp_and_avif` /
        `guess_format_does_not_over_match_the_masked_rows`
- [x] Extension lookup (`from_extension`/`from_path`, case-insensitive) and MIME-type
      lookup (`from_mime_type`/`to_mime_type`) for all 15 formats
- [x] `ImageReader<R: BufRead + Seek>` — `open`/`new`/`with_guessed_format`/`format`/
      `decode`/`into_dimensions`; `open` trusts the path extension (not content),
      `with_guessed_format` sniffs content and leaves the format unchanged on no match,
      matching `image::ImageReader`'s own contract
- [x] Top-level free functions: `open`, `load_from_memory`, `load_from_memory_with_format`,
      `image_dimensions`

### Typed pixel buffers
- [x] `Primitive` (`u8`/`u16`/`f32`) and `Pixel` (`Luma`/`LumaA`/`Rgb`/`Rgba`, each generic
      over `Primitive`) — owned-value traits only, no zero-copy `&[S] -> &P` cast
      (`#![forbid(unsafe_code)]`)
- [x] `ImageBuffer<P>` — `new`/`from_pixel`/`from_fn`/`from_raw`/`into_raw`/`as_raw`/
      `as_raw_mut`/`dimensions`/`width`/`height`/`get_pixel`/`get_pixel_checked`/
      `put_pixel`/`pixels`; `pixel_slice`/`get_pixel_mut_channels` are the zero-copy
      escape hatches in place of `image`'s `&Pixel`/`&mut Pixel`
- [x] Ten `image`-shaped type aliases (`RgbImage`, `Rgba16Image`, `GrayImage`, …)
- [x] `ColorType` (10 variants) and `ExtendedColorType` (29 + `Unknown(u8)`), with
      `bytes_per_pixel`/`bits_per_pixel`/`channel_count`/`has_alpha`/`has_color` and the
      `ColorType <-> ExtendedColorType` conversions

### `DynamicImage`
- [x] All ten variants (`ImageLuma8`/`ImageLumaA8`/`ImageRgb8`/`ImageRgba8`/…16/
      `ImageRgb32F`/`ImageRgba32F`)
- [x] `dimensions`/`width`/`height`/`color`/`has_alpha`
- [x] `to_rgba16`/`to_rgba8`/`to_rgb16`/`to_rgb8`/`to_luma8`/`to_luma16`/
      `to_luma_alpha8`/`to_luma_alpha16` conversions. Luma uses `image` 0.25's own
      sRGB / Rec. 709 integer weights (`SRGB_LUMA = [2126, 7152, 722]` over
      `SRGB_LUMA_DIV = 10000`, truncating), byte-identical to `image` for the four 8-bit
      variants; a 16-bit/float source narrows before luma here where `image` narrows
      after, so those six can differ by a level (documented on `to_luma8`). Every
      method's exact output samples for every variant are pinned by
      `tests/conversions.rs`.
- [x] `into_rgba8`/`into_rgb8`/`into_luma8`/`into_rgba16` (reuse the buffer when already
      the target variant)
- [x] `from_decoder` (assembles from any `ImageDecoder`) and the crate-private
      `from_decoded` (assembles from raw decoded bytes + `ColorType`)
- [x] `write_to`/`write_with_encoder`/`save`/`save_with_format`

### Codecs
- [x] `codecs::png::{PngDecoder, PngEncoder, CompressionType, FilterType}` — wraps
      `oxiarc_png::{Decoder, Encoder}` with `Transformations::EXPAND`; explicit 16-bit
      big-endian <-> native-endian swap on both directions (verified against
      `oxiarc_png::decode`'s own documented big-endian `Image::data`, not just
      facade-to-facade agreement)
- [x] `codecs::jpeg::{JpegDecoder, JpegEncoder}` — wraps `oxiarc_jpeg::{Decoder, Encoder}`;
      CMYK/YCCK/raw-component decode is a named `Unsupported` (no `ColorType` for it,
      matching `image`); 16-bit decode is `[u16]`-typed already, no swap needed
- [x] `codecs::tiff::{TiffDecoder, TiffEncoder}` — wraps `oxiarc_tiff::{Decoder, Encoder}`
      **natively** (not the `compat` feature — `oxiarc-tiff`'s `compat` is its own
      migration layer for `tiff`-crate consumers specifically; this facade is for
      `image`-crate consumers and calls `Decoder::read_image`/`read_image_rgba8`/
      `Samples` directly, per `oxiarc-tiff`'s own note to this track). Fast path for the
      ten clean `(ColorType, channels, SampleFormat)` combinations (`Gray`/`GrayA`/`Rgb`/
      `Rgba` at 8, 16, and — new this session — 32-bit IEEE float); every other TIFF
      (palette, CMYK, YCbCr, Lab, Multiband, or a 32-bit file whose declared
      `SampleFormat` is genuinely `Uint` rather than `IeeeFp`) falls back to
      `read_image_rgba8`, which already owns that colour math
  - [x] 32-bit float (`Rgb32F`/`Rgba32F`) round trip, end to end: `ImageSpec`'s
        `SampleFormat` override on encode, a `SampleFormat`-checked fast path on decode,
        `un_premultiply_f32` for the (rare, but real) `AssociatedAlpha`-declaring case —
        verified empirically (`tests/roundtrip.rs::tiff::{rgb32f,rgba32f}`,
        `src/codecs/tiff.rs`'s own `round_trip_32_bit_float_*` and
        `a_genuine_32_bit_integer_rgb_tiff_is_not_misread_as_float` unit tests), not
        assumed from reading `oxiarc-tiff`'s source alone
  - [x] Two `copy_from_slice` panics on untrusted decoder-output length (the
        `read_image_rgba8` fallback and the typed-`Samples` fast path) replaced with a
        named `ImageError::Decoding` — a lenient or truncated file can make the header's
        promised size and the decoder's actual output size disagree, and this crate's
        own "every decode failure is one of six `ImageError` variants, never a panic"
        guarantee did not previously hold for that specific case
  - [x] `TiffEncoder::write_image` buffer-length pre-validation (`ImageError::Parameter`
        on mismatch), matching the pattern `codecs::png`/`codecs::jpeg` already had —
        TIFF was the one codec without it
  - [x] **Fixed in verification**: `un_premultiply_f32` clamped its result to `1.0`,
        silently destroying an HDR highlight, while the straight-alpha path beside it
        copies out-of-range float samples through untouched. The file's `ExtraSamples`
        tag — invisible to the caller — should not decide whether values above `1.0`
        survive a decode. Clamp removed; pinned by `un_premultiply_f32_preserves_values_above_one`
        and by an end-to-end `AssociatedAlpha` float TIFF round trip

### Errors
- [x] `ImageError` (6 variants), `ImageResult`, `ImageFormatHint`, `DecodingError`,
      `EncodingError`, `ParameterError`(`Kind`), `LimitError`(`Kind`),
      `UnsupportedError`(`Kind`) — shaped after `image` 0.25's own six top-level variants
- [x] `From` conversions from every codec crate's own error type
      (`oxiarc_png::{DecodingError,EncodingError}`, `oxiarc_jpeg::JpegError`,
      `oxiarc_tiff::TiffError`), each routing an I/O failure to `ImageError::IoError` and
      a limits failure to `ImageError::Limits` before falling through to
      `Decoding`/`Encoding` — documented (in `error.rs`'s doc comments) where a single
      shared error enum on the codec side means an encode-time failure still reports as
      `Decoding` for JPEG/TIFF (PNG has separate `DecodingError`/`EncodingError` types,
      so does not have this limitation)

### Documentation and tests
- [x] Crate-level "Migrating from `image`" doc section (`src/lib.rs`) and README table,
      kept in sync
- [x] 13 doctests across `lib.rs`, `buffer.rs`, `format.rs`, `reader.rs`, `dynamic.rs`
      (`DynamicImage::from_decoder`), and one per codec module
      (`codecs::{png,jpeg,tiff}`'s `*Encoder`/`*Decoder` types)
- [x] `tests/roundtrip.rs` — every `ColorType` through PNG and TIFF (byte-exact), JPEG
      within a documented lossy tolerance; PNG's `Rgb32F`/`Rgba32F` named-`Unsupported`
      rejection; TIFF's `Cmyk8` encode-only case; JPEG's narrowing (`Rgba8`->`Rgb8`,
      16-bit->8-bit, CMYK encode-but-not-decode)
- [x] `tests/sniffing.rs` — magic-byte detection for all three real encodings, TIFF byte
      order, `ImageReader::with_guessed_format`, and the extension-vs-content
      disagreement on a deliberately mislabeled file
- [x] `tests/error_mapping.rs` — missing file, unrecognised/absent extension, truncated
      bytes per format, PNG float-write rejection, `Box<dyn std::error::Error>`
      propagation via `?`, and a `source()` chain check against a real hand-built
      malformed-JPEG-segment failure (not a truncation — a value-level error, so the
      `source()` chain is unambiguously exercised)
- [x] `tests/conversions.rs` (added in verification) — a golden table pinning the *exact*
      output samples of all eight `to_*` and four `into_*` conversions for all ten
      `DynamicImage` variants. Its job is to make a conversion rewrite prove it changed
      nothing: run it before and after any edit to `src/dynamic.rs`
- [x] `tests/adversarial.rs` (added in verification) — malformed-input hardening for all
      three codecs: truncation at every offset, single-byte corruption at every offset
      (three bit patterns), byte drops and inserts at every offset, 200 randomised
      truncate-plus-garbage mutations per format, degenerate 0-4 byte buffers,
      1/2/3/7/64-byte-chunk readers (whose output must equal a single-read decode
      exactly), hand-built PNG/TIFF headers declaring up to `u32::MAX` in both axes, and
      re-encoding whatever a corrupted decode produced through all three encoders.
      Anything that does decode is checked for dimension/sample-count agreement, so a
      silently half-filled buffer fails as loudly as a panic would

## Fixed during adversarial verification (see the `IMG-verify` handoff for the full
   write-up and the measurements behind each one)

- [x] **`guess_format` never recognised AVIF** — a dead magic-table row; see the sniffing
      section above
- [x] **Grayscale used BT.601 luma weights, not `image`'s sRGB / Rec. 709 ones** — pure
      red grayscaled to 76 where `image` gives 54, an 8.6 %-of-range systematic error in
      the headline call of a crate that advertises itself as an `image` drop-in. Now
      `(2126 R + 7152 G + 722 B) / 10000`, byte-identical to `image` for the four 8-bit
      variants. No JPEG output byte moved: `write_to(.., ImageFormat::Jpeg)` routes only
      *grayscale* sources through `to_luma8`, and both coefficient sets sum to exactly
      their divisor, so `luma(l, l, l) == l` under either — pinned by
      `a_grayscale_source_is_luma_weight_independent`
- [x] **The `DynamicImage` conversions were 3-6x slower than necessary** — per-pixel
      `get_pixel`/`put_pixel` with `f64` scaling, rewritten as per-variant slice loops
      with exact integer scaling (5.8x `Rgb8 -> to_rgba8`, 4.3x `Luma8 -> to_rgba8`,
      3.0x `Rgb16 -> to_rgba8` at 1024x1024, interleaved A/B, best of 25). Output
      identity is proven by `tests/conversions.rs` plus exhaustive equivalence tests over
      all 256 `u8` and all 65 536 `u16` sample values in `src/color.rs`
- [x] **`ImageDecoder::read_image` documented a panic it never performed**, and PNG
      returned `Ok(())` from an over-sized buffer leaving the tail unwritten. All three
      codecs now share `traits::check_read_buffer` and return `ImageError::Parameter` in
      both directions; the trait doc states this as a deliberate deviation from `image`
- [x] **A 9..15-bit JPEG decoded sixteen times too dark.** `decode_into_u16` writes the
      frame's own unscaled samples, so a 12-bit frame filled only `0..=4095` of a buffer
      this crate labels `ColorType::L16` (whose contract is `0..=65535`); `to_rgba8()` of
      such an image was near-black. `codecs::jpeg::scale_to_full_range` now lifts every
      sample to the full range (identity at `P == 16`), pinned end to end by
      `a_twelve_bit_jpeg_decodes_at_the_full_sixteen_bit_scale` — which builds its 12-bit
      fixture through `oxiarc_jpeg::Encoder::encode_u16`, since this facade has no 12-bit
      encode path to make one with
- [x] **`codecs::png::CompressionType::Level(n > 9)`** was passed through to
      `oxiarc-deflate`, which has no defined level above 9. Now clamped
- [x] **`write_with_encoder`'s doc claim that an incompatible colour type "returns
      `Unsupported` rather than silently narrowing"** was false for alpha: JPEG accepts a
      four-channel buffer through `oxiarc_jpeg::InputColor::Rgba` and drops the alpha.
      Behaviour kept (it is a deliberate superset of `image`, which rejects `Rgba8`
      outright), doc corrected to state both outcomes

## Future Enhancements

### Reduced-scale JPEG decode
- [ ] Expose `oxiarc_jpeg::Decoder::set_scale_denom`/the `djpeg -scale M/N` family
  - **Goal:** let a caller decode a large JPEG at 1/2, 1/4 or 1/8 resolution directly,
    matching `image`'s `ImageReader`-independent scaled-decode story if one is ever
    added upstream, or at minimum matching `oxiarc-jpeg`'s own native capability.
  - **Design:** `codecs::jpeg::JpegDecoder` would need a builder step
    (`JpegDecoder::with_scale_denom(r, denom)` alongside `new`) before `read_info`, since
    scaling changes `dimensions()`'s answer.
  - **Files:** MODIFY `src/codecs/jpeg.rs`.
  - **Tests:** a decode-at-1/2/1/4/1/8 test comparing dimensions and a coarse pixel
    check against the full-resolution decode.
  - **Risk:** low; `oxiarc-jpeg` already implements the feature natively (not part of
    the `W1-F1` contract, per that track's own handoff note), this crate would only be
    wiring it through.
- [ ] Not started because no concrete downstream caller has asked for it yet (the
  `image` crate itself has no equivalent public API to mirror); revisit if a real
  Tier-B project needs it.

### `ImageDecoder` metadata passthrough
- [ ] `icc_profile()`/`exif_metadata()`/`orientation()` default methods on
  `ImageDecoder`, matching `image` 0.25's fuller trait
  - **Goal:** let a caller read ICC/EXIF metadata through the trait uniformly across
    formats, instead of reaching into each concrete `codecs::*::*Decoder`'s own
    `icc_profile()`/`exif()` (PNG has neither exposed yet; JPEG has both; TIFF has
    `icc_profile()`).
  - **Design:** add default trait methods returning `Ok(None)`, override per codec where
    the underlying crate already exposes the data (`oxiarc_jpeg::Decoder::{icc_profile,
    exif}`, `oxiarc_tiff::Decoder::icc_profile`); PNG's `oxiarc_png::Reader` does not
    expose `iCCP`/`eXIf` chunk contents through its `Reader` type yet (only through
    ancillary-chunk parsing) — would need a small `oxiarc-png` API addition, i.e. a
    cross-crate dependency this track cannot resolve alone.
  - **Files:** MODIFY `src/traits.rs`, `src/codecs/{png,jpeg,tiff}.rs`.
  - **Tests:** one round trip per format that has real metadata support.
  - **Risk:** medium — the PNG half is blocked on an `oxiarc-png` API addition outside
    this track's ownership.

## Test Coverage

Measured 2026-09-07 (`cargo nextest run -p oxiarc-image --all-features` +
`cargo test --doc -p oxiarc-image --all-features`).

- **109** unit + integration tests via `cargo nextest` (69 unit in `src/`, 40 across
  `tests/{roundtrip,sniffing,error_mapping}.rs`)
- **13** doctests (`cargo test --doc`)
- **122 total**, 0 failing

## Code Statistics

Measured 2026-09-07 (`tokei`, Rust "Code" column, i.e. excluding comments/blanks).

| File | Lines |
|---|---|
| `src/dynamic.rs` | 486 |
| `src/codecs/tiff.rs` | 371 |
| `src/error.rs` | 356 |
| `src/color.rs` | 355 |
| `src/format.rs` | 279 |
| `src/codecs/png.rs` | 260 |
| `src/buffer.rs` | 211 |
| `src/codecs/jpeg.rs` | 176 |
| `src/reader.rs` | 187 |
| `src/traits.rs` | 24 |
| `src/lib.rs` | 23 |
| `src/codecs/mod.rs` | 3 |
| **`src/` total** | **2,731** |
| `tests/roundtrip.rs` | 207 |
| `tests/error_mapping.rs` | 95 |
| `tests/sniffing.rs` | 93 |
| **`tests/` total** | **395** |
| **Crate total** | **3,126** |

Every file is well under the 2000-line policy ceiling (largest is `dynamic.rs` at 486
tokei code lines / 658 `wc -l`, under a third of it); no split is warranted.
`codecs/tiff.rs` gained a regression test (see "### Codecs" above) that pins the
exact byte output of `oxiarc-tiff`'s known float-fallback gap so a future upstream fix
is caught by a failing assertion, not just a stale doc comment.

## Known Limitations

1. **No image-processing operations** — by design, not a gap; see the README's "What
   this is not" section. A project needing both I/O and processing keeps `image` for
   processing and swaps only its I/O path via this crate's `codecs::*` types, or waits
   for a separate OxiArc processing crate.
2. **Reduced-scale JPEG decode is not exposed** — see "Future Enhancements" above;
   `oxiarc-jpeg` has the capability natively, this facade does not wire it through yet
   (no concrete caller has asked for it).
3. **`ImageDecoder` has no metadata-passthrough default methods** (`icc_profile`,
   `exif_metadata`, `orientation`) — each codec's own concrete type exposes what the
   underlying crate has (JPEG the most, TIFF `icc_profile()` only, PNG none yet); see
   "Future Enhancements".
4. **A JPEG/TIFF encode-time failure can report as `ImageError::Decoding`** rather than
   `Encoding`, because `oxiarc_jpeg::JpegError`/`oxiarc_tiff::TiffError` are single enums
   shared by both directions with no way to tell which one produced a given value from
   the value alone — documented in `error.rs`'s doc comments on both `From` impls. PNG
   does not have this limitation (`oxiarc_png` has separate `DecodingError`/
   `EncodingError` types). Mitigated in practice for the one common encode-time mistake
   (a mismatched buffer length): all three codecs' `write_image` now validate the
   buffer length themselves and return `ImageError::Parameter` before ever reaching the
   underlying crate.
5. **Coordination note, not a defect:** during this session, `cargo clippy -p
   oxiarc-image --all-features --all-targets -- -D warnings` transiently failed once
   due to an in-progress, unrelated clippy lint in the `oxiarc-jpeg` dependency (a
   concurrent track's own WIP, fixed moments later by that track). `cargo clippy -p
   oxiarc-image --all-features --all-targets --no-deps -- -D warnings` lints only this
   crate's own code (skipping dependency crates entirely) and is the right command to
   reach for if this happens again while another track has uncommitted work on a crate
   `oxiarc-image` depends on.

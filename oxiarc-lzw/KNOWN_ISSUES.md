# Known Issues

Last re-verified: 2026-07-13 (v0.3.6 hardening pass), against Pillow 12.1.0 /
libtiff 4.7.1 on macOS. The full test matrix below was re-run at that date.

## Resolved Issues

### Issue 1: All 256 Byte Values (0-255)

**Status**: FIXED (bit-width synchronization)

Encoding a sequence containing all 256 byte values decoded bytes 254-255
incorrectly, caused by an encoder/decoder desync at the 9-to-10 bit width
transition. Fixed by the decoder-side `update_bit_width_decode()` threshold
(one below the encoder's, compensating for the decoder's one-entry lag).
Covered by `test_lzw_all_byte_values` and the `allbytes_256` reference
fixture (which is byte-identical to libtiff's output).

### Issue 2: Large Repetitive Data ("Invalid Code: 753")

**Status**: STALE — no longer reproduces; removed from the issue list

Older revisions reported an `Invalid LZW code: 753` decode failure on large
(>100 KB) highly repetitive inputs. Re-running the matrix shows this does
not reproduce at any size:

| Input | Size | Result |
|-------|------|--------|
| "The quick brown fox..." x 100 | 4.5 KB | PASS (byte-identical to libtiff, `fox_4500` fixture) |
| "The quick brown fox..." repeated | 1 MiB | PASS (both directions vs Pillow/libtiff) |
| "The quick brown fox..." x 200,000 | 9.2 MB | PASS (`test_lzw_very_large_input`) |
| All-same-byte | 100 KB | PASS (both directions vs Pillow/libtiff) |

The former workarounds (use DEFLATE, split files, fall back to `weezl`) are
no longer needed.

### Issue 3: No TIFF 6.0 ClearCode support (total libtiff/Pillow interop failure)

**Status**: FIXED in the v0.3.6 hardening pass (LZW-01)

Until this pass, `LzwConfig::TIFF` set `use_clear_code: false`: the decoder
rejected code 256 as `InvalidClearCode` and the encoder never emitted one.
TIFF 6.0 **mandates** a ClearCode as the first code of every strip and again
when the code table reaches entry 4094, so every strip written by
libtiff/Pillow/GDAL/Photoshop failed to decode, and every oxiarc-encoded
strip was rejected by those tools — self round-trips passed while real-world
TIFF interop failed 100% in both directions.

The fix (mirroring libtiff's `tif_lzw.c` exactly):

- `LzwConfig::TIFF` now sets `use_clear_code: true`;
- the encoder emits a leading ClearCode, resets the table (with another
  ClearCode) when `next_code` reaches 4094, and accounts for the decoder's
  phantom final table entry before writing EOI so the EOI width always
  matches (libtiff `LZWPostEncode` semantics);
- the decoder accepts ClearCode resets anywhere in the stream (including
  libtiff's ratio-checkpoint resets), dropping back to 9-bit codes.

**Differential verification (2026-07-13, both directions)** — see
`tests/tiff_lzw_oracle.rs` (feature `tiff-oracle`) and the always-run pinned
fixtures in `tests/tiff_ref_fixtures.rs`:

- decode: 125/125 Pillow/libtiff-produced strips decode byte-identically
  (sizes 1 B - 1 MiB, including width-transition boundary sweeps and
  table-fill ClearCode resets);
- encode: 125/125 oxiarc-produced TIFF-LZW files decoded correctly by
  Pillow and accepted by `tiffcp`;
- bonus: oxiarc's compressed output is **byte-identical** to libtiff's for
  all 125 corpus cases (TIFF LZW with libtiff's parameters is fully
  deterministic), which the pinned fixtures now gate on every `cargo test`.

### Issue 4: 64 MiB-per-frame allocation DoS in the streaming decoder

**Status**: FIXED in the v0.3.6 hardening pass (LZW-02)

`LzwStreamDecoder` (TIFF mode) passed a hardcoded 64 MiB sentinel as
`expected_size` for every frame, and `LzwDecoder::decode` pre-reserved it
verbatim — 200,000 tiny frames (~2 MB of input) forced ~12.5 TB of
cumulative allocator traffic. Now the stream framing carries the true
uncompressed length per frame (8-byte header: compressed + uncompressed
u32), the decoder clamps its up-front reservation to 64 KiB and grows
incrementally, and each frame's decoded length is validated against the
header (mismatch = `InvalidData`, never silently short/padded data).
Re-measured: 200,000 tiny frames decode correctly with ~7 MiB peak RSS.
Note: this changed the (crate-private) streaming frame format; streams
written by older versions must be rewritten.

### Issue 5: `LzwConfig` struct-literal panics

**Status**: FIXED in the v0.3.6 hardening pass (LZW-03)

`LzwConfig::new(0, 12).clear_code()` panicked with subtract-with-overflow in
debug builds (and returned bogus values in release). `LzwConfig::new` now
validates `9 <= min_bits <= max_bits <= 16` (the ceiling was 12 before
0.4.2, which raised it to the UNIX `compress` maximum) and returns `Result`;
`clear_code`/`eoi_code`/`first_code`/`max_code` use saturating arithmetic so
even an invalid struct-literal config cannot panic, and
`LzwConfig::validate()` is the authoritative gate (called by
`LzwEncoder::new`/`LzwDecoder::new` via the dictionary).

---

## Current Limitations

- **Horizontal-differencing predictor (TIFF tag 317)**: out of scope for
  this crate. Predictor pre/post-processing is a container-level transform
  that callers (e.g. OxiGDAL) must apply around the raw LZW codec.
- **Old-style LSB-first TIFF LZW**: supported since 0.4.2 via
  `LzwConfig::TIFF_COMPAT_LSB` (libtiff's `LZWDecodeCompat` variant: the
  standard late width change plus LSB-first packing). libtiff only ever
  reads that variant, so this crate reads it too and never writes it — the
  TIFF encoder stays on `LzwConfig::TIFF`.
- **Streaming frame format**: the `LzwStreamEncoder`/`LzwStreamDecoder`
  framing is private to this crate (it is not part of the TIFF or GIF file
  formats) and changed in v0.3.6 (Issue 4 above).

## Verified Test Matrix (re-run 2026-07-13)

- Empty input, 1 byte, all byte values, alternating patterns: PASS
- Boundary sizes 1-40, 240-280, 500-530 (9->10 bit + EOI phantom-entry
  corners): PASS, byte-identical vs libtiff in both directions
- 1 KiB - 256 KiB incompressible (table-fill ClearCode resets): PASS, both
  directions
- Repetitive/text data up to 9.2 MB: PASS
- Truncated/corrupted strips: return `Err` (or detectably short data),
  never panic, never full-length wrong bytes
- GIF LZW round-trip suite (`gif_lzw`): PASS (unchanged by this pass)

## Added 2026-09-08 (0.4.2, track LZW3)

- Code widths 9-16 and an explicit `LzwConfig::bit_order`; `LzwConfig::GIF`
  is LSB-first and no longer identical to `LzwConfig::TIFF_OLD_STYLE` (a
  documented footgun, now closed and pinned by a behavioural test).
- UNIX `compress` / `.Z` container (`oxiarc_lzw::z`), byte-identical to
  `compress -b N -c` in both directions; `gzip -dc` and `uncompress -c`
  reproduce this crate's streams for every width those tools accept
  (they refuse `max_bits < 12` on macOS/BSD, which is their limitation, not
  this crate's).
- `.Z` truncation is deliberately **not** an error: the format has no
  end-of-information code, so a truncated stream decodes to a prefix, the
  same way `gzip -dc` behaves. Callers that need completeness must get it
  from the transport.

## Production Readiness

**Verdict**: PRODUCTION READY for TIFF LZW interop with the real-world
toolchain (libtiff, Pillow, GDAL) — now backed by two-direction differential
evidence rather than self round-trips alone. Regressions are gated by the
always-run pinned-fixture tests and, where the tools are installed, the
`tiff-oracle` differential suite.

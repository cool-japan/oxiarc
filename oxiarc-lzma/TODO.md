
# oxiarc-lzma - Development Status (v0.2.7, 2026-04-21)

## Completed Features (COMPLETE)

### Range Coder (363 lines)
- [x] `RangeEncoder` with 64-bit accumulator
- [x] `RangeDecoder` for decompression
- [x] 11-bit probability model (2048 states)
- [x] Normalization threshold: 0x01000000
- [x] Cache-based carry propagation
- [x] `encode_bit()` / `decode_bit()` with probability update
- [x] `encode_direct_bit()` / `decode_direct_bit()` (50% probability)
- [x] `encode_direct_bits()` / `decode_direct_bits()` (multiple bits)
- [x] `encode_bit_tree()` / `decode_bit_tree()` (normal order)
- [x] `encode_bit_tree_reverse()` / `decode_bit_tree_reverse()` (reverse order)
- [x] `flush()` and `finish()` for finalization
- [x] LZMA2-style initialization (`new_lzma2()`)

### Model (390 lines)
- [x] `LzmaProperties` (lc, lp, pb)
- [x] Properties byte encoding/decoding
- [x] `State` machine (12 states)
- [x] State transitions for all match types
- [x] `LiteralModel` with context-dependent probabilities
- [x] `LengthModel` (choice, choice2, low, mid, high)
- [x] `DistanceModel` (slot, special, align)
- [x] `LzmaModel` combining all sub-models
- [x] Model initialization with PROB_INIT
- [x] `num_pos_states()` and `num_lit_states()` calculations

### Encoder (517 lines)
- [x] `LzmaEncoder` high-level API
- [x] Literal encoding (normal and matched)
- [x] Match encoding with distance
- [x] Rep match encoding (rep0, rep1, rep2, rep3)
- [x] Short rep encoding (single byte)
- [x] Length encoding (low, mid, high ranges)
- [x] Distance encoding (slot + direct + align)
- [x] End marker encoding
- [x] Header writing (properties + dict size + uncompressed size)
- [x] `compress()` one-shot function
- [x] `compress_raw()` without header
- [x] Multiple compression levels (dict size selection)

### Decoder (439 lines)
- [x] `LzmaDecoder` high-level API
- [x] Header parsing
- [x] Literal decoding (normal and matched)
- [x] Match decoding with distance
- [x] Rep match decoding (all four slots)
- [x] Short rep decoding
- [x] Length decoding
- [x] Distance decoding
- [x] End marker detection
- [x] Known size termination
- [x] `decompress()` one-shot function
- [x] `decompress_raw()` without header

### Lib (212 lines)
- [x] `LzmaLevel` with preset dictionary sizes
- [x] `compress_bytes()` convenience function
- [x] `decompress_bytes()` convenience function
- [x] Comprehensive test suite

## Future Enhancements

### LZMA2 Support
- [x] LZMA2 stream format (chunked)
- [x] Uncompressed chunks
- [x] Property changes mid-stream
- [x] Reset codes

### Compression Improvements
- [x] Better match finding (hash chain optimization)
  - FNV-1a hash function with good distribution
  - Chain table linking positions with same hash
  - Level-dependent chain depth (4 to 1024)
  - Quick 3-byte rejection for faster matching
- [x] Optimal parsing (price calculation)
  - Price calculation infrastructure for all encoding operations
  - Bit encoding price tables (pre-computed)
  - Match vs literal price estimation
  - Rep match price calculation
  - Distance and length encoding prices
  - Simplified optimal sequence selection (heuristic-based)
  - Enabled for compression levels 8-9
- [x] Fast bytes parameter
  - Configurable fast_bytes (5-273)
  - Level 8: 64 fast bytes
  - Level 9: 128 fast bytes
- [x] Nice length parameter
  - Configurable nice_length (8-273)
  - Level 8: 128 nice length
  - Level 9: 273 nice length
- [ ] Full dynamic programming optimal parser
  - Backward optimal parsing with DP table
  - Multiple path tracking
  - Forward and backward pass optimization
- [ ] Binary tree match finder

### Performance
- [ ] SIMD-accelerated match finding
- [ ] Multi-threaded compression
- [ ] Memory pool for large dictionaries
- [ ] Streaming with bounded memory

### Features
- [ ] Custom dictionary initialization
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** `LzmaEncoder`, `LzmaDecoder`, and `LzmaStreaming*` types accept `ProgressHandle` AND `CancellationToken`. Two TODO items closed in one move.
  - **Design:**
    - `.with_progress(handle)` + `.with_cancel(token)` builders on encode/decode types.
    - Emit `on_progress(input_consumed, Some(total))` where total is known at input-size level (encoder knows input size; decoder may know uncompressed_size from container).
    - `token.check()?` at every range-coder normalize boundary or every N iterations (N = 4096 bytes to keep overhead <1%).
  - **Files:** MODIFY `oxiarc-lzma/src/encode.rs`, `decode.rs`, `streaming.rs` (exact names TBD during implementation).
  - **Prerequisites:** both core primitives already in.
  - **Tests:** round-trip counting sink; cancellation fixture cancels mid-decode and observes `OxiArcError::Cancelled`.
  - **Risk:** cancellation granularity — too fine adds overhead, too coarse delays. Mitigated by picking 4 KiB input-chunk granularity (tuned to amortize check cost).
- [ ] Async I/O

### Integration
- [x] 7z container support (via oxiarc-archive)
- [x] XZ container support
- [x] ZIP method 14 (LZMA) support (planned 2026-04-20)
  - **Goal:** `ZipReader` decompresses entries with method=14 (LZMA); `ZipWriter` can emit method=14 entries when configured. `oxiarc_lzma` is the codec backend. Both sides interoperate with 7-Zip and Info-ZIP `unzip` built with LZMA support.
  - **Design:**
    - **Format (APPNOTE §5.8.8):** method-14 entry compressed data is `[major_ver: u8][minor_ver: u8][props_size: u16_le][lzma_props: props_size bytes][lzma_stream: N bytes]`. `props_size` is always 5 for standard LZMA. `lzma_props` is the 5-byte `(lc/lp/pb packed, dict_size[4 LE])` header as defined by the LZMA SDK.
    - **EOS-marker semantics (APPNOTE §4.4.4, general-purpose bit 1):** for method=14, bit 1 of the local-file-header general-purpose-bit-flag word controls whether the LZMA stream carries an end-of-stream marker. If bit 1 is set → EOS marker is present and terminates the stream. If bit 1 is clear → no EOS marker, extraction stops at `compressed_size` bytes.
    - **Our choice on write:** always emit with EOS marker + set bit 1 in the LFH gp-flag. This matches 7-Zip's default output and is the interop-safe path (Info-ZIP's `unzip` historically preferred EOS-bearing streams). Store the reported `compressed_size` anyway (header is authoritative for skipping), but the decoder terminates on the marker.
    - **Our choice on read:** honour bit 1 — call `oxiarc_lzma::decompress_raw` in EOS-aware mode when set, in known-size mode when clear. Report `OxiArcError::Malformed("method 14 without EOS and without known size")` if both the gp-flag bit 1 is clear and `compressed_size` is zero/unknown (ZIP64 streaming edge case).
    - Extend `zip::header::types::CompressionMethod` with `Lzma` variant (value 14) + `from_u16` + `to_core()` mappings.
    - In `zip/header/reader.rs` decompress path (line ~417), add `CoreMethod::Lzma` branch: parse the 4-byte prefix + 5-byte props, then call `oxiarc_lzma::decompress_raw` with the appropriate EOS mode.
    - Writer: `ZipWriter::add_file_lzma(name, data)` (simple default path) + `ZipWriter::add_file_with_method(name, data, Method::Lzma)` (generic). Writer implementation: compress with `oxiarc_lzma::compress_raw` producing `[5-byte props][stream-with-EOS]`, then prepend the 4-byte `[major, minor, props_size_le_u16]` header, set LFH gp-flag bit 1, set compression_method=14.
    - Add `oxiarc-lzma.workspace = true` to `oxiarc-archive/Cargo.toml` if not already.
  - **Files:**
    - MODIFY `oxiarc-archive/src/zip/header/types.rs` — add `Lzma` variant to `CompressionMethod` + `from_u16`/`to_core()`.
    - MODIFY `oxiarc-archive/src/zip/header/reader.rs` — decompress dispatch that honours gp-flag bit 1.
    - MODIFY `oxiarc-archive/src/zip/header/writer.rs` — compress dispatch + `add_file_lzma` API + set gp-flag bit 1 + emit 4-byte method-14 prefix.
    - MODIFY `oxiarc-archive/Cargo.toml` — add `oxiarc-lzma` dep if missing.
    - MODIFY `oxiarc-core/src/entry.rs` — add `Lzma` to core `CompressionMethod` enum.
  - **Prerequisites:** `oxiarc-lzma` must expose a raw-stream compress/decompress API that accepts/emits the 5-byte props header **and** supports both EOS-aware and known-size decode modes. Audit during implementation: `compress_raw` / `decompress_raw` already exist in `oxiarc-lzma` (confirmed); verify the decode path's EOS-mode knob, add one if missing.
  - **Tests:**
    - Round-trip: write ZIP with 3 LZMA-method files via `ZipWriter`, read back via `ZipReader`, byte-for-byte match.
    - Interop-write fixture: produce a ZIP and verify structural correctness (LFH gp-flag bit 1 set, method = 14, 4+5-byte prefix, EOS marker at stream end by inspecting the last 5 bytes of LZMA stream).
    - Interop-read fixture: hand-craft a 4+5-byte prefix + an `oxiarc_lzma::compress_raw` output manually wrapped into a ZIP LFH — confirm `ZipReader` extracts correctly.
    - Edge case: entry with gp-flag bit 1 clear (no-EOS / known-size) — verify decode terminates at `compressed_size`.
  - **Risk:** LZMA SDK version bytes (`major`, `minor`) vary across tools; 7-Zip emits `0x13 0x00` (= 19.0), Info-ZIP emits others. Accept any version on read (we rely on the props, not the version). On write, emit whichever `oxiarc-lzma` currently reports — document as "SDK-version-opaque" in the module doc.

## Test Coverage

- Total: 66 tests (range_coder 3, model 4, encoder 7, decoder 2, lzma2 5, optimal 7, lib 13, plus more)

## Code Statistics

| File | Lines |
|------|-------|
| encoder.rs | 832 |
| lzma2.rs | 772 |
| optimal.rs | 474 |
| decoder.rs | 456 |
| model.rs | 390 |
| range_coder.rs | 361 |
| lib.rs | 280 |
| (other) | ~303 |
| **Total** | **~3,868** |

## Technical Notes

### Range Coder Internals

The range coder maintains:
- `range`: Current interval size (32-bit)
- `low`: Interval start (64-bit for carry handling)
- `cache`: Pending output byte
- `cache_size`: Number of pending 0xFF bytes

### Probability Update Formula

```
if bit == 0:
    prob += (2048 - prob) >> 5  // Move toward 2048
else:
    prob -= prob >> 5            // Move toward 0
```

This gives ~3% probability change per update.

### Distance Encoding

Distance encoding uses:
1. Slot (6 bits): Determines distance range
2. Direct bits: Fixed 50% probability bits
3. Align bits (4 bits): Context-dependent

```
Slot 0-3: Distance = slot
Slot 4-13: Distance = ((2 | (slot & 1)) << num_bits) + reverse_bits
Slot 14+: Distance = ((2 | (slot & 1)) << num_bits) + direct_bits + align_bits
```

## Known Limitations

1. Optimal parsing uses simplified heuristics (not full DP)
3. Single-threaded only
4. High memory usage for large dictionaries

## Optimal Parsing Implementation

### Current Implementation (Levels 8-9)

The current optimal parsing implementation uses price estimation and heuristic-based selection:

1. **Price Calculation**:
   - Pre-computed probability-to-price conversion table
   - Prices measured in 1/16th bit units for precision
   - Separate price calculators for literals, matches, and rep matches
   - Distance and length encoding price estimation

2. **Match Selection**:
   - Find all matches at current position (not just best)
   - Calculate prices for rep matches (rep0-rep3)
   - Use heuristic comparison to select best encoding
   - Consider match length and distance in price estimation

3. **Parameters**:
   - **fast_bytes**: Number of bytes to process with simplified optimization
     - Level 8: 64 bytes
     - Level 9: 128 bytes
   - **nice_length**: Match length threshold for immediate acceptance
     - Level 8: 128 bytes
     - Level 9: 273 bytes (maximum)

### Future Enhancement: Full Dynamic Programming

A complete optimal parser would implement:

1. **Backward Optimal Parsing**:
   - DP table storing optimal choices for each position
   - Track multiple paths through the data
   - Backtrack to find globally optimal sequence

2. **Forward-Backward Pass**:
   - Forward pass: build DP table with all possible encodings
   - Backward pass: select optimal path from end to start
   - Update probability models during optimization

3. **Advanced Price Calculation**:
   - Context-dependent probability tracking
   - State machine simulation for accurate pricing
   - Literal context modeling (previous byte, position)

This would provide compression ratios similar to 7-Zip's LZMA implementation.

# oxiarc-lzma - Development Status

## Completed Features

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
- [ ] LZMA2 stream format (chunked)
- [ ] Uncompressed chunks
- [ ] Property changes mid-stream
- [ ] Reset codes

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
- [ ] Progress callbacks
- [ ] Cancellation support
- [ ] Async I/O

### Integration
- [ ] 7z container support (via oxiarc-archive)
- [ ] XZ container support
- [ ] ZIP method 14 support

## Test Coverage

- range_coder: 3 tests
- model: 4 tests
- encoder: 7 tests (includes hash chain tests)
- decoder: 2 tests
- lzma2: 5 tests
- optimal: 7 tests (price calculation and optimal parser)
- lib: 13 tests (includes optimal vs greedy comparison)
- Total: 41 tests

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
| **Total** | **3,565** |

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

1. LZMA2 format partially implemented (no chunking, property changes)
2. Optimal parsing uses simplified heuristics (not full DP)
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

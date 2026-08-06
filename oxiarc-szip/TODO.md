# oxiarc-szip - Development Status (v0.4.2, 2026-08-06)

## Completed Features (COMPLETE)

### Decode
- [x] Full CCSDS-121.0-B-2 unrestricted-option-space entropy decoder
  (`decode`): fundamental-sequence (zero-run/one-terminated) codewords,
  the second-extension option, sample-split (Golomb-Rice) blocks with
  FS-quotients-then-k-bit-remainders grouping, zero-block runs (including
  ROS and 64-block segments), and no-compression blocks
- [x] Unit-delay (NN) predictor inversion with theta-clamped residual
  mapping per CCSDS-121.0-B-2 §4
- [x] MSB-first (standard) and LSB-first (crate-local extension) bit
  ordering (`SzipParams::msb`)
- [x] `id_len`/`k_max` matching the unrestricted CCSDS option space
  (3/4/5-bit option-ID field for bpp ≤ 8/16/32)
- [x] Reference-sample-interval (RSI) framing: option ID precedes all
  `pixels_per_block` samples of an RSI's first block; reference sample is
  sample #0 when `nn_preprocess` is on
- [x] Whole-block padding: a trailing partial block is padded/discarded
  rather than misread
- [x] `SzipParams` validation (`InvalidParam`) and typed error surface
  (`SzipError`, `#[non_exhaustive]`)

### Encode
- [x] `encode()` / `encode_bytes()`: CCSDS-121.0-B-2-conformant framing
  using the no-compression option for every block — always spec-valid,
  round-trips through this crate's own decoder and through the libaec
  reference decoder, but does not attempt entropy compression (see Known
  Limitations)

### Interoperability
- [x] Differential validation against libaec 1.1.4 in both directions,
  embedded as fixtures in `tests/libaec_interop.rs` (always-run, no
  external dependency): 888/888 libaec-encoded streams decode
  byte-identical; 1776/1776 oxiarc-encoded streams are accepted
  byte-identical by the reference decoder
- [x] `libaec-oracle` opt-in feature: live differential gate that compiles
  a small C shim against an installed libaec (self-skips without a C
  toolchain or libaec install; never enabled by default, so the crate's
  default-feature build stays Pure Rust)

### Quality / Testing
- [x] Corrupted/truncated-input hardening (`tests/corrupt_input.rs`):
  bit-flip and multi-bit-flip mutation of real fixtures, truncated
  prefix/mid-block streams — decoder never panics, always errors cleanly
  or returns valid output
- [x] Property-based round-trip testing (`tests/proptest_roundtrip.rs`)
- [x] `examples/sample_decode.rs`
- [x] `benches/szip_bench.rs` (criterion)

## Future Enhancements

### Real entropy-coding encoder
- [ ] Block-adaptive encoder that actually selects among fundamental-sequence,
  second-extension, sample-split (with k selection), and zero-block-run
  options instead of always emitting no-compression blocks. This is the
  main remaining gap: today `encode()` exists purely to make round-trip
  and libaec-interop testing possible, not to achieve compression ratio.
  **Design sketch:** per-block, compute the CCSDS-121.0-B-2 cost estimate
  for each option (§5.1's `Delta` heuristic or brute-force cost comparison
  against the reference `k`), pick the minimum, matching libaec's own
  `create_encoder`/`optimum_k` approach.
- [ ] Signed-sample preprocessing (`AEC_DATA_SIGNED` in libaec) — currently
  unimplemented; documented in README.md and `lib.rs`
- [ ] CCSDS *restricted* option set (`AEC_RESTRICTED` in libaec) — currently
  unimplemented; only the unrestricted option space is supported

### Performance
- [ ] SIMD-accelerated bit (de)packing in `bitreader.rs`

## Test Coverage

Per-module/binary test counts (`cargo nextest run -p oxiarc-szip`, default
features, verified 2026-08-03):

- decode (unit tests, covering both `decode` and `encode`/`encode_bytes`
  round-trips): 25 tests
- corrupt_input (integration): 6 tests
- libaec_interop (integration): 3 tests
- proptest_roundtrip (integration): 2 tests
- **Total: 45 tests** (`cargo nextest run -p oxiarc-szip`) + 1 doctest
  (`cargo test --doc -p oxiarc-szip`)

Additional live differential coverage is available but not counted above:
`cargo test -p oxiarc-szip --features libaec-oracle` (self-skips without a
C toolchain/libaec install).

## Code Statistics

Code lines per file (`oxiarc-szip/src`, verified 2026-08-03):

| File | Lines |
|------|-------|
| decode.rs | 965 |
| params.rs | 146 |
| bitreader.rs | 156 |
| lib.rs | 85 |
| error.rs | 71 |
| encode.rs | 214 |
| **Total** | **1,637** |

## Known Limitations

1. `encode()` always emits no-compression blocks — it is a conformant AEC
   byte stream, and every conforming decoder (including libaec) reads it
   correctly, but it does not reduce data size. See "Real entropy-coding
   encoder" above.
2. Signed-sample preprocessing (libaec's `AEC_DATA_SIGNED`) is not
   implemented.
3. The CCSDS *restricted* option set is not implemented; only the
   unrestricted option space (the common case, and what libaec defaults to)
   is supported.
4. `libaec-oracle` is a dev/test-only feature (compiles a C shim against an
   installed libaec); it is never enabled by default and does not affect
   the Pure Rust guarantee of the shipped library.

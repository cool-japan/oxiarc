
# oxiarc-core - Development Status (v0.3.3, 2026-06-06)

## Completed Features (COMPLETE)

### BitStream (601 lines)
- [x] LSB-first bit packing (standard for DEFLATE/LZH)
- [x] u64 internal buffer for efficient reads/writes
- [x] Generic over `Read`/`Write` traits
- [x] `read_bits(count: u8) -> u32`
- [x] `write_bits(value: u32, count: u8)`
- [x] `read_byte()` / `write_byte()`
- [x] Byte alignment (`align_to_byte()`)
- [x] Peek bits without consuming
- [x] MSB-first mode for special cases
- [x] Bit counting (`bits_read()`, `bits_written()`)

### RingBuffer (417 lines)
- [x] Configurable sizes (4K, 8K, 32K, 64K)
- [x] Safe indexing with modulo wrapping
- [x] `OutputRingBuffer` for decompression
- [x] `copy_from_self(distance, length)` for match expansion
- [x] Efficient bulk copy operations
- [x] `get(offset)` with negative indexing

### CRC (940 lines)
- [x] CRC-32 (ZIP/GZIP): polynomial 0xEDB88320 (reflected)
- [x] CRC-16/ARC (LZH): polynomial 0xA001 (reflected)
- [x] Pre-computed lookup tables (256 entries)
- [x] Slicing-by-8 tables and optimizations
- [x] DualCrc optimization
- [x] Incremental computation
- [x] One-shot `compute()` convenience method

### Traits (283 lines)
- [x] `Decompressor` trait with streaming interface
- [x] `Compressor` trait with streaming interface
- [x] `ArchiveReader` trait for reading archives
- [x] `ArchiveWriter` trait for writing archives
- [x] `DecompressStatus` / `CompressStatus` enums
- [x] `FlushMode` enum (None, Sync, Full, Finish)
- [x] `CompressionLevel` (0-9)
- [x] Default implementations for `decompress_all()` / `compress_all()`

### Entry (463 lines)
- [x] `Entry` struct with full metadata
- [x] `EntryType` enum (File, Directory, Symlink, etc.)
- [x] `CompressionMethod` enum (Stored, Deflate, LZSS, LZMA, etc.)
- [x] `FileAttributes` for permissions
- [x] Path sanitization (`sanitized_name()`)
- [x] Space savings calculation
- [x] Compression ratio
- [x] Unix/DOS attribute conversion

### Error (228 lines)
- [x] `OxiArcError` with thiserror derive
- [x] `Io` variant (from `std::io::Error`)
- [x] `InvalidMagic` with expected/found
- [x] `UnsupportedMethod`
- [x] `CrcMismatch` with expected/computed
- [x] `InvalidHuffmanCode`
- [x] `Corrupted` with offset and message
- [x] `InvalidHeader`
- [x] `Result<T>` type alias

## Future Enhancements

### Performance
- [x] SIMD-accelerated CRC32 (planned 2026-04-20)
  - **Goal:** Verify the PCLMULQDQ/PMULL implementations in `crc_simd.rs` are wired into `Crc32::compute` via runtime feature detection on x86_64 and aarch64; add benchmarks if absent; enable `simd` feature by default on these targets.
  - **Design:**
    - Runtime dispatch: `Crc32::compute(data)` checks `std::is_x86_feature_detected!("pclmulqdq")` (or equivalent `is_aarch64_feature_detected!("aes")` for PMULL) once (stored in `OnceLock<fn(&[u8]) -> u32>`), then dispatches.
    - Update `oxiarc-core/Cargo.toml` to add `simd` to `[features] default = [..., "simd"]` **only for x86_64/aarch64 targets** via `[target.'cfg(any(target_arch = "x86_64", target_arch = "aarch64"))'.dependencies]`-style gating — actually, features can't be target-conditional directly; instead, make the `simd` module always compile-available on those targets and fall back to the slicing-by-8 scalar path on others.
    - Preferred approach: drop the `feature = "simd"` gate, make `crc_simd` always compiled on x86_64/aarch64 via `#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]`, dispatch at runtime, keep a scalar fallback always compiled.
  - **Files:**
    - MODIFY `oxiarc-core/src/crc.rs` — add runtime dispatch in `Crc32::compute` + `Crc32::update`.
    - MODIFY `oxiarc-core/src/crc_simd.rs` — replace `#[cfg(feature = "simd")]` with `#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]`.
    - MODIFY `oxiarc-core/src/lib.rs` — same gating.
    - MODIFY `oxiarc-core/Cargo.toml` — remove `simd` feature, or re-document as a "enable extra SIMD" opt-in with a no-op default on non-supported targets.
  - **Prerequisites:** none.
  - **Tests:** cross-validate SIMD path against scalar path on a randomized 1 MiB buffer — bit-exact equality; add a benchmark (criterion or black_box baseline) showing SIMD > 4× scalar on supported targets.
  - **Downstream compatibility (explicit):** the `simd` cargo-feature **name stays defined** in `oxiarc-core/Cargo.toml` as a no-op alias after the gate is removed. Any downstream `features = ["simd"]` (including workspace consumers) must continue to resolve and compile unchanged. Document the alias as deprecated-but-preserved in the feature doc-comment, and leave a follow-up to remove it after one minor release cycle.
  - **Risk:** feature-flag removal can break downstream consumers → mitigated by the no-op alias above. Secondary risk: `std::is_x86_feature_detected!` / `std::is_aarch64_feature_detected!` invocation cost on hot paths — dispatch only once via `OnceLock<fn>` to amortize.
- [x] Enable SIMD CRC32 dispatch on aarch64 with corrected fold constants (completed 2026-05-06)
  - **Goal:** `SimdCrc32Dispatcher::update` routes to `crc32_pmull` on aarch64 when `is_aarch64_feature_detected!("aes")` returns true. Output is bitwise-identical to scalar `software_crc32` for all input lengths 0–4096 bytes. `#[ignore]` removed from `test_pmull_matches_scalar_vectors`; test passes on Apple Silicon. x86_64 PCLMULQDQ path remains but its dispatch branch still returns false — regression test stays `#[ignore]` until a CI x86 runner exists.
  - **Design:** (1) Pin aarch64 fold constants verbatim from crc32fast or zlib-ng with citation comment. (2) Replace crc_simd.rs lines 262–273 aarch64 constants block. (3) Wire `is_simd_available()` (line 507): on aarch64 return `is_aarch64_feature_detected!("aes")`; keep x86 returning false. (4) `SimdCrc32Dispatcher::update` (lines 543–547): branch to `crc32_pmull` when aarch64+feature, else scalar. (5) `crc.rs` lines 161–164: route through `SimdCrc32Dispatcher` instead of direct `software_crc32`. (6) Un-ignore `test_pmull_matches_scalar_vectors` (line 864); add length-sweep and randomized tests.
  - **Files:** MODIFY `oxiarc-core/src/crc_simd.rs`, MODIFY `oxiarc-core/src/crc.rs`
  - **Tests:** un-ignored `test_pmull_matches_scalar_vectors`; `test_pmull_length_sweep` (19 lengths 0–4096); `test_pmull_random_inputs` (100 random vectors, fixed seed); x86 test stays `#[ignore]`
  - **Risk:** If `is_aarch64_feature_detected!("aes")` unavailable on the toolchain, fall back to `cfg!(target_feature = "aes")`; if neither resolves, keep dispatch off but land corrected constants.
- [ ] Vectorized bit operations
- [ ] Zero-copy buffer operations

### Features
- [x] Async I/O support (planned 2026-04-20)
  - **Goal:** Verify `oxiarc-core::async_io` is feature-complete (AsyncCompressor/AsyncDecompressor traits + Wrapper adapters + StreamingAsync* + `compress_concurrent`/`decompress_concurrent`) and add any missing pieces: doc examples, round-trip test covering all four compose directions, re-export audit.
  - **Design:** Read `oxiarc-core/src/async_io.rs` fully, inventory the public surface, write a doc-test + integration test matrix covering (sync codec → AsyncCompressorWrapper), (streaming sync codec → StreamingAsyncCompressorWrapper), and the concurrent helpers. If a method is stubbed, fill it in.
  - **Files:** MODIFY `oxiarc-core/src/async_io.rs` (doc examples only if already complete); ADD `oxiarc-core/tests/async_io.rs` (or extend existing).
  - **Prerequisites:** none.
  - **Tests:** four-way matrix above; `tokio = { version = "*", features = ["rt", "macros", "io-util"] }` dev-dep (latest).
  - **Risk:** if implementation has gaps, the verify becomes genuine implementation work; that is acceptable per IMPLEMENT POLICY. Report `deviated` if scope explodes beyond this run.
- [x] Memory-mapped file support — MappedFile struct with Deref/AsRef<[u8]>, mmap feature flag (done 2026-05-16)
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** A single `ProgressSink` trait lives in `oxiarc-core::progress` and is consumable by every codec + archive reader. This run wires it into the three streaming readers (TAR/ZIP/LZH). Per-codec adoption for brotli/deflate/lzma/snappy/cli is deferred to future runs.
  - **Design:** `pub trait ProgressSink: Send + Sync { fn on_progress(&self, processed: u64, total: Option<u64>); fn on_entry(&self, _name: &str, _index: u64) {} fn on_finish(&self) {} }`. Plus `NoopProgress` impl, `ProgressHandle = Arc<dyn ProgressSink>`, and `noop_progress()` helper. Stream readers accept `Option<ProgressHandle>` via `.with_progress(handle)`.
  - **Files:** `oxiarc-core/src/progress.rs` (NEW ~80 lines), `oxiarc-core/src/lib.rs` (MODIFY)
  - **Prerequisites:** none
  - **Tests:** unit (NoopProgress callable, ProgressHandle is Send+Sync); integration in tar/stream.rs (counting sink observes N calls, processed is monotonic)
  - **Risk:** Arc+vtable overhead per chunk — mitigated by `Option` guard (zero overhead when None)
- [x] Cancellation support (planned 2026-04-20)
  - **Goal:** `CancellationToken` in `oxiarc-core::cancel`; archive stream readers check it at entry boundaries; add `OxiarcError::Cancelled`.
  - **Design:** `struct CancellationToken { flag: Arc<AtomicBool> }` with `cancel()`, `is_cancelled()`, `check() -> Result<(), OxiarcError>`. Atomic store/load with Release/Acquire ordering. Stream readers accept `Option<CancellationToken>` via `.with_cancel(token)`.
  - **Files:** `oxiarc-core/src/cancel.rs` (NEW ~60 lines), `oxiarc-core/src/error.rs` (MODIFY — add Cancelled variant), `oxiarc-core/src/lib.rs` (MODIFY)
  - **Prerequisites:** none
  - **Tests:** unit (Send+Sync+Clone, cross-thread cancel observed); integration in zip/stream.rs (next_entry returns Err(Cancelled) after cancel)
  - **Risk:** none — textbook cooperative cancellation primitive

### API
- [ ] `no_std` support (optional)
- [x] Serde serialization for Entry (optional `serde` feature, v0.2.6)
- [x] Builder pattern for Entry (EntryBuilder with fluent API, v0.2.6)

## Test Coverage

- Total: 132 tests (CRC-32/64 slicing-by-8, DualCrc optimization, size boundary, bitstream, ringbuffer, entry, traits)

## Code Statistics

| File | Lines |
|------|-------|
| crc.rs | 940 |
| bitstream.rs | 601 |
| entry.rs | 463 |
| ringbuffer.rs | 417 |
| traits.rs | 283 |
| error.rs | 228 |
| lib.rs | 83 |
| (other) | ~550 |
| **Total** | **~3,565** |

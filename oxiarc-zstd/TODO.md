# oxiarc-zstd - Development Status (v0.4.2, 2026-09-07)

## Completed Features (COMPLETE)

### Zstandard Core
- [x] Pure Rust implementation
- [x] **Reference interoperability, both directions** (new in 0.3.6): the FSE/Huffman
      backward bitstream now follows the RFC 8878 / reference `BIT_*` semantics
      (LIFO, sentinel-anchored) in reader AND writer; Huffman weight decoding uses
      two interleaved FSE states with the implied-last-weight deduction and full
      Kraft validation; the 4-stream jump table is parsed as stream sizes with
      monotonic bounds checks. Verified: real `zstd`-CLI frames (levels 1-19,
      `--ultra -22`, `--long`, `--no-check`, `--no-content-size`, raw dictionaries,
      multi-frame) decode byte-identically, and all oxiarc frames are accepted by
      `zstd -d` (embedded fixtures always-on; live matrix behind `zstd-oracle`)
- [x] Huffman-compressed literal sections on the encode path (self-verified, with
      Raw/RLE fallback); real FSE compression-table (`FSE_buildCTable`) sequence
      encoding with the RFC 8878 predefined/RLE tables (new in 0.3.6)
- [x] Fixed `set_content_size(false)` corrupting inputs >= 256 B (truncated 1-byte
      FCS): such frames now use an explicit windowed header (new in 0.3.6)
- [x] Raw-content dictionaries carry no Dictionary_ID, so `zstd -d -D <dict>`
      accepts oxiarc dictionary frames (new in 0.3.6)
- [x] Fast decompression
- [x] Parallel compression (Rayon)
- [x] Dictionary support (raw-content, interoperable both directions)
- [x] Checksum support (XXH64)
- [x] Streaming API
- [x] **Bounded, truly incremental decoding** (new in 0.4.2, Phase 8 / W1-B):
      `ZstdStream` push decoder — `decode(&mut self, input, output, FlushMode)
      -> Result<ZstdProgress { consumed, produced, status }>` with
      `ZstdStatus::{NeedInput, NeedOutput, StreamEnd}`, `finish()`, `reset()`,
      `unused_input()`, and the `with_max_output` / `with_max_window` /
      `with_multi_frame` / `with_dictionary` builders. Resumable at every
      structural boundary (frame magic, frame header, block header, block
      payload, block drain, checksum), a real sliding-window ring
      (`src/window.rs`) with lazy growth and chunked `copy_within` match
      execution, a sticky fault latch, and a pre-decode output budget
      (`Frame_Content_Size` exact per frame, `Raw`/`RLE` exact per block,
      compressed blocks charged after decode with a bounded 128 KiB overshoot)
- [x] Incremental XXH64 (`XxHash64::{new, with_seed, update, finish,
      finish_checksum, reset, total_len}`), so frame checksums are verified
      without retaining the output (new in 0.4.2)
- [x] Bomb-safe one-shot helpers `decompress_into(src, dst)`,
      `decompress_with_limit(data, max)` and
      `decompress_multi_frame_with_limit(data, max)` (new in 0.4.2)
- [x] `ZstdStreamDecoder<R>` re-based on `ZstdStream`: 64 KiB staging buffers,
      `Interrupted` retried, `WouldBlock` propagated, inner `Ok(0)` switches to
      `FlushMode::Finish` (truncation is an error, not a short read),
      no-progress is an `Err` rather than a spin; `with_max_output` /
      `with_max_window` / `unused_input` added, every prior public item kept,
      `decompressed_size()` re-documented as "produced so far" (new in 0.4.2)
- [x] Async adapters behind the `async-io` feature: `AsyncZstdReader<R>`
      (`tokio::io::AsyncRead`, `Poll::Pending`-safe) and
      `AsyncZstdDecompressor` (`oxiarc_core::async_io::AsyncDecompressor`),
      both bounded and built on `ZstdStream` (new in 0.4.2)
- [x] `ZstdDecoder::reset` now clears the literals Huffman table and the three
      sequence FSE tables as well as the repeat offsets, so a reused decoder no
      longer accepts a `Treeless`/`Repeat` block at the start of a new frame
      using the previous frame's tables (defect 5.5 of the streaming-truth
      audit, fixed in 0.4.2)
- [x] Hardened frame-header handling: `try_reserve`-bounded output allocation against untrusted `Frame_Content_Size`, and a bounded `Window_Descriptor` for large one-shot frames (new in 0.3.6)
- [x] Every FSE/Huffman table and state index bounds-checked; malformed or
      truncated input returns `Err` (60k-case mutation fuzz: zero panics)
- [x] `BlockType`/`LiteralsBlockType` marked `#[non_exhaustive]`; `lz77`/`bitwriter` advanced re-exports demoted to `#[doc(hidden)]` (API freeze, new in 0.3.6)
- [x] All features tested (274 tests + 12 doctests passing; 12 of the 274 are the live `zstd-oracle` differential suite, which self-skips without the `zstd` CLI)

## Milestone: COMPLETE

All features implemented and tested. API is stable.

## Ratio (resolved 2026-08-04)

Custom block-optimal `FSE_Compressed_Mode` sequence tables **are** emitted:
`src/fse_encoder.rs` carries reference-faithful ports of `FSE_normalizeCount`
(including the `FSE_normalizeM2` fallback) and `FSE_writeNCount`, and
`src/compressed_block.rs` picks per category (literal length / offset / match
length) whichever of RLE, predefined and custom costs fewest bits *including*
the table description. Measured on a 1.5 MB structured-record corpus:
173,521 -> 109,359 bytes at level 1 (-37 %).

## Bounded-decoding scope notes (0.4.2)

- A block's *interior* — the Huffman/FSE table descriptions and the backward
  sequence bitstream — is decoded atomically once the whole block payload is
  buffered. RFC 8878 caps a block at 128 KiB, so the carry is bounded; a
  mid-bitstream state machine would have the same worst-case memory.
- The push decoder resolves matches against a real window ring, so an offset
  larger than the frame's own declared window is rejected where the legacy
  whole-output `Vec` decoder accepted it. Every reference frame in the
  `zstd-oracle` matrix (levels 1-22, `--long=24`, `--no-check`,
  `--no-content-size`, dictionaries, multi-frame) decodes byte-identically.
- `ZstdStream` defaults to an 8 MiB declared-window ceiling. `ZstdStreamDecoder`,
  the async adapters and the bounded one-shot helpers leave it unrestricted,
  because `zstd --long` frames declare 16-128 MiB; their memory is bounded by
  the output budget plus lazy window growth instead.
- `ZstdStream` errors on a non-Zstandard magic at frame 0 (the legacy
  `decompress_multi_frame` returned `Ok(empty)`); trailing garbage *after* at
  least one complete frame still ends the stream gracefully and is recoverable
  via `unused_input()`.
- The window ring's *first* allocation is never driven by a declared header
  field: `Frame_Content_Size` and `Window_Size` are attacker-controlled, so the
  ring starts at one block (128 KiB) and doubles only as real bytes arrive. An
  18-byte frame declaring a terabyte of content allocates 128 KiB, not 8 MiB
  (regression test `declared_content_size_cannot_force_an_allocation`).
- The wrapped-ring path is gated by `oracle_incremental_small_window_large_payload`:
  4 MiB of payload through reference frames declaring 1-128 KiB windows
  (`--zstd=wlog=10/11/17`, `--long=17`), decoded byte-identically at three chunk
  schedules with the ring never exceeding the declared window. Every other oracle
  input is smaller than zstd's 2 MiB default window, so without that leg the
  wrap arithmetic would never be exercised against real frames.
- Throughput (interleaved A/B, best of 40, 1 MiB payloads): incremental decode is
  0.92x the one-shot path on entropy-coded data — the price of the one extra
  ring-to-caller copy that bounded memory requires — and 2.10x on raw-block data.
  The audit gate is 0.85x.

## Pending

- [x] Add `with_progress` / `with_cancel` builders to zstd codecs (done 2026-05-06)
  - **Goal:** `ZstdEncoder`, `ZstdStreamEncoder<W>`, `ZstdStreamDecoder<R>` gain `with_progress` and `with_cancel` builders. Per-block hooks.
  - **Design:** Mirror bzip2 template. `ZstdEncoder` (encode.rs:33) — progress once after compress, cancel at start. `ZstdStreamEncoder` (streaming.rs:51) + `ZstdStreamDecoder` (streaming.rs:206) — hook per zstd-block boundary.
  - **Files:** MODIFY `oxiarc-zstd/src/encode.rs`, MODIFY `oxiarc-zstd/src/streaming.rs`, possibly MODIFY `oxiarc-zstd/Cargo.toml`
  - **Tests:** `test_zstd_stream_encoder_progress_reports`, `test_zstd_stream_encoder_cancel_aborts`, same for StreamDecoder
  - **Risk:** low

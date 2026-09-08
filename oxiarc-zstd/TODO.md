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
- [x] Dictionary support (raw-content, interoperable both directions). RFC 8878 §5
      *formatted* dictionaries (`Magic_Number` `0xEC30A437`, what `zstd --train`
      writes) are **refused with a named error** by every entry point that takes
      a dictionary rather than being mistaken for content, and `ZstdStream`
      refuses a frame naming a non-zero `Dictionary_ID` when no dictionary is
      supplied (new in 0.4.2; see the scope notes below)
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
- [x] All features tested (297 tests + 12 doctests passing; 13 of the 297 are the live `zstd-oracle` differential suite, which self-skips without the `zstd` CLI, and 4 are the `tests/mutation_differential.rs` mutate-and-compare suite)

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
- **Dictionaries are raw content only.** RFC 8878 §5 allows two shapes: raw
  content (the bytes *are* the history prefix — what `train_dictionary` emits and
  what `zstd -D` interoperates with in both directions) and *formatted* (magic
  `0xEC30A437` + `Dictionary_ID` + a Huffman literals table + three FSE tables +
  three repeat offsets + content). Only raw content is implemented. A formatted
  dictionary is refused by name (`OxiArcError::UnsupportedMethod`) in
  `ZstdStream::with_dictionary` (checked at each frame header, so it survives
  `reset()`, which clears only the per-stream fault latch), in
  `ZstdStreamDecoder`/the async adapters that wrap it, and in the one-shot
  `decompress_with_dict` / `decompress_multi_frame_with_dict`. Refusing is the
  *safe* behaviour, not a shortcut: frames built against a formatted dictionary
  reference its entropy tables through `Repeat_Mode`, so seeding the window with
  the dictionary's header and tables would return silently wrong bytes. Pinned by
  `formatted_dictionary_is_rejected_rather_than_used_as_content` (hermetic) and
  `oracle_formatted_dictionary_is_refused_not_misdecoded` (a real `zstd --train`
  dictionary, self-skipping). Implementing the formatted shape would mean loading
  its entropy tables into `LiteralsDecoder`/`SequencesDecoder` and its three
  repeat offsets — a well-defined follow-up, deliberately out of the Phase 8
  contract (which specifies `with_dictionary(Vec<u8>)` only).
- **The legacy one-shot decoders and `ZstdStream` enforce the same format rules,
  from shared code** (`frame::require_dictionary`, `frame::block_rfc_max`,
  `frame::charge_block`), after differential fuzzing (`fuzz_zstd_stream`) found
  four inputs the legacy path accepted and `ZstdStream` refused: a frame naming a
  `Dictionary_ID` with no dictionary supplied, a block regenerating more than
  `min(Window_Size, 128 KiB)`, an ~11 MB declared window, and leading garbage
  before any frame. Three are now format errors on both paths; frame-boundary
  classification is shared too (`frame::multi_frame_scan` mirrors the stream's
  state machine, including "a skippable frame does not count as a decoded
  frame"). Pinned by `tests/legacy_hardening.rs`, which drives both paths over
  every case, including the fuzzer's own artifacts.
- **The declared `Window_Size` is the one deliberate split**, and it is about
  memory rather than conformance: `ZstdStream` keeps a real ring and refuses a
  declaration above `with_max_window` (8 MiB default); the one-shot decoders keep
  no ring — their output `Vec` is the window — and accept any declaration;
  `decompress_with_limit` / `decompress_multi_frame_with_limit` refuse only above
  `max(max_output, 128 MiB)`, 128 MiB being `zstd -d`'s own
  `ZSTD_WINDOWLOG_MAX_DEFAULT`. Tying that ceiling to the caller's limit is
  measurably wrong: with `zstd` 1.5.7, a payload piped through `-3` declares a
  2 MiB window and one piped through `--long=24 -6` declares 16 MiB, whatever the
  payload's length and with no `Frame_Content_Size` to fall back on, so
  `Window_Size > limit` rejects ordinary reference frames (it fails
  `oracle_bounded_helpers_on_reference_frames` outright). The 11 MB frame that
  motivated the rule is structurally identical to those, so no declaration-only
  rule can separate them.
- **A skippable frame in front of a Zstandard frame is metadata on every entry
  point** (`frame::skippable_prefix_len`). `zstd -d` decodes `[skippable][frame]`
  exactly like `[frame]`, and so did `ZstdStream` / `decompress_into` /
  `decompress_with_limit`, but `decompress` / `decompress_frame` /
  `ZstdDecoder::decode_frame` stopped at the skippable magic with
  `InvalidMagic` — the last accept/refuse divergence between the two families,
  found by the adversarial sweep in `tests/legacy_verify.rs`. `decompress_frame`
  counts the prefix in the bytes it reports consumed. A *truncated* skippable
  frame stays an error wherever it sits.
- **`ZstdDecoder::decode_frame` resets per-frame state on entry.** Before, a
  decoder reused after a failed frame carried that frame's partial output into
  the next call — silently when the next frame declared neither a checksum nor a
  `Frame_Content_Size` (131 076 bytes returned where 4 were expected). Pinned by
  `legacy_verify::a_reused_decoder_does_not_carry_a_failed_frame_into_the_next_one`.
  `reset()` stays public but is no longer something a caller must remember.
- **The block ceiling is the RFC's, deliberately stricter than `zstd -d` in one
  corner.** RFC 8878 §3.1.1.2.3 caps a block at `min(Window_Size, 128 KiB)`;
  measured against `zstd` 1.5.7, the reference decoder lets a block past that cap
  when the frame also declares a larger `Frame_Content_Size` (e.g. a 1 KiB window
  with `Frame_Content_Size` = 2048 and one 2048-byte `Raw` block decodes there and
  is refused here). No encoder produces such a frame: a `--zstd=wlog=10` frame
  declares a 1 KiB window next to a content size up to 500 KB and still keeps
  every block inside 1 KiB (pinned by
  `legacy_verify::oracle::reference_frames_with_a_tiny_declared_window_still_decode`).
  The gap is pinned in both directions by
  `legacy_verify::oracle::the_block_ceiling_is_stricter_than_the_reference_only_where_the_rfc_says_so`,
  which also asserts this crate is never *more* lenient than the reference.
- Throughput (interleaved A/B, best of 40, 1 MiB payloads): incremental decode is
  0.92x the one-shot path on entropy-coded data — the price of the one extra
  ring-to-caller copy that bounded memory requires — and 2.10x on raw-block data.
  The audit gate is 0.85x.

## Decode throughput (2026-09-08)

The decoder was rebuilt around the reference decoder's data layout after a
TIFF-strip measurement put it ~8x behind `libzstd`. Measurement harness:
`examples/decode_throughput.rs` (criterion-free, interleaved rounds against
`zstd -b -d`, medians **and** best-of, load average printed). Design notes worth
keeping:

- **`FseBitReader` keeps a 64-bit container** (`src/backward_bits.rs`), not a
  per-read byte gather. The gather cost up to five bounds-checked byte loads per
  `read_bits`, and a literals stream calls it once per output byte.
- **`BitCursor` is the register-resident half of it.** A loop that drives
  `&mut FseBitReader` puts the container and the consumed-bit count in memory;
  `detach()` hands the loop a `Copy` cursor instead. Nothing reattaches — a
  decode either runs a bitstream to its end or fails.
- **Reloads are scheduled, not tested.** The literals loop reloads once per four
  symbols per stream (four 12-bit codes fit a fresh container) and the sequence
  loop twice per sequence (offset+match-length extras, then literal-length extra
  plus the three state updates). A conditional reload is a data-dependent branch
  that mispredicts at roughly the rate it fires.
- **Literals decode four streams in lockstep** — that is what RFC 8878's
  four-stream layout is *for*: four independent `peek -> load -> skip` chains
  instead of one. The checked sequential decoder is kept and is re-run over the
  four streams, in order, whenever the fast pass reports anything wrong, so every
  pinned error message and the order in which the four streams report them are
  unchanged.
- **`HuffmanTable::is_complete()`** is computed once per table so the per-symbol
  validity test leaves the inner loop. A table with an undefined prefix is
  refused by `interleaved_pass` rather than trusted.
- **Overlapping matches use pattern doubling** on both paths (`frame::append_match`,
  `window::copy_match`): the run length goes `offset, 2*offset, 4*offset, ...`
  because a back-reference is periodic with every multiple of its offset. The
  old loop was `out[i % offset]` — an integer *division* per output byte.
- **`src/short_copy.rs`** exists because `copy_from_slice`/`copy_within` are
  `memcpy`/`memmove` **calls** at runtime lengths, and a block is a stream of
  3-20 byte runs; `_platform_memmove` was 51 % of the 50 MB text decode. Short
  runs go through a fixed-width word ladder (with an overlapping tail, which
  never writes past the run — the window's next bytes are live history).
- **The ring has no `%`.** `cap` is not a power of two, so every `% cap` was a
  real `udiv`; each is one conditional subtraction now (`window::wrap`).
- **The ring grows in place** and zeroes only the new tail. Allocating a fresh
  buffer per growth zeroed ~2x the final capacity over a doubling sequence.
- **Scratch buffers are reused.** The legacy `ZstdDecoder` now owns `lit_buf`
  and `seq_buf` like `ZstdStream` does (it allocated a fresh `Vec` per block),
  and the literals buffer is grow-only, so its `resize` memset stops happening
  after the first few blocks.
- **`xxhash::read_u64_le`** is one fixed-width load; it was eight
  bounds-checked byte loads and was not being inlined (10 % of a literal-dense
  decode).

Known, deliberate and *not* a pathology: `decompress`/`ZstdDecoder::decode_frame`
return an owned `Vec`, so an incompressible frame pays first-touch page faults
for a fresh buffer on every call, which `decompress_into` (writing into the
caller's buffer) does not. Use `decompress_into` when the output size is known.

## Pending

- [x] Add `with_progress` / `with_cancel` builders to zstd codecs (done 2026-05-06)
  - **Goal:** `ZstdEncoder`, `ZstdStreamEncoder<W>`, `ZstdStreamDecoder<R>` gain `with_progress` and `with_cancel` builders. Per-block hooks.
  - **Design:** Mirror bzip2 template. `ZstdEncoder` (encode.rs:33) — progress once after compress, cancel at start. `ZstdStreamEncoder` (streaming.rs:51) + `ZstdStreamDecoder` (streaming.rs:206) — hook per zstd-block boundary.
  - **Files:** MODIFY `oxiarc-zstd/src/encode.rs`, MODIFY `oxiarc-zstd/src/streaming.rs`, possibly MODIFY `oxiarc-zstd/Cargo.toml`
  - **Tests:** `test_zstd_stream_encoder_progress_reports`, `test_zstd_stream_encoder_cancel_aborts`, same for StreamDecoder
  - **Risk:** low

# OxiArc fuzz harnesses

This directory is a standalone `cargo-fuzz` crate (it is **not** a member of
the root workspace — see the top-level `[workspace]` table in
`fuzz/Cargo.toml` — so it manages its own `Cargo.lock` and does not
participate in the main workspace's dependency resolution).

## Prerequisites

```bash
cargo install cargo-fuzz
rustup component add rust-src --toolchain nightly
```

`cargo-fuzz` requires a nightly toolchain (for `-Z sanitizer=address` /
libFuzzer instrumentation) even though the rest of the OxiArc workspace
builds on stable.

## Building all targets

```bash
cd fuzz
cargo +nightly fuzz build
```

Build a single target:

```bash
cargo +nightly fuzz build fuzz_snappy_crc32c
```

## Running a target

```bash
cargo +nightly fuzz run fuzz_snappy_crc32c
```

Add `-- -max_len=65536` or other libFuzzer flags after a second `--` if
needed, e.g.:

```bash
cargo +nightly fuzz run fuzz_zip_read -- -max_len=1048576 -timeout=10
```

Each run keeps discovered crashes under `fuzz/artifacts/<target>/` and a
growing corpus under `fuzz/corpus/<target>/` (both are fuzz-crate-local,
untracked scratch state — see the ownership note below).

## Seeding corpora

Every target benefits from being seeded with small, valid samples of its
input format before a long fuzzing run, since libFuzzer's coverage-guided
mutation starts from whatever is in `fuzz/corpus/<target>/`. Handy sources
already in this repository:

- `oxiarc-archive/tests/data/*.tar`, `*.zip`, etc. — hand-crafted archive
  fixtures used by the crate's own integration tests.
- Any small, real-world `.gz`/`.zst`/`.lz4`/`.br`/`.sz`/`.lzh`/`.cab`/`.iso`/
  `.7z` sample file works too — a handful of KB is enough as a seed; the
  fuzzer discovers interesting mutations from there.

Example, seeding the zip reader from the archive crate's own test fixtures:

```bash
mkdir -p fuzz/corpus/fuzz_zip_read
cp ../oxiarc-archive/tests/data/*.zip fuzz/corpus/fuzz_zip_read/ 2>/dev/null || true
cargo +nightly fuzz run fuzz_zip_read
```

For the codec-level targets (`fuzz_inflate`, `fuzz_bzip2_decompress`,
`fuzz_lzma_decompress`, `fuzz_zstd_frame`, `fuzz_lz4_decompress`,
`fuzz_brotli_decompress`, `fuzz_snappy_decompress`, `fuzz_lzw_decompress`,
`fuzz_szip_decode`) a quick way to bootstrap a seed corpus is to compress a
handful of small files with each crate's own encoder and drop the output
into the matching `fuzz/corpus/<target>/` directory — the round-trip output
is a guaranteed-valid input for the corresponding decoder.

## Coverage / priority

Targets, in the priority order called out for this hardening pass:

1. **`fuzz_snappy_crc32c`** (highest priority) — `crc32c_sse42`
   (`oxiarc-snappy/src/crc32c.rs:65-95`) performs raw unaligned pointer reads
   over attacker-influenced data; this target sweeps buffer lengths and
   alignments (including a full prefix sweep) to hit every stride/tail
   branch of that routine via the safe, dispatching `crc32c()` entry point.
2. **`fuzz_archive_detect`** (high priority) — `ArchiveFormat::detect` /
   `from_magic` are the first code path any untrusted file hits before
   OxiArc even picks a format-specific parser.
3. All remaining codec (`fuzz_inflate`, `fuzz_gzip_header`,
   `fuzz_zlib_header`, `fuzz_bzip2_decompress`, `fuzz_lzma_decompress`,
   `fuzz_zstd_frame`, `fuzz_lz4_decompress`, `fuzz_brotli_decompress`,
   `fuzz_snappy_decompress`, `fuzz_lzh_decode`, `fuzz_lzw_decompress`,
   `fuzz_szip_decode`) and archive-container (`fuzz_zip_read`,
   `fuzz_tar_read`, `fuzz_cab_read`, `fuzz_iso9660_read`,
   `fuzz_sevenz_header`) entry points.

Every target's contract is the same: feed it arbitrary bytes, and the
decoder must return `Ok(..)` or a structured `Err(..)` — it must never
panic, abort, hang, or (for the ASan-instrumented `cargo fuzz build`)
read/write out of bounds.

## Structured-input targets

A few decoders need more than a raw byte slice — e.g. `oxiarc_lzhuf::decode_lzh`
also takes an `LzhMethod` and a declared uncompressed size, and
`oxiarc_lzw::decompress` also takes an `LzwConfig` and expected size. Those
targets (`fuzz_lzh_decode`, `fuzz_lzw_decompress`, `fuzz_szip_decode`) use
`arbitrary::Unstructured` to peel a handful of header bytes off the front of
the fuzz input to synthesize those parameters (with declared sizes capped
to a sane ceiling so a bogus header can't force an unbounded allocation),
then hand the remaining bytes to the decoder as the compressed payload.

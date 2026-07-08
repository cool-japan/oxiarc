# Contributing to OxiArc

Thanks for your interest in contributing to OxiArc, the Pure Rust
archive/compression workspace! This document describes how to build, test,
and submit changes, and the policies every contribution is expected to
follow.

## Getting Started

```bash
git clone https://github.com/cool-japan/oxiarc
cd oxiarc
cargo build --workspace --all-features
```

Requirements:

- Rust 1.85+ (Edition 2024) — install/update via `rustup update`.
- No external C/Fortran toolchain is required; OxiArc has no C bindings.

## Before You Submit

Run the full local validation pipeline and make sure it is clean:

```bash
# Format
cargo fmt --all -- --check

# Lint (zero warnings, no exceptions)
cargo clippy --workspace --all-features --all-targets -- -D warnings

# Build
cargo build --workspace --all-features

# Tests (unit + integration + doctests)
cargo nextest run --workspace --all-features
cargo test --doc --workspace --all-features

# Optional but encouraged for anything touching unsafe/parsing code:
cargo miri test -p oxiarc-core   # (and any other crate you touched)
```

A pull request that adds warnings, fails `clippy -D warnings`, or breaks
existing tests will not be merged.

## Project Policies

These are non-negotiable project-wide policies (see `CLAUDE.md` for the
canonical source):

1. **Pure Rust policy** — no C/Fortran dependencies. If a C binding is ever
   unavoidable it must be strictly feature-gated and off by default; default
   features must remain 100% pure Rust.
2. **No-warnings policy** — the workspace must compile and lint
   (`cargo clippy --all-features --all-targets`) with zero warnings.
3. **No-unwrap policy** — do not use `.unwrap()`, `.expect()`, or other
   panicking constructs on paths that process untrusted input (archive
   contents, compressed streams, CLI arguments derived from files on disk).
   Return a proper `Result`/`OxiArcError` instead. `.expect()` is acceptable
   only for truly-impossible invariants established immediately above it in
   the same function, and should be rare.
4. **Workspace dependency policy** — declare dependencies with
   `*.workspace = true` in each crate's `Cargo.toml` and manage versions
   centrally in the root `[workspace.dependencies]` table. Do not pin a
   version directly in a member crate's `Cargo.toml`.
5. **Latest-crates policy** — use the latest available stable versions from
   crates.io for any new or bumped dependency.
6. **File-size policy** — keep individual source files under ~2000 lines;
   split large modules (the `splitrs` tool is used internally for this) once
   they approach the limit.
7. **Naming conventions** — standard Rust naming (`snake_case` for
   functions/variables, `UpperCamelCase` for types, `SCREAMING_SNAKE_CASE`
   for constants). Follow `rustfmt` defaults.

## Development Workflow

1. **Fork and branch**
   ```bash
   git checkout -b feature/your-feature-name
   ```
2. **Make focused changes** — prefer small, reviewable commits/PRs over one
   large change spanning many crates.
3. **Add tests** for new functionality and for every bug fix (a regression
   test that fails before the fix and passes after it).
4. **Update documentation** — keep `README.md`, per-crate `README.md`s, and
   rustdoc comments in sync with behavior. Do not describe unimplemented or
   partially-implemented functionality as complete; if something is
   best-effort or has a known limitation, say so explicitly.
5. **Update `CHANGELOG.md`** under an `[Unreleased]`/upcoming-version section
   following [Keep a Changelog](https://keepachangelog.com/en/1.0.0/) style.
6. **Open a pull request** describing the change, its motivation, and how it
   was tested. Reference any related issues.

## Security-Sensitive Code

OxiArc parses untrusted, potentially adversarial input (archives and
compressed streams from arbitrary sources) by design. Contributions that
touch parsing/decoding paths should:

- Bound every allocation derived from an untrusted length/size field (prefer
  `try_reserve`/`try_reserve_exact` over `reserve`, and cap against a sane
  maximum or the actual remaining input size) rather than trusting a
  declared size outright.
- Avoid recursive descent without a depth limit when walking untrusted
  container structures (directories, headers, symlink chains).
- Use constant-time comparison for anything that authenticates/verifies a
  secret (passwords, HMAC/authentication tags).
- Sanitize any path derived from archive entry names before writing to disk
  (reject `..`, absolute paths, drive-prefixes, and symlink escapes — "Zip
  Slip" class issues).

See `SECURITY.md` for how to report a suspected vulnerability privately
instead of via a public issue/PR.

## Testing Conventions

- Use `std::env::temp_dir()` (or a crate already in the dependency tree that
  wraps it, e.g. `tempfile`) for any filesystem-touching test — never
  hardcode a path like `/tmp/...`.
- Prefer small, fast fixtures. Multi-hundred-KB fixtures for compression
  tests significantly slow down the suite (BWT/entropy coding stages are
  particularly sensitive to input size and compression level) — a few KB at
  a representative level is usually enough to exercise the same code paths.
- Property-based tests (`proptest`) and fuzz targets (`cargo fuzz`, under
  `fuzz/`) are welcome additions for parsing/codec code. `fuzz/` is a
  standalone `cargo-fuzz` crate (not a workspace member — it manages its own
  `Cargo.lock`) with 18 existing harnesses (`fuzz_zip_read`, `fuzz_tar_read`,
  `fuzz_lzma_decompress`, `fuzz_zstd_frame`, `fuzz_iso9660_read`, and more —
  see `fuzz/README.md`) covering every container/codec reader; building and
  running them requires a nightly toolchain (`cargo +nightly fuzz run
  <target>`), which is why they are not part of the stable-toolchain
  `cargo nextest` suite.
- `oxiarc-lzhuf` and `oxiarc-archive` have an opt-in, off-by-default
  `lha-oracle` Cargo feature (`oxiarc-lzhuf`'s implies `parallel`) that shells
  out to a real `lha` (Lhasa) CLI to cross-validate OxiArc-produced LZH/LHA
  archives against a genuine third-party implementation. It self-skips
  cleanly when `lha` is not on `PATH`, so it is safe to enable locally
  (`cargo nextest run -p oxiarc-lzhuf -p oxiarc-archive --features
  lha-oracle`) but is not required for the standard validation pipeline
  above.

## Benchmarks

- Benchmarks live under each crate's `benches/` directory and use
  `criterion`.
- Include a mix of data patterns (uniform/repetitive, text, binary/random)
  when benchmarking a codec, since compressors behave very differently
  across these.

## Commit and PR Etiquette

- Write clear, descriptive commit messages explaining *why*, not just *what*.
- Do not use `cargo publish` — releases are cut and published by the
  maintainers.
- Do not force-push over a PR under review without calling it out.

## Questions

Open a GitHub issue for questions, or reach out via
[contact@cooljapan.tech](mailto:contact@cooljapan.tech).

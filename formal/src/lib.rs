//! `oxiarc-formal`: machine-checked obligations for `oxiarc`'s bit-cache,
//! xxhash32 and Huffman-entry API.
//!
//! `oxiarc` is the COOLJAPAN replacement for `zip`/`flate2`/`zstd`/`lz4` and
//! friends. A decompressor reads attacker-controlled bytes and turns them
//! into shift counts, masks and indices, so its bit arithmetic is exactly the
//! code where "no test found a problem" is worth least and "no input violates
//! this" is worth most. This package states such obligations as
//! `#[harness]` entry points over the **public** API of `oxiarc-core`,
//! `oxiarc-lz4` and `oxiarc-deflate`, and records the verdict
//! `cargo formal check` actually returned for each of them.
//!
//! Nothing here is a copy of `oxiarc`: the three crates are ordinary path
//! dependencies (`../oxiarc-core`, `../oxiarc-lz4`, `../oxiarc-deflate`), and
//! `cargo-formal` lowers their reachable bodies into this crate's
//! verification condition through the `dep-crates` key of
//! `[package.metadata.formal]`. Every target is reachable from outside the
//! crate; private helpers appear only as inlined callees.
//!
//! This package is not self-hosting: `cargo-formal` does not depend on
//! `oxiarc`, so nothing here verifies the verifier.
//!
//! # The three builds
//!
//! This crate is exercised in exactly the three ways `oxiformal`'s own docs
//! describe, and each proves something different:
//!
//! 1. **`cargo build`** (plain, stable). Harnesses vanish entirely (neither
//!    `#[cfg(formal)]` nor `#[cfg(all(test, oxiformal_runtime_checks))]`
//!    applies), so this only checks that the package and its three
//!    dependencies type-check on stable. `cargo test` additionally runs
//!    [`harness::plain_tests`], which holds a concrete witness for every
//!    `refuted` row below plus a handful of sanity checks -- ordinary Rust
//!    tests, no solver involved.
//! 2. **`RUSTFLAGS="--cfg oxiformal_runtime_checks" cargo test`**. Every
//!    harness becomes a `#[test]` that loops [`oxiformal::rt::iterations`]
//!    (256 by default) times through a random draw. A harness whose
//!    counterexample is *dense* under uniform random inputs is additionally
//!    marked `#[should_panic]`, because `oxiformal::rt::run_harness`
//!    propagates any panic that is not an `AssumeViolation`. A harness whose
//!    counterexample is a single narrow point in a huge domain is left
//!    unmarked: it is genuinely `refuted` at L1, but a fair-coin fuzzer
//!    essentially never finds the witness in 256 draws, so the test passes --
//!    which is not evidence of correctness.
//! 3. **`cargo +nightly-2026-06-20 check` with `--cfg formal
//!    -Zcrate-attr=feature(register_tool) -Zcrate-attr=register_tool(formal_tool)`**
//!    and a separate `--target-dir`, exactly the flags the `cargo-formal`
//!    driver build uses. This only type-checks the `#[cfg(formal)]` copy of
//!    every harness; it does not run the driver or the solver, so it says
//!    nothing about which harnesses are `proved` or `refuted`.
//!
//! The verdicts in [`harness`] and in `EXPECTED.toml` come from a separate,
//! real `cargo formal check` run and are **measurements, not predictions**.
//! `README.md` names the run, the command and the solver version.
//!
//! # Reading a verdict
//!
//! `proved` means the solver could not find an input that violates the
//! property within the harness's bound (and, for a harness with no explicit
//! `assert(..)`, that the code under test never traps for any input the
//! harness allows). `refuted` means a concrete counterexample exists; the doc
//! comment names it and `plain_tests` runs it. `unknown` means the answer is
//! not known -- either the `unwind` bound was hit before the property could
//! be decided, or the solver returned a model that cargo-formal's mandatory
//! model check refused (`solver-model-rejected`; see `README.md`).
//! `unsupported`/`unverifiable` means the encoder could not reduce the
//! harness to a verification condition at all, and the doc comment says which
//! construct stopped it.

#![forbid(unsafe_code)]

pub mod harness;

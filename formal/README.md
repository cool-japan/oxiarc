# oxiarc-formal

Machine-checked obligations for `oxiarc`'s bit-cache, xxhash32 and
Huffman-entry API, written with [`cargo-formal`](https://github.com/cool-japan/cargo-formal)
and `oxiformal`.

A decompressor reads attacker-controlled bytes and turns them into shift
counts, masks and indices. That is exactly the code where "no test found a
problem" is worth least and "no input violates this" is worth most. This
package states thirteen such properties as `#[harness]` entry points over the
**public** API of `oxiarc-core`, `oxiarc-lz4` and `oxiarc-deflate`, and records
the verdict the solver actually returned for each of them.

It is a standalone package with its own `[workspace]`: it is not a member of
the `oxiarc` root workspace, it is never published (`publish = false`), and
nothing outside this directory is touched by building it.

## What is verified

Nothing here is a copy of `oxiarc`. The three crates are ordinary path
dependencies, and `cargo-formal` lowers their reachable bodies into the
verification condition through `[package.metadata.formal] dep-crates`. Private
helpers (`bulk_load`, `xxhash::read_u32_le`, `round32`) are verified as inlined
callees of the public entry points that reach them, which is the honest scope:
a caller can only ever reach them that way.

| Target | File | Harnesses |
|---|---|---|
| `BitCache::refill_bytes`, `peek_bits`, `consume`, `align_to_byte`, `take_byte`, `refill_bulk` | `oxiarc-core/src/bitstream.rs` | 8 |
| `xxhash32`, `xxhash32_with_seed`, `XxHash32::{with_seed,update,finish}` | `oxiarc-lz4/src/xxhash.rs` | 3 |
| `HuffmanTree::{entry_length, entry_symbol, from_code_lengths}` | `oxiarc-deflate/src/huffman.rs` | 2 |

## The three builds

```sh
# 1. plain, stable: type-checks the package and runs `harness::plain_tests`
#    (17 ordinary tests, including a concrete run of every counterexample the
#    solver reported).
cargo build
cargo test

# 2. randomized execution: every harness becomes a #[test] that draws random
#    inputs. A harness whose counterexample is dense under uniform draws is
#    additionally marked #[should_panic]; both of this package's are.
RUSTFLAGS="--cfg oxiformal_runtime_checks" cargo test

# 3. the driver's own type-check of the #[cfg(formal)] copy of each harness.
RUSTFLAGS="--cfg formal -Zcrate-attr=feature(register_tool) -Zcrate-attr=register_tool(formal_tool)" \
  cargo +nightly-2026-06-20 check --target-dir target/formal-check
```

All three pass with zero warnings, as does
`cargo clippy --all-targets -- -D warnings`.

## Running the verifier

```sh
# `oxiformal` is not on crates.io yet, so both the CLI and the driver come from
# the cargo-formal checkout beside this repository. FORMAL_DRIVER must point at
# the *release* driver binary.
FORMAL_DRIVER=../../cargo-formal/driver/target/release/formal-driver \
  cargo formal check

cargo formal status
```

`cargo formal check` exits non-zero while any obligation is refuted, which is
the intended state here: two of them are refuted on purpose (see below).

## Measured verdicts

**Measured, not predicted.** Every row is from a real `cargo formal check` run
on 2026-09-08 with the release CLI (`cargo-formal` 0.1.0), the release driver
with dependency-body and monomorphic-instance lowering, OxiZ 0.3.3 and rustc
`nightly-2026-06-20`. `EXPECTED.toml` is the machine-readable mirror of this
table, and each harness's doc comment carries the same verdict. The run lowered
**28 dependency bodies** (27 reachable) and **2 monomorphic instances**.

| # | Harness | Property | Verdict |
|---|---|---|---|
| 1 | `refill_bytes_never_overfills_harness` | `assert` (4 sites) | **proved** |
| 2 | `refill_bytes_want_bound_is_asserted_harness` | `panic` | **refuted** — `want = 128` |
| 3 | `peek_bits_within_its_precondition_harness` | `assert` (2 sites) | **proved** |
| 4 | `peek_bits_count_bound_is_necessary_harness` | `panic` | **refuted** — `count = 255` |
| 4 | " | `shift-overflow` (2 sites) | **proved** |
| 5 | `consume_never_underflows_harness` | `assert` (2 sites) | **proved** |
| 6 | `align_to_byte_leaves_whole_bytes_harness` | `assert` (3 sites) | **proved** |
| 7 | `take_byte_is_some_iff_eight_bits_harness` | `assert` (5 sites) | **proved** |
| 8 | `refill_bulk_zero_extends_harness` | `assert` (4 sites) | **proved** |
| 9 | `xxhash32_never_panics_on_a_short_input_harness` | `bounds-check` (126 obligations, 5 sites) | **proved** |
| 9 | " | `slice-range` (31 obligations, 6 sites) | **proved** |
| 9 | " | `unwinding-assertion` (3 sites) | **proved** |
| 10 | `xxhash32_one_shot_equals_streaming_harness` | whole harness | **unsupported** (`aliasing`) |
| 11 | `xxhash32_agrees_with_seed_zero_harness` | `assert` | **timeout** |
| 11 | " | `unwinding-assertion` (3 sites) | **proved** |
| 12 | `entry_round_trip_harness` | `assert` (2 sites) | **proved** |
| 13 | `from_code_lengths_never_panics_harness` | whole harness | **unsupported** (`unsupported-callee`) |

17 property rows over 13 harnesses: **12 proved / 2 refuted / 1 timeout /
2 unsupported**.

### Layer counters

Including the incidental MIR-inserted checks the table above does not
enumerate:

| Layer | Counters |
|---|---|
| `hygiene` | PASS — 0 errors, 0 warnings, 0 notes, 2 files scanned, 0 `unsafe` sites |
| `bmc` | **505 proved / 2 refuted / 1 timeout / 0 unknown / 2 unsupported / 0 unverifiable** over 510 obligations (508 backed by a `vc/NNNN.smt2` reproduction; the 2 `unsupported` harnesses generate none) |
| `contract` | 0 proved / 0 refuted (this package states no `#[requires]`/`#[ensures]` — see the contract candidates below) |
| `theorem` | not run |
| `audit` | `unverifiable` over the lowered dependency bodies: `raw-pointer` 12, `dyn-trait` 1, `float` 1 — all in `oxiarc` items no harness here reaches |
| coverage | 13 harnesses; `annotated/public` 0.0 % (the package has no public functions of its own; coverage counts *home* functions, and every function under test lives in a dependency) |

### `solver-model-rejected`: 0

cargo-formal is pinned to OxiZ **0.3.3**, whose model gate (upstream item
U-Z10) turns a solver model that does not satisfy the verification condition
into `unknown` rather than an invented counterexample. Other packages measured
in this phase carry a large `solver-model-rejected` tail for that reason.
**This package has none**: there is not a single `unknown` in the run, so no
row above moves when the pin advances to the OxiZ release that carries the fix.

The one `timeout` is a different failure and does not move with the pin either:
`xxhash32_agrees_with_seed_zero_harness` asks whether two structurally
identical bit-blasted avalanche circuits can differ. Re-solving that condition
with a four-fold budget (120 000 ms instead of 30 000 ms) still returns
`unknown`, so it is not a borderline miss — it is a missing structural-hashing
optimisation in the bit-blaster, and it is this package's strongest argument
for hash-consing it.

### The two refutations

Both are unchecked preconditions of public functions, and both are stated as
`debug_assert!` in `oxiarc` today:

* `BitCache::refill_bytes` (`oxiarc-core/src/bitstream.rs:208`) asserts
  `want <= 56` and takes `want` straight from its caller. The assertion is
  **not** load-bearing: harness 1 proves the accumulator bound *without* the
  `assume`, because the loop guard `self.len <= 55` (`:212`) caps it whatever
  `want` is. So this is a contract candidate, not a defect.
* `BitCache::peek_bits` (`:134`) asserts `count <= 32`, and here the bound
  **is** load-bearing: for `count` in `33..=63` the mask is wider than the
  `u32` the function returns, and for `count >= 64` the `1u64 << count` at
  `:136` is out of range. Note the split verdict: the `shift-overflow` row is
  *proved*, because the encoder continues along the edge where the assertion
  holds, so the shift is only ever reached with `count <= 32`. The out-of-range
  shift would be reachable only in a build with `-C debug-assertions=off
  -C overflow-checks=on`, which L1 does not model.

`bulk_load`'s own `debug_assert!(*bits <= 55)` (`:71`) has **no public
counterpart**: `refill_bulk` returns early when `self.len > 55` (`:181-183`)
and `BitCache`'s fields are private, so a caller cannot manufacture the
violating state. Harness 8 proves the four shifts inside `bulk_load`
(`:73-76`) instead.

### The two `unsupported` rows

Both are statements about the encoder, not about `oxiarc`:

* `xxhash32_one_shot_equals_streaming_harness` — `aliasing` at
  `oxiarc-lz4/src/xxhash.rs:156:24`. `XxHash32::update` does
  `self.buffer[..remaining].copy_from_slice(&data[pos..])`, and a `&mut`
  borrow of a window whose bounds are symbolic is refused by design. Needs a
  symbolic-window `copy_from_slice`/`index_mut` rule.
* `from_code_lengths_never_panics_harness` — `unsupported-callee` at
  `oxiarc-core/src/error.rs:190:22`, "no `From<&str> for std::string::String`
  implementation in the module set", reached through `build_into`'s error path
  (`oxiarc-deflate/src/huffman.rs:272`). This is **not** where it was predicted
  to stop: `Iterator::sum` (`huffman.rs:309`), `Vec::resize` and
  `Vec::reserve_exact` all have encoder rules now and are walked past. The
  DEFLATE table builder is one `String::from(&str)` rule short of encodable.
  Every public constructor routes through `build_into`
  (`from_code_lengths` `:155`, `from_code_length_code` `:176`,
  `rebuild_from_code_lengths` `:207`, `rebuild_from_code_length_code` `:232`),
  so the private `HuffmanTree::reverse_bits` (`:528`) currently has no
  verifiable public entry point at all.

### A note on `unwind`

The manifest sets `unwind = 16`. The three `xxhash` harnesses override it to 6
with `#[harness(unwind = 6)]`, because every loop they reach is provably
shorter than that for a source of at most four bytes: the 16-byte fast path
(`oxiarc-lz4/src/xxhash.rs:30`) cannot run at all, the 4-byte tail loop
(`:54`) runs at most once, and the byte tail loop (`:61`) at most four times.
The bound is not taken on trust — all six `unwinding-assertion` obligations of
those harnesses came back **proved**, which is exactly the statement "six
unrolls were enough"; had they not been, every verdict of the harness would
have degraded to `unknown`. For comparison, at `unwind = 16` the package
generates 1068 verification conditions instead of 508, and the
`xxhash32_agrees_with_seed_zero_harness` ones take about 20 s each.

## In-source contract candidates

The honest end state is `#[oxiformal::requires]` / `#[ensures]` on the real
`oxiarc` functions, with the proof harnesses beside them. That needs
`oxiformal` on crates.io, so this package states the properties from outside
instead. These are the contracts this trial would write into the source, each
with the evidence that makes it a real finding rather than decoration.

**Reachable through the public API and measured here**

| Where | Contract | Evidence |
|---|---|---|
| `oxiarc-core/src/bitstream.rs:134` `BitCache::peek_bits` | `requires(count <= 32)` | measured `panic = refuted`, counterexample `count = 255`. Load-bearing: the mask is wrong for `33..=63` and the shift at `:136` is out of range from 64. The strongest candidate in `oxiarc`. |
| `oxiarc-core/src/bitstream.rs:127` `peek_mask`, `:134` `peek_bits` | `ensures` "every bit above `available()` reads as zero" (doc `:123-125`) | measured `proved` in harness 8, in its `u32`-visible form |
| `oxiarc-core/src/bitstream.rs:208` `BitCache::refill_bytes` | `requires(want <= 56)` | measured `panic = refuted`, counterexample `want = 128`; documentation-grade, since harness 1 proves the accumulator bound without it |
| `oxiarc-core/src/bitstream.rs:265` `BitCache::take_byte` | `requires(self.available() % 8 == 0)` | the doc at `:257-260` states it ("the cache must already be byte-aligned") and nothing checks it. Harness 7 proves the arithmetic is total for *every* state, so this precondition is about stream semantics, not memory safety — which is precisely why a checked contract is the right home for it |
| `oxiarc-core/src/bitstream.rs:70` `bulk_load` | `requires(*bits <= 55)` | existing `debug_assert!` `:71` and the doc `:60-62`. **Already discharged** by `refill_bulk`'s guard `:181-183`; harness 8 proves the four shifts at `:73-76`. Documentation-grade, not a defect |
| `oxiarc-lz4/src/xxhash.rs:85` `read_u32_le` | `requires(data.len() >= 4)` | four unchecked indices at `:86`. Private; every public caller bounds it first, which is exactly what harness 9's 126 `proved` `bounds-check` obligations state |
| `oxiarc-deflate/src/huffman.rs:599` `entry_length`, `:605` `entry_symbol` | `ensures` the packing round trip | measured `proved` in harness 12 |

**Real, but not harnessable in Phase 2**

| Where | Contract | Why not measured |
|---|---|---|
| `oxiarc-deflate/src/huffman.rs:528` `reverse_bits` | `requires(length <= 15 && code < 1 << length)`, `ensures(reverse_bits(r, length) == code)` | private, and every public route to it (`:155`, `:176`, `:207`, `:232`) goes through the `unsupported` `build_into` |
| `oxiarc-core/src/bitstream.rs:483` `try_fill` (`count <= 57`), `:576` `refill_cache` (`want <= 57`), `:620` `refill_byte` (`bits_in_buffer <= 56`), `:770` `read_bits`, `:801` `peek_bits`, `:1104` `write_bits` (`count <= 32`) | the corresponding `requires` | all sit behind an `R: Read` / `W: Write` bound, which Phase 2 does not encode |

## Files

```
formal/
  Cargo.toml      own [workspace]; the three oxiarc crates + oxiformal by path
  .gitignore      /target, /Cargo.lock
  README.md       this file
  EXPECTED.toml   the measured verdict table, machine-readable
  src/lib.rs      module docs: the three builds, how to read a verdict
  src/harness.rs  the harnesses and their plain-build witness tests
```

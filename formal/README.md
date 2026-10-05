# oxiarc-formal

Machine-checked obligations for `oxiarc`'s bit-cache, xxhash32 and
Huffman-entry API, written with [`cargo-formal`](https://github.com/cool-japan/cargo-formal)
and `oxiformal`.

A decompressor reads attacker-controlled bytes and turns them into shift
counts, masks and indices. That is exactly the code where "no test found a
problem" is worth least and "no input violates this" is worth most. This
package states fifteen such properties as `#[harness]` entry points over the
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
| `HuffmanTree::{entry_length, entry_symbol, from_code_lengths}` | `oxiarc-deflate/src/huffman.rs` | 4 |

## The three builds

```sh
# 1. plain, stable: type-checks the package and runs `harness::plain_tests`
#    (20 ordinary tests, including a concrete run of every counterexample the
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
table, and each harness's doc comment carries the same verdict. The run
lowered **28 dependency bodies** (27 reachable) and **2 monomorphic instances**.

Re-measured on **2026-09-14** with a release CLI **rebuilt from the
cargo-formal tree that carries that day's encoder rules** (the release driver,
the OxiZ 0.3.3 pin and rustc `nightly-2026-06-20` are unchanged; 36 s wall,
exit status 1 for the two intended refutations). Every row below, every layer
counter and both counterexamples came back **unchanged**. One thing did move:
the *reason and site* of `from_code_lengths_never_panics_harness`'s
whole-harness refusal — `aliasing` at
`oxiarc-deflate/src/huffman.rs:363:28` now, no longer `unsupported-callee` at
`oxiarc-core/src/error.rs:190:22`. The verdict, and therefore the row, is the
same; the section "The two `unsupported` rows" below says what changed and why
the old explanation was wrong. Note that the *rows* reproduce on either CLI,
but the new refusal site does not: the `cargo formal check` invocation above
run with a released `cargo-formal` 0.1.0 CLI still stops at the older
`unsupported-callee` site, because the rules that walk past it are not in that
release yet.

Re-measured on **2026-10-05** with a release CLI and driver built from the
cargo-formal tree at commit `cab6f97` plus that day's hygiene-scanner and
conformance-suite changes (neither touches the encoder, the solver or the
driver); OxiZ 0.3.3, rustc `nightly-2026-06-20`, `--jobs 2`, a fresh
`--target-dir`, 34 s wall, exit status 1 for the two intended refutations.
**Two harnesses were added** over the `HuffmanTree` entry accessors — rows 14
and 15 below. Both are `proved`. Every 2026-09 row came back unchanged; the
counters rose by the new harnesses' obligations only: bmc **513 proved /
2 refuted / 1 timeout / 2 unsupported** over 518 obligations (516 with a
reproduction), from 505 / 510 / 508. The run now lowers **39 dependency
bodies** (27 reachable) rather than 28 — the encoder lowers more bodies since
2026-09, with the 27 reachable and every verdict unchanged. The two new
harnesses exercise `entry_length` / `entry_symbol` (`huffman.rs:599` / `:605`)
directly: that they are total over every `u32`, read exactly the documented
bit fields, and ignore the high byte that carries `ENTRY_SUBTABLE`. The two
refutations reproduce with the model values `want = 57` and `count = 255`, each
on an empty source (`want = 57` is the 2026-09-15 value too; the 2026-09-08
run reported `want = 128` on a seven-byte source — both violate `want <= 56`,
and `plain_tests` runs both).

| # | Harness | Property | Verdict |
|---|---|---|---|
| 1 | `refill_bytes_never_overfills_harness` | `assert` (4 sites) | **proved** |
| 2 | `refill_bytes_want_bound_is_asserted_harness` | `panic` | **refuted** — `want = 57` (2026-09-08: `128`) |
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
| 13 | `from_code_lengths_never_panics_harness` | whole harness | **unsupported** (`aliasing`) |
| 14 | `entry_accessors_match_the_bit_layout_harness` | `assert` (2 sites) | **proved** |
| 15 | `entry_accessors_ignore_the_high_byte_harness` | `assert` (2 sites) | **proved** |

19 property rows over 15 harnesses: **14 proved / 2 refuted / 1 timeout /
2 unsupported**. Rows 14–15 are new in the 2026-10-05 re-measure: two
`proved` harnesses over the `HuffmanTree` entry accessors (totality and the
exact bit fields over every `u32`, and that neither accessor reads bits
24..32), beside the `entry_round_trip_harness` the package already had. See the
dated note under *Measured verdicts*.

### Evidence grade

Every `proved` row above is **reproduction only (claim unmet)** under
cargo-formal's default `claim-requires` (`lrat`, `oxilean-verify`,
`external-replay`): it is the pinned solver's `unsat`, reproduced under a fixed
seed and conflict budget, not an independently checked proof, and the default
run says so (`claim unmet 513 of 513 proved`). The same day's
`cargo formal check --evidence lrat` (the same binaries, a fresh
`--target-dir`, `--no-cache`; two runs, identical) raised **0 soundness
incidents** and exited 1 for the two intended refutations. **74 of the 513**
proved obligations carry an LRAT certificate from the bit-level engine, checked
by `oxiz-proof` (8 of the 26 user-written `assert` obligations); in 83 more the
bit-level encoder folded an assertion to the constant `false`, which leaves no
clause set to certify; the other 356 carry no bit-level check at all, because
the pinned solver answered their vacuity question `unsat` and the certificate
pass skips a condition on that answer. OxiZ 0.3.3 has a documented
wrong-`unsat` class (upstream U-Z19; cargo-formal's conformance fixture
`u21_pinned_selector_two_define_funs`, reduced from a deliberately false
assertion over `oxiarc-core`'s `BitCache` that a default run reported `proved`
and `--evidence lrat` refuted), which is why a reproduction alone is not a
proof.

### Layer counters

Including the incidental MIR-inserted checks the table above does not
enumerate:

| Layer | Counters |
|---|---|
| `hygiene` | PASS — 0 errors, 0 warnings, 0 notes, 2 files scanned, 0 `unsafe` sites |
| `bmc` | **513 proved / 2 refuted / 1 timeout / 0 unknown / 2 unsupported / 0 unverifiable** over 518 obligations (516 backed by a `vc/NNNN.smt2` reproduction; the 2 `unsupported` harnesses generate none). *(Was 505 / 510 / 508 before the two entry-accessor harnesses; the +8 proved obligations are their four `assert`s and four incidental `shift-overflow`s.)* |
| `contract` | 0 proved / 0 refuted (this package states no `#[requires]`/`#[ensures]` — see the contract candidates below) |
| `theorem` | not run |
| `audit` | **not run in this invocation** — `layers_run` is `hygiene`, `bmc`, `contract`; see the inventory note below. *(That is the 2026-09 runs. On 2026-10-05 cargo-formal runs it as part of every `check`: `layers_run` is `hygiene`, `bmc`, `contract`, `audit`, and `layers.audit` reads `trusted` 0, `uncovered_unsafe` 0 and `unverifiable_functions` `raw-pointer` 1, `dyn-trait` 1, `float` 1 — the three the inventory note below names.)* |
| coverage | 15 harnesses; `annotated/public` 0.0 % (the package has no public functions of its own; coverage counts *home* functions, and every function under test lives in a dependency) |

The `audit` layer did not run in the 2026-09 runs, so the two histograms this
package can quote come from the run's own `inventory.json` instead. Its
`unverifiable_by_reason` is
`raw-pointer` 1, `dyn-trait` 1, `float` 1 — all three on
`from_code_lengths_never_panics_harness`, the only harness the inventory marks
`unverifiable`. Its impurity histogram over the 15 harnesses (2026-10-05) is
`unknown-callee` 12 — the `oxiformal::any_vec` draw, which 12 of the 15
harnesses make — and `dyn-dispatch` 1, `float` 1, `raw-pointer` 1, all three on
`from_code_lengths_never_panics_harness`, beside the `impure-callee` entries
that follow from the oxiarc bodies the harnesses call;
`entry_round_trip_harness` and the two entry-accessor harnesses draw no `Vec`
and are pure. (The 2026-09 runs recorded the `any_vec` draw as `raw-pointer`
12, the `Vec` deref behind it; the purity analysis no longer counts that deref
as a raw-pointer use.)

### `solver-model-rejected`: 0

cargo-formal is pinned to OxiZ **0.3.3**, whose model gate (upstream item
U-Z10) turns a solver model that does not satisfy the verification condition
into `unknown` rather than an invented counterexample. Other packages measured
in this phase carry a large `solver-model-rejected` tail for that reason.
**This package has none**: there is not a single `unknown` in the run, so no
row above moves when the pin advances to the OxiZ release that carries the fix.

The one `timeout` is a different failure and does not move with the pin either:
`xxhash32_agrees_with_seed_zero_harness` asks whether two structurally
identical bit-blasted avalanche circuits can differ — a miter. The solver gave
up after a measured **30 505 ms** against the manifest's 30 000 ms budget ("the
solver did not finish within 30000 ms"). A separate probe at a four-fold budget
(120 000 ms) still returns `unknown`, so it is not a borderline miss — it is a
missing structural-hashing optimisation in the bit-blaster (OxiZ intake
#P2b-14), and it is this package's strongest argument for hash-consing it.

### The two refutations

Both are unchecked preconditions of public functions, and both are stated as
`debug_assert!` in `oxiarc` today:

* `BitCache::refill_bytes` (`oxiarc-core/src/bitstream.rs:208`) asserts
  `want <= 56` and takes `want` straight from its caller. The assertion is
  **not** load-bearing: harness 1 proves the accumulator bound *without* the
  `assume`, because the loop guard `self.len <= 55` (`:211`) caps it whatever
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

Both are statements about the encoder, not about `oxiarc`, and since the
2026-09-14 encoder rules landed they are the *same* statement: a `&mut` borrow
taken at an index the encoder knows only symbolically. cargo-formal refuses
that by design — it is its TODO P3-26 — and neither row is a finding about
`oxiarc`.

* `xxhash32_one_shot_equals_streaming_harness` — `aliasing` at
  `oxiarc-lz4/src/xxhash.rs:156:24`. The refused callee is
  `<[T; N] as IndexMut<I>>::index_mut`, which "takes a mutable borrow of a
  window whose bounds are not constants": column 24 is the `[..remaining]` of
  `self.buffer[..remaining].copy_from_slice(&data[pos..])` in
  `XxHash32::update`, with `self.buffer: [u8; 16]`.
* `from_code_lengths_never_panics_harness` — `aliasing` at
  `oxiarc-deflate/src/huffman.rs:363:28`. The refused callee is
  `<Vec<T, A> as IndexMut<I>>::index_mut`, which "takes a mutable borrow of a
  symbolically indexed element": column 28 is the `[idx]` of
  `symbols[idx] = symbol as u16` in `build_into`'s symbol-assignment loop,
  whose index is `symbol_offsets[len] + (current_code[len] - base_codes[len])`
  — symbolic in the code lengths the harness draws.

**The second row moved on 2026-09-14, and its old explanation was wrong in an
instructive way.** Until that day it read `unsupported-callee` at
`oxiarc-core/src/error.rs:190:22`, "no `From<&str> for std::string::String`
implementation in the module set", and the natural reading of that — "the
DEFLATE table builder is one `From<&str> for String` rule short" — was **not**
what the encoder was doing. The rule was there. The copy was
refused on *length*: the literal `"Empty code lengths"`
(`oxiarc-deflate/src/huffman.rs:272`) is 18 bytes against this package's
sequence bound of 16, and the generic-conversion path swallowed that error and
reported its own fallback message instead, which named the missing `From` impl
and hid the real cause. With the length check fixed — an owned `String` built
from a `&str` is an exact copy even when the source is longer than the
destination's bound — the encoder now walks straight through the
`impl Into<String>` constructor and on past the `1..=max_length` loop
(`huffman.rs:302`), the `format!` inside `invalid_header(format!(..))`
(`huffman.rs:282`) and `code_lengths.len().next_power_of_two()`
(`huffman.rs:332`), stopping only at the symbol-assignment write above. The
blockers predicted before the very first run (`Iterator::sum` at
`huffman.rs:311`, `Vec::resize`, `Vec::reserve_exact`) are walked past as well.

The measured effect on this package is **none**. The layer counters are
unchanged at 505 proved / 2 refuted / 0 unknown / 1 timeout / 2 unsupported
over 510 obligations, and the harness still mints no verification condition —
what changed is the diagnosis, from a string model to a symbolic `IndexMut`
write. Every public constructor routes through `build_into`
(`from_code_lengths` `:155`, `from_code_length_code` `:176`,
`rebuild_from_code_lengths` `:207`, `rebuild_from_code_length_code` `:232`), so
the private `HuffmanTree::reverse_bits` (`:528`) still has no verifiable public
entry point at all.

### A note on `unwind`

The manifest sets `unwind = 16`. The three `xxhash` harnesses override it to 6
with `#[harness(unwind = 6)]`, because every loop they reach is provably
shorter than that for a source of at most four bytes: the 16-byte fast path
(`oxiarc-lz4/src/xxhash.rs:30`) cannot run at all, the 4-byte tail loop
(`:54`) runs at most once, and the byte tail loop (`:61`) at most four times.
The bound is not taken on trust — all six `unwinding-assertion` obligations of
those harnesses came back **proved**, which is exactly the statement "six
unrolls were enough"; had they not been, every verdict of the harness would
have degraded to `unknown`. A separate probe at the manifest's `unwind = 16`
generated 1068 verification conditions instead of this run's 508, and the
`xxhash32_agrees_with_seed_zero_harness` ones took about 20 s each.

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
| `oxiarc-core/src/bitstream.rs:208` `BitCache::refill_bytes` | `requires(want <= 56)` | measured `panic = refuted`, counterexample `want = 57` (2026-09-08: `128`); documentation-grade, since harness 1 proves the accumulator bound without it |
| `oxiarc-core/src/bitstream.rs:265` `BitCache::take_byte` | `requires(self.available() % 8 == 0)` | the doc at `:249-251` states it ("the cache must already be byte-aligned") and nothing checks it. Harness 7 proves the arithmetic is total for *every* state, so this precondition is about stream semantics, not memory safety — which is precisely why a checked contract is the right home for it |
| `oxiarc-core/src/bitstream.rs:70` `bulk_load` | `requires(*bits <= 55)` | existing `debug_assert!` `:71` and the doc `:60-62`. **Already discharged** by `refill_bulk`'s guard `:181-183`; harness 8 proves the four shifts at `:73-76`. Documentation-grade, not a defect |
| `oxiarc-lz4/src/xxhash.rs:85` `read_u32_le` | `requires(data.len() >= 4)` | four unchecked indices at `:86`. Private; every public caller bounds it first, which is exactly what harness 9's 126 `proved` `bounds-check` obligations state |
| `oxiarc-deflate/src/huffman.rs:599` `entry_length`, `:605` `entry_symbol` | `ensures` the packing round trip; `ensures(entry_length(e) == e & 0xFF)` and `ensures(entry_symbol(e) == (e >> 8) & 0xFFFF)` for every `e` | measured `proved` in harnesses 12, 14 and 15 (round trip for legal lengths; totality, the exact bit fields and high-byte independence over every `u32`) |

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

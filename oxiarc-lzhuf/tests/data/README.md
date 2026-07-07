# LZH / LHA reference corpus (external, genuine)

This directory holds `.lzh` fixture archives that were **produced by genuine,
independent LHA-family tools** (not by OxiArc), together with the exact
decompressed plaintext of each (`*.expected`). They exist to let the LZH `-lh5-`
spec-compatibility work be validated against real-world archives rather than
against OxiArc's own round-trip.

## Provenance summary

- **Archive source:** the test-archive collection of the
  [`fragglet/lhasa`](https://github.com/fragglet/lhasa) project (a clean-room
  LHA *extraction* tool), directory `test/archives/`. Each archive there was
  created by a specific historical LHA implementation (named by the archive's
  upstream sub-directory, e.g. `lha_unix114i` = *LHa for UNIX 1.14i*).
- **Fetched:** 2026-07-07, as the repository tarball
  `https://codeload.github.com/fragglet/lhasa/tar.gz/refs/heads/master`
  (a single request; the per-file raw CDN was rate-limiting bursts).
- **Upstream revision:** `master` @ commit
  `75ed83559f23e9538e0045c62f53f77ab03d03d6`.
- **Decompression oracle:** `Lhasa v0.6.0` (Simon Howard), installed via
  Homebrew (`brew install lhasa`; binary `lha`, `/opt/homebrew/bin/lha`).

### Oracle direction: DECOMPRESS-ONLY

`lha` (Lhasa 0.6.0) can **list / test / extract / print** only. It has **no
create/add command** (`usage: lha [-]{lvtxep...}`). It is therefore a *one-way*
oracle:

- It can be used to **read/verify** archives (`lha t` checks the stored CRC-16;
  `lha x` extracts and CRC-verifies).
- It **cannot** compress, so we cannot mint new `-lh5-` archives with it, and we
  cannot byte-compare an OxiArc-produced archive against an `lha`-produced one.

Consequence for the later encode-direction oracle test: it must shell out to
`lha t` / `lha x` on the *OxiArc-produced* archive and diff the extracted bytes
against the original plaintext — **not** compare compressed bytes byte-for-byte
against an lhasa archive, because two conformant encoders may emit different
(but equally valid) compressed streams for the same input.

## How each plaintext was obtained and verified

For every `NAME.lzh` fixture:

1. `lha t NAME.lzh` was run — all fixtures report **CRC OK** (the archive's own
   stored CRC-16 validates its payload).
2. `lha xfqw=<tmp> NAME.lzh` extracted the single member; that member's exact
   bytes are stored here as `NAME.expected`. Because extraction is CRC-checked
   by `lha`, a successful extraction means the plaintext matches the checksum
   embedded by the original (independent) encoder.

The four `-lh5-` archives that carry the GPL-2 text all decompress to the
**byte-identical** file (sha256 `8177f975…`, 18 092 bytes) despite coming from
different tools (LHa for UNIX 1.14i and LHA 2.55e for DOS) and different header
levels — an independent cross-tool confirmation of the ground truth.

## Fixtures

| Archive (`*.lzh`)             | Method  | Hdr lvl | OS  | Packed  | Unpacked  | CRC-16 | Member (internal name) | Plaintext (`*.expected`) |
|-------------------------------|---------|:------:|:---:|--------:|----------:|:------:|------------------------|--------------------------|
| `lha_unix114i_h0_lh0.lzh`     | `-lh0-` |   0    | –   |   6 829 |     6 829 | `b6d5` | `gpl-2.gz`             | 6 829 B (raw gzip bytes) |
| `lha_unix114i_h0_lh5.lzh`     | `-lh5-` |   0    | –   |   6 996 |    18 092 | `a33a` | `gpl-2`                | 18 092 B                 |
| `lha_unix114i_h1_lh5.lzh`     | `-lh5-` |   1    | `U` |   6 996 |    18 092 | `a33a` | `gpl-2`                | 18 092 B                 |
| `lha_unix114i_h2_lh5.lzh`     | `-lh5-` |   2    | `U` |   6 996 |    18 092 | `a33a` | `gpl-2`                | 18 092 B                 |
| `lha213_lh5_long.lzh`         | `-lh5-` |   1    | `M` |  84 000 | 1 241 658 | `6a7c` | `long.txt`             | 1 241 658 B (multi-block)|
| `lha255e_lh5.lzh`             | `-lh5-` |   1    | `M` |   7 004 |    18 092 | `a33a` | `gpl-2`                | 18 092 B                 |

OS byte: `U` = UNIX, `M` = MS-DOS, `–` = level-0 (no OS identifier field).

### Why these were chosen

- **`-lh5-` across header levels 0 / 1 / 2** (`lha_unix114i_h{0,1,2}_lh5`): the
  primary target method, from the canonical *LHa for UNIX 1.14i* (the reference
  implementation whose MSB-first bit I/O `oxiarc-core::msb_bitstream` mirrors),
  exercising all three header-level layouts over identical payload.
- **Cross-tool `-lh5-`** (`lha255e_lh5`): same GPL-2 payload compressed by a
  *different* encoder (LHA 2.55e, DOS) — guards against over-fitting to one
  encoder's bit choices.
- **Large multi-block `-lh5-`** (`lha213_lh5_long`): 1.24 MB of text packed to
  84 KB, which spans many Huffman blocks (re-sent code tables) — the case a
  single-block decoder silently gets wrong.
- **`-lh0-` stored baseline** (`lha_unix114i_h0_lh0`): no compression; a trivial
  ground-truth anchor and a binary payload (a gzip stream) for the stored path.

## Upstream paths (for re-fetch / traceability)

All under `https://github.com/fragglet/lhasa/tree/master/test/archives/`:

| Local file                    | Upstream path                          |
|-------------------------------|----------------------------------------|
| `lha_unix114i_h0_lh0.lzh`     | `lha_unix114i/h0_lh0.lzh`              |
| `lha_unix114i_h0_lh5.lzh`     | `lha_unix114i/h0_lh5.lzh`              |
| `lha_unix114i_h1_lh5.lzh`     | `lha_unix114i/h1_lh5.lzh`              |
| `lha_unix114i_h2_lh5.lzh`     | `lha_unix114i/h2_lh5.lzh`              |
| `lha213_lh5_long.lzh`         | `lha213/lh5_long.lzh`                  |
| `lha255e_lh5.lzh`             | `lha255e/lh5.lzh`                      |

## sha256 manifest

```
568edb96b0cf40d219a94146d51f679da49f5046db3475c4aaf03e5f24504de0  lha_unix114i_h0_lh0.lzh
cf709905a6d89fb407fd283cccd2a0f9f544b1fb8d78e97af218e8ddaa977d77  lha_unix114i_h0_lh5.lzh
511e1d679cbedcb24b95d7578ff1553100878de7c6f0729ce159f637b866acb5  lha_unix114i_h1_lh5.lzh
67fa06a23f8b85373fff2a8bb3bfc13e3ab491b82c0c2933fa187305bd594ad6  lha_unix114i_h2_lh5.lzh
8bc9c2c20bf12d0fa50a8ffbfae5e2fc7af88f63fc3be894fa2aff633db37877  lha213_lh5_long.lzh
7f10d0be69536733217e984e4ff81d8980f2df06822581d7c006257bfad34851  lha255e_lh5.lzh
5c423e9bdf915d23972369959f5a71bfbcc1d32d09fb8d7198755861d289966e  lha_unix114i_h0_lh0.expected
8177f97513213526df2cf6184d8ff986c675afb514d4e68a404010521b880643  lha_unix114i_h0_lh5.expected
8177f97513213526df2cf6184d8ff986c675afb514d4e68a404010521b880643  lha_unix114i_h1_lh5.expected
8177f97513213526df2cf6184d8ff986c675afb514d4e68a404010521b880643  lha_unix114i_h2_lh5.expected
1211b353951c19b6e69a28c1f7ed5bdf123015e6e22b5d3109135c76f8488188  lha213_lh5_long.expected
8177f97513213526df2cf6184d8ff986c675afb514d4e68a404010521b880643  lha255e_lh5.expected
```

## Licensing note

The compressed payload in the `-lh5-`/`-lh0-` fixtures is the text of the
GNU General Public License v2 (`gpl-2`) and a large public text (`long.txt`),
carried verbatim inside the upstream `fragglet/lhasa` test suite (GPL-licensed
project). They are included here purely as binary test vectors for format
interoperability.

# oxiarc-cli - Development Status (v0.4.2, 2026-09-08)

## Completed Features (COMPLETE)

### Commands
- [x] `list` (alias: `l`) - List archive contents
  - [x] Simple file listing
  - [x] Verbose mode with sizes and ratios
  - [x] ZIP format support
  - [x] TAR format support
  - [x] LZH format support
  - [x] GZIP format support

- [x] `extract` (alias: `x`) - Extract files
  - [x] ZIP extraction with directory creation
  - [x] GZIP extraction
  - [x] Output directory option (`-o`)

- [x] `info` (alias: `i`) - Show archive information
  - [x] Format detection
  - [x] File metadata
  - [x] Content statistics

- [x] `detect` - Detect archive format
  - [x] Magic byte display
  - [x] Format classification
  - [x] PNG/JPEG/TIFF image recognition (Phase 8 wave 3, 2026-09-07): a
        CLI-layer magic-byte fallback (`src/image_probe.rs`) consulted only
        when `ArchiveFormat::detect_with_path` reports `Unknown` — never a
        new `ArchiveFormat` variant (see `repo-conventions.md` §13's cost
        table). Prints dimensions, colour type/bit depth and compression.
  - [x] `info`'s image path additionally prints a chunk/segment/IFD summary
        (PNG chunks via a hand-rolled container walk; JPEG marker segments
        up to `SOS`; TIFF via `Decoder::all_tags()`).
  - [x] `list`/`extract`/`test`/`convert`/`add` refuse a recognised image
        with an enriched "this looks like a PNG/JPEG/TIFF image, not an
        archive" error (`utils::image_format_hint`) instead of a bare
        "unrecognized format" message.

### Infrastructure (332 lines)
- [x] clap-based argument parsing
- [x] Subcommand structure
- [x] Error handling with exit codes
- [x] Format auto-detection
- [x] Path sanitization for security

## Future Enhancements

### Commands Status
- [x] `create` (alias: `c`) - Create archives (COMPLETED)
  - [x] ZIP creation
  - [x] GZIP creation
  - [x] TAR creation
  - [x] LZH creation (LH0 stored mode and LH5 compression)
  - [x] XZ creation
  - [x] LZ4 creation
  - [x] Zstandard creation
  - [x] Bzip2 creation
  - [x] Brotli creation (.br/.brotli)
  - [x] Snappy creation (.sz/.snappy)
  - [x] Compression level option

- [x] `test` (alias: `t`) - Test archive integrity (COMPLETED)
  - [x] CRC verification
  - [x] Header validation
  - [x] Report corrupted entries
  - [x] Brotli and Snappy format testing support

- [x] `convert` - Convert between formats (COMPLETED)
  - [x] ZIP/TAR/GZIP/LZH/XZ/7z/CAB/LZ4/Zstd/Bzip2/Brotli/Snappy interconversion
  - [x] Refuses to silently overwrite an existing output file (0.3.6 fix —
        previously truncated it silently)

### Extract Features
- [x] TAR extraction
- [x] LZH extraction
- [x] ZIP extraction
- [x] GZIP extraction
- [x] XZ extraction
- [x] 7z extraction
- [x] CAB extraction
- [x] LZ4 extraction
- [x] Zstandard extraction
- [x] Bzip2 extraction
- [x] Brotli extraction (.br/.brotli)
- [x] Snappy extraction (.sz/.snappy)
- [x] File pattern filtering (include/exclude)
- [x] Progress bars
- [x] Preserve timestamps
- [x] Preserve permissions
- [x] Overwrite prompts
- [x] Skip existing files
- [x] Real symlink creation for archive entries that declare one (currently
      only TAR reads populate symlink metadata), instead of silently
      following/overwriting through them; Windows falls back to a warning
      when `SeCreateSymbolicLinkPrivilege` is unavailable (0.3.6)
- [x] `--progress`/`-P` is a genuine opt-in flag, off by default (0.3.6 fix
      — previously inert, defaulting to `true` regardless of the flag)
- [x] Filesystem errors during `create`/`add`/`extract` name the offending
      path instead of a bare I/O error (0.3.6)
- [x] `unreachable!()` at the end of `cmd_extract` replaced with a proper
      `Err(...)` return, per the no-panic policy (0.3.6)

### List Improvements
- [x] JSON output (`--json`)
- [x] Sorting options (name, size, date, ratio)
- [x] Filter by pattern
- [x] Show modification times
- [x] Tree view (verified 2026-04-20 with `cli_tree.rs` integration test — guards `├`/`└` connectors and nested path rendering)

### Create Command Options
- [x] Compression level (`-l store|fast|normal|best`)
- [x] Recursive directory inclusion
- [x] Exclude patterns
- [x] Store vs. compress threshold (2026-04-20 — `--compress-threshold N`, files below N bytes bypass deflate; verified with `cli_create_threshold.rs`)
- [x] Add files to existing archive (2026-04-20 — `oxiarc add <archive> <files>` for ZIP/TAR/LZH, with `--dry-run`; verified with `cli_add.rs`)
- [x] Symlink-aware directory traversal for `create`/`add` — `symlink_metadata` (never follows links) plus a visited-canonical-path `HashSet` guarding directory cycles (0.3.6 fix; previously could infinite-loop or dereference-and-copy through a circular/malicious symlink)

### User Experience
- [x] Progress bars (indicatif)
- [x] Colored output (colored/termcolor) (planned 2026-04-20) — verified with integration tests across list/info/detect/extract + NO_COLOR env
  - **Goal:** `oxiarc list`, `oxiarc info`, and `oxiarc detect` produce colored output on terminals, plain output when piped, with a global `--color {auto,always,never}` flag. Errors → red; dirs/files/symlinks → distinct hues.
  - **Design:** Add `owo-colors` (latest, pure Rust) and `supports-color` (latest, pure Rust) to `oxiarc-cli/Cargo.toml`. New `src/style.rs` (~80 lines) centralizes `ColorChoice` enum, TTY detection (`NO_COLOR`/`FORCE_COLOR` respected), and styled helpers. Root clap command gains `#[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]`. Four commands (`list`, `info`, `detect`, `extract --verbose`) route output through style helpers.
  - **Files:** `oxiarc-cli/Cargo.toml` (MODIFY +2 deps), `src/style.rs` (NEW), `src/main.rs` (MODIFY), `src/list.rs`, `src/info.rs`, `src/detect.rs`, `src/extract.rs` (MODIFY)
  - **Prerequisites:** none
  - **Tests:** unit in style.rs (Auto+no-tty→plain, Always→ANSI, Never→plain); integration via assert_cmd (`oxiarc list --color=never` → no ESC sequences)
  - **Risk:** owo-colors is pure Rust; no policy issue
- [ ] Interactive mode
- [x] Verbose/quiet modes (`-v`, `-q`)
- [x] Global `--quiet`/`-q` flag applied consistently across every subcommand (0.3.6)
- [x] Dry-run mode (`--dry-run`, `-n`) for create and extract commands

### I/O Options
- [x] Stdin/stdout support (`-`) — `extract` already accepted `-`; `list`,
      `test`, `info`, and `detect` gained it in 0.3.6
- [x] Password for encrypted archives (2026-04-20 — `--password` on `extract` plus interactive prompt via `rpassword`; wrong password exits 2; verified with `cli_password.rs`)
- [ ] Multi-volume archives
- [x] CLI `--memory-limit <BYTES>` for extract/list (completed 2026-05-06)
  - **Goal:** `oxiarc extract --memory-limit 100M` and `oxiarc list --memory-limit 100M` cap peak in-memory allocation per entry. Entry exceeding cap → clear error with entry name + required vs allowed bytes. Default = unlimited.
  - **Design:** Add `#[arg(long, value_parser = parse_byte_size)] memory_limit: Option<u64>` to `Commands::Extract` and `Commands::List` in main.rs. Helper `parse_byte_size(s: &str) -> Result<u64>`: accepts `100`, `100K`, `100M`, `100G` (decimal multipliers 1000/1_000_000/1_000_000_000). In extract.rs and list.rs pre-flight check: compare `entry.uncompressed_size()` against limit; error: `"entry '{name}' requires {req} bytes, exceeds --memory-limit {lim}"`. Place helper in `oxiarc-cli/src/util.rs` (reuse if exists).
  - **Files:** MODIFY `oxiarc-cli/src/main.rs`, MODIFY `oxiarc-cli/src/commands/extract.rs`, MODIFY `oxiarc-cli/src/commands/list.rs`, possibly NEW/MODIFY `oxiarc-cli/src/util.rs`
  - **Tests:** unit `parse_byte_size` tests (100→100, "100K"→100_000, "garbage"→Err); CLI integration: ZIP with 1KB + 1MB + `--memory-limit 10K` → 1KB ok, 1MB errors with entry name; default-unlimited test
  - **Risk:** very low — pre-flight check only

### Platform
- [x] Shell completion scripts (bash, zsh, fish, powershell) - COMPLETED
  - [x] Hidden `completion` subcommand
  - [x] Bash completion generation
  - [x] Zsh completion generation
  - [x] Fish completion generation
  - [x] PowerShell completion generation
  - [x] Installation instructions in README
- [x] Man page generation (planned 2026-04-20) — verified with integration test; generates .TH/.SH-compliant roff files
  - **Goal:** `oxiarc man [OUTPUT_DIR]` writes mandoc-format man pages (one per subcommand + top-level) to a given directory. Default output dir: `./man/`.
  - **Design:** Add `clap_mangen = "0.2"` (latest, pure Rust) to `oxiarc-cli/Cargo.toml`. New `src/commands/man.rs` (~70 lines) or `src/man.rs` (mirroring existing command layout) builds `clap_mangen::Man` for each subcommand via `Cli::command()` tree, writes `oxiarc-<subcmd>.1` and `oxiarc.1`.
  - **Files:** `oxiarc-cli/Cargo.toml` (MODIFY +1 dep), new man command file (MODIFY/NEW), `src/main.rs` (MODIFY — register subcommand)
  - **Prerequisites:** none
  - **Tests:** integration via assert_cmd (`oxiarc man /tmp/<tempdir>` → oxiarc.1 and oxiarc-list.1 exist, start with `.TH OXIARC`). Use `std::env::temp_dir()`.
  - **Risk:** none; clap_mangen is well-trodden
- [x] Windows-specific handling (2026-04-20 — `src/windows.rs` sanitizes reserved basenames CON/PRN/AUX/NUL/COM1-9/LPT1-9 on extract, appends `_` to stem, `--strict-names` refuses; long paths auto-prefixed with `\\?\` on Windows; verified with `cli_windows_names.rs`)
  - [x] Fixed long-path (`\\?\`) and reserved device-name sanitization for a
        previously-unhandled trailing `.`/space on the final path component (0.3.6)
- [x] `man/` troff man pages regenerated for every subcommand and relocated
      under `oxiarc-cli/` (was elsewhere); shell completions (bash/zsh/fish/
      PowerShell) regenerated to match the 0.3.6 flag set (0.3.6)

### Security Hardening (0.3.6)

- [x] Zip-Slip/path-traversal hardening: `sanitize_relative_path` (in
      `src/windows.rs`) now treats both `/` and `\` as separators and strips
      `.`/`..`/root/drive-prefix components (previously a bare `..`
      component could pass through); all archive readers (ZIP, 7z, CAB, LZH,
      ISO 9660) route extracted names through it during `extract`, and
      `resolve_output_path` (in `src/commands/extract.rs`) independently
      rejects any join that would resolve outside the output root.

## Test Coverage

- 121 tests passing (16 integration-test files under `tests/` — including
  `cli_image_detect` new in 0.4.2 (Phase 8: `detect`/`info` PNG/JPEG/TIFF
  recognition, plus every other subcommand's image-aware refusal) — plus
  library unit tests in `main.rs`/`utils.rs`/`windows.rs`/`style.rs`/
  `image_probe.rs`/`commands/extract.rs`/`commands/man.rs`)
  (`cargo nextest run -p oxiarc-cli --all-features`, measured 2026-09-08)

## Code Statistics

| File | Lines (code) |
|------|---------------|
| commands/extract.rs | 1,413 |
| commands/create.rs | 571 |
| utils.rs | 453 |
| commands/add.rs | 434 |
| image_probe.rs | 427 |
| commands/convert.rs | 419 |
| main.rs | 377 |
| commands/list.rs | 350 |
| commands/test.rs | 334 |
| windows.rs | 231 |
| commands/info.rs | 203 |
| style.rs | 152 |
| commands/detect.rs | 67 |
| commands/man.rs | 56 |
| commands/mod.rs | 27 |
| **Total** | **5,514** |

(measured via `tokei oxiarc-cli/src`, code lines only — excludes comments/blanks;
new in 0.4.2: `image_probe.rs`, the PNG/JPEG/TIFF magic-sniffing/summary module
behind `detect`/`info`'s CLI-layer image fallback, Phase 8 Wave 3)

## Command Reference

Verbatim `oxiarc --help` output (regenerated 2026-09-08 from the built
binary; `completion` is `#[command(hide = true)]` and correctly does not
appear here).

```
OxiArc is a Pure Rust implementation of common archive formats.
Supported formats: ZIP, GZIP, TAR, LZH, XZ, 7z, LZ4, Zstd, Bzip2, Brotli, Snappy

Examples:
  oxiarc list archive.zip
  oxiarc list archive.7z
  oxiarc list --json --tree archive.zip
  oxiarc extract archive.zip
  oxiarc extract archive.7z
  oxiarc extract data.xz
  oxiarc extract data.lz4
  oxiarc extract data.zst
  oxiarc extract data.bz2
  oxiarc extract data.br
  oxiarc extract data.sz
  oxiarc extract --password secret --lenient archive.zip
  oxiarc extract --memory-limit 512M archive.zip
  oxiarc create archive.zip file1.txt file2.txt
  oxiarc create data.xz file.txt
  oxiarc create data.lz4 file.txt
  oxiarc create data.bz2 file.txt
  oxiarc create data.br file.txt
  oxiarc create data.sz file.txt
  oxiarc add archive.zip newfile.txt
  oxiarc add archive.tar dir/
  oxiarc convert archive.lzh output.zip
  oxiarc convert archive.7z output.zip
  oxiarc test archive.lzh
  oxiarc info archive.7z
  oxiarc detect photo.jpg
  oxiarc info image.png
  oxiarc man ./man


Usage: oxiarc [OPTIONS] <COMMAND>

Commands:
  list     List contents of an archive
  extract  Extract files from an archive
  test     Test archive integrity
  create   Create a new archive
  add      Add files to an existing archive (ZIP, TAR, LZH)
  info     Show information about an archive, or a PNG/JPEG/TIFF image
  detect   Detect archive format, or a PNG/JPEG/TIFF image
  convert  Convert archive to another format
  man      Generate man pages for all subcommands
  help     Print this message or the help of the given subcommand(s)

Options:
      --color <COLOR>
          Control color output
          
          [default: auto]
          [possible values: auto, always, never]

  -q, --quiet
          Suppress non-error output

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## Known Limitations

1. No encrypted-archive *creation*; extraction of ZipCrypto- and AES-encrypted
   ZIP entries is supported via `extract --password` (wrong password exits 2)
2. No interactive mode

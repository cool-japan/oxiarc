
# oxiarc-cli [Stable]

Command-line interface for OxiArc - The Oxidized Archiver.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-cli.svg)](https://crates.io/crates/oxiarc-cli)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-09-08) | 124 tests passing**


## Features

- List, extract, create, append to, convert, and test archives in ZIP, GZIP, TAR, LZH, XZ, 7z, CAB, LZ4, Zstandard, Bzip2, Brotli, Snappy, and ISO 9660 formats
- Dry-run mode for previewing operations
- Opt-in progress bar on `extract` (`--progress`/`-P`, off by default), include/exclude filters, JSON output
- Global `--quiet`/`-q` and `--color {auto,always,never}` flags
- Shell completions for bash, zsh, fish, PowerShell; `man/` troff man pages for every subcommand
- `--memory-limit <BYTES>` option for `extract` and `list` (accepts human-friendly sizes: `100M`, `1G`, etc.) (new in 0.2.8)
- `-` (stdin) accepted as the archive argument by `list`, `extract`, `test`, `info`, and `detect`
- Symlink-aware directory traversal for `create`/`add` (never follows symlinks, cycle-safe via a visited-canonical-path guard) and real-symlink creation on `extract`
- `convert` refuses to silently overwrite an existing output file
- `detect`/`info` also recognise PNG, JPEG and TIFF images by magic (a CLI-layer fallback over `oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`, never a new archive format) and print their dimensions/colour type/bit depth/compression, plus a chunk/segment/IFD summary for `info`; every other subcommand refuses an image with a clear "not an archive" error

All features are implemented and tested. API is stable.

A Pure Rust CLI tool for working with archive files. Supports listing, extracting, creating, appending to, converting, and testing ZIP, GZIP, TAR, LZH, XZ, 7z, CAB, LZ4, Zstandard, Bzip2, Brotli, Snappy, and ISO 9660 archives. Includes dry-run mode for previewing operations without writing files.

## Installation

```bash
# Build from source
cargo build --release -p oxiarc-cli

# Install globally
cargo install --path oxiarc-cli

# Or run directly
cargo run -p oxiarc-cli -- list archive.zip
```

## Shell Completions

oxiarc provides shell completion scripts for bash, zsh, fish, and PowerShell.

### Installing Completions

**Bash:**
```bash
# Generate completion script
oxiarc completion bash > oxiarc.bash

# Install (choose one location):
sudo cp oxiarc.bash /etc/bash_completion.d/
# or
cp oxiarc.bash ~/.local/share/bash-completion/completions/

# Or add to your .bashrc:
echo 'source /path/to/oxiarc.bash' >> ~/.bashrc
```

**Zsh:**
```bash
# Generate completion script
oxiarc completion zsh > _oxiarc

# Install to a directory in your $fpath
# For example, if /usr/local/share/zsh/site-functions is in your fpath:
sudo cp _oxiarc /usr/local/share/zsh/site-functions/
# or for user-only installation:
mkdir -p ~/.zsh/completions
cp _oxiarc ~/.zsh/completions/
echo 'fpath=(~/.zsh/completions $fpath)' >> ~/.zshrc
echo 'autoload -Uz compinit && compinit' >> ~/.zshrc
```

**Fish:**
```bash
# Generate completion script
oxiarc completion fish > oxiarc.fish

# Install
mkdir -p ~/.config/fish/completions
cp oxiarc.fish ~/.config/fish/completions/
```

**PowerShell:**
```powershell
# Generate completion script
oxiarc completion powershell > _oxiarc.ps1

# Add to your PowerShell profile
# Find your profile location with: $PROFILE
# Then add this line to your profile:
# . /path/to/_oxiarc.ps1
```

## Commands

### list (l)

List contents of an archive:

```bash
# Simple listing
oxiarc list archive.zip

# Verbose with sizes and compression ratios
oxiarc list -v archive.zip

# Limit memory usage during listing
oxiarc list --memory-limit 100M archive.zip
oxiarc list --memory-limit 1G archive.zip
```

**Output (verbose):**
```
Archive: archive.zip (ZIP)

      Size Compressed  Ratio   Method  Name
------------------------------------------------------------
      1234        567  54.1%  Deflate  readme.txt
      5678       1234  78.3%  Deflate  src/main.rs
         0          0      -   Stored  d images/
------------------------------------------------------------
      6912       1801  73.9%          2 files
```

### extract (x)

Extract files from an archive:

```bash
# Extract all to current directory
oxiarc extract archive.zip

# Extract to specific directory
oxiarc extract archive.zip -o output_dir/

# Dry-run mode (preview without writing)
oxiarc extract archive.zip --dry-run

# Limit memory usage during extraction
oxiarc extract --memory-limit 512M archive.zip -o output_dir/
oxiarc extract --memory-limit 2G large.iso -o output_dir/

# Extract specific files
oxiarc extract archive.zip file1.txt file2.txt

# Show a progress bar while extracting (opt-in, off by default)
oxiarc extract archive.zip --progress
```

### info (i)

Show detailed information about an archive:

```bash
oxiarc info archive.zip
```

**Output:**
```
Archive Information
===================
File: archive.zip
Format: ZIP
Size: 12345 bytes
MIME type: application/zip

Contents:
  Files: 5
  Directories: 2
  Total size: 45678 bytes
  Compressed size: 12000 bytes
  Compression ratio: 73.7%
```

`info` also recognises PNG, JPEG and TIFF files (they are not archives, but
`oxiarc-archive` correctly reports them as `Unknown`, so this CLI checks for
them as a fallback — see [Format Support](#format-support)):

```bash
oxiarc info photo.jpg
```

```
Image Information
==================
File: photo.jpg
Format: JPEG image
Size: 45678 bytes

Dimensions: 1920x1080
Colour type: Rgb (3 components)
Bit depth: 8-bit
Compression: Baseline/Huffman

Segments:
  SOI
  APPn: 16 bytes
  DQT: 67 bytes
  DQT: 67 bytes
  SOF0 (baseline): 17 bytes
  DHT: 31 bytes
  DHT: 181 bytes
  SOS: 12 bytes header, then entropy-coded data
```

`Colour type` is the *output* colour space the decoder will hand back
(`Rgb` for a normal 3-component JPEG), not the `YCbCr` the samples are
stored in; the segment list stops at `SOS`, since everything past it is
entropy-coded scan data that only the real decoder can walk.

### detect

Detect the format of a file:

```bash
oxiarc detect unknown_file.bin
```

**Output:**
```
File: unknown_file.bin
Format: GZIP
Extension: .gz
MIME type: application/gzip
Magic bytes: [1F, 8B, 08, 00, ...]
Type: Compression (single file)
```

Against a PNG/JPEG/TIFF file, `detect` prints its dimensions, colour type/bit
depth and compression instead of the extension/MIME/magic-bytes block above:

```
File: image.png
Format: PNG image
Dimensions: 800x600
Colour type: Rgba
Bit depth: 8-bit
Compression: Deflate (zlib)
Type: Image (not an archive)
```

### convert

Convert an archive from one format to another:

```bash
oxiarc convert archive.lzh output.zip
oxiarc convert archive.7z output.zip -l best
```

`convert` refuses to overwrite an existing output file — remove it first if
you want to re-run a conversion.

## Man Pages

Generate mandoc-format man pages for every subcommand:

```bash
oxiarc man ./man
```

Pre-generated pages ship under [`man/`](man/) in this crate.

## Format Support

| Format | list | extract | info | detect | create |
|--------|------|---------|------|--------|--------|
| ZIP | Yes | Yes | Yes | Yes | Yes |
| GZIP | Yes | Yes | Yes | Yes | Yes |
| TAR | Yes | Yes | Yes | Yes | Yes |
| LZH | Yes | Yes | Yes | Yes | Yes |
| XZ | Yes | Yes | Yes | Yes | Yes |
| 7z | Yes | Yes | Yes | Yes | No |
| CAB | Yes | Yes | Yes | Yes | No |
| LZ4 | Yes | Yes | Yes | Yes | Yes |
| Zstandard | Yes | Yes | Yes | Yes | Yes |
| Bzip2 | Yes | Yes | Yes | Yes | Yes |
| Brotli | Yes | Yes | Yes | Yes | Yes |
| Snappy | Yes | Yes | Yes | Yes | Yes |
| ISO 9660 | Yes | Yes | Yes | Yes | No |
| PNG / JPEG / TIFF | No | No | Yes | Yes | No |

`add` (append to an existing archive) is supported for ZIP, TAR, and LZH only.

PNG/JPEG/TIFF are recognised by `detect`/`info` only — an image is neither
an archive nor a bare compression stream, so `list`/`extract`/`test`/
`convert`/`add` refuse one with a clear "this looks like a PNG/JPEG/TIFF
image, not an archive" error rather than a bare "unrecognized format"
message. This recognition is a CLI-layer fallback (via `oxiarc-png`/
`oxiarc-jpeg`/`oxiarc-tiff` directly) and never extends
`oxiarc_archive::ArchiveFormat` itself.

## Examples

```bash
# List a ZIP archive
oxiarc l archive.zip

# Extract GZIP file
oxiarc x data.gz -o ./

# Show info about LZH archive
oxiarc i legacy.lzh

# Detect format
oxiarc detect mystery.bin

# Verbose listing of TAR
oxiarc list -v backup.tar

# List contents of an ISO 9660 image
oxiarc list disc.iso

# Show info about an ISO image
oxiarc info disc.iso

# Detect ISO 9660 image
oxiarc detect disc.iso

# Extract with memory limit
oxiarc extract --memory-limit 1G large_archive.zip -o ./output/
```

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error (invalid archive, I/O error, etc.) |
| 2 | `test` found bad/corrupted entries; a wrong or missing decryption password (`extract`); or `add` targeting a non-appendable archive format |

An unrecognized or corrupt archive format is treated as an error by every
command that has to open the archive to do its job: `extract`, `test`, and
`list` (including `list --json`, which prints nothing on stdout in that case)
all exit `1` with a message on stderr. `detect` is exempt by design — its job
is precisely to report unrecognized formats, so it always exits `0` and
prints `Format: Unknown` instead.

Exit code `2` is used more specifically: `oxiarc test` exits `2` when one or
more entries fail their integrity check; `oxiarc extract` exits `2` if a
password prompt fails (no TTY available) or if decryption fails (wrong
password); `oxiarc add` exits `2` when the target archive's format does not
support in-place appending (only ZIP, TAR, and LZH are appendable).

If interrupted with Ctrl-C mid-operation, `oxiarc` does not currently perform
any special cleanup of partially-written output files — a partially
extracted/created file may be left on disk.

## Error Messages

```
Error: Invalid magic number: expected [50, 4B], found [00, 00]
Error: Unsupported compression method: LZMA
Error: checksum mismatch: expected 0xabcd1234, computed 0x12345678
Error: Corrupted data at offset 1234
Error: unsupported or unrecognized archive format for mystery.bin: Unknown
```

## Usage with Pipes

```bash
# Extract a single-file format to stdout
oxiarc extract file.gz -o - | less

# list/test/info/detect all accept "-" to read the archive from stdin
cat archive.zip | oxiarc list -
cat archive.zip | oxiarc test -
cat archive.zip | oxiarc info -
cat archive.zip | oxiarc detect -
```

## Build Options

```bash
# Release build with optimizations
cargo build --release -p oxiarc-cli

# Debug build
cargo build -p oxiarc-cli

# With all features
cargo build --release -p oxiarc-cli --all-features
```

## Dependencies

- `clap` - Command-line argument parsing
- `oxiarc-archive` - Archive format handling
- `oxiarc-core` - Core types and traits

## License

Apache-2.0

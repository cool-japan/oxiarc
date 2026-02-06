# oxiarc-cli

Command-line interface for OxiArc - The Oxidized Archiver.

## Overview

A Pure Rust CLI tool for working with archive files. Supports listing, extracting, and inspecting ZIP, GZIP, TAR, and LZH archives.

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

# Extract specific files (future)
oxiarc extract archive.zip file1.txt file2.txt
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

## Format Support

| Format | list | extract | create |
|--------|------|---------|--------|
| ZIP | Yes | Yes | No |
| GZIP | Yes | Yes | No |
| TAR | Yes | No | No |
| LZH | Yes | No | No |

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
```

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error (invalid archive, I/O error, etc.) |

## Error Messages

```
Error: Invalid magic number: expected [50, 4B], found [00, 00]
Error: Unsupported compression method: LZMA
Error: CRC mismatch: expected 0xABCD1234, computed 0x12345678
Error: Corrupted data at offset 1234
```

## Usage with Pipes

```bash
# Extract GZIP to stdout (future)
oxiarc extract file.gz -c | less

# List contents from stdin (future)
cat archive.zip | oxiarc list -
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

MIT OR Apache-2.0

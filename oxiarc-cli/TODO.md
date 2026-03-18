# oxiarc-cli - Development Status

## Completed Features

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

### List Improvements
- [x] JSON output (`--json`)
- [x] Sorting options (name, size, date, ratio)
- [x] Filter by pattern
- [x] Show modification times
- [ ] Tree view

### Create Command Options
- [x] Compression level (`-l 0-9`)
- [x] Recursive directory inclusion
- [x] Exclude patterns
- [ ] Store vs. compress threshold
- [ ] Add files to existing archive

### User Experience
- [x] Progress bars (indicatif)
- [ ] Colored output (colored/termcolor)
- [ ] Interactive mode
- [x] Verbose/quiet modes (`-v`, `-q`)
- [x] Dry-run mode (`--dry-run`, `-n`) for create and extract commands

### I/O Options
- [x] Stdin/stdout support (`-`)
- [ ] Password for encrypted archives
- [ ] Multi-volume archives
- [ ] Memory limit option

### Platform
- [x] Shell completion scripts (bash, zsh, fish, powershell) - COMPLETED
  - [x] Hidden `completion` subcommand
  - [x] Bash completion generation
  - [x] Zsh completion generation
  - [x] Fish completion generation
  - [x] PowerShell completion generation
  - [x] Installation instructions in README
- [ ] Man page generation
- [ ] Windows-specific handling

## Test Coverage

- Integration tests needed
- Currently relies on library tests

## Code Statistics

| File | Lines |
|------|-------|
| main.rs | ~1,886 |
| utils.rs | ~129 |
| create.rs | ~200 |
| extract.rs | ~200 |
| (other modules) | ~28 |
| **Total** | **~2,443** |

## Command Reference

```
oxiarc 0.2.5
The Oxidized Archiver - Pure Rust archive utility

USAGE:
    oxiarc <COMMAND>

COMMANDS:
    list     List contents of an archive [aliases: l]
    extract  Extract files from an archive [aliases: x]
    info     Show information about an archive [aliases: i]
    detect   Detect archive format
    test     Test archive integrity [aliases: t]
    create   Create an archive [aliases: c]
    convert  Convert between archive formats
    help     Print help information

OPTIONS:
    -h, --help     Print help information
    -V, --version  Print version information
```

## Known Limitations

1. No encrypted archive support
2. No interactive mode

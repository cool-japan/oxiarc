
using namespace System.Management.Automation
using namespace System.Management.Automation.Language

Register-ArgumentCompleter -Native -CommandName 'oxiarc' -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)

    $commandElements = $commandAst.CommandElements
    $command = @(
        'oxiarc'
        for ($i = 1; $i -lt $commandElements.Count; $i++) {
            $element = $commandElements[$i]
            if ($element -isnot [StringConstantExpressionAst] -or
                $element.StringConstantType -ne [StringConstantType]::BareWord -or
                $element.Value.StartsWith('-') -or
                $element.Value -eq $wordToComplete) {
                break
        }
        $element.Value
    }) -join ';'

    $completions = @(switch ($command) {
        'oxiarc' {
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List contents of an archive')
            [CompletionResult]::new('extract', 'extract', [CompletionResultType]::ParameterValue, 'Extract files from an archive')
            [CompletionResult]::new('test', 'test', [CompletionResultType]::ParameterValue, 'Test archive integrity')
            [CompletionResult]::new('create', 'create', [CompletionResultType]::ParameterValue, 'Create a new archive')
            [CompletionResult]::new('add', 'add', [CompletionResultType]::ParameterValue, 'Add files to an existing archive (ZIP, TAR, LZH)')
            [CompletionResult]::new('info', 'info', [CompletionResultType]::ParameterValue, 'Show information about an archive, or a PNG/JPEG/TIFF image')
            [CompletionResult]::new('detect', 'detect', [CompletionResultType]::ParameterValue, 'Detect archive format, or a PNG/JPEG/TIFF image')
            [CompletionResult]::new('convert', 'convert', [CompletionResultType]::ParameterValue, 'Convert archive to another format')
            [CompletionResult]::new('completion', 'completion', [CompletionResultType]::ParameterValue, 'Generate shell completion scripts')
            [CompletionResult]::new('man', 'man', [CompletionResultType]::ParameterValue, 'Generate man pages for all subcommands')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'oxiarc;list' {
            [CompletionResult]::new('-s', '-s', [CompletionResultType]::ParameterName, 'Sort entries by: name, size, date, or ratio')
            [CompletionResult]::new('--sort', '--sort', [CompletionResultType]::ParameterName, 'Sort entries by: name, size, date, or ratio')
            [CompletionResult]::new('-I', '-I ', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('--include', '--include', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('-X', '-X ', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('--exclude', '--exclude', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('--memory-limit', '--memory-limit', [CompletionResultType]::ParameterName, 'Refuse to extract entries exceeding this memory limit (e.g. 100M, 512K, 1G)')
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-j', '-j', [CompletionResultType]::ParameterName, 'Output as JSON (machine-readable)')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Output as JSON (machine-readable)')
            [CompletionResult]::new('-T', '-T ', [CompletionResultType]::ParameterName, 'Display as directory tree')
            [CompletionResult]::new('--tree', '--tree', [CompletionResultType]::ParameterName, 'Display as directory tree')
            [CompletionResult]::new('-r', '-r', [CompletionResultType]::ParameterName, 'Reverse sort order')
            [CompletionResult]::new('--reverse', '--reverse', [CompletionResultType]::ParameterName, 'Reverse sort order')
            [CompletionResult]::new('--lenient', '--lenient', [CompletionResultType]::ParameterName, 'Continue on corruption when reading the archive (emit warnings to stderr)')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;extract' {
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Output directory (use "-" for stdout when extracting single-file formats)')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Output directory (use "-" for stdout when extracting single-file formats)')
            [CompletionResult]::new('-I', '-I ', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('--include', '--include', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('-X', '-X ', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('--exclude', '--exclude', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Format hint for stdin (gzip, xz, bz2, lz4, zst, br, snappy)')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Format hint for stdin (gzip, xz, bz2, lz4, zst, br, snappy)')
            [CompletionResult]::new('--password', '--password', [CompletionResultType]::ParameterName, 'Password for encrypted entries (prompts interactively if omitted)')
            [CompletionResult]::new('--memory-limit', '--memory-limit', [CompletionResultType]::ParameterName, 'Refuse to extract entries exceeding this memory limit (e.g. 100M, 512K, 1G)')
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-P', '-P ', [CompletionResultType]::ParameterName, 'Show progress bar (opt-in; off by default)')
            [CompletionResult]::new('--progress', '--progress', [CompletionResultType]::ParameterName, 'Show progress bar (opt-in; off by default)')
            [CompletionResult]::new('--overwrite', '--overwrite', [CompletionResultType]::ParameterName, 'Always overwrite existing files (default behavior)')
            [CompletionResult]::new('--skip-existing', '--skip-existing', [CompletionResultType]::ParameterName, 'Skip extraction if file already exists')
            [CompletionResult]::new('--prompt', '--prompt', [CompletionResultType]::ParameterName, 'Prompt user before overwriting each file')
            [CompletionResult]::new('-t', '-t', [CompletionResultType]::ParameterName, 'Preserve file timestamps (modification time)')
            [CompletionResult]::new('--preserve-timestamps', '--preserve-timestamps', [CompletionResultType]::ParameterName, 'Preserve file timestamps (modification time)')
            [CompletionResult]::new('--preserve-permissions', '--preserve-permissions', [CompletionResultType]::ParameterName, 'Preserve file permissions (Unix mode)')
            [CompletionResult]::new('-p', '-p', [CompletionResultType]::ParameterName, 'Preserve all metadata (timestamps and permissions)')
            [CompletionResult]::new('--preserve', '--preserve', [CompletionResultType]::ParameterName, 'Preserve all metadata (timestamps and permissions)')
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Dry run: show what would be extracted without writing files')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Dry run: show what would be extracted without writing files')
            [CompletionResult]::new('--strict-names', '--strict-names', [CompletionResultType]::ParameterName, 'Refuse to extract entries whose basename is a Windows reserved name (CON, NUL, COM1.., LPT1..). Default: append ''_'' to the stem')
            [CompletionResult]::new('--lenient', '--lenient', [CompletionResultType]::ParameterName, 'Continue on corruption (CRC mismatch, bad TAR checksum, etc.) with warnings instead of errors')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;test' {
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;create' {
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Archive format (required for stdout: gzip, xz, bz2, lz4, zst, br, snappy)')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Archive format (required for stdout: gzip, xz, bz2, lz4, zst, br, snappy)')
            [CompletionResult]::new('-l', '-l', [CompletionResultType]::ParameterName, 'Compression level')
            [CompletionResult]::new('--compression', '--compression', [CompletionResultType]::ParameterName, 'Compression level')
            [CompletionResult]::new('--compress-threshold', '--compress-threshold', [CompletionResultType]::ParameterName, 'Files smaller than this (bytes) are stored, not compressed (ZIP only; 0 disables)')
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Dry run: show what would be done without creating the archive')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Dry run: show what would be done without creating the archive')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;add' {
            [CompletionResult]::new('-l', '-l', [CompletionResultType]::ParameterName, 'Compression level (used when the archive format supports it)')
            [CompletionResult]::new('--compression', '--compression', [CompletionResultType]::ParameterName, 'Compression level (used when the archive format supports it)')
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('-n', '-n', [CompletionResultType]::ParameterName, 'Dry run: show planned changes without modifying the archive')
            [CompletionResult]::new('--dry-run', '--dry-run', [CompletionResultType]::ParameterName, 'Dry run: show planned changes without modifying the archive')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;info' {
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;detect' {
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;convert' {
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Output format (zip, tar, gzip, lzh, xz, lz4, br, snappy) - auto-detected from extension if not specified')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Output format (zip, tar, gzip, lzh, xz, lz4, br, snappy) - auto-detected from extension if not specified')
            [CompletionResult]::new('-l', '-l', [CompletionResultType]::ParameterName, 'Compression level for output')
            [CompletionResult]::new('--compression', '--compression', [CompletionResultType]::ParameterName, 'Compression level for output')
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;completion' {
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;man' {
            [CompletionResult]::new('--color', '--color', [CompletionResultType]::ParameterName, 'Control color output')
            [CompletionResult]::new('-q', '-q', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('--quiet', '--quiet', [CompletionResultType]::ParameterName, 'Suppress non-error output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;help' {
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List contents of an archive')
            [CompletionResult]::new('extract', 'extract', [CompletionResultType]::ParameterValue, 'Extract files from an archive')
            [CompletionResult]::new('test', 'test', [CompletionResultType]::ParameterValue, 'Test archive integrity')
            [CompletionResult]::new('create', 'create', [CompletionResultType]::ParameterValue, 'Create a new archive')
            [CompletionResult]::new('add', 'add', [CompletionResultType]::ParameterValue, 'Add files to an existing archive (ZIP, TAR, LZH)')
            [CompletionResult]::new('info', 'info', [CompletionResultType]::ParameterValue, 'Show information about an archive, or a PNG/JPEG/TIFF image')
            [CompletionResult]::new('detect', 'detect', [CompletionResultType]::ParameterValue, 'Detect archive format, or a PNG/JPEG/TIFF image')
            [CompletionResult]::new('convert', 'convert', [CompletionResultType]::ParameterValue, 'Convert archive to another format')
            [CompletionResult]::new('completion', 'completion', [CompletionResultType]::ParameterValue, 'Generate shell completion scripts')
            [CompletionResult]::new('man', 'man', [CompletionResultType]::ParameterValue, 'Generate man pages for all subcommands')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'oxiarc;help;list' {
            break
        }
        'oxiarc;help;extract' {
            break
        }
        'oxiarc;help;test' {
            break
        }
        'oxiarc;help;create' {
            break
        }
        'oxiarc;help;add' {
            break
        }
        'oxiarc;help;info' {
            break
        }
        'oxiarc;help;detect' {
            break
        }
        'oxiarc;help;convert' {
            break
        }
        'oxiarc;help;completion' {
            break
        }
        'oxiarc;help;man' {
            break
        }
        'oxiarc;help;help' {
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}

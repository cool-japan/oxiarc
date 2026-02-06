
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
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('-V', '-V ', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('--version', '--version', [CompletionResultType]::ParameterName, 'Print version')
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List contents of an archive')
            [CompletionResult]::new('extract', 'extract', [CompletionResultType]::ParameterValue, 'Extract files from an archive')
            [CompletionResult]::new('test', 'test', [CompletionResultType]::ParameterValue, 'Test archive integrity')
            [CompletionResult]::new('create', 'create', [CompletionResultType]::ParameterValue, 'Create a new archive')
            [CompletionResult]::new('info', 'info', [CompletionResultType]::ParameterValue, 'Show information about an archive')
            [CompletionResult]::new('detect', 'detect', [CompletionResultType]::ParameterValue, 'Detect archive format')
            [CompletionResult]::new('convert', 'convert', [CompletionResultType]::ParameterValue, 'Convert archive to another format')
            [CompletionResult]::new('completion', 'completion', [CompletionResultType]::ParameterValue, 'Generate shell completion scripts')
            [CompletionResult]::new('help', 'help', [CompletionResultType]::ParameterValue, 'Print this message or the help of the given subcommand(s)')
            break
        }
        'oxiarc;list' {
            [CompletionResult]::new('-I', '-I ', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('--include', '--include', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('-X', '-X ', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('--exclude', '--exclude', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-j', '-j', [CompletionResultType]::ParameterName, 'Output as JSON (machine-readable)')
            [CompletionResult]::new('--json', '--json', [CompletionResultType]::ParameterName, 'Output as JSON (machine-readable)')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;extract' {
            [CompletionResult]::new('-o', '-o', [CompletionResultType]::ParameterName, 'Output directory (use "-" for stdout when extracting single-file formats)')
            [CompletionResult]::new('--output', '--output', [CompletionResultType]::ParameterName, 'Output directory (use "-" for stdout when extracting single-file formats)')
            [CompletionResult]::new('-I', '-I ', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('--include', '--include', [CompletionResultType]::ParameterName, 'Include only files matching pattern (glob syntax: *.txt, src/**/*)')
            [CompletionResult]::new('-X', '-X ', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('--exclude', '--exclude', [CompletionResultType]::ParameterName, 'Exclude files matching pattern (glob syntax)')
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Format hint for stdin (gzip, xz, bz2, lz4, zst)')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Format hint for stdin (gzip, xz, bz2, lz4, zst)')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-P', '-P ', [CompletionResultType]::ParameterName, 'Show progress bar')
            [CompletionResult]::new('--progress', '--progress', [CompletionResultType]::ParameterName, 'Show progress bar')
            [CompletionResult]::new('--overwrite', '--overwrite', [CompletionResultType]::ParameterName, 'Always overwrite existing files (default behavior)')
            [CompletionResult]::new('--skip-existing', '--skip-existing', [CompletionResultType]::ParameterName, 'Skip extraction if file already exists')
            [CompletionResult]::new('--prompt', '--prompt', [CompletionResultType]::ParameterName, 'Prompt user before overwriting each file')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;test' {
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Show verbose output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;create' {
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Archive format (required for stdout: gzip, xz, bz2, lz4, zst)')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Archive format (required for stdout: gzip, xz, bz2, lz4, zst)')
            [CompletionResult]::new('-l', '-l', [CompletionResultType]::ParameterName, 'Compression level')
            [CompletionResult]::new('--compression', '--compression', [CompletionResultType]::ParameterName, 'Compression level')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;info' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;detect' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;convert' {
            [CompletionResult]::new('-f', '-f', [CompletionResultType]::ParameterName, 'Output format (zip, tar, gzip, lzh, xz, lz4) - auto-detected from extension if not specified')
            [CompletionResult]::new('--format', '--format', [CompletionResultType]::ParameterName, 'Output format (zip, tar, gzip, lzh, xz, lz4) - auto-detected from extension if not specified')
            [CompletionResult]::new('-l', '-l', [CompletionResultType]::ParameterName, 'Compression level for output')
            [CompletionResult]::new('--compression', '--compression', [CompletionResultType]::ParameterName, 'Compression level for output')
            [CompletionResult]::new('-v', '-v', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('--verbose', '--verbose', [CompletionResultType]::ParameterName, 'Verbose output')
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help (see more with ''--help'')')
            break
        }
        'oxiarc;completion' {
            [CompletionResult]::new('-h', '-h', [CompletionResultType]::ParameterName, 'Print help')
            [CompletionResult]::new('--help', '--help', [CompletionResultType]::ParameterName, 'Print help')
            break
        }
        'oxiarc;help' {
            [CompletionResult]::new('list', 'list', [CompletionResultType]::ParameterValue, 'List contents of an archive')
            [CompletionResult]::new('extract', 'extract', [CompletionResultType]::ParameterValue, 'Extract files from an archive')
            [CompletionResult]::new('test', 'test', [CompletionResultType]::ParameterValue, 'Test archive integrity')
            [CompletionResult]::new('create', 'create', [CompletionResultType]::ParameterValue, 'Create a new archive')
            [CompletionResult]::new('info', 'info', [CompletionResultType]::ParameterValue, 'Show information about an archive')
            [CompletionResult]::new('detect', 'detect', [CompletionResultType]::ParameterValue, 'Detect archive format')
            [CompletionResult]::new('convert', 'convert', [CompletionResultType]::ParameterValue, 'Convert archive to another format')
            [CompletionResult]::new('completion', 'completion', [CompletionResultType]::ParameterValue, 'Generate shell completion scripts')
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
        'oxiarc;help;help' {
            break
        }
    })

    $completions.Where{ $_.CompletionText -like "$wordToComplete*" } |
        Sort-Object -Property ListItemText
}

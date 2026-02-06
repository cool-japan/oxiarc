# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_oxiarc_global_optspecs
	string join \n h/help V/version
end

function __fish_oxiarc_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_oxiarc_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_oxiarc_using_subcommand
	set -l cmd (__fish_oxiarc_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c oxiarc -n "__fish_oxiarc_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -s V -l version -d 'Print version'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "list" -d 'List contents of an archive'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "extract" -d 'Extract files from an archive'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "test" -d 'Test archive integrity'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "create" -d 'Create a new archive'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "info" -d 'Show information about an archive'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "detect" -d 'Detect archive format'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "convert" -d 'Convert archive to another format'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "completion" -d 'Generate shell completion scripts'
complete -c oxiarc -n "__fish_oxiarc_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand list" -s I -l include -d 'Include only files matching pattern (glob syntax: *.txt, src/**/*)' -r
complete -c oxiarc -n "__fish_oxiarc_using_subcommand list" -s X -l exclude -d 'Exclude files matching pattern (glob syntax)' -r
complete -c oxiarc -n "__fish_oxiarc_using_subcommand list" -s v -l verbose -d 'Show verbose output'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand list" -s j -l json -d 'Output as JSON (machine-readable)'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand list" -s h -l help -d 'Print help'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s o -l output -d 'Output directory (use "-" for stdout when extracting single-file formats)' -r
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s I -l include -d 'Include only files matching pattern (glob syntax: *.txt, src/**/*)' -r
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s X -l exclude -d 'Exclude files matching pattern (glob syntax)' -r
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s f -l format -d 'Format hint for stdin (gzip, xz, bz2, lz4, zst)' -r -f -a "zip\t'ZIP archive'
tar\t'TAR archive'
gzip\t'GZIP compressed file'
lzh\t'LZH archive'
xz\t'XZ compressed file'
lz4\t'LZ4 compressed file'
bz2\t'Bzip2 compressed file'
zst\t'Zstandard compressed file'"
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s v -l verbose -d 'Show verbose output'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s P -l progress -d 'Show progress bar'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -l overwrite -d 'Always overwrite existing files (default behavior)'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -l skip-existing -d 'Skip extraction if file already exists'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -l prompt -d 'Prompt user before overwriting each file'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand extract" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand test" -s v -l verbose -d 'Show verbose output'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand test" -s h -l help -d 'Print help'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand create" -s f -l format -d 'Archive format (required for stdout: gzip, xz, bz2, lz4, zst)' -r -f -a "zip\t'ZIP archive'
tar\t'TAR archive'
gzip\t'GZIP compressed file'
lzh\t'LZH archive'
xz\t'XZ compressed file'
lz4\t'LZ4 compressed file'
bz2\t'Bzip2 compressed file'
zst\t'Zstandard compressed file'"
complete -c oxiarc -n "__fish_oxiarc_using_subcommand create" -s l -l compression -d 'Compression level' -r -f -a "store\t'Store without compression'
fast\t'Fast compression'
normal\t'Normal compression (default)'
best\t'Best compression'"
complete -c oxiarc -n "__fish_oxiarc_using_subcommand create" -s v -l verbose -d 'Verbose output'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand create" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand info" -s h -l help -d 'Print help'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand detect" -s h -l help -d 'Print help'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand convert" -s f -l format -d 'Output format (zip, tar, gzip, lzh, xz, lz4) - auto-detected from extension if not specified' -r -f -a "zip\t'ZIP archive'
tar\t'TAR archive'
gzip\t'GZIP compressed file'
lzh\t'LZH archive'
xz\t'XZ compressed file'
lz4\t'LZ4 compressed file'
bz2\t'Bzip2 compressed file'
zst\t'Zstandard compressed file'"
complete -c oxiarc -n "__fish_oxiarc_using_subcommand convert" -s l -l compression -d 'Compression level for output' -r -f -a "store\t'Store without compression'
fast\t'Fast compression'
normal\t'Normal compression (default)'
best\t'Best compression'"
complete -c oxiarc -n "__fish_oxiarc_using_subcommand convert" -s v -l verbose -d 'Verbose output'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand convert" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand completion" -s h -l help -d 'Print help'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "list" -d 'List contents of an archive'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "extract" -d 'Extract files from an archive'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "test" -d 'Test archive integrity'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "create" -d 'Create a new archive'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "info" -d 'Show information about an archive'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "detect" -d 'Detect archive format'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "convert" -d 'Convert archive to another format'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "completion" -d 'Generate shell completion scripts'
complete -c oxiarc -n "__fish_oxiarc_using_subcommand help; and not __fish_seen_subcommand_from list extract test create info detect convert completion help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'

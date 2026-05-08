//! OxiArc CLI - The Oxidized Archiver
//!
//! A Pure Rust archive utility supporting ZIP, GZIP, TAR, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2, Brotli, and Snappy formats.

mod commands;
mod style;
mod utils;
mod windows;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use commands::{
    CompressionLevel, OutputFormat, SortBy, cmd_add, cmd_convert, cmd_create, cmd_detect,
    cmd_extract, cmd_info, cmd_list, cmd_man, cmd_test,
};
use std::io;
use std::path::PathBuf;
use style::{ColorChoice, Styler};

#[derive(Parser)]
#[command(name = "oxiarc")]
#[command(
    author,
    version,
    about = "The Oxidized Archiver - Pure Rust archive utility"
)]
#[command(long_about = "
OxiArc is a Pure Rust implementation of common archive formats.
Supported formats: ZIP, GZIP, TAR, LZH, XZ, 7z, LZ4, Zstd, Bzip2, Brotli, Snappy

Examples:
  oxiarc list archive.zip
  oxiarc list archive.7z
  oxiarc extract archive.zip
  oxiarc extract archive.7z
  oxiarc extract data.xz
  oxiarc extract data.lz4
  oxiarc extract data.zst
  oxiarc extract data.bz2
  oxiarc extract data.br
  oxiarc extract data.sz
  oxiarc create archive.zip file1.txt file2.txt
  oxiarc create data.xz file.txt
  oxiarc create data.lz4 file.txt
  oxiarc create data.bz2 file.txt
  oxiarc create data.br file.txt
  oxiarc create data.sz file.txt
  oxiarc convert archive.lzh output.zip
  oxiarc convert archive.7z output.zip
  oxiarc test archive.lzh
  oxiarc info archive.7z
")]
pub struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Control color output
    #[arg(long, value_enum, default_value_t = ColorChoice::Auto, global = true)]
    color: ColorChoice,
}

#[derive(Subcommand)]
enum Commands {
    /// List contents of an archive
    #[command(alias = "l")]
    List {
        /// Archive file to list
        archive: PathBuf,

        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Output as JSON (machine-readable)
        #[arg(short, long)]
        json: bool,

        /// Display as directory tree
        #[arg(short = 'T', long)]
        tree: bool,

        /// Sort entries by: name, size, date, or ratio
        #[arg(short, long, value_enum, default_value = "name")]
        sort: SortBy,

        /// Reverse sort order
        #[arg(short = 'r', long)]
        reverse: bool,

        /// Include only files matching pattern (glob syntax: *.txt, src/**/*)
        #[arg(short = 'I', long)]
        include: Vec<String>,

        /// Exclude files matching pattern (glob syntax)
        #[arg(short = 'X', long)]
        exclude: Vec<String>,

        /// Continue on corruption when reading the archive (emit warnings to stderr)
        #[arg(long)]
        lenient: bool,

        /// Refuse to extract entries exceeding this memory limit (e.g. 100M, 512K, 1G)
        #[arg(long, value_parser = crate::utils::parse_byte_size)]
        memory_limit: Option<u64>,
    },

    /// Extract files from an archive
    #[command(alias = "x")]
    #[command(group = clap::ArgGroup::new("overwrite_mode").multiple(false))]
    Extract {
        /// Archive file to extract (use "-" for stdin)
        archive: String,

        /// Output directory (use "-" for stdout when extracting single-file formats)
        #[arg(short, long, default_value = ".")]
        output: String,

        /// Files to extract (all if empty)
        files: Vec<String>,

        /// Include only files matching pattern (glob syntax: *.txt, src/**/*)
        #[arg(short = 'I', long)]
        include: Vec<String>,

        /// Exclude files matching pattern (glob syntax)
        #[arg(short = 'X', long)]
        exclude: Vec<String>,

        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Show progress bar
        #[arg(short = 'P', long, default_value = "true")]
        progress: bool,

        /// Format hint for stdin (gzip, xz, bz2, lz4, zst, br, snappy)
        #[arg(short, long, value_enum)]
        format: Option<OutputFormatArg>,

        /// Always overwrite existing files (default behavior)
        #[arg(long, group = "overwrite_mode")]
        overwrite: bool,

        /// Skip extraction if file already exists
        #[arg(long, group = "overwrite_mode")]
        skip_existing: bool,

        /// Prompt user before overwriting each file
        #[arg(long, group = "overwrite_mode")]
        prompt: bool,

        /// Preserve file timestamps (modification time)
        #[arg(short = 't', long)]
        preserve_timestamps: bool,

        /// Preserve file permissions (Unix mode)
        #[arg(long)]
        preserve_permissions: bool,

        /// Preserve all metadata (timestamps and permissions)
        #[arg(short = 'p', long)]
        preserve: bool,

        /// Dry run: show what would be extracted without writing files
        #[arg(short = 'n', long)]
        dry_run: bool,

        /// Password for encrypted entries (prompts interactively if omitted)
        #[arg(long)]
        password: Option<String>,

        /// Refuse to extract entries whose basename is a Windows reserved name
        /// (CON, NUL, COM1.., LPT1..). Default: append '_' to the stem.
        #[arg(long)]
        strict_names: bool,

        /// Continue on corruption (CRC mismatch, bad TAR checksum, etc.) with warnings instead of errors
        #[arg(long)]
        lenient: bool,

        /// Refuse to extract entries exceeding this memory limit (e.g. 100M, 512K, 1G)
        #[arg(long, value_parser = crate::utils::parse_byte_size)]
        memory_limit: Option<u64>,
    },

    /// Test archive integrity
    #[command(alias = "t")]
    Test {
        /// Archive file to test
        archive: PathBuf,

        /// Show verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Create a new archive
    #[command(alias = "c")]
    Create {
        /// Output archive file (use "-" for stdout)
        archive: String,

        /// Files to add to the archive
        files: Vec<PathBuf>,

        /// Archive format (required for stdout: gzip, xz, bz2, lz4, zst, br, snappy)
        #[arg(short, long, value_enum)]
        format: Option<OutputFormatArg>,

        /// Compression level
        #[arg(short = 'l', long, value_enum, default_value = "normal")]
        compression: CompressionLevelArg,

        /// Files smaller than this (bytes) are stored, not compressed (ZIP only; 0 disables)
        #[arg(long, default_value_t = 0)]
        compress_threshold: u64,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Dry run: show what would be done without creating the archive
        #[arg(short = 'n', long)]
        dry_run: bool,
    },

    /// Add files to an existing archive (ZIP, TAR, LZH)
    Add {
        /// Existing archive file to append to
        archive: PathBuf,

        /// Files to add to the archive
        files: Vec<PathBuf>,

        /// Compression level (used when the archive format supports it)
        #[arg(short = 'l', long, value_enum, default_value = "normal")]
        compression: CompressionLevelArg,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,

        /// Dry run: show planned changes without modifying the archive
        #[arg(short = 'n', long)]
        dry_run: bool,
    },

    /// Show information about an archive
    #[command(alias = "i")]
    Info {
        /// Archive file to inspect
        archive: PathBuf,
    },

    /// Detect archive format
    Detect {
        /// File to detect
        file: PathBuf,
    },

    /// Convert archive to another format
    Convert {
        /// Input archive file
        input: PathBuf,

        /// Output archive file
        output: PathBuf,

        /// Output format (zip, tar, gzip, lzh, xz, lz4, br, snappy) - auto-detected from extension if not specified
        #[arg(short, long, value_enum)]
        format: Option<OutputFormatArg>,

        /// Compression level for output
        #[arg(short = 'l', long, value_enum, default_value = "normal")]
        compression: CompressionLevelArg,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
    },

    /// Generate shell completion scripts
    #[command(hide = true)]
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Generate man pages for all subcommands
    Man {
        /// Directory to write man pages into (default: ./man)
        out_dir: Option<PathBuf>,
    },
}

/// Output archive format (for clap ValueEnum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormatArg {
    /// ZIP archive
    Zip,
    /// TAR archive
    Tar,
    /// GZIP compressed file
    Gzip,
    /// LZH archive
    Lzh,
    /// XZ compressed file
    Xz,
    /// LZ4 compressed file
    Lz4,
    /// Bzip2 compressed file
    Bz2,
    /// Zstandard compressed file
    Zst,
    /// Brotli compressed file
    Br,
    /// Snappy compressed file
    Snappy,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(arg: OutputFormatArg) -> Self {
        match arg {
            OutputFormatArg::Zip => OutputFormat::Zip,
            OutputFormatArg::Tar => OutputFormat::Tar,
            OutputFormatArg::Gzip => OutputFormat::Gzip,
            OutputFormatArg::Lzh => OutputFormat::Lzh,
            OutputFormatArg::Xz => OutputFormat::Xz,
            OutputFormatArg::Lz4 => OutputFormat::Lz4,
            OutputFormatArg::Bz2 => OutputFormat::Bz2,
            OutputFormatArg::Zst => OutputFormat::Zst,
            OutputFormatArg::Br => OutputFormat::Br,
            OutputFormatArg::Snappy => OutputFormat::Snappy,
        }
    }
}

/// Compression level (for clap ValueEnum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum CompressionLevelArg {
    /// Store without compression
    Store,
    /// Fast compression
    Fast,
    /// Normal compression (default)
    #[default]
    Normal,
    /// Best compression
    Best,
}

impl From<CompressionLevelArg> for CompressionLevel {
    fn from(arg: CompressionLevelArg) -> Self {
        match arg {
            CompressionLevelArg::Store => CompressionLevel::Store,
            CompressionLevelArg::Fast => CompressionLevel::Fast,
            CompressionLevelArg::Normal => CompressionLevel::Normal,
            CompressionLevelArg::Best => CompressionLevel::Best,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    let styler = Styler::new(cli.color);

    let result = match cli.command {
        Commands::List {
            archive,
            verbose,
            json,
            tree,
            sort,
            reverse,
            include,
            exclude,
            lenient,
            memory_limit,
        } => {
            let options = commands::list::ListOptions {
                verbose,
                json,
                tree,
                sort_by: sort,
                reverse,
                include: &include,
                exclude: &exclude,
                lenient,
                memory_limit,
            };
            cmd_list(&archive, &options, &styler)
        }
        Commands::Extract {
            archive,
            output,
            files,
            include,
            exclude,
            verbose,
            progress,
            format,
            overwrite,
            skip_existing,
            prompt,
            preserve_timestamps,
            preserve_permissions,
            preserve,
            dry_run,
            password,
            strict_names,
            lenient,
            memory_limit,
        } => cmd_extract(
            commands::extract::ExtractArgs {
                archive: &archive,
                output: &output,
                files: &files,
                include: &include,
                exclude: &exclude,
                verbose,
                progress,
                format_hint: format.map(Into::into),
                overwrite,
                skip_existing,
                prompt,
                preserve_timestamps,
                preserve_permissions,
                preserve,
                dry_run,
                password,
                strict_names,
                lenient,
                memory_limit,
            },
            &styler,
        ),
        Commands::Test { archive, verbose } => cmd_test(&archive, verbose),
        Commands::Create {
            archive,
            files,
            format,
            compression,
            compress_threshold,
            verbose,
            dry_run,
        } => cmd_create(
            &archive,
            &files,
            format.map(Into::into),
            compression.into(),
            compress_threshold,
            verbose,
            dry_run,
        ),
        Commands::Add {
            archive,
            files,
            compression,
            verbose,
            dry_run,
        } => cmd_add(&archive, &files, compression.into(), verbose, dry_run),
        Commands::Info { archive } => cmd_info(&archive, &styler),
        Commands::Detect { file } => cmd_detect(&file, &styler),
        Commands::Convert {
            input,
            output,
            format,
            compression,
            verbose,
        } => cmd_convert(
            &input,
            &output,
            format.map(Into::into),
            compression.into(),
            verbose,
        ),
        Commands::Completion { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "oxiarc", &mut io::stdout());
            return;
        }
        Commands::Man { out_dir } => {
            let cmd = Cli::command();
            if let Err(e) = cmd_man(cmd, out_dir) {
                eprintln!("{}: {e}", styler.error("Error"));
                std::process::exit(1);
            }
            return;
        }
    };

    if let Err(e) = result {
        eprintln!("{}: {}", styler.error("Error"), e);
        std::process::exit(1);
    }
}

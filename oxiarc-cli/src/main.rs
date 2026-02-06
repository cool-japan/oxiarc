//! OxiArc CLI - The Oxidized Archiver
//!
//! A Pure Rust archive utility supporting ZIP, GZIP, TAR, LZH, XZ, 7z, CAB, LZ4, Zstd, and Bzip2 formats.

mod commands;
mod utils;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::{Shell, generate};
use commands::{
    CompressionLevel, OutputFormat, cmd_convert, cmd_create, cmd_detect, cmd_extract, cmd_info,
    cmd_list, cmd_test,
};
use std::io;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "oxiarc")]
#[command(
    author,
    version,
    about = "The Oxidized Archiver - Pure Rust archive utility"
)]
#[command(long_about = "
OxiArc is a Pure Rust implementation of common archive formats.
Supported formats: ZIP, GZIP, TAR, LZH, XZ, 7z, LZ4, Zstd, Bzip2

Examples:
  oxiarc list archive.zip
  oxiarc list archive.7z
  oxiarc extract archive.zip
  oxiarc extract archive.7z
  oxiarc extract data.xz
  oxiarc extract data.lz4
  oxiarc extract data.zst
  oxiarc extract data.bz2
  oxiarc create archive.zip file1.txt file2.txt
  oxiarc create data.xz file.txt
  oxiarc create data.lz4 file.txt
  oxiarc create data.bz2 file.txt
  oxiarc convert archive.lzh output.zip
  oxiarc convert archive.7z output.zip
  oxiarc test archive.lzh
  oxiarc info archive.7z
")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
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

        /// Include only files matching pattern (glob syntax: *.txt, src/**/*)
        #[arg(short = 'I', long)]
        include: Vec<String>,

        /// Exclude files matching pattern (glob syntax)
        #[arg(short = 'X', long)]
        exclude: Vec<String>,
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

        /// Format hint for stdin (gzip, xz, bz2, lz4, zst)
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

        /// Archive format (required for stdout: gzip, xz, bz2, lz4, zst)
        #[arg(short, long, value_enum)]
        format: Option<OutputFormatArg>,

        /// Compression level
        #[arg(short = 'l', long, value_enum, default_value = "normal")]
        compression: CompressionLevelArg,

        /// Verbose output
        #[arg(short, long)]
        verbose: bool,
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

        /// Output format (zip, tar, gzip, lzh, xz, lz4) - auto-detected from extension if not specified
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

    let result = match cli.command {
        Commands::List {
            archive,
            verbose,
            json,
            include,
            exclude,
        } => cmd_list(&archive, verbose, json, &include, &exclude),
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
        } => cmd_extract(
            &archive,
            &output,
            &files,
            &include,
            &exclude,
            verbose,
            progress,
            format.map(Into::into),
            overwrite,
            skip_existing,
            prompt,
            preserve_timestamps,
            preserve_permissions,
            preserve,
        ),
        Commands::Test { archive, verbose } => cmd_test(&archive, verbose),
        Commands::Create {
            archive,
            files,
            format,
            compression,
            verbose,
        } => cmd_create(
            &archive,
            &files,
            format.map(Into::into),
            compression.into(),
            verbose,
        ),
        Commands::Info { archive } => cmd_info(&archive),
        Commands::Detect { file } => cmd_detect(&file),
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
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

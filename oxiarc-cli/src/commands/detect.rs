use crate::style::Styler;
use crate::utils::{input_display_name, open_input};
use oxiarc_archive::ArchiveFormat;

pub fn cmd_detect(
    file: &str,
    quiet: bool,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = open_input(file)?;

    let (format, magic) = ArchiveFormat::detect(&mut reader)?;

    // Detection still runs under --quiet (so an unreadable input errors), but
    // its informational report is suppressed.
    if quiet {
        return Ok(());
    }

    println!("File: {}", styler.path(&input_display_name(file)));
    println!("Format: {}", styler.success(&format.to_string()));
    println!("Extension: .{}", format.extension());
    println!("MIME type: {}", format.mime_type());
    println!("Magic bytes: {:02X?}", &magic[..magic.len().min(16)]);

    if format.is_archive() {
        println!("Type: Archive (multiple files)");
    } else if format.is_compression_only() {
        println!("Type: Compression (single file)");
    }

    Ok(())
}

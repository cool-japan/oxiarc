use crate::image_probe::{self, ImageKind};
use crate::style::Styler;
use crate::utils::{input_display_name, open_input, sniff_image_kind};
use oxiarc_archive::ArchiveFormat;
use std::io::{Read, Seek, SeekFrom};

pub fn cmd_detect(
    file: &str,
    quiet: bool,
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = open_input(file)?;

    // `detect_with_path` adds a filename-extension fallback for the magic-less
    // formats (raw Brotli `.br`, raw Snappy `.sz`); `-` (stdin) has no
    // extension, so it degrades to plain content detection.
    let (format, magic) = ArchiveFormat::detect_with_path(&mut reader, file)?;

    // PNG/JPEG/TIFF are recognised only as a fallback when content
    // detection above found nothing archive- or compression-shaped — see
    // `image_probe`'s module docs for why this never becomes a new
    // `ArchiveFormat` variant.
    //
    // The magic check runs on a 16-byte prefix (`sniff_image_kind`), and the
    // file is only pulled into memory once that prefix already says it is an
    // image: `detect` is routinely pointed at large unknown blobs, and
    // buffering one whole just to answer "not an image" would make the
    // diagnostic more expensive than every other branch of this command.
    let image = if format == ArchiveFormat::Unknown {
        match sniff_image_kind(&mut reader) {
            Some(kind) => {
                reader.seek(SeekFrom::Start(0))?;
                let mut data = Vec::new();
                reader.read_to_end(&mut data)?;
                Some((kind, data))
            }
            None => None,
        }
    } else {
        None
    };

    // Detection still runs under --quiet (so an unreadable input errors), but
    // its informational report is suppressed.
    if quiet {
        return Ok(());
    }

    println!("File: {}", styler.path(&input_display_name(file)));

    if let Some((kind, data)) = &image {
        print_image_detection(*kind, data, styler)?;
        return Ok(());
    }

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

fn print_image_detection(
    kind: ImageKind,
    data: &[u8],
    styler: &Styler,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("Format: {}", styler.success(kind.label()));
    match image_probe::describe(kind, data, false) {
        Ok(summary) => {
            println!(
                "Dimensions: {}x{}",
                summary.dimensions.0, summary.dimensions.1
            );
            println!("Colour type: {}", summary.colour);
            println!("Bit depth: {}", summary.bit_depth);
            println!("Compression: {}", summary.compression);
        }
        Err(e) => {
            println!("{}: {e}", styler.error("Warning"));
        }
    }
    println!("Type: Image (not an archive)");
    Ok(())
}

use oxiarc_archive::zip::{ZipCompressionLevel, ZipReader, ZipWriter};
use std::io::Cursor;

#[test]
fn test_multifile_deflate_bug() -> Result<(), Box<dyn std::error::Error>> {
    // Create compressible test data
    let data1 = "Test data file 1. ".repeat(100);
    let data2 = "File 2 with different content. ".repeat(100);
    let data3 = "Third file data here. ".repeat(100);
    let data4 = "Fourth file for testing. ".repeat(100);
    let data5 = "Fifth and final file. ".repeat(100);

    eprintln!("Creating ZIP with 5 deflate-compressed files...");
    let mut output = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut output);
        writer.set_compression(ZipCompressionLevel::Normal);

        writer.add_file("file1.txt", data1.as_bytes())?;
        eprintln!("  Added file1.txt");

        writer.add_file("file2.txt", data2.as_bytes())?;
        eprintln!("  Added file2.txt");

        writer.add_file("file3.txt", data3.as_bytes())?;
        eprintln!("  Added file3.txt");

        writer.add_file("file4.txt", data4.as_bytes())?;
        eprintln!("  Added file4.txt");

        writer.add_file("file5.txt", data5.as_bytes())?;
        eprintln!("  Added file5.txt");

        writer.finish()?;
    }
    eprintln!("ZIP created, size: {} bytes", output.len());

    eprintln!("\nReading and extracting files...");
    let cursor = Cursor::new(output);
    let mut reader = ZipReader::new(cursor)?;

    let entries: Vec<_> = reader.entries().to_vec();
    eprintln!("Found {} entries", entries.len());

    for (i, entry) in entries.iter().enumerate() {
        eprintln!("\nExtracting entry {}: {}", i + 1, entry.name);
        eprintln!(
            "  Method: {:?}, Size: {}, Compressed: {}",
            entry.method, entry.size, entry.compressed_size
        );

        match reader.extract(entry) {
            Ok(data) => {
                eprintln!("  ✓ Extracted {} bytes", data.len());
            }
            Err(e) => {
                eprintln!("  ✗ ERROR: {}", e);
                return Err(e.into());
            }
        }
    }

    eprintln!("\n✓ All files extracted successfully!");
    Ok(())
}

use oxiarc_archive::zip::{ZipCompressionLevel, ZipWriter};
use oxiarc_deflate::deflate;

#[test]
fn test_simple_deflate() {
    // Test deflate directly
    let data = "Test data. ".repeat(50);
    println!("Input size: {}", data.len());

    let compressed = deflate(data.as_bytes(), 6).unwrap();
    println!("Compressed size: {}", compressed.len());
    println!(
        "First 20 bytes: {:?}",
        &compressed[..compressed.len().min(20)]
    );
    println!(
        "Last 20 bytes: {:?}",
        &compressed[compressed.len().saturating_sub(20)..]
    );

    // Try to inflate it back
    use oxiarc_deflate::inflate;
    match inflate(&compressed) {
        Ok(decompressed) => {
            println!("Decompressed size: {}", decompressed.len());
            assert_eq!(decompressed, data.as_bytes());
            println!("✓ Deflate/inflate roundtrip works!");
        }
        Err(e) => {
            println!("✗ Inflate error: {}", e);
            panic!("Inflate failed");
        }
    }
}

#[test]
fn test_zip_single_file() {
    let data = "Test data. ".repeat(50);
    println!("\nTesting ZIP with single file...");
    println!("Input size: {}", data.len());

    let mut output = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut output);
        writer.set_compression(ZipCompressionLevel::Normal);
        writer.add_file("test.txt", data.as_bytes()).unwrap();
        writer.finish().unwrap();
    }
    println!("ZIP size: {}", output.len());

    // Read it back
    use oxiarc_archive::zip::ZipReader;
    use std::io::Cursor;

    let cursor = Cursor::new(output);
    let mut reader = ZipReader::new(cursor).unwrap();
    let entries: Vec<_> = reader.entries().to_vec();

    println!("Entry: {}", entries[0].name);
    println!("  Method: {:?}", entries[0].method);
    println!(
        "  Size: {}, Compressed: {}",
        entries[0].size, entries[0].compressed_size
    );

    match reader.extract(&entries[0]) {
        Ok(extracted) => {
            println!("  ✓ Extracted {} bytes", extracted.len());
            assert_eq!(extracted, data.as_bytes());
        }
        Err(e) => {
            println!("  ✗ ERROR: {}", e);
            panic!("Extraction failed");
        }
    }
}

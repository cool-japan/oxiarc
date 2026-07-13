//! ZIP round-trip: build an in-memory archive with [`ZipWriter`], then read
//! it back with [`ZipReader`] and extract every entry.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-archive --example zip_roundtrip
//! ```

use oxiarc_archive::zip::{ZipCompressionLevel, ZipReader, ZipWriter};
use std::io::Cursor;

fn main() {
    let readme = "Hello from OxiArc!\n".repeat(64);
    let notes = b"Binary-ish notes payload.".to_vec();

    // ---- Write ----------------------------------------------------------
    let mut archive_bytes = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut archive_bytes);
        writer.set_compression(ZipCompressionLevel::Normal);
        writer
            .add_file("README.txt", readme.as_bytes())
            .expect("add_file README.txt");
        writer
            .add_file("notes/data.bin", &notes)
            .expect("add_file notes/data.bin");
        writer.add_directory("empty_dir/").expect("add_directory");
        writer.finish().expect("finish writer");
    }
    println!("Wrote ZIP archive: {} bytes", archive_bytes.len());

    // ---- Read + extract ---------------------------------------------------
    let mut reader = ZipReader::new(Cursor::new(archive_bytes)).expect("ZipReader::new");
    let entries = reader.entries().to_vec();
    println!("Archive contains {} entries:", entries.len());

    for entry in &entries {
        println!(
            "  - {} ({} bytes compressed -> {} bytes)",
            entry.name, entry.compressed_size, entry.size
        );
        let data = reader.extract(entry).expect("extract entry");
        match entry.name.as_str() {
            "README.txt" => assert_eq!(data, readme.as_bytes()),
            "notes/data.bin" => assert_eq!(data, notes),
            _ => {}
        }
    }

    println!("Round-trip verified: all entries extracted with matching content.");
}

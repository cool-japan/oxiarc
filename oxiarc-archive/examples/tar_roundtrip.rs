//! TAR round-trip: build an in-memory archive with [`TarWriter`], then read
//! it back with [`TarReader`] and extract every entry.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-archive --example tar_roundtrip
//! ```

use oxiarc_archive::tar::{TarReader, TarWriter};
use std::io::Cursor;

fn main() {
    let file_a = b"first file contents\n".to_vec();
    let file_b = "second file, ".repeat(32).into_bytes();

    // ---- Write ----------------------------------------------------------
    let mut archive_bytes = Vec::new();
    {
        let mut writer = TarWriter::new(&mut archive_bytes);
        writer.add_directory("payload/").expect("add_directory");
        writer
            .add_file("payload/a.txt", &file_a)
            .expect("add_file payload/a.txt");
        writer
            .add_file("payload/b.txt", &file_b)
            .expect("add_file payload/b.txt");
        writer
            .add_symlink("payload/link.txt", "a.txt")
            .expect("add_symlink");
        writer.finish().expect("finish writer");
    }
    println!("Wrote TAR archive: {} bytes", archive_bytes.len());

    // ---- Read + extract ---------------------------------------------------
    let mut reader = TarReader::new(Cursor::new(archive_bytes)).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    println!("Archive contains {} entries:", entries.len());

    for entry in &entries {
        println!("  - {} ({} bytes)", entry.name, entry.size);
    }

    let extracted_a = reader
        .extract_by_name("payload/a.txt")
        .expect("extract_by_name a.txt")
        .expect("entry a.txt present");
    assert_eq!(extracted_a, file_a);

    let extracted_b = reader
        .extract_by_name("payload/b.txt")
        .expect("extract_by_name b.txt")
        .expect("entry b.txt present");
    assert_eq!(extracted_b, file_b);

    println!("Round-trip verified: file contents match originals.");
}

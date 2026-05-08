//! Memory-mapped I/O tests for the LZH reader.

use crate::lzh::reader::open_lzh_mmap;
use crate::lzh::writer::{LzhCompressionLevel, LzhWriter};
use std::io::Write;

/// Write a test LZH to a temp file and return the path.
fn create_test_lzh_file(name: &str) -> std::path::PathBuf {
    let temp_dir = std::env::temp_dir();
    let path = temp_dir.join(format!("oxiarc_mmap_lzh_test_{}.lzh", name));

    let mut lzh_bytes = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut lzh_bytes);
        writer.set_compression(LzhCompressionLevel::Store);
        writer
            .add_file("hello.txt", b"Hello, mmap!")
            .expect("add_file hello.txt failed");
        writer
            .add_file(
                "repeat.txt",
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(64).as_slice(),
            )
            .expect("add_file repeat.txt failed");
        writer.finish().expect("finish failed");
    }

    let mut file = std::fs::File::create(&path).expect("create failed");
    file.write_all(&lzh_bytes).expect("write failed");
    file.sync_all().expect("sync failed");
    path
}

#[test]
fn test_mmap_lzh_read() {
    let path = create_test_lzh_file("read");

    let mut reader = open_lzh_mmap(&path).expect("open_lzh_mmap failed");
    let entries = reader.entries();

    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.name == "hello.txt"));
    assert!(entries.iter().any(|e| e.name == "repeat.txt"));

    let hello = entries
        .iter()
        .find(|e| e.name == "hello.txt")
        .cloned()
        .expect("hello.txt entry");
    let data = reader.extract_to_vec(&hello).expect("extract hello.txt");
    assert_eq!(data, b"Hello, mmap!");

    let repeat = entries
        .iter()
        .find(|e| e.name == "repeat.txt")
        .cloned()
        .expect("repeat.txt entry");
    let data = reader.extract_to_vec(&repeat).expect("extract repeat.txt");
    let expected: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ".repeat(64).to_vec();
    assert_eq!(data, expected);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_mmap_lzh_multiple_reads() {
    let path = create_test_lzh_file("multi_read");

    let mut reader = open_lzh_mmap(&path).expect("open_lzh_mmap failed");
    let entries = reader.entries();
    let hello = entries
        .iter()
        .find(|e| e.name == "hello.txt")
        .cloned()
        .expect("hello.txt entry");

    let data1 = reader.extract_to_vec(&hello).expect("first extract");
    let data2 = reader.extract_to_vec(&hello).expect("second extract");
    assert_eq!(data1, data2);
    assert_eq!(data1, b"Hello, mmap!");

    let _ = std::fs::remove_file(&path);
}

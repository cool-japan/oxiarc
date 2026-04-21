//! Integration test for `oxiarc create --compress-threshold`.
//!
//! Two files are dropped into a unique temp directory: a tiny one below the
//! threshold and a large, highly-compressible one above it. After the archive
//! is created, `ZipReader` is used to inspect the per-entry compression
//! method and verify the small file is Stored while the large file uses
//! Deflate.

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use oxiarc_archive::zip::ZipReader;
use oxiarc_core::entry::CompressionMethod;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn workdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxiarc_create_threshold_{}", std::process::id()));
    // Best-effort cleanup before the test, so a prior crash can't wedge us.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

#[test]
fn test_compress_threshold_stores_small_and_deflates_large() {
    let wd = workdir();
    let small = wd.join("tiny.txt");
    let big = wd.join("big.txt");

    std::fs::write(&small, vec![b'A'; 100]).expect("write small");
    // 10_000 bytes of repeating content — highly compressible, will deflate.
    std::fs::write(&big, vec![b'Z'; 10_000]).expect("write big");

    let archive = wd.join("out.zip");
    let status = Command::new(cli_bin())
        .args(["create", "--compress-threshold", "1024", "--color=never"])
        .arg(&archive)
        .arg(&small)
        .arg(&big)
        .status()
        .expect("run oxiarc create");
    assert!(status.success(), "create failed");

    let bytes = std::fs::read(&archive).expect("read archive");
    let reader = ZipReader::new(Cursor::new(&bytes)).expect("ZipReader::new");

    let entries = reader.entries();
    // At least 2 entries (may be 2 exactly if no directories were added).
    assert!(entries.len() >= 2, "expected at least 2 entries");

    let tiny = entries
        .iter()
        .find(|e| e.name.ends_with("tiny.txt"))
        .expect("tiny.txt entry");
    let bigf = entries
        .iter()
        .find(|e| e.name.ends_with("big.txt"))
        .expect("big.txt entry");

    assert_eq!(
        tiny.method,
        CompressionMethod::Stored,
        "tiny.txt should be Stored, got {:?}",
        tiny.method
    );
    assert_eq!(
        bigf.method,
        CompressionMethod::Deflate,
        "big.txt should be Deflate, got {:?}",
        bigf.method
    );

    let _ = std::fs::remove_dir_all(&wd);
}

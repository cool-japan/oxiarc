//! Integration tests for the `oxiarc convert` subcommand.
//!
//! Covers a round trip across zip -> tar -> lzh -> zip (each hop exercised
//! through the actual CLI binary), plus the "refuse to clobber an existing
//! output" guard added to `cmd_convert`.

use std::path::PathBuf;
use std::process::Command;

use oxiarc_archive::{LzhReader, LzhWriter, TarReader, TarWriter, ZipReader, ZipWriter};

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn workdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxiarc_convert_{}_{}", std::process::id(), label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

fn make_zip(path: &PathBuf) {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("hello.txt", b"Hello, OxiArc!")
            .expect("add hello.txt");
        w.add_file("nested/data.bin", b"binary\x00\x01\x02payload")
            .expect("add nested/data.bin");
        w.add_directory("nested/").expect("add directory");
        w.finish().expect("finish zip");
    }
    std::fs::write(path, &buf).expect("write zip fixture");
}

/// Read back every non-directory entry name/content pair from a ZIP archive.
fn read_zip_entries(path: &PathBuf) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).expect("read zip");
    let mut zip = ZipReader::new(std::io::Cursor::new(bytes)).expect("open zip");
    let entries = zip.entries().to_vec();
    entries
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| {
            let data = zip.extract(e).expect("extract zip entry");
            (e.name.clone(), data)
        })
        .collect()
}

fn read_tar_entries(path: &PathBuf) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).expect("read tar");
    let mut tar = TarReader::new(std::io::Cursor::new(bytes)).expect("open tar");
    let entries = tar.entries().to_vec();
    entries
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| {
            let data = tar.extract_to_vec(e).expect("extract tar entry");
            (e.name.clone(), data)
        })
        .collect()
}

fn read_lzh_entries(path: &PathBuf) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(path).expect("read lzh");
    let mut lzh = LzhReader::new(std::io::Cursor::new(bytes)).expect("open lzh");
    let entries = lzh.entries().to_vec();
    entries
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| {
            let data = lzh.extract_to_vec(e).expect("extract lzh entry");
            (e.name.clone(), data)
        })
        .collect()
}

fn sorted(mut v: Vec<(String, Vec<u8>)>) -> Vec<(String, Vec<u8>)> {
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn run_convert(input: &PathBuf, output: &PathBuf) -> std::process::Output {
    Command::new(cli_bin())
        .args(["convert", "--color=never"])
        .arg(input)
        .arg(output)
        .output()
        .expect("run oxiarc convert")
}

#[test]
fn test_convert_round_trip_zip_tar_lzh_zip() {
    let wd = workdir("roundtrip");

    let zip1 = wd.join("a.zip");
    make_zip(&zip1);
    let expected = sorted(read_zip_entries(&zip1));

    // zip -> tar
    let tar1 = wd.join("b.tar");
    let out = run_convert(&zip1, &tar1);
    assert!(
        out.status.success(),
        "zip->tar convert failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let tar_entries = sorted(read_tar_entries(&tar1));
    assert_eq!(tar_entries, expected, "tar round trip lost/changed data");

    // tar -> lzh
    let lzh1 = wd.join("c.lzh");
    let out = run_convert(&tar1, &lzh1);
    assert!(
        out.status.success(),
        "tar->lzh convert failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let lzh_entries = sorted(read_lzh_entries(&lzh1));
    assert_eq!(lzh_entries, expected, "lzh round trip lost/changed data");

    // lzh -> zip (closes the loop back to the original format)
    let zip2 = wd.join("d.zip");
    let out = run_convert(&lzh1, &zip2);
    assert!(
        out.status.success(),
        "lzh->zip convert failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let zip2_entries = sorted(read_zip_entries(&zip2));
    assert_eq!(
        zip2_entries, expected,
        "full round trip zip->tar->lzh->zip lost/changed data"
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_convert_reports_progress_when_verbose() {
    let wd = workdir("verbose");
    let zip1 = wd.join("a.zip");
    make_zip(&zip1);

    let tar1 = wd.join("b.tar");
    let out = Command::new(cli_bin())
        .args(["convert", "--color=never", "--verbose"])
        .arg(&zip1)
        .arg(&tar1)
        .output()
        .expect("run oxiarc convert --verbose");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Converting"),
        "expected a 'Converting' banner, got: {}",
        stdout
    );
    assert!(
        stdout.contains("hello.txt"),
        "verbose output should mention each added entry, got: {}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_convert_refuses_to_overwrite_existing_output() {
    let wd = workdir("overwrite_guard");
    let zip1 = wd.join("a.zip");
    make_zip(&zip1);

    // Pre-existing output with unrelated content: convert must not touch it.
    let tar1 = wd.join("b.tar");
    let sentinel: &[u8] = b"not a real tar file, must survive untouched";
    std::fs::write(&tar1, sentinel).expect("seed pre-existing output");

    let out = run_convert(&zip1, &tar1);
    assert!(
        !out.status.success(),
        "convert must refuse to clobber an existing output file"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected an 'already exists' error, got: {}",
        stderr
    );

    // The pre-existing file must be left completely untouched.
    let after = std::fs::read(&tar1).expect("read output after failed convert");
    assert_eq!(
        after, sentinel,
        "convert must not modify the pre-existing output file at all"
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_convert_lzh_to_zip_with_explicit_format_and_compression() {
    let wd = workdir("explicit_format");
    let lzh1 = wd.join("in.lzh");
    {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = LzhWriter::new(&mut buf);
        w.add_file("readme.md", b"# Title\n\nSome content for compression.\n")
            .expect("add readme.md");
        w.finish().expect("finish lzh");
        drop(w);
        std::fs::write(&lzh1, &buf).expect("write lzh fixture");
    }

    // Output has no recognizable extension, so --format is required.
    let out_path = wd.join("out.converted");
    let out = Command::new(cli_bin())
        .args([
            "convert",
            "--color=never",
            "--format",
            "zip",
            "--compression",
            "best",
        ])
        .arg(&lzh1)
        .arg(&out_path)
        .output()
        .expect("run oxiarc convert --format zip");
    assert!(
        out.status.success(),
        "convert with explicit --format failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let entries = read_zip_entries(&out_path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "readme.md");
    assert_eq!(entries[0].1, b"# Title\n\nSome content for compression.\n");

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_convert_tar_input_with_directories_preserved() {
    let wd = workdir("tar_dirs");
    let tar1 = wd.join("with_dirs.tar");
    {
        let mut buf: Vec<u8> = Vec::new();
        let mut w = TarWriter::new(&mut buf);
        w.add_directory("docs/").expect("add docs dir");
        w.add_file("docs/readme.txt", b"documentation")
            .expect("add file");
        w.finish().expect("finish tar");
        drop(w);
        std::fs::write(&tar1, &buf).expect("write tar fixture");
    }

    let zip_out = wd.join("with_dirs.zip");
    let out = run_convert(&tar1, &zip_out);
    assert!(
        out.status.success(),
        "tar(with dirs)->zip convert failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let bytes = std::fs::read(&zip_out).expect("read converted zip");
    let mut zip = ZipReader::new(std::io::Cursor::new(bytes)).expect("open converted zip");
    let entries = zip.entries().to_vec();
    assert!(
        entries
            .iter()
            .any(|e| e.is_dir() && e.name.trim_end_matches('/') == "docs"),
        "expected a directory entry for docs/, got: {:?}",
        entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    let file_entry = entries
        .iter()
        .find(|e| e.name == "docs/readme.txt")
        .expect("expected docs/readme.txt entry");
    let data = zip.extract(file_entry).expect("extract readme.txt");
    assert_eq!(data, b"documentation");

    let _ = std::fs::remove_dir_all(&wd);
}

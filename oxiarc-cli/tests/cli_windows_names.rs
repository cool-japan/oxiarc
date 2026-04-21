//! Integration tests for Windows-reserved-name sanitization on extract.
//!
//! These tests run on every platform. On non-Windows the renaming still
//! takes place because the sanitizer is always active — the archive could
//! be shipped later to Windows, and forbidding reserved names at creation
//! time lets users discover the problem immediately rather than on a
//! remote machine.

use std::path::PathBuf;
use std::process::Command;

use oxiarc_archive::zip::ZipWriter;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn workdir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("oxiarc_winnames_{}_{}", std::process::id(), label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

fn make_zip_with_reserved(archive: &PathBuf, entry_name: &str) {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = ZipWriter::new(&mut buf);
        w.add_file(entry_name, b"payload").expect("add");
        w.finish().expect("finish");
    }
    std::fs::write(archive, &buf).expect("write");
}

#[test]
fn test_extract_sanitizes_reserved_names() {
    let wd = workdir("sanitize");
    let archive = wd.join("a.zip");
    let out = wd.join("out");
    // Use the lowercase form so the name is safe on Unix fs's too
    // (the reserved-name match is case-insensitive).
    make_zip_with_reserved(&archive, "con.txt");

    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "-o"])
        .arg(&out)
        .arg(&archive)
        .output()
        .expect("run extract");
    assert!(
        output.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Sanitized to con_.txt
    let sanitized = out.join("con_.txt");
    assert!(
        sanitized.exists(),
        "expected {} to exist after sanitization",
        sanitized.display()
    );
    let body = std::fs::read(&sanitized).expect("read sanitized");
    assert_eq!(body, b"payload");

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_extract_strict_names_rejects() {
    let wd = workdir("strict");
    let archive = wd.join("a.zip");
    let out = wd.join("out");
    make_zip_with_reserved(&archive, "con.txt");

    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "--strict-names", "-o"])
        .arg(&out)
        .arg(&archive)
        .output()
        .expect("run extract");
    assert!(
        !output.status.success(),
        "--strict-names must reject reserved basenames"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("reserved"),
        "expected reserved-name error, got: {}",
        stderr
    );

    let _ = std::fs::remove_dir_all(&wd);
}

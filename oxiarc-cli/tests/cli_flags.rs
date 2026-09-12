//! Integration tests for CLI flags that previously had zero or only
//! incidental coverage:
//!   - `test` on an intact archive (the only prior fixture was corrupted)
//!   - `list --sort` (name/size/date/ratio) and `--reverse`
//!   - `extract --memory-limit` rejecting an oversized entry
//!   - `extract --preserve-timestamps` / `--preserve-permissions`
//!   - `list --include`/`--exclude`/`--json`

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, UNIX_EPOCH};

use oxiarc_archive::{TarWriter, ZipCompressionLevel, ZipWriter};

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn workdir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("oxiarc_cliflags_{}_{}", std::process::id(), label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

// ---------------------------------------------------------------------
// `test` positive case
// ---------------------------------------------------------------------

#[test]
fn test_command_exits_zero_on_intact_archive() {
    let wd = workdir("test_positive");
    let archive = wd.join("intact.zip");
    {
        let mut buf = Vec::new();
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("one.txt", b"first file contents")
            .expect("add one.txt");
        w.add_file("two.txt", b"second file contents")
            .expect("add two.txt");
        w.finish().expect("finish zip");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out = Command::new(cli_bin())
        .args(["test", "--color=never", "--verbose"])
        .arg(&archive)
        .output()
        .expect("run oxiarc test");

    assert!(
        out.status.success(),
        "test on an intact archive must exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("OK: one.txt"), "stdout: {stdout}");
    assert!(stdout.contains("OK: two.txt"), "stdout: {stdout}");
    assert!(stdout.contains("Total files: 2"), "stdout: {stdout}");
    assert!(stdout.contains("Failed: 0"), "stdout: {stdout}");
    assert!(stdout.contains("All files OK"), "stdout: {stdout}");

    let _ = std::fs::remove_dir_all(&wd);
}

// ---------------------------------------------------------------------
// `list --sort` / `--reverse`
// ---------------------------------------------------------------------

fn zip_json_names(archive: &PathBuf, extra_args: &[&str]) -> Vec<String> {
    let mut args = vec!["list", "--color=never", "--json"];
    args.extend_from_slice(extra_args);
    let out = Command::new(cli_bin())
        .args(&args)
        .arg(archive)
        .output()
        .expect("run oxiarc list --json");
    assert!(
        out.status.success(),
        "list --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON from list --json: {e}\n{stdout}"));
    parsed["entries"]
        .as_array()
        .expect("entries array")
        .iter()
        .map(|e| e["name"].as_str().expect("name field").to_string())
        .collect()
}

fn make_sort_fixture_tar(path: &PathBuf) {
    // Three files with distinct sizes and distinct, explicit mtimes so that
    // name/size/date sorting all disagree with each other and each is
    // unambiguous.
    let mut buf = Vec::new();
    let mut w = TarWriter::new(&mut buf);
    let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

    // "beta"  : size 30,  mtime base       (oldest)
    // "alpha" : size 10,  mtime base + 200 (middle)
    // "gamma" : size 20,  mtime base + 400 (newest)
    w.add_file_with_metadata("beta", &[b'b'; 30], 0o644, base)
        .expect("add beta");
    w.add_file_with_metadata("alpha", &[b'a'; 10], 0o644, base + Duration::from_secs(200))
        .expect("add alpha");
    w.add_file_with_metadata("gamma", &[b'g'; 20], 0o644, base + Duration::from_secs(400))
        .expect("add gamma");
    w.finish().expect("finish tar");
    drop(w);
    std::fs::write(path, &buf).expect("write tar fixture");
}

#[test]
fn test_list_sort_by_name() {
    let wd = workdir("sort_name");
    let archive = wd.join("sorted.tar");
    make_sort_fixture_tar(&archive);

    let names = zip_json_names(&archive, &["--sort", "name"]);
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);

    let names_rev = zip_json_names(&archive, &["--sort", "name", "--reverse"]);
    assert_eq!(names_rev, vec!["gamma", "beta", "alpha"]);

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_list_sort_by_size() {
    let wd = workdir("sort_size");
    let archive = wd.join("sorted.tar");
    make_sort_fixture_tar(&archive);

    // alpha=10 < gamma=20 < beta=30
    let names = zip_json_names(&archive, &["--sort", "size"]);
    assert_eq!(names, vec!["alpha", "gamma", "beta"]);

    let names_rev = zip_json_names(&archive, &["--sort", "size", "--reverse"]);
    assert_eq!(names_rev, vec!["beta", "gamma", "alpha"]);

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_list_sort_by_date() {
    let wd = workdir("sort_date");
    let archive = wd.join("sorted.tar");
    make_sort_fixture_tar(&archive);

    // beta (oldest) < alpha (middle) < gamma (newest)
    let names = zip_json_names(&archive, &["--sort", "date"]);
    assert_eq!(names, vec!["beta", "alpha", "gamma"]);

    let names_rev = zip_json_names(&archive, &["--sort", "date", "--reverse"]);
    assert_eq!(names_rev, vec!["gamma", "alpha", "beta"]);

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_list_sort_by_ratio() {
    let wd = workdir("sort_ratio");
    let archive = wd.join("ratio.zip");
    {
        let mut buf = Vec::new();
        let mut w = ZipWriter::new(&mut buf);
        w.set_compression(ZipCompressionLevel::Best);
        // "compressible": long run of one byte -> deflates very well (low ratio).
        w.add_file("compressible.bin", &vec![0u8; 4096])
            .expect("add compressible");
        // "incompressible": stored explicitly, so compressed == uncompressed
        // (ratio == 1.0, the worst possible).
        w.add_file_stored("incompressible.bin", &vec![0u8; 4096])
            .expect("add incompressible stored");
        w.finish().expect("finish zip");
        drop(w);
        std::fs::write(&archive, &buf).expect("write zip fixture");
    }

    let names = zip_json_names(&archive, &["--sort", "ratio"]);
    assert_eq!(
        names,
        vec!["compressible.bin", "incompressible.bin"],
        "lower compressed/size ratio should sort first"
    );

    let names_rev = zip_json_names(&archive, &["--sort", "ratio", "--reverse"]);
    assert_eq!(names_rev, vec!["incompressible.bin", "compressible.bin"]);

    let _ = std::fs::remove_dir_all(&wd);
}

// ---------------------------------------------------------------------
// `list --include` / `--exclude` / `--json`
// ---------------------------------------------------------------------

#[test]
fn test_list_include_exclude_json() {
    let wd = workdir("include_exclude");
    let archive = wd.join("filters.zip");
    {
        let mut buf = Vec::new();
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("keep/a.txt", b"a").expect("add a");
        w.add_file("keep/b.txt", b"b").expect("add b");
        w.add_file("skip/c.txt", b"c").expect("add c");
        w.add_file("keep/d.log", b"d").expect("add d");
        w.finish().expect("finish zip");
        drop(w);
        std::fs::write(&archive, &buf).expect("write zip fixture");
    }

    // --include restricts to the glob, --exclude removes a subset of that.
    let mut names = zip_json_names(
        &archive,
        &["--include", "keep/*.txt", "--exclude", "keep/b.txt"],
    );
    names.sort();
    assert_eq!(names, vec!["keep/a.txt"]);

    // Include-only.
    let mut names2 = zip_json_names(&archive, &["--include", "keep/*"]);
    names2.sort();
    assert_eq!(names2, vec!["keep/a.txt", "keep/b.txt", "keep/d.log"]);

    // Exclude-only.
    let mut names3 = zip_json_names(&archive, &["--exclude", "skip/*"]);
    names3.sort();
    assert_eq!(names3, vec!["keep/a.txt", "keep/b.txt", "keep/d.log"]);

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_list_json_metadata_fields_present() {
    let wd = workdir("json_fields");
    let archive = wd.join("meta.zip");
    {
        let mut buf = Vec::new();
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("meta.txt", b"content for metadata check")
            .expect("add");
        w.finish().expect("finish");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out = Command::new(cli_bin())
        .args(["list", "--color=never", "--json", "--verbose"])
        .arg(&archive)
        .output()
        .expect("run list --json --verbose");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(parsed["format"], "ZIP");
    let entries = parsed["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["name"], "meta.txt");
    assert_eq!(entry["size"], 26);
    assert_eq!(entry["is_dir"], false);
    assert!(entry["compressed_size"].is_u64());
    assert!(entry["ratio"].is_number());
    assert!(entry["method"].is_string());

    let _ = std::fs::remove_dir_all(&wd);
}

// ---------------------------------------------------------------------
// `extract --memory-limit`
// ---------------------------------------------------------------------

#[test]
fn test_extract_memory_limit_rejects_oversized_entry() {
    let wd = workdir("memlimit");
    let archive = wd.join("big.zip");
    let big_payload = vec![b'x'; 4096];
    {
        let mut buf = Vec::new();
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("small.txt", b"tiny").expect("add small");
        w.add_file("big.bin", &big_payload).expect("add big");
        w.finish().expect("finish zip");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out_dir = wd.join("out");
    let out = Command::new(cli_bin())
        .args(["extract", "--color=never", "--memory-limit", "1K", "-o"])
        .arg(&out_dir)
        .arg(&archive)
        .output()
        .expect("run oxiarc extract --memory-limit");

    assert!(
        !out.status.success(),
        "extract must fail when an entry exceeds --memory-limit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("exceeds") && stderr.contains("memory-limit"),
        "expected a memory-limit error, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_extract_memory_limit_allows_entries_within_bound() {
    let wd = workdir("memlimit_ok");
    let archive = wd.join("small.zip");
    {
        let mut buf = Vec::new();
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("small.txt", b"tiny content").expect("add small");
        w.finish().expect("finish zip");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out_dir = wd.join("out");
    let out = Command::new(cli_bin())
        .args(["extract", "--color=never", "--memory-limit", "1M", "-o"])
        .arg(&out_dir)
        .arg(&archive)
        .output()
        .expect("run oxiarc extract --memory-limit (generous)");

    assert!(
        out.status.success(),
        "extract with a generous --memory-limit must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out_dir.join("small.txt").exists());

    let _ = std::fs::remove_dir_all(&wd);
}

// ---------------------------------------------------------------------
// `extract --preserve-timestamps` / `--preserve-permissions`
// ---------------------------------------------------------------------

#[test]
fn test_extract_preserve_timestamps() {
    let wd = workdir("preserve_mtime");
    let archive = wd.join("stamped.tar");
    let target_mtime = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    {
        let mut buf = Vec::new();
        let mut w = TarWriter::new(&mut buf);
        w.add_file_with_metadata("stamped.txt", b"content", 0o644, target_mtime)
            .expect("add stamped file");
        w.finish().expect("finish tar");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out_dir = wd.join("out");
    let out = Command::new(cli_bin())
        .args(["extract", "--color=never", "--preserve-timestamps", "-o"])
        .arg(&out_dir)
        .arg(&archive)
        .output()
        .expect("run oxiarc extract --preserve-timestamps");
    assert!(
        out.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let extracted = out_dir.join("stamped.txt");
    assert!(extracted.exists());
    let metadata = std::fs::metadata(&extracted).expect("stat extracted file");
    let actual_mtime = metadata
        .modified()
        .expect("mtime supported on this platform");
    let actual_secs = actual_mtime
        .duration_since(UNIX_EPOCH)
        .expect("mtime after epoch")
        .as_secs();
    assert_eq!(
        actual_secs, 1_600_000_000,
        "extracted file's mtime should match the archived timestamp exactly"
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[cfg(unix)]
#[test]
fn test_extract_preserve_permissions() {
    // Unix-only, so both imports live here rather than at module scope where
    // they would be dead code (a `-D warnings` error) on other targets.
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;

    let wd = workdir("preserve_mode");
    let archive = wd.join("modes.tar");
    let now = SystemTime::now();
    {
        let mut buf = Vec::new();
        let mut w = TarWriter::new(&mut buf);
        w.add_file_with_metadata("secret.sh", b"#!/bin/sh\necho hi\n", 0o750, now)
            .expect("add secret.sh with explicit mode");
        w.finish().expect("finish tar");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out_dir = wd.join("out");
    let out = Command::new(cli_bin())
        .args(["extract", "--color=never", "--preserve-permissions", "-o"])
        .arg(&out_dir)
        .arg(&archive)
        .output()
        .expect("run oxiarc extract --preserve-permissions");
    assert!(
        out.status.success(),
        "extract failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let extracted = out_dir.join("secret.sh");
    assert!(extracted.exists());
    let metadata = std::fs::metadata(&extracted).expect("stat extracted file");
    let mode = metadata.permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o750,
        "extracted file should carry the archived Unix mode bits, got {:o}",
        mode
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_extract_without_preserve_flags_ignores_archived_mtime() {
    // Baseline/negative check: without --preserve-timestamps the extracted
    // file's mtime must reflect "now" (creation time), not the archived
    // value, confirming the flag actually gates the behavior.
    let wd = workdir("no_preserve");
    let archive = wd.join("stamped.tar");
    let ancient = UNIX_EPOCH + Duration::from_secs(1_000_000);
    {
        let mut buf = Vec::new();
        let mut w = TarWriter::new(&mut buf);
        w.add_file_with_metadata("old.txt", b"content", 0o644, ancient)
            .expect("add old.txt");
        w.finish().expect("finish tar");
        drop(w);
        std::fs::write(&archive, &buf).expect("write fixture");
    }

    let out_dir = wd.join("out");
    let out = Command::new(cli_bin())
        .args(["extract", "--color=never", "-o"])
        .arg(&out_dir)
        .arg(&archive)
        .output()
        .expect("run oxiarc extract (no preserve flags)");
    assert!(out.status.success());

    let extracted = out_dir.join("old.txt");
    let metadata = std::fs::metadata(&extracted).expect("stat extracted file");
    let actual_mtime = metadata.modified().expect("mtime supported");
    assert!(
        actual_mtime > ancient + Duration::from_secs(60),
        "without --preserve-timestamps the file's mtime should be roughly 'now', not the archived 1970s timestamp"
    );

    let _ = std::fs::remove_dir_all(&wd);
}

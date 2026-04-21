//! Integration tests for `--color {auto,always,never}` behaviour across commands.
//!
//! These tests invoke the compiled `oxiarc` binary as a subprocess and verify
//! that ANSI escape codes are emitted only when colorization is requested (or
//! detected) and are suppressed otherwise. Each test builds its own ZIP fixture
//! on a unique path so that Cargo's default parallel test runner does not race
//! on a shared filename inside `std::env::temp_dir()`.

use std::path::PathBuf;
use std::process::Command;

/// Path to the compiled `oxiarc` binary. Set by Cargo when running integration tests.
fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

/// Build a small ZIP fixture in a per-test temp file; return its path.
///
/// Uniqueness is derived from `(pid, label)` so each test owns its fixture even
/// when Cargo runs them concurrently inside the same test binary.
fn make_fixture_zip(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "oxiarc_cli_color_fixture_{}_{label}.zip",
        std::process::id()
    ));

    use oxiarc_archive::zip::ZipWriter;
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut buf);
        writer
            .add_file("hello.txt", b"Hello, CLI color test!")
            .expect("add hello");
        writer
            .add_file("nested/file.txt", b"Nested file content")
            .expect("add nested");
        writer.finish().expect("finish");
    }
    std::fs::write(&path, &buf).expect("write fixture");
    path
}

const ANSI_ESC: u8 = 0x1b;

fn contains_ansi(bytes: &[u8]) -> bool {
    bytes.windows(2).any(|w| w[0] == ANSI_ESC && w[1] == b'[')
}

#[test]
fn test_list_color_never() {
    let fixture = make_fixture_zip("list_never");
    let output = Command::new(cli_bin())
        .args(["list", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc list");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !contains_ansi(&output.stdout),
        "unexpected ANSI escape in --color=never output"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_list_color_always() {
    let fixture = make_fixture_zip("list_always");
    let output = Command::new(cli_bin())
        .args(["list", "--color=always"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc list");
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // `list` defaults to plain rows; not every line triggers a styled helper,
    // so we only guarantee command success here. The `never` tests are the
    // strong negative side of the coverage, and `test_color_always_overrides_no_color`
    // below proves that `--color=always` does emit ANSI when a styled helper
    // fires (errors).
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_info_color_never() {
    let fixture = make_fixture_zip("info_never");
    let output = Command::new(cli_bin())
        .args(["info", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc info");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!contains_ansi(&output.stdout));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_detect_color_never() {
    let fixture = make_fixture_zip("detect_never");
    let output = Command::new(cli_bin())
        .args(["detect", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc detect");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!contains_ansi(&output.stdout));
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_extract_color_never() {
    let fixture = make_fixture_zip("extract_never");
    let tmpdir =
        std::env::temp_dir().join(format!("oxiarc_cli_extract_never_{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir).expect("create tmpdir");
    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "-o"])
        .arg(&tmpdir)
        .arg(&fixture)
        .output()
        .expect("run oxiarc extract");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!contains_ansi(&output.stdout));
    let _ = std::fs::remove_dir_all(&tmpdir);
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_no_color_env_var_auto() {
    // With stdout wired to a pipe (via `Command::output`), `supports_color`
    // already reports no-TTY, so `Auto` falls through to plain output
    // regardless of `NO_COLOR`. We still run this to exercise the
    // code path and to document the intent: `NO_COLOR=1` must never
    // *produce* ANSI when combined with `Auto`.
    let fixture = make_fixture_zip("nocolor_auto");
    let output = Command::new(cli_bin())
        .args(["list"])
        .env("NO_COLOR", "1")
        .arg(&fixture)
        .output()
        .expect("run oxiarc list");
    assert!(output.status.success());
    assert!(
        !contains_ansi(&output.stdout),
        "NO_COLOR=1 with --color=auto must not emit ANSI"
    );
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_color_always_overrides_no_color() {
    // `--color=always` deliberately bypasses the `NO_COLOR` env check
    // (see `oxiarc-cli/src/style.rs`). We exercise this by forcing an
    // error (missing archive) so the `Error:` label runs through the
    // styled helper and an ANSI escape becomes observable on stderr.
    let missing = std::env::temp_dir().join(format!(
        "oxiarc_cli_missing_{}_{}.zip",
        std::process::id(),
        "always_overrides"
    ));
    // Make sure the path really does not exist.
    let _ = std::fs::remove_file(&missing);

    let output = Command::new(cli_bin())
        .args(["list", "--color=always"])
        .env("NO_COLOR", "1")
        .arg(&missing)
        .output()
        .expect("run oxiarc list");

    assert!(
        !output.status.success(),
        "expected failure for missing archive"
    );
    assert!(
        contains_ansi(&output.stderr),
        "--color=always must emit ANSI on error output even when NO_COLOR=1 is set; stderr was: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

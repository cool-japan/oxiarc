//! Integration test for `oxiarc man <output_dir>`.
//!
//! Exercises the full `oxiarc` binary end-to-end (complementing the unit test
//! inside `src/commands/man.rs`, which drives the same helper directly).

use std::path::PathBuf;
use std::process::Command;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

#[test]
fn test_man_page_generation() {
    let tmpdir = std::env::temp_dir().join(format!("oxiarc_man_cli_test_{}", std::process::id()));
    std::fs::create_dir_all(&tmpdir).expect("create tmpdir");

    let output = Command::new(cli_bin())
        .arg("man")
        .arg(&tmpdir)
        .output()
        .expect("run oxiarc man");

    assert!(
        output.status.success(),
        "man failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Assert at least 2 files written (top-level binary + >= 1 subcommand page).
    let entries: Vec<_> = std::fs::read_dir(&tmpdir)
        .expect("read tmpdir")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.len() >= 2,
        "expected >= 2 man pages, got {}",
        entries.len()
    );

    // At least one file must carry a roff title header (.TH) and at least one
    // must carry the NAME+SYNOPSIS sections typical of a clap_mangen render.
    let mut found_th = false;
    let mut found_synopsis = false;
    for entry in &entries {
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if content.lines().any(|l| l.starts_with(".TH")) {
                found_th = true;
            }
            if content.contains(".SH NAME") && content.contains(".SH SYNOPSIS") {
                found_synopsis = true;
            }
        }
    }
    assert!(found_th, "no file started with .TH");
    assert!(
        found_synopsis,
        "no file contained both .SH NAME and .SH SYNOPSIS"
    );

    let _ = std::fs::remove_dir_all(&tmpdir);
}

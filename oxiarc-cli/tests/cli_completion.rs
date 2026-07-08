//! Integration tests for `oxiarc completion <shell>`.
//!
//! The subcommand is hidden from `--help` but still fully invokable; these
//! tests assert every supported `Shell` variant produces a non-trivial,
//! plausible completion script on stdout and exits 0.

use std::path::PathBuf;
use std::process::Command;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn run_completion(shell: &str) -> std::process::Output {
    Command::new(cli_bin())
        .args(["completion", shell])
        .output()
        .expect("run oxiarc completion")
}

/// One (shell name, a substring expected to appear in a well-formed script
/// for that shell) pair. The substring check is deliberately loose — the
/// exact template is clap_complete's concern, not ours — but it guards
/// against the command silently emitting nothing or a generic error.
const SHELLS: &[(&str, &str)] = &[
    ("bash", "complete"),
    ("zsh", "#compdef"),
    ("fish", "complete"),
    ("elvish", "oxiarc"),
    ("powershell", "oxiarc"),
];

#[test]
fn test_completion_generates_nonempty_script_for_every_shell() {
    for (shell, marker) in SHELLS {
        let out = run_completion(shell);
        assert!(
            out.status.success(),
            "completion for {shell} did not exit 0: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.trim().is_empty(),
            "completion script for {shell} was empty"
        );
        assert!(
            stdout.contains("oxiarc"),
            "completion script for {shell} should reference the binary name, got: {stdout}"
        );
        assert!(
            stdout.contains(marker),
            "completion script for {shell} missing expected marker {marker:?}, got: {stdout}"
        );
    }
}

#[test]
fn test_completion_bash_mentions_all_top_level_subcommands() {
    let out = run_completion("bash");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Non-exhaustive, but enough to catch a completion script that silently
    // regenerated against a stale/half-registered `Commands` enum.
    for subcommand in [
        "list", "extract", "test", "create", "add", "info", "detect", "convert", "man",
    ] {
        assert!(
            stdout.contains(subcommand),
            "bash completion script should mention subcommand '{subcommand}'"
        );
    }
}

#[test]
fn test_completion_rejects_unknown_shell() {
    let out = run_completion("not-a-real-shell");
    assert!(
        !out.status.success(),
        "an unrecognized shell value must be rejected"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.trim().is_empty(),
        "clap should report a usage error for an invalid shell value"
    );
}

#[test]
fn test_completion_is_deterministic() {
    // Same invocation twice should yield byte-identical output — a sanity
    // check that generation has no accidental nondeterminism (e.g. embedding
    // a timestamp or PID).
    let first = run_completion("bash");
    let second = run_completion("bash");
    assert!(first.status.success());
    assert!(second.status.success());
    assert_eq!(first.stdout, second.stdout);
}

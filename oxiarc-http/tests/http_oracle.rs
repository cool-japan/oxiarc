//! Differential tests against reference encoders, behind the `http-oracle`
//! feature.
//!
//! Every other test in this crate decodes what `oxiarc`'s own encoders
//! produced, which cannot catch a shared misunderstanding of the format.
//! These decode what CPython's `zlib`/`gzip` modules and the `brotli` /
//! `zstd` CLIs produced instead.
//!
//! Each test **self-skips with a printed note** when its tool is missing,
//! matching the established `zlib-oracle` / `brotli-oracle` / `zstd-oracle`
//! pattern, so a hermetic machine still gets a green, reproducible run.
//!
//! ```text
//! cargo test -p oxiarc-http --features http-oracle,brotli,zstd --test http_oracle
//! ```

#![cfg(feature = "http-oracle")]
// Each helper below belongs to one coding's oracle and is therefore reachable
// only under that coding's feature; the file is a test harness, not library
// code, so a per-combination `#[cfg]` on every helper would be noise.
#![allow(dead_code)]

mod common;

use std::path::PathBuf;
use std::process::Command;

use oxiarc_http::{DecodeLimits, Decoder, decode_body_from_header};

/// A unique scratch path under the process/thread-specific temp directory.
fn temp_path(label: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "oxiarc_http_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    path
}

fn python3_available() -> bool {
    Command::new("python3")
        .args(["-c", "import zlib, gzip"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a python snippet that reads `argv[1]` and writes `argv[2]`.
fn python_transform(label: &str, script: &str, input: &[u8]) -> Option<Vec<u8>> {
    let in_path = temp_path(&format!("{label}_in"));
    let out_path = temp_path(&format!("{label}_out"));
    std::fs::write(&in_path, input).ok()?;
    let status = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&in_path)
        .arg(&out_path)
        .status()
        .ok()?;
    let result = if status.success() {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
    result
}

/// Pipe `input` through `tool args...` (stdin to stdout) via temp files, so
/// no pipe-deadlock handling is needed.
fn cli_transform(label: &str, tool: &str, args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let in_path = temp_path(&format!("{label}_in"));
    let out_path = temp_path(&format!("{label}_out"));
    std::fs::write(&in_path, input).ok()?;
    let stdin = std::fs::File::open(&in_path).ok()?;
    let stdout = std::fs::File::create(&out_path).ok()?;
    let status = Command::new(tool)
        .args(args)
        .stdin(stdin)
        .stdout(stdout)
        .status()
        .ok()?;
    let result = if status.success() {
        std::fs::read(&out_path).ok()
    } else {
        None
    };
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
    result
}

/// Decode `wire` both in one shot and byte at a time, asserting they agree.
fn decode_both_ways(header: &str, wire: &[u8]) -> Vec<u8> {
    let one_shot = decode_body_from_header(header, wire, &DecodeLimits::default())
        .unwrap_or_else(|e| panic!("{header}: one-shot decode failed: {e}"));

    let mut decoder =
        Decoder::from_header(header, &DecodeLimits::default()).expect("build decoder");
    let mut streamed = Vec::new();
    for byte in wire {
        decoder
            .feed_into(&[*byte], &mut streamed)
            .unwrap_or_else(|e| panic!("{header}: byte-at-a-time decode failed: {e}"));
    }
    decoder
        .finish_into(&mut streamed)
        .unwrap_or_else(|e| panic!("{header}: finish failed: {e}"));

    assert_eq!(
        one_shot, streamed,
        "{header}: one-shot and byte-at-a-time disagree"
    );
    one_shot
}

const PY_GZIP: &str = r#"
import sys, gzip
with open(sys.argv[1], 'rb') as f:
    data = f.read()
with open(sys.argv[2], 'wb') as f:
    f.write(gzip.compress(data, 9))
"#;

const PY_GZIP_MULTI: &str = r#"
import sys, gzip
with open(sys.argv[1], 'rb') as f:
    data = f.read()
half = len(data) // 2
with open(sys.argv[2], 'wb') as f:
    f.write(gzip.compress(data[:half], 9) + gzip.compress(data[half:], 6))
"#;

const PY_GZIP_NAMED: &str = r#"
import sys, gzip, io
with open(sys.argv[1], 'rb') as f:
    data = f.read()
buf = io.BytesIO()
with gzip.GzipFile(fileobj=buf, mode='wb', filename='body.txt', mtime=1700000000) as g:
    g.write(data)
with open(sys.argv[2], 'wb') as f:
    f.write(buf.getvalue())
"#;

const PY_ZLIB: &str = r#"
import sys, zlib
with open(sys.argv[1], 'rb') as f:
    data = f.read()
with open(sys.argv[2], 'wb') as f:
    f.write(zlib.compress(data, 9))
"#;

const PY_RAW_DEFLATE: &str = r#"
import sys, zlib
with open(sys.argv[1], 'rb') as f:
    data = f.read()
c = zlib.compressobj(9, zlib.DEFLATED, -15)
with open(sys.argv[2], 'wb') as f:
    f.write(c.compress(data) + c.flush())
"#;

const PY_ZLIB_FDICT: &str = r#"
import sys, zlib
with open(sys.argv[1], 'rb') as f:
    data = f.read()
c = zlib.compressobj(9, zlib.DEFLATED, 15, 9, zlib.Z_DEFAULT_STRATEGY, zdict=b'the quick brown fox')
with open(sys.argv[2], 'wb') as f:
    f.write(c.compress(data) + c.flush())
"#;

fn payload() -> Vec<u8> {
    common::json(300_000)
}

#[cfg(feature = "gzip")]
#[test]
fn cpython_gzip_streams_decode() {
    if !python3_available() {
        eprintln!("skipping: python3 with `zlib`/`gzip` is unavailable");
        return;
    }
    let plain = payload();

    let wire = python_transform("gzip", PY_GZIP, &plain).expect("python gzip.compress");
    assert_eq!(decode_both_ways("gzip", &wire), plain, "gzip.compress");

    let wire = python_transform("gzip_multi", PY_GZIP_MULTI, &plain).expect("two gzip members");
    assert_eq!(decode_both_ways("gzip", &wire), plain, "multi-member");

    let wire = python_transform("gzip_named", PY_GZIP_NAMED, &plain).expect("gzip with FNAME");
    assert_eq!(decode_both_ways("gzip", &wire), plain, "FNAME + MTIME");
}

#[cfg(all(feature = "deflate", feature = "gzip"))]
#[test]
fn cpython_deflate_streams_decode_in_all_three_spellings() {
    if !python3_available() {
        eprintln!("skipping: python3 with `zlib`/`gzip` is unavailable");
        return;
    }
    let plain = payload();

    // RFC 9110 §8.4.1.2's conformant spelling.
    let wire = python_transform("zlib", PY_ZLIB, &plain).expect("zlib.compress");
    assert_eq!(decode_both_ways("deflate", &wire), plain, "zlib-wrapped");

    // The non-conformant one §8.4.1.2 sanctions accepting.
    let wire = python_transform("raw", PY_RAW_DEFLATE, &plain).expect("raw deflate");
    assert_eq!(decode_both_ways("deflate", &wire), plain, "raw");

    // A gzip stream mislabelled `deflate`, which browsers accept.
    let wire = python_transform("gzip_as_deflate", PY_GZIP, &plain).expect("gzip");
    assert_eq!(
        decode_both_ways("deflate", &wire),
        plain,
        "gzip mislabelled deflate"
    );
}

#[cfg(feature = "deflate")]
#[test]
fn a_cpython_zlib_stream_with_a_preset_dictionary_is_refused_cleanly() {
    if !python3_available() {
        eprintln!("skipping: python3 with `zlib`/`gzip` is unavailable");
        return;
    }
    let plain = common::text(4_000);
    let wire = python_transform("fdict", PY_ZLIB_FDICT, &plain).expect("zlib with FDICT");
    assert_ne!(wire[1] & 0x20, 0, "the fixture must actually set FDICT");
    decode_body_from_header("deflate", &wire, &DecodeLimits::default())
        .expect_err("FDICT cannot be satisfied over HTTP");
}

#[cfg(feature = "brotli")]
#[test]
fn reference_brotli_streams_decode() {
    if !tool_available("brotli") {
        eprintln!("skipping: the `brotli` CLI is unavailable");
        return;
    }
    let plain = payload();
    for quality in ["-q", "1"].chunks(2).chain([["-q", "11"].as_slice()]) {
        let mut args = vec!["-c"];
        args.extend_from_slice(quality);
        let wire = match cli_transform("brotli", "brotli", &args, &plain) {
            Some(wire) => wire,
            None => {
                eprintln!("skipping: `brotli {args:?}` failed");
                return;
            }
        };
        assert_eq!(decode_both_ways("br", &wire), plain, "brotli {args:?}");
    }
}

#[cfg(feature = "zstd")]
#[test]
fn reference_zstd_streams_decode() {
    if !tool_available("zstd") {
        eprintln!("skipping: the `zstd` CLI is unavailable");
        return;
    }
    let plain = payload();
    for args in [
        vec!["-c", "-1", "-q"],
        vec!["-c", "-19", "-q"],
        vec!["-c", "-3", "-q", "--no-check"],
    ] {
        let wire = match cli_transform("zstd", "zstd", &args, &plain) {
            Some(wire) => wire,
            None => {
                eprintln!("skipping: `zstd {args:?}` failed");
                return;
            }
        };
        assert_eq!(decode_both_ways("zstd", &wire), plain, "zstd {args:?}");
    }
}

#[cfg(feature = "zstd")]
#[test]
fn concatenated_reference_zstd_frames_decode() {
    if !tool_available("zstd") {
        eprintln!("skipping: the `zstd` CLI is unavailable");
        return;
    }
    let first = common::text(50_000);
    let second = common::json(50_000);
    let Some(a) = cli_transform("zstd_a", "zstd", &["-c", "-3", "-q"], &first) else {
        eprintln!("skipping: `zstd` failed");
        return;
    };
    let Some(b) = cli_transform("zstd_b", "zstd", &["-c", "-3", "-q"], &second) else {
        eprintln!("skipping: `zstd` failed");
        return;
    };
    let mut wire = a;
    wire.extend_from_slice(&b);
    let mut expected = first;
    expected.extend_from_slice(&second);
    assert_eq!(decode_both_ways("zstd", &wire), expected);
}

#[cfg(all(feature = "brotli", feature = "gzip"))]
#[test]
fn a_reference_built_chain_decodes_in_reverse_order() {
    if !python3_available() || !tool_available("brotli") {
        eprintln!("skipping: python3 or the `brotli` CLI is unavailable");
        return;
    }
    let plain = common::text(120_000);
    let Some(inner) = cli_transform("chain_br", "brotli", &["-c", "-q", "5"], &plain) else {
        eprintln!("skipping: `brotli` failed");
        return;
    };
    let wire = python_transform("chain_gzip", PY_GZIP, &inner).expect("python gzip");
    assert_eq!(decode_both_ways("br, gzip", &wire), plain);
}

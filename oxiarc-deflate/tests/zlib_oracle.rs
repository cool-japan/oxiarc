//! Live differential tests against reference codecs: CPython's `zlib`/`gzip`
//! modules and the system `gzip` CLI.
//!
//! Both directions are exercised for the streaming/trait wrappers fixed in
//! DEFLATE-01..05:
//!   1. reference-compress → oxiarc-decode must be byte-identical, and
//!   2. oxiarc-encode → reference-decode must be accepted and byte-identical.
//!
//! Gated behind the `zlib-oracle` feature (which implies `parallel`). Each
//! test self-skips — not fails — when `python3` / `gzip` are absent, so the
//! suite is safe to enable unconditionally in CI.

#![cfg(feature = "zlib-oracle")]

use oxiarc_core::traits::{CompressStatus, Compressor, Decompressor, FlushMode};
use oxiarc_deflate::{
    Deflater, GzipStreamDecoder, InflateReader, InflateStatus, InflateWrapper, Inflater,
    WrappedInflate, ZlibStreamDecoder, compress_gzip_parallel, gzip_decompress,
};
use std::io::Read;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Reference-tool plumbing
// ---------------------------------------------------------------------------

fn python3_available() -> bool {
    Command::new("python3")
        .args(["-c", "import zlib, gzip"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn gzip_cli_available() -> bool {
    Command::new("gzip")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Unique temp-file path (tests run in parallel within one process).
fn temp_path(tag: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "oxiarc_zlib_oracle_{}_{}_{}",
        std::process::id(),
        n,
        tag
    ))
}

/// Run a python3 snippet with input/output file paths in argv, panicking
/// with stderr on failure.
fn run_python(script: &str, args: &[&std::path::Path]) -> Vec<u8> {
    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(script);
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.output().expect("failed to spawn python3");
    assert!(
        out.status.success(),
        "python3 oracle failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Diverse payloads crossing internal 32 KiB window / 64 KiB / chunk
/// boundaries: empty, tiny, all-zeros, repetitive text, pseudo-random.
fn oracle_payloads() -> Vec<(&'static str, Vec<u8>)> {
    let mut payloads: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one_byte", vec![0x42]),
        ("all_zeros_64k", vec![0u8; 65_536]),
        (
            "repetitive_text_128k",
            b"The quick brown fox jumps over the lazy dog. "
                .iter()
                .cycle()
                .take(131_072 + 7)
                .copied()
                .collect(),
        ),
    ];
    // Pseudo-random (xorshift) — incompressible; 1 MiB + odd remainder.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut random = Vec::with_capacity(1_048_576 + 13);
    while random.len() < 1_048_576 + 13 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        random.extend_from_slice(&state.to_le_bytes());
    }
    random.truncate(1_048_576 + 13);
    payloads.push(("random_1m", random));
    payloads
}

// ---------------------------------------------------------------------------
// Direction 1: reference-compress → oxiarc-decode
// ---------------------------------------------------------------------------

/// Python-produced multi-member gzip (one member per 256 KiB slice, mixed
/// levels) must decode byte-identically via the serial `GzipDecoder`.
#[test]
fn oracle_python_multi_member_gzip_decodes() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping (self-skip, not a failure)");
        return;
    }

    let script = r#"
import sys, gzip
data = open(sys.argv[1], 'rb').read()
out = bytearray()
CHUNK = 256 * 1024
levels = [9, 1, 6]
if not data:
    out += gzip.compress(b'', mtime=0)
else:
    for i in range(0, len(data), CHUNK):
        out += gzip.compress(data[i:i+CHUNK], compresslevel=levels[(i // CHUNK) % 3], mtime=0)
sys.stdout.buffer.write(bytes(out))
"#;

    let mut decoded_members = 0usize;
    for (label, payload) in oracle_payloads() {
        let in_path = temp_path(&format!("py_gzip_in_{label}"));
        std::fs::write(&in_path, &payload).expect("write temp input");
        let compressed = run_python(script, &[&in_path]);
        let _ = std::fs::remove_file(&in_path);

        let decompressed = gzip_decompress(&compressed).unwrap_or_else(|e| {
            panic!("GzipDecoder failed on python multi-member gzip ({label}): {e}")
        });
        assert_eq!(
            decompressed, payload,
            "python multi-member gzip decode mismatch ({label})"
        );
        decoded_members += compressed
            .windows(3)
            .filter(|w| w[0] == 0x1f && w[1] == 0x8b && w[2] == 0x08)
            .count()
            .max(1);
    }
    eprintln!(
        "[zlib-oracle] python multi-member gzip: 5/5 payloads, ~{decoded_members} members decoded byte-identical"
    );
}

/// gzip-CLI-produced members concatenated with `cat` semantics must decode
/// via the serial decoder (the CLI writes FNAME headers — also exercised).
#[test]
fn oracle_gzip_cli_concatenated_members_decode() {
    if !gzip_cli_available() {
        eprintln!("[zlib-oracle] gzip CLI not found; skipping (self-skip, not a failure)");
        return;
    }

    let part_a: Vec<u8> = b"alpha ".iter().cycle().take(100_000).copied().collect();
    let part_b: Vec<u8> = (0u8..=255).cycle().take(70_000).collect();

    let mut concatenated = Vec::new();
    for (tag, part, level) in [("a", &part_a, "-9"), ("b", &part_b, "-1")] {
        let path = temp_path(&format!("cli_member_{tag}"));
        std::fs::write(&path, part).expect("write temp member");
        let out = Command::new("gzip")
            .arg("-c")
            .arg(level)
            .arg(&path)
            .output()
            .expect("failed to spawn gzip");
        assert!(out.status.success(), "gzip CLI compress failed");
        concatenated.extend_from_slice(&out.stdout);
        let _ = std::fs::remove_file(&path);
    }

    let decompressed = gzip_decompress(&concatenated)
        .expect("GzipDecoder failed on gzip-CLI concatenated members");
    let mut expected = part_a;
    expected.extend_from_slice(&part_b);
    assert_eq!(
        decompressed, expected,
        "gzip CLI multi-member decode mismatch"
    );
    eprintln!("[zlib-oracle] gzip CLI 2-member concatenation decoded byte-identical");
}

/// Python-produced concatenated zlib streams must decode via
/// `ZlibStreamDecoder` in O(n) — including with a member count large enough
/// that the old quadratic fallback would have been visibly slow.
#[test]
fn oracle_python_concatenated_zlib_decodes() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping (self-skip, not a failure)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
out = bytearray()
CHUNK = 64 * 1024
if not data:
    out += zlib.compress(b'')
else:
    for i in range(0, len(data), CHUNK):
        out += zlib.compress(data[i:i+CHUNK], (i // CHUNK) % 9 + 1)
sys.stdout.buffer.write(bytes(out))
"#;

    for (label, payload) in oracle_payloads() {
        let in_path = temp_path(&format!("py_zlib_in_{label}"));
        std::fs::write(&in_path, &payload).expect("write temp input");
        let compressed = run_python(script, &[&in_path]);
        let _ = std::fs::remove_file(&in_path);

        let mut decoder = ZlibStreamDecoder::new(&compressed[..]);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).unwrap_or_else(|e| {
            panic!("ZlibStreamDecoder failed on python concatenated zlib ({label}): {e}")
        });
        assert_eq!(
            output, payload,
            "python concatenated zlib decode mismatch ({label})"
        );
    }
    eprintln!("[zlib-oracle] python concatenated zlib: 5/5 payloads decoded byte-identical");
}

/// Python-produced raw DEFLATE must round-trip through the streaming
/// `Decompressor` trait with a small bounded output buffer (DEFLATE-01).
#[test]
fn oracle_python_raw_deflate_streaming_decode() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping (self-skip, not a failure)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
c = zlib.compressobj(6, zlib.DEFLATED, -15)
out = c.compress(data) + c.flush()
sys.stdout.buffer.write(out)
"#;

    for (label, payload) in oracle_payloads() {
        let in_path = temp_path(&format!("py_deflate_in_{label}"));
        std::fs::write(&in_path, &payload).expect("write temp input");
        let compressed = run_python(script, &[&in_path]);
        let _ = std::fs::remove_file(&in_path);

        let mut inflater = Inflater::new();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        let mut pos = 0usize;
        loop {
            let (consumed, produced, status) = inflater
                .decompress(&compressed[pos..], &mut buf)
                .unwrap_or_else(|e| panic!("streaming decode of python deflate ({label}): {e}"));
            pos += consumed;
            out.extend_from_slice(&buf[..produced]);
            if status == oxiarc_core::traits::DecompressStatus::Done {
                break;
            }
        }
        assert_eq!(
            out, payload,
            "bounded-buffer decode of python raw deflate mismatch ({label})"
        );
    }
    eprintln!("[zlib-oracle] python raw deflate → bounded-buffer Inflater: 5/5 byte-identical");
}

// ---------------------------------------------------------------------------
// Direction 2: oxiarc-encode → reference-decode
// ---------------------------------------------------------------------------

/// The streaming `Compressor` trait driven with chunked input and a small
/// bounded output buffer (DEFLATE-02 + DEFLATE-03) must emit one continuous
/// raw DEFLATE stream that CPython's `zlib.decompressobj(-15)` accepts.
#[test]
fn oracle_streaming_compressor_output_accepted_by_python() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping (self-skip, not a failure)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
d = zlib.decompressobj(-15)
out = d.decompress(data) + d.flush()
if d.unconsumed_tail:
    raise SystemExit('unconsumed tail: %d bytes' % len(d.unconsumed_tail))
sys.stdout.buffer.write(out)
"#;

    for level in [0u8, 1, 6, 9] {
        for (label, payload) in oracle_payloads() {
            let mut deflater = Deflater::new(level);
            let mut compressed = Vec::new();
            let mut buf = [0u8; 512];
            let mut pos = 0usize;
            const INPUT_CHUNK: usize = 40_000;
            loop {
                let end = (pos + INPUT_CHUNK).min(payload.len());
                let flush = if pos >= payload.len() {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };
                let (consumed, produced, status) = deflater
                    .compress(&payload[pos..end], &mut buf, flush)
                    .unwrap_or_else(|e| panic!("compress failed ({label}, level {level}): {e}"));
                pos += consumed;
                compressed.extend_from_slice(&buf[..produced]);
                if status == CompressStatus::Done {
                    break;
                }
            }

            let comp_path = temp_path(&format!("ox_deflate_{label}_{level}"));
            std::fs::write(&comp_path, &compressed).expect("write temp compressed");
            let decoded = run_python(script, &[&comp_path]);
            let _ = std::fs::remove_file(&comp_path);

            assert_eq!(
                decoded, payload,
                "python rejected/corrupted oxiarc streaming deflate ({label}, level {level})"
            );
        }
    }
    eprintln!(
        "[zlib-oracle] oxiarc bounded-buffer Compressor → python zlib(-15): 20/20 byte-identical"
    );
}

/// `compress_gzip_parallel` multi-member output must be accepted by both
/// CPython's `gzip.decompress` and the system `gzip -d`.
#[test]
fn oracle_parallel_gzip_output_accepted_by_references() {
    let payload: Vec<u8> = b"parallel gzip interop payload / "
        .iter()
        .cycle()
        .take(1_600_000) // > 3 members at 512 KiB per chunk
        .copied()
        .collect();
    let compressed = compress_gzip_parallel(&payload, 6).expect("parallel compress failed");

    // Own decoder must agree regardless of reference availability.
    assert_eq!(
        gzip_decompress(&compressed).expect("own GzipDecoder failed"),
        payload,
        "own decoder mismatch on parallel output"
    );

    let mut checked = Vec::new();

    if python3_available() {
        let script = r#"
import sys, gzip
sys.stdout.buffer.write(gzip.decompress(open(sys.argv[1], 'rb').read()))
"#;
        let path = temp_path("parallel_gzip_py");
        std::fs::write(&path, &compressed).expect("write temp compressed");
        let decoded = run_python(script, &[&path]);
        let _ = std::fs::remove_file(&path);
        assert_eq!(decoded, payload, "python gzip rejected parallel output");
        checked.push("python gzip.decompress");
    }

    if gzip_cli_available() {
        let path = temp_path("parallel_gzip_cli.gz");
        std::fs::write(&path, &compressed).expect("write temp compressed");
        let out = Command::new("gzip")
            .arg("-dc")
            .arg(&path)
            .output()
            .expect("failed to spawn gzip");
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "gzip CLI rejected parallel output: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, payload, "gzip CLI output mismatch");
        checked.push("gzip -dc");
    }

    if checked.is_empty() {
        eprintln!("[zlib-oracle] no reference tools found; own-decoder check only (self-skip)");
    } else {
        eprintln!(
            "[zlib-oracle] compress_gzip_parallel (4 members) accepted byte-identical by: {}",
            checked.join(", ")
        );
    }
}

// ---------------------------------------------------------------------------
// Resumable push decoder against CPython's zlib/gzip
// ---------------------------------------------------------------------------

/// Drive the push decoder over `input`, one byte of compressed data per
/// call into a small output buffer — the worst-case schedule for a
/// resumable state machine.
fn push_decode_wrapped(
    wrapper: InflateWrapper,
    input: &[u8],
    multi_member: bool,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut decoder = WrappedInflate::new(wrapper).multi_member(multi_member);
    let mut out = Vec::new();
    let mut scratch = [0u8; 13];
    let mut fed = 0usize;
    loop {
        let end = (fed + 1).min(input.len());
        let flush = if end >= input.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = decoder.inflate(&input[fed..end], &mut scratch, flush)?;
        fed += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
    }
}

/// D4: the same payload compressed by CPython with `wbits` = -15 (raw), 15
/// (zlib) and 31 (gzip) must decode through the matching wrapper **and**
/// through `Auto`, fed one byte at a time.
#[test]
fn oracle_python_wbits_matrix_through_the_push_decoder() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping wbits matrix (self-skip)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
wbits = int(sys.argv[3])
c = zlib.compressobj(6, zlib.DEFLATED, wbits)
out = c.compress(data) + c.flush()
open(sys.argv[2], 'wb').write(out)
"#;

    for (name, payload) in oracle_payloads() {
        for (wbits, wrapper) in [
            (-15i32, InflateWrapper::Raw),
            (15, InflateWrapper::Zlib),
            (31, InflateWrapper::Gzip),
        ] {
            let src = temp_path(&format!("wbits_src_{name}"));
            let dst = temp_path(&format!("wbits_dst_{name}"));
            std::fs::write(&src, &payload).expect("write payload");
            run_python(
                script,
                &[&src, &dst, std::path::Path::new(&wbits.to_string())],
            );
            let compressed = std::fs::read(&dst).expect("read compressed");
            let _ = std::fs::remove_file(&src);
            let _ = std::fs::remove_file(&dst);

            let named = push_decode_wrapped(wrapper, &compressed, false)
                .unwrap_or_else(|e| panic!("{name} wbits {wbits} named: {e}"));
            assert_eq!(named, payload, "{name} wbits {wbits}: named wrapper");

            let sniffed = push_decode_wrapped(InflateWrapper::Auto, &compressed, false)
                .unwrap_or_else(|e| panic!("{name} wbits {wbits} auto: {e}"));
            assert_eq!(sniffed, payload, "{name} wbits {wbits}: Auto");
        }
    }
    eprintln!("[zlib-oracle] CPython wbits -15/15/31 decoded byte-at-a-time through the push API");
}

/// D3: CPython's `Z_SYNC_FLUSH` output — a stream of sync-flushed units,
/// exactly what RFC 4978 peers emit — decoded one byte at a time.
#[test]
fn oracle_python_sync_flush_stream_through_the_push_decoder() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping sync-flush stream (self-skip)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
c = zlib.compressobj(6, zlib.DEFLATED, -15)
out = b''
step = max(1, len(data) // 5)
for i in range(0, len(data), step) if data else []:
    out += c.compress(data[i:i+step]) + c.flush(zlib.Z_SYNC_FLUSH)
out += c.flush()
open(sys.argv[2], 'wb').write(out)
"#;

    for (name, payload) in oracle_payloads() {
        let src = temp_path(&format!("sync_src_{name}"));
        let dst = temp_path(&format!("sync_dst_{name}"));
        std::fs::write(&src, &payload).expect("write payload");
        run_python(script, &[&src, &dst]);
        let compressed = std::fs::read(&dst).expect("read compressed");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);

        let decoded = push_decode_wrapped(InflateWrapper::Raw, &compressed, false)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(decoded, payload, "{name}: sync-flushed stream");
    }
    eprintln!(
        "[zlib-oracle] CPython Z_SYNC_FLUSH streams decoded byte-at-a-time through the push API"
    );
}

/// D5: CPython's `decompressobj().decompress(data, max_length=n)` is the
/// direct analogue of a bounded-output push call. Feeding both the same
/// stream with the same output budget must produce the same bytes.
#[test]
fn oracle_python_bounded_output_matches() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping bounded-output check (self-skip)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
n = int(sys.argv[3])
d = zlib.decompressobj()
out = d.decompress(data, n)
open(sys.argv[2], 'wb').write(out)
"#;

    for (name, payload) in oracle_payloads() {
        let compressed = oxiarc_deflate::zlib_compress(&payload, 6).expect("zlib_compress");
        for budget in [1usize, 97, 4096, 65_536] {
            let src = temp_path(&format!("bounded_src_{name}_{budget}"));
            let dst = temp_path(&format!("bounded_dst_{name}_{budget}"));
            std::fs::write(&src, &compressed).expect("write compressed");
            run_python(
                script,
                &[&src, &dst, std::path::Path::new(&budget.to_string())],
            );
            let expected = std::fs::read(&dst).expect("read reference output");
            let _ = std::fs::remove_file(&src);
            let _ = std::fs::remove_file(&dst);

            // One push call with an output buffer of exactly `budget`.
            let mut decoder = WrappedInflate::new(InflateWrapper::Zlib);
            let mut scratch = vec![0u8; budget];
            let progress = decoder
                .inflate(&compressed, &mut scratch, FlushMode::None)
                .unwrap_or_else(|e| panic!("{name} budget {budget}: {e}"));
            assert_eq!(
                &scratch[..progress.produced],
                &expected[..],
                "{name} budget {budget}: bounded output diverged from CPython"
            );
        }
    }
    eprintln!("[zlib-oracle] bounded-output push calls match CPython decompressobj(max_length=n)");
}

// ---------------------------------------------------------------------------
// The `Read` adapters against the reference codecs
// ---------------------------------------------------------------------------

/// Read `reader` to the end `chunk` bytes at a time.
fn read_all_chunked<R: Read>(mut reader: R, chunk: usize) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
    }
}

/// A source that hands out at most `step` bytes per `read`, so the adapter
/// is driven with the short reads a socket really produces.
struct DribbleSource<'a> {
    data: &'a [u8],
    pos: usize,
    step: usize,
}

impl Read for DribbleSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.step).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// `InflateReader` must decode CPython's output for all three `wbits`
/// framings, and via `Auto`, through both a dribbling source and one-byte
/// caller reads — the two schedules that break a naive adapter.
#[test]
fn oracle_python_wbits_matrix_through_the_read_adapter() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping reader wbits matrix (self-skip)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
wbits = int(sys.argv[3])
c = zlib.compressobj(6, zlib.DEFLATED, wbits)
open(sys.argv[2], 'wb').write(c.compress(data) + c.flush())
"#;

    for (name, payload) in oracle_payloads() {
        for (wbits, framing) in [
            (-15i32, InflateWrapper::Raw),
            (15, InflateWrapper::Zlib),
            (31, InflateWrapper::Gzip),
        ] {
            let src = temp_path(&format!("rdwbits_src_{name}_{wbits}"));
            let dst = temp_path(&format!("rdwbits_dst_{name}_{wbits}"));
            std::fs::write(&src, &payload).expect("write payload");
            run_python(
                script,
                &[&src, &dst, std::path::Path::new(&wbits.to_string())],
            );
            let compressed = std::fs::read(&dst).expect("read compressed");
            let _ = std::fs::remove_file(&src);
            let _ = std::fs::remove_file(&dst);

            for chunk in [1usize, 65_536] {
                let source = DribbleSource {
                    data: &compressed,
                    pos: 0,
                    step: 5,
                };
                let decoded = read_all_chunked(InflateReader::new(source, framing), chunk)
                    .unwrap_or_else(|e| panic!("{name} wbits {wbits} chunk {chunk}: {e}"));
                assert_eq!(decoded, payload, "{name} wbits {wbits} chunk {chunk}");
            }

            // `Auto` must reach the same answer without being told.
            let decoded = read_all_chunked(InflateReader::auto(&compressed[..]), 3)
                .unwrap_or_else(|e| panic!("{name} wbits {wbits} auto: {e}"));
            assert_eq!(decoded, payload, "{name} wbits {wbits}: Auto reader");
        }
    }
    eprintln!("[zlib-oracle] CPython wbits -15/15/31 decoded through InflateReader");
}

/// The system `gzip` CLI's multi-member output (`gzip -c a b`) must decode
/// through `GzipStreamDecoder`, `InflateReader` and the async adapter to
/// the same bytes.
#[test]
fn oracle_gzip_cli_multi_member_through_the_read_adapters() {
    if !gzip_cli_available() {
        eprintln!("[zlib-oracle] gzip CLI not found; skipping multi-member readers (self-skip)");
        return;
    }

    let first = b"first file from the gzip CLI\n".repeat(400);
    let second = b"second file, quite different content\n".repeat(250);
    let path_a = temp_path("cli_a");
    let path_b = temp_path("cli_b");
    std::fs::write(&path_a, &first).expect("write a");
    std::fs::write(&path_b, &second).expect("write b");

    let out = Command::new("gzip")
        .args(["-c"])
        .arg(&path_a)
        .arg(&path_b)
        .output()
        .expect("spawn gzip");
    assert!(out.status.success(), "gzip -c failed");
    let compressed = out.stdout;
    let _ = std::fs::remove_file(&path_a);
    let _ = std::fs::remove_file(&path_b);

    let mut expected = first;
    expected.extend_from_slice(&second);

    for chunk in [1usize, 3, 65_536] {
        let decoded = read_all_chunked(GzipStreamDecoder::new(&compressed[..]), chunk)
            .unwrap_or_else(|e| panic!("GzipStreamDecoder chunk {chunk}: {e}"));
        assert_eq!(decoded, expected, "GzipStreamDecoder chunk {chunk}");

        let decoded = read_all_chunked(InflateReader::gzip(&compressed[..]), chunk)
            .unwrap_or_else(|e| panic!("InflateReader chunk {chunk}: {e}"));
        assert_eq!(decoded, expected, "InflateReader chunk {chunk}");
    }

    let mut reader = InflateReader::gzip(&compressed[..]);
    let mut sink = Vec::new();
    reader.read_to_end(&mut sink).expect("read_to_end");
    assert_eq!(reader.members_decoded(), 2, "gzip -c a b emits two members");
    assert_eq!(gzip_decompress(&compressed).expect("one-shot"), expected);

    eprintln!("[zlib-oracle] `gzip -c a b` decoded through both Read adapters");
}

/// CPython's concatenated zlib members, decoded through the `Read` adapters
/// with the trailing-fragment shapes the short-tail rule governs.
#[test]
fn oracle_python_concatenated_zlib_through_the_read_adapter() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping reader concatenation (self-skip)");
        return;
    }

    let script = r#"
import sys, zlib
data = open(sys.argv[1], 'rb').read()
half = len(data) // 2
open(sys.argv[2], 'wb').write(
    zlib.compress(data[:half], 9) + zlib.compress(data[half:], 1))
"#;

    for (name, payload) in oracle_payloads() {
        let src = temp_path(&format!("rdcat_src_{name}"));
        let dst = temp_path(&format!("rdcat_dst_{name}"));
        std::fs::write(&src, &payload).expect("write payload");
        run_python(script, &[&src, &dst]);
        let base = std::fs::read(&dst).expect("read compressed");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);

        for tail in [
            &b""[..],
            &[0x78][..],
            &[0x78, 0x9c][..],
            &[0x00, 0x00, 0x00][..],
        ] {
            let mut compressed = base.clone();
            compressed.extend_from_slice(tail);
            for chunk in [1usize, 65_536] {
                let decoded = read_all_chunked(ZlibStreamDecoder::new(&compressed[..]), chunk)
                    .unwrap_or_else(|e| panic!("{name} tail {tail:02x?} chunk {chunk}: {e}"));
                assert_eq!(decoded, payload, "{name} tail {tail:02x?} chunk {chunk}");
            }
        }
    }
    eprintln!("[zlib-oracle] CPython concatenated zlib decoded through ZlibStreamDecoder");
}

/// D9: `Adler32` against CPython's `zlib.adler32`, directly rather than
/// through a zlib trailer.
///
/// The checksum was rewritten in 0.4.2 into a lane-accumulator form that a
/// compiler can vectorise (2.4 GB/s -> ~23 GB/s). The identity behind it is
/// exact, so "close enough" is not a possible outcome: every length here is
/// compared byte for byte with the reference, including the lengths that
/// straddle the 32-byte group and the `NMAX` block, and the incremental feed
/// that a streaming decoder actually uses.
#[test]
fn oracle_python_adler32_matches_at_every_boundary() {
    if !python3_available() {
        eprintln!("[zlib-oracle] python3 not found; skipping adler32 check (self-skip)");
        return;
    }

    let lengths: Vec<usize> = vec![
        0, 1, 2, 31, 32, 33, 63, 64, 65, 255, 5519, 5520, 5535, 5536, 5551, 5552, 5553, 11_071,
        11_072, 11_104, 70_000,
    ];

    for len in lengths {
        for (tag, data) in [
            ("max", vec![0xFFu8; len]),
            ("zero", vec![0u8; len]),
            (
                "mixed",
                (0..len)
                    .map(|i| (i.wrapping_mul(2_654_435_761) >> 7) as u8)
                    .collect::<Vec<u8>>(),
            ),
        ] {
            let path = temp_path(&format!("adler_{tag}_{len}"));
            std::fs::write(&path, &data).expect("write payload");
            let out = run_python(
                "import sys, zlib\n\
                 data = open(sys.argv[1], 'rb').read()\n\
                 sys.stdout.write(str(zlib.adler32(data) & 0xFFFFFFFF))\n",
                &[&path],
            );
            let _ = std::fs::remove_file(&path);
            let expected: u32 = String::from_utf8_lossy(&out)
                .trim()
                .parse()
                .expect("python adler32");

            assert_eq!(
                oxiarc_deflate::Adler32::checksum(&data),
                expected,
                "[zlib-oracle] one-shot adler32 differs at len={len} ({tag})"
            );

            // The streaming feed a decoder performs must agree too.
            let mut incremental = oxiarc_deflate::Adler32::new();
            let mut pos = 0usize;
            let mut step = 1usize;
            while pos < data.len() {
                let take = step.min(data.len() - pos);
                incremental.update(&data[pos..pos + take]);
                pos += take;
                step = (step * 3 + 1).min(4096);
            }
            assert_eq!(
                incremental.finish(),
                expected,
                "[zlib-oracle] incremental adler32 differs at len={len} ({tag})"
            );
        }
    }

    eprintln!("[zlib-oracle] CPython zlib.adler32 agrees at every group/NMAX boundary");
}

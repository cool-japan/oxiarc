//! Unit and oracle tests for the `.xz` non-last filters.
//!
//! Split out of `filters/mod.rs` for file size; `use super::*` gives this
//! module the same access to the converters' private helpers it had when it
//! was an inline `mod tests`.

use super::*;

/// Deterministic pseudo-instruction stream with many filter-matching
/// patterns, so the converters actually fire.
fn pattern_bytes(len: usize, marker: &[u8]) -> Vec<u8> {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(len + 16);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(marker);
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

fn all_filters() -> Vec<XzFilter> {
    let mut filters = vec![
        XzFilter::Delta { distance: 1 },
        XzFilter::Delta { distance: 4 },
        XzFilter::Delta { distance: 256 },
    ];
    for id in [
        FILTER_BCJ_X86,
        FILTER_BCJ_POWERPC,
        FILTER_BCJ_IA64,
        FILTER_BCJ_ARM,
        FILTER_BCJ_ARMTHUMB,
        FILTER_BCJ_SPARC,
        FILTER_BCJ_ARM64,
    ] {
        filters.push(XzFilter::Bcj {
            id,
            start_offset: 0,
        });
    }
    filters
}

#[test]
fn every_filter_round_trips() {
    let markers: [&[u8]; 6] = [
        &[0xE8, 0x00, 0x00, 0x00],
        &[0x48, 0x00, 0x00, 0x01],
        &[0x00, 0x00, 0x00, 0xEB],
        &[0x40, 0x00, 0x00, 0x00],
        &[0x00, 0xF0, 0x00, 0xF8],
        &[0x00, 0x00, 0x00, 0x94],
    ];
    for filter in all_filters() {
        for marker in markers {
            for len in [0usize, 1, 5, 16, 17, 64, 1024, 4099] {
                let original = pattern_bytes(len, marker);
                let mut buffer = original.clone();
                filter.encode(&mut buffer);
                filter.decode(&mut buffer);
                assert_eq!(buffer, original, "{filter:?} len {len}");
            }
        }
    }
}

#[test]
fn short_buffers_are_left_alone() {
    for filter in all_filters() {
        for len in 0..5usize {
            let original: Vec<u8> = (0..len as u8).collect();
            let mut buffer = original.clone();
            filter.decode(&mut buffer);
            if matches!(filter, XzFilter::Bcj { .. }) {
                assert_eq!(buffer, original, "{filter:?} len {len}");
            }
        }
    }
}

#[test]
fn parse_accepts_valid_properties() {
    assert_eq!(
        XzFilter::parse(FILTER_DELTA, &[0]).expect("delta dist 1"),
        XzFilter::Delta { distance: 1 }
    );
    assert_eq!(
        XzFilter::parse(FILTER_DELTA, &[255]).expect("delta dist 256"),
        XzFilter::Delta { distance: 256 }
    );
    assert_eq!(
        XzFilter::parse(FILTER_BCJ_X86, &[]).expect("x86 without props"),
        XzFilter::Bcj {
            id: FILTER_BCJ_X86,
            start_offset: 0
        }
    );
    assert_eq!(
        XzFilter::parse(FILTER_BCJ_ARM, &[8, 0, 0, 0]).expect("arm with start offset"),
        XzFilter::Bcj {
            id: FILTER_BCJ_ARM,
            start_offset: 8
        }
    );
}

#[test]
fn parse_rejects_malformed_and_unsupported_filters() {
    assert!(XzFilter::parse(FILTER_DELTA, &[]).is_err());
    assert!(XzFilter::parse(FILTER_DELTA, &[0, 0]).is_err());
    assert!(XzFilter::parse(FILTER_BCJ_X86, &[0, 0]).is_err());
    // A misaligned start offset for a 4-byte-aligned architecture.
    assert!(XzFilter::parse(FILTER_BCJ_ARM, &[2, 0, 0, 0]).is_err());
    // RISC-V is 2-byte aligned: odd offsets only. (Measured against
    // `xz --format=raw --riscv=start=N`, which accepts 0/2/4/6/8 and
    // rejects 1 and 3 with "Unsupported options".)
    assert_eq!(
        XzFilter::parse(FILTER_BCJ_RISCV, &[2, 0, 0, 0]).expect("riscv start=2"),
        XzFilter::Bcj {
            id: FILTER_BCJ_RISCV,
            start_offset: 2
        }
    );
    assert!(XzFilter::parse(FILTER_BCJ_RISCV, &[1, 0, 0, 0]).is_err());
    // Unknown IDs are rejected, never silently ignored.
    assert!(XzFilter::parse(0x1234, &[]).is_err());
}

// -----------------------------------------------------------------
// liblzma cross-validation (feature `xz-oracle`)
// -----------------------------------------------------------------

/// Per-filter differential test against liblzma itself, through
/// CPython's `lzma` module.
///
/// The container-level oracle in `tests/xz_module.rs` proves the whole
/// pipeline agrees with the `xz` CLI, but it cannot see the *filtered*
/// intermediate, so a converter that never fires would pass it
/// silently. This test extracts that intermediate — compress with
/// `[filter, LZMA2]`, decompress with `[LZMA2]` alone — and compares it
/// against [`XzFilter::encode`] byte for byte, then checks
/// [`XzFilter::decode`] inverts it. It also asserts each converter
/// actually transformed something, so the comparison is meaningful.
///
/// Self-skips when `python3` (with `lzma`) is unavailable.
/// `FILTER_ARM64` is not exposed by CPython's module; ARM64 is covered
/// by the `xz` CLI test in `tests/xz_module.rs`.
#[cfg(feature = "xz-oracle")]
#[test]
fn filters_match_liblzma_byte_for_byte() {
    use std::process::Command;

    const PY: &str = r#"
import base64, os, sys, lzma

FILTERS = {
"delta1": [{"id": lzma.FILTER_DELTA, "dist": 1}],
"delta4": [{"id": lzma.FILTER_DELTA, "dist": 4}],
"delta256": [{"id": lzma.FILTER_DELTA, "dist": 256}],
"x86": [{"id": lzma.FILTER_X86}],
"powerpc": [{"id": lzma.FILTER_POWERPC}],
"ia64": [{"id": lzma.FILTER_IA64}],
"arm": [{"id": lzma.FILTER_ARM}],
"armthumb": [{"id": lzma.FILTER_ARMTHUMB}],
"sparc": [{"id": lzma.FILTER_SPARC}],
}
LZMA2 = {"id": lzma.FILTER_LZMA2, "preset": 1}

name = sys.argv[1]
payload = base64.b64decode(sys.stdin.read())
chain = FILTERS[name] + [LZMA2]
packed = lzma.compress(payload, format=lzma.FORMAT_RAW, filters=chain)
filtered = lzma.decompress(packed, format=lzma.FORMAT_RAW, filters=[LZMA2])
sys.stdout.write(base64.b64encode(filtered).decode())
"#;

    let python_ok = Command::new("python3")
        .args(["-c", "import lzma, base64"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !python_ok {
        eprintln!("[xz-oracle] python3 with `lzma` unavailable; skipping (self-skip)");
        return;
    }

    let dir = std::env::temp_dir().join(format!(
        "oxiarc_xz_filters_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let script = dir.join("filters.py");
    std::fs::write(&script, PY).expect("write script");

    // Payloads dense in each architecture's branch encodings, plus a
    // pseudo-random one, so every converter has something to convert.
    let markers: [&[u8]; 8] = [
        &[0xE8, 0x00, 0x00, 0x00],
        &[0xE9, 0xFF, 0xFF, 0xFF],
        &[0x48, 0x00, 0x10, 0x01],
        &[0x10, 0x20, 0x30, 0xEB],
        &[0x11, 0xF0, 0x22, 0xF8],
        &[0x40, 0x00, 0x12, 0x34],
        &[0x16, 0x00, 0x00, 0x00],
        &[0x7F, 0xC0, 0x00, 0x00],
    ];
    let mut payloads: Vec<Vec<u8>> = Vec::new();
    for marker in markers {
        let mut data = Vec::with_capacity(32 * 1024);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        while data.len() < 32 * 1024 {
            data.extend_from_slice(marker);
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            data.extend_from_slice(&seed.to_le_bytes());
            data.extend_from_slice(marker);
        }
        data.truncate(32 * 1024);
        payloads.push(data);
    }
    // Pure noise: exercises the "must not fire" side of every rule.
    let mut noise = Vec::with_capacity(32 * 1024);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    while noise.len() < 32 * 1024 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        noise.extend_from_slice(&seed.to_le_bytes());
    }
    payloads.push(noise);

    let cases: [(&str, XzFilter); 9] = [
        ("delta1", XzFilter::Delta { distance: 1 }),
        ("delta4", XzFilter::Delta { distance: 4 }),
        ("delta256", XzFilter::Delta { distance: 256 }),
        (
            "x86",
            XzFilter::Bcj {
                id: FILTER_BCJ_X86,
                start_offset: 0,
            },
        ),
        (
            "powerpc",
            XzFilter::Bcj {
                id: FILTER_BCJ_POWERPC,
                start_offset: 0,
            },
        ),
        (
            "ia64",
            XzFilter::Bcj {
                id: FILTER_BCJ_IA64,
                start_offset: 0,
            },
        ),
        (
            "arm",
            XzFilter::Bcj {
                id: FILTER_BCJ_ARM,
                start_offset: 0,
            },
        ),
        (
            "armthumb",
            XzFilter::Bcj {
                id: FILTER_BCJ_ARMTHUMB,
                start_offset: 0,
            },
        ),
        (
            "sparc",
            XzFilter::Bcj {
                id: FILTER_BCJ_SPARC,
                start_offset: 0,
            },
        ),
    ];

    let mut checked = 0usize;
    for (name, filter) in cases {
        let mut fired = false;
        for payload in &payloads {
            let reference = run_python(&script, name, payload);
            assert_eq!(reference.len(), payload.len(), "[{name}] length changed");

            let mut mine = payload.clone();
            filter.encode(&mut mine);
            assert_eq!(mine, reference, "[{name}] encode differs from liblzma");

            filter.decode(&mut mine);
            assert_eq!(&mine, payload, "[{name}] decode does not invert encode");

            if reference != *payload {
                fired = true;
            }
            checked += 1;
        }
        assert!(
            fired,
            "[{name}] the converter never fired on any payload; the comparison would be vacuous"
        );
    }

    eprintln!("[xz-oracle] {checked} filter/payload pairs match liblzma byte for byte");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Run the liblzma driver and return the filtered intermediate.
#[cfg(feature = "xz-oracle")]
fn run_python(script: &std::path::Path, name: &str, payload: &[u8]) -> Vec<u8> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("python3")
        .arg(script)
        .arg(name)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn python3");
    let encoded = base64_encode(payload);
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(encoded.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait for python3");
    assert!(
        output.status.success(),
        "python driver failed for {name}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    base64_decode(&output.stdout).expect("decode python output")
}

/// Minimal base64 (the test pipes binary through a text stream; adding
/// a dependency for this would violate the workspace's dependency
/// policy for a four-line helper).
#[cfg(feature = "xz-oracle")]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Inverse of [`base64_encode`]; `None` on malformed input.
#[cfg(feature = "xz-oracle")]
fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for &byte in data {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// Second, independent per-filter oracle: the `xz` CLI's `--format=raw`
/// mode, which can encode with `[filter, LZMA2]` and then decode with
/// `[LZMA2]` alone, exposing the filtered intermediate.
///
/// This covers **ARM64 and RISC-V**, which CPython's `lzma` module does
/// not expose, and cross-checks every other converter through a second
/// code path. Both directions are compared, because a converter whose
/// encoder and decoder are different scans (RISC-V) can round-trip its
/// own output perfectly while disagreeing with liblzma. Each case
/// asserts the converter actually fired, so a no-op implementation
/// cannot pass.
#[cfg(feature = "xz-oracle")]
#[test]
fn filters_match_the_xz_cli_byte_for_byte() {
    let Some(dir) = oracle_dir("cli") else {
        return;
    };
    let payload = branch_dense_payload();

    let cases: [(&str, XzFilter); 11] = [
        ("--delta=dist=1", XzFilter::Delta { distance: 1 }),
        ("--delta=dist=4", XzFilter::Delta { distance: 4 }),
        ("--delta=dist=256", XzFilter::Delta { distance: 256 }),
        ("--x86", bcj(FILTER_BCJ_X86)),
        ("--powerpc", bcj(FILTER_BCJ_POWERPC)),
        ("--ia64", bcj(FILTER_BCJ_IA64)),
        ("--arm", bcj(FILTER_BCJ_ARM)),
        ("--armthumb", bcj(FILTER_BCJ_ARMTHUMB)),
        ("--sparc", bcj(FILTER_BCJ_SPARC)),
        ("--arm64", bcj(FILTER_BCJ_ARM64)),
        ("--riscv", bcj(FILTER_BCJ_RISCV)),
    ];

    let mut checked = 0usize;
    for (flag, filter) in cases {
        compare_with_xz(&dir, flag, filter, &payload);
        checked += 1;
    }

    eprintln!(
        "[xz-oracle] {checked} filters match the `xz` CLI byte for byte in both \
         directions (ARM64 and RISC-V included)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same differential at **non-zero start offsets**.
///
/// Every BCJ converter folds `now_pos + i` into the branch target and
/// x86 additionally seeds `prev_pos` with `now_pos - 5`, so a start
/// offset near `u32::MAX` exercises wrap-around arithmetic that a
/// crafted block header can reach from untrusted input. The hermetic
/// tests below prove those cases round-trip; only this one proves they
/// round-trip to the *same bytes liblzma produces*.
#[cfg(feature = "xz-oracle")]
#[test]
fn bcj_filters_match_the_xz_cli_at_nonzero_start_offsets() {
    let Some(dir) = oracle_dir("cli_start") else {
        return;
    };
    let payload = branch_dense_payload();

    let flags: [(&str, u64); 8] = [
        ("x86", FILTER_BCJ_X86),
        ("powerpc", FILTER_BCJ_POWERPC),
        ("ia64", FILTER_BCJ_IA64),
        ("arm", FILTER_BCJ_ARM),
        ("armthumb", FILTER_BCJ_ARMTHUMB),
        ("sparc", FILTER_BCJ_SPARC),
        ("arm64", FILTER_BCJ_ARM64),
        ("riscv", FILTER_BCJ_RISCV),
    ];

    let mut checked = 0usize;
    for (name, id) in flags {
        let alignment = bcj_alignment(id);
        for candidate in [16u32, 4096, 0x8000_0000, u32::MAX - 64, u32::MAX] {
            let start_offset = candidate - (candidate % alignment);
            let flag = format!("--{name}=start={start_offset}");
            compare_with_xz(&dir, &flag, XzFilter::Bcj { id, start_offset }, &payload);
            checked += 1;
        }
    }

    eprintln!("[xz-oracle] {checked} (filter, start_offset) pairs match the `xz` CLI");
    let _ = std::fs::remove_dir_all(&dir);
}

/// RISC-V over several payload shapes, not just the shared one.
///
/// The filter has four distinct conversion paths (JAL, a real
/// AUIPC+inst2 pair, the "fake" pair the encoder undoes to stay
/// bijective, and the special-marker form the decoder undoes) whose
/// triggers are rare in random data. Each shape below is aimed at a
/// different one, and the assertions inside [`compare_with_xz`] require
/// the converter to fire on every shape in both directions.
#[cfg(feature = "xz-oracle")]
#[test]
fn riscv_matches_the_xz_cli_on_many_payloads() {
    let Some(dir) = oracle_dir("cli_riscv") else {
        return;
    };
    let filter = bcj(FILTER_BCJ_RISCV);
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    // 1. Synthetic RISC-V code: AUIPC paired with JALR/ADDI/LD/SD over
    //    every register, plus JAL with each convertible rd.
    let mut code = Vec::with_capacity(48 * 1024);
    for register in 0..32u32 {
        for pair_opcode in [0x67u32, 0x13, 0x03, 0x23] {
            let immediate = next() as u32 & 0xFFF;
            let auipc = 0x17 | (register << 7) | ((next() as u32 & 0xF_FFFF) << 12);
            let inst2 = pair_opcode | (register << 15) | (immediate << 20);
            code.extend_from_slice(&auipc.to_le_bytes());
            code.extend_from_slice(&inst2.to_le_bytes());
        }
        // JAL with rd = x1 and rd = x5 (converted) and rd = x0 (not).
        for rd in [1u32, 5, 0] {
            let jal = 0x6F | (rd << 7) | ((next() as u32 & 0xF_FFFF) << 12);
            code.extend_from_slice(&jal.to_le_bytes());
        }
        // Unaligned filler, so pairs also start at odd 2-byte offsets.
        code.extend_from_slice(&(next() as u16).to_le_bytes());
    }
    while code.len() < 48 * 1024 {
        let block = code.clone();
        code.extend_from_slice(&block);
    }
    code.truncate(48 * 1024);

    // 2. Dense in the encoder's special marker format, which is what
    //    drives the "fake" conversions on both sides.
    let mut markers = Vec::with_capacity(32 * 1024);
    while markers.len() < 32 * 1024 {
        // AUIPC opcode, rd = x2, the two packed opcode bits set, and a
        // non-x0/x2 value in the top five bits.
        let special = 0x3117u32 | (((next() as u32 % 30) + 1) << 27);
        markers.extend_from_slice(&special.to_le_bytes());
        markers.extend_from_slice(&(next() as u32).to_le_bytes());
    }
    markers.truncate(32 * 1024);

    // 3. Pure noise: the "must not fire wrongly" side, and the shape a
    //    `.xz` bomb would carry.
    let mut noise = Vec::with_capacity(32 * 1024);
    while noise.len() < 32 * 1024 {
        noise.extend_from_slice(&next().to_le_bytes());
    }

    // 4. Noise with every RISC-V opcode byte sprayed in, so both the
    //    AUIPC and JAL scans fire at many alignments.
    let mut mixed = Vec::with_capacity(32 * 1024);
    while mixed.len() < 32 * 1024 {
        for byte in next().to_le_bytes() {
            mixed.push(match byte % 5 {
                0 => 0x17,
                1 => 0x97,
                2 => 0xEF,
                3 => 0xE7,
                _ => byte,
            });
        }
    }

    for (name, payload) in [
        ("code", &code),
        ("markers", &markers),
        ("noise", &noise),
        ("mixed", &mixed),
    ] {
        compare_with_xz(&dir, "--riscv", filter, payload);
        eprintln!("[xz-oracle] riscv payload `{name}` matches the `xz` CLI");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Compare both directions of one filter against the `xz` CLI over
/// `payload`, asserting the converter actually fired each way.
#[cfg(feature = "xz-oracle")]
fn compare_with_xz(dir: &std::path::Path, flag: &str, filter: XzFilter, payload: &[u8]) {
    let raw = dir.join("payload.bin");
    std::fs::write(&raw, payload).expect("write payload");

    // Encode direction: compress with `[filter, LZMA2]`, then strip
    // LZMA2 alone — what is left is liblzma's filtered intermediate.
    let packed = run_xz(
        &["--format=raw", "-T1", "-c", flag, "--lzma2=preset=1"],
        &raw,
        flag,
    );
    let packed_path = dir.join("packed.raw");
    std::fs::write(&packed_path, &packed).expect("write packed");
    let reference = run_xz(
        &["-d", "--format=raw", "-T1", "-c", "--lzma2=preset=1"],
        &packed_path,
        flag,
    );

    assert_eq!(reference.len(), payload.len(), "[{flag}] length changed");
    assert_ne!(
        reference, payload,
        "[{flag}] the converter never fired; the comparison would be vacuous"
    );

    let mut mine = payload.to_vec();
    filter.encode(&mut mine);
    assert_eq!(mine, reference, "[{flag}] encode differs from the xz CLI");

    filter.decode(&mut mine);
    assert_eq!(mine, payload, "[{flag}] decode does not invert encode");

    // Decode direction, over bytes liblzma's encoder would never emit:
    // wrap the plain payload in LZMA2 alone and let `xz -d` undo the
    // filter on the way out. This is the check that a self-consistent
    // but wrong converter fails.
    let lzma2_only = run_xz(
        &["--format=raw", "-T1", "-c", "--lzma2=preset=1"],
        &raw,
        flag,
    );
    let lzma2_path = dir.join("lzma2_only.raw");
    std::fs::write(&lzma2_path, &lzma2_only).expect("write lzma2-only");
    let reference_decoded = run_xz(
        &["-d", "--format=raw", "-T1", "-c", flag, "--lzma2=preset=1"],
        &lzma2_path,
        flag,
    );

    assert_eq!(
        reference_decoded.len(),
        payload.len(),
        "[{flag}] reverse length changed"
    );
    assert_ne!(
        reference_decoded, payload,
        "[{flag}] the decoder never fired; the comparison would be vacuous"
    );

    let mut mine_decoded = payload.to_vec();
    filter.decode(&mut mine_decoded);
    assert_eq!(
        mine_decoded, reference_decoded,
        "[{flag}] decode differs from the xz CLI on arbitrary input"
    );
}

/// Run `xz` with `args` over `path`, returning stdout.
#[cfg(feature = "xz-oracle")]
fn run_xz(args: &[&str], path: &std::path::Path, flag: &str) -> Vec<u8> {
    let output = std::process::Command::new("xz")
        .args(args)
        .arg(path)
        .output()
        .expect("spawn xz");
    assert!(
        output.status.success(),
        "xz {args:?} ({flag}) failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// A private temp directory, or `None` when `xz` is not installed (the
/// oracle tests self-skip rather than fail).
#[cfg(feature = "xz-oracle")]
fn oracle_dir(tag: &str) -> Option<std::path::PathBuf> {
    let xz_ok = std::process::Command::new("xz")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if !xz_ok {
        eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip)");
        return None;
    }
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_xz_filters_{tag}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    Some(dir)
}

/// 64 KiB dense in every architecture's branch encodings.
#[cfg(feature = "xz-oracle")]
fn branch_dense_payload() -> Vec<u8> {
    let mut payload = Vec::with_capacity(64 * 1024);
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let markers: [[u8; 4]; 16] = [
        [0xE8, 0x00, 0x00, 0x00],
        [0xE9, 0xFF, 0xFF, 0xFF],
        [0x48, 0x00, 0x10, 0x01],
        [0x10, 0x20, 0x30, 0xEB],
        [0x11, 0xF0, 0x22, 0xF8],
        [0x40, 0x00, 0x12, 0x34],
        [0x11, 0x22, 0x33, 0x94],
        [0x11, 0x22, 0x33, 0x90],
        [0x16, 0x00, 0x00, 0x00],
        // RISC-V. In order: `auipc ra, 0x12` + `jalr ra, 0x34(ra)` —
        // a real AUIPC+inst2 pair, the filter's main case; `auipc a0,
        // 0x56` + `addi a0, a0, 0x78` — the other common pair shape;
        // `jal ra, ...`; an `auipc` whose rd is x0 (a landing pad, never
        // converted); and an instance of the encoder's special marker
        // format, which drives the "fake" conversion that keeps the
        // filter bijective on non-code data.
        [0x97, 0x20, 0x01, 0x00],
        [0xE7, 0x80, 0x40, 0x03],
        [0x17, 0x65, 0x00, 0x00],
        [0x13, 0x05, 0x85, 0x07],
        [0xEF, 0x00, 0x12, 0x34],
        [0x17, 0x00, 0x00, 0x00],
        [0x17, 0x31, 0x00, 0x08],
    ];
    while payload.len() < 64 * 1024 {
        for marker in markers {
            payload.extend_from_slice(&marker);
        }
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        payload.extend_from_slice(&seed.to_le_bytes());
    }
    payload.truncate(64 * 1024);
    payload
}

/// Shorthand for a BCJ filter with no start offset.
#[cfg(feature = "xz-oracle")]
fn bcj(id: u64) -> XzFilter {
    XzFilter::Bcj {
        id,
        start_offset: 0,
    }
}

#[test]
fn delta_matches_the_textbook_definition() {
    let original: Vec<u8> = (0..64u8).collect();
    let mut encoded = original.clone();
    delta_encode(&mut encoded, 1);
    assert_eq!(encoded[0], 0);
    assert!(encoded[1..].iter().all(|&b| b == 1));
    delta_decode(&mut encoded, 1);
    assert_eq!(encoded, original);
}

// -----------------------------------------------------------------
// Start offsets (`start_offset` property of every BCJ filter)
// -----------------------------------------------------------------

/// Every BCJ converter's arithmetic is `now_pos`-relative and wraps:
/// x86 seeds `prev_pos` with `now_pos - 5`, and each converter folds
/// `now_pos + i` into the branch target. All of that is exercised only
/// when `start_offset != 0`, which every other test in this crate
/// leaves at zero. A crafted block header may set it to any aligned
/// 32-bit value, so the wrap-around cases are reachable from untrusted
/// input; an index or arithmetic panic there would be a decode-time
/// denial of service.
fn start_offsets_for(id: u64) -> Vec<u32> {
    let alignment = bcj_alignment(id);
    let mut offsets = vec![0u32];
    for candidate in [
        1u32,
        5,
        16,
        4096,
        0x0001_0000,
        0x8000_0000,
        u32::MAX - 64,
        u32::MAX - 16,
        u32::MAX - 4,
        u32::MAX,
    ] {
        // Keep only what `XzFilter::parse` would accept.
        let aligned = candidate - (candidate % alignment);
        if !offsets.contains(&aligned) {
            offsets.push(aligned);
        }
    }
    offsets
}

fn bcj_ids() -> [u64; 8] {
    [
        FILTER_BCJ_X86,
        FILTER_BCJ_POWERPC,
        FILTER_BCJ_IA64,
        FILTER_BCJ_ARM,
        FILTER_BCJ_ARMTHUMB,
        FILTER_BCJ_SPARC,
        FILTER_BCJ_ARM64,
        FILTER_BCJ_RISCV,
    ]
}

#[test]
fn bcj_converters_round_trip_at_every_start_offset() {
    let markers: [&[u8]; 11] = [
        &[0xE8, 0x00, 0x00, 0x00],
        &[0xE9, 0xFF, 0xFF, 0xFF],
        &[0x48, 0x00, 0x00, 0x01],
        &[0x00, 0x00, 0x00, 0xEB],
        &[0x40, 0x00, 0x00, 0x00],
        &[0x00, 0xF0, 0x00, 0xF8],
        &[0x00, 0x00, 0x00, 0x94],
        // RISC-V: `auipc ra, 0`, `jalr ra, 0(ra)`, `jal ra, 0`, and a
        // hand-built instance of the encoder's special (marker) format,
        // which drives the bijectivity ("fake" conversion) branches.
        &[0x97, 0x00, 0x00, 0x00],
        &[0xE7, 0x80, 0x00, 0x00],
        &[0xEF, 0x00, 0x00, 0x00],
        &[0x17, 0x31, 0x00, 0x08],
    ];
    for id in bcj_ids() {
        for start_offset in start_offsets_for(id) {
            let filter = XzFilter::Bcj { id, start_offset };
            for marker in markers {
                for len in [0usize, 4, 5, 15, 16, 17, 32, 64, 256, 1024, 4099] {
                    let original = pattern_bytes(len, marker);
                    let mut buffer = original.clone();
                    filter.encode(&mut buffer);
                    filter.decode(&mut buffer);
                    assert_eq!(
                        buffer, original,
                        "{filter:?} start_offset {start_offset} len {len}"
                    );
                }
            }
        }
    }
}

#[test]
fn bcj_converters_never_panic_on_hostile_bytes() {
    // A dense mixture of every converter's trigger bytes, so the branch
    // that indexes `MASK_TO_BIT_NUMBER` with `prev_mask >> 1` (x86) is
    // driven hard from many `prev_mask` histories.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut data = Vec::with_capacity(8192);
    while data.len() < 8192 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let bytes = seed.to_le_bytes();
        // Bias heavily toward every converter's trigger bytes (the
        // last three are RISC-V: AUIPC, JALR and JAL opcodes) so they
        // fire on nearly every position instead of almost never.
        for byte in bytes {
            data.push(match byte % 13 {
                0 => 0xE8,
                1 => 0xE9,
                2 => 0xEB,
                3 => 0x48,
                4 => 0x40,
                5 => 0xF0,
                6 => 0xF8,
                7 => 0x94,
                8 => 0x00,
                9 => 0x17,
                10 => 0xE7,
                11 => 0xEF,
                _ => byte,
            });
        }
    }

    for id in bcj_ids() {
        for start_offset in start_offsets_for(id) {
            let filter = XzFilter::Bcj { id, start_offset };
            for window in [1usize, 5, 16, 17, 63, 4096, data.len()] {
                let mut buffer = data[..window.min(data.len())].to_vec();
                let before = buffer.len();
                filter.decode(&mut buffer);
                assert_eq!(buffer.len(), before, "{filter:?} changed the block length");
                // And the inverse must still be an inverse.
                let decoded = buffer.clone();
                filter.encode(&mut buffer);
                filter.decode(&mut buffer);
                assert_eq!(buffer, decoded, "{filter:?} start_offset {start_offset}");
            }
        }
    }
}

#[test]
fn delta_never_panics_on_any_distance() {
    for distance in 1..=256usize {
        for len in [0usize, 1, 2, 255, 256, 257, 1024] {
            let original = pattern_bytes(len, &[0x01, 0x02, 0x03, 0x04]);
            let mut buffer = original.clone();
            delta_encode(&mut buffer, distance);
            delta_decode(&mut buffer, distance);
            assert_eq!(buffer, original, "distance {distance} len {len}");
        }
    }
}

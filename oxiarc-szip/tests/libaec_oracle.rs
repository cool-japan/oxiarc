//! Live differential oracle against a locally installed libaec (the CCSDS
//! 121.0-B reference implementation), validating BOTH directions:
//!
//! 1. libaec-encode → `oxiarc_szip::decode` must be byte-identical.
//! 2. `oxiarc_szip::encode_bytes` → libaec-decode must be accepted and
//!    byte-identical.
//!
//! Gated behind the `libaec-oracle` feature. At runtime the test compiles a
//! tiny C shim against libaec found under `$LIBAEC_PREFIX`, `/opt/homebrew`,
//! `/usr/local`, or `/usr`; it self-skips (prints a note, does not fail)
//! when no C compiler or libaec installation is available. The always-run
//! interop gate lives in `tests/libaec_interop.rs` (embedded fixtures).
#![cfg(feature = "libaec-oracle")]

use oxiarc_szip::{SzipParams, decode, encode_bytes};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const AEC_DATA_MSB: u32 = 4;
const AEC_DATA_PREPROCESS: u32 = 8;
const AEC_PAD_RSI: u32 = 32;

/// C shim around libaec's buffer API (see libaec.h).
const HARNESS_C: &str = r#"
#include <libaec.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static unsigned char *read_file(const char *path, size_t *len)
{
    FILE *f = fopen(path, "rb");
    if (!f) { perror("fopen in"); exit(3); }
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    unsigned char *buf = malloc(n > 0 ? (size_t)n : 1);
    if (n > 0 && fread(buf, 1, (size_t)n, f) != (size_t)n) { perror("fread"); exit(3); }
    fclose(f);
    *len = (size_t)n;
    return buf;
}

static void write_file(const char *path, const unsigned char *buf, size_t len)
{
    FILE *f = fopen(path, "wb");
    if (!f) { perror("fopen out"); exit(3); }
    if (len > 0 && fwrite(buf, 1, len, f) != len) { perror("fwrite"); exit(3); }
    fclose(f);
}

int main(int argc, char **argv)
{
    if (argc < 8) { fprintf(stderr, "bad usage\n"); return 3; }

    struct aec_stream strm;
    memset(&strm, 0, sizeof(strm));
    strm.bits_per_sample = (unsigned)atoi(argv[2]);
    strm.block_size = (unsigned)atoi(argv[3]);
    strm.rsi = (unsigned)atoi(argv[4]);
    strm.flags = (unsigned)atoi(argv[5]);

    if (strcmp(argv[1], "encode") == 0) {
        size_t in_len;
        unsigned char *in = read_file(argv[6], &in_len);
        size_t out_cap = in_len * 4 + 65536;
        unsigned char *out = malloc(out_cap);
        strm.next_in = in;
        strm.avail_in = in_len;
        strm.next_out = out;
        strm.avail_out = out_cap;
        int status = aec_buffer_encode(&strm);
        if (status != AEC_OK) {
            fprintf(stderr, "aec_buffer_encode failed: %d\n", status);
            return 2;
        }
        write_file(argv[7], out, strm.total_out);
        return 0;
    } else if (strcmp(argv[1], "decode") == 0 && argc >= 9) {
        size_t out_len = (size_t)strtoull(argv[6], NULL, 10);
        size_t in_len;
        unsigned char *in = read_file(argv[7], &in_len);
        unsigned char *out = malloc(out_len ? out_len : 1);
        memset(out, 0xA5, out_len);
        strm.next_in = in;
        strm.avail_in = in_len;
        strm.next_out = out;
        strm.avail_out = out_len;
        int status = aec_buffer_decode(&strm);
        if (status != AEC_OK) {
            fprintf(stderr, "aec_buffer_decode failed: %d\n", status);
            return 2;
        }
        if (strm.total_out != out_len) {
            fprintf(stderr, "short output: %zu of %zu\n", strm.total_out, out_len);
            return 2;
        }
        write_file(argv[8], out, out_len);
        return 0;
    }
    fprintf(stderr, "unknown mode\n");
    return 3;
}
"#;

/// Locate a libaec installation prefix (include/libaec.h + lib/libaec.*).
fn find_libaec_prefix() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env_prefix) = std::env::var("LIBAEC_PREFIX") {
        candidates.push(PathBuf::from(env_prefix));
    }
    candidates.push(PathBuf::from("/opt/homebrew"));
    candidates.push(PathBuf::from("/usr/local"));
    candidates.push(PathBuf::from("/usr"));

    candidates.into_iter().find(|prefix| {
        let header = prefix.join("include").join("libaec.h");
        let lib = prefix.join("lib");
        header.is_file()
            && ["libaec.dylib", "libaec.so", "libaec.a"]
                .iter()
                .any(|name| lib.join(name).is_file())
    })
}

/// Compile the harness once per process; `None` means self-skip.
fn harness() -> Option<&'static PathBuf> {
    static HARNESS: OnceLock<Option<PathBuf>> = OnceLock::new();
    HARNESS
        .get_or_init(|| {
            let prefix = find_libaec_prefix()?;
            let dir = std::env::temp_dir()
                .join(format!("oxiarc_szip_libaec_oracle_{}", std::process::id()));
            std::fs::create_dir_all(&dir).ok()?;
            let src = dir.join("aec_harness.c");
            std::fs::write(&src, HARNESS_C).ok()?;
            let bin = dir.join("aec_harness");
            let lib_dir = prefix.join("lib");
            let status = Command::new("cc")
                .arg("-O2")
                .arg("-I")
                .arg(prefix.join("include"))
                .arg("-L")
                .arg(&lib_dir)
                .arg(format!("-Wl,-rpath,{}", lib_dir.display()))
                .arg("-laec")
                .arg(&src)
                .arg("-o")
                .arg(&bin)
                .status()
                .ok()?;
            if status.success() { Some(bin) } else { None }
        })
        .as_ref()
}

// ── Deterministic data generation ──────────────────────────────────────────

struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn gen_samples(pattern: &str, n: usize, xmax: u64, seed: u64) -> Vec<u64> {
    let mut rng = Xorshift64::new(seed);
    match pattern {
        "zeros" => vec![0; n],
        "const" => vec![42.min(xmax); n],
        "ramp" => (0..n as u64).map(|i| i % (xmax + 1)).collect(),
        "smooth" => {
            let mut v = (xmax / 2) as i64;
            (0..n)
                .map(|_| {
                    v += (rng.next_u64() % 7) as i64 - 3;
                    v = v.clamp(0, xmax as i64);
                    v as u64
                })
                .collect()
        }
        "random" => (0..n).map(|_| rng.next_u64() % (xmax + 1)).collect(),
        "tiny" => (0..n).map(|_| rng.next_u64() % 2).collect(),
        "mixed" => {
            let q = (n / 4).max(1);
            let mut v = (xmax / 2) as i64;
            (0..n)
                .map(|i| {
                    if i < q || i >= 3 * q {
                        0
                    } else if i < 2 * q {
                        v += (rng.next_u64() % 5) as i64 - 2;
                        v = v.clamp(0, xmax as i64);
                        v as u64
                    } else {
                        rng.next_u64() % (xmax + 1)
                    }
                })
                .collect()
        }
        other => panic!("unknown pattern {other}"),
    }
}

fn pack_be(samples: &[u64], bps: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * bps);
    for &s in samples {
        match bps {
            1 => out.push(s as u8),
            2 => out.extend_from_slice(&(s as u16).to_be_bytes()),
            _ => out.extend_from_slice(&(s as u32).to_be_bytes()),
        }
    }
    out
}

fn bytes_per_sample(bpp: u8) -> usize {
    match bpp {
        1..=8 => 1,
        9..=16 => 2,
        _ => 4,
    }
}

// ── The oracle matrix ──────────────────────────────────────────────────────

#[test]
fn libaec_differential_both_directions() {
    let Some(harness) = harness() else {
        eprintln!(
            "[libaec-oracle] no C compiler or libaec installation found; \
             skipping (self-skip, not a failure)"
        );
        return;
    };

    let dir = std::env::temp_dir().join(format!("oxiarc_szip_oracle_io_{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create oracle temp dir");
    let raw_f = dir.join("raw.bin");
    let enc_f = dir.join("stream.aec");
    let dec_f = dir.join("decoded.bin");

    let configs: &[(u8, u32, u32)] = &[
        // (bits_per_pixel, pixels_per_block, rsi in blocks)
        (8, 8, 1),
        (8, 16, 4),
        (8, 64, 2),
        (8, 8, 128),
        (4, 8, 2),
        (1, 8, 2),
        (16, 16, 2),
        (16, 8, 1),
        (13, 16, 2),
        (32, 8, 1),
        (32, 16, 4),
    ];
    let patterns = ["zeros", "ramp", "smooth", "random", "tiny", "mixed"];

    let (mut total_a, mut ok_a, mut total_b, mut ok_b) = (0u32, 0u32, 0u32, 0u32);
    let mut case: u64 = 0;

    for &(bpp, ppb, rsi_blocks) in configs {
        let xmax = (1u64 << bpp) - 1;
        let bps = bytes_per_sample(bpp);
        let rsi_samples = rsi_blocks * ppb;
        let mut sizes = vec![
            1usize,
            ppb as usize - 1,
            ppb as usize,
            rsi_samples as usize,
            rsi_samples as usize + 1,
            3 * rsi_samples as usize + 5,
            1000,
        ];
        sizes.sort_unstable();
        sizes.dedup();

        for pattern in patterns {
            for &n in &sizes {
                for pp in [false, true] {
                    for pad in [false, true] {
                        case += 1;
                        let samples =
                            gen_samples(pattern, n, xmax, 0xC0FF_EE00 ^ (case * 0x9E37_79B9));
                        let raw = pack_be(&samples, bps);
                        std::fs::write(&raw_f, &raw).expect("write raw");

                        let flags = AEC_DATA_MSB
                            | if pp { AEC_DATA_PREPROCESS } else { 0 }
                            | if pad { AEC_PAD_RSI } else { 0 };
                        let params = SzipParams {
                            bits_per_pixel: bpp,
                            pixels_per_block: ppb,
                            samples: n,
                            reference_sample_interval: rsi_samples,
                            msb: true,
                            nn_preprocess: pp,
                            rsi_byte_align: pad,
                        };
                        let tag = format!(
                            "bpp={bpp} J={ppb} rsi={rsi_blocks} pat={pattern} n={n} pp={pp} pad={pad}"
                        );

                        // Direction A: libaec encode → oxiarc decode.
                        // (AEC_PAD_RSI is decode-only in libaec, so the
                        // reference encoder direction skips pad=true.)
                        if !pad {
                            total_a += 1;
                            let status = Command::new(harness)
                                .args([
                                    "encode",
                                    &bpp.to_string(),
                                    &ppb.to_string(),
                                    &rsi_blocks.to_string(),
                                    &flags.to_string(),
                                ])
                                .arg(&raw_f)
                                .arg(&enc_f)
                                .output()
                                .expect("run harness encode");
                            assert!(
                                status.status.success(),
                                "[{tag}] libaec encode failed: {}",
                                String::from_utf8_lossy(&status.stderr)
                            );
                            let stream = std::fs::read(&enc_f).expect("read libaec stream");
                            let decoded = decode(&stream, &params)
                                .unwrap_or_else(|e| panic!("[{tag}] oxiarc decode failed: {e}"));
                            assert_eq!(decoded, raw, "[{tag}] direction A byte mismatch");
                            ok_a += 1;
                        }

                        // Direction B: oxiarc encode → libaec decode.
                        total_b += 1;
                        let encoded = encode_bytes(&raw, &params)
                            .unwrap_or_else(|e| panic!("[{tag}] oxiarc encode failed: {e}"));
                        std::fs::write(&enc_f, &encoded).expect("write oxiarc stream");
                        let status = Command::new(harness)
                            .args([
                                "decode",
                                &bpp.to_string(),
                                &ppb.to_string(),
                                &rsi_blocks.to_string(),
                                &flags.to_string(),
                                &raw.len().to_string(),
                            ])
                            .arg(&enc_f)
                            .arg(&dec_f)
                            .output()
                            .expect("run harness decode");
                        assert!(
                            status.status.success(),
                            "[{tag}] libaec rejected oxiarc stream: {}",
                            String::from_utf8_lossy(&status.stderr)
                        );
                        let roundtrip = std::fs::read(&dec_f).expect("read libaec output");
                        assert_eq!(roundtrip, raw, "[{tag}] direction B byte mismatch");
                        ok_b += 1;
                    }
                }
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    eprintln!(
        "[libaec-oracle] direction A (libaec->oxiarc): {ok_a}/{total_a} byte-identical; \
         direction B (oxiarc->libaec): {ok_b}/{total_b} byte-identical"
    );
    assert_eq!(ok_a, total_a);
    assert_eq!(ok_b, total_b);
}

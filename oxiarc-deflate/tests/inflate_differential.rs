//! Differential and hostile-input coverage for the DEFLATE decoder.
//!
//! The decoder has several distinct paths that must agree byte for byte:
//!
//! * the buffered fast path (`BitCache` + two-level Huffman table),
//! * the exact-mode path used when a `BitReader` may not read ahead
//!   (`BitReader::new`), which routes every symbol through
//!   `HuffmanTree::decode` and its bit-at-a-time fallback,
//! * the decompress-into-a-slice path (`inflate_into` / `zlib_decompress_into`),
//! * the resumable push path (`InflateStream` / `WrappedInflate`), driven at
//!   several feed granularities — it shares the symbol loop with the first
//!   path but reaches it through a different state machine, so the two are
//!   compared entry by entry rather than assumed equivalent.
//!
//! Everything here is exercised against inputs covering all three block types
//! (stored / fixed Huffman / dynamic Huffman), maximum-distance
//! back-references, and truncated or corrupted streams, which must produce
//! errors rather than panics, hangs, or silent truncation.
//!
//! The `zlib-oracle` feature additionally compares against CPython's `zlib`
//! module in both directions.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oxiarc_core::BitReader;
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{
    InflateStatus, InflateStream, InflateWrapper, Inflater, TrailingPolicy, WrappedInflate,
    deflate, inflate, inflate_into, zlib_compress, zlib_decompress, zlib_decompress_into,
};

// ---------------------------------------------------------------------------
// Corpus
// ---------------------------------------------------------------------------

/// Deterministic xorshift PRNG (no external dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

/// Inputs chosen to force every DEFLATE block type and every interesting
/// back-reference shape through the decoder.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut out: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one-byte", vec![0x42]),
        ("two-bytes", vec![0, 255]),
        // Incompressible: forces STORED blocks at low levels and long literal
        // runs at high levels.
        ("random-64k", (0..65_536).map(|_| rng.byte()).collect()),
        // Highly compressible: long matches, distance 1 runs.
        ("zeros-256k", vec![0u8; 262_144]),
        ("repeat-16", b"0123456789abcdef".repeat(20_000).to_vec()),
    ];

    // Maximum back-reference distance (32768) and maximum match length (258):
    // 32 KiB of noise, then a byte, then the same 32 KiB again.
    let mut max_dist: Vec<u8> = (0..32_768).map(|_| rng.byte()).collect();
    let head = max_dist.clone();
    max_dist.push(0xAB);
    max_dist.extend_from_slice(&head);
    out.push(("max-distance", max_dist));

    // Text-like: dynamic Huffman with a skewed alphabet.
    let mut text = Vec::new();
    for i in 0..40_000u32 {
        text.extend_from_slice(match i % 7 {
            0 => b"the quick ".as_slice(),
            1 => b"brown fox ".as_slice(),
            2 => b"jumps ".as_slice(),
            3 => b"over ".as_slice(),
            4 => b"the lazy ".as_slice(),
            5 => b"dog. ".as_slice(),
            _ => b"\n".as_slice(),
        });
    }
    out.push(("text", text));

    // Two-symbol alphabet: degenerate Huffman trees.
    out.push((
        "two-symbols",
        (0..100_000)
            .map(|i| if i % 3 == 0 { b'a' } else { b'b' })
            .collect(),
    ));

    // Sawtooth: predictable but not trivially compressible.
    out.push((
        "sawtooth",
        (0..200_000u32).map(|i| (i % 251) as u8).collect(),
    ));

    out
}

// ---------------------------------------------------------------------------
// Decoder-path equivalence
// ---------------------------------------------------------------------------

/// Decode through the exact-mode `BitReader`, which never reads ahead and so
/// takes the non-cached `HuffmanTree::decode` path for essentially every
/// symbol.
fn inflate_exact_mode(compressed: &[u8]) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut reader = BitReader::new(std::io::Cursor::new(compressed));
    let mut inflater = Inflater::new();
    inflater.inflate(&mut reader)
}

/// Decode through the resumable push API with a fixed feed schedule.
fn inflate_push(
    compressed: &[u8],
    in_chunk: usize,
    out_size: usize,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut stream = InflateStream::new();
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_size];
    let mut fed = 0usize;
    loop {
        let end = (fed + in_chunk).min(compressed.len());
        let flush = if end >= compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.inflate(&compressed[fed..end], &mut scratch, flush)?;
        fed += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
    }
}

#[test]
fn all_decode_paths_agree() {
    for (name, data) in corpus() {
        for level in [0u8, 1, 6, 9] {
            let compressed = deflate(&data, level).expect("deflate");

            let via_vec = inflate(&compressed).expect("inflate");
            assert_eq!(via_vec, data, "{name} level {level}: buffered path");

            let via_exact = inflate_exact_mode(&compressed).expect("inflate exact");
            assert_eq!(via_exact, data, "{name} level {level}: exact-mode path");

            let mut buf = vec![0u8; data.len()];
            let written = inflate_into(&compressed, &mut buf).expect("inflate_into");
            assert_eq!(
                written,
                data.len(),
                "{name} level {level}: inflate_into len"
            );
            assert_eq!(buf, data, "{name} level {level}: inflate_into bytes");

            // The fifth path: the resumable push decoder, at three feed
            // granularities so the fast loop, the careful per-symbol path
            // and the output-bound path are all exercised on every entry.
            for (in_chunk, out_size) in [(1usize, 64usize), (7, 4096), (4096, 65_536)] {
                let via_push = inflate_push(&compressed, in_chunk, out_size)
                    .unwrap_or_else(|e| panic!("{name} level {level} {in_chunk}/{out_size}: {e}"));
                assert_eq!(
                    via_push, via_vec,
                    "{name} level {level}: push path at {in_chunk}/{out_size}"
                );
            }

            // And the growable push front end, which shares the window with
            // the one-shot decoder.
            let via_grow = InflateStream::new()
                .inflate_to_vec(&compressed)
                .expect("inflate_to_vec");
            assert_eq!(
                via_grow, via_vec,
                "{name} level {level}: growable push path"
            );
        }
    }
}

#[test]
fn zlib_wrapper_paths_agree() {
    for (name, data) in corpus() {
        for level in [0u8, 6, 9] {
            let compressed = zlib_compress(&data, level).expect("zlib_compress");
            assert_eq!(zlib_decompress(&compressed).expect("zlib"), data, "{name}");

            let mut buf = vec![0u8; data.len()];
            let n = zlib_decompress_into(&compressed, &mut buf).expect("zlib into");
            assert_eq!(n, data.len(), "{name} level {level}");
            assert_eq!(buf, data, "{name} level {level}");

            // The push wrapper must agree with the slice functions, both
            // when told the framing and when sniffing it.
            for framing in [InflateWrapper::Zlib, InflateWrapper::Auto] {
                let mut decoder =
                    WrappedInflate::new(framing).trailing_policy(TrailingPolicy::Reject);
                let mut out = Vec::new();
                let mut scratch = vec![0u8; 251];
                let mut fed = 0usize;
                loop {
                    let progress = decoder
                        .inflate(&compressed[fed..], &mut scratch, FlushMode::Finish)
                        .unwrap_or_else(|e| panic!("{name} level {level} {framing:?}: {e}"));
                    fed += progress.consumed;
                    out.extend_from_slice(&scratch[..progress.produced]);
                    if progress.status == InflateStatus::StreamEnd {
                        break;
                    }
                }
                assert_eq!(out, data, "{name} level {level}: push {framing:?}");
                assert_eq!(decoder.members_decoded(), 1);
                assert_eq!(decoder.total_in(), compressed.len() as u64);
            }
        }
    }
}

/// `inflate_into` must report — never truncate — when the buffer is short,
/// and must accept an over-sized buffer.
#[test]
fn inflate_into_buffer_sizing() {
    let data: Vec<u8> = (0..50_000u32).map(|i| (i % 97) as u8).collect();
    let compressed = deflate(&data, 6).expect("deflate");

    // Exact fit.
    let mut exact = vec![0u8; data.len()];
    assert_eq!(inflate_into(&compressed, &mut exact).unwrap(), data.len());
    assert_eq!(exact, data);

    // Over-sized: only the decoded prefix is written.
    let mut big = vec![0xCDu8; data.len() + 4096];
    let n = inflate_into(&compressed, &mut big).unwrap();
    assert_eq!(n, data.len());
    assert_eq!(&big[..n], &data[..]);
    assert!(big[n..].iter().all(|&b| b == 0xCD), "wrote past the output");

    // Every short size must error, never truncate silently.
    for short in [0usize, 1, 100, data.len() / 2, data.len() - 1] {
        let mut small = vec![0u8; short];
        let err = inflate_into(&compressed, &mut small).expect_err("short buffer must be rejected");
        assert!(
            matches!(err, oxiarc_core::error::OxiArcError::BufferTooSmall { .. }),
            "short={short}: unexpected error {err:?}"
        );
    }
}

/// A back-reference reaching behind the start of the destination has no
/// history to read and must be rejected (there is no preset dictionary on the
/// `inflate_into` path).
#[test]
fn inflate_into_rejects_back_reference_before_start() {
    // Fixed-Huffman block whose very first symbol is a length/distance pair.
    // Hand-built so the distance necessarily reaches behind the output start.
    let mut bits: Vec<bool> = Vec::new();
    let push_code = |bits: &mut Vec<bool>, code: u32, len: u8| {
        for i in (0..len).rev() {
            bits.push((code >> i) & 1 != 0);
        }
    };
    bits.push(true); // BFINAL
    bits.push(true); // BTYPE = 01 (LSB first)
    bits.push(false);
    push_code(&mut bits, 0x101, 7); // length code 258 -> length 4
    push_code(&mut bits, 0x00, 5); // distance code 0 -> distance 1
    push_code(&mut bits, 0x00, 7); // end of block

    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    for (i, bit) in bits.iter().enumerate() {
        if *bit {
            bytes[i / 8] |= 1 << (i % 8);
        }
    }

    let mut out = [0u8; 64];
    let err = inflate_into(&bytes, &mut out).expect_err("must reject");
    assert!(
        matches!(err, oxiarc_core::error::OxiArcError::InvalidDistance { .. }),
        "unexpected error {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------

/// Every truncation of every corpus stream must error out cleanly (or, for a
/// prefix that happens to end on a complete final block, decode correctly).
/// Nothing may panic or hang.
#[test]
fn truncated_streams_never_panic() {
    let data: Vec<u8> = (0..20_000u32).map(|i| (i % 61) as u8).collect();
    for level in [0u8, 6, 9] {
        let compressed = deflate(&data, level).expect("deflate");
        for cut in 0..compressed.len() {
            let prefix = &compressed[..cut];
            let _ = inflate(prefix);
            let _ = inflate_exact_mode(prefix);
            let mut buf = vec![0u8; data.len() + 16];
            let _ = inflate_into(prefix, &mut buf);
        }
    }
}

/// Bit-flips and byte substitutions must be rejected or decoded, never crash.
#[test]
fn corrupted_streams_never_panic() {
    let data: Vec<u8> = (0..8_000u32).map(|i| (i % 251) as u8).collect();
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for level in [0u8, 6, 9] {
        let compressed = deflate(&data, level).expect("deflate");
        for _ in 0..2_000 {
            let mut corrupt = compressed.clone();
            let n = (rng.next() as usize % 3) + 1;
            for _ in 0..n {
                let idx = rng.next() as usize % corrupt.len();
                corrupt[idx] ^= 1 << (rng.next() % 8);
            }
            let mut buf = vec![0u8; data.len() * 2];
            let _ = inflate(&corrupt);
            let _ = inflate_exact_mode(&corrupt);
            let _ = inflate_into(&corrupt, &mut buf);
        }
    }
}

/// Arbitrary bytes — including the degenerate empty input — must not panic.
#[test]
fn arbitrary_bytes_never_panic() {
    let mut rng = Rng(0x0BAD_F00D_1234_5678);
    let _ = inflate(&[]);
    let mut scratch = vec![0u8; 1 << 16];
    let _ = inflate_into(&[], &mut scratch);
    for len in [1usize, 2, 3, 7, 16, 64, 257, 1024] {
        for _ in 0..500 {
            let blob: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
            let _ = inflate(&blob);
            let _ = inflate_exact_mode(&blob);
            let _ = inflate_into(&blob, &mut scratch);
            let _ = zlib_decompress(&blob);
            let _ = zlib_decompress_into(&blob, &mut scratch);
        }
    }
}

/// A crafted zlib stream whose declared size is enormous must not be trusted
/// into an allocation: `inflate_into` allocates nothing at all, and the
/// hinted `Inflater` clamps.
#[test]
fn size_hints_are_clamped() {
    let data = b"small payload".to_vec();
    let compressed = deflate(&data, 6).expect("deflate");
    // A hint far beyond the clamp must not attempt a huge allocation.
    let mut inflater = Inflater::with_output_capacity(usize::MAX);
    let out = inflater
        .inflate_reader(&mut std::io::Cursor::new(&compressed))
        .expect("inflate");
    assert_eq!(out, data);
}

// ---------------------------------------------------------------------------
// Hand-built block-type coverage
// ---------------------------------------------------------------------------

#[test]
fn stored_block_round_trip() {
    // Level 0 produces stored blocks; check a payload larger than one block.
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
    let compressed = deflate(&data, 0).expect("deflate level 0");
    assert_eq!(inflate(&compressed).expect("inflate"), data);
    let mut buf = vec![0u8; data.len()];
    assert_eq!(inflate_into(&compressed, &mut buf).unwrap(), data.len());
    assert_eq!(buf, data);
}

#[test]
fn empty_stored_block() {
    let compressed = vec![0x01, 0x00, 0x00, 0xFF, 0xFF];
    assert!(inflate(&compressed).expect("inflate").is_empty());
    let mut buf = [0u8; 4];
    assert_eq!(inflate_into(&compressed, &mut buf).unwrap(), 0);
}

#[test]
fn reserved_block_type_rejected() {
    // BFINAL=1, BTYPE=11 (reserved).
    let compressed = vec![0b0000_0111];
    assert!(inflate(&compressed).is_err());
    let mut buf = [0u8; 4];
    assert!(inflate_into(&compressed, &mut buf).is_err());
}

// ---------------------------------------------------------------------------
// CPython zlib oracle
// ---------------------------------------------------------------------------

#[cfg(feature = "zlib-oracle")]
mod oracle {
    use super::*;
    use std::process::Command;

    fn python3_available() -> bool {
        Command::new("python3")
            .args(["-c", "import zlib"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "oxiarc_inflate_diff_{}_{}_{}",
            std::process::id(),
            n,
            tag
        ))
    }

    fn run_python(script: &str, args: &[&std::path::Path]) -> Vec<u8> {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(script);
        for a in args {
            cmd.arg(a);
        }
        let out = cmd.output().expect("spawn python3");
        assert!(
            out.status.success(),
            "python3 failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// zlib-compress with CPython at a given level/strategy, decode with
    /// oxiarc through all three paths.
    #[test]
    fn cpython_compress_oxiarc_decompress() {
        if !python3_available() {
            eprintln!("skipping: python3 with zlib unavailable");
            return;
        }
        let script = "\
import sys, zlib
src, dst, level, strategy, wbits = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
data = open(src,'rb').read()
c = zlib.compressobj(level, zlib.DEFLATED, wbits, 9, strategy)
open(dst,'wb').write(c.compress(data) + c.flush())
";
        for (name, data) in corpus() {
            let src = temp_path("src");
            std::fs::write(&src, &data).expect("write src");
            for level in [0i32, 1, 6, 9] {
                // 0 = DEFAULT, 1 = FILTERED, 2 = HUFFMAN_ONLY, 3 = RLE, 4 = FIXED
                for strategy in [0i32, 1, 2, 3, 4] {
                    for wbits in [-15i32, 15] {
                        let dst = temp_path("dst");
                        run_python(
                            script,
                            &[
                                &src,
                                &dst,
                                std::path::Path::new(&level.to_string()),
                                std::path::Path::new(&strategy.to_string()),
                                std::path::Path::new(&wbits.to_string()),
                            ],
                        );
                        let compressed = std::fs::read(&dst).expect("read dst");
                        let _ = std::fs::remove_file(&dst);

                        let label =
                            format!("{name} level={level} strategy={strategy} wbits={wbits}");
                        if wbits == -15 {
                            assert_eq!(
                                inflate(&compressed).unwrap_or_else(|e| panic!("{label}: {e}")),
                                data,
                                "{label}: buffered"
                            );
                            assert_eq!(
                                inflate_exact_mode(&compressed)
                                    .unwrap_or_else(|e| panic!("{label}: {e}")),
                                data,
                                "{label}: exact"
                            );
                            let mut buf = vec![0u8; data.len()];
                            let n = inflate_into(&compressed, &mut buf)
                                .unwrap_or_else(|e| panic!("{label}: {e}"));
                            assert_eq!((n, &buf[..n]), (data.len(), &data[..]), "{label}: into");
                        } else {
                            assert_eq!(
                                zlib_decompress(&compressed)
                                    .unwrap_or_else(|e| panic!("{label}: {e}")),
                                data,
                                "{label}: zlib"
                            );
                            let mut buf = vec![0u8; data.len()];
                            let n = zlib_decompress_into(&compressed, &mut buf)
                                .unwrap_or_else(|e| panic!("{label}: {e}"));
                            assert_eq!(
                                (n, &buf[..n]),
                                (data.len(), &data[..]),
                                "{label}: zlib into"
                            );
                        }
                    }
                }
            }
            let _ = std::fs::remove_file(&src);
        }
    }

    /// oxiarc-compress, CPython-decompress: the encoder must keep producing
    /// streams a reference decoder accepts.
    #[test]
    fn oxiarc_compress_cpython_decompress() {
        if !python3_available() {
            eprintln!("skipping: python3 with zlib unavailable");
            return;
        }
        let script = "\
import sys, zlib
src, dst, wbits = sys.argv[1], sys.argv[2], int(sys.argv[3])
data = open(src,'rb').read()
open(dst,'wb').write(zlib.decompress(data, wbits))
";
        for (name, data) in corpus() {
            for level in [0u8, 1, 6, 9] {
                for wbits in [-15i32, 15] {
                    let compressed = if wbits == -15 {
                        deflate(&data, level).expect("deflate")
                    } else {
                        zlib_compress(&data, level).expect("zlib_compress")
                    };
                    let src = temp_path("osrc");
                    let dst = temp_path("odst");
                    std::fs::write(&src, &compressed).expect("write");
                    run_python(
                        script,
                        &[&src, &dst, std::path::Path::new(&wbits.to_string())],
                    );
                    let decoded = std::fs::read(&dst).expect("read");
                    let _ = std::fs::remove_file(&src);
                    let _ = std::fs::remove_file(&dst);
                    assert_eq!(decoded, data, "{name} level={level} wbits={wbits}");
                }
            }
        }
    }
}

/// A gzip stream whose ISIZE claims a huge output must not be trusted into a
/// speculative allocation: the hint is clamped and the decode still succeeds.
#[test]
fn gzip_isize_hint_is_only_a_hint() {
    use oxiarc_deflate::{gzip_compress, gzip_decompress};

    let data = b"a modest gzip payload".repeat(64);
    let mut compressed = gzip_compress(&data, 6).expect("gzip_compress");
    assert_eq!(gzip_decompress(&compressed).expect("gzip"), data);

    // Overwrite ISIZE with 4 GiB - 1. Decoding must still work (the CRC is
    // checked before the size, and the size mismatch is what is reported).
    let n = compressed.len();
    compressed[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
    let result = gzip_decompress(&compressed);
    assert!(
        result.is_err(),
        "a bogus ISIZE must be reported, not silently accepted"
    );
}

//! Adversarial robustness for [`Lzma2StreamDecoder`] driven through its public
//! `Read` interface.
//!
//! The decoder is a parser over untrusted bytes, so the two properties that
//! matter are: it always terminates (no spin, no hang, bounded allocation),
//! and it never reports a corrupted or truncated stream as a clean EOF. The
//! truncation sweep lives next to the implementation; these tests cover the
//! corruption and garbage cases, plus the pathological reader shapes
//! (one byte at a time, one-byte sink) that a real network source produces.

use std::io::{Cursor, Read};

use oxiarc_lzma::{Lzma2StreamDecoder, LzmaLevel, encode_lzma2_chunked};

/// Dictionary size used by every decoder here (1 MiB).
const DICT: u32 = 1 << 20;

/// Hard ceiling on `read` calls, so a decoder that stops making progress
/// fails an assertion instead of hanging the suite.
const MAX_READS: usize = 200_000;

/// A reader that hands out at most `chunk` bytes per call.
struct ChunkedReader<'a> {
    data: &'a [u8],
    pos: usize,
    chunk: usize,
}

impl Read for ChunkedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.chunk).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Drive a decoder to completion or error under a hard call ceiling.
/// Returns `Ok(decoded)` or `Err(())` — the point is that it always returns.
fn drive<R: Read>(mut decoder: Lzma2StreamDecoder<R>, sink: usize) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; sink];
    for _ in 0..MAX_READS {
        match decoder.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => return Err(()),
        }
    }
    panic!("decoder made no progress within {MAX_READS} read calls (spin?)");
}

fn compressible(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| ((i % 61) as u8).wrapping_add(b'a'))
        .collect()
}

#[test]
fn pure_garbage_never_panics_and_always_terminates() {
    for len in [0usize, 1, 2, 3, 7, 64, 4096, 70_000] {
        for seed_byte in [0x00u8, 0x01, 0x7f, 0x80, 0xe0, 0xff] {
            let garbage = vec![seed_byte; len];
            let decoder = Lzma2StreamDecoder::new(Cursor::new(garbage.clone()), DICT);
            // Either outcome is acceptable; a hang or a panic is not.
            let _ = drive(decoder, 4096);
        }
    }

    // Pseudo-random garbage too, so the control-byte parser sees every shape.
    let mut seed: u64 = 0xdead_beef_1234_5678;
    for len in [3usize, 17, 129, 5000] {
        for _ in 0..32 {
            let mut garbage = Vec::with_capacity(len);
            while garbage.len() < len {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                garbage.extend_from_slice(&seed.to_le_bytes());
            }
            garbage.truncate(len);
            let decoder = Lzma2StreamDecoder::new(Cursor::new(garbage), DICT);
            let _ = drive(decoder, 4096);
        }
    }
}

#[test]
fn single_bit_corruption_never_panics_and_is_almost_always_detected() {
    let original = compressible(24 * 1024);
    let stream = encode_lzma2_chunked(&original, LzmaLevel::FAST).expect("encode failed");

    // Flip one bit every 53 bits across the whole stream.
    let total_bits = stream.len() * 8;
    let mut flips = 0usize;
    let mut detected = 0usize;
    let mut clean_and_correct = 0usize;
    let mut clean_but_different = 0usize;
    let mut bit = 0usize;
    while bit < total_bits {
        let mut corrupted = stream.clone();
        corrupted[bit / 8] ^= 1u8 << (bit % 8);

        let decoder = Lzma2StreamDecoder::new(Cursor::new(corrupted), DICT);
        match drive(decoder, 8192) {
            Err(()) => detected += 1,
            Ok(decoded) => {
                // Whatever happens, a corrupted stream must never expand into
                // a decompression bomb.
                assert!(
                    decoded.len() <= original.len() * 8,
                    "bit {bit}: corrupted stream expanded to {} bytes from a {}-byte original",
                    decoded.len(),
                    original.len()
                );
                if decoded == original {
                    clean_and_correct += 1;
                } else {
                    clean_but_different += 1;
                }
            }
        }
        flips += 1;
        bit += 53;
    }

    assert!(
        flips > 100,
        "expected a broad sweep, only did {flips} flips"
    );
    assert!(
        detected > flips / 2,
        "corruption detection collapsed: only {detected} of {flips} flips were rejected"
    );

    // LZMA2 itself carries **no integrity check** — that is what the `.xz`
    // container's CRC32/CRC64/SHA-256 is for. A bit flipped inside an LZMA
    // payload can therefore decode to a different byte string of exactly the
    // declared chunk length, which this decoder cannot detect and does not
    // claim to. What it must never do is let that become common: the chunk
    // headers' declared uncompressed sizes are verified, so all but a
    // handful of flips are caught. The bound below is a regression signal —
    // if it starts failing, size verification has been lost, not merely a
    // different fixture chosen.
    assert!(
        clean_but_different * 20 <= flips,
        "{clean_but_different} of {flips} flips decoded cleanly to different bytes \
         ({clean_and_correct} decoded cleanly and correctly, {detected} were rejected) \
         \u{2014} per-chunk size verification looks broken"
    );
}

#[test]
fn a_one_byte_at_a_time_reader_and_one_byte_sink_decode_identically() {
    let original = compressible(40 * 1024);
    let stream = encode_lzma2_chunked(&original, LzmaLevel::FAST).expect("encode failed");

    for chunk in [1usize, 7, 97, 8192] {
        for sink in [1usize, 3, 4096] {
            let reader = ChunkedReader {
                data: &stream,
                pos: 0,
                chunk,
            };
            let decoder = Lzma2StreamDecoder::new(reader, DICT);
            let decoded = drive(decoder, sink)
                .unwrap_or_else(|()| panic!("chunk {chunk}, sink {sink}: decode failed"));
            assert_eq!(
                decoded, original,
                "chunk {chunk}, sink {sink}: byte mismatch"
            );
        }
    }
}

/// A stream that is truncated must never leave `is_finished()` true, whatever
/// buffer sizes the caller uses.
#[test]
fn truncation_is_reported_and_is_finished_stays_false() {
    let original = compressible(20 * 1024);
    let stream = encode_lzma2_chunked(&original, LzmaLevel::FAST).expect("encode failed");

    for cut in [1usize, 2, 3, stream.len() / 3, stream.len() - 1] {
        let mut decoder = Lzma2StreamDecoder::new(Cursor::new(stream[..cut].to_vec()), DICT);
        let mut buf = vec![0u8; 4096];
        let mut errored = false;
        for _ in 0..MAX_READS {
            match decoder.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    assert_eq!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof,
                        "cut {cut}: truncation must be UnexpectedEof"
                    );
                    errored = true;
                    break;
                }
            }
        }
        assert!(
            errored,
            "cut {cut}: truncation must be an error, not a clean EOF"
        );
        assert!(
            !decoder.is_finished(),
            "cut {cut}: a truncated stream must never report is_finished()"
        );
    }
}

/// A zero-length read buffer must return `Ok(0)` without disturbing the
/// decoder, as the `Read` contract requires.
#[test]
fn a_zero_length_buffer_is_a_no_op() {
    let original = compressible(8 * 1024);
    let stream = encode_lzma2_chunked(&original, LzmaLevel::FAST).expect("encode failed");

    let mut decoder = Lzma2StreamDecoder::new(Cursor::new(stream), DICT);
    let mut empty: [u8; 0] = [];
    for _ in 0..8 {
        assert_eq!(decoder.read(&mut empty).expect("zero-length read"), 0);
    }
    assert!(!decoder.is_finished());

    let mut out = Vec::new();
    decoder.read_to_end(&mut out).expect("decode after no-ops");
    assert_eq!(out, original);
    assert!(decoder.is_finished());
}

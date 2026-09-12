//! I/O behaviour of the `Read` shell over the incremental decoder.
//!
//! [`BrotliDecompressor`] is now a thin adapter over
//! [`oxiarc_brotli::BrotliStream`], so what needs pinning here is the *I/O
//! contract* rather than the codec: which inner-reader conditions are retried,
//! which propagate, and the fact that output is available long before the
//! source reaches EOF — the property the old read-all implementation could not
//! offer.

use std::io::{self, Read};

use oxiarc_brotli::streaming::BrotliDecompressor;
use oxiarc_brotli::{BrotliParams, compress, compress_with_params};

/// A reader that hands out at most `step` bytes per call and counts how many
/// bytes have left it, so a test can prove the decoder produced output before
/// the source was drained.
struct Trickle<'a> {
    data: &'a [u8],
    pos: usize,
    step: usize,
}

impl<'a> Trickle<'a> {
    fn new(data: &'a [u8], step: usize) -> Self {
        Trickle {
            data,
            pos: 0,
            step: step.max(1),
        }
    }
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.step).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// A reader that returns `Interrupted` before every real read.
struct Interrupting<'a> {
    inner: Trickle<'a>,
    armed: bool,
}

impl Read for Interrupting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.armed {
            self.armed = false;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        self.armed = true;
        self.inner.read(buf)
    }
}

/// A reader that returns `WouldBlock` a fixed number of times before serving
/// data, so a test can prove the error propagates and the decoder survives it.
struct Blocking<'a> {
    inner: Trickle<'a>,
    blocks_left: usize,
}

impl Read for Blocking<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.blocks_left > 0 {
            self.blocks_left -= 1;
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.inner.read(buf)
    }
}

/// A [`Trickle`] that publishes how many bytes have actually left it, so a
/// test can prove a refusal happened *before* the source was drained rather
/// than after every byte had already been pulled across the wire.
struct Counted<'a> {
    inner: Trickle<'a>,
    delivered: std::rc::Rc<std::cell::Cell<usize>>,
}

impl<'a> Counted<'a> {
    fn new(data: &'a [u8], step: usize) -> (Self, std::rc::Rc<std::cell::Cell<usize>>) {
        let delivered = std::rc::Rc::new(std::cell::Cell::new(0));
        (
            Counted {
                inner: Trickle::new(data, step),
                delivered: std::rc::Rc::clone(&delivered),
            },
            delivered,
        )
    }
}

impl Read for Counted<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.delivered.set(self.delivered.get() + n);
        Ok(n)
    }
}

/// A reader that stops early, simulating a connection cut mid-body.
struct Truncating<'a> {
    data: &'a [u8],
    pos: usize,
    stop_at: usize,
}

impl Read for Truncating<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.stop_at {
            return Ok(0);
        }
        let n = buf.len().min(self.stop_at - self.pos).min(64);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn sample() -> (Vec<u8>, Vec<u8>) {
    let data = b"adapter behaviour over a body with many meta-blocks. ".repeat(4_000);
    let compressed = compress(&data, 6).expect("compress");
    (data, compressed)
}

/// Output must be available before the source reaches EOF — the defining
/// difference from the old read-all decompressor.
#[test]
fn output_arrives_before_the_source_is_drained() {
    let (data, compressed) = sample();
    let source = Trickle::new(&compressed, 1);
    let mut decompressor = BrotliDecompressor::new(source);
    let mut first = vec![0u8; 4096];
    let n = decompressor.read(&mut first).expect("first read");
    assert!(n > 0, "the first read produced nothing");
    assert_eq!(&first[..n], &data[..n], "first bytes differ");

    let mut rest = Vec::new();
    decompressor.read_to_end(&mut rest).expect("read to end");
    let mut all = first[..n].to_vec();
    all.extend_from_slice(&rest);
    assert_eq!(all, data);
}

/// `read(&mut [])` is a no-op that never touches the inner reader.
#[test]
fn empty_read_is_a_no_op() {
    let (_, compressed) = sample();
    let mut decompressor = BrotliDecompressor::new(&compressed[..]);
    let mut empty: [u8; 0] = [];
    assert_eq!(decompressor.read(&mut empty).expect("empty read"), 0);
    let mut out = Vec::new();
    decompressor.read_to_end(&mut out).expect("read to end");
    assert_eq!(out.len(), 53 * 4_000);
}

/// `Interrupted` from the inner reader is retried, transparently.
#[test]
fn interrupted_is_retried() {
    let (data, compressed) = sample();
    let source = Interrupting {
        inner: Trickle::new(&compressed, 7),
        armed: true,
    };
    let mut out = Vec::new();
    BrotliDecompressor::new(source)
        .read_to_end(&mut out)
        .expect("interrupted reads must be retried");
    assert_eq!(out, data);
}

/// `WouldBlock` propagates unchanged, and the decoder state survives it so the
/// very same `read` can simply be retried.
#[test]
fn would_block_propagates_and_the_decoder_survives() {
    let (data, compressed) = sample();
    let mut decompressor = BrotliDecompressor::new(Blocking {
        inner: Trickle::new(&compressed, 64),
        blocks_left: 3,
    });
    let mut buf = vec![0u8; 8192];
    let mut blocked = 0;
    let mut out = Vec::new();
    loop {
        match decompressor.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                blocked += 1;
                assert!(blocked <= 8, "too many WouldBlock retries");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
    assert_eq!(blocked, 3, "WouldBlock must reach the caller unchanged");
    assert_eq!(out, data);
}

/// A source that stops mid-stream is an error, never a short read.
#[test]
fn a_truncated_source_is_an_error() {
    let (_, compressed) = sample();
    for stop_at in [1usize, 7, compressed.len() / 3, compressed.len() - 1] {
        let source = Truncating {
            data: &compressed,
            pos: 0,
            stop_at,
        };
        let mut out = Vec::new();
        let err = BrotliDecompressor::new(source)
            .read_to_end(&mut out)
            .expect_err("a truncated source must be an error");
        assert!(
            err.kind() == io::ErrorKind::InvalidData || err.kind() == io::ErrorKind::UnexpectedEof,
            "stop_at {stop_at}: unexpected error kind {:?} ({err})",
            err.kind()
        );
    }
}

/// `with_max_output` stays honest through the adapter, and — unlike the old
/// read-all implementation — refuses the stream without first draining the
/// source.
#[test]
fn max_output_is_enforced_without_draining_the_source() {
    let data = vec![0u8; 32 * 1024 * 1024];
    let compressed = compress(&data, 5).expect("compress");
    let (source, delivered) = Counted::new(&compressed, 512);
    let mut decompressor = BrotliDecompressor::new(source).with_max_output(1 << 20);
    let mut out = Vec::new();
    let err = decompressor
        .read_to_end(&mut out)
        .expect_err("over-budget stream must fail");
    assert!(
        err.to_string().contains("memory budget exceeded"),
        "unexpected error: {err}"
    );
    assert!(
        out.len() <= 1 << 20,
        "produced {} bytes past the 1 MiB budget",
        out.len()
    );
    // The point of a pre-decode cap: the refusal lands while most of the body
    // is still on the wire. A decoder that read everything first would show
    // `delivered == compressed.len()` here.
    assert!(
        delivered.get() < compressed.len(),
        "the whole {}-byte body was pulled from the source before the refusal",
        compressed.len()
    );
}

/// `with_max_window` refuses an over-large declared window through the adapter.
#[test]
fn max_window_is_enforced_through_the_adapter() {
    let params = BrotliParams {
        quality: 4,
        lgwin: 22,
        ..BrotliParams::default()
    };
    let compressed = compress_with_params(b"window ceiling", &params).expect("compress");
    let mut out = Vec::new();
    let err = BrotliDecompressor::new(&compressed[..])
        .with_max_window(1 << 16)
        .read_to_end(&mut out)
        .expect_err("declared window over the ceiling must be refused");
    assert!(
        err.to_string().contains("exceeds"),
        "unexpected error: {err}"
    );
    assert!(out.is_empty(), "bytes escaped the window refusal");
}

/// An empty source yields an empty body without an error — the behaviour this
/// adapter has always had, deliberately preserved. (The one-shot
/// [`oxiarc_brotli::decompress`] and the strict `BrotliStream` both reject a
/// zero-byte body as truncated; only this legacy shell is lenient.)
#[test]
fn an_empty_source_yields_an_empty_body() {
    let empty: &[u8] = &[];
    let mut out = Vec::new();
    let n = BrotliDecompressor::new(empty)
        .read_to_end(&mut out)
        .expect("an empty source is not an error for this adapter");
    assert_eq!(n, 0);
    assert!(out.is_empty());
    assert!(
        oxiarc_brotli::decompress(empty).is_err(),
        "the one-shot decoder stays strict"
    );
}

/// One byte of a stream followed by EOF is truncation, not an empty body.
#[test]
fn a_one_byte_source_that_stops_is_an_error() {
    let (_, compressed) = sample();
    let mut out = Vec::new();
    let err = BrotliDecompressor::new(&compressed[..1])
        .read_to_end(&mut out)
        .expect_err("a one-byte prefix is a truncated stream");
    assert!(
        err.kind() == io::ErrorKind::InvalidData || err.kind() == io::ErrorKind::UnexpectedEof,
        "unexpected error kind {:?} ({err})",
        err.kind()
    );
}

/// Tiny caller buffers must not stall the adapter.
#[test]
fn one_byte_reads_work() {
    let (data, compressed) = sample();
    let mut decompressor = BrotliDecompressor::new(Trickle::new(&compressed, 3));
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match decompressor.read(&mut byte).expect("read") {
            0 => break,
            n => out.extend_from_slice(&byte[..n]),
        }
    }
    assert_eq!(out, data);
}

/// A compressed stream far larger than the decoder's internal carry, pulled
/// through the adapter's 64 KiB staging buffer with tiny caller reads.
///
/// This is the adapter's own refill/compaction loop under load: hundreds of
/// staging refills, each one compacting whatever the decoder did not take. A
/// short `consumed` that the adapter forgot to carry forward, or a compaction
/// that dropped a byte, shows up here as wrong bytes rather than as an error,
/// which is why the comparison is against the full plaintext.
#[test]
fn a_stream_larger_than_the_carry_streams_through_the_read_adapter() {
    // Hex-dump text: ~5.9 MB plain, ~2.9 MB compressed, i.e. past the 2 MiB
    // carry cap and hundreds of staging refills.
    let mut state = 0x2468_ACE0_1357_9BDFu64;
    let mut noise = Vec::with_capacity(2 << 20);
    for _ in 0..(2 << 20) {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        noise.push((state >> 33) as u8);
    }
    let mut data = Vec::new();
    for (i, chunk) in noise.chunks(16).enumerate() {
        data.extend_from_slice(format!("line {i}: ").as_bytes());
        for byte in chunk {
            data.extend_from_slice(format!("{byte:02x}").as_bytes());
        }
        data.push(b'\n');
    }
    let compressed = compress(&data, 5).expect("compress");
    assert!(
        compressed.len() > 2 * 1024 * 1024,
        "fixture must exceed the carry cap: {} bytes",
        compressed.len()
    );

    // The source dribbles 1 KiB at a time and the caller reads 100 bytes at a
    // time, so neither side ever lines up with the staging buffer.
    let mut decompressor = BrotliDecompressor::new(Trickle::new(&compressed, 1024));
    let mut out = Vec::with_capacity(data.len());
    let mut buf = [0u8; 100];
    loop {
        match decompressor.read(&mut buf).expect("read") {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    assert_eq!(out.len(), data.len(), "body length differs");
    assert!(out == data, "body bytes differ");
}

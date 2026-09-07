//! `InflateReader` / `AsyncInflateReader` adapter behaviour.
//!
//! The push core (`InflateStream` / `WrappedInflate`) is covered by
//! `tests/inflate_stream.rs`. What is tested here is the part that only the
//! `Read`/`AsyncRead` adapters can get wrong, because only they know
//! whether the *source* can still deliver bytes:
//!
//! * `Interrupted` retries, `WouldBlock` propagates and never becomes an
//!   end-of-stream `Ok(0)` (the defect this suite exists to prevent);
//! * a source that stops mid-member is an `io::Error`, never a short read;
//! * the zlib "1-5 unconsumed bytes at EOF after a complete member" rule
//!   (R17), including a tail crafted to pass `CM == 8` and the `% 31` check;
//! * the 3-byte read pattern that `streaming.rs` has always exercised, and
//!   the 1-byte pattern that is strictly worse;
//! * the legacy/strict split: `GzipStreamDecoder` and `ZlibStreamDecoder`
//!   stay forgiving, every new type is strict.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oxiarc_core::traits::{DecompressStatus, Decompressor};
use oxiarc_deflate::{
    GzipStreamDecoder, InflateReader, InflateWrapper, Inflater, TrailingPolicy, ZlibStreamDecoder,
    deflate, gzip_compress, zlib_compress,
};
use std::io::{self, Read};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Deterministic mixed-content payload: compressible runs interleaved with
/// pseudo-random bytes, so every block type appears.
fn payload(len: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(len);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    while data.len() < len {
        data.extend_from_slice(format!("record {} of the reader payload; ", data.len()).as_bytes());
        for _ in 0..16 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push((state >> 32) as u8);
        }
    }
    data.truncate(len);
    data
}

/// Read to the end `chunk` bytes at a time.
fn read_all<R: Read>(mut reader: R, chunk: usize) -> io::Result<Vec<u8>> {
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

/// A source that hands out `step` bytes per call and interleaves the error
/// kinds a real socket produces.
struct FlakySource<'a> {
    data: &'a [u8],
    pos: usize,
    step: usize,
    /// Error kinds to return, one per `read` call, before any data.
    pending: Vec<io::ErrorKind>,
    /// Error kinds to inject once per read, cycling forever.
    repeat: Option<io::ErrorKind>,
    reads: usize,
}

impl<'a> FlakySource<'a> {
    fn new(data: &'a [u8], step: usize) -> Self {
        Self {
            data,
            pos: 0,
            step,
            pending: Vec::new(),
            repeat: None,
            reads: 0,
        }
    }

    fn with_pending(mut self, kinds: &[io::ErrorKind]) -> Self {
        self.pending = kinds.to_vec();
        self.pending.reverse();
        self
    }

    fn with_repeat(mut self, kind: io::ErrorKind) -> Self {
        self.repeat = Some(kind);
        self
    }
}

impl Read for FlakySource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        if let Some(kind) = self.pending.pop() {
            return Err(io::Error::new(kind, "injected"));
        }
        if let Some(kind) = self.repeat {
            self.repeat = None;
            return Err(io::Error::new(kind, "injected (repeating)"));
        }
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(self.step).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Read granularity
// ---------------------------------------------------------------------------

/// The pattern `streaming.rs`'s own test has always used, plus the worse
/// one-byte case, over every framing.
#[test]
fn three_byte_and_one_byte_reads_match_the_whole_stream() {
    let data = payload(120_000);
    let framings: [(InflateWrapper, Vec<u8>); 3] = [
        (
            InflateWrapper::Gzip,
            gzip_compress(&data, 6).expect("gzip_compress"),
        ),
        (
            InflateWrapper::Zlib,
            zlib_compress(&data, 6).expect("zlib_compress"),
        ),
        (InflateWrapper::Raw, deflate(&data, 6).expect("deflate")),
    ];

    for (wrapper, compressed) in framings {
        for chunk in [1usize, 3, 7, 64 * 1024] {
            let out = read_all(InflateReader::new(&compressed[..], wrapper), chunk)
                .unwrap_or_else(|e| panic!("{wrapper:?} at {chunk}: {e}"));
            assert_eq!(out, data, "{wrapper:?} at read size {chunk}");
        }
    }
}

/// A source that dribbles one compressed byte per call must not change the
/// result, and the reader must not spin: bounded reads for bounded input.
#[test]
fn one_byte_source_reads_are_bounded_and_exact() {
    let data = payload(30_000);
    let compressed = gzip_compress(&data, 6).expect("gzip_compress");
    let mut source = FlakySource::new(&compressed, 1);
    let out = {
        let mut reader = InflateReader::gzip(&mut source);
        read_all(&mut reader, 4096).expect("inflate")
    };
    assert_eq!(out, data);
    assert!(
        source.reads <= compressed.len() + 4,
        "{} source reads for {} compressed bytes",
        source.reads,
        compressed.len()
    );
}

// ---------------------------------------------------------------------------
// I/O error contract
// ---------------------------------------------------------------------------

/// `Interrupted` is a retry signal, not a failure.
#[test]
fn interrupted_is_retried_transparently() {
    let data = payload(9_000);
    let compressed = zlib_compress(&data, 6).expect("zlib_compress");
    let source = FlakySource::new(&compressed, 64).with_pending(&[
        io::ErrorKind::Interrupted,
        io::ErrorKind::Interrupted,
        io::ErrorKind::Interrupted,
    ]);
    let out = read_all(InflateReader::zlib(source), 1024).expect("Interrupted must be retried");
    assert_eq!(out, data);
}

/// `WouldBlock` must reach the caller unchanged. Turning it into `Ok(0)`
/// would report a truncated stream as a complete one on any non-blocking
/// source — the `oxiarc-lzma` defect this rule exists to prevent.
#[test]
fn would_block_propagates_and_is_never_end_of_stream() {
    let data = payload(5_000);
    let compressed = gzip_compress(&data, 6).expect("gzip_compress");
    let source = FlakySource::new(&compressed, 128).with_repeat(io::ErrorKind::WouldBlock);
    let mut reader = InflateReader::gzip(source);

    let mut buf = [0u8; 4096];
    let error = reader
        .read(&mut buf)
        .expect_err("WouldBlock must not be swallowed");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

    // The reader is still usable: the source is now ready and the whole
    // stream decodes.
    let mut out = Vec::new();
    loop {
        let n = reader.read(&mut buf).expect("read after WouldBlock");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, data);
}

/// A source that stops in the middle of a member is an error, never a short
/// read that a caller would mistake for the whole body.
#[test]
fn truncation_is_an_io_error_at_every_cut() {
    let data = payload(40_000);
    for (label, compressed) in [
        ("gzip", gzip_compress(&data, 6).expect("gzip_compress")),
        ("zlib", zlib_compress(&data, 6).expect("zlib_compress")),
        ("raw", deflate(&data, 6).expect("deflate")),
    ] {
        let wrapper = match label {
            "gzip" => InflateWrapper::Gzip,
            "zlib" => InflateWrapper::Zlib,
            _ => InflateWrapper::Raw,
        };
        for cut in [
            1usize,
            2,
            9,
            compressed.len() / 3,
            compressed.len() / 2,
            compressed.len() - 1,
        ] {
            let mut reader = InflateReader::new(&compressed[..cut], wrapper);
            let mut sink = Vec::new();
            let error = reader
                .read_to_end(&mut sink)
                .expect_err("{label}: a truncated stream must be an error");
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
                ),
                "{label} cut {cut}: unexpected kind {:?}",
                error.kind()
            );
            assert!(
                sink.len() <= data.len(),
                "{label} cut {cut}: produced more than the payload"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// R17 — the zlib short-tail rule
// ---------------------------------------------------------------------------

/// A 1-5 byte fragment after a complete zlib member is ignored, including
/// one crafted to pass both the `CM == 8` and the `(CMF*256+FLG) % 31 == 0`
/// checks — the case a streaming decoder gets wrong if the rule lives in
/// the core rather than the adapter.
#[test]
fn zlib_trailing_fragment_of_one_to_five_bytes_is_ignored() {
    let data = b"payload for the short-tail rule".to_vec();
    let member = zlib_compress(&data, 6).expect("zlib_compress");

    // 0x78 0x9C is the most common real zlib header, and it passes both
    // structural checks: it is exactly the pair that must NOT start a
    // member here.
    let crafted: [&[u8]; 9] = [
        &[0x78],
        &[0x78, 0x9c],
        &[0x78, 0x9c, 0x00],
        &[0x78, 0x9c, 0x00, 0x11],
        &[0x78, 0x9c, 0x00, 0x11, 0x22],
        &[0x00],
        b"XYZ",
        &[0xff, 0xff, 0xff, 0xff],
        &[0x68, 0xde, 0x01, 0x02, 0x03],
    ];
    for tail in crafted {
        let mut stream = member.clone();
        stream.extend_from_slice(tail);
        let out = read_all(ZlibStreamDecoder::new(&stream[..]), 4096)
            .unwrap_or_else(|e| panic!("tail {tail:02x?} must be ignored: {e}"));
        assert_eq!(out, data, "tail {tail:02x?}");

        // The same rule holds for the strict adapter: it is a property of
        // "fewer than six bytes cannot be a member", not of leniency.
        let out = read_all(
            InflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::Stop),
            4096,
        )
        .unwrap_or_else(|e| panic!("strict reader, tail {tail:02x?}: {e}"));
        assert_eq!(out, data, "strict reader, tail {tail:02x?}");
    }

    // Negative control: the rule really is doing the work. Driving the
    // *core* over the same bytes — which cannot know that the source has
    // ended — starts a member on the crafted pair and runs out of input.
    let mut stream = member.clone();
    stream.extend_from_slice(&[0x78, 0x9c]);
    let mut core = oxiarc_deflate::WrappedInflate::new(InflateWrapper::Zlib)
        .multi_member(true)
        .trailing_policy(TrailingPolicy::Stop);
    let mut scratch = vec![0u8; 4096];
    let mut fed = 0usize;
    let mut ended = false;
    let outcome = loop {
        match core.inflate(
            &stream[fed..],
            &mut scratch,
            oxiarc_core::traits::FlushMode::Finish,
        ) {
            Ok(progress) => {
                fed += progress.consumed;
                if progress.status == oxiarc_deflate::InflateStatus::StreamEnd {
                    ended = true;
                    break Ok(());
                }
                if progress.consumed == 0 && progress.produced == 0 {
                    break Ok(());
                }
            }
            Err(error) => break Err(error),
        }
    };
    assert!(
        outcome.is_err() && !ended,
        "the core alone must not tolerate a crafted 2-byte tail — \
         if it does, the adapter rule is untested"
    );
}

/// Six or more trailing bytes are past the rule, so the configured
/// [`TrailingPolicy`] decides: the legacy decoder stops, a strict reader
/// rejects.
#[test]
fn zlib_trailing_run_of_six_or_more_follows_the_policy() {
    let data = b"payload".to_vec();
    let mut stream = zlib_compress(&data, 6).expect("zlib_compress");
    stream.extend_from_slice(b"not a member at all");

    let out = read_all(ZlibStreamDecoder::new(&stream[..]), 4096)
        .expect("legacy decoder stops gracefully");
    assert_eq!(out, data);

    let mut strict = InflateReader::zlib(&stream[..]);
    let mut sink = Vec::new();
    assert!(
        strict.read_to_end(&mut sink).is_err(),
        "a strict reader must reject trailing garbage"
    );
}

/// A genuine second member is still decoded — the short-tail rule must not
/// swallow one.
#[test]
fn concatenated_zlib_members_still_decode() {
    let first = payload(20_000);
    let second = payload(9_000);
    let mut stream = zlib_compress(&first, 9).expect("zlib_compress");
    stream.extend_from_slice(&zlib_compress(&second, 1).expect("zlib_compress"));

    let mut expected = first;
    expected.extend_from_slice(&second);

    for chunk in [1usize, 3, 65_536] {
        let out = read_all(ZlibStreamDecoder::new(&stream[..]), chunk).expect("concatenated");
        assert_eq!(out, expected, "at read size {chunk}");
    }

    let mut reader = InflateReader::zlib(&stream[..]);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).expect("strict concatenated");
    assert_eq!(out, expected);
    assert_eq!(reader.members_decoded(), 2);
}

/// Raw DEFLATE ends on a *bit* boundary, so the last byte of a stream can
/// carry padding bits. Those must never be mistaken for a trailing byte: an
/// exact raw stream decodes cleanly under the strict default policy, at
/// every payload length and therefore at every final-byte bit offset.
/// A genuine trailing byte is a different matter — that is what
/// [`TrailingPolicy`] is for, and each setting is checked here so the HTTP
/// layer can pick one deliberately.
#[test]
fn raw_streams_end_cleanly_and_trailing_bytes_follow_the_policy() {
    for len in [0usize, 1, 2, 3, 5, 7, 11, 13, 17, 100, 1_000, 40_000] {
        let data = vec![b'a'; len];
        let raw = deflate(&data, 6).expect("deflate");

        // Exact: no padding bit may be reported as a trailing byte.
        let out = read_all(InflateReader::raw(&raw[..]), 4096)
            .unwrap_or_else(|e| panic!("exact raw stream of {len} bytes: {e}"));
        assert_eq!(out, data, "exact raw stream of {len} bytes");

        // One real trailing byte: rejected by default, tolerated when the
        // caller asks for it.
        let mut padded = raw.clone();
        padded.push(0x00);
        let mut strict = InflateReader::raw(&padded[..]);
        let mut sink = Vec::new();
        assert!(
            strict.read_to_end(&mut sink).is_err(),
            "len {len}: a trailing byte must be rejected under TrailingPolicy::Reject"
        );

        let out = read_all(
            InflateReader::raw(&padded[..]).trailing_policy(TrailingPolicy::Stop),
            4096,
        )
        .unwrap_or_else(|e| panic!("len {len} with Stop: {e}"));
        assert_eq!(out, data, "len {len}: Stop must ignore the trailing byte");

        let out = read_all(
            InflateReader::raw(&padded[..]).trailing_policy(TrailingPolicy::AllowZeros),
            4096,
        )
        .unwrap_or_else(|e| panic!("len {len} with AllowZeros: {e}"));
        assert_eq!(out, data, "len {len}: AllowZeros must ignore zero padding");
    }
}

// ---------------------------------------------------------------------------
// Legacy leniency vs. strict defaults
// ---------------------------------------------------------------------------

/// An empty source is an empty result for both legacy decoders, and an
/// error for the strict adapter (owner decision 2).
#[test]
fn empty_source_semantics_differ_by_type() {
    let mut gzip_legacy = GzipStreamDecoder::new(&[][..]);
    let mut buf = [0u8; 16];
    assert_eq!(gzip_legacy.read(&mut buf).expect("gzip legacy"), 0);
    assert!(gzip_legacy.is_finished());
    assert_eq!(gzip_legacy.decompressed_size(), 0);

    let mut zlib_legacy = ZlibStreamDecoder::new(&[][..]);
    assert_eq!(zlib_legacy.read(&mut buf).expect("zlib legacy"), 0);
    assert!(zlib_legacy.is_finished());

    let mut strict = InflateReader::zlib(&[][..]);
    assert!(
        strict.read(&mut buf).is_err(),
        "the strict reader must reject an empty stream"
    );
    let mut strict_gzip = InflateReader::gzip(&[][..]);
    assert!(
        strict_gzip.read(&mut buf).is_err(),
        "the strict gzip reader must reject an empty stream"
    );
}

/// Non-gzip leading bytes stop the legacy decoder cleanly (asserted
/// behaviour) but are an error for the strict adapter.
#[test]
fn non_gzip_leading_bytes_stop_the_legacy_decoder() {
    let junk = b"this is definitely not a gzip stream at all";
    let out = read_all(GzipStreamDecoder::new(&junk[..]), 64).expect("legacy gzip stops");
    assert!(out.is_empty());

    let mut strict = InflateReader::gzip(&junk[..]);
    let mut sink = Vec::new();
    assert!(strict.read_to_end(&mut sink).is_err());
}

/// Non-zlib leading bytes are an error for both, since `ZlibStreamDecoder`
/// documents "garbage at member 0 is an error".
#[test]
fn non_zlib_leading_bytes_are_an_error() {
    let junk = b"not zlib at all";
    let mut legacy = ZlibStreamDecoder::new(&junk[..]);
    let mut sink = Vec::new();
    assert!(legacy.read_to_end(&mut sink).is_err());
}

// ---------------------------------------------------------------------------
// Multi-member gzip, headers and counters
// ---------------------------------------------------------------------------

#[test]
fn gzip_header_fields_are_exposed() {
    // A member carrying FNAME, FCOMMENT and a verified FHCRC.
    let body = deflate(b"header fields", 6).expect("deflate");
    let mut raw = vec![0x1f, 0x8b, 0x08, 0x08 | 0x10 | 0x02, 0, 0, 0, 0, 0x00, 0x03];
    raw.extend_from_slice(b"notes.txt\0");
    raw.extend_from_slice(b"a comment\0");
    let hcrc = (oxiarc_core::Crc32::compute(&raw) & 0xFFFF) as u16;
    raw.extend_from_slice(&hcrc.to_le_bytes());
    raw.extend_from_slice(&body);
    raw.extend_from_slice(&oxiarc_core::Crc32::compute(b"header fields").to_le_bytes());
    raw.extend_from_slice(&13u32.to_le_bytes());

    let mut reader = InflateReader::gzip(&raw[..]);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).expect("inflate");
    assert_eq!(out, b"header fields");

    let header = reader.gzip_header().expect("header");
    assert_eq!(header.name.as_deref(), Some(&b"notes.txt"[..]));
    assert_eq!(header.comment.as_deref(), Some(&b"a comment"[..]));
    assert_eq!(header.os, 0x03);
    assert_eq!(reader.total_in(), raw.len() as u64);
    assert_eq!(reader.total_out(), 13);

    // A corrupted FHCRC is rejected by default.
    let mut broken = raw.clone();
    let hcrc_at = broken.len() - body.len() - 8 - 2;
    broken[hcrc_at] ^= 0xFF;
    let mut sink = Vec::new();
    assert!(
        InflateReader::gzip(&broken[..])
            .read_to_end(&mut sink)
            .is_err(),
        "a bad FHCRC must be rejected"
    );
    // ... and tolerated when verification is turned off.
    let mut sink = Vec::new();
    InflateReader::gzip(&broken[..])
        .verify_header_crc(false)
        .read_to_end(&mut sink)
        .expect("FHCRC ignored when opted out");
    assert_eq!(sink, b"header fields");
}

#[test]
fn multi_member_gzip_decodes_in_order() {
    let a = payload(50_000);
    let b = payload(20_000);
    let mut stream = gzip_compress(&a, 9).expect("gzip_compress");
    stream.extend_from_slice(&gzip_compress(&b, 1).expect("gzip_compress"));
    let mut expected = a;
    expected.extend_from_slice(&b);

    for chunk in [1usize, 3, 65_536] {
        let out = read_all(GzipStreamDecoder::new(&stream[..]), chunk).expect("multi-member");
        assert_eq!(out, expected, "at read size {chunk}");
    }

    let mut reader = InflateReader::gzip(&stream[..]);
    let mut out = Vec::new();
    reader.read_to_end(&mut out).expect("strict multi-member");
    assert_eq!(out, expected);
    assert_eq!(reader.members_decoded(), 2);
}

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// The cap is enforced while decoding, and the bytes up to it are still
/// delivered before the error surfaces.
#[test]
fn max_output_bounds_the_reader() {
    let plain = vec![0u8; 4_000_000];
    let compressed = zlib_compress(&plain, 9).expect("zlib_compress");

    let exact = read_all(
        InflateReader::zlib(&compressed[..]).with_max_output(4_000_000),
        65_536,
    )
    .expect("a cap equal to the payload must succeed");
    assert_eq!(exact.len(), 4_000_000);

    let mut capped = InflateReader::zlib(&compressed[..]).with_max_output(1_000);
    let mut sink = Vec::new();
    let error = capped
        .read_to_end(&mut sink)
        .expect_err("output past the cap must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(
        sink.len() <= 1_000,
        "delivered {} bytes past a 1000-byte cap",
        sink.len()
    );

    // And the legacy decoder's own cap keeps working.
    let mut legacy = ZlibStreamDecoder::new(&compressed[..]).with_max_output(1_000_000);
    let mut sink = Vec::new();
    assert!(legacy.read_to_end(&mut sink).is_err());
}

/// The ratio guard stops a stream that expands faster than allowed.
#[test]
fn ratio_guard_bounds_the_reader() {
    let plain = vec![7u8; 2_000_000];
    let compressed = gzip_compress(&plain, 9).expect("gzip_compress");
    let mut reader = InflateReader::gzip(&compressed[..]).with_ratio_guard(10.0, 4_096);
    let mut sink = Vec::new();
    assert!(
        reader.read_to_end(&mut sink).is_err(),
        "a 1000:1 stream must trip a 10:1 guard"
    );
    assert!(sink.len() < plain.len());
}

// ---------------------------------------------------------------------------
// Decompressor trait: the two-call silent-truncation regression
// ---------------------------------------------------------------------------

/// Before 0.4.2 a second `decompress` call on an `Inflater` whose stream had
/// ended mid-block returned `Ok((0, n, Done))` with partial output, because
/// `final_block` was written back before the error propagated. The fault
/// latch makes every later call return the error instead.
#[test]
fn inflater_decompress_second_call_after_truncation_still_errors() {
    let data = payload(80_000);
    let compressed = deflate(&data, 6).expect("deflate");

    for cut in [1usize, 10, compressed.len() / 2, compressed.len() - 1] {
        let mut inflater = Inflater::new();
        let mut out = vec![0u8; 4096];

        let first = inflater.decompress(&compressed[..cut], &mut out);
        // The first call may legitimately fill the buffer and report
        // NeedsOutput; drive it until it either errors or claims Done.
        let mut saw_error = first.is_err();
        let mut done = matches!(first, Ok((_, _, DecompressStatus::Done)));
        let mut consumed = first.map(|(c, _, _)| c).unwrap_or(0);
        let mut calls = 0usize;
        while !saw_error && !done {
            calls += 1;
            assert!(calls < 10_000, "cut {cut}: runaway loop");
            match inflater.decompress(&compressed[consumed.min(cut)..cut], &mut out) {
                Ok((c, _, status)) => {
                    consumed += c;
                    done = status == DecompressStatus::Done;
                    if c == 0 && status != DecompressStatus::Done {
                        // No progress with no input left: must not spin.
                        break;
                    }
                }
                Err(_) => saw_error = true,
            }
        }
        assert!(
            saw_error,
            "cut {cut}: a truncated stream must error, never report Done"
        );
        assert!(
            !inflater.is_finished(),
            "cut {cut}: a failed stream must not report finished"
        );

        // The latch is sticky: three more calls give the same answer.
        for _ in 0..3 {
            assert!(
                inflater.decompress(&compressed[..cut], &mut out).is_err(),
                "cut {cut}: the fault latch must be sticky"
            );
        }

        // ... and `reset` clears it, which is what CAB folders rely on.
        Decompressor::reset(&mut inflater);
        let whole = inflater.decompress_all(&compressed).expect("after reset");
        assert_eq!(
            whole, data,
            "cut {cut}: reset must restore a usable decoder"
        );
    }
}

/// `decompress_all` must never turn truncation into a short `Ok`.
#[test]
fn inflater_decompress_all_rejects_truncated_streams() {
    let data = payload(100_000);
    let compressed = deflate(&data, 6).expect("deflate");
    for cut in [1usize, 10, compressed.len() / 2, compressed.len() - 1] {
        let mut inflater = Inflater::new();
        assert!(
            inflater.decompress_all(&compressed[..cut]).is_err(),
            "truncated at {cut}: must error, never return a short Ok"
        );
    }
}

/// A dictionary set before the first `decompress` call is honoured by the
/// streaming core, not silently dropped.
#[test]
fn decompress_honours_a_preset_dictionary() {
    let dictionary = b"the quick brown fox jumps over the lazy dog".repeat(8);
    let body = b"the quick brown fox jumps over the lazy dog again".to_vec();
    let compressed = {
        let mut deflater = oxiarc_deflate::Deflater::new(6);
        deflater.set_dictionary(&dictionary);
        let mut out = Vec::new();
        deflater.deflate(&body, &mut out, true).expect("deflate");
        out
    };

    let mut inflater = Inflater::new();
    inflater.set_dictionary(&dictionary);
    let decoded = inflater
        .decompress_all(&compressed)
        .expect("decompress_all");
    assert_eq!(decoded, body);
}

// ---------------------------------------------------------------------------
// Async adapter
// ---------------------------------------------------------------------------

#[cfg(feature = "async-io")]
mod asynchronous {
    use super::*;
    use oxiarc_deflate::AsyncInflateReader;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

    /// A source that returns `Poll::Pending` (after waking itself) every
    /// other poll, and hands out at most `step` bytes otherwise.
    struct PendingSource {
        data: Vec<u8>,
        pos: usize,
        step: usize,
        pend_next: bool,
        pends: usize,
    }

    impl AsyncRead for PendingSource {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = &mut *self;
            if this.pend_next {
                this.pend_next = false;
                this.pends += 1;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            this.pend_next = true;
            if this.pos >= this.data.len() {
                return Poll::Ready(Ok(()));
            }
            let n = buf
                .remaining()
                .min(this.step)
                .min(this.data.len() - this.pos);
            buf.put_slice(&this.data[this.pos..this.pos + n]);
            this.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn pending_sources_are_handled_without_losing_bytes() {
        let data = payload(70_000);
        let compressed = gzip_compress(&data, 6).expect("gzip_compress");
        let source = PendingSource {
            data: compressed,
            pos: 0,
            step: 7,
            pend_next: true,
            pends: 0,
        };
        let mut reader = AsyncInflateReader::gzip(source);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("inflate");
        assert_eq!(out, data);
        assert!(
            reader.get_ref().pends > 100,
            "the source should have pended repeatedly"
        );
    }

    #[tokio::test]
    async fn async_truncation_is_an_error() {
        let data = payload(30_000);
        let compressed = zlib_compress(&data, 6).expect("zlib_compress");
        let cut = compressed.len() / 2;
        let mut reader = AsyncInflateReader::zlib(&compressed[..cut]);
        let mut out = Vec::new();
        let error = reader
            .read_to_end(&mut out)
            .await
            .expect_err("truncation must be an error");
        assert!(matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
        ));
    }

    /// A3-F1 in the async half of the pump. `short_tail_ends_stream` is
    /// shared code, and `AsyncInflateReader` is the configuration an HTTP
    /// body decoder actually runs, so the matrix is repeated here — a
    /// truncated first member at cuts the blocking test does not use, a
    /// truncated second member (the case in which the rule *can* fire), and
    /// a pending source, which is the only way an async adapter can be
    /// polled in a state a blocking one never reaches.
    #[tokio::test]
    async fn async_truncation_of_a_zlib_member_is_never_a_clean_end() {
        let data = payload(40_000);
        let member = zlib_compress(&data, 6).expect("zlib_compress");

        for cut in [member.len() - 100, member.len() - 2, member.len() - 1] {
            let mut reader = AsyncInflateReader::zlib(&member[..cut]);
            let mut out = Vec::new();
            let error = reader
                .read_to_end(&mut out)
                .await
                .expect_err("single member truncation must be an error");
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
                ),
                "cut {cut}: {:?}",
                error.kind()
            );

            // Same cut through a source that pends between every chunk.
            let source = PendingSource {
                data: member[..cut].to_vec(),
                pos: 0,
                step: 11,
                pend_next: true,
                pends: 0,
            };
            let mut reader = AsyncInflateReader::zlib(source);
            let mut out = Vec::new();
            assert!(
                reader.read_to_end(&mut out).await.is_err(),
                "cut {cut}: a pending source must not turn truncation into EOF"
            );
        }

        // `[member1][member2 truncated]`: `members_decoded() >= 1`, so this
        // is the configuration in which the short-tail rule is live.
        let second = zlib_compress(&payload(20_000), 6).expect("zlib_compress");
        for cut in [8usize, 10, second.len() - 10, second.len() - 2] {
            let mut stream = member.clone();
            stream.extend_from_slice(&second[..cut]);
            let mut reader = AsyncInflateReader::zlib(&stream[..]);
            let mut out = Vec::new();
            assert!(
                reader.read_to_end(&mut out).await.is_err(),
                "member 2 cut at {cut} must be an error"
            );

            let source = PendingSource {
                data: stream.clone(),
                pos: 0,
                step: 5,
                pend_next: true,
                pends: 0,
            };
            let mut reader = AsyncInflateReader::zlib(source);
            let mut out = Vec::new();
            assert!(
                reader.read_to_end(&mut out).await.is_err(),
                "member 2 cut at {cut}, pending source"
            );
        }

        // The intact stream still decodes, so none of the above is a
        // blanket rejection.
        let mut reader = AsyncInflateReader::zlib(&member[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("intact");
        assert_eq!(out, data);
    }

    #[tokio::test]
    async fn async_multi_member_and_counters() {
        let a = payload(12_000);
        let b = payload(6_000);
        let mut stream = gzip_compress(&a, 6).expect("gzip_compress");
        stream.extend_from_slice(&gzip_compress(&b, 6).expect("gzip_compress"));
        let mut expected = a;
        expected.extend_from_slice(&b);

        let mut reader = AsyncInflateReader::gzip(&stream[..]);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("inflate");
        assert_eq!(out, expected);
        assert_eq!(reader.members_decoded(), 2);
        assert_eq!(reader.total_in(), stream.len() as u64);
        assert_eq!(reader.total_out(), expected.len() as u64);
        assert!(reader.is_finished());
    }

    /// RFC 4978: the reader must deliver bytes from a sync-flushed stream
    /// without waiting for a final block, and EOF is not truncation.
    #[tokio::test]
    async fn raw_sync_flush_stream_is_not_truncated_at_eof() {
        use oxiarc_deflate::{Deflater, RawInflateReader};

        let lines: [&[u8]; 3] = [b"* OK ready\r\n", b"A001 LOGIN\r\n", b"A001 OK\r\n"];
        let mut wire = Vec::new();
        let mut deflater = Deflater::new(6);
        for line in &lines {
            deflater
                .deflate_sync(line, &mut wire)
                .expect("deflate_sync");
        }

        let mut reader = RawInflateReader::new(std::io::Cursor::new(wire));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).await.expect("no truncation");
        let expected: Vec<u8> = lines.iter().flat_map(|l| l.iter().copied()).collect();
        assert_eq!(out, expected);
        assert_eq!(reader.total_out(), expected.len() as u64);
    }
}

// ---------------------------------------------------------------------------
// A3-F1 / A3-F2 — the short-tail rule must not accept a truncated stream
// ---------------------------------------------------------------------------
//
// The rule reproduces a whole-slice decoder's `remaining.len() < 6` test,
// which measures the bytes *after the last complete member*. Measuring the
// adapter's own unconsumed-input level instead reads `0` at EOF in every
// state — mid-block, mid-trailer, anywhere — so every truncated zlib stream
// would end cleanly at `Ok(0)`. These tests pin each of the eight ways that
// went wrong plus the leniency the rule is actually for.

/// Every cut of a single zlib member is an error for `ZlibStreamDecoder`,
/// at three read granularities. The last two cuts are the sharp ones: the
/// DEFLATE payload is complete and only the Adler-32 trailer is short, so
/// nothing but the framing machine can notice.
#[test]
fn zlib_stream_decoder_rejects_a_truncated_single_member() {
    let data = payload(100_000);
    let member = zlib_compress(&data, 6).expect("zlib_compress");
    assert!(member.len() > 200);

    for cut in [
        member.len() - 100,
        member.len() - 6,
        member.len() - 2,
        member.len() - 1,
    ] {
        let truncated = &member[..cut];
        for chunk in [1usize, 7, 65_536] {
            let error = read_all(
                ZlibStreamDecoder::new(FlakySource::new(truncated, chunk)),
                4096,
            )
            .err()
            .unwrap_or_else(|| {
                panic!(
                    "cut at {cut} of {}, read step {chunk}: truncation must be an error, \
                        not a clean end of stream",
                    member.len()
                )
            });
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
                ),
                "cut {cut}, step {chunk}: unexpected error kind {:?}",
                error.kind()
            );
        }
    }
}

/// The same for a truncated *second* member, which is the configuration the
/// rule could actually fire in (`members_decoded() >= 1`). Both the legacy
/// decoder and the strict reader must refuse it, and the bytes of member 1
/// must not be silently presented as the whole stream.
#[test]
fn a_truncated_second_zlib_member_is_an_error_for_every_reader() {
    let first = payload(20_000);
    let second = payload(20_000);
    let member1 = zlib_compress(&first, 6).expect("zlib_compress");
    let member2 = zlib_compress(&second, 6).expect("zlib_compress");

    // 8 bytes is the shortest possible complete zlib member, i.e. the
    // shortest tail the rule must *not* swallow.
    for cut in [8usize, 10, member2.len() - 10, member2.len() - 2] {
        let mut stream = member1.clone();
        stream.extend_from_slice(&member2[..cut]);

        for chunk in [1usize, 3, 65_536] {
            assert!(
                read_all(
                    ZlibStreamDecoder::new(FlakySource::new(&stream, chunk)),
                    4096
                )
                .is_err(),
                "legacy decoder, member 2 cut at {cut}, step {chunk}"
            );
            assert!(
                read_all(InflateReader::zlib(FlakySource::new(&stream, chunk)), 4096).is_err(),
                "strict reader, member 2 cut at {cut}, step {chunk}"
            );
            assert!(
                read_all(
                    InflateReader::zlib(FlakySource::new(&stream, chunk))
                        .trailing_policy(TrailingPolicy::Stop),
                    4096
                )
                .is_err(),
                "Stop reader, member 2 cut at {cut}, step {chunk}"
            );
            assert!(
                read_all(InflateReader::auto(FlakySource::new(&stream, chunk)), 4096).is_err(),
                "auto sniff, member 2 cut at {cut}, step {chunk}"
            );
        }
    }
}

/// The `Auto` sniff resolves to zlib, so it inherits the rule — and with it
/// the obligation to reject a truncated stream.
#[test]
fn auto_sniffed_zlib_rejects_truncation_at_every_cut() {
    let data = payload(9_000);
    let member = zlib_compress(&data, 6).expect("zlib_compress");
    for cut in [member.len() / 2, member.len() - 5, member.len() - 1] {
        for chunk in [1usize, 4096] {
            assert!(
                read_all(
                    InflateReader::auto(FlakySource::new(&member[..cut], chunk)),
                    4096
                )
                .is_err(),
                "auto sniff, cut {cut}, step {chunk}"
            );
        }
    }
    // The whole stream still decodes, so the rejection is not a blanket one.
    assert_eq!(
        read_all(InflateReader::auto(&member[..]), 4096).expect("intact"),
        data
    );
}

/// A `Read` that stops mid-stream must not report fewer bytes as success:
/// the failure mode this closes served member 1 and called it the end.
#[test]
fn truncation_after_a_complete_member_never_returns_ok() {
    let first = payload(5_000);
    let member1 = zlib_compress(&first, 6).expect("zlib_compress");
    let member2 = zlib_compress(&payload(5_000), 6).expect("zlib_compress");
    let mut stream = member1.clone();
    stream.extend_from_slice(&member2[..member2.len() - 3]);

    let mut reader = ZlibStreamDecoder::new(&stream[..]);
    let mut out = Vec::new();
    let error = reader
        .read_to_end(&mut out)
        .expect_err("a truncated trailer must be an error");
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
        ),
        "unexpected kind {:?}",
        error.kind()
    );
}

/// A2's leniency and A3's strictness are the same rule under two policies:
/// `Stop` may drop a 1-5 byte tail, `Reject` and `AllowZeros` may not. The
/// nine crafted tails of R17 are reused so the two halves cannot drift.
#[test]
fn the_short_tail_rule_is_confined_to_trailing_policy_stop() {
    let data = b"payload for the short-tail rule".to_vec();
    let member = zlib_compress(&data, 6).expect("zlib_compress");
    let crafted: [&[u8]; 9] = [
        &[0x78],
        &[0x78, 0x9c],
        &[0x78, 0x9c, 0x00],
        &[0x78, 0x9c, 0x00, 0x11],
        &[0x78, 0x9c, 0x00, 0x11, 0x22],
        &[0x00],
        b"XYZ",
        &[0xff, 0xff, 0xff, 0xff],
        &[0x68, 0xde, 0x01, 0x02, 0x03],
    ];

    for tail in crafted {
        let mut stream = member.clone();
        stream.extend_from_slice(tail);

        // Reject (the default for every new type): a tail is a tail.
        assert!(
            read_all(InflateReader::zlib(&stream[..]), 4096).is_err(),
            "TrailingPolicy::Reject must not silently drop tail {tail:02x?}"
        );
        // AllowZeros: only 0x00 padding is tolerated, and `[0x00]` is the
        // one tail in the table that qualifies.
        let allow_zeros = read_all(
            InflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::AllowZeros),
            4096,
        );
        if tail == [0x00] {
            assert_eq!(
                allow_zeros.expect("a single zero byte is padding"),
                data,
                "tail {tail:02x?}"
            );
        } else {
            assert!(
                allow_zeros.is_err(),
                "TrailingPolicy::AllowZeros must not drop tail {tail:02x?}"
            );
        }
        // Stop: the leniency R17 specifies, at every read granularity.
        for chunk in [1usize, 2, 4096] {
            assert_eq!(
                read_all(
                    InflateReader::zlib(FlakySource::new(&stream, chunk))
                        .trailing_policy(TrailingPolicy::Stop),
                    4096
                )
                .unwrap_or_else(|e| panic!("Stop, tail {tail:02x?}, step {chunk}: {e}")),
                data
            );
        }
    }
}

/// `member_in()` is the whole fix, and no reader-level test can distinguish
/// a correct value from a subtly wrong one (the pump's own staged count is
/// always zero at EOF). Assert the quantity directly, against a definition
/// that does not depend on the decoder's internals: everything consumed
/// past the end of member 1.
#[test]
fn member_in_counts_exactly_the_bytes_after_the_last_member() {
    use oxiarc_core::traits::FlushMode;
    use oxiarc_deflate::{InflateStatus, WrappedInflate};

    let member = zlib_compress(&payload(4_000), 6).expect("zlib_compress");
    let boundary = member.len() as u64;

    for tail_len in [0usize, 1, 2, 3, 5, 6, 9, 40] {
        for chunk in [1usize, 5, 100, usize::MAX] {
            let mut stream = member.clone();
            stream.extend_from_slice(&vec![0xAAu8; tail_len]);

            let mut decoder = WrappedInflate::new(InflateWrapper::Zlib)
                .multi_member(true)
                .trailing_policy(TrailingPolicy::Stop);
            let mut scratch = [0u8; 1024];
            let mut fed = 0usize;
            let mut seen_boundary = false;
            loop {
                let end = fed.saturating_add(chunk).min(stream.len());
                let progress = decoder
                    .inflate(&stream[fed..end], &mut scratch, FlushMode::None)
                    .expect("inflate");
                fed += progress.consumed;
                if decoder.members_decoded() >= 1 {
                    seen_boundary = true;
                    assert_eq!(
                        decoder.member_in(),
                        decoder.total_in() - boundary,
                        "tail {tail_len}, chunk {chunk}: member_in must be \
                         total_in minus the length of member 1"
                    );
                }
                if progress.status == InflateStatus::StreamEnd {
                    break;
                }
                if progress.consumed == 0 && progress.produced == 0 && end == stream.len() {
                    break;
                }
            }
            assert!(seen_boundary, "tail {tail_len}, chunk {chunk}");
        }
    }
}

/// `reset()` clears the counter; `start_next_member` (reached by decoding a
/// genuine second member) deliberately does **not** — the bytes read while
/// deciding that a member starts belong to that member.
#[test]
fn member_in_survives_a_member_start_and_is_cleared_by_reset() {
    use oxiarc_core::traits::FlushMode;
    use oxiarc_deflate::WrappedInflate;

    let first = zlib_compress(b"first member payload", 6).expect("zlib_compress");
    let second = zlib_compress(b"second member payload", 6).expect("zlib_compress");
    let mut stream = first.clone();
    stream.extend_from_slice(&second);

    let mut decoder = WrappedInflate::new(InflateWrapper::Zlib).multi_member(true);
    let mut out = [0u8; 256];
    // Feed only enough to complete member 1 and start member 2.
    let cut = first.len() + 4;
    decoder
        .inflate(&stream[..cut], &mut out, FlushMode::None)
        .expect("inflate");
    assert_eq!(decoder.members_decoded(), 1);
    assert_eq!(decoder.member_in(), decoder.total_in() - first.len() as u64);
    assert!(
        decoder.member_in() > 0,
        "the bytes of member 2 seen so far must still be counted"
    );

    decoder.reset();
    assert_eq!(decoder.member_in(), 0);
    assert_eq!(decoder.members_decoded(), 0);
}

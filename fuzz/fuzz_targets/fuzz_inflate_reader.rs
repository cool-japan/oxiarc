//! Fuzz target for `oxiarc_deflate::InflateReader<R>`, the `std::io::Read`
//! adapter: driven by a source that hands back adversarial, arbitrary-sized
//! short reads (never assuming the caller's buffer is filled in one call —
//! exactly what a real socket does), it must never panic or hang, and must
//! agree byte-for-byte with `WrappedInflate` driven directly over the same
//! bytes in one shot whenever the direct path succeeds.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateReader, InflateStatus, InflateWrapper, WrappedInflate};
use std::io::Read;

/// A `Read` source that hands back short reads of pseudo-random size, seeded
/// from a small fixed sequence drawn from the fuzz input itself so the
/// pattern is deterministic (and therefore reproducible) for a given input.
struct RandomReadReader<'a> {
    data: &'a [u8],
    pos: usize,
    sizes: &'a [usize],
    next_size: usize,
}

impl<'a> RandomReadReader<'a> {
    fn new(data: &'a [u8], sizes: &'a [usize]) -> Self {
        Self {
            data,
            pos: 0,
            sizes,
            next_size: 0,
        }
    }
}

impl Read for RandomReadReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let want = self.sizes[self.next_size % self.sizes.len()].max(1);
        self.next_size = self.next_size.wrapping_add(1);
        let n = buf.len().min(want).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Bound on `read()` calls so a stalled adapter panics instead of hanging
/// the fuzzer.
const CALL_GUARD: u32 = 2_000_000;

fn read_all_bounded<R: Read>(mut reader: R) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 211];
    let mut calls = 0u32;
    loop {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress reading from InflateReader");
        match reader.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) => return Err(e),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let Ok(wrapper_pick) = unstructured.arbitrary::<u8>() else {
        return;
    };
    let Ok(size_seed) = unstructured.arbitrary::<[u8; 4]>() else {
        return;
    };

    let wrapper = match wrapper_pick % 4 {
        0 => InflateWrapper::Raw,
        1 => InflateWrapper::Zlib,
        2 => InflateWrapper::Gzip,
        _ => InflateWrapper::Auto,
    };

    // A handful of short-read sizes derived from the fuzz input, always
    // including 1 (byte-at-a-time is the case that matters most).
    let sizes: [usize; 5] = [
        1,
        1 + (size_seed[0] as usize % 5),
        1 + (size_seed[1] as usize % 17),
        1 + (size_seed[2] as usize % 251),
        1 + (size_seed[3] as usize % 4096),
    ];

    let payload = unstructured.take_rest();

    let via_reader = read_all_bounded(InflateReader::new(
        RandomReadReader::new(payload, &sizes),
        wrapper,
    ));

    // Direct reference: the same wrapper, driven over the whole slice with
    // `WrappedInflate` in one non-final call followed by an explicit Finish
    // signal, matching what `InflateReader` itself does under the hood.
    let direct = {
        let mut decoder = WrappedInflate::new(wrapper).multi_member(true);
        let mut out = Vec::new();
        let mut sink = [0u8; 4096];
        let mut pos = 0usize;
        let mut calls = 0u32;
        let mut ended = false;
        let mut failed = None;
        while !ended && pos < payload.len() {
            calls += 1;
            assert!(calls < CALL_GUARD, "no progress in the direct reference");
            match decoder.inflate(&payload[pos..], &mut sink, FlushMode::None) {
                Ok(progress) => {
                    out.extend_from_slice(&sink[..progress.produced]);
                    pos += progress.consumed;
                    if progress.status == InflateStatus::StreamEnd {
                        ended = true;
                    }
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        while !ended && failed.is_none() {
            calls += 1;
            assert!(
                calls < CALL_GUARD,
                "no progress finishing the direct reference"
            );
            match decoder.inflate(&[], &mut sink, FlushMode::Finish) {
                Ok(progress) => {
                    out.extend_from_slice(&sink[..progress.produced]);
                    if progress.status == InflateStatus::StreamEnd {
                        ended = true;
                    }
                }
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(out),
        }
    };

    if let Ok(expected) = &direct {
        match &via_reader {
            Ok(actual) => assert_eq!(
                expected, actual,
                "InflateReader({wrapper:?}) under random short reads diverged from the \
                 direct WrappedInflate reference"
            ),
            Err(e) => panic!(
                "InflateReader({wrapper:?}) rejected under random short reads what the \
                 direct reference accepted ({} bytes): {e}",
                expected.len()
            ),
        }
    }
    // If the direct reference itself failed, `via_reader` failing too is
    // expected; `via_reader` succeeding where the reference did not is not
    // asserted against — `InflateReader` layers `io::Error` semantics
    // (`UnexpectedEof`, `Interrupted` retry, `WouldBlock` propagation) on
    // top that the hand-rolled reference above does not reproduce exactly.
});

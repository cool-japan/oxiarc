//! Adversarial verification sweep (track `A-verify`).
//!
//! `tests/inflate_stream.rs` proves the push core right on *well-formed*
//! input and on truncation; `tests/inflate_reader.rs` proves the adapters'
//! end-of-stream rules. What neither reaches is the combination this file
//! exists for:
//!
//! * a **mutation** sweep — truncate, bit-flip, drop a byte, insert a byte,
//!   duplicate a byte — driven through *every* front end the crate exposes,
//!   including the `Read` adapters and the `Decompressor` impl, with an
//!   explicit call bound so a state machine that stops making progress fails
//!   instead of hanging. Dropping and inserting a byte is what the existing
//!   sweeps never do, and it is the only mutation that shifts the bit phase
//!   of everything downstream, which is how the length/distance arithmetic
//!   (`pos - distance`, `hist.len() - from_history`, `stored_remaining -=
//!   take`, `lengths[filled - 1]`) is reached with hostile values;
//! * the `member_in()` invariant on the path where the trailing fragment
//!   **is** a plausible zlib header, i.e. `Between` → `start_next_member` →
//!   `ZlHeader` → `Deflate`. The existing invariant test fills the tail with
//!   `0xAA`, which fails the header test and short-circuits into `close()`,
//!   so the counting done by `start_next_member`'s successor states is
//!   unexercised — and that is exactly the path the F1 fix depends on;
//! * zero-length-output calls repeated to a fixed point, and resumption
//!   afterwards.
//!
//! Every assertion here is a property that must hold whatever the input is:
//! no panic, no unbounded loop, and `Ok` from one front end agrees byte for
//! byte with `Ok` from another.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oxiarc_core::traits::{DecompressStatus, Decompressor, FlushMode};
use oxiarc_deflate::{
    GzipStreamDecoder, InflateReader, InflateStatus, InflateStream, InflateWrapper, Inflater,
    TrailingPolicy, WrappedInflate, ZlibStreamDecoder, deflate, gzip_compress, inflate,
    inflate_into, zlib_compress,
};
use std::io::{self, Read};

// ---------------------------------------------------------------------------
// Corpora and framing
// ---------------------------------------------------------------------------

/// Deterministic xorshift, so the corpus is identical on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
}

/// Small payloads that between them force stored, fixed and dynamic blocks,
/// long matches, `distance == 1` runs and incompressible data.
fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let random: Vec<u8> = (0..400).map(|_| rng.byte()).collect();
    let mixed = {
        let mut data = Vec::new();
        for i in 0..40u32 {
            data.extend_from_slice(format!("line {i}: the quick brown fox; ").as_bytes());
            data.push(rng.byte());
        }
        data
    };
    vec![
        ("empty", Vec::new()),
        ("one", vec![b'x']),
        ("runs", vec![b'a'; 500]),
        (
            "text",
            b"the quick brown fox jumps over the lazy dog. ".repeat(9),
        ),
        ("mixed", mixed),
        ("random", random),
    ]
}

/// The three container framings, plus the compressor that produces each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Raw,
    Zlib,
    Gzip,
}

impl Framing {
    fn compress(self, data: &[u8], level: u8) -> Vec<u8> {
        match self {
            Framing::Raw => deflate(data, level).expect("deflate"),
            Framing::Zlib => zlib_compress(data, level).expect("zlib_compress"),
            Framing::Gzip => gzip_compress(data, level).expect("gzip_compress"),
        }
    }

    fn wrapper(self) -> InflateWrapper {
        match self {
            Framing::Raw => InflateWrapper::Raw,
            Framing::Zlib => InflateWrapper::Zlib,
            Framing::Gzip => InflateWrapper::Gzip,
        }
    }
}

// ---------------------------------------------------------------------------
// Bounded drivers
// ---------------------------------------------------------------------------

/// Upper bound on decoder calls for a stream of `in_len` compressed bytes fed
/// `in_chunk` at a time into an `out_size` buffer. Generous, but finite: the
/// point is that a decoder which stops making progress trips it rather than
/// spinning forever.
fn call_bound(in_len: usize, in_chunk: usize, out_size: usize) -> usize {
    // 64 MiB of output is far more than any corpus here can produce, and the
    // limits below cap it anyway.
    let out_calls = (64 * 1024 * 1024) / out_size.max(1);
    4 * (in_len / in_chunk.max(1) + out_calls.min(4096) + 16)
}

/// Drive [`InflateStream`] to completion or to an error, with a call bound.
fn push_raw(
    compressed: &[u8],
    in_chunk: usize,
    out_size: usize,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut stream = InflateStream::new().with_max_output(4 * 1024 * 1024);
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_size];
    let mut fed = 0usize;
    let mut calls = 0usize;
    let bound = call_bound(compressed.len(), in_chunk, out_size);
    loop {
        calls += 1;
        assert!(
            calls < bound,
            "InflateStream made no progress in {calls} calls"
        );
        let end = (fed + in_chunk).min(compressed.len());
        let slice = compressed.get(fed..end).unwrap_or_default();
        let flush = if end >= compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.inflate(slice, &mut scratch, flush)?;
        fed += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
    }
}

/// Drive [`WrappedInflate`] to completion or to an error, with a call bound.
fn push_wrapped(
    compressed: &[u8],
    wrapper: InflateWrapper,
    policy: TrailingPolicy,
    in_chunk: usize,
    out_size: usize,
) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut decoder = WrappedInflate::new(wrapper)
        .multi_member(true)
        .trailing_policy(policy)
        .with_max_output(4 * 1024 * 1024);
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_size];
    let mut fed = 0usize;
    let mut calls = 0usize;
    let bound = call_bound(compressed.len(), in_chunk, out_size);
    loop {
        calls += 1;
        assert!(
            calls < bound,
            "WrappedInflate({wrapper:?}) made no progress in {calls} calls"
        );
        let end = (fed + in_chunk).min(compressed.len());
        let slice = compressed.get(fed..end).unwrap_or_default();
        let flush = if end >= compressed.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = decoder.inflate(slice, &mut scratch, flush)?;
        fed += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            return Ok(out);
        }
    }
}

/// `Read` to the end with an explicit call bound.
fn read_bounded<R: Read>(mut reader: R, chunk: usize, bound: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; chunk.max(1)];
    let mut calls = 0usize;
    loop {
        calls += 1;
        assert!(
            calls < bound,
            "Read adapter made no progress in {calls} calls"
        );
        let n = reader.read(&mut buf)?;
        if n == 0 {
            return Ok(out);
        }
        out.extend_from_slice(&buf[..n]);
        assert!(
            out.len() <= 8 * 1024 * 1024,
            "Read adapter produced more than any corpus here can"
        );
    }
}

/// Drive the `Decompressor` impl, advancing `input` by `consumed` as the
/// contract requires, with a call bound.
fn decompressor_loop(compressed: &[u8], out_size: usize) -> oxiarc_core::error::Result<Vec<u8>> {
    let mut inflater = Inflater::new();
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_size];
    let mut fed = 0usize;
    let mut calls = 0usize;
    let bound = call_bound(compressed.len(), 1, out_size);
    loop {
        calls += 1;
        assert!(
            calls < bound,
            "Decompressor made no progress in {calls} calls"
        );
        let (consumed, produced, status) =
            inflater.decompress(compressed.get(fed..).unwrap_or_default(), &mut scratch)?;
        fed += consumed;
        out.extend_from_slice(&scratch[..produced]);
        if status == DecompressStatus::Done {
            return Ok(out);
        }
        assert!(
            out.len() <= 8 * 1024 * 1024,
            "Decompressor produced more than any corpus here can"
        );
        if consumed == 0 && produced == 0 {
            // Nothing consumed, nothing produced and not done: under
            // `FlushMode::Finish` this cannot make progress ever again.
            panic!("Decompressor stalled with status {status:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The mutation sweep
// ---------------------------------------------------------------------------

/// Every mutation applied to a compressed stream.
fn mutations(base: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    // Truncation at every offset.
    for cut in 0..base.len() {
        out.push(base.get(..cut).unwrap_or_default().to_vec());
    }
    for offset in 0..base.len() {
        // Flip the low bit and the high bit: the first perturbs a Huffman
        // code, the second a length/distance extra field more often.
        for mask in [0x01u8, 0x80] {
            let mut copy = base.to_vec();
            if let Some(slot) = copy.get_mut(offset) {
                *slot ^= mask;
            }
            out.push(copy);
        }
        // Drop a byte: everything downstream shifts by 8 bits.
        let mut dropped = base.to_vec();
        dropped.remove(offset);
        out.push(dropped);
        // Insert a byte: the same shift the other way.
        let mut inserted = base.to_vec();
        inserted.insert(offset, 0x5A);
        out.push(inserted);
        // Duplicate a byte, which keeps the length plausible.
        let mut duplicated = base.to_vec();
        let byte = duplicated.get(offset).copied().unwrap_or(0);
        duplicated.insert(offset, byte);
        out.push(duplicated);
    }
    out
}

/// Run one mutated stream through every front end. Nothing may panic, nothing
/// may loop without progress, and two front ends that both succeed must agree
/// byte for byte.
fn probe_every_front_end(framing: Framing, bytes: &[u8], label: &str) {
    let wrapper = framing.wrapper();

    // 1. The push core, at two granularities. Both must agree.
    if framing == Framing::Raw {
        let whole = push_raw(bytes, usize::MAX, 4096);
        let dribble = push_raw(bytes, 1, 7);
        match (&whole, &dribble) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{label}: push granularity changed the output"),
            (Err(_), Err(_)) => {}
            (a, b) => panic!("{label}: push granularity changed acceptance: {a:?} vs {b:?}"),
        }
        // The one-shot entry points must not disagree with the push core.
        if let Ok(expected) = &whole {
            match inflate(bytes) {
                Ok(got) => assert_eq!(&got, expected, "{label}: inflate() disagreed"),
                Err(_) => {
                    // `inflate()` has no trailing-byte policy of its own and
                    // stops at the end of the DEFLATE stream, so it may
                    // reject where the push core (with `Finish`) accepts and
                    // vice versa; only agreement on success is asserted.
                }
            }
            let mut dst = vec![0u8; expected.len()];
            if let Ok(n) = inflate_into(bytes, &mut dst) {
                assert_eq!(n, expected.len(), "{label}: inflate_into length");
                assert_eq!(&dst[..n], &expected[..], "{label}: inflate_into bytes");
            }
        }
        // `Decompressor` runs the same core under `Finish`.
        match (&whole, decompressor_loop(bytes, 64)) {
            (Ok(a), Ok(b)) => assert_eq!(a, &b, "{label}: Decompressor disagreed"),
            (Err(_), Ok(b)) => panic!(
                "{label}: Decompressor accepted what the push core rejected: {} bytes",
                b.len()
            ),
            _ => {}
        }
    }

    // 2. The framing machine, at two granularities and two policies.
    for policy in [TrailingPolicy::Reject, TrailingPolicy::Stop] {
        let whole = push_wrapped(bytes, wrapper, policy, usize::MAX, 4096);
        let dribble = push_wrapped(bytes, wrapper, policy, 1, 3);
        match (&whole, &dribble) {
            (Ok(a), Ok(b)) => assert_eq!(
                a, b,
                "{label}/{policy:?}: wrapper granularity changed the output"
            ),
            (Err(_), Err(_)) => {}
            (a, b) => {
                panic!("{label}/{policy:?}: wrapper granularity changed acceptance: {a:?} vs {b:?}")
            }
        }
        // `Auto` must resolve to the same framing and produce the same bytes
        // for gzip and zlib (a raw stream may legally sniff as zlib).
        if framing != Framing::Raw {
            let sniffed = push_wrapped(bytes, InflateWrapper::Auto, policy, usize::MAX, 4096);
            if let (Ok(a), Ok(b)) = (&whole, &sniffed) {
                assert_eq!(a, b, "{label}/{policy:?}: Auto disagreed with {framing:?}");
            }
        }
    }

    // 3. The blocking `Read` adapters, including one-byte reads.
    for chunk in [1usize, 4096] {
        let bound = call_bound(bytes.len(), 1, chunk) + 4096;
        let strict = read_bounded(InflateReader::new(bytes, wrapper), chunk, bound);
        let lenient = read_bounded(
            InflateReader::new(bytes, wrapper).trailing_policy(TrailingPolicy::Stop),
            chunk,
            bound,
        );
        if let (Ok(a), Ok(b)) = (&strict, &lenient) {
            assert_eq!(a, b, "{label}: policy changed the decoded bytes");
        }
        let _ = read_bounded(InflateReader::auto(bytes), chunk, bound);
        match framing {
            Framing::Zlib => {
                let legacy = read_bounded(ZlibStreamDecoder::new(bytes), chunk, bound);
                if let (Ok(a), Ok(b)) = (&strict, &legacy) {
                    assert_eq!(a, b, "{label}: ZlibStreamDecoder disagreed");
                }
            }
            Framing::Gzip => {
                let legacy = read_bounded(GzipStreamDecoder::new(bytes), chunk, bound);
                if let (Ok(a), Ok(b)) = (&strict, &legacy) {
                    assert_eq!(a, b, "{label}: GzipStreamDecoder disagreed");
                }
            }
            Framing::Raw => {}
        }
    }
}

/// The sweep itself. Small corpora, but every mutation of every byte of every
/// framing at two compression levels.
#[test]
fn mutated_streams_never_panic_hang_or_disagree() {
    for (name, payload) in corpus() {
        for level in [0u8, 6] {
            for framing in [Framing::Raw, Framing::Zlib, Framing::Gzip] {
                let base = framing.compress(&payload, level);
                // The unmutated stream first, as a control.
                probe_every_front_end(framing, &base, &format!("{name}/{level}/{framing:?}/clean"));
                for (index, mutated) in mutations(&base).into_iter().enumerate() {
                    probe_every_front_end(
                        framing,
                        &mutated,
                        &format!("{name}/{level}/{framing:?}/mutation {index}"),
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `member_in()` on the plausible-header path
// ---------------------------------------------------------------------------

/// The F1 fix rests entirely on `member_in()`, and the existing invariant test
/// fills the trailing fragment with `0xAA` — which fails
/// `zlib_header_is_plausible` and therefore never reaches
/// `start_next_member` → `ZlHeader` → `Deflate`. Those three states each
/// account for bytes differently (`field` replay, accumulator drain, core
/// `consumed`), so the interesting arithmetic is exactly the one the existing
/// test skips.
#[test]
fn member_in_is_exact_when_the_tail_is_a_plausible_zlib_header() {
    let first = zlib_compress(&b"first member of the pair".repeat(40), 6).expect("zlib_compress");
    let second = zlib_compress(&b"second member of the pair".repeat(40), 6).expect("zlib_compress");
    let boundary = first.len() as u64;

    for tail_len in 1..=16usize {
        for chunk in [1usize, 2, 3, 7, usize::MAX] {
            let mut stream = first.clone();
            stream.extend_from_slice(second.get(..tail_len).unwrap_or_default());

            let mut decoder = WrappedInflate::new(InflateWrapper::Zlib)
                .multi_member(true)
                .trailing_policy(TrailingPolicy::Stop);
            let mut scratch = [0u8; 512];
            let mut fed = 0usize;
            let mut checked = false;
            let mut calls = 0usize;
            loop {
                calls += 1;
                assert!(calls < 4096, "tail {tail_len}, chunk {chunk}: no progress");
                let end = fed.saturating_add(chunk).min(stream.len());
                let progress = decoder
                    .inflate(
                        stream.get(fed..end).unwrap_or_default(),
                        &mut scratch,
                        FlushMode::None,
                    )
                    .expect("inflate");
                fed += progress.consumed;
                if decoder.members_decoded() >= 1 {
                    checked = true;
                    assert_eq!(
                        decoder.member_in(),
                        decoder.total_in() - boundary,
                        "tail {tail_len}, chunk {chunk}: member_in must count exactly \
                         the bytes consumed past member 1"
                    );
                    // And it can never claim more than actually exists.
                    assert!(
                        decoder.member_in() <= tail_len as u64,
                        "tail {tail_len}, chunk {chunk}: member_in {} exceeds the tail",
                        decoder.member_in()
                    );
                }
                if progress.status == InflateStatus::StreamEnd {
                    break;
                }
                if progress.consumed == 0 && progress.produced == 0 && end == stream.len() {
                    break;
                }
            }
            assert!(
                checked,
                "tail {tail_len}, chunk {chunk}: never saw member 1 complete"
            );
        }
    }
}

/// The adapter-level consequence of the invariant above, and the property the
/// short-tail rule must never break: **a successful decode returns exactly the
/// bytes of the complete members and nothing else.**
///
/// The whole-slice decoder this rule reproduces tested `remaining.len() < 6`
/// *before* it touched the fragment, so a dropped fragment contributed no
/// output. A push decoder decodes the fragment as it arrives and only learns
/// at EOF that nothing follows; if the rule then ends the stream cleanly, the
/// caller has already been handed payload from a member whose Adler-32 is
/// never checked. That is silent truncation wearing an `Ok`.
#[test]
fn a_truncated_second_member_never_leaks_bytes_into_a_clean_result() {
    let first = zlib_compress(&b"first member".repeat(30), 6).expect("zlib_compress");
    let second = zlib_compress(&b"second member".repeat(30), 6).expect("zlib_compress");
    let expected = b"first member".repeat(30);

    for tail_len in 1..=16usize {
        let mut stream = first.clone();
        stream.extend_from_slice(second.get(..tail_len).unwrap_or_default());

        for chunk in [1usize, 5, 65536] {
            let bound = call_bound(stream.len(), 1, chunk) + 4096;
            let legacy = read_bounded(ZlibStreamDecoder::new(&stream[..]), chunk, bound);
            let lenient = read_bounded(
                InflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::Stop),
                chunk,
                bound,
            );
            let strict = read_bounded(InflateReader::zlib(&stream[..]), chunk, bound);

            for (name, outcome) in [("ZlibStreamDecoder", &legacy), ("Stop", &lenient)] {
                if let Ok(bytes) = outcome {
                    assert_eq!(
                        bytes.as_slice(),
                        &expected[..],
                        "tail {tail_len}, chunk {chunk}: {name} returned Ok with \
                         {} bytes — a fragment of an unverified member leaked into \
                         a clean result",
                        bytes.len()
                    );
                }
            }
            if tail_len >= 6 {
                assert!(
                    legacy.is_err(),
                    "tail {tail_len}, chunk {chunk}: a {tail_len}-byte truncated second \
                     member came back as Ok"
                );
                assert!(
                    lenient.is_err(),
                    "tail {tail_len}, chunk {chunk}: Stop accepted a truncated member"
                );
            }
            // The strict default rejects every fragment, long or short.
            assert!(
                strict.is_err(),
                "tail {tail_len}, chunk {chunk}: TrailingPolicy::Reject accepted a fragment"
            );
        }
    }
}

/// The smallest fragment that decodes something, built by hand so it cannot
/// drift with the compressor: `78 9c` is the commonest real zlib header (and
/// passes both structural checks), `72 04` is a non-final fixed-Huffman block
/// carrying the single literal `A`.
///
/// Four bytes is below the six-byte threshold, so the short-tail rule wants to
/// drop it — but by then `A` has been served. Before this was fixed,
/// `ZlibStreamDecoder` returned `Ok("payloadA")` where the whole-slice decoder
/// it replaces returned `Ok("payload")`.
#[test]
fn a_four_byte_fragment_that_decodes_a_literal_is_an_error() {
    const LITERAL_FRAGMENT: [u8; 4] = [0x78, 0x9c, 0x72, 0x04];

    let member = zlib_compress(b"payload", 6).expect("zlib_compress");
    let mut stream = member.clone();
    stream.extend_from_slice(&LITERAL_FRAGMENT);

    // The fragment really does decode a byte on its own, so the test is not
    // vacuous: fed as a stream of its own it produces `A` and then runs out.
    let solo = push_wrapped(
        &LITERAL_FRAGMENT,
        InflateWrapper::Zlib,
        TrailingPolicy::Stop,
        usize::MAX,
        64,
    );
    assert!(
        solo.is_err(),
        "the fragment is a truncated member and must not decode cleanly"
    );

    for chunk in [1usize, 3, 65536] {
        let bound = call_bound(stream.len(), 1, chunk) + 4096;
        assert!(
            read_bounded(ZlibStreamDecoder::new(&stream[..]), chunk, bound).is_err(),
            "chunk {chunk}: ZlibStreamDecoder served a byte of an unverified member \
             and then reported a clean end of stream"
        );
        assert!(
            read_bounded(
                InflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::Stop),
                chunk,
                bound,
            )
            .is_err(),
            "chunk {chunk}: Stop served a byte of an unverified member"
        );
    }
}

/// The negative control for the two tests above: the leniency itself must
/// survive. Every fragment that decodes **nothing** — padding, garbage, a
/// crafted header, a header plus an unfinished stored-block length — is still
/// dropped in silence, exactly as the whole-slice decoder dropped it.
#[test]
fn fragments_that_decode_nothing_are_still_ignored() {
    let data = b"payload for the short-tail rule".to_vec();
    let member = zlib_compress(&data, 6).expect("zlib_compress");
    let fragments: [&[u8]; 9] = [
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
    for tail in fragments {
        let mut stream = member.clone();
        stream.extend_from_slice(tail);
        for chunk in [1usize, 4096] {
            let bound = call_bound(stream.len(), 1, chunk) + 4096;
            assert_eq!(
                read_bounded(ZlibStreamDecoder::new(&stream[..]), chunk, bound)
                    .unwrap_or_else(|e| panic!("tail {tail:02x?}, chunk {chunk}: {e}")),
                data,
                "tail {tail:02x?}, chunk {chunk}"
            );
            assert_eq!(
                read_bounded(
                    InflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::Stop),
                    chunk,
                    bound,
                )
                .unwrap_or_else(|e| panic!("strict/Stop, tail {tail:02x?}, chunk {chunk}: {e}")),
                data,
                "strict/Stop, tail {tail:02x?}, chunk {chunk}"
            );
        }
    }
}

/// A whole second member must still decode, at every feed granularity — the
/// negative control for the test above (the rule must not eat real data).
#[test]
fn a_complete_second_member_still_decodes_at_every_granularity() {
    let first = zlib_compress(&b"first member".repeat(30), 6).expect("zlib_compress");
    let second = zlib_compress(&b"second member".repeat(30), 6).expect("zlib_compress");
    let mut stream = first.clone();
    stream.extend_from_slice(&second);
    let mut expected = b"first member".repeat(30);
    expected.extend_from_slice(&b"second member".repeat(30));

    for chunk in [1usize, 2, 3, 7, 64, 65536] {
        let bound = call_bound(stream.len(), 1, chunk) + 4096;
        assert_eq!(
            read_bounded(ZlibStreamDecoder::new(&stream[..]), chunk, bound).expect("legacy"),
            expected,
            "chunk {chunk}"
        );
        assert_eq!(
            read_bounded(InflateReader::zlib(&stream[..]), chunk, bound).expect("strict"),
            expected,
            "chunk {chunk}"
        );
        assert_eq!(
            read_bounded(InflateReader::auto(&stream[..]), chunk, bound).expect("auto"),
            expected,
            "chunk {chunk}"
        );
    }
}

// ---------------------------------------------------------------------------
// Zero-length output, repeated to a fixed point
// ---------------------------------------------------------------------------

/// A caller that offers no output space must reach a fixed point rather than
/// consuming input for ever, must not latch a fault, and must resume exactly
/// once space appears.
#[test]
fn zero_length_output_reaches_a_fixed_point_and_resumes() {
    let payload = b"zero space then real space, repeated".repeat(200);

    for framing in [Framing::Raw, Framing::Zlib, Framing::Gzip] {
        let compressed = framing.compress(&payload, 6);

        // The raw core.
        if framing == Framing::Raw {
            let mut stream = InflateStream::new();
            let mut total_consumed = 0usize;
            for call in 0..32 {
                let progress = stream
                    .inflate(
                        compressed.get(total_consumed..).unwrap_or_default(),
                        &mut [],
                        FlushMode::None,
                    )
                    .expect("zero-length output must not be an error");
                total_consumed += progress.consumed;
                assert_eq!(progress.produced, 0);
                assert_eq!(progress.status, InflateStatus::NeedOutput);
                assert!(
                    total_consumed <= 8,
                    "call {call}: a zero-length output call absorbed {total_consumed} \
                     bytes; only the bit accumulator may fill"
                );
            }
            // Resume with real space: the rest decodes exactly.
            let mut out = Vec::new();
            let mut scratch = [0u8; 4096];
            let mut fed = total_consumed;
            loop {
                let progress = stream
                    .inflate(
                        compressed.get(fed..).unwrap_or_default(),
                        &mut scratch,
                        FlushMode::Finish,
                    )
                    .expect("resume");
                fed += progress.consumed;
                out.extend_from_slice(&scratch[..progress.produced]);
                if progress.status == InflateStatus::StreamEnd {
                    break;
                }
            }
            assert_eq!(out, payload, "resumption after zero-length output");
        }

        // The framing machine: header bytes may be consumed, but the total
        // still cannot exceed the header plus one accumulator load.
        let mut decoder = WrappedInflate::new(framing.wrapper()).multi_member(true);
        let mut consumed = 0usize;
        for _ in 0..32 {
            let progress = decoder
                .inflate(
                    compressed.get(consumed..).unwrap_or_default(),
                    &mut [],
                    FlushMode::None,
                )
                .expect("zero-length output must not be an error");
            consumed += progress.consumed;
            assert_eq!(progress.produced, 0);
        }
        assert!(
            consumed <= 64,
            "{framing:?}: zero-length output calls absorbed {consumed} bytes"
        );
        let mut out = Vec::new();
        let mut scratch = [0u8; 4096];
        let mut fed = consumed;
        loop {
            let progress = decoder
                .inflate(
                    compressed.get(fed..).unwrap_or_default(),
                    &mut scratch,
                    FlushMode::Finish,
                )
                .expect("resume");
            fed += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            if progress.status == InflateStatus::StreamEnd {
                break;
            }
        }
        assert_eq!(
            out, payload,
            "{framing:?}: resumption after zero-length output"
        );
    }
}

// ---------------------------------------------------------------------------
// Hostile declared sizes
// ---------------------------------------------------------------------------

/// A gzip header that declares a 64 KiB `FEXTRA` and then stops must be an
/// error, and the decoder must never have grown past the documented cap while
/// deciding that. Also covers the `FNAME`/`FCOMMENT` cap with a field that
/// never terminates.
#[test]
fn hostile_gzip_header_lengths_are_bounded() {
    // FEXTRA declared as 65535 bytes, only 10 supplied.
    let mut truncated_extra = vec![0x1f, 0x8b, 0x08, 0x04, 0, 0, 0, 0, 0x00, 0xff];
    truncated_extra.extend_from_slice(&65535u16.to_le_bytes());
    truncated_extra.extend_from_slice(&[0u8; 10]);
    let bound = call_bound(truncated_extra.len(), 1, 64) + 64;
    assert!(
        read_bounded(InflateReader::gzip(&truncated_extra[..]), 64, bound).is_err(),
        "a truncated FEXTRA must be an error"
    );

    // FNAME that never terminates: rejected at the 64 KiB cap, and the error
    // must arrive well before the whole 1 MiB is read.
    let mut runaway_name = vec![0x1f, 0x8b, 0x08, 0x08, 0, 0, 0, 0, 0x00, 0xff];
    runaway_name.extend_from_slice(&vec![b'A'; 1024 * 1024]);
    let bound = call_bound(runaway_name.len(), 1, 4096) + 4096;
    let mut reader = InflateReader::gzip(&runaway_name[..]);
    let mut sink = vec![0u8; 4096];
    let mut calls = 0usize;
    let error = loop {
        calls += 1;
        assert!(calls < bound, "runaway FNAME did not terminate");
        match reader.read(&mut sink) {
            Ok(0) => panic!("a runaway FNAME must not be a clean end of stream"),
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    assert!(
        error.to_string().contains("64 KiB"),
        "expected the header-field cap, got {error}"
    );
    assert!(
        reader.total_in() < 256 * 1024,
        "the cap fired after {} bytes",
        reader.total_in()
    );
}

/// A zlib member whose payload expands enormously must stop at the configured
/// cap, deliver exactly the bytes up to it, and then fail — never allocate to
/// the full expansion first.
#[test]
fn an_expanding_member_stops_at_the_cap_through_the_reader() {
    let bomb = zlib_compress(&vec![0u8; 4 * 1024 * 1024], 6).expect("zlib_compress");
    let mut reader = ZlibStreamDecoder::new(&bomb[..]).with_max_output(4096);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1024];
    let error = loop {
        match reader.read(&mut buf) {
            Ok(0) => panic!("a capped stream must fail, not end cleanly"),
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                assert!(
                    out.len() <= 4096,
                    "delivered {} bytes past the cap",
                    out.len()
                );
            }
            Err(error) => break error,
        }
    };
    assert_eq!(out.len(), 4096, "the bytes up to the cap must still arrive");
    assert!(
        error.to_string().contains("budget") || error.to_string().contains("memory"),
        "unexpected error {error}"
    );
}

// ---------------------------------------------------------------------------
// The `Decompressor` contract
// ---------------------------------------------------------------------------

/// A one-byte output buffer must not turn into an unbounded loop, and the
/// documented "advance `input` by `consumed`" contract must produce exactly
/// the original bytes.
#[test]
fn decompressor_with_a_one_byte_buffer_terminates_and_is_exact() {
    let payload = b"one byte at a time through the trait".repeat(30);
    let compressed = deflate(&payload, 6).expect("deflate");
    assert_eq!(
        decompressor_loop(&compressed, 1).expect("decompress"),
        payload
    );

    // Truncation stays an error however small the buffer is, and stays an
    // error on every later call.
    let cut = compressed.len() - 3;
    let mut inflater = Inflater::new();
    let mut scratch = [0u8; 1];
    let mut fed = 0usize;
    let mut calls = 0usize;
    let error = loop {
        calls += 1;
        assert!(calls < 100_000, "truncated stream did not terminate");
        match inflater.decompress(compressed.get(fed..cut).unwrap_or_default(), &mut scratch) {
            Ok((consumed, _, status)) => {
                fed += consumed;
                assert_ne!(
                    status,
                    DecompressStatus::Done,
                    "a truncated stream reported Done"
                );
            }
            Err(error) => break error,
        }
    };
    let replay = inflater
        .decompress(&[], &mut scratch)
        .expect_err("the fault must be sticky");
    assert_eq!(replay.to_string(), error.to_string());
}

// ---------------------------------------------------------------------------
// The async adapter shares the pump, so it must share the rule
// ---------------------------------------------------------------------------

#[cfg(feature = "async-io")]
mod asynchronous {
    use super::*;
    use oxiarc_deflate::AsyncInflateReader;
    use tokio::io::AsyncReadExt;

    /// `AsyncInflateReader` is the adapter `oxiarc-http`'s `AsyncDecodedBody`
    /// will run, and `short_tail_ends_stream` lives in the pump both adapters
    /// share — so the fragment rule is asserted here directly rather than by
    /// inheritance from the blocking twin.
    #[test]
    fn a_decoding_fragment_is_an_error_through_the_async_adapter() {
        const LITERAL_FRAGMENT: [u8; 4] = [0x78, 0x9c, 0x72, 0x04];
        let member = zlib_compress(&b"async payload".repeat(50), 6).expect("zlib_compress");
        let expected = b"async payload".repeat(50);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            // A fragment that decodes a byte must not end the stream cleanly.
            let mut stream = member.clone();
            stream.extend_from_slice(&LITERAL_FRAGMENT);
            let mut reader =
                AsyncInflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::Stop);
            let mut out = Vec::new();
            reader
                .read_to_end(&mut out)
                .await
                .expect_err("a fragment that decoded a literal must be an error");

            // A fragment that decodes nothing still is ignored.
            let mut stream = member.clone();
            stream.extend_from_slice(&[0x78, 0x9c]);
            let mut reader =
                AsyncInflateReader::zlib(&stream[..]).trailing_policy(TrailingPolicy::Stop);
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.expect("inert fragment");
            assert_eq!(out, expected);

            // And the whole member alone is unaffected.
            let mut reader =
                AsyncInflateReader::zlib(&member[..]).trailing_policy(TrailingPolicy::Stop);
            let mut out = Vec::new();
            reader.read_to_end(&mut out).await.expect("clean member");
            assert_eq!(out, expected);
        });
    }
}

// ---------------------------------------------------------------------------
// Per-member `ISIZE`, which shares the counter the fragment rule reads
// ---------------------------------------------------------------------------

/// `member_out` doubles as the gzip `ISIZE` accumulator, and the member
/// boundary now re-bases it in two places (the trailer states and
/// `start_next_member`). If either reset were dropped or moved, member 2's
/// `ISIZE` would be compared against member 1's output as well; if the reset
/// ran too early, member 1's `ISIZE` would be compared against zero. Both
/// directions are pinned here, on a concatenated stream, at three feed
/// granularities.
#[test]
fn per_member_gzip_isize_survives_the_member_boundary_rebase() {
    let first = gzip_compress(&b"first gzip member".repeat(20), 6).expect("gzip_compress");
    let second =
        gzip_compress(&b"second gzip member, longer".repeat(20), 6).expect("gzip_compress");
    let mut clean = first.clone();
    clean.extend_from_slice(&second);
    let mut expected = b"first gzip member".repeat(20);
    expected.extend_from_slice(&b"second gzip member, longer".repeat(20));

    for chunk in [1usize, 7, 65536] {
        let bound = call_bound(clean.len(), 1, chunk) + 4096;
        assert_eq!(
            read_bounded(GzipStreamDecoder::new(&clean[..]), chunk, bound).expect("legacy"),
            expected,
            "chunk {chunk}"
        );
        assert_eq!(
            read_bounded(InflateReader::gzip(&clean[..]), chunk, bound).expect("strict"),
            expected,
            "chunk {chunk}"
        );

        // Corrupt member 1's ISIZE (the last 4 bytes of member 1).
        let mut broken = clean.clone();
        let at = first.len() - 4;
        broken[at] ^= 0xFF;
        assert!(
            read_bounded(InflateReader::gzip(&broken[..]), chunk, bound).is_err(),
            "chunk {chunk}: member 1 ISIZE must be checked against member 1's output"
        );

        // Corrupt member 2's ISIZE (the last 4 bytes of the stream).
        let mut broken = clean.clone();
        let at = clean.len() - 4;
        broken[at] ^= 0xFF;
        assert!(
            read_bounded(InflateReader::gzip(&broken[..]), chunk, bound).is_err(),
            "chunk {chunk}: member 2 ISIZE must be checked against member 2's output"
        );

        // And a *correct* member 2 ISIZE must not be rejected because member
        // 1's output was still counted — the failure the reset prevents.
        assert_eq!(
            read_bounded(InflateReader::gzip(&clean[..]), chunk, bound).expect("clean again"),
            expected,
            "chunk {chunk}"
        );
    }
}

// ---------------------------------------------------------------------------
// An error must stay an error
// ---------------------------------------------------------------------------

/// Every failure path in the adapters must be **sticky**. A caller that
/// ignores one `Err` and reads again must not be handed an `Ok(0)` (the
/// truncation defect wearing a delay) or an `Ok(n)` of bytes decoded by the
/// call that failed. `is_finished()` must also stay `false`: the stream did
/// not finish, it failed.
///
/// This covers the pump-raised error as well as the wrapper-latched ones —
/// the pump raises its own `UnexpectedEof` without touching the wrapper's
/// fault latch, so stickiness there rests on the state machine not moving,
/// not on the latch.
#[test]
fn adapter_errors_are_sticky_across_repeated_reads() {
    const LITERAL_FRAGMENT: [u8; 4] = [0x78, 0x9c, 0x72, 0x04];
    let payload = b"sticky error payload".repeat(50);
    let zl = zlib_compress(&payload, 6).expect("zlib_compress");
    let gz = gzip_compress(&payload, 6).expect("gzip_compress");

    let mut fragment_tail = zl.clone();
    fragment_tail.extend_from_slice(&LITERAL_FRAGMENT);

    let mut corrupt = zl.clone();
    let mid = corrupt.len() / 2;
    corrupt[mid] ^= 0xFF;

    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "zlib truncated",
            zl.get(..zl.len() - 5).unwrap_or_default().to_vec(),
        ),
        (
            "gzip truncated",
            gz.get(..gz.len() - 5).unwrap_or_default().to_vec(),
        ),
        ("zlib corrupt mid-payload", corrupt),
        ("decoding fragment after a member", fragment_tail),
    ];

    for (name, bytes) in cases {
        // Every reader configuration that can reach an error on this input.
        let readers: Vec<(&str, Box<dyn Read>)> = vec![
            (
                "InflateReader::zlib",
                Box::new(InflateReader::zlib(&bytes[..])),
            ),
            (
                "InflateReader::zlib(Stop)",
                Box::new(InflateReader::zlib(&bytes[..]).trailing_policy(TrailingPolicy::Stop)),
            ),
            (
                "InflateReader::auto",
                Box::new(InflateReader::auto(&bytes[..])),
            ),
            (
                "ZlibStreamDecoder",
                Box::new(ZlibStreamDecoder::new(&bytes[..])),
            ),
            (
                "GzipStreamDecoder",
                Box::new(GzipStreamDecoder::new(&bytes[..])),
            ),
        ];
        for (which, mut reader) in readers {
            let mut buf = vec![0u8; 4096];
            let mut calls = 0usize;
            let first = loop {
                calls += 1;
                assert!(calls < 100_000, "{name}/{which}: no progress");
                match reader.read(&mut buf) {
                    Ok(0) => break None,
                    Ok(_) => {}
                    Err(error) => break Some(error),
                }
            };
            let Some(first) = first else {
                // This reader legitimately accepts the input (e.g. the
                // lenient gzip decoder over a zlib stream); nothing to pin.
                continue;
            };
            for round in 0..3 {
                let again = reader.read(&mut buf);
                match again {
                    Err(error) => assert_eq!(
                        error.kind(),
                        first.kind(),
                        "{name}/{which} round {round}: the error changed kind"
                    ),
                    Ok(served) => panic!(
                        "{name}/{which} round {round}: a failed stream returned \
                         Ok({served}) — an error must not decay into a clean end \
                         of stream or into more bytes"
                    ),
                }
            }
        }
    }
}

//! `Content-Encoding: compress` / `x-compress` — the legacy UNIX
//! `compress(1)` `.Z` format, driven through `oxiarc_lzw::z::ZReader`.
//!
//! `ZReader<R: Read>` is a genuinely incremental *pull* decoder (it reads at
//! most 16 KiB of compressed input per internal step); [`CodingDecoder`] is a
//! *push* seam (wire bytes arrive a call at a time, from whatever source the
//! caller has). [`CompressCodingDecoder`] is the bridge between the two: a
//! small shared queue plays the part of `ZReader`'s inner [`Read`], fed from
//! outside as [`decode`](CodingDecoder::decode) is called, and `ZReader`'s
//! own `Read::read` reports [`io::ErrorKind::WouldBlock`] when that queue
//! runs dry but more input may still come — never a real `Ok(0)`, which is
//! reserved for the genuine end of body.
//!
//! # `Arc<Mutex<..>>`, not `Rc<RefCell<..>>`
//!
//! `ZReader<R>` owns `R` outright — there is no `get_mut`/`set_inner` to
//! reach back into it once built — so the queue has to be a second, shared
//! handle onto the *same* allocation, not a value this struct also owns by
//! value (that would be self-referential). `Rc<RefCell<_>>` is the usual
//! single-threaded shape for that, but [`CodingDecoder`] requires `Send`
//! (a [`Decoder`](crate::Decoder) is routinely moved across an executor's
//! threads), and `Rc` never is. The `Mutex` is never actually contended —
//! exactly one `&mut CompressCodingDecoder` call site ever touches it at a
//! time — so this costs an uncontended lock per `Read::read`, not real
//! synchronization.
//!
//! # Bounding the bridge
//!
//! Wire bytes are pulled into the queue [`INPUT_CHUNK`] at a time, and only
//! when `ZReader` has actually asked for more (its own `Read::read` returned
//! [`io::ErrorKind::WouldBlock`]) — never eagerly, regardless of how much
//! the caller offered in one [`decode`](CodingDecoder::decode) call.
//!
//! The **decoded** side needs more than that. `ZReader` decodes one pull of
//! compressed input to completion into a `Vec` of its own before serving the
//! first byte of it, and that `Vec` is bounded only by
//! [`oxiarc_lzw::z::ZReader::with_max_output`] — which is a *whole-stream*
//! budget (set here from [`DecodeLimits::max_output`], 64 MiB by default,
//! `u64::MAX` under [`DecodeLimits::unlimited`]). Left at `ZReader`'s own
//! 16 KiB pull, a body compressed 5000:1 therefore expands ~80 MiB inside
//! one `read` call, and peak memory tracks the *body*, not the staging
//! buffers — precisely what the rest of this crate is built not to do
//! (`tests/allocations.rs`).
//!
//! So the bridge meters the pull instead: [`BridgeHandle::read`] hands back
//! at most [`Bridge::pull_limit`] bytes per call, and the stage retunes that
//! limit after every completed fill so one fill decodes about
//! [`TARGET_FILL`] bytes:
//!
//! * the estimator is the worst *per-fill* expansion ratio measured so far
//!   (bytes handed out of `ZReader` ÷ bytes drained into it, both counted
//!   per fill), floored by the cumulative ratio — measured, never guessed,
//!   and monotone, so a body whose expansion climbs as its LZW table fills
//!   cannot re-open the meter on the strength of a calmer average;
//! * the limit starts at [`MIN_PULL`] and at most **doubles** per fill, so a
//!   stream whose ratio climbs — a benign prefix followed by a bomb — cannot
//!   ride a large pull into one huge fill: the fill that discovers the new
//!   ratio is at most twice the last one, and the pull collapses immediately
//!   after;
//! * it never exceeds [`MAX_PULL`], `ZReader`'s own internal chunk, so a
//!   low-ratio body behaves exactly as it did before this metering existed.
//!
//! [`MIN_PULL`] is two 16-bit code groups, so a pull always completes at
//! least one group and progress is guaranteed. `DecodeLimits::max_output` is
//! still handed to `ZReader` as before — the two are independent: the pull
//! limit bounds *peak* memory whatever the budget, and the budget bounds the
//! *total* whatever the pull.
//!
//! # `.Z` has no end-of-information code
//!
//! Unlike every other coding here, reaching the true end of the wire body
//! is **not**, on its own, verifiable: a `.Z` stream carries no checksum and
//! no EOI marker, so [`CodingDecoder::finish`] has nothing left to check —
//! [`decode`](CodingDecoder::decode) reaching
//! [`CodingStatus::StreamEnd`] already *is* the whole verification this
//! format offers. A response cut short by a transport bug (or an attacker)
//! decodes to a plausible, silently short prefix — `gzip -dc`/`uncompress`
//! do the same — so a caller that must detect truncation needs an
//! independent signal (`Content-Length`, a chunked terminator); see
//! [`TrailingData`](crate::TrailingData)'s docs for how the *other* codings
//! here handle it, and why this one structurally cannot.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::sync::{Arc, Mutex, PoisonError};

use oxiarc_core::traits::FlushMode;
use oxiarc_lzw::z::ZReader;

use crate::coding::ContentCoding;
use crate::decode::coding::{CodingDecoder, CodingProgress, CodingStatus, is_finish};
use crate::error::{HttpCodingError, LimitKind, Result};
use crate::limits::DecodeLimits;

/// Compressed bytes accepted from the wire per bridge top-up. Bounds how
/// much can sit un-decoded in the queue at once: `ZReader` itself only ever
/// pulls 16 KiB per internal step, so this is generous headroom, not a
/// number tuned to that internal constant.
const INPUT_CHUNK: usize = 64 * 1024;

/// Decoded bytes one `ZReader` fill should aim to produce — the same 64 KiB
/// scale as the decode chain's own staging buffers, so the `compress` stage
/// costs the pump no more than any other coding does.
const TARGET_FILL: u64 = 64 * 1024;

/// Smallest compressed pull. A `.Z` code group is eight codes, i.e. at most
/// 16 bytes at the 16-bit ceiling, so this always completes at least one
/// group: a pull can never hand `ZReader` too little to make progress.
const MIN_PULL: u64 = 32;

/// Largest compressed pull: `ZReader`'s own internal chunk size, above which
/// a bigger limit changes nothing.
const MAX_PULL: u64 = 16 * 1024;

/// The exact wording `oxiarc_lzw::LzwError::OutputLimitExceeded`'s
/// `Display` opens with (`oxiarc-lzw/src/error.rs`). Recovering the typed
/// error would need `oxiarc_lzw::z::io::to_io` to preserve the concrete
/// error rather than a formatted string, which it does not; matched here
/// the same way `decode/deflate.rs::map_error` recovers
/// `TrailingGarbage` from `oxiarc-deflate`'s own message text. If this
/// wording ever changes, [`output_limit_message_is_still_this_prefix`]
/// fails loudly rather than this silently degrading to a generic
/// [`HttpCodingError::Corrupt`].
const OUTPUT_LIMIT_PREFIX: &str = "decompressed output exceeds the ";

/// The shared state between [`CompressCodingDecoder`] and the small [`Read`]
/// handle `ZReader` pulls from.
#[derive(Debug)]
struct Bridge {
    queue: VecDeque<u8>,
    /// Set true exactly when the *caller* has handed over its last byte
    /// (`is_finish(flush)` on a `decode` call that also drained `input`).
    /// Once true and `queue` is empty, [`BridgeHandle::read`] reports a
    /// real end of stream instead of "come back later".
    no_more_input: bool,
    /// Most bytes one [`BridgeHandle::read`] may hand back — the meter that
    /// bounds how much a single `ZReader` fill can expand. Retuned by
    /// [`CompressCodingDecoder::retune_pull`]; see the module docs.
    pull_limit: u64,
    /// Compressed bytes handed to `ZReader` so far. The denominator of the
    /// expansion ratio, and how the stage detects that a fill happened.
    drained: u64,
}

impl Default for Bridge {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            no_more_input: false,
            // Start small: the very first fill has no measured ratio to go
            // on, and it is the one fill a bomb would otherwise win with.
            pull_limit: MIN_PULL,
            drained: 0,
        }
    }
}

/// The `Read` end `ZReader` owns. `CompressCodingDecoder` holds the other
/// `Arc` clone onto the same [`Bridge`] and pushes into it; two independent
/// handles onto one heap allocation, not a self-referential borrow.
#[derive(Debug)]
struct BridgeHandle(Arc<Mutex<Bridge>>);

fn lock(bridge: &Mutex<Bridge>) -> std::sync::MutexGuard<'_, Bridge> {
    // Never actually contended (see the module docs); recovering from
    // poisoning rather than propagating it is the same choice this crate
    // makes nowhere else only because nothing else here uses a `Mutex` —
    // there is no unverified data to distrust in a poisoned `Bridge`, only
    // whatever bytes were already queued.
    bridge.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Read for BridgeHandle {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut bridge = lock(&self.0);
        if bridge.queue.is_empty() {
            return if bridge.no_more_input {
                Ok(0)
            } else {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            };
        }
        // `pull_limit` is never below `MIN_PULL`, so this can only be zero
        // when `out` itself is — which `ZReader` never does.
        let limit = usize::try_from(bridge.pull_limit).unwrap_or(usize::MAX);
        let take = out.len().min(bridge.queue.len()).min(limit);
        for (slot, byte) in out.iter_mut().zip(bridge.queue.drain(..take)) {
            *slot = byte;
        }
        bridge.drained += take as u64;
        Ok(take)
    }
}

/// The `compress` / `x-compress` stage.
pub(crate) struct CompressCodingDecoder {
    bridge: Arc<Mutex<Bridge>>,
    reader: ZReader<BridgeHandle>,
    max_output: u64,
    /// Bytes handed *out* of `ZReader` so far — the numerator of the
    /// expansion ratio; see [`Self::retune_pull`].
    served: u64,
    /// `Bridge::drained` as of the last fill boundary, so a growing
    /// `drained` is how the next fill is detected.
    drained_at_last_fill: u64,
    /// `served` as of the last fill boundary. Its distance from `served` at
    /// the *next* boundary is exactly the intervening fill's decoded size.
    served_at_last_fill: u64,
    /// Compressed bytes the most recently started fill pulled in, held until
    /// its decoded size is known at the next boundary.
    last_fill_in: u64,
    /// The largest expansion ratio any single completed fill has shown.
    ///
    /// The *cumulative* ratio is not enough on its own: a `.Z` stream's
    /// expansion climbs as its table fills, so an average taken over the
    /// whole stream so far badly understates what the next fill will do —
    /// measured, that let one fill of a 4992:1 body reach ~950 KiB where the
    /// per-fill figure holds it near [`TARGET_FILL`]. This is monotone, so
    /// once a fill has expanded sharply the meter never re-opens on the
    /// strength of a calmer average.
    worst_fill_ratio: u64,
    /// Mirror of [`Bridge::pull_limit`], kept here so retuning does not need
    /// to read it back under the lock.
    pull_limit: u64,
}

impl std::fmt::Debug for CompressCodingDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompressCodingDecoder")
            .field("header", &self.reader.header())
            .finish_non_exhaustive()
    }
}

impl CompressCodingDecoder {
    /// A `compress` stage bounded by `limits`.
    pub(crate) fn new(limits: &DecodeLimits) -> Self {
        let bridge = Arc::new(Mutex::new(Bridge::default()));
        let reader =
            ZReader::new(BridgeHandle(Arc::clone(&bridge))).with_max_output(limits.max_output);
        Self {
            bridge,
            reader,
            max_output: limits.max_output,
            served: 0,
            drained_at_last_fill: 0,
            served_at_last_fill: 0,
            last_fill_in: 0,
            worst_fill_ratio: 1,
            pull_limit: MIN_PULL,
        }
    }

    /// Push `bytes` into the bridge queue and set whether more may follow.
    fn push(&self, bytes: &[u8], no_more_input: bool) {
        let mut bridge = lock(&self.bridge);
        bridge.queue.extend(bytes);
        bridge.no_more_input = no_more_input;
    }

    /// Set "no more input is ever coming" without pushing any new bytes —
    /// used only once `input` is already exhausted for this call.
    fn mark_no_more_input(&self) {
        lock(&self.bridge).no_more_input = true;
    }

    /// Retune [`Bridge::pull_limit`] if a `ZReader` fill has completed since
    /// the last call, so the *next* fill decodes about [`TARGET_FILL`] bytes.
    ///
    /// Called immediately before `self.served` is credited with the bytes a
    /// `read` just returned, so at the moment the ratio is computed `served`
    /// covers exactly the fills that are already finished and `drained`
    /// covers exactly the input they consumed — no half-counted fill. See
    /// the module docs for why the limit only ever doubles.
    fn retune_pull(&mut self) {
        let drained = lock(&self.bridge).drained;
        if drained <= self.drained_at_last_fill {
            return; // No new fill: leave the meter where it is.
        }
        // The fill that started at the previous boundary has now finished
        // handing out its bytes, so its ratio is finally exact.
        //
        // `>= MIN_PULL`, not `> 0`: a caller feeding tiny chunks can starve
        // the queue so that a fill runs on only a handful of bytes, and
        // dividing a whole fill's output by a fraction of a pull would
        // record a ratio that says more about the caller's chunking than
        // about the body. `worst_fill_ratio` is monotone, so one such
        // reading would pin the meter shut for the rest of the stream — a
        // throughput cliff for a caller that later switches to large
        // chunks. Skipping those readings costs nothing in safety: a fill
        // that ran on under `MIN_PULL` bytes cannot expand further than one
        // that ran on `MIN_PULL`, and the cumulative ratio below still
        // covers every byte either way.
        if self.last_fill_in >= MIN_PULL {
            let out = self.served - self.served_at_last_fill;
            self.worst_fill_ratio = self
                .worst_fill_ratio
                .max(out.div_ceil(self.last_fill_in).max(1));
        }
        self.last_fill_in = drained - self.drained_at_last_fill;
        self.drained_at_last_fill = drained;
        self.served_at_last_fill = self.served;

        let cumulative = self.served.div_ceil(drained).max(1);
        let ratio = self.worst_fill_ratio.max(cumulative);
        let target = (TARGET_FILL / ratio).max(MIN_PULL);
        let doubled = self.pull_limit.saturating_mul(2);
        self.pull_limit = target.min(doubled).clamp(MIN_PULL, MAX_PULL);
        lock(&self.bridge).pull_limit = self.pull_limit;
    }

    /// Translate an [`io::Error`] surfaced by [`ZReader::read`].
    fn map_io_error(&self, error: io::Error) -> HttpCodingError {
        if error.kind() == io::ErrorKind::InvalidData {
            let message = error.to_string();
            if let Some(rest) = message.strip_prefix(OUTPUT_LIMIT_PREFIX) {
                // `rest` reads e.g. "1048576-byte limit", which is
                // `self.max_output` restated; there is nothing to parse back
                // out, so this only confirms the shape matched.
                let _ = rest;
                // `ZReader` refuses the chunk that *would* cross the budget,
                // so it never says how far past it the body actually goes —
                // only that it does. Report `max_output + 1`, the same
                // "at least this much" convention `LimitedSink::overflow`
                // uses, rather than inventing a figure: claiming exactly
                // `max_output` would assert the body stopped precisely at
                // the limit, which is the one thing known to be false.
                return HttpCodingError::LimitExceeded {
                    limit: self.max_output as f64,
                    kind: LimitKind::Output {
                        produced: self.max_output.saturating_add(1),
                    },
                };
            }
        }
        HttpCodingError::Corrupt {
            coding: ContentCoding::Compress,
            source: Box::new(error),
        }
    }
}

impl CodingDecoder for CompressCodingDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<CodingProgress> {
        let mut consumed = 0usize;
        let mut produced = 0usize;
        let last_call = is_finish(flush);
        // Re-entry guard for the "mark, then retry" step below: with the
        // queue provably empty (see `BridgeHandle::read`), the very next
        // read after marking `no_more_input` can only be `Ok(0)`, never
        // another `WouldBlock` — but a hang from a wrong assumption here
        // would be exactly HTTP-verify's F-1 class of defect, so this stays
        // a hard error instead of an unguarded loop-back.
        let mut marked_this_call = false;

        loop {
            if produced == output.len() {
                return Ok(CodingProgress {
                    consumed,
                    produced,
                    status: CodingStatus::NeedOutput,
                });
            }
            match self.reader.read(&mut output[produced..]) {
                Ok(0) => {
                    return Ok(CodingProgress {
                        consumed,
                        produced,
                        status: CodingStatus::StreamEnd,
                    });
                }
                Ok(n) => {
                    // Order matters: retune first, while `served` still
                    // describes only completed fills, then credit `n` to the
                    // fill it actually came from.
                    self.retune_pull();
                    self.served += n as u64;
                    produced += n;
                    continue;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // Falls through to feed more input below.
                }
                Err(error) => return Err(self.map_io_error(error)),
            }

            let remaining = &input[consumed..];
            if remaining.is_empty() {
                if !last_call {
                    return Ok(CodingProgress {
                        consumed,
                        produced,
                        status: CodingStatus::NeedInput,
                    });
                }
                if marked_this_call {
                    return Err(HttpCodingError::Corrupt {
                        coding: ContentCoding::Compress,
                        source: Box::new(oxiarc_core::OxiArcError::corrupted(
                            consumed as u64,
                            "compress decoder made no progress after end of input",
                        )),
                    });
                }
                self.mark_no_more_input();
                marked_this_call = true;
                continue;
            }
            let take = remaining.len().min(INPUT_CHUNK);
            self.push(&remaining[..take], false);
            consumed += take;
            marked_this_call = false;
        }
    }

    fn finish(&mut self) -> Result<()> {
        // See the module docs: `.Z` has no checksum or end-of-information
        // code, so reaching `CodingStatus::StreamEnd` from `decode` already
        // is the whole verification this format offers. Nothing left to do.
        Ok(())
    }

    fn coding(&self) -> &ContentCoding {
        static COMPRESS: ContentCoding = ContentCoding::Compress;
        &COMPRESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(decoder: &mut CompressCodingDecoder, body: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut scratch = [0u8; 64];
        let mut pos = 0usize;
        loop {
            let progress = decoder.decode(&body[pos..], &mut scratch, FlushMode::Finish)?;
            pos += progress.consumed;
            out.extend_from_slice(&scratch[..progress.produced]);
            match progress.status {
                CodingStatus::StreamEnd => break,
                CodingStatus::NeedOutput => continue,
                CodingStatus::NeedInput => {
                    if progress.consumed == 0 && progress.produced == 0 {
                        break;
                    }
                }
            }
        }
        decoder.finish()?;
        Ok(out)
    }

    #[test]
    fn output_limit_message_is_still_this_prefix() {
        let wire =
            oxiarc_lzw::z::compress(b"at least one byte of real output", 16).expect("compress");
        let err = oxiarc_lzw::z::decompress_with_limit(&wire, 0)
            .expect_err("a zero-byte limit must be refused when the stream needs any output");
        assert!(
            err.to_string().starts_with(OUTPUT_LIMIT_PREFIX),
            "oxiarc-lzw's OutputLimitExceeded wording changed to {err:?}; update \
             OUTPUT_LIMIT_PREFIX in decode/compress.rs to match"
        );
    }

    #[test]
    fn compress_stage_round_trips() {
        let body = b"compress over http, decoded incrementally, bounded by a real budget".repeat(4);
        let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");
        let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
        assert_eq!(drive(&mut d, &wire).expect("decode"), body);
        assert_eq!(d.coding(), &ContentCoding::Compress);
    }

    #[test]
    fn byte_at_a_time_feeding_matches_the_whole_body() {
        let body = b"chunk invariance matters here as much as anywhere else in this crate, \
                      arguably more so given the bridge in the middle"
            .repeat(3);
        let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");

        let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
        let mut out = Vec::new();
        let mut scratch = [0u8; 4096];
        for (index, byte) in wire.iter().enumerate() {
            let flush = if index + 1 == wire.len() {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            loop {
                let progress = d
                    .decode(std::slice::from_ref(byte), &mut scratch, flush)
                    .expect("decode");
                out.extend_from_slice(&scratch[..progress.produced]);
                if progress.consumed > 0 || progress.status != CodingStatus::NeedInput {
                    break;
                }
            }
        }
        d.finish().expect("finish");
        assert_eq!(out, body);
    }

    #[test]
    fn chunk_invariance_holds_at_several_granularities() {
        let body = b"the same body, fed in different-sized pieces, must decode to the exact \
                      same bytes every time no matter where the wire happened to split"
            .repeat(5);
        let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");

        let mut reference: Option<Vec<u8>> = None;
        for chunk_size in [1usize, 2, 3, 7, 64, 4096, wire.len()] {
            let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
            let mut out = Vec::new();
            let mut scratch = [0u8; 8192];
            let mut pos = 0usize;
            while pos < wire.len() {
                let end = (pos + chunk_size).min(wire.len());
                let flush = if end == wire.len() {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };
                loop {
                    let progress = d
                        .decode(&wire[pos..end], &mut scratch, flush)
                        .expect("decode");
                    out.extend_from_slice(&scratch[..progress.produced]);
                    pos += progress.consumed;
                    if progress.status != CodingStatus::NeedOutput {
                        break;
                    }
                }
            }
            d.finish().expect("finish");
            match &reference {
                None => reference = Some(out),
                Some(expected) => {
                    assert_eq!(&out, expected, "chunk_size={chunk_size}: output diverged");
                }
            }
        }
        assert_eq!(reference.as_deref(), Some(&body[..]));
    }

    #[test]
    fn an_empty_body_is_an_error() {
        let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
        drive(&mut d, b"").expect_err("an empty body is not a valid .Z stream");
    }

    #[test]
    fn a_header_only_body_decodes_to_empty_output() {
        // `.Z` has no EOI code: a bare 3-byte header with nothing after it
        // is a *complete*, valid, empty stream — not truncated, unlike an
        // entirely empty body (which never even names its own format).
        let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
        assert_eq!(
            drive(&mut d, &[0x1F, 0x9D, 0x90]).expect("a bare header is a valid empty stream"),
            b""
        );
    }

    #[test]
    fn truncation_is_not_an_error_by_design() {
        // The format-level contract this module's docs describe: a cut-off
        // body still decodes, to whatever prefix the bytes that arrived
        // produce.
        let body = b"the quick brown fox jumps over the lazy dog. ".repeat(64);
        let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");
        let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
        let decoded =
            drive(&mut d, &wire[..wire.len() / 2]).expect("a truncated .Z body is not an error");
        assert!(!decoded.is_empty());
        assert!(body.starts_with(&decoded));
    }

    #[test]
    fn the_output_cap_is_enforced_during_decoding() {
        let body = vec![0u8; 4 * 1024 * 1024];
        let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");
        let limits = DecodeLimits::default().with_max_output(1024);
        let mut d = CompressCodingDecoder::new(&limits);
        let err = drive(&mut d, &wire).expect_err("the cap must fire before full expansion");
        assert!(matches!(
            err,
            HttpCodingError::LimitExceeded {
                kind: LimitKind::Output { .. },
                ..
            }
        ));
    }

    /// A cheap, deterministic PRNG stream — real xorshift, not a
    /// `rand`-crate dependency — so this test's input resists LZW's own
    /// compression well enough that the wire is comfortably wider than
    /// [`INPUT_CHUNK`]; a highly repetitive body would compress the whole
    /// 512 KiB down to a few hundred bytes and make the assertion below
    /// vacuously true.
    fn xorshift_bytes(len: usize) -> Vec<u8> {
        let mut state: u32 = 0x2545_F491;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Pump `body` through `decoder` until it stops making progress,
    /// discarding the output. Bounded so a stalled decoder fails the test
    /// instead of hanging it.
    fn drain(decoder: &mut CompressCodingDecoder, body: &[u8]) {
        let mut scratch = [0u8; 8192];
        let mut pos = 0usize;
        for _ in 0..1_000_000 {
            let progress = decoder
                .decode(&body[pos..], &mut scratch, FlushMode::Finish)
                .expect("decode");
            pos += progress.consumed;
            if progress.status == CodingStatus::StreamEnd {
                return;
            }
            if progress.consumed == 0 && progress.produced == 0 {
                return;
            }
        }
        panic!("the compress stage made no progress within a million calls");
    }

    #[test]
    fn the_pull_meter_closes_on_a_high_ratio_body() {
        // The regression guard for this stage's peak-memory bound: one
        // `ZReader` fill decodes a whole pull of compressed input into a
        // `Vec` of its own before serving any of it, so on a body that
        // expands 1000:1 the meter has to drive the pull right down or peak
        // memory tracks the *body* (measured in `tests/allocations.rs`).
        // `unlimited()` deliberately removes `max_output`, so the meter is
        // the only thing left holding the line.
        let bomb = oxiarc_lzw::z::compress(&vec![0u8; 8 * 1024 * 1024], 16).expect("compress");
        let mut d = CompressCodingDecoder::new(&DecodeLimits::unlimited());
        drain(&mut d, &bomb);
        assert!(
            d.pull_limit <= 256,
            "a ~1400:1 body left the pull meter at {} bytes; one fill would \
             then decode about {} bytes",
            d.pull_limit,
            d.pull_limit * 1400
        );
        assert!(d.pull_limit >= MIN_PULL, "the meter must never stall");
    }

    #[test]
    fn the_pull_meter_opens_fully_on_a_low_ratio_body() {
        // The other half: metering must not cost throughput on a body that
        // does not expand. Pseudo-random bytes barely compress at all, so
        // the meter has to walk all the way back up to `MAX_PULL` —
        // `ZReader`'s own internal chunk, i.e. exactly the behaviour it had
        // before the meter existed.
        let noisy = oxiarc_lzw::z::compress(&xorshift_bytes(512 * 1024), 16).expect("compress");
        let mut d = CompressCodingDecoder::new(&DecodeLimits::unlimited());
        drain(&mut d, &noisy);
        assert_eq!(
            d.pull_limit, MAX_PULL,
            "a barely-compressible body left the pull meter at {} bytes",
            d.pull_limit
        );
    }

    #[test]
    fn the_pull_meter_never_stalls_on_the_narrowest_code_width() {
        // `MIN_PULL` has to clear one whole code group at every width, or a
        // pull could hand `ZReader` too little to decode a single code and
        // the stage would spin. Nine bits is the narrowest `.Z` width.
        let body = b"a narrow-code stream, decoded through the smallest pull".repeat(8);
        let wire = oxiarc_lzw::z::compress(&body, 9).expect("compress");
        let mut d = CompressCodingDecoder::new(&DecodeLimits::unlimited());
        assert_eq!(drive(&mut d, &wire).expect("decode"), body);
    }

    #[test]
    fn the_pull_meter_never_changes_the_decoded_bytes() {
        // The meter's whole job is to change *when* `ZReader` is fed, and
        // the sequence of pulls it produces depends on how the caller
        // chunks the wire and how much output room it offers. So sweep both
        // axes, over three bodies with very different expansion ratios, and
        // require byte-identical output every time. A meter that dropped or
        // duplicated a pull would show up here and nowhere else.
        let bodies: [Vec<u8>; 3] = [
            b"ordinary text, moderately compressible, repeated a good many times ".repeat(200),
            vec![b'z'; 400_000],        // extreme ratio: the meter closes hard
            xorshift_bytes(120 * 1024), // no ratio at all: the meter opens fully
        ];
        for body in bodies {
            let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");
            for input_chunk in [1usize, 5, 31, 32, 33, 1024, 65_536, wire.len()] {
                for out_len in [1usize, 3, 32, 4096, 70_000] {
                    let mut d = CompressCodingDecoder::new(&DecodeLimits::unlimited());
                    let mut scratch = vec![0u8; out_len];
                    let mut out = Vec::new();
                    let mut pos = 0usize;
                    let mut calls = 0usize;
                    loop {
                        calls += 1;
                        assert!(
                            calls < 5_000_000,
                            "input_chunk={input_chunk} out_len={out_len}: no progress"
                        );
                        let end = (pos + input_chunk).min(wire.len());
                        let flush = if end == wire.len() {
                            FlushMode::Finish
                        } else {
                            FlushMode::None
                        };
                        let progress = d
                            .decode(&wire[pos..end], &mut scratch, flush)
                            .expect("decode");
                        pos += progress.consumed;
                        out.extend_from_slice(&scratch[..progress.produced]);
                        match progress.status {
                            CodingStatus::StreamEnd => break,
                            CodingStatus::NeedOutput => continue,
                            CodingStatus::NeedInput => {
                                if progress.consumed == 0
                                    && progress.produced == 0
                                    && pos == wire.len()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    d.finish().expect("finish");
                    assert_eq!(
                        out.len(),
                        body.len(),
                        "input_chunk={input_chunk} out_len={out_len}: short output"
                    );
                    assert_eq!(
                        out, body,
                        "input_chunk={input_chunk} out_len={out_len}: output diverged"
                    );
                }
            }
        }
    }

    #[test]
    fn a_wide_input_slice_never_holds_more_than_one_chunk_at_once() {
        // Regression guard for the bridge's own bomb surface: feeding a
        // huge slice in one `decode` call, with only a tiny output room,
        // must not buffer the whole thing — `consumed` per call should stay
        // near `INPUT_CHUNK`, not jump to the caller's entire input length.
        let body = xorshift_bytes(512 * 1024);
        let wire = oxiarc_lzw::z::compress(&body, 16).expect("compress");
        assert!(
            wire.len() > INPUT_CHUNK,
            "this test needs a wire body wider than one chunk to mean anything; \
             got only {} bytes",
            wire.len()
        );
        let mut d = CompressCodingDecoder::new(&DecodeLimits::default());
        let mut tiny_out = [0u8; 16];
        let progress = d
            .decode(&wire, &mut tiny_out, FlushMode::None)
            .expect("decode");
        assert!(
            progress.consumed <= INPUT_CHUNK,
            "consumed {} bytes of a {} wide slice against a 16-byte output room",
            progress.consumed,
            wire.len()
        );
    }
}

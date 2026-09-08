//! `Content-Encoding: dcb` — Compression Dictionary Transport (RFC 9842),
//! Brotli variant — driven through `oxiarc_brotli`'s shared-dictionary
//! support.
//!
//! A `dcb` body is `oxiarc_brotli::dcb`'s 36-byte preamble (4-byte magic
//! `FF 44 43 42` + a 32-byte SHA-256 of the dictionary) followed by an
//! ordinary shared-dictionary Brotli stream. This stage buffers exactly
//! that preamble across as many `decode` calls as it takes to arrive,
//! verifies it against the caller-supplied dictionary
//! (`oxiarc_brotli::verify_dcb_header`, which hashes via
//! `oxiarc_core::sha256` under the hood), and — only once the digest
//! checks out — hands everything after it to a
//! [`BrotliStream::with_dictionary`], unchanged from that point on. A body
//! naming a dictionary other than the one supplied is rejected before a
//! single Brotli byte is decoded, never silently decoded against the wrong
//! bytes.
//!
//! # Why this needs its own stage, unlike `br`
//!
//! RFC 7932 Brotli has no field to carry a dictionary identifier — that is
//! exactly what the 36-byte preamble supplies at the HTTP-coding layer, so
//! `dcb` cannot simply be "`br` plus a dictionary" the way `dcz` layers a
//! dictionary onto `zstd`'s own `Dictionary_ID`. Everything below the
//! preamble is a plain [`BrotliStream`], so this module owns exactly one
//! thing beyond `decode/brotli.rs`: buffering and verifying those 36 bytes.

use std::fmt;

use oxiarc_brotli::{BrotliStatus, BrotliStream};
use oxiarc_core::traits::FlushMode;

use super::brotli::map_error_for;
use crate::coding::ContentCoding;
use crate::decode::coding::{CodingDecoder, CodingProgress, CodingStatus, is_finish};
use crate::error::{HttpCodingError, Result};
use crate::limits::DecodeLimits;

/// `4 (magic) + 32 (SHA-256)`, restated so the buffering loop below does not
/// depend on reading `oxiarc_brotli::dcb::DCB_HEADER_LEN`'s value out of a
/// non-`const`-friendly re-export path; it is asserted equal to it in tests.
const HEADER_LEN: usize = 36;

/// [`super::brotli::map_error_for`] bound to `dcb`.
///
/// Every error this stage can surface — a short body, a bad magic, a
/// dictionary-digest mismatch, a corrupt meta-block — comes back from
/// `oxiarc_brotli` as a [`BrotliError`](oxiarc_brotli::BrotliError), and the
/// plain `br` stage's mapper would label all of them
/// `Corrupt { coding: Brotli }`. `dcb` is a *different* content coding, and
/// `HttpCodingError::Corrupt::coding` is the public field a caller reads to
/// learn which coding in the chain failed (and what `Display` prints), so it
/// has to say `dcb` — matching how `DczCodingDecoder` reports `dcz` rather
/// than `zstd`.
fn map_error(error: oxiarc_brotli::BrotliError) -> HttpCodingError {
    map_error_for(&ContentCoding::Dcb, error)
}

/// Where a [`DcbCodingDecoder`] is in its two phases.
enum State {
    /// Accumulating the 36-byte preamble; `Vec::len()` is always `< HEADER_LEN`
    /// here — the instant it reaches `HEADER_LEN` the state becomes `Stream`.
    Header(Vec<u8>),
    /// Preamble verified; everything from here on is shared-dictionary
    /// Brotli, decoded exactly as the plain `br` stage decodes `BrotliStream`.
    /// Boxed: `BrotliStream` is hundreds of bytes, next to a 24-byte `Vec` in
    /// the other arm (`clippy::large_enum_variant`).
    Stream(Box<BrotliStream>),
}

/// The `dcb` stage (RFC 9842, Brotli variant).
///
/// Only buildable with a dictionary in hand — see
/// [`Decoder::with_dictionary`](crate::Decoder::with_dictionary) — so unlike
/// every other stage this one never exists in a "waiting for its dictionary"
/// state; the dictionary is fixed at construction and the 36-byte preamble
/// is verified against exactly that dictionary the moment it arrives.
pub(crate) struct DcbCodingDecoder {
    /// The dictionary this body must name. Moved into the inner
    /// [`BrotliStream`] once the preamble verifies (`with_dictionary` takes
    /// ownership), so this is `None` for the rest of the stream's life.
    dictionary: Option<Vec<u8>>,
    max_output: u64,
    state: State,
}

impl fmt::Debug for DcbCodingDecoder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DcbCodingDecoder")
            .field(
                "phase",
                &match &self.state {
                    State::Header(buf) => format!("header({}/{HEADER_LEN})", buf.len()),
                    State::Stream(_) => "stream".to_string(),
                },
            )
            .finish_non_exhaustive()
    }
}

impl DcbCodingDecoder {
    /// A `dcb` stage bounded by `limits`, verified against `dictionary`.
    pub(crate) fn new(limits: &DecodeLimits, dictionary: Vec<u8>) -> Self {
        Self {
            dictionary: Some(dictionary),
            max_output: limits.max_output,
            state: State::Header(Vec::with_capacity(HEADER_LEN)),
        }
    }

    /// The short-body error shared by an early `Finish` and a `finish()`
    /// reached while the preamble never completed.
    fn short_body_error(len: usize) -> HttpCodingError {
        map_error(oxiarc_brotli::BrotliError::CorruptedData(format!(
            "dcb body of {len} bytes is shorter than its {HEADER_LEN}-byte header"
        )))
    }
}

impl CodingDecoder for DcbCodingDecoder {
    fn decode(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<CodingProgress> {
        let mut consumed = 0usize;
        if let State::Header(buf) = &mut self.state {
            let need = HEADER_LEN - buf.len();
            let take = need.min(input.len());
            buf.extend_from_slice(&input[..take]);
            consumed += take;
            if buf.len() < HEADER_LEN {
                return if is_finish(flush) {
                    Err(Self::short_body_error(buf.len()))
                } else {
                    Ok(CodingProgress {
                        consumed,
                        produced: 0,
                        status: CodingStatus::NeedInput,
                    })
                };
            }
            // `self.dictionary` is `Some` for exactly as long as `self.state`
            // is `Header` — see the field doc — so this is always populated
            // here; `take_or_default` is used only to move it out without an
            // `unwrap()`.
            let dictionary = self.dictionary.take().unwrap_or_default();
            oxiarc_brotli::verify_dcb_header(buf, &dictionary).map_err(map_error)?;
            self.state = State::Stream(Box::new(
                BrotliStream::new()
                    .with_dictionary(dictionary)
                    .with_max_output(self.max_output),
            ));
        }

        // Reachable only once the block above has just set `Stream`, or on
        // a later call that starts in it already; the `Header` arm above
        // always either returns or transitions, so this is never the stale
        // `Header` case.
        let State::Stream(inner) = &mut self.state else {
            return Ok(CodingProgress {
                consumed,
                produced: 0,
                status: CodingStatus::NeedInput,
            });
        };
        let flush = if is_finish(flush) {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = inner
            .decode(&input[consumed..], output, flush)
            .map_err(map_error)?;
        consumed += progress.consumed;
        let status = match progress.status {
            BrotliStatus::NeedInput => CodingStatus::NeedInput,
            BrotliStatus::NeedOutput => CodingStatus::NeedOutput,
            BrotliStatus::StreamEnd => CodingStatus::StreamEnd,
        };
        Ok(CodingProgress {
            consumed,
            produced: progress.produced,
            status,
        })
    }

    fn finish(&mut self) -> Result<()> {
        match &mut self.state {
            State::Stream(inner) => inner.finish().map_err(map_error),
            State::Header(buf) => Err(Self::short_body_error(buf.len())),
        }
    }

    fn coding(&self) -> &ContentCoding {
        static DCB: ContentCoding = ContentCoding::Dcb;
        &DCB
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(decoder: &mut DcbCodingDecoder, body: &[u8]) -> Result<Vec<u8>> {
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

    fn params() -> oxiarc_brotli::BrotliParams {
        oxiarc_brotli::BrotliParams {
            quality: 5,
            ..oxiarc_brotli::BrotliParams::default()
        }
    }

    #[test]
    fn header_len_matches_the_brotli_crate_constant() {
        assert_eq!(HEADER_LEN, oxiarc_brotli::DCB_HEADER_LEN);
    }

    #[test]
    fn dcb_stage_round_trips() {
        let dictionary = b"a shared dictionary both ends already have, repeated".repeat(16);
        let body = b"dcb over http, decoded incrementally, against a shared dictionary";
        let wire = oxiarc_brotli::compress_dcb(body, &dictionary, &params()).expect("compress");

        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), dictionary);
        assert_eq!(drive(&mut d, &wire).expect("decode"), body);
        assert_eq!(d.coding(), &ContentCoding::Dcb);
    }

    #[test]
    fn byte_at_a_time_feeding_matches_the_whole_body() {
        let dictionary = b"byte at a time over the header boundary too".repeat(20);
        let body = b"a body long enough that the header and the stream both split awkwardly \
                     across a one-byte-at-a-time feed";
        let wire = oxiarc_brotli::compress_dcb(body, &dictionary, &params()).expect("compress");

        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), dictionary);
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
    fn wrong_dictionary_is_rejected_before_any_byte_is_decoded() {
        let dictionary = b"the real dictionary".repeat(8);
        let wrong = b"a completely different dictionary".repeat(8);
        let body = b"secret payload";
        let wire = oxiarc_brotli::compress_dcb(body, &dictionary, &params()).expect("compress");

        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), wrong);
        let err = drive(&mut d, &wire).expect_err("a wrong dictionary must be refused");
        assert!(matches!(err, HttpCodingError::Corrupt { .. }));
    }

    #[test]
    fn a_body_shorter_than_the_header_is_an_error() {
        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), b"dictionary".to_vec());
        drive(&mut d, &[0xFFu8; 10]).expect_err("10 bytes is short of the 36-byte header");
    }

    #[test]
    fn an_empty_body_is_an_error() {
        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), b"dictionary".to_vec());
        drive(&mut d, b"").expect_err("an empty body is not a valid dcb stream");
    }

    #[test]
    fn truncation_inside_the_brotli_stream_is_an_error() {
        let dictionary = b"a shared dictionary, repeated enough to matter".repeat(16);
        let body = vec![b'q'; 8192];
        let wire = oxiarc_brotli::compress_dcb(&body, &dictionary, &params()).expect("compress");

        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), dictionary);
        drive(&mut d, &wire[..wire.len() - 2])
            .expect_err("a truncated dcb stream must be an error");
    }

    #[test]
    fn a_body_that_is_exactly_the_header_with_no_stream_is_an_error() {
        // The exact boundary `a_body_shorter_than_the_header_is_an_error`
        // (10 bytes) and `truncation_inside_the_brotli_stream_is_an_error`
        // (`len - 2`) leave unpinned: precisely 36 bytes, a complete and
        // *valid* preamble naming the right dictionary, with nothing after
        // it. The `dcz` twin of this case
        // (`decode::zstd::tests::a_body_that_is_exactly_the_preamble_with_no_frame_is_an_error`)
        // needed an explicit guard because `ZstdStream` reads zero frames as
        // a clean empty stream; this one must be an error too, and for the
        // same reason — an empty body under a real content coding is
        // truncated, not empty.
        let dictionary = b"a shared dictionary both ends already have".repeat(8);
        let wire = oxiarc_brotli::write_dcb_header(&dictionary).to_vec();
        assert_eq!(wire.len(), HEADER_LEN);

        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), dictionary);
        drive(&mut d, &wire).expect_err(
            "a dcb body that is only the header, with no Brotli stream after it, must be an error",
        );
    }

    #[test]
    fn every_error_names_dcb_and_never_plain_br() {
        // `HttpCodingError::Corrupt::coding` is public: a caller reads it to
        // learn which coding in the chain failed, and `Display` prints it.
        // This stage shares `decode/brotli.rs`'s error mapper, which used to
        // hard-wire `ContentCoding::Brotli` — so every `dcb` failure was
        // reported against `br`, a coding the response never named.
        let dictionary = b"the real dictionary".repeat(8);
        let wrong = b"a completely different dictionary".repeat(8);
        let body = b"a body long enough to have a payload to corrupt".repeat(4);
        let wire = oxiarc_brotli::compress_dcb(&body, &dictionary, &params()).expect("compress");

        let mut short = vec![0xFFu8; 10];
        short[..4].copy_from_slice(&oxiarc_brotli::DCB_MAGIC);
        let mut bad_magic = wire.clone();
        bad_magic[3] ^= 0xFF;
        let mut bad_payload = wire.clone();
        let last = bad_payload.len() - 1;
        bad_payload[last] ^= 0xFF;

        let cases: [(&str, &[u8], Vec<u8>); 4] = [
            ("short body", &short, dictionary.clone()),
            ("bad magic", &bad_magic, dictionary.clone()),
            ("wrong dictionary", &wire, wrong),
            ("corrupt payload", &bad_payload, dictionary.clone()),
        ];
        for (label, wire, dict) in cases {
            let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), dict);
            let err = drive(&mut d, wire).expect_err(label);
            match err {
                HttpCodingError::Corrupt { coding, .. } => assert_eq!(
                    coding,
                    ContentCoding::Dcb,
                    "{label}: reported against {coding}, not dcb"
                ),
                other => panic!("{label}: expected Corrupt, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_bad_magic_is_rejected_with_the_header_fully_buffered() {
        let mut body = vec![0u8; HEADER_LEN];
        body[..4].copy_from_slice(&[0xFF, 0x44, 0x43, 0x00]); // wrong 4th byte
        let mut d = DcbCodingDecoder::new(&DecodeLimits::default(), b"dictionary".to_vec());
        drive(&mut d, &body).expect_err("a bad magic must be rejected");
    }
}

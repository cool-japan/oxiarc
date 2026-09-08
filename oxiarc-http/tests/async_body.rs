//! `AsyncDecodedBody` over a real async source (feature `async-io`).
//!
//! The interesting property is not "it decodes" — the sync adapter already
//! proves the chain works — it is that a `Poll::Pending` source never turns
//! into an empty body, and that a source which arrives in small, delayed
//! pieces produces exactly the same bytes as one that arrives all at once.

#![cfg(feature = "async-io")]

mod common;

use std::pin::Pin;
use std::task::{Context, Poll};

use oxiarc_http::{ContentCoding, DecodeLimits, DecodedBody, Decoder, TrailingData};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

/// A source that yields `step` bytes, then `Poll::Pending` (waking itself),
/// then the next `step` bytes — the shape a socket actually has.
struct Stuttering {
    data: Vec<u8>,
    pos: usize,
    step: usize,
    pending_next: bool,
}

impl Stuttering {
    fn new(data: Vec<u8>, step: usize) -> Self {
        Self {
            data,
            pos: 0,
            step,
            pending_next: true,
        }
    }
}

impl AsyncRead for Stuttering {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pending_next {
            self.pending_next = false;
            // Wake immediately: this models a socket that had nothing ready
            // this instant but will have something in a moment.
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.pending_next = true;
        let remaining = self.data.len() - self.pos;
        let n = self.step.min(buf.remaining()).min(remaining);
        let (pos, step) = (self.pos, n);
        buf.put_slice(&self.data[pos..pos + step]);
        self.pos += n;
        Poll::Ready(Ok(()))
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn encode(coding: &ContentCoding, plain: &[u8]) -> Vec<u8> {
    match coding {
        ContentCoding::Gzip => oxiarc_deflate::gzip_compress(plain, 6).expect("gzip"),
        ContentCoding::Deflate => oxiarc_deflate::zlib_compress(plain, 6).expect("zlib"),
        #[cfg(feature = "brotli")]
        ContentCoding::Brotli => oxiarc_brotli::compress(plain, 4).expect("brotli"),
        #[cfg(feature = "zstd")]
        ContentCoding::Zstd => oxiarc_zstd::compress(plain).expect("zstd"),
        #[cfg(feature = "compress")]
        ContentCoding::Compress => oxiarc_lzw::z::compress(plain, 16).expect("compress"),
        ContentCoding::Identity => plain.to_vec(),
        other => panic!("no encoder wired for {other}"),
    }
}

fn codings() -> Vec<ContentCoding> {
    [
        ContentCoding::Identity,
        ContentCoding::Gzip,
        ContentCoding::Deflate,
        ContentCoding::Brotli,
        ContentCoding::Zstd,
        ContentCoding::Compress,
    ]
    .into_iter()
    .filter(ContentCoding::is_decodable)
    .collect()
}

#[test]
fn a_pending_stuttering_source_decodes_to_the_same_bytes() {
    let plain = common::json(300_000);
    runtime().block_on(async {
        for coding in codings() {
            let wire = encode(&coding, &plain);
            for step in [1usize, 3, 4096] {
                let mut body = oxiarc_http::AsyncDecodedBody::with_codings(
                    Stuttering::new(wire.clone(), step),
                    std::slice::from_ref(&coding),
                    &DecodeLimits::default(),
                )
                .expect("decoder");
                let mut out = Vec::new();
                body.read_to_end(&mut out)
                    .await
                    .unwrap_or_else(|e| panic!("{coding} step {step}: {e}"));
                assert_eq!(out, plain, "{coding} step {step}");
                assert!(body.is_finished(), "{coding} step {step}");
            }
        }
    });
}

#[test]
fn the_async_and_sync_adapters_agree_byte_for_byte() {
    let plain = common::pseudo_random(150_000);
    runtime().block_on(async {
        for coding in codings() {
            let wire = encode(&coding, &plain);

            let mut sync_body = DecodedBody::with_codings(
                &wire[..],
                std::slice::from_ref(&coding),
                &DecodeLimits::default(),
            )
            .expect("decoder");
            let from_sync = sync_body.read_to_vec().expect("sync read");

            let mut async_body = oxiarc_http::AsyncDecodedBody::with_codings(
                Stuttering::new(wire.clone(), 7),
                std::slice::from_ref(&coding),
                &DecodeLimits::default(),
            )
            .expect("decoder");
            let mut from_async = Vec::new();
            async_body
                .read_to_end(&mut from_async)
                .await
                .expect("async read");

            assert_eq!(from_sync, from_async, "{coding}: sync vs async");
            assert_eq!(from_sync, plain, "{coding}");
        }
    });
}

#[test]
fn a_truncated_async_body_is_an_error_not_a_short_read() {
    let plain = common::text(50_000);
    runtime().block_on(async {
        for coding in codings() {
            if coding == ContentCoding::Identity || coding == ContentCoding::Compress {
                // `.Z` has no end-of-information code (see
                // `ContentCoding::Compress`'s doc comment): truncation is a
                // documented short read here, not the bug this test hunts
                // for.
                continue;
            }
            let wire = encode(&coding, &plain);
            let mut body = oxiarc_http::AsyncDecodedBody::with_codings(
                Stuttering::new(wire[..wire.len() - 6].to_vec(), 64),
                std::slice::from_ref(&coding),
                &DecodeLimits::default(),
            )
            .expect("decoder");
            let mut out = Vec::new();
            let error = body
                .read_to_end(&mut out)
                .await
                .expect_err("truncation must be reported");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{coding}");
        }
    });
}

#[test]
fn limits_apply_to_the_async_adapter_too() {
    let bomb = common::gzip_wrap(&common::single_block_bomb(64 * 1024 * 1024), &[]);
    runtime().block_on(async {
        let mut body = oxiarc_http::AsyncDecodedBody::with_codings(
            Stuttering::new(bomb, 4096),
            &[ContentCoding::Gzip],
            &DecodeLimits::default()
                .with_max_output(256 * 1024)
                .with_max_ratio(None),
        )
        .expect("decoder");
        let mut out = Vec::new();
        let error = body
            .read_to_end(&mut out)
            .await
            .expect_err("a bomb must be refused on the async path too");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(out.len() <= 256 * 1024);
    });
}

/// The trailing-garbage verdict must not depend on where the poll boundaries
/// fall — in particular not on the boundary that lands exactly at the end of
/// the encoded stream, which is where an adapter that closes on `StreamEnd`
/// and stops polling would silently accept the response.
///
/// The sync twin is `chunking::trailing_garbage_is_rejected_at_every_read_boundary`.
#[test]
fn trailing_garbage_is_rejected_whatever_the_poll_boundaries() {
    let plain = common::text(20_000);
    runtime().block_on(async {
        for coding in codings() {
            if coding == ContentCoding::Identity || coding == ContentCoding::Compress {
                // `.Z` cannot distinguish "the stream ended" from "more
                // codes happened to follow" — see
                // `ContentCoding::Compress`'s doc comment.
                continue;
            }
            let wire = encode(&coding, &plain);
            for step in [1usize, 7, wire.len()] {
                let mut with_garbage = wire.clone();
                with_garbage.extend_from_slice(b"XXXX");
                let mut body = oxiarc_http::AsyncDecodedBody::with_codings(
                    Stuttering::new(with_garbage, step),
                    std::slice::from_ref(&coding),
                    &DecodeLimits::default(),
                )
                .expect("decoder");
                let mut out = Vec::new();
                let error = body
                    .read_to_end(&mut out)
                    .await
                    .expect_err("trailing garbage must be rejected on the async path too");
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{coding}");
            }
        }
    });
}

#[test]
fn a_non_default_trailing_policy_reaches_the_async_adapter() {
    let plain = b"body with junk after it";
    let mut wire = oxiarc_deflate::gzip_compress(plain, 6).expect("gzip");
    wire.extend_from_slice(b"XY");
    runtime().block_on(async {
        let decoder = Decoder::new(&[ContentCoding::Gzip], &DecodeLimits::default())
            .expect("decoder")
            .trailing_data(TrailingData::Ignore);
        let mut body =
            oxiarc_http::AsyncDecodedBody::with_decoder(Stuttering::new(wire, 3), decoder);
        let mut out = Vec::new();
        body.read_to_end(&mut out).await.expect("ignore the tail");
        assert_eq!(out, plain);
    });
}

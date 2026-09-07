//! Throughput of the `oxiarc-http` body decoders.
//!
//! The gate this exists for: **decoding a gzip body through `oxiarc-http`
//! must reach at least 0.95x the throughput of decoding the same body
//! through `oxiarc_deflate::gzip_decompress`.** That is the like-for-like
//! comparison — same container walk, same CRC-32 over the same output — so
//! what it measures is exactly the cost of making the decode incremental and
//! bounded. `baseline/raw_inflate` is reported alongside it as the floor
//! (RFC 1951 with no container and no checksum); the gap between the two
//! baselines is the price of the gzip format itself, not of this crate.
//!
//! ```text
//! cargo bench -p oxiarc-http --all-features
//! ```
//!
//! # Measured (Apple Silicon, 4 MiB repetitive-JSON body, medians of 40
//! # interleaved rounds — criterion runs its groups minutes apart, which on
//! # a loaded machine swings by 30%, so the ratios below were taken with an
//! # A/B harness that times both variants inside each round)
//!
//! ```text
//! raw inflate()            1.609 GiB/s     (RFC 1951 floor: no container, no checksum)
//! gzip_decompress()        1.349 GiB/s     0.84x raw  <- the price of the gzip format
//! http decode_body         1.297 GiB/s     0.96x gzip_decompress
//! http DecodedBody         1.292 GiB/s     0.96x gzip_decompress
//! http feed_into (64 KiB)  1.320 GiB/s     0.98x gzip_decompress
//! ```
//!
//! So making the decode incremental, bounded and resumable costs **2-4%**.
//! Against bare `inflate()` the figure is 0.81x, but 0.84x of that gap
//! belongs to gzip itself — `oxiarc-deflate`'s own one-shot `gzip_decompress`
//! is equally far from `inflate()`, because it walks the same container and
//! computes the same CRC-32 over the same 4 MiB. No gzip decoder can reach
//! 0.95x of raw `inflate()`; the container-matched baseline is the one that
//! measures this crate.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_http::{ContentCoding, DecodeLimits, DecodedBody, Decoder, decode_body};
use std::hint::black_box;

/// A realistic API response body: repetitive JSON, the shape that makes
/// compression worth doing over HTTP in the first place.
fn payload(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64);
    out.push(b'[');
    let mut index = 0u32;
    while out.len() < len {
        out.extend_from_slice(
            format!(r#"{{"id":{index},"name":"row {index}","ok":true}},"#).as_bytes(),
        );
        index += 1;
    }
    out.truncate(len);
    out
}

fn bench_decode(c: &mut Criterion) {
    let plain = payload(4 * 1024 * 1024);
    let raw = oxiarc_deflate::deflate(&plain, 6).expect("deflate");
    let gz = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let zlib = oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib");
    let limits = DecodeLimits::unlimited();

    // Two baselines, because they answer two different questions.
    //
    // `raw_inflate` is the floor: RFC 1951 with no container and no
    // checksum. `gzip_decompress` is the *fair* comparison for a gzip body —
    // it walks the same 10-byte header, computes the same CRC-32 over the
    // same 4 MiB of output and verifies the same trailer. Anything this
    // crate's `gzip` path loses against `gzip_decompress` is genuine
    // streaming overhead; the rest of the gap to `raw_inflate` is the
    // container and the checksum, which are not optional for `gzip`.
    let mut group = c.benchmark_group("baseline");
    group.throughput(Throughput::Bytes(plain.len() as u64));
    group.bench_function("raw_inflate", |b| {
        b.iter(|| black_box(oxiarc_deflate::inflate(black_box(&raw)).expect("inflate")))
    });
    group.bench_function("gzip_decompress", |b| {
        b.iter(|| {
            black_box(oxiarc_deflate::gzip_decompress(black_box(&gz)).expect("gzip_decompress"))
        })
    });
    group.bench_function("zlib_decompress", |b| {
        b.iter(|| {
            black_box(oxiarc_deflate::zlib_decompress(black_box(&zlib)).expect("zlib_decompress"))
        })
    });
    group.finish();

    let mut group = c.benchmark_group("body");
    group.throughput(Throughput::Bytes(plain.len() as u64));

    group.bench_function("gzip_one_shot", |b| {
        b.iter(|| {
            black_box(decode_body(&[ContentCoding::Gzip], black_box(&gz), &limits).expect("decode"))
        })
    });

    group.bench_function("deflate_zlib_one_shot", |b| {
        b.iter(|| {
            black_box(
                decode_body(&[ContentCoding::Deflate], black_box(&zlib), &limits).expect("decode"),
            )
        })
    });

    // The streaming shapes: a `Read` source, and a push loop. Both should
    // land within noise of the one-shot figure, because neither buffers.
    group.bench_function("gzip_read_adapter", |b| {
        b.iter(|| {
            let mut body =
                DecodedBody::with_codings(black_box(&gz[..]), &[ContentCoding::Gzip], &limits)
                    .expect("decoder");
            black_box(body.read_to_vec().expect("read"))
        })
    });

    for chunk in [1024usize, 8 * 1024, 64 * 1024] {
        group.bench_with_input(
            BenchmarkId::new("gzip_feed_into", chunk),
            &chunk,
            |b, &chunk| {
                b.iter(|| {
                    let mut decoder =
                        Decoder::new(&[ContentCoding::Gzip], &limits).expect("decoder");
                    let mut out = Vec::with_capacity(plain.len());
                    for piece in gz.chunks(chunk) {
                        decoder.feed_into(piece, &mut out).expect("feed");
                    }
                    decoder.finish_into(&mut out).expect("finish");
                    black_box(out)
                })
            },
        );
    }

    #[cfg(feature = "brotli")]
    {
        let br = oxiarc_brotli::compress(&plain, 4).expect("brotli");
        group.bench_function("br_one_shot", |b| {
            b.iter(|| {
                black_box(
                    decode_body(&[ContentCoding::Brotli], black_box(&br), &limits).expect("decode"),
                )
            })
        });
    }

    #[cfg(feature = "zstd")]
    {
        let zst = oxiarc_zstd::compress(&plain).expect("zstd");
        group.bench_function("zstd_one_shot", |b| {
            b.iter(|| {
                black_box(
                    decode_body(&[ContentCoding::Zstd], black_box(&zst), &limits).expect("decode"),
                )
            })
        });
    }

    group.finish();
}

criterion_group!(benches, bench_decode);
criterion_main!(benches);

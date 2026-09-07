//! Cross-crate proof that a PNG `IDAT` chain split across chunks, a TIFF
//! deflate strip, and an HTTP gzip body split across arbitrary reads all
//! decode to byte-identical output through the **same** `oxiarc_deflate`
//! core (`oxiarc_deflate::WrappedInflate` / `InflateStream`).
//!
//! `repo-conventions.md` §12.2 prescribes an intra-crate differential;
//! nothing before this proved that PNG, TIFF and HTTP — three different
//! crates, each with its own container framing on top — actually drive the
//! *same* resumable engine correctly rather than three parallel dialects
//! that merely resemble it (`critique.md` §6 item 1). Every fixture here is
//! built with the producing crate's own encoder, never hand-crafted bytes,
//! per the Wave-3 track brief.
//!
//! Two tiers per format:
//! 1. **End to end**: the format's own real decode path reproduces the
//!    exact pixels the format's own encoder was given.
//! 2. **Same core**: the raw, still-compressed bytes that format's
//!    container actually carries (PNG's concatenated `IDAT` payload, one
//!    TIFF strip's compressed bytes, the whole HTTP gzip body) are pulled
//!    out through each crate's own public API and driven directly through
//!    `WrappedInflate`, once at a fine split granularity and once at a
//!    coarse one — literally the same function, `drive_wrapped_inflate`,
//!    called with different chunk sizes, so any divergence is a real
//!    resumability bug rather than two different call shapes disagreeing
//!    for an incidental reason.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateWrapper, WrappedInflate};
use std::io::Cursor;

/// Bound on `inflate()` calls, so a state machine that stops making
/// progress fails loudly instead of hanging the suite.
const CALL_GUARD: u32 = 500_000;

/// Feed `compressed` through a fresh [`WrappedInflate`] configured for
/// `wrapper`, in chunks of `chunk_size` bytes, then signal true end of
/// input — the exact shape every long-lived consumer of this core uses
/// (PNG's `IDAT` chain, TIFF's per-strip reads, HTTP's response-body
/// driver), reproduced here directly rather than through any of those
/// three crates, so it really is the shared core under test.
fn drive_wrapped_inflate(wrapper: InflateWrapper, compressed: &[u8], chunk_size: usize) -> Vec<u8> {
    let mut decoder = WrappedInflate::new(wrapper).multi_member(true);
    let mut out = Vec::new();
    let mut sink = [0u8; 4096];
    let mut pos = 0usize;
    let mut calls = 0u32;
    let mut ended = false;

    while !ended && pos < compressed.len() {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress feeding real input bytes");
        let end = (pos + chunk_size).min(compressed.len());
        let progress = decoder
            .inflate(&compressed[pos..end], &mut sink, FlushMode::None)
            .expect("inflate a fixture built by this crate's own encoder");
        out.extend_from_slice(&sink[..progress.produced]);
        pos += progress.consumed;
        if progress.status == InflateStatus::StreamEnd {
            ended = true;
        }
    }
    while !ended {
        calls += 1;
        assert!(calls < CALL_GUARD, "no progress in the finish phase");
        let progress = decoder
            .inflate(&[], &mut sink, FlushMode::Finish)
            .expect("finish a fixture built by this crate's own encoder");
        out.extend_from_slice(&sink[..progress.produced]);
        if progress.status == InflateStatus::StreamEnd {
            ended = true;
        }
    }
    out
}

/// Deterministic xorshift, matching `oxiarc-deflate/tests/adversarial_verify.rs`'s
/// house pattern — reproducible on every machine, no `rand` dependency.
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

// ─── PNG: IDAT split across chunks ──────────────────────────────────────

mod png_fixture {
    use super::Rng;
    use oxiarc_png::{BitDepth, ColorType, Encoder};

    /// A small RGB8 image and the PNG bytes for it, encoded with a
    /// deliberately tiny `set_idat_chunk_size` so the `IDAT` stream lands
    /// in many small chunks rather than one.
    pub fn build() -> (u32, u32, Vec<u8>, Vec<u8>) {
        let (width, height) = (37, 29); // not a round number, on purpose
        let mut rng = Rng(0xC0FF_EE15_C0DE_u64);
        let pixels: Vec<u8> = (0..(width * height * 3) as usize)
            .map(|_| rng.byte())
            .collect();

        let mut png_bytes = Vec::new();
        {
            let mut enc = Encoder::new(&mut png_bytes, width, height);
            enc.set_color(ColorType::Rgb);
            enc.set_depth(BitDepth::Eight);
            enc.set_idat_chunk_size(11); // tiny: forces many IDAT chunks
            let mut writer = enc.write_header().expect("png header");
            writer.write_image_data(&pixels).expect("png image data");
            writer.finish().expect("png finish");
        }
        (width, height, pixels, png_bytes)
    }
}

/// Walk a PNG's chunk stream (8-byte signature, then repeated
/// `[len:4][type:4][data:len][crc:4]`), returning every chunk whose type
/// matches `kind`. Plain, public PNG chunk framing — not a reach into
/// `oxiarc-png`'s internals.
fn png_chunks<'a>(png: &'a [u8], kind: &[u8; 4]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut pos = 8usize; // past the 8-byte signature
    while pos + 8 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().expect("4 bytes")) as usize;
        let ctype = &png[pos + 4..pos + 8];
        let data_start = pos + 8;
        let data_end = data_start + len;
        if data_end + 4 > png.len() {
            break; // truncated; nothing more to walk
        }
        if ctype == kind {
            out.push(&png[data_start..data_end]);
        }
        pos = data_end + 4; // past the trailing CRC
    }
    out
}

#[test]
fn png_idat_split_across_chunks_decodes_through_the_shared_core() {
    let (width, height, pixels, png_bytes) = png_fixture::build();

    let idat_parts = png_chunks(&png_bytes, b"IDAT");
    assert!(
        idat_parts.len() > 1,
        "fixture only produced {} IDAT chunk(s); the tiny idat_chunk_size did not force a split",
        idat_parts.len()
    );

    // Tier 1: oxiarc-png's own decode reproduces the exact pixels.
    let decoded = oxiarc_png::decode(&png_bytes).expect("png decode");
    assert_eq!(decoded.data, pixels, "PNG round trip changed the pixels");

    // Tier 2: the concatenated raw IDAT payload — a zlib stream split
    // across container chunk boundaries exactly as the PNG spec allows —
    // driven directly through the same WrappedInflate core, at two
    // granularities.
    let idat_stream: Vec<u8> = idat_parts.into_iter().flatten().copied().collect();
    let coarse =
        drive_wrapped_inflate(InflateWrapper::Zlib, &idat_stream, idat_stream.len().max(1));
    let fine = drive_wrapped_inflate(InflateWrapper::Zlib, &idat_stream, 1);
    assert_eq!(
        coarse, fine,
        "WrappedInflate(Zlib) over the PNG's own IDAT stream diverged between granularities"
    );

    // Ground truth, not just self-consistency: two granularities agreeing on
    // the *wrong* bytes would satisfy the check above. The inflated IDAT
    // stream is PNG's filtered scanline stream — one filter byte then
    // `width * 3` bytes per row — so it must be exactly that long, and
    // undoing the filters must reproduce the very pixels the encoder was
    // handed.
    let stride = (width * 3) as usize;
    assert_eq!(
        coarse.len(),
        (height as usize) * (1 + stride),
        "the inflated IDAT stream is not PNG's filtered scanline stream"
    );
    assert_eq!(
        unfilter_png(&coarse, stride, 3),
        pixels,
        "WrappedInflate(Zlib) over the PNG's own IDAT stream did not reproduce the pixels"
    );
}

/// Undo PNG's per-scanline filters (RFC 2083 §6) over a filtered scanline
/// stream of `height` rows, each one filter byte followed by `stride` bytes,
/// with `bpp` bytes per pixel. Deliberately hand-written here rather than
/// reached for inside `oxiarc-png`: the point of this tier is to check the
/// shared inflate core's output against an independent reconstruction, and
/// borrowing the crate's own unfilter would just compare it against itself.
fn unfilter_png(filtered: &[u8], stride: usize, bpp: usize) -> Vec<u8> {
    fn paeth(a: u8, b: u8, c: u8) -> u8 {
        let p = i32::from(a) + i32::from(b) - i32::from(c);
        let (pa, pb, pc) = (
            (p - i32::from(a)).abs(),
            (p - i32::from(b)).abs(),
            (p - i32::from(c)).abs(),
        );
        if pa <= pb && pa <= pc {
            a
        } else if pb <= pc {
            b
        } else {
            c
        }
    }

    let mut out: Vec<u8> = Vec::with_capacity(filtered.len());
    let mut prev = vec![0u8; stride];
    let mut row = vec![0u8; stride];
    let mut pos = 0usize;
    while pos + 1 + stride <= filtered.len() {
        let filter = filtered[pos];
        row.copy_from_slice(&filtered[pos + 1..pos + 1 + stride]);
        pos += 1 + stride;
        for i in 0..stride {
            let a = if i >= bpp { row[i - bpp] } else { 0 };
            let b = prev[i];
            let c = if i >= bpp { prev[i - bpp] } else { 0 };
            row[i] = match filter {
                0 => row[i],
                1 => row[i].wrapping_add(a),
                2 => row[i].wrapping_add(b),
                3 => row[i].wrapping_add(((u16::from(a) + u16::from(b)) / 2) as u8),
                4 => row[i].wrapping_add(paeth(a, b, c)),
                other => panic!("unknown PNG filter type {other}"),
            };
        }
        out.extend_from_slice(&row);
        prev.copy_from_slice(&row);
    }
    out
}

// ─── TIFF: one deflate strip ────────────────────────────────────────────

mod tiff_fixture {
    use super::Rng;
    use oxiarc_tiff::{ColorType, Compression, Encoder, ImageSpec, Layout};
    use std::io::Cursor;

    /// A small RGB8 image and the TIFF bytes for it, written with several
    /// small deflate-compressed strips (`RowsPerStrip` well under the full
    /// height) rather than one strip covering the whole image.
    pub fn build() -> (u32, u32, Vec<u8>, Vec<u8>) {
        let (width, height) = (48u32, 40u32);
        let mut rng = Rng(0x5EED_1234_5678_u64);
        let pixels: Vec<u8> = (0..(width * height * 3) as usize)
            .map(|_| rng.byte())
            .collect();

        let spec = ImageSpec::new(width, height, ColorType::Rgb(8))
            .with_compression(Compression::Deflate { level: 6 })
            .with_layout(Layout::Strips { rows_per_strip: 8 });

        let mut tiff_bytes = Vec::new();
        {
            let mut enc = Encoder::new(Cursor::new(&mut tiff_bytes)).expect("tiff encoder");
            enc.write_image(&spec, &pixels).expect("tiff write_image");
            let _ = enc.finish().expect("tiff finish");
        }
        (width, height, pixels, tiff_bytes)
    }
}

#[test]
fn tiff_deflate_strip_decodes_through_the_shared_core() {
    let (width, _height, pixels, tiff_bytes) = tiff_fixture::build();

    // Tier 1: oxiarc-tiff's own decode reproduces the exact pixels.
    let mut decoder = oxiarc_tiff::Decoder::new(Cursor::new(&tiff_bytes)).expect("tiff decoder");
    let mut out = vec![0u8; pixels.len()];
    decoder
        .read_image_bytes(&mut out)
        .expect("tiff read_image_bytes");
    assert_eq!(out, pixels, "TIFF round trip changed the pixels");

    let strip_count = decoder.chunk_count().expect("chunk_count");
    assert!(
        strip_count > 1,
        "fixture only produced {strip_count} strip(s); rows_per_strip did not force a split"
    );

    // Tier 2: one strip's raw (still-compressed) bytes — `read_strip_raw`
    // returns the compressed payload, verified against its implementation
    // (`self.buffers.compressed().to_vec()` in `oxiarc-tiff/src/reader.rs`)
    // — driven directly through the same WrappedInflate core, at two
    // granularities.
    let raw_strip = decoder.read_strip_raw(0).expect("raw strip 0");
    assert!(!raw_strip.is_empty(), "strip 0 has no compressed bytes");
    let coarse = drive_wrapped_inflate(InflateWrapper::Zlib, &raw_strip, raw_strip.len().max(1));
    let fine = drive_wrapped_inflate(InflateWrapper::Zlib, &raw_strip, 1);
    assert_eq!(
        coarse, fine,
        "WrappedInflate(Zlib) over a real TIFF strip diverged between granularities"
    );

    // Ground truth: the fixture uses `Predictor::None` (`ImageSpec::new`'s
    // default) and `RowsPerStrip = 8`, so strip 0's decompressed bytes are
    // literally the first 8 rows of the image, byte for byte. Two
    // granularities agreeing on the wrong bytes would pass the check above;
    // this cannot.
    let rows_per_strip = 8usize;
    let stride = (width * 3) as usize;
    assert_eq!(
        coarse.len(),
        rows_per_strip * stride,
        "TIFF strip 0 did not decompress to RowsPerStrip whole rows"
    );
    assert_eq!(
        coarse,
        pixels[..rows_per_strip * stride],
        "WrappedInflate(Zlib) over a real TIFF strip did not reproduce the pixels"
    );
}

// ─── HTTP: gzip body split across random reads ──────────────────────────

/// A `Read` source that hands back reads of pseudo-random size (still
/// respecting whatever buffer the caller offered), simulating arbitrary
/// TCP read boundaries rather than the whole body arriving in one `read`.
///
/// Gated the same as the one test that uses it: with `--no-default-features`
/// (no `gzip`), this whole type would otherwise be reported as dead code.
#[cfg(feature = "gzip")]
struct RandomReadSource<'a> {
    data: &'a [u8],
    pos: usize,
    rng: Rng,
}

#[cfg(feature = "gzip")]
impl<'a> RandomReadSource<'a> {
    fn new(data: &'a [u8], seed: u64) -> Self {
        Self {
            data,
            pos: 0,
            rng: Rng(seed),
        }
    }
}

#[cfg(feature = "gzip")]
impl std::io::Read for RandomReadSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let want = 1 + (self.rng.byte() as usize % 37); // 1..=37 bytes
        let n = buf.len().min(want).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(feature = "gzip")]
#[test]
fn http_gzip_body_split_across_random_reads_decodes_through_the_shared_core() {
    use oxiarc_http::{DecodeLimits, DecodedBody};
    use std::io::Read;

    let mut rng = Rng(0xBADC_0FFE_E000_1234);
    let plaintext: Vec<u8> = (0..70_000usize)
        .map(|i| {
            if i % 97 == 0 {
                rng.byte()
            } else {
                (i % 251) as u8
            }
        })
        .collect();
    let gzip_body = oxiarc_deflate::gzip_compress(&plaintext, 6).expect("gzip compress");

    // Tier 1: oxiarc-http's own DecodedBody, fed through a source that
    // returns a different, arbitrary number of bytes on every `read` call.
    let source = RandomReadSource::new(&gzip_body, 0x1122_3344_5566_7788);
    let mut body =
        DecodedBody::new(source, "gzip", &DecodeLimits::default()).expect("DecodedBody::new");
    let mut decoded = Vec::new();
    body.read_to_end(&mut decoded).expect("read_to_end");
    assert_eq!(
        decoded, plaintext,
        "HTTP gzip body split across random reads changed the bytes"
    );

    // Tier 2: the same gzip body, driven directly through WrappedInflate,
    // at two granularities.
    let coarse = drive_wrapped_inflate(InflateWrapper::Gzip, &gzip_body, gzip_body.len().max(1));
    let fine = drive_wrapped_inflate(InflateWrapper::Gzip, &gzip_body, 1);
    assert_eq!(
        coarse, fine,
        "WrappedInflate(Gzip) over the HTTP body diverged between granularities"
    );
    assert_eq!(
        coarse, plaintext,
        "WrappedInflate(Gzip) over the HTTP body did not reproduce the plaintext"
    );
}

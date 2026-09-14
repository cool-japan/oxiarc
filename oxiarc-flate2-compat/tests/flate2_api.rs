//! Behavioural tests of the flate2-shaped API, against reference streams
//! produced by CPython's `zlib`/`gzip` modules and the `gzip` CLI
//! (`tests/data/`), and against the raw `Compress`/`Decompress` contract
//! that `compression-codecs` relies on.

use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};

use oxiarc_flate2_compat::{
    Compress, Compression, Crc, Decompress, FlushCompress, FlushDecompress, GzBuilder, Status,
    bufread, read, write,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const TEXT: &[u8] = include_bytes!("data/text.txt");
const TEXT_ZLIB: &[u8] = include_bytes!("data/text.zlib");
const TEXT_DEFLATE: &[u8] = include_bytes!("data/text.deflate");
const TEXT_CLI_GZ: &[u8] = include_bytes!("data/text_cli.gz");
const MULTI_GZ: &[u8] = include_bytes!("data/multi.gz");
const DICT_ZLIB: &[u8] = include_bytes!("data/dict.zlib");
const SYNC_ZLIB: &[u8] = include_bytes!("data/sync.zlib");
const SYNC_PREFIX_LEN: &str = include_str!("data/sync_prefix_len.txt");

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|i| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            // Mix compressible runs with noise.
            if (i / 64) % 3 == 0 {
                b'a' + (i % 7) as u8
            } else {
                (state >> 33) as u8
            }
        })
        .collect()
}

/// Reader that hands out at most `chunk` bytes per call.
struct Trickle<'a> {
    data: &'a [u8],
    chunk: usize,
}

impl Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.data.len().min(self.chunk).min(buf.len());
        buf[..n].copy_from_slice(&self.data[..n]);
        self.data = &self.data[n..];
        Ok(n)
    }
}

#[test]
fn reference_zlib_decodes_all_readers() -> TestResult {
    let mut out = Vec::new();
    read::ZlibDecoder::new(TEXT_ZLIB).read_to_end(&mut out)?;
    assert_eq!(out, TEXT);

    let mut out = Vec::new();
    bufread::ZlibDecoder::new(TEXT_ZLIB).read_to_end(&mut out)?;
    assert_eq!(out, TEXT);

    let mut out = Vec::new();
    read::ZlibDecoder::new(Trickle {
        data: TEXT_ZLIB,
        chunk: 1,
    })
    .read_to_end(&mut out)?;
    assert_eq!(out, TEXT);

    let mut w = write::ZlibDecoder::new(Vec::new());
    for piece in TEXT_ZLIB.chunks(3) {
        w.write_all(piece)?;
    }
    assert_eq!(w.finish()?, TEXT);
    Ok(())
}

#[test]
fn reference_raw_deflate_and_gzip_cli() -> TestResult {
    let mut out = Vec::new();
    read::DeflateDecoder::new(TEXT_DEFLATE).read_to_end(&mut out)?;
    assert_eq!(out, TEXT);

    let mut out = Vec::new();
    let mut gz = read::GzDecoder::new(TEXT_CLI_GZ);
    gz.read_to_end(&mut out)?;
    assert_eq!(out, TEXT);
    let header = gz.header().ok_or("header")?;
    assert_eq!(header.filename(), None);
    assert_eq!(header.operating_system(), 3);
    Ok(())
}

#[test]
fn multi_member_gzip() -> TestResult {
    // Single-member decoder stops after member one and leaves the rest.
    let mut first = Vec::new();
    let mut single = bufread::GzDecoder::new(MULTI_GZ);
    single.read_to_end(&mut first)?;
    assert_eq!(first, &TEXT[..3000]);
    assert_eq!(
        single.header().and_then(|h| h.filename()),
        Some(&b"a.txt"[..])
    );
    assert_eq!(single.header().map(|h| h.mtime()), Some(1_700_000_000));
    let rest = single.into_inner();
    assert!(
        rest.starts_with(&[0x1f, 0x8b]),
        "second member left in place"
    );

    let mut all = Vec::new();
    read::MultiGzDecoder::new(Trickle {
        data: MULTI_GZ,
        chunk: 7,
    })
    .read_to_end(&mut all)?;
    assert_eq!(all, TEXT);

    let mut w = write::MultiGzDecoder::new(Vec::new());
    for piece in MULTI_GZ.chunks(5) {
        w.write_all(piece)?;
    }
    assert_eq!(w.finish()?, TEXT);
    Ok(())
}

#[test]
fn corrupt_gzip_crc_is_an_error() {
    let mut bad = TEXT_CLI_GZ.to_vec();
    let n = bad.len();
    bad[n - 6] ^= 0xff;
    let mut out = Vec::new();
    let err = read::GzDecoder::new(&bad[..]).read_to_end(&mut out);
    assert!(err.is_err());

    let mut bad = TEXT_ZLIB.to_vec();
    let n = bad.len();
    bad[n - 1] ^= 0x01;
    let err = read::ZlibDecoder::new(&bad[..]).read_to_end(&mut Vec::new());
    assert!(err.is_err());
}

#[test]
fn truncated_stream_is_unexpected_eof() {
    let cut = &TEXT_ZLIB[..TEXT_ZLIB.len() / 2];
    let err = read::ZlibDecoder::new(cut).read_to_end(&mut Vec::new());
    match err {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        Ok(_) => panic!("truncated stream decoded without error"),
    }
}

#[test]
fn non_gzip_input_errors() {
    let err = read::GzDecoder::new(&b"<svg>not gzip</svg>"[..]).read_to_end(&mut Vec::new());
    assert!(err.is_err());
    let err = read::GzDecoder::new(&b""[..]).read_to_end(&mut Vec::new());
    assert!(err.is_err());
}

#[test]
fn roundtrip_every_level_and_container() -> TestResult {
    let data = pseudo_random(200_000, 7);
    for level in 0..=10u32 {
        let lvl = Compression::new(level);

        let mut e = write::ZlibEncoder::new(Vec::new(), lvl);
        e.write_all(&data)?;
        let z = e.finish()?;
        let mut out = Vec::new();
        read::ZlibDecoder::new(&z[..]).read_to_end(&mut out)?;
        assert_eq!(out, data, "zlib level {level}");

        let mut e = write::GzEncoder::new(Vec::new(), lvl);
        for piece in data.chunks(4099) {
            e.write_all(piece)?;
        }
        let g = e.finish()?;
        let mut out = Vec::new();
        read::MultiGzDecoder::new(&g[..]).read_to_end(&mut out)?;
        assert_eq!(out, data, "gzip level {level}");

        let mut out = Vec::new();
        read::DeflateEncoder::new(&data[..], lvl).read_to_end(&mut out)?;
        let mut back = Vec::new();
        bufread::DeflateDecoder::new(&out[..]).read_to_end(&mut back)?;
        assert_eq!(back, data, "deflate level {level}");
    }
    Ok(())
}

#[test]
fn read_side_encoders_roundtrip() -> TestResult {
    let data = pseudo_random(70_000, 11);
    let mut z = Vec::new();
    read::ZlibEncoder::new(
        Trickle {
            data: &data,
            chunk: 333,
        },
        Compression::best(),
    )
    .read_to_end(&mut z)?;
    let mut out = Vec::new();
    read::ZlibDecoder::new(&z[..]).read_to_end(&mut out)?;
    assert_eq!(out, data);

    let mut g = Vec::new();
    read::GzEncoder::new(&data[..], Compression::fast()).read_to_end(&mut g)?;
    let mut out = Vec::new();
    read::GzDecoder::new(&g[..]).read_to_end(&mut out)?;
    assert_eq!(out, data);
    Ok(())
}

#[test]
fn gz_encoder_finishes_on_drop() -> TestResult {
    let mut buf = Vec::new();
    {
        let mut e = write::GzEncoder::new(&mut buf, Compression::default());
        e.write_all(b"dropped, not finished")?;
    }
    let mut out = String::new();
    read::GzDecoder::new(&buf[..]).read_to_string(&mut out)?;
    assert_eq!(out, "dropped, not finished");
    Ok(())
}

#[test]
fn gz_builder_header_fields_roundtrip() -> TestResult {
    let mut e = GzBuilder::new()
        .filename("model.nnef")
        .comment("oxiarc")
        .mtime(42)
        .write(Vec::new(), Compression::default());
    e.write_all(b"payload")?;
    let g = e.finish()?;
    let mut d = read::GzDecoder::new(&g[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out)?;
    let h = d.header().ok_or("header")?;
    assert_eq!(h.filename(), Some(&b"model.nnef"[..]));
    assert_eq!(h.comment(), Some(&b"oxiarc"[..]));
    assert_eq!(h.mtime(), 42);
    assert_eq!(out, b"payload");
    Ok(())
}

#[test]
fn decompress_stops_exactly_at_stream_end() -> TestResult {
    // raw deflate followed by unrelated bytes: total_in must be exactly the
    // payload length so the caller can find what follows (gzip trailer).
    let mut stream = TEXT_DEFLATE.to_vec();
    stream.extend_from_slice(b"TRAILER!");
    for chunk in [1usize, 2, 3, 5, 64, 4096, stream.len()] {
        let mut d = Decompress::new(false);
        let mut out = vec![0u8; TEXT.len() + 16];
        let mut pos_in = 0usize;
        let mut pos_out = 0usize;
        loop {
            let end = (pos_in + chunk).min(stream.len());
            let before_in = d.total_in();
            let before_out = d.total_out();
            let status = d.decompress(
                &stream[pos_in..end],
                &mut out[pos_out..],
                FlushDecompress::None,
            )?;
            pos_in += (d.total_in() - before_in) as usize;
            pos_out += (d.total_out() - before_out) as usize;
            if status == Status::StreamEnd {
                break;
            }
        }
        assert_eq!(&out[..pos_out], TEXT, "chunk {chunk}");
        assert_eq!(pos_in, TEXT_DEFLATE.len(), "chunk {chunk}");
        assert_eq!(&stream[pos_in..], b"TRAILER!");
        // Further calls keep reporting StreamEnd without consuming.
        let status = d.decompress(b"xx", &mut out, FlushDecompress::None)?;
        assert_eq!(status, Status::StreamEnd);
        assert_eq!(d.total_in(), TEXT_DEFLATE.len() as u64);
    }
    Ok(())
}

#[test]
fn zlib_decompress_leaves_trailing_bytes() -> TestResult {
    let mut stream = TEXT_ZLIB.to_vec();
    stream.extend_from_slice(b"NEXT");
    let mut d = Decompress::new(true);
    let mut out = Vec::with_capacity(TEXT.len() + 10);
    let status = d.decompress_vec(&stream, &mut out, FlushDecompress::Finish)?;
    assert_eq!(status, Status::StreamEnd);
    assert_eq!(out, TEXT);
    assert_eq!(d.total_in(), TEXT_ZLIB.len() as u64);
    Ok(())
}

#[test]
fn decompress_buf_error_when_no_progress() -> TestResult {
    let mut d = Decompress::new(true);
    let mut out = [0u8; 16];
    assert_eq!(
        d.decompress(&[], &mut out, FlushDecompress::None)?,
        Status::BufError
    );
    // Incomplete input with Finish is not a hard error.
    let status = d.decompress(&TEXT_ZLIB[..10], &mut out, FlushDecompress::Finish)?;
    assert_ne!(status, Status::StreamEnd);
    Ok(())
}

#[test]
fn zlib_dictionary_roundtrip_and_needs_dictionary() -> TestResult {
    let dict = b"quick brown fox lazy dog";
    // Reference stream (CPython zlib with zdict).
    let mut d = Decompress::new(true);
    let mut out = Vec::with_capacity(TEXT.len());
    let err = d
        .decompress_vec(DICT_ZLIB, &mut out, FlushDecompress::None)
        .err()
        .ok_or("expected a dictionary request")?;
    let wanted = err.needs_dictionary().ok_or("needs_dictionary")?;
    let consumed = d.total_in() as usize;
    assert_eq!(d.set_dictionary(dict)?, wanted);
    let status = d.decompress_vec(&DICT_ZLIB[consumed..], &mut out, FlushDecompress::None)?;
    assert_eq!(status, Status::StreamEnd);
    assert_eq!(out, TEXT);

    // Our own stream with a dictionary.
    let mut c = Compress::new(Compression::default(), true);
    let id = c.set_dictionary(dict)?;
    assert_eq!(id, wanted);
    let mut z = Vec::with_capacity(TEXT.len() + 64);
    assert_eq!(
        c.compress_vec(TEXT, &mut z, FlushCompress::Finish)?,
        Status::StreamEnd
    );
    assert_eq!(z[1] & 0x20, 0x20, "FDICT set");
    let mut d = Decompress::new(true);
    d.set_dictionary(dict)?;
    let mut out = Vec::with_capacity(TEXT.len());
    d.decompress_vec(&z, &mut out, FlushDecompress::Finish)?;
    assert_eq!(out, TEXT);
    Ok(())
}

/// The `compression-codecs` FlateEncoder/FlateDecoder driving pattern:
/// encode with `None`, flush with one `Sync` then `None` until no output,
/// finish with `Finish` until `StreamEnd`, all through a tiny output buffer.
#[test]
fn compression_codecs_style_driver() -> TestResult {
    let data = pseudo_random(50_000, 3);
    for zlib in [false, true] {
        let mut c = Compress::new(Compression::new(5), zlib);
        let mut compressed = Vec::new();
        let mut out = [0u8; 7];
        let mut sync_points = Vec::new();
        for (i, piece) in data.chunks(9_999).enumerate() {
            let mut input = piece;
            while !input.is_empty() {
                let before_in = c.total_in();
                let before_out = c.total_out();
                let status = c.compress(input, &mut out, FlushCompress::None)?;
                assert_ne!(status, Status::StreamEnd);
                input = &input[(c.total_in() - before_in) as usize..];
                compressed.extend_from_slice(&out[..(c.total_out() - before_out) as usize]);
            }
            if i % 2 == 0 {
                let before_out = c.total_out();
                c.compress(&[], &mut out, FlushCompress::Sync)?;
                compressed.extend_from_slice(&out[..(c.total_out() - before_out) as usize]);
                loop {
                    let before_out = c.total_out();
                    c.compress(&[], &mut out, FlushCompress::None)?;
                    let n = (c.total_out() - before_out) as usize;
                    compressed.extend_from_slice(&out[..n]);
                    if n == 0 {
                        break;
                    }
                }
                sync_points.push((compressed.len(), c.total_in() as usize));
                // A second Sync with nothing new is a no-op (BufError).
                assert_eq!(
                    c.compress(&[], &mut out, FlushCompress::Sync)?,
                    Status::BufError
                );
            }
        }
        loop {
            let before_out = c.total_out();
            let status = c.compress(&[], &mut out, FlushCompress::Finish)?;
            compressed.extend_from_slice(&out[..(c.total_out() - before_out) as usize]);
            if status == Status::StreamEnd {
                break;
            }
        }
        assert_eq!(c.total_in() as usize, data.len());
        assert_eq!(c.total_out() as usize, compressed.len());

        // Every sync point makes all input so far decodable.
        for &(clen, dlen) in &sync_points {
            let mut d = Decompress::new(zlib);
            let mut decoded = Vec::with_capacity(data.len());
            d.decompress_vec(&compressed[..clen], &mut decoded, FlushDecompress::Sync)?;
            assert_eq!(decoded, &data[..dlen], "sync point {clen}");
        }

        // Full decode through a tiny output buffer.
        let mut d = Decompress::new(zlib);
        let mut decoded = Vec::new();
        let mut input = &compressed[..];
        loop {
            let before_in = d.total_in();
            let before_out = d.total_out();
            let status = d.decompress(input, &mut out, FlushDecompress::None)?;
            input = &input[(d.total_in() - before_in) as usize..];
            decoded.extend_from_slice(&out[..(d.total_out() - before_out) as usize]);
            if status == Status::StreamEnd {
                break;
            }
            assert_ne!(status, Status::BufError, "stalled");
        }
        assert!(input.is_empty());
        assert_eq!(decoded, data);
    }
    Ok(())
}

#[test]
fn reference_sync_flush_prefix_decodes() -> TestResult {
    let prefix: usize = SYNC_PREFIX_LEN.trim().parse()?;
    let mut d = Decompress::new(true);
    let mut out = Vec::with_capacity(TEXT.len());
    d.decompress_vec(&SYNC_ZLIB[..prefix], &mut out, FlushDecompress::Sync)?;
    assert_eq!(out, &TEXT[..1000]);
    d.decompress_vec(&SYNC_ZLIB[prefix..], &mut out, FlushDecompress::Finish)?;
    assert_eq!(out, TEXT);
    Ok(())
}

#[test]
fn uninit_variants_match() -> TestResult {
    let data = pseudo_random(10_000, 5);
    let mut c = Compress::new(Compression::default(), true);
    let mut buf = vec![std::mem::MaybeUninit::<u8>::uninit(); 20_000];
    let status = c.compress_uninit(&data, &mut buf, FlushCompress::Finish)?;
    assert_eq!(status, Status::StreamEnd);
    let n = c.total_out() as usize;
    let compressed: Vec<u8> = buf[..n]
        .iter()
        .map(|b| {
            // SAFETY: the first `total_out` bytes were written above.
            unsafe { b.assume_init() }
        })
        .collect();
    let mut d = Decompress::new(true);
    let mut out = vec![std::mem::MaybeUninit::<u8>::uninit(); 10_000];
    let status = d.decompress_uninit(&compressed, &mut out, FlushDecompress::Finish)?;
    assert_eq!(status, Status::StreamEnd);
    let decoded: Vec<u8> = out
        .iter()
        .map(|b| {
            // SAFETY: decompress_uninit initialises the whole slice.
            unsafe { b.assume_init() }
        })
        .collect();
    assert_eq!(decoded, data);
    Ok(())
}

#[test]
fn compress_reset_and_set_level() -> TestResult {
    let data = pseudo_random(100_000, 9);
    let mut c = Compress::new(Compression::fast(), true);
    let mut z = Vec::with_capacity(200_000);
    c.compress_vec(&data[..50_000], &mut z, FlushCompress::None)?;
    c.set_level(Compression::best())?;
    c.compress_vec(&data[50_000..], &mut z, FlushCompress::Finish)?;
    let mut out = Vec::new();
    read::ZlibDecoder::new(&z[..]).read_to_end(&mut out)?;
    assert_eq!(out, data);

    c.reset();
    assert_eq!(c.total_in(), 0);
    let mut z2 = Vec::with_capacity(200);
    c.compress_vec(b"again", &mut z2, FlushCompress::Finish)?;
    let mut out = Vec::new();
    read::ZlibDecoder::new(&z2[..]).read_to_end(&mut out)?;
    assert_eq!(out, b"again");
    Ok(())
}

#[test]
fn gzip_via_compress_new_gzip() -> TestResult {
    let mut c = Compress::new_gzip(Compression::default(), 15);
    let mut g = Vec::with_capacity(TEXT.len() + 64);
    assert_eq!(
        c.compress_vec(TEXT, &mut g, FlushCompress::Finish)?,
        Status::StreamEnd
    );
    let mut out = Vec::new();
    read::GzDecoder::new(&g[..]).read_to_end(&mut out)?;
    assert_eq!(out, TEXT);
    let mut d = Decompress::new_gzip(15);
    let mut out = Vec::with_capacity(TEXT.len());
    assert_eq!(
        d.decompress_vec(TEXT_CLI_GZ, &mut out, FlushDecompress::Finish)?,
        Status::StreamEnd
    );
    assert_eq!(out, TEXT);
    Ok(())
}

#[test]
fn crc_matches_reference() {
    let mut crc = Crc::new();
    crc.update(TEXT);
    let n = TEXT_CLI_GZ.len();
    let stored = u32::from_le_bytes([
        TEXT_CLI_GZ[n - 8],
        TEXT_CLI_GZ[n - 7],
        TEXT_CLI_GZ[n - 6],
        TEXT_CLI_GZ[n - 5],
    ]);
    assert_eq!(crc.sum(), stored);
    assert_eq!(crc.amount() as usize, TEXT.len());
}

#[test]
fn bufread_decoder_leaves_following_bytes() -> TestResult {
    let mut input = TEXT_ZLIB.to_vec();
    input.extend_from_slice(b"tail");
    let mut dec = bufread::ZlibDecoder::new(&input[..]);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    assert_eq!(out, TEXT);
    let mut rest = dec.into_inner();
    assert_eq!(rest.fill_buf()?, b"tail");
    Ok(())
}

/// Our output is accepted by CPython's zlib (skipped when python3 is absent).
#[test]
fn python_zlib_accepts_our_streams() -> TestResult {
    let data = pseudo_random(30_000, 13);
    let mut e = write::GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(&data)?;
    let gz = e.finish()?;
    let mut z = write::ZlibEncoder::new(Vec::new(), Compression::best());
    z.write_all(&data)?;
    let zl = z.finish()?;
    for (stream, wbits) in [(gz, "31"), (zl, "15")] {
        let child = Command::new("python3")
            .args([
                "-c",
                &format!(
                    "import sys,zlib; sys.stdout.buffer.write(zlib.decompress(sys.stdin.buffer.read(), {wbits}))"
                ),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("python3 not available; skipping oracle");
            return Ok(());
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&stream)?;
        }
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "python rejected stream");
        assert_eq!(output.stdout, data);
    }
    Ok(())
}

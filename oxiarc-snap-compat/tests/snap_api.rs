//! snap-shaped API behaviour: parquet's raw-codec call pattern, hand-built
//! reference blocks from the Snappy format description, and framed
//! round trips through both directions.

use std::io::{Read, Write};

use oxiarc_snap_compat::raw::{Decoder, Encoder, decompress_len, max_compress_len};
use oxiarc_snap_compat::{Error, read, write};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn sample(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_u32;
    (0..len)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if i % 100 < 60 {
                b"parquet"[i % 7]
            } else {
                state as u8
            }
        })
        .collect()
}

/// `parquet::compression::snappy_codec` exactly: size the output with
/// `max_compress_len`, compress into it, truncate; decompress into a
/// buffer sized by `decompress_len`.
#[test]
fn parquet_call_pattern() -> TestResult {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    for len in [0usize, 1, 100, 65_536, 65_537, 1_000_000] {
        let input = sample(len);
        let mut output_buf = Vec::new();
        let offset = output_buf.len();
        let required_len = max_compress_len(input.len());
        output_buf.resize(offset + required_len, 0);
        let n = encoder.compress(&input, &mut output_buf[offset..])?;
        output_buf.truncate(offset + n);

        let len_hint = decompress_len(&output_buf)?;
        assert_eq!(len_hint, input.len());
        let mut out = vec![0u8; len_hint];
        let n = decoder.decompress(&output_buf, &mut out)?;
        assert_eq!(n, input.len());
        assert_eq!(out, input);
    }
    Ok(())
}

#[test]
fn reference_blocks() -> TestResult {
    // 11-byte literal: varint 11, tag (11-1)<<2.
    let literal = b"\x0b\x28Hello World";
    assert_eq!(Decoder::new().decompress_vec(literal)?, b"Hello World");
    // 20 x 'a': literal 'a' then a 2-byte-offset copy of length 19 at
    // offset 1 (tag (19-1)<<2 | 2).
    let copy = [0x14, 0x00, b'a', 0x4a, 0x01, 0x00];
    assert_eq!(Decoder::new().decompress_vec(&copy)?, vec![b'a'; 20]);
    Ok(())
}

#[test]
fn corrupt_input_errors() {
    // Copy offset beyond the output produced so far.
    let bad = [0x05, 0x0a, 0x09, 0x00];
    let err = Decoder::new().decompress_vec(&bad);
    assert!(err.is_err());
    // Header claims more bytes than the body yields.
    let short = b"\x20\x28Hello World";
    assert!(Decoder::new().decompress_vec(short).is_err());
    // Error converts into io::Error, as parquet's `?` needs.
    let io_err: std::io::Error = Error::Empty.into();
    assert_eq!(io_err.kind(), std::io::ErrorKind::Other);
}

#[test]
fn framed_roundtrips() -> TestResult {
    let data = sample(300_000);

    // compat writer -> compat reader
    let mut enc = write::FrameEncoder::new(Vec::new());
    for piece in data.chunks(10_001) {
        enc.write_all(piece)?;
    }
    let framed = enc.into_inner().map_err(|e| e.into_error())?;
    assert!(framed.starts_with(b"\xff\x06\x00\x00sNaPpY"));
    let mut out = Vec::new();
    read::FrameDecoder::new(&framed[..]).read_to_end(&mut out)?;
    assert_eq!(out, data);

    // oxiarc-snappy's own framed encoder -> compat reader
    let mut native = oxiarc_snappy::FrameEncoder::new(Vec::new());
    native.write_all(&data)?;
    let native = native.finish()?;
    let mut out = Vec::new();
    read::FrameDecoder::new(&native[..]).read_to_end(&mut out)?;
    assert_eq!(out, data);

    // compat read-side encoder -> oxiarc-snappy's decoder
    let mut framed2 = Vec::new();
    read::FrameEncoder::new(&data[..]).read_to_end(&mut framed2)?;
    let mut out = Vec::new();
    oxiarc_snappy::FrameDecoder::new(&framed2[..]).read_to_end(&mut out)?;
    assert_eq!(out, data);
    Ok(())
}

#[test]
fn frame_encoder_flushes_on_drop() -> TestResult {
    let mut buf = Vec::new();
    {
        let mut enc = write::FrameEncoder::new(&mut buf);
        enc.write_all(b"dropped")?;
    }
    let mut out = Vec::new();
    read::FrameDecoder::new(&buf[..]).read_to_end(&mut out)?;
    assert_eq!(out, b"dropped");
    Ok(())
}

#[test]
fn framed_checksum_mismatch_detected() -> TestResult {
    let mut enc = write::FrameEncoder::new(Vec::new());
    enc.write_all(&sample(5000))?;
    let mut framed = enc.into_inner().map_err(|e| e.into_error())?;
    // Flip a CRC byte of the first data chunk (after the 10-byte stream id
    // and 4-byte chunk header).
    framed[14] ^= 0xff;
    let mut out = Vec::new();
    assert!(
        read::FrameDecoder::new(&framed[..])
            .read_to_end(&mut out)
            .is_err()
    );
    Ok(())
}

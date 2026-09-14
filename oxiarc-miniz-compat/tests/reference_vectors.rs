//! Streams produced by CPython's zlib (the reference implementation) decode
//! through every miniz-shaped entry point, and our streams are accepted by
//! it (oracle test self-skips without python3).

use std::io::Write;
use std::process::{Command, Stdio};

use oxiarc_miniz_compat::inflate::core::inflate_flags::{
    TINFL_FLAG_PARSE_ZLIB_HEADER, TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use oxiarc_miniz_compat::inflate::core::{DecompressorOxide, decompress};
use oxiarc_miniz_compat::inflate::stream::{InflateState, inflate};
use oxiarc_miniz_compat::inflate::{TINFLStatus, decompress_to_vec, decompress_to_vec_zlib};
use oxiarc_miniz_compat::{DataFormat, MZFlush, MZStatus, deflate};

/// `zlib.compress(b'Hello, miniz_oxide compatibility! ' * 20, 9)`
const ZLIB_L9: [u8; 50] = [
    120, 218, 243, 72, 205, 201, 201, 215, 81, 200, 205, 204, 203, 172, 138, 207, 175, 200, 76, 73,
    85, 72, 206, 207, 45, 72, 44, 201, 76, 202, 204, 201, 44, 169, 84, 84, 240, 24, 85, 49, 170,
    98, 192, 85, 0, 0, 182, 70, 254, 137,
];

/// The same text, raw DEFLATE at level 1 (`wbits = -15`).
const RAW_L1: [u8; 44] = [
    243, 72, 205, 201, 201, 215, 81, 200, 205, 204, 203, 172, 138, 207, 175, 200, 76, 73, 85, 72,
    206, 207, 45, 72, 44, 201, 76, 202, 204, 201, 44, 169, 84, 84, 240, 24, 85, 49, 26, 30, 3, 158,
    62, 0,
];

fn text() -> Vec<u8> {
    b"Hello, miniz_oxide compatibility! ".repeat(20)
}

#[test]
fn one_shot_helpers() {
    assert_eq!(decompress_to_vec_zlib(&ZLIB_L9).ok(), Some(text()));
    assert_eq!(decompress_to_vec(&RAW_L1).ok(), Some(text()));
}

#[test]
fn core_decompress_exact_like_backtrace() {
    let expected = text();
    let mut out = vec![0u8; expected.len()];
    let (status, read, written) = decompress(
        &mut DecompressorOxide::new(),
        &ZLIB_L9,
        &mut out,
        0,
        TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF | TINFL_FLAG_PARSE_ZLIB_HEADER,
    );
    assert_eq!(status, TINFLStatus::Done);
    assert_eq!((read, written), (ZLIB_L9.len(), expected.len()));
    assert_eq!(out, expected);
}

#[test]
fn stream_inflate_byte_at_a_time() {
    let mut state = InflateState::new(DataFormat::Zlib);
    let mut out = vec![0u8; 1024];
    let mut produced = 0;
    for (i, byte) in ZLIB_L9.iter().enumerate() {
        let res = inflate(
            &mut state,
            std::slice::from_ref(byte),
            &mut out[produced..],
            MZFlush::None,
        );
        produced += res.bytes_written;
        if i + 1 == ZLIB_L9.len() {
            assert_eq!(res.status, Ok(MZStatus::StreamEnd));
        }
    }
    assert_eq!(&out[..produced], &text()[..]);
}

#[test]
fn python_accepts_our_output() -> Result<(), Box<dyn std::error::Error>> {
    let data: Vec<u8> = (0..20_000u32).map(|i| (i % 91) as u8).collect();
    for level in [0u8, 1, 6, 10] {
        let packed = deflate::compress_to_vec_zlib(&data, level);
        let child = Command::new("python3")
            .args([
                "-c",
                "import sys,zlib; sys.stdout.buffer.write(zlib.decompress(sys.stdin.buffer.read()))",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("python3 not available; skipping oracle");
            return Ok(());
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&packed)?;
        }
        let output = child.wait_with_output()?;
        assert!(output.status.success(), "python rejected level {level}");
        assert_eq!(output.stdout, data);
    }
    Ok(())
}

//! lz4_flex-shaped API behaviour, including Parquet's exact LZ4 / LZ4_RAW /
//! Hadoop call patterns and interop with the reference `lz4` CLI (skipped
//! when it is not installed).

use std::io::{Read, Write};
use std::process::{Command, Stdio};

use oxiarc_lz4flex_compat::block::{self, DecompressError};
use oxiarc_lz4flex_compat::frame::{self, BlockSize, FrameInfo};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn sample(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            if i % 500 < 300 {
                b"column"[i % 6]
            } else {
                (i * 2_654_435_761 % 251) as u8
            }
        })
        .collect()
}

/// `parquet::compression::lz4_raw_codec`.
#[test]
fn parquet_lz4_raw_pattern() -> TestResult {
    for len in [1usize, 17, 4096, 300_000] {
        let input = sample(len);
        let mut output_buf = vec![9u8; 3];
        let offset = output_buf.len();
        let required_len = block::get_maximum_output_size(input.len());
        output_buf.resize(offset + required_len, 0);
        let n = block::compress_into(&input, &mut output_buf[offset..])?;
        output_buf.truncate(offset + n);

        let mut out = vec![0u8; input.len()];
        let got = block::decompress_into(&output_buf[offset..], &mut out)?;
        assert_eq!(got, input.len());
        assert_eq!(out, input);
    }
    Ok(())
}

#[test]
fn decompress_into_too_small_reports_expected() {
    let input = sample(10_000);
    let compressed = block::compress(&input);
    let mut small = vec![0u8; 100];
    match block::decompress_into(&compressed, &mut small) {
        Err(DecompressError::OutputTooSmall { expected, actual }) => {
            assert_eq!(expected, 10_000);
            assert_eq!(actual, 100);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(block::decompress_into(&[0xf0], &mut small).is_err());
}

#[test]
#[allow(deprecated)]
fn root_reexports_and_size_prepended() -> TestResult {
    let input = sample(5000);
    let packed = oxiarc_lz4flex_compat::compress_prepend_size(&input);
    assert_eq!(&packed[..4], &5000u32.to_le_bytes());
    assert_eq!(
        oxiarc_lz4flex_compat::decompress_size_prepended(&packed)?,
        input
    );
    let mut out = vec![0u8; 5000];
    let n = oxiarc_lz4flex_compat::decompress_into(&packed[4..], &mut out)?;
    assert_eq!(n, 5000);
    let dict = b"column column column";
    let with_dict = block::compress_prepend_size_with_dict(&input, dict);
    assert_eq!(
        block::decompress_size_prepended_with_dict(&with_dict, dict)?,
        input
    );
    Ok(())
}

/// `parquet::compression::lz4_codec` (frame, 4 KiB write/read buffers).
#[test]
fn parquet_lz4_frame_pattern() -> TestResult {
    const LZ4_BUFFER_SIZE: usize = 4096;
    let input_buf = sample(123_457);
    let mut output_buf = Vec::new();
    let mut encoder = frame::FrameEncoder::new(&mut output_buf);
    let mut from = 0;
    loop {
        let to = std::cmp::min(from + LZ4_BUFFER_SIZE, input_buf.len());
        encoder.write_all(&input_buf[from..to])?;
        from += LZ4_BUFFER_SIZE;
        if from >= input_buf.len() {
            break;
        }
    }
    encoder.finish()?;

    let mut decoder = frame::FrameDecoder::new(&output_buf[..]);
    let mut buffer = [0u8; LZ4_BUFFER_SIZE];
    let mut decoded = Vec::new();
    loop {
        let len = decoder.read(&mut buffer)?;
        if len == 0 {
            break;
        }
        decoded.write_all(&buffer[..len])?;
    }
    assert_eq!(decoded, input_buf);
    Ok(())
}

#[test]
fn frame_info_options_and_auto_finish() -> TestResult {
    let input = sample(700_000);
    let info = FrameInfo::new()
        .block_size(BlockSize::Max256KB)
        .block_checksums(true)
        .content_checksum(true)
        .content_size(Some(input.len() as u64));
    let mut enc = frame::FrameEncoder::with_frame_info(info, Vec::new());
    enc.write_all(&input)?;
    let framed = enc.finish()?;
    let mut out = Vec::new();
    frame::FrameDecoder::new(&framed[..]).read_to_end(&mut out)?;
    assert_eq!(out, input);

    let mut buf = Vec::new();
    {
        let mut auto = frame::FrameEncoder::new(&mut buf).auto_finish();
        auto.write_all(b"finished by drop")?;
    }
    let mut out = Vec::new();
    frame::FrameDecoder::new(&buf[..]).read_to_end(&mut out)?;
    assert_eq!(out, b"finished by drop");
    Ok(())
}

#[test]
fn corrupt_frame_is_invalid_data() -> TestResult {
    let mut enc =
        frame::FrameEncoder::with_frame_info(FrameInfo::new().content_checksum(true), Vec::new());
    enc.write_all(&sample(50_000))?;
    let mut framed = enc.finish()?;
    let n = framed.len();
    framed[n - 2] ^= 0x55;
    let err = frame::FrameDecoder::new(&framed[..]).read_to_end(&mut Vec::new());
    assert!(err.is_err());
    let e: frame::Error = std::io::Error::other("x").into();
    assert!(matches!(e, frame::Error::IoError(_)));
    Ok(())
}

fn lz4_cli(args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("lz4")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let data = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&data));
    let output = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    output.status.success().then_some(output.stdout)
}

/// Interop with the reference `lz4` CLI in both directions, including its
/// default linked-block mode.
#[test]
fn reference_cli_interop() -> TestResult {
    let input = sample(400_000);
    let Some(linked) = lz4_cli(&["-c", "-BD"], &input) else {
        eprintln!("lz4 CLI not available; skipping");
        return Ok(());
    };
    let mut out = Vec::new();
    frame::FrameDecoder::new(&linked[..]).read_to_end(&mut out)?;
    assert_eq!(out, input, "linked-block frame from lz4 -BD");

    if let Some(indep) = lz4_cli(&["-c", "-BI", "--content-size"], &input) {
        let mut out = Vec::new();
        frame::FrameDecoder::new(&indep[..]).read_to_end(&mut out)?;
        assert_eq!(out, input, "independent frame from lz4 -BI");
    }

    let mut enc = frame::FrameEncoder::new(Vec::new());
    enc.write_all(&input)?;
    let ours = enc.finish()?;
    let decoded = lz4_cli(&["-d", "-c"], &ours).ok_or("lz4 -d rejected our frame")?;
    assert_eq!(decoded, input);
    Ok(())
}

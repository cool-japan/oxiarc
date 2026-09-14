//! Extra streaming compression functionality: zlib's `deflate()` over a
//! [`CompressorOxide`].

use super::core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress};
use crate::{MZError, MZFlush, MZStatus, StreamResult};

/// Try to compress from `input` to `output` with the given
/// [`CompressorOxide`].
///
/// Mirrors miniz: an empty `output` is `Err(MZError::Buf)`; after the
/// stream is done, `Finish` keeps returning `StreamEnd` and anything else is
/// `Err(MZError::Buf)`.
pub fn deflate(
    compressor: &mut CompressorOxide,
    input: &[u8],
    output: &mut [u8],
    flush: MZFlush,
) -> StreamResult {
    if output.is_empty() {
        return StreamResult::error(MZError::Buf);
    }
    if compressor.prev_return_status() == TDEFLStatus::Done {
        return if flush == MZFlush::Finish {
            StreamResult {
                bytes_written: 0,
                bytes_consumed: 0,
                status: Ok(MZStatus::StreamEnd),
            }
        } else {
            StreamResult::error(MZError::Buf)
        };
    }

    let mut bytes_written = 0;
    let mut bytes_consumed = 0;
    let mut next_in = input;
    let mut next_out = output;

    let status = loop {
        let (defl_status, in_bytes, out_bytes) =
            compress(compressor, next_in, next_out, TDEFLFlush::from(flush));
        next_in = &next_in[in_bytes..];
        next_out = &mut next_out[out_bytes..];
        bytes_consumed += in_bytes;
        bytes_written += out_bytes;

        match defl_status {
            TDEFLStatus::BadParam => break Err(MZError::Param),
            TDEFLStatus::PutBufFailed => break Err(MZError::Stream),
            TDEFLStatus::Done => break Ok(MZStatus::StreamEnd),
            TDEFLStatus::Okay => {}
        }
        if next_out.is_empty() {
            break Ok(MZStatus::Ok);
        }
        if next_in.is_empty() && flush != MZFlush::Finish {
            let total_changed = bytes_written > 0 || bytes_consumed > 0;
            break if flush != MZFlush::None || total_changed {
                Ok(MZStatus::Ok)
            } else {
                Err(MZError::Buf)
            };
        }
        if in_bytes == 0 && out_bytes == 0 {
            break if bytes_written > 0 || bytes_consumed > 0 {
                Ok(MZStatus::Ok)
            } else {
                Err(MZError::Buf)
            };
        }
    };
    StreamResult {
        bytes_consumed,
        bytes_written,
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deflate::core::create_comp_flags_from_zip_params;

    #[test]
    fn stream_deflate_small_output() {
        let data: Vec<u8> = (0..40_000u32).map(|i| (i % 17) as u8).collect();
        let mut c = CompressorOxide::new(create_comp_flags_from_zip_params(9, -15, 0));
        let mut out = [0u8; 100];
        let mut packed = Vec::new();
        let mut input = &data[..];
        loop {
            let res = deflate(&mut c, input, &mut out, MZFlush::Finish);
            input = &input[res.bytes_consumed..];
            packed.extend_from_slice(&out[..res.bytes_written]);
            match res.status {
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) => {}
                Err(e) => panic!("deflate error {e:?}"),
            }
        }
        assert_eq!(crate::inflate::decompress_to_vec(&packed).ok(), Some(data));
    }
}

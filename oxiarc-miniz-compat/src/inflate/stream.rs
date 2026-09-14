//! Extra streaming decompression functionality: zlib's `inflate()` over an
//! [`InflateState`].

use super::TINFLStatus;
use super::core::inflate_flags::{
    TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_IGNORE_ADLER32, TINFL_FLAG_PARSE_ZLIB_HEADER,
    TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
};
use super::core::{DecompressorOxide, decompress};
use crate::{DataFormat, MZError, MZFlush, MZStatus, StreamResult};

/// Tag that determines reset policy of [`InflateState`].
pub trait ResetPolicy {
    /// Performs reset.
    fn reset(&self, state: &mut InflateState);
}

/// Resets state, without performing expensive ops (e.g. zeroing buffer).
#[derive(Debug, Clone, Copy)]
pub struct MinReset;

impl ResetPolicy for MinReset {
    fn reset(&self, state: &mut InflateState) {
        let format = state.data_format;
        state.reset(format);
    }
}

/// Resets state and zero memory, continuing to use the same data format.
#[derive(Debug, Clone, Copy)]
pub struct ZeroReset;

impl ResetPolicy for ZeroReset {
    fn reset(&self, state: &mut InflateState) {
        let format = state.data_format;
        state.reset(format);
    }
}

/// Full reset of the state, including zeroing memory. Requires to provide
/// new data format.
#[derive(Debug, Clone, Copy)]
pub struct FullReset(pub DataFormat);

impl ResetPolicy for FullReset {
    fn reset(&self, state: &mut InflateState) {
        state.reset(self.0);
    }
}

/// A struct that keeps track of the state of the inflate stream.
#[derive(Debug)]
pub struct InflateState {
    decomp: DecompressorOxide,
    data_format: DataFormat,
    /// Status of the last call, used to replay errors.
    last_status: TINFLStatus,
    has_flushed: bool,
}

impl Default for InflateState {
    fn default() -> Self {
        InflateState::new(DataFormat::Raw)
    }
}

impl InflateState {
    /// Create a new state.
    pub fn new(data_format: DataFormat) -> InflateState {
        InflateState {
            decomp: DecompressorOxide::new(),
            data_format,
            last_status: TINFLStatus::NeedsMoreInput,
            has_flushed: false,
        }
    }

    /// Create a new state on the heap.
    pub fn new_boxed(data_format: DataFormat) -> Box<InflateState> {
        Box::new(InflateState::new(data_format))
    }

    /// Access the inner decompressor.
    pub fn decompressor(&mut self) -> &mut DecompressorOxide {
        &mut self.decomp
    }

    /// Return the status of the last call to `inflate` with this
    /// `InflateState`.
    pub const fn last_status(&self) -> TINFLStatus {
        self.last_status
    }

    /// Create a new state using miniz/zlib style window bits parameter.
    pub fn new_boxed_with_window_bits(window_bits: i32) -> Box<InflateState> {
        InflateState::new_boxed(DataFormat::from_window_bits(window_bits))
    }

    /// Reset the decompressor without re-allocating memory, using the given
    /// data format.
    pub fn reset(&mut self, data_format: DataFormat) {
        *self = InflateState::new(data_format);
    }

    /// Resets the state according to specified policy.
    pub fn reset_as<T: ResetPolicy>(&mut self, policy: T) {
        policy.reset(self);
    }
}

/// Try to decompress from `input` to `output` with the given
/// [`InflateState`].
///
/// # Flushing
///
/// [`MZFlush::Finish`] marks `input` as the end of the stream; an
/// incomplete stream then yields `Err(MZError::Buf)`. [`MZFlush::Full`] is
/// rejected with `Err(MZError::Stream)`, as in miniz.
pub fn inflate(
    state: &mut InflateState,
    input: &[u8],
    output: &mut [u8],
    flush: MZFlush,
) -> StreamResult {
    if flush == MZFlush::Full {
        return StreamResult::error(MZError::Stream);
    }
    match state.last_status {
        TINFLStatus::FailedCannotMakeProgress => return StreamResult::error(MZError::Buf),
        s if (s as i8) < 0 => return StreamResult::error(MZError::Data),
        _ => {}
    }
    if state.has_flushed && flush != MZFlush::Finish {
        return StreamResult::error(MZError::Stream);
    }
    state.has_flushed |= flush == MZFlush::Finish;

    let mut flags = TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    match state.data_format {
        DataFormat::Zlib => flags |= TINFL_FLAG_PARSE_ZLIB_HEADER,
        DataFormat::ZLibIgnoreChecksum => {
            flags |= TINFL_FLAG_PARSE_ZLIB_HEADER | TINFL_FLAG_IGNORE_ADLER32;
        }
        DataFormat::Raw => {}
    }
    if flush != MZFlush::Finish {
        flags |= TINFL_FLAG_HAS_MORE_INPUT;
    }

    let (status, consumed, written) = decompress(&mut state.decomp, input, output, 0, flags);
    state.last_status = status;
    let result = match status {
        TINFLStatus::Done => Ok(MZStatus::StreamEnd),
        TINFLStatus::FailedCannotMakeProgress => Err(MZError::Buf),
        s if (s as i8) < 0 => Err(MZError::Data),
        _ if consumed == 0 && written == 0 => Err(MZError::Buf),
        _ if flush == MZFlush::Finish && status == TINFLStatus::HasMoreOutput => Err(MZError::Buf),
        _ => Ok(MZStatus::Ok),
    };
    StreamResult {
        bytes_consumed: consumed,
        bytes_written: written,
        status: result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_inflate_chunked() {
        let data: Vec<u8> = (0..50_000u32).map(|i| (i % 13) as u8).collect();
        let z = crate::deflate::compress_to_vec_zlib(&data, 6);
        let mut state = InflateState::new_boxed(DataFormat::Zlib);
        let mut out = vec![0u8; 777];
        let mut decoded = Vec::new();
        let mut input = &z[..];
        loop {
            let res = inflate(&mut state, input, &mut out, MZFlush::None);
            input = &input[res.bytes_consumed..];
            decoded.extend_from_slice(&out[..res.bytes_written]);
            match res.status {
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) => {}
                Err(e) => panic!("inflate error {e:?}"),
            }
        }
        assert!(input.is_empty());
        assert_eq!(decoded, data);
    }

    #[test]
    fn finish_on_truncated_is_buf_error() {
        let z = crate::deflate::compress_to_vec(&[1u8; 5000], 6);
        let mut state = InflateState::new(DataFormat::Raw);
        let mut out = vec![0u8; 10_000];
        let res = inflate(&mut state, &z[..z.len() / 2], &mut out, MZFlush::Finish);
        assert_eq!(res.status, Err(MZError::Buf));
    }
}

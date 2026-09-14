//! The raw Snappy block format (no framing, no checksums).

use crate::error::from_snappy;
use crate::{Error, MAX_INPUT_SIZE, Result};

/// Returns the maximum compressed size given the uncompressed size.
///
/// If the uncompressed size exceeds the maximum allowable size then this
/// returns 0.
pub fn max_compress_len(input_len: usize) -> usize {
    let input_len = input_len as u64;
    if input_len > MAX_INPUT_SIZE {
        return 0;
    }
    let max = 32 + input_len + (input_len / 6);
    if max > MAX_INPUT_SIZE {
        0
    } else {
        max as usize
    }
}

/// Returns the decompressed size (in bytes) of the compressed bytes given.
///
/// # Errors
///
/// [`Error::Header`] when the varint length header is invalid.
pub fn decompress_len(input: &[u8]) -> Result<usize> {
    if input.is_empty() {
        return Ok(0);
    }
    Ok(read_header(input)?.0)
}

/// Decode the varint length header: `(decompressed_len, header_len)`.
fn read_header(input: &[u8]) -> Result<(usize, usize)> {
    let mut value: u64 = 0;
    for (i, &byte) in input.iter().enumerate().take(5) {
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            if value > MAX_INPUT_SIZE {
                return Err(Error::TooBig {
                    given: value,
                    max: MAX_INPUT_SIZE,
                });
            }
            return Ok((value as usize, i + 1));
        }
    }
    Err(Error::Header)
}

/// Encoder is a raw encoder for compressing bytes in the Snappy format.
///
/// This encoder does not use the Snappy frame format and simply compresses
/// the given bytes in one big Snappy block (that is, it has a single
/// header).
#[derive(Debug, Default)]
pub struct Encoder {
    _priv: (),
}

impl Encoder {
    /// Return a new encoder that can be used for compressing bytes.
    pub fn new() -> Encoder {
        Encoder { _priv: () }
    }

    /// Compresses all bytes in `input` into `output`.
    ///
    /// `output` must be large enough to hold the maximum possible compressed
    /// size of `input`, which can be computed using [`max_compress_len`].
    /// On success, this returns the number of bytes written to `output`.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for inputs over `u32::MAX` bytes;
    /// [`Error::BufferTooSmall`] when `output` is smaller than
    /// `max_compress_len(input.len())`.
    pub fn compress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        if input.len() as u64 > MAX_INPUT_SIZE {
            return Err(Error::TooBig {
                given: input.len() as u64,
                max: MAX_INPUT_SIZE,
            });
        }
        let min = max_compress_len(input.len());
        if output.len() < min {
            return Err(Error::BufferTooSmall {
                given: output.len() as u64,
                min: min as u64,
            });
        }
        let compressed = oxiarc_snappy::compress(input);
        if compressed.len() > output.len() {
            return Err(Error::BufferTooSmall {
                given: output.len() as u64,
                min: compressed.len() as u64,
            });
        }
        output[..compressed.len()].copy_from_slice(&compressed);
        Ok(compressed.len())
    }

    /// Compresses all bytes in `input` into a freshly allocated `Vec`.
    ///
    /// # Errors
    ///
    /// [`Error::TooBig`] for inputs over `u32::MAX` bytes.
    pub fn compress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        if input.len() as u64 > MAX_INPUT_SIZE {
            return Err(Error::TooBig {
                given: input.len() as u64,
                max: MAX_INPUT_SIZE,
            });
        }
        Ok(oxiarc_snappy::compress(input))
    }
}

/// Decoder is a raw decoder for decompressing bytes in the Snappy format.
///
/// This decoder does not use the Snappy frame format and simply
/// decompresses the given bytes as if it were returned from `Encoder`.
#[derive(Debug, Default)]
pub struct Decoder {
    _priv: (),
}

impl Decoder {
    /// Return a new decoder that can be used for decompressing bytes.
    pub fn new() -> Decoder {
        Decoder { _priv: () }
    }

    /// Decompresses all bytes in `input` into `output`.
    ///
    /// `output` must be large enough to hold all decompressed bytes (see
    /// [`decompress_len`]). On success, this returns the number of bytes
    /// written to `output`.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] for empty input, [`Error::BufferTooSmall`], or any
    /// corruption error.
    pub fn decompress(&mut self, input: &[u8], output: &mut [u8]) -> Result<usize> {
        if input.is_empty() {
            return Err(Error::Empty);
        }
        let (len, _) = read_header(input)?;
        if output.len() < len {
            return Err(Error::BufferTooSmall {
                given: output.len() as u64,
                min: len as u64,
            });
        }
        let decoded = oxiarc_snappy::decompress(input).map_err(|e| from_snappy(e, len))?;
        if decoded.len() != len {
            return Err(Error::HeaderMismatch {
                expected_len: len as u64,
                got_len: decoded.len() as u64,
            });
        }
        output[..len].copy_from_slice(&decoded);
        Ok(len)
    }

    /// Decompresses all bytes in `input` into a freshly allocated `Vec`.
    ///
    /// # Errors
    ///
    /// See [`Decoder::decompress`].
    pub fn decompress_vec(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut buf = vec![0; decompress_len(input)?];
        let n = self.decompress(input, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_and_bounds() {
        assert_eq!(max_compress_len(0), 32);
        assert_eq!(max_compress_len(600), 32 + 600 + 100);
        assert_eq!(decompress_len(&[0xfe, 0xff, 0x7f]), Ok(2_097_150));
        assert_eq!(
            decompress_len(&[0x80, 0x80, 0x80, 0x80, 0x80]),
            Err(Error::Header)
        );
        assert_eq!(Decoder::new().decompress(&[], &mut []), Err(Error::Empty));
    }

    #[test]
    fn buffer_too_small() {
        let mut enc = Encoder::new();
        let mut out = [0u8; 10];
        assert!(matches!(
            enc.compress(b"hello", &mut out),
            Err(Error::BufferTooSmall { .. })
        ));
        let compressed = enc.compress_vec(&[b'x'; 100]).unwrap_or_default();
        let mut small = [0u8; 50];
        assert!(matches!(
            Decoder::new().decompress(&compressed, &mut small),
            Err(Error::BufferTooSmall {
                given: 50,
                min: 100
            })
        ));
    }
}

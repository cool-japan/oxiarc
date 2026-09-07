//! `Content-Encoding: dcb` — dictionary-compressed Brotli (RFC 9842).
//!
//! A `dcb` body is a Brotli stream with a 36-byte preamble that names the
//! dictionary it was compressed against:
//!
//! ```text
//! FF 44 43 42            4-byte magic
//! <32 bytes>             SHA-256 of the dictionary
//! <brotli stream>        compressed against that dictionary
//! ```
//!
//! The preamble exists so a recipient can check it holds the right dictionary
//! before decoding; the compressed body itself is an ordinary shared-dictionary
//! Brotli stream (see [`crate::shared_dict`]). Everything here is framing —
//! [`parse_header`] hands back the brotli stream, and an HTTP client that
//! already knows which dictionary it advertised can simply feed that to
//! [`BrotliStream::with_dictionary`](crate::BrotliStream::with_dictionary).
//!
//! # Example
//!
//! ```rust
//! use oxiarc_brotli::{dcb, BrotliParams};
//!
//! let dictionary = b"<nav class=\"site\"><a href=\"/\">home</a>".repeat(32);
//! let page = b"<nav class=\"site\"><a href=\"/\">home</a><main>hello</main>";
//!
//! let params = BrotliParams { quality: 9, ..BrotliParams::default() };
//! let body = dcb::compress(page, &dictionary, &params).expect("compress");
//!
//! assert_eq!(&body[..4], &dcb::DCB_MAGIC);
//! assert_eq!(&body[4..36], &dcb::dictionary_id(&dictionary));
//! assert_eq!(dcb::decompress(&body, &dictionary).expect("decompress"), page);
//! ```

use oxiarc_core::sha256::{Sha256, hex32};

use crate::compress::{BrotliParams, compress_with_dictionary};
use crate::decompress::{decompress_with_dictionary, decompress_with_dictionary_and_limit};
use crate::error::{BrotliError, BrotliResult};

/// The four magic bytes that open a `dcb` body: `FF 44 43 42`.
pub const DCB_MAGIC: [u8; 4] = [0xFF, 0x44, 0x43, 0x42];

/// Total length of the `dcb` preamble: 4 magic bytes plus a 32-byte digest.
pub const DCB_HEADER_LEN: usize = 36;

/// The 32-byte SHA-256 digest that identifies a dictionary in a `dcb` body and
/// in the `Available-Dictionary` request header.
#[must_use]
pub fn dictionary_id(dictionary: &[u8]) -> [u8; 32] {
    Sha256::compute(dictionary)
}

/// Build the 36-byte `dcb` preamble for `dictionary`.
#[must_use]
pub fn write_header(dictionary: &[u8]) -> [u8; DCB_HEADER_LEN] {
    let mut header = [0u8; DCB_HEADER_LEN];
    header[..4].copy_from_slice(&DCB_MAGIC);
    header[4..].copy_from_slice(&dictionary_id(dictionary));
    header
}

/// Split a `dcb` body into the dictionary digest it names and the Brotli
/// stream that follows.
///
/// The digest is *not* checked against any dictionary here; that is
/// [`verify_header`]'s job, and a caller that already knows which dictionary it
/// advertised may skip it.
///
/// # Errors
///
/// [`BrotliError::CorruptedData`] when `body` is shorter than
/// [`DCB_HEADER_LEN`] or does not start with [`DCB_MAGIC`].
pub fn parse_header(body: &[u8]) -> BrotliResult<([u8; 32], &[u8])> {
    if body.len() < DCB_HEADER_LEN {
        return Err(BrotliError::CorruptedData(format!(
            "dcb body of {} bytes is shorter than its {DCB_HEADER_LEN}-byte header",
            body.len()
        )));
    }
    if body[..4] != DCB_MAGIC {
        return Err(BrotliError::CorruptedData(
            "dcb body does not start with the FF 44 43 42 magic".to_string(),
        ));
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&body[4..DCB_HEADER_LEN]);
    Ok((id, &body[DCB_HEADER_LEN..]))
}

/// [`parse_header`], additionally checking that `dictionary` is the one the
/// body names.
///
/// # Errors
///
/// [`parse_header`]'s errors, plus [`BrotliError::DictionaryError`] when the
/// digest does not match `dictionary`.
pub fn verify_header<'a>(body: &'a [u8], dictionary: &[u8]) -> BrotliResult<&'a [u8]> {
    let (id, stream) = parse_header(body)?;
    let expected = dictionary_id(dictionary);
    if id != expected {
        return Err(BrotliError::DictionaryError(format!(
            "dcb body names dictionary {} but {} was supplied",
            hex32(&id),
            hex32(&expected)
        )));
    }
    Ok(stream)
}

/// Compress `data` against `dictionary` and prepend the `dcb` preamble.
///
/// # Errors
///
/// The errors of [`compress_with_dictionary`].
pub fn compress(data: &[u8], dictionary: &[u8], params: &BrotliParams) -> BrotliResult<Vec<u8>> {
    let stream = compress_with_dictionary(data, dictionary, params)?;
    let mut body = Vec::with_capacity(DCB_HEADER_LEN + stream.len());
    body.extend_from_slice(&write_header(dictionary));
    body.extend_from_slice(&stream);
    Ok(body)
}

/// Verify a `dcb` body's dictionary digest and decompress it.
///
/// # Errors
///
/// [`verify_header`]'s errors and those of
/// [`decompress_with_dictionary`].
pub fn decompress(body: &[u8], dictionary: &[u8]) -> BrotliResult<Vec<u8>> {
    decompress_with_dictionary(verify_header(body, dictionary)?, dictionary)
}

/// [`decompress`] with the caller-chosen output budget every untrusted body
/// deserves.
///
/// # Errors
///
/// [`verify_header`]'s errors and those of
/// [`decompress_with_dictionary_and_limit`].
pub fn decompress_with_limit(
    body: &[u8],
    dictionary: &[u8],
    max_output: usize,
) -> BrotliResult<Vec<u8>> {
    decompress_with_dictionary_and_limit(verify_header(body, dictionary)?, dictionary, max_output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `dcb` dictionary id is a plain SHA-256 of the dictionary bytes,
    /// so the FIPS 180-4 example digests must come straight back out of
    /// [`dictionary_id`]. This is what pins the identifier to the hash RFC
    /// 9842 names, independently of `oxiarc-core`'s own SHA-256 tests.
    #[test]
    fn a_dictionary_id_is_the_sha256_of_the_dictionary() {
        for (input, want) in [
            (
                &b""[..],
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                &b"abc"[..],
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                &b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"[..],
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ] {
            assert_eq!(hex32(&dictionary_id(input)), want, "digest of {input:?}");
            assert_eq!(&write_header(input)[4..], &dictionary_id(input));
        }
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            hex32(&dictionary_id(&million_a)),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn header_round_trips() {
        let dict = b"a shared dictionary".repeat(10);
        let header = write_header(&dict);
        assert_eq!(&header[..4], &DCB_MAGIC);
        let mut body = header.to_vec();
        body.extend_from_slice(b"not really brotli");
        let (id, stream) = parse_header(&body).expect("parse");
        assert_eq!(id, dictionary_id(&dict));
        assert_eq!(stream, b"not really brotli");
        assert!(verify_header(&body, &dict).is_ok());
        assert!(verify_header(&body, b"a different dictionary").is_err());
    }

    #[test]
    fn a_short_or_unmagic_body_is_rejected() {
        assert!(parse_header(&[]).is_err());
        assert!(parse_header(&[0xFF; DCB_HEADER_LEN - 1]).is_err());
        let mut wrong = vec![0u8; DCB_HEADER_LEN];
        wrong[..4].copy_from_slice(&[0xFF, 0x44, 0x43, 0x00]);
        assert!(parse_header(&wrong).is_err());
    }
}

//! The three textual chunks: `tEXt`, `zTXt` and `iTXt`.
//!
//! # Keyword rules
//!
//! ISO/IEC 15948 clause 11.3.4.2 is precise about keywords and most decoders
//! are not. A keyword is 1 to 79 bytes of printable Latin-1 — `0x20..=0x7E`
//! and `0xA1..=0xFF` — with **no leading space, no trailing space and no two
//! consecutive spaces**. [`validate_keyword`] implements exactly that, and both
//! the reader and the writer go through it.

use std::io::Write;

use crate::chunk;
use crate::error::{DecodingError, EncodingError, EncodingFormatErrorKind, FormatErrorKind};

/// The largest text payload this crate will decompress out of a `zTXt` or
/// `iTXt` chunk, matching the `png` crate's limit.
pub const DECOMPRESSION_LIMIT: usize = 2 * 1024 * 1024;

/// Check a keyword against clause 11.3.4.2.
///
/// ```
/// use oxiarc_png::text_metadata::validate_keyword;
/// assert!(validate_keyword("Title").is_ok());
/// assert!(validate_keyword("Two words").is_ok());
/// assert!(validate_keyword("").is_err());              // empty
/// assert!(validate_keyword(" Title").is_err());        // leading space
/// assert!(validate_keyword("Title ").is_err());        // trailing space
/// assert!(validate_keyword("Two  spaces").is_err());   // consecutive spaces
/// assert!(validate_keyword("Tab\there").is_err());     // not printable Latin-1
/// ```
pub fn validate_keyword(keyword: &str) -> Result<(), DecodingError> {
    let bytes = encode_latin1(keyword).ok_or(FormatErrorKind::InvalidTextKeyword)?;
    validate_keyword_bytes(&bytes)
}

/// As [`validate_keyword`] but on the raw Latin-1 bytes read from a file.
pub fn validate_keyword_bytes(keyword: &[u8]) -> Result<(), DecodingError> {
    if keyword.is_empty() || keyword.len() > 79 {
        return Err(FormatErrorKind::InvalidTextKeyword.into());
    }
    if keyword[0] == b' ' || keyword[keyword.len() - 1] == b' ' {
        return Err(FormatErrorKind::InvalidTextKeyword.into());
    }
    let mut previous_space = false;
    for &b in keyword {
        let printable = (0x20..=0x7E).contains(&b) || (0xA1..=0xFF).contains(&b);
        if !printable {
            return Err(FormatErrorKind::InvalidTextKeyword.into());
        }
        if b == b' ' {
            if previous_space {
                return Err(FormatErrorKind::InvalidTextKeyword.into());
            }
            previous_space = true;
        } else {
            previous_space = false;
        }
    }
    Ok(())
}

/// Decode Latin-1 bytes into a `String`. Every byte is a valid code point, so
/// this cannot fail.
#[must_use]
pub fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| char::from(*b)).collect()
}

/// Encode a `String` as Latin-1, or `None` when a character does not fit.
#[must_use]
pub fn encode_latin1(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|c| u8::try_from(u32::from(c)).ok())
        .collect()
}

/// Text that may or may not have been decompressed yet.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OptCompressed {
    /// The decompressed text.
    Uncompressed(String),
    /// The raw zlib-compressed bytes, exactly as stored.
    Compressed(Vec<u8>),
}

/// An uncompressed Latin-1 text chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TEXtChunk {
    /// The keyword, 1 to 79 printable Latin-1 characters.
    pub keyword: String,
    /// The text. May be empty and may contain newlines.
    pub text: String,
}

impl TEXtChunk {
    /// Build a chunk from a keyword and its text.
    pub fn new(keyword: impl Into<String>, text: impl Into<String>) -> Self {
        TEXtChunk {
            keyword: keyword.into(),
            text: text.into(),
        }
    }

    /// Parse a `tEXt` payload: `keyword \0 text`.
    pub fn parse(data: &[u8]) -> Result<TEXtChunk, DecodingError> {
        let split = data
            .iter()
            .position(|b| *b == 0)
            .ok_or(FormatErrorKind::BadTextEncoding(
                "tEXt has no null separator",
            ))?;
        validate_keyword_bytes(&data[..split])?;
        Ok(TEXtChunk {
            keyword: decode_latin1(&data[..split]),
            text: decode_latin1(&data[split + 1..]),
        })
    }
}

/// A zlib-compressed Latin-1 text chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZTXtChunk {
    /// The keyword, 1 to 79 printable Latin-1 characters.
    pub keyword: String,
    text: OptCompressed,
}

impl ZTXtChunk {
    /// Build a chunk from an already-decompressed string.
    pub fn new(keyword: impl Into<String>, text: impl Into<String>) -> Self {
        ZTXtChunk {
            keyword: keyword.into(),
            text: OptCompressed::Uncompressed(text.into()),
        }
    }

    /// Parse a `zTXt` payload: `keyword \0 method compressed-text`.
    ///
    /// The text is left compressed; call [`ZTXtChunk::decompress_text`] or
    /// [`ZTXtChunk::get_text`] to expand it under the size limit.
    pub fn parse(data: &[u8]) -> Result<ZTXtChunk, DecodingError> {
        let split = data
            .iter()
            .position(|b| *b == 0)
            .ok_or(FormatErrorKind::BadTextEncoding(
                "zTXt has no null separator",
            ))?;
        validate_keyword_bytes(&data[..split])?;
        let rest = &data[split + 1..];
        let method = *rest
            .first()
            .ok_or(FormatErrorKind::BadTextEncoding("zTXt has no method byte"))?;
        if method != 0 {
            return Err(
                FormatErrorKind::BadTextEncoding("zTXt compression method is not zlib").into(),
            );
        }
        Ok(ZTXtChunk {
            keyword: decode_latin1(&data[..split]),
            text: OptCompressed::Compressed(rest[1..].to_vec()),
        })
    }

    /// Decompress the text in place, bounded by [`DECOMPRESSION_LIMIT`].
    pub fn decompress_text(&mut self) -> Result<(), DecodingError> {
        self.decompress_text_with_limit(DECOMPRESSION_LIMIT)
    }

    /// Decompress the text in place, bounded by `limit` bytes.
    pub fn decompress_text_with_limit(&mut self, limit: usize) -> Result<(), DecodingError> {
        if let OptCompressed::Compressed(bytes) = &self.text {
            let raw = crate::zlib::inflate_zlib_capped(bytes, limit)?;
            self.text = OptCompressed::Uncompressed(decode_latin1(&raw));
        }
        Ok(())
    }

    /// The decompressed text, decompressing first if necessary.
    pub fn get_text(&self) -> Result<String, DecodingError> {
        match &self.text {
            OptCompressed::Uncompressed(text) => Ok(text.clone()),
            OptCompressed::Compressed(bytes) => {
                let raw = crate::zlib::inflate_zlib_capped(bytes, DECOMPRESSION_LIMIT)?;
                Ok(decode_latin1(&raw))
            }
        }
    }

    /// Compress the text in place so it is ready to be written.
    pub fn compress_text(&mut self) -> Result<(), EncodingError> {
        if let OptCompressed::Uncompressed(text) = &self.text {
            let bytes = encode_latin1(text).ok_or(EncodingFormatErrorKind::BadTextEncoding(
                "zTXt text is not representable in Latin-1",
            ))?;
            let compressed = oxiarc_deflate::zlib_compress(&bytes, 6)
                .map_err(|err| EncodingError::IoError(std::io::Error::other(err.to_string())))?;
            self.text = OptCompressed::Compressed(compressed);
        }
        Ok(())
    }
}

/// A UTF-8 international text chunk, optionally compressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ITXtChunk {
    /// The keyword, 1 to 79 printable Latin-1 characters.
    pub keyword: String,
    /// Whether the stored text is compressed.
    pub compressed: bool,
    /// An RFC 3066 language tag, possibly empty.
    pub language_tag: String,
    /// The keyword translated into `language_tag`, possibly empty.
    pub translated_keyword: String,
    text: OptCompressed,
}

impl ITXtChunk {
    /// Build an uncompressed chunk from a keyword and its text.
    pub fn new(keyword: impl Into<String>, text: impl Into<String>) -> Self {
        ITXtChunk {
            keyword: keyword.into(),
            compressed: false,
            language_tag: String::new(),
            translated_keyword: String::new(),
            text: OptCompressed::Uncompressed(text.into()),
        }
    }

    /// Parse an `iTXt` payload:
    /// `keyword \0 flag method language \0 translated \0 text`.
    pub fn parse(data: &[u8]) -> Result<ITXtChunk, DecodingError> {
        let bad = |m: &'static str| DecodingError::from(FormatErrorKind::BadTextEncoding(m));
        let k_end = data
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad("iTXt has no null after the keyword"))?;
        validate_keyword_bytes(&data[..k_end])?;
        let rest = &data[k_end + 1..];
        if rest.len() < 2 {
            return Err(bad("iTXt is missing its compression fields"));
        }
        let compressed = match rest[0] {
            0 => false,
            1 => true,
            _ => return Err(bad("iTXt compression flag is not 0 or 1")),
        };
        if compressed && rest[1] != 0 {
            return Err(bad("iTXt compression method is not zlib"));
        }
        let rest = &rest[2..];
        let l_end = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad("iTXt has no null after the language tag"))?;
        let language_tag = decode_latin1(&rest[..l_end]);
        let rest = &rest[l_end + 1..];
        let t_end = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| bad("iTXt has no null after the translated keyword"))?;
        let translated_keyword = String::from_utf8(rest[..t_end].to_vec())
            .map_err(|_| bad("iTXt translated keyword is not UTF-8"))?;
        let body = &rest[t_end + 1..];
        let text = if compressed {
            OptCompressed::Compressed(body.to_vec())
        } else {
            OptCompressed::Uncompressed(
                String::from_utf8(body.to_vec()).map_err(|_| bad("iTXt text is not UTF-8"))?,
            )
        };
        Ok(ITXtChunk {
            keyword: decode_latin1(&data[..k_end]),
            compressed,
            language_tag,
            translated_keyword,
            text,
        })
    }

    /// Decompress the text in place, bounded by [`DECOMPRESSION_LIMIT`].
    pub fn decompress_text(&mut self) -> Result<(), DecodingError> {
        self.decompress_text_with_limit(DECOMPRESSION_LIMIT)
    }

    /// Decompress the text in place, bounded by `limit` bytes.
    pub fn decompress_text_with_limit(&mut self, limit: usize) -> Result<(), DecodingError> {
        if let OptCompressed::Compressed(bytes) = &self.text {
            let raw = crate::zlib::inflate_zlib_capped(bytes, limit)?;
            let text = String::from_utf8(raw).map_err(|_| {
                DecodingError::from(FormatErrorKind::BadTextEncoding("iTXt text is not UTF-8"))
            })?;
            self.text = OptCompressed::Uncompressed(text);
            self.compressed = false;
        }
        Ok(())
    }

    /// The decompressed text, decompressing first if necessary.
    pub fn get_text(&self) -> Result<String, DecodingError> {
        match &self.text {
            OptCompressed::Uncompressed(text) => Ok(text.clone()),
            OptCompressed::Compressed(bytes) => {
                let raw = crate::zlib::inflate_zlib_capped(bytes, DECOMPRESSION_LIMIT)?;
                String::from_utf8(raw).map_err(|_| {
                    DecodingError::from(FormatErrorKind::BadTextEncoding("iTXt text is not UTF-8"))
                })
            }
        }
    }

    /// Compress the text in place so it is ready to be written.
    pub fn compress_text(&mut self) -> Result<(), EncodingError> {
        if let OptCompressed::Uncompressed(text) = &self.text {
            let compressed = oxiarc_deflate::zlib_compress(text.as_bytes(), 6)
                .map_err(|err| EncodingError::IoError(std::io::Error::other(err.to_string())))?;
            self.text = OptCompressed::Compressed(compressed);
            self.compressed = true;
        }
        Ok(())
    }
}

/// A text chunk that can be written out.
pub trait EncodableTextChunk {
    /// Write the whole chunk, framing and CRC included.
    fn encode<W: Write>(&self, w: &mut W) -> Result<(), EncodingError>;
}

impl EncodableTextChunk for TEXtChunk {
    fn encode<W: Write>(&self, w: &mut W) -> Result<(), EncodingError> {
        let keyword = encode_latin1(&self.keyword).ok_or(
            EncodingFormatErrorKind::BadTextEncoding("keyword is not representable in Latin-1"),
        )?;
        validate_keyword_bytes(&keyword)
            .map_err(|_| EncodingFormatErrorKind::BadTextEncoding("invalid keyword"))?;
        let text = encode_latin1(&self.text).ok_or(EncodingFormatErrorKind::BadTextEncoding(
            "tEXt text is not representable in Latin-1",
        ))?;
        let mut payload = keyword;
        payload.push(0);
        payload.extend_from_slice(&text);
        chunk::write_chunk(w, chunk::tEXt, &payload)?;
        Ok(())
    }
}

impl EncodableTextChunk for ZTXtChunk {
    fn encode<W: Write>(&self, w: &mut W) -> Result<(), EncodingError> {
        let keyword = encode_latin1(&self.keyword).ok_or(
            EncodingFormatErrorKind::BadTextEncoding("keyword is not representable in Latin-1"),
        )?;
        validate_keyword_bytes(&keyword)
            .map_err(|_| EncodingFormatErrorKind::BadTextEncoding("invalid keyword"))?;
        let mut copy = self.clone();
        copy.compress_text()?;
        let OptCompressed::Compressed(body) = &copy.text else {
            return Err(EncodingFormatErrorKind::Unrecoverable.into());
        };
        let mut payload = keyword;
        payload.push(0);
        payload.push(0); // compression method: zlib
        payload.extend_from_slice(body);
        chunk::write_chunk(w, chunk::zTXt, &payload)?;
        Ok(())
    }
}

impl EncodableTextChunk for ITXtChunk {
    fn encode<W: Write>(&self, w: &mut W) -> Result<(), EncodingError> {
        let keyword = encode_latin1(&self.keyword).ok_or(
            EncodingFormatErrorKind::BadTextEncoding("keyword is not representable in Latin-1"),
        )?;
        validate_keyword_bytes(&keyword)
            .map_err(|_| EncodingFormatErrorKind::BadTextEncoding("invalid keyword"))?;
        let mut copy = self.clone();
        if copy.compressed {
            copy.compress_text()?;
        }
        let mut payload = keyword;
        payload.push(0);
        payload.push(u8::from(copy.compressed));
        payload.push(0); // compression method: zlib
        payload.extend_from_slice(copy.language_tag.as_bytes());
        payload.push(0);
        payload.extend_from_slice(copy.translated_keyword.as_bytes());
        payload.push(0);
        match &copy.text {
            OptCompressed::Compressed(body) => payload.extend_from_slice(body),
            OptCompressed::Uncompressed(text) => payload.extend_from_slice(text.as_bytes()),
        }
        chunk::write_chunk(w, chunk::iTXt, &payload)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_rules() {
        assert!(validate_keyword("Author").is_ok());
        assert!(validate_keyword("Copyright notice").is_ok());
        assert!(validate_keyword(&"k".repeat(79)).is_ok());
        assert!(validate_keyword(&"k".repeat(80)).is_err());
        assert!(validate_keyword("").is_err());
        assert!(validate_keyword(" lead").is_err());
        assert!(validate_keyword("trail ").is_err());
        assert!(validate_keyword("two  spaces").is_err());
        assert!(validate_keyword("new\nline").is_err());
        // 0xA0 (non-breaking space) is explicitly excluded by the spec.
        assert!(validate_keyword_bytes(&[b'a', 0xA0, b'b']).is_err());
        assert!(validate_keyword_bytes(&[b'a', 0xA1, b'b']).is_ok());
        // Characters above Latin-1 cannot be encoded at all.
        assert!(validate_keyword("\u{1F600}").is_err());
    }

    #[test]
    fn text_round_trips() {
        let chunk = TEXtChunk::new("Title", "A picture");
        let mut out = Vec::new();
        chunk.encode(&mut out).expect("encode");
        let payload = &out[8..out.len() - 4];
        assert_eq!(TEXtChunk::parse(payload).expect("parse"), chunk);
    }

    #[test]
    fn text_requires_a_null_separator() {
        assert!(TEXtChunk::parse(b"no null here").is_err());
    }

    #[test]
    fn ztxt_round_trips_through_compression() {
        let mut chunk = ZTXtChunk::new("Comment", "x".repeat(1000));
        let mut out = Vec::new();
        chunk.encode(&mut out).expect("encode");
        let payload = &out[8..out.len() - 4];
        let mut parsed = ZTXtChunk::parse(payload).expect("parse");
        assert_eq!(parsed.keyword, "Comment");
        parsed.decompress_text().expect("decompress");
        assert_eq!(parsed.get_text().expect("text"), "x".repeat(1000));
        chunk.compress_text().expect("compress");
    }

    #[test]
    fn ztxt_enforces_the_decompression_limit() {
        let bomb = ZTXtChunk::new("Comment", "a".repeat(100_000));
        let mut out = Vec::new();
        bomb.encode(&mut out).expect("encode");
        let payload = &out[8..out.len() - 4];
        let mut parsed = ZTXtChunk::parse(payload).expect("parse");
        assert!(parsed.decompress_text_with_limit(1024).is_err());
        assert!(parsed.decompress_text().is_ok());
    }

    #[test]
    fn ztxt_rejects_bad_method() {
        assert!(ZTXtChunk::parse(b"kw\0\x01data").is_err());
        assert!(ZTXtChunk::parse(b"kw\0").is_err());
        assert!(ZTXtChunk::parse(b"kw").is_err());
    }

    #[test]
    fn itxt_round_trips_compressed_and_plain() {
        for compressed in [false, true] {
            let mut chunk = ITXtChunk::new("Title", "Ünicode ✓ text");
            chunk.language_tag = "en".to_string();
            chunk.translated_keyword = "Titel".to_string();
            chunk.compressed = compressed;
            let mut out = Vec::new();
            chunk.encode(&mut out).expect("encode");
            let payload = &out[8..out.len() - 4];
            let parsed = ITXtChunk::parse(payload).expect("parse");
            assert_eq!(parsed.compressed, compressed);
            assert_eq!(parsed.language_tag, "en");
            assert_eq!(parsed.translated_keyword, "Titel");
            assert_eq!(parsed.get_text().expect("text"), "Ünicode ✓ text");
        }
    }

    #[test]
    fn itxt_rejects_malformed_payloads() {
        assert!(ITXtChunk::parse(b"kw").is_err());
        assert!(ITXtChunk::parse(b"kw\0").is_err());
        assert!(ITXtChunk::parse(b"kw\0\x02\0en\0t\0text").is_err());
        assert!(ITXtChunk::parse(b"kw\0\x01\x01en\0t\0text").is_err());
        assert!(ITXtChunk::parse(b"kw\0\0\0en\0t\0\xff\xfe").is_err());
    }

    #[test]
    fn latin1_helpers() {
        assert_eq!(decode_latin1(&[0xE9]), "é");
        assert_eq!(encode_latin1("é"), Some(vec![0xE9]));
        assert_eq!(encode_latin1("✓"), None);
    }
}

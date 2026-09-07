//! Adversarial verification of the `.xz` framing layer (track D-verify).
//!
//! Everything here targets a field the reader *parses* but could plausibly
//! ignore, or an input shape the round-trip and oracle suites cannot reach:
//!
//! * **Padding fields.** Block Padding (xz spec §3.4) and Index Padding
//!   (§4.4) must be null bytes. Block Padding is covered by no checksum at
//!   all, so accepting arbitrary bytes there let a crafted stream carry up
//!   to three bytes past every integrity check — and on `LZMA_CHECK_NONE`
//!   streams, which is what libtiff writes for TIFF `Compression = 34925`,
//!   the block has no check either.
//! * **The Stream Footer's own CRC-32** (§2.1.2.1), which was read and
//!   never verified.
//! * **A declared Compressed Size of zero** (§3.1.4 requires non-zero),
//!   which used to route the block to the self-describing path and so opt
//!   out of the exact-consumption cross-check.
//! * **Awkward `Read` implementations**: one byte per call, and one that
//!   injects `ErrorKind::Interrupted`.
//! * **Truncation at every single offset** of a small stream (the suite in
//!   `xz_module.rs` samples 400 offsets of a large one; the framing fields
//!   this file is about are only a few bytes wide, so they need every cut).
//!
//! Each corruption repairs the CRC-32 that covers it, so the stream would
//! be *accepted* without the check under test rather than rejected by
//! accident.

use oxiarc_core::crc::Crc32;
use oxiarc_core::error::OxiArcError;
use oxiarc_lzma::LzmaLevel;
use oxiarc_lzma::xz::{self, CheckType, XzWriter};
use std::io::{Cursor, Read};

const XZ_STREAM_HEADER_LEN: usize = 12;
const XZ_STREAM_FOOTER_LEN: usize = 12;

fn payload_bytes(len: usize) -> Vec<u8> {
    // Deterministic, moderately compressible: long runs plus noise, so the
    // compressed size is not a multiple of four for every length tried.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let byte = (seed >> 24) as u8;
        if byte % 5 == 0 {
            out.extend(std::iter::repeat_n(byte, 17.min(len - out.len())));
        } else {
            out.push(byte);
        }
    }
    out.truncate(len);
    out
}

fn stream_of(payload: &[u8], check: CheckType) -> Vec<u8> {
    XzWriter::new(LzmaLevel::new(6))
        .with_check_type(check)
        .compress(payload)
        .expect("compress")
}

/// Read a multibyte integer, returning the value and its encoded length.
fn read_varint(data: &[u8], mut offset: usize) -> (u64, usize) {
    let start = offset;
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = data[offset];
        offset += 1;
        value |= u64::from(byte & 0x7F) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
    }
    (value, offset - start)
}

/// Offsets of the interesting fields of the first (and, for these payloads,
/// only) block of a stream this crate wrote.
///
/// The writer always declares both optional size fields (block header flags
/// `0xC0`), so the layout can be read straight off the header.
struct BlockLayout {
    /// Offset of the block header's size byte.
    header_start: usize,
    /// Total block header length, including its trailing CRC-32.
    header_len: usize,
    /// Offset of the compressed-size varint inside the stream.
    compressed_size_field: usize,
    /// Length of that varint in bytes.
    compressed_size_field_len: usize,
    /// Declared size of the Compressed Data field.
    compressed_size: usize,
    /// Offset of the Block Padding field.
    padding_start: usize,
    /// Length of the Block Padding field (0..=3).
    padding_len: usize,
}

fn parse_first_block(stream: &[u8]) -> BlockLayout {
    let header_start = XZ_STREAM_HEADER_LEN;
    let size_byte = stream[header_start];
    assert_ne!(size_byte, 0x00, "expected a block, found the index");
    let header_len = (usize::from(size_byte) + 1) * 4;
    let flags = stream[header_start + 1];
    assert_eq!(
        flags, 0xC0,
        "this helper assumes the writer declares both size fields"
    );
    let compressed_size_field = header_start + 2;
    let (compressed_size, compressed_size_field_len) = read_varint(stream, compressed_size_field);
    let compressed_size = usize::try_from(compressed_size).expect("compressed size fits usize");
    let data_start = header_start + header_len;
    let padding_start = data_start + compressed_size;
    let padding_len = (4 - (compressed_size % 4)) % 4;
    BlockLayout {
        header_start,
        header_len,
        compressed_size_field,
        compressed_size_field_len,
        compressed_size,
        padding_start,
        padding_len,
    }
}

/// Recompute the block header's trailing CRC-32 after the header was edited.
fn repair_block_header_crc(stream: &mut [u8], layout: &BlockLayout) {
    let crc_pos = layout.header_start + layout.header_len - 4;
    let crc = Crc32::compute(&stream[layout.header_start..crc_pos]);
    stream[crc_pos..crc_pos + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Offset and length of the Index field (indicator through its CRC-32).
fn index_span(stream: &[u8]) -> (usize, usize) {
    let footer_start = stream.len() - XZ_STREAM_FOOTER_LEN;
    let backward_size = u32::from_le_bytes([
        stream[footer_start + 4],
        stream[footer_start + 5],
        stream[footer_start + 6],
        stream[footer_start + 7],
    ]);
    let index_len = (usize::try_from(backward_size).expect("backward size") + 1) * 4;
    (footer_start - index_len, index_len)
}

fn repair_index_crc(stream: &mut [u8]) {
    let (start, len) = index_span(stream);
    let crc = Crc32::compute(&stream[start..start + len - 4]);
    stream[start + len - 4..start + len].copy_from_slice(&crc.to_le_bytes());
}

fn decode_all(stream: &[u8]) -> Result<Vec<u8>, OxiArcError> {
    xz::decompress(&mut Cursor::new(stream))
}

// ---------------------------------------------------------------------------
// Padding fields
// ---------------------------------------------------------------------------

#[test]
fn block_padding_must_be_null() {
    // Find a payload whose compressed data is not a multiple of four bytes,
    // so a Block Padding field actually exists.
    let mut prepared = None;
    for len in 100..400 {
        let payload = payload_bytes(len);
        let stream = stream_of(&payload, CheckType::Crc32);
        let layout = parse_first_block(&stream);
        if layout.padding_len > 0 {
            prepared = Some((payload, stream, layout));
            break;
        }
    }
    let (payload, stream, layout) = prepared.expect("a payload with block padding");
    assert_eq!(decode_all(&stream).expect("clean decode"), payload);

    for offset in 0..layout.padding_len {
        for value in [0x01u8, 0x80, 0xFF] {
            let mut corrupted = stream.clone();
            corrupted[layout.padding_start + offset] = value;
            // No CRC covers Block Padding, so nothing else can catch this.
            let err = decode_all(&corrupted).expect_err("non-null block padding must be rejected");
            assert!(
                format!("{err}").contains("padding"),
                "unexpected error for padding byte {offset} = {value:#04X}: {err}"
            );
        }
    }
}

#[test]
fn block_padding_must_be_null_without_a_check_field() {
    // The `LZMA_CHECK_NONE` shape libtiff writes: the block carries no
    // check at all, so the padding bytes are the last unverified field in
    // the whole stream.
    let mut prepared = None;
    for len in 100..400 {
        let payload = payload_bytes(len);
        let stream = stream_of(&payload, CheckType::None);
        let layout = parse_first_block(&stream);
        if layout.padding_len > 0 {
            prepared = Some((payload, stream, layout));
            break;
        }
    }
    let (payload, stream, layout) = prepared.expect("a check-less payload with block padding");
    assert_eq!(decode_all(&stream).expect("clean decode"), payload);

    let mut corrupted = stream.clone();
    corrupted[layout.padding_start] = 0x7F;
    assert!(
        decode_all(&corrupted).is_err(),
        "non-null block padding must be rejected on a CHECK_NONE stream too"
    );
    let mut dst = vec![0u8; payload.len()];
    assert!(xz::decompress_into(&corrupted, &mut dst).is_err());
}

#[test]
fn index_padding_must_be_null() {
    // Pick a payload whose index has padding: the index is
    // indicator + count + (unpadded, uncompressed) + padding + CRC-32, and
    // its total length is a multiple of four.
    let mut prepared = None;
    for len in 100..600 {
        let payload = payload_bytes(len);
        let stream = stream_of(&payload, CheckType::Crc32);
        let (start, index_len) = index_span(&stream);
        // indicator(1) + count(1) + two varints, then padding, then CRC(4).
        let (_, unpadded_len) = read_varint(&stream, start + 2);
        let (_, uncompressed_len) = read_varint(&stream, start + 2 + unpadded_len);
        let used = 2 + unpadded_len + uncompressed_len;
        let padding_len = index_len - 4 - used;
        if padding_len > 0 {
            prepared = Some((payload, stream, start + used, padding_len));
            break;
        }
    }
    let (payload, stream, padding_start, padding_len) =
        prepared.expect("a payload with index padding");
    assert_eq!(decode_all(&stream).expect("clean decode"), payload);

    for offset in 0..padding_len {
        let mut corrupted = stream.clone();
        corrupted[padding_start + offset] = 0xA5;
        // Repair the index CRC-32 so the stream is only rejectable by the
        // padding rule itself.
        repair_index_crc(&mut corrupted);
        let err = decode_all(&corrupted).expect_err("non-null index padding must be rejected");
        assert!(
            format!("{err}").contains("padding"),
            "unexpected error for index padding byte {offset}: {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// Stream footer CRC-32
// ---------------------------------------------------------------------------

#[test]
fn stream_footer_crc_is_verified() {
    let payload = payload_bytes(3000);
    let stream = stream_of(&payload, CheckType::Crc64);
    assert_eq!(decode_all(&stream).expect("clean decode"), payload);

    let footer_start = stream.len() - XZ_STREAM_FOOTER_LEN;
    for byte in 0..4 {
        let mut corrupted = stream.clone();
        corrupted[footer_start + byte] ^= 0x40;
        // Only the CRC-32 field itself is touched: Backward Size and Stream
        // Flags still describe the stream correctly, so every other footer
        // check passes and this one is the only thing that can fire.
        let err = decode_all(&corrupted).expect_err("a corrupt footer CRC must be reported");
        assert!(
            matches!(err, OxiArcError::CrcMismatch { .. }),
            "footer CRC byte {byte} produced {err}"
        );
    }
}

// ---------------------------------------------------------------------------
// Declared Compressed Size
// ---------------------------------------------------------------------------

#[test]
fn a_declared_compressed_size_of_zero_is_rejected() {
    let payload = payload_bytes(40);
    let stream = stream_of(&payload, CheckType::Crc32);
    let layout = parse_first_block(&stream);
    assert_eq!(
        layout.compressed_size_field_len, 1,
        "this payload should compress to under 128 bytes"
    );
    assert!(layout.compressed_size > 0);

    let mut corrupted = stream.clone();
    corrupted[layout.compressed_size_field] = 0x00;
    repair_block_header_crc(&mut corrupted, &layout);
    // Without the non-zero rule this fell through to the self-describing
    // block path, which would have decoded it happily.
    let err = decode_all(&corrupted).expect_err("a zero compressed size must be rejected");
    assert!(
        format!("{err}").contains("compressed size of zero"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// Awkward readers
// ---------------------------------------------------------------------------

/// A `Read` that hands out at most one byte per call and injects
/// `ErrorKind::Interrupted` every few calls.
struct DribbleReader<'a> {
    data: &'a [u8],
    pos: usize,
    calls: usize,
    interrupt_every: usize,
}

impl Read for DribbleReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.interrupt_every > 0 && self.calls % self.interrupt_every == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "synthetic interrupt",
            ));
        }
        if self.pos >= self.data.len() || buf.is_empty() {
            return Ok(0);
        }
        buf[0] = self.data[self.pos];
        self.pos += 1;
        Ok(1)
    }
}

#[test]
fn a_one_byte_at_a_time_reader_decodes_identically() {
    let payload = payload_bytes(20_000);
    for check in [
        CheckType::None,
        CheckType::Crc32,
        CheckType::Crc64,
        CheckType::Sha256,
    ] {
        let stream = stream_of(&payload, check);
        for interrupt_every in [0usize, 3, 7] {
            let mut reader = DribbleReader {
                data: &stream,
                pos: 0,
                calls: 0,
                interrupt_every,
            };
            let decoded = xz::decompress(&mut reader).expect("dribbled decode");
            assert_eq!(
                decoded, payload,
                "check {check:?}, interrupt {interrupt_every}"
            );
        }
    }
}

#[test]
fn a_one_byte_at_a_time_reader_sees_multi_stream_files() {
    // `read_up_to` (the Stream Padding scanner) is the one place that reads
    // without `read_exact`, so it has to loop over short reads itself.
    let a = stream_of(b"first", CheckType::Crc32);
    let b = stream_of(b"second", CheckType::None);
    let mut file = a.clone();
    file.extend(std::iter::repeat_n(0u8, 8));
    file.extend_from_slice(&b);
    file.extend(std::iter::repeat_n(0u8, 4));

    let mut reader = DribbleReader {
        data: &file,
        pos: 0,
        calls: 0,
        interrupt_every: 5,
    };
    assert_eq!(
        xz::decompress(&mut reader).expect("dribbled multi-stream"),
        b"firstsecond"
    );
}

// ---------------------------------------------------------------------------
// Exhaustive truncation
// ---------------------------------------------------------------------------

#[test]
fn truncation_at_every_offset_is_an_error_and_never_hangs() {
    let payload = payload_bytes(700);
    for check in [CheckType::None, CheckType::Crc32, CheckType::Sha256] {
        let stream = stream_of(&payload, check);
        for cut in 0..stream.len() {
            let truncated = &stream[..cut];
            if let Ok(decoded) = decode_all(truncated) {
                panic!(
                    "check {check:?}: truncating to {cut}/{} succeeded with {} bytes",
                    stream.len(),
                    decoded.len()
                );
            }
            let mut dst = vec![0u8; payload.len()];
            assert!(
                xz::decompress_into(truncated, &mut dst).is_err(),
                "check {check:?}: decompress_into accepted a {cut}-byte prefix"
            );
            assert!(
                xz::decompress_with_limit(truncated, payload.len()).is_err(),
                "check {check:?}: decompress_with_limit accepted a {cut}-byte prefix"
            );
        }
        // The untruncated stream still decodes, so the sweep above is not
        // vacuously passing on a broken stream.
        assert_eq!(decode_all(&stream).expect("clean decode"), payload);
    }
}

#[test]
fn a_one_byte_output_buffer_reports_buffer_too_small() {
    let payload = payload_bytes(5000);
    let stream = stream_of(&payload, CheckType::Crc32);
    let mut dst = [0u8; 1];
    match xz::decompress_into(&stream, &mut dst) {
        Err(OxiArcError::BufferTooSmall { needed, available }) => {
            assert_eq!(available, 1);
            assert!(needed > 1, "needed must be a lower bound above the cap");
        }
        other => panic!("expected BufferTooSmall, got {other:?}"),
    }
}

#[test]
fn a_huge_declared_compressed_size_is_rejected_before_allocating() {
    // A 5-byte varint reaching ~2^32 in the Compressed Size field: the
    // reader must refuse it on the declared value alone rather than trying
    // to allocate the buffer first.
    let payload = payload_bytes(40);
    let stream = stream_of(&payload, CheckType::Crc32);
    let layout = parse_first_block(&stream);
    assert_eq!(layout.compressed_size_field_len, 1);

    // Rebuild the block header with a 5-byte compressed-size varint. The
    // header grows by four bytes, which keeps its 4-byte alignment, so only
    // the size byte and the CRC-32 have to be adjusted.
    let mut header = stream[layout.header_start..layout.header_start + layout.header_len].to_vec();
    let crc_pos = header.len() - 4;
    let mut rebuilt = Vec::new();
    rebuilt.push(header[1]); // flags
    rebuilt.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x0F]); // ~2^32 - 2^28
    rebuilt.extend_from_slice(&header[2 + layout.compressed_size_field_len..crc_pos]);
    // Keep the total header length a multiple of four by dropping the four
    // padding bytes the growth consumed, or adding some if there were none.
    while (1 + rebuilt.len() + 4) % 4 != 0 {
        rebuilt.push(0x00);
    }
    let size_byte = u8::try_from((1 + rebuilt.len() + 4) / 4 - 1).expect("header size byte");
    header.clear();
    header.push(size_byte);
    header.extend_from_slice(&rebuilt);
    let crc = Crc32::compute(&header);
    header.extend_from_slice(&crc.to_le_bytes());

    let mut crafted = stream[..layout.header_start].to_vec();
    crafted.extend_from_slice(&header);
    crafted.extend_from_slice(&stream[layout.header_start + layout.header_len..]);

    let err = decode_all(&crafted).expect_err("an absurd compressed size must be rejected");
    assert!(
        format!("{err}").contains("exceeding") || format!("{err}").contains("compressed size"),
        "unexpected error: {err}"
    );
}

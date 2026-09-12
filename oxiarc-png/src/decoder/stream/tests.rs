//! Unit tests for the push state machine: framing, ordering and policy.

use super::*;
use crate::chunk::{ChunkType, write_chunk};

/// Assemble a PNG out of ready-made chunks, with the signature prepended.
fn assemble(chunks: &[(ChunkType, Vec<u8>)]) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    for (kind, data) in chunks {
        write_chunk(&mut out, *kind, data).expect("write");
    }
    out
}

fn ihdr(width: u32, height: u32, depth: u8, color: u8) -> Vec<u8> {
    let mut d = vec![0u8; 13];
    d[0..4].copy_from_slice(&width.to_be_bytes());
    d[4..8].copy_from_slice(&height.to_be_bytes());
    d[8] = depth;
    d[9] = color;
    d
}

fn gray_idat(rows: &[&[u8]]) -> Vec<u8> {
    let mut raw = Vec::new();
    for row in rows {
        raw.push(0u8);
        raw.extend_from_slice(row);
    }
    oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib")
}

/// A minimal valid 2x1 grayscale file.
fn minimal() -> Vec<Vec<u8>> {
    vec![ihdr(2, 1, 8, 0), gray_idat(&[&[1, 2]]), Vec::new()]
}

fn minimal_png() -> Vec<u8> {
    let parts = minimal();
    assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, parts[2].clone()),
    ])
}

/// Drive the decoder over a whole buffer, collecting every event.
fn events(decoder: &mut StreamingDecoder, data: &[u8]) -> Result<Vec<Decoded>, DecodingError> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut guard = 0;
    loop {
        guard += 1;
        assert!(guard < 100_000, "no progress");
        let (consumed, event) = decoder.update(&data[pos..], None)?;
        pos += consumed;
        let finished = matches!(event, Decoded::ImageEnd);
        if event != Decoded::Nothing {
            out.push(event);
        }
        if finished {
            return Ok(out);
        }
        if consumed == 0 && pos >= data.len() && decoder.is_finished() {
            return Ok(out);
        }
        if consumed == 0 && pos >= data.len() {
            decoder.notify_eof();
        }
    }
}

#[test]
fn a_minimal_file_produces_the_expected_event_sequence() {
    let mut decoder = StreamingDecoder::new();
    let seen = events(&mut decoder, &minimal_png()).expect("decode");
    assert!(matches!(seen[0], Decoded::ChunkBegin(13, chunk::IHDR)));
    assert!(matches!(
        seen[1],
        Decoded::Header {
            width: 2,
            height: 1,
            ..
        }
    ));
    assert!(seen.iter().any(|e| matches!(e, Decoded::ImageEnd)));
    assert!(decoder.is_finished());
    assert_eq!(decoder.info().expect("info").size(), (2, 1));
}

#[test]
fn byte_at_a_time_matches_whole_buffer() {
    let png = minimal_png();
    let mut whole = StreamingDecoder::new();
    let a = events(&mut whole, &png).expect("whole");

    let mut piecewise = StreamingDecoder::new();
    let mut b = Vec::new();
    let mut pos = 0;
    let mut guard = 0;
    while pos < png.len() {
        guard += 1;
        assert!(guard < 100_000);
        let end = (pos + 1).min(png.len());
        let (consumed, event) = piecewise.update(&png[pos..end], None).expect("step");
        pos += consumed;
        if event != Decoded::Nothing {
            b.push(event);
        }
    }
    assert_eq!(a, b);
}

#[test]
fn a_bad_signature_is_rejected() {
    let mut decoder = StreamingDecoder::new();
    let err = decoder.update(b"not a png at all", None).unwrap_err();
    assert!(matches!(
        err.format_kind(),
        Some(FormatErrorKind::InvalidSignature)
    ));
    // The error is latched.
    assert!(decoder.update(b"more", None).is_err());
}

#[test]
fn an_unknown_critical_chunk_is_fatal_and_an_unknown_ancillary_one_is_kept() {
    let parts = minimal();
    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (ChunkType(*b"prVt"), vec![1, 2, 3]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    let seen = events(&mut decoder, &png).expect("decode");
    assert!(seen.iter().any(|e| matches!(
        e,
        Decoded::RetainedUnknownChunk(k) if *k == ChunkType(*b"prVt")
    )));
    let info = decoder.info().expect("info");
    assert_eq!(info.unknown_chunks.len(), 1);
    assert_eq!(info.unknown_chunks[0].data, vec![1, 2, 3]);
    assert!(info.unknown_chunks[0].safe_to_copy);

    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (ChunkType(*b"PrVt"), vec![1]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    let err = events(&mut decoder, &png).unwrap_err();
    assert!(matches!(
        err.format_kind(),
        Some(FormatErrorKind::UnrecognizedCriticalChunk { .. })
    ));
}

#[test]
fn retaining_unknown_chunks_can_be_turned_off() {
    let parts = minimal();
    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (ChunkType(*b"prVt"), vec![1, 2, 3]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut options = DecodeOptions::default();
    options.set_retain_unknown_chunks(false);
    let mut decoder = StreamingDecoder::new_with_options(options);
    let seen = events(&mut decoder, &png).expect("decode");
    assert!(seen.iter().any(|e| matches!(
        e,
        Decoded::SkippedAncillaryChunk(k) if *k == ChunkType(*b"prVt")
    )));
    assert!(decoder.info().expect("info").unknown_chunks.is_empty());
}

#[test]
fn crc_policy() {
    let parts = minimal();
    // Corrupt an ancillary chunk's CRC.
    let mut png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (chunk::gAMA, 45455u32.to_be_bytes().to_vec()),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    // Flip a byte of the gAMA chunk's stored CRC, wherever it landed.
    let gama_start = png
        .windows(4)
        .position(|w| w == b"gAMA")
        .expect("gAMA is present");
    let gama_crc = gama_start + 4 + 4;
    png[gama_crc] ^= 0xFF;
    let mut decoder = StreamingDecoder::new();
    let seen = events(&mut decoder, &png).expect("lenient decode");
    assert!(seen.iter().any(|e| matches!(
        e,
        Decoded::BadAncillaryChunk(k) if *k == chunk::gAMA
    )));
    assert!(decoder.info().expect("info").gama_chunk.is_none());

    // The same corruption is fatal when ancillary failures are not skipped.
    let mut options = DecodeOptions::default();
    options.set_skip_ancillary_crc_failures(false);
    let mut decoder = StreamingDecoder::new_with_options(options);
    assert!(events(&mut decoder, &png).is_err());

    // ... and tolerated entirely when CRCs are ignored.
    let mut options = DecodeOptions::default();
    options.set_ignore_crc(true);
    let mut decoder = StreamingDecoder::new_with_options(options);
    events(&mut decoder, &png).expect("ignored");
    assert!(decoder.info().expect("info").gama_chunk.is_some());
}

#[test]
fn a_critical_crc_failure_is_always_fatal() {
    let parts = minimal();
    let mut png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    png[8 + 12 + 4] ^= 0xFF; // inside IHDR's payload
    let mut decoder = StreamingDecoder::new();
    let err = events(&mut decoder, &png).unwrap_err();
    assert!(matches!(
        err.format_kind(),
        Some(FormatErrorKind::CrcMismatch { .. })
    ));
}

#[test]
fn chunk_ordering_rules() {
    let parts = minimal();
    type Case = (&'static str, Vec<(ChunkType, Vec<u8>)>);
    let cases: Vec<Case> = vec![
        (
            "chunk before IHDR",
            vec![
                (chunk::gAMA, 1u32.to_be_bytes().to_vec()),
                (chunk::IHDR, parts[0].clone()),
                (chunk::IDAT, parts[1].clone()),
                (chunk::IEND, Vec::new()),
            ],
        ),
        (
            "duplicate IHDR",
            vec![
                (chunk::IHDR, parts[0].clone()),
                (chunk::IHDR, parts[0].clone()),
                (chunk::IDAT, parts[1].clone()),
                (chunk::IEND, Vec::new()),
            ],
        ),
        (
            "gAMA after IDAT",
            vec![
                (chunk::IHDR, parts[0].clone()),
                (chunk::IDAT, parts[1].clone()),
                (chunk::gAMA, 1u32.to_be_bytes().to_vec()),
                (chunk::IEND, Vec::new()),
            ],
        ),
        (
            "restarted IDAT run",
            vec![
                (chunk::IHDR, parts[0].clone()),
                (chunk::IDAT, parts[1].clone()),
                (chunk::tEXt, b"Note\0text".to_vec()),
                (chunk::IDAT, parts[1].clone()),
                (chunk::IEND, Vec::new()),
            ],
        ),
        (
            "IEND without IDAT",
            vec![(chunk::IHDR, parts[0].clone()), (chunk::IEND, Vec::new())],
        ),
    ];
    for (name, chunks) in cases {
        let png = assemble(&chunks);
        let mut decoder = StreamingDecoder::new();
        assert!(events(&mut decoder, &png).is_err(), "{name} should fail");
    }
}

#[test]
fn a_reserved_type_bit_is_only_fatal_in_strict_mode() {
    let parts = minimal();
    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (ChunkType(*b"prVt"), vec![1]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    // "prVt" has bit 5 of the third byte clear; build one that does not.
    let reserved = ChunkType(*b"prvt");
    assert!(crate::chunk::reserved_set(reserved));
    let png_reserved = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (reserved, vec![1]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    assert!(events(&mut decoder, &png).is_ok());
    let mut decoder = StreamingDecoder::new();
    assert!(events(&mut decoder, &png_reserved).is_ok());

    let mut options = DecodeOptions::default();
    options.set_strict(true);
    let mut decoder = StreamingDecoder::new_with_options(options);
    let err = events(&mut decoder, &png_reserved).unwrap_err();
    assert!(matches!(
        err.format_kind(),
        Some(FormatErrorKind::ReservedBitSet { .. })
    ));
}

#[test]
fn a_wrong_length_is_fatal_for_critical_chunks_and_rejects_ancillary_ones() {
    let parts = minimal();
    let png = assemble(&[
        (chunk::IHDR, vec![0u8; 12]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    assert!(events(&mut decoder, &png).is_err());

    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (chunk::gAMA, vec![0u8; 3]),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    events(&mut decoder, &png).expect("ancillary length errors are recoverable");
    assert!(decoder.info().expect("info").gama_chunk.is_none());
}

#[test]
fn text_chunks_are_parsed_and_can_be_ignored() {
    let parts = minimal();
    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (chunk::tEXt, b"Title\0A picture".to_vec()),
        (chunk::IDAT, parts[1].clone()),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    events(&mut decoder, &png).expect("decode");
    let info = decoder.info().expect("info");
    assert_eq!(info.uncompressed_latin1_text.len(), 1);
    assert_eq!(info.uncompressed_latin1_text[0].text, "A picture");

    let mut options = DecodeOptions::default();
    options.set_ignore_text_chunk(true);
    let mut decoder = StreamingDecoder::new_with_options(options);
    events(&mut decoder, &png).expect("decode");
    assert!(
        decoder
            .info()
            .expect("info")
            .uncompressed_latin1_text
            .is_empty()
    );
}

#[test]
fn cgbi_is_accepted_leniently_and_rejected_strictly() {
    let raw = [0u8, 1, 2];
    let deflated = oxiarc_deflate::deflate(&raw, 6).expect("deflate");
    let png = assemble(&[
        (chunk::CgBI, vec![0x50, 0x00, 0x20, 0x06]),
        (chunk::IHDR, ihdr(2, 1, 8, 0)),
        (chunk::IDAT, deflated),
        (chunk::IEND, Vec::new()),
    ]);
    let mut decoder = StreamingDecoder::new();
    events(&mut decoder, &png).expect("lenient");
    assert!(decoder.info().expect("info").cgbi.is_some());

    let mut options = DecodeOptions::default();
    options.set_strict(true);
    let mut decoder = StreamingDecoder::new_with_options(options);
    let err = events(&mut decoder, &png).unwrap_err();
    assert!(matches!(
        err.format_kind(),
        Some(FormatErrorKind::CgbiUnsupported)
    ));
}

#[test]
fn oversized_chunks_are_refused_before_being_read() {
    let mut png = SIGNATURE.to_vec();
    // A header declaring 2 GiB, without the payload behind it.
    png.extend_from_slice(&0x7FFF_FFFFu32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    let limits = DecodeLimits::default().with_max_chunk_len(1024);
    let mut options = DecodeOptions::default();
    options.set_limits(limits);
    let mut decoder = StreamingDecoder::new_with_options(options);
    let mut pos = 0;
    let mut result = Ok(());
    for _ in 0..100 {
        match decoder.update(&png[pos..], None) {
            Ok((consumed, _)) => pos += consumed,
            Err(err) => {
                result = Err(err);
                break;
            }
        }
    }
    assert!(matches!(result, Err(DecodingError::LimitsExceeded)));
}

#[test]
fn a_chunk_length_above_the_spec_cap_is_rejected() {
    let mut png = SIGNATURE.to_vec();
    png.extend_from_slice(&0x8000_0000u32.to_be_bytes());
    png.extend_from_slice(b"IHDR");
    let mut decoder = StreamingDecoder::new();
    let mut err = None;
    let mut pos = 0;
    for _ in 0..20 {
        match decoder.update(&png[pos..], None) {
            Ok((consumed, _)) => pos += consumed,
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(matches!(
        err.as_ref().and_then(|e| e.format_kind()),
        Some(FormatErrorKind::ChunkTooLong { .. })
    ));
}

#[test]
fn reset_returns_the_decoder_to_its_starting_state() {
    let png = minimal_png();
    let mut decoder = StreamingDecoder::new();
    events(&mut decoder, &png).expect("first");
    assert!(decoder.is_finished());
    decoder.reset();
    assert!(!decoder.is_finished());
    assert!(decoder.info().is_none());
    events(&mut decoder, &png).expect("second");
    assert!(decoder.is_finished());
}

#[test]
fn trailing_data_after_iend_is_ignored_unless_strict() {
    let mut png = minimal_png();
    png.extend_from_slice(b"trailing garbage");
    let mut decoder = StreamingDecoder::new();
    events(&mut decoder, &png).expect("lenient");

    let mut options = DecodeOptions::default();
    options.set_error_on_trailing_data(true);
    let mut decoder = StreamingDecoder::new_with_options(options);
    let mut pos = 0;
    let mut err = None;
    for _ in 0..1000 {
        match decoder.update(&png[pos..], None) {
            Ok((consumed, _)) => {
                pos += consumed;
                if consumed == 0 && pos >= png.len() {
                    break;
                }
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    assert!(matches!(
        err.as_ref().and_then(|e| e.format_kind()),
        Some(FormatErrorKind::TrailingData)
    ));
}

#[test]
fn a_missing_iend_is_reported() {
    let parts = minimal();
    let png = assemble(&[
        (chunk::IHDR, parts[0].clone()),
        (chunk::IDAT, parts[1].clone()),
    ]);
    let mut decoder = StreamingDecoder::new();
    let err = events(&mut decoder, &png).unwrap_err();
    assert!(
        matches!(err.format_kind(), Some(FormatErrorKind::MissingIend)),
        "{err}"
    );
}

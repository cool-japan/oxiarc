//! Property-based tests for the `.Z` (UNIX `compress`) container.
//!
//! Every call that touches arbitrary (i.e. very likely corrupt) bytes goes
//! through a *bounded* entry point: a few kilobytes of 16-bit `.Z` codes can
//! legitimately expand to hundreds of megabytes, so an unbounded
//! `decompress` inside a proptest would be a memory bomb of our own making.

use std::io::{Read, Write};

use oxiarc_lzw::z::{
    MAX_MAX_BITS, MIN_MAX_BITS, ZHeader, ZReader, ZWriter, compress_with_block_mode, decompress,
    decompress_into, decompress_with_limit,
};
use proptest::prelude::*;

/// Output cap for arbitrary-input cases (8 MiB).
const LIMIT: usize = 8 * 1024 * 1024;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// Every width and both block modes round-trip arbitrary bytes.
    #[test]
    fn roundtrip(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        max_bits in MIN_MAX_BITS..=MAX_MAX_BITS,
        block_mode in any::<bool>(),
    ) {
        let stream = compress_with_block_mode(&data, max_bits, block_mode)
            .expect("compress must not fail for a valid width");
        let header = ZHeader::parse(&stream).expect("our own header");
        prop_assert_eq!(header.max_bits, max_bits);
        prop_assert_eq!(header.block_mode, block_mode);
        prop_assert_eq!(decompress(&stream).expect("decompress our own stream"), data);
    }

    /// The three one-shot decode entry points agree on our own streams.
    #[test]
    fn entry_points_agree(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        max_bits in MIN_MAX_BITS..=MAX_MAX_BITS,
        block_mode in any::<bool>(),
    ) {
        let stream = compress_with_block_mode(&data, max_bits, block_mode)
            .expect("compress");
        let mut into = vec![0xA5u8; data.len() + 16];
        let written = decompress_into(&stream, &mut into[..data.len()])
            .expect("decompress_into");
        prop_assert_eq!(written, data.len());
        prop_assert_eq!(&into[..data.len()], &data[..]);
        prop_assert!(into[data.len()..].iter().all(|&b| b == 0xA5), "wrote past the buffer");
        prop_assert_eq!(
            decompress_with_limit(&stream, data.len()).expect("decompress_with_limit"),
            data
        );
    }

    /// `ZWriter` produces exactly what the one-shot encoder does, whatever
    /// the write boundaries are.
    #[test]
    fn the_writer_is_boundary_independent(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        chunk in 1usize..512,
        max_bits in MIN_MAX_BITS..=MAX_MAX_BITS,
        block_mode in any::<bool>(),
    ) {
        let expected = compress_with_block_mode(&data, max_bits, block_mode).expect("compress");
        let header = ZHeader::new(max_bits, block_mode).expect("header");
        let mut writer = ZWriter::with_header(Vec::new(), header).expect("writer");
        for piece in data.chunks(chunk) {
            writer.write_all(piece).expect("write");
        }
        let produced = writer.finish().expect("finish");
        prop_assert_eq!(produced, expected);
    }

    /// `ZReader` reproduces the one-shot decode whatever the read sizes are.
    #[test]
    fn the_reader_is_boundary_independent(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        buf_len in 1usize..64,
        max_bits in MIN_MAX_BITS..=MAX_MAX_BITS,
        block_mode in any::<bool>(),
    ) {
        let stream = compress_with_block_mode(&data, max_bits, block_mode).expect("compress");
        let mut reader = ZReader::new(&stream[..]).with_max_output(LIMIT as u64);
        let mut out = Vec::new();
        let mut scratch = vec![0u8; buf_len];
        loop {
            let read = reader.read(&mut scratch).expect("read");
            if read == 0 {
                break;
            }
            out.extend_from_slice(&scratch[..read]);
        }
        prop_assert_eq!(out, data);
    }

    /// Truncating one of our own streams yields a prefix of the payload and
    /// never an error — `.Z` has no end-of-information code.
    #[test]
    fn truncation_yields_a_prefix(
        data in prop::collection::vec(any::<u8>(), 64..4096),
        cut in 0usize..4096,
        max_bits in MIN_MAX_BITS..=MAX_MAX_BITS,
        block_mode in any::<bool>(),
    ) {
        let stream = compress_with_block_mode(&data, max_bits, block_mode).expect("compress");
        let cut = ZHeader::LEN + cut.min(stream.len() - ZHeader::LEN);
        let decoded = decompress_with_limit(&stream[..cut], LIMIT)
            .expect("a truncated .Z is a prefix, not an error");
        prop_assert!(decoded.len() <= data.len());
        prop_assert_eq!(&decoded[..], &data[..decoded.len()]);
    }

    /// Arbitrary bytes behind a valid header must never panic, and must
    /// respect the output bound.
    #[test]
    fn arbitrary_bodies_never_panic(
        body in prop::collection::vec(any::<u8>(), 0..2048),
        max_bits in MIN_MAX_BITS..=MAX_MAX_BITS,
        block_mode in any::<bool>(),
        dst_len in 0usize..4096,
    ) {
        let header = ZHeader::new(max_bits, block_mode).expect("header");
        let mut stream = header.to_bytes().to_vec();
        stream.extend_from_slice(&body);

        let outcome = std::panic::catch_unwind(|| {
            let bounded = decompress_with_limit(&stream, LIMIT).map(|out| out.len() <= LIMIT);
            let mut dst = vec![0xA5u8; dst_len + 8];
            let into = decompress_into(&stream, &mut dst[..dst_len]).map(|n| n <= dst_len);
            let overrun = dst[dst_len..].iter().any(|&b| b != 0xA5);
            (bounded, into, overrun)
        });
        prop_assert!(outcome.is_ok(), "decoding arbitrary bytes must not panic");
        if let Ok((bounded, into, overrun)) = outcome {
            prop_assert!(!overrun, "decompress_into wrote past its buffer");
            if let Ok(within) = bounded {
                prop_assert!(within, "the output bound was not respected");
            }
            if let Ok(within) = into {
                prop_assert!(within, "decompress_into reported more bytes than fit");
            }
        }
    }

    /// Wholly arbitrary bytes (header included) must never panic either.
    #[test]
    fn arbitrary_streams_never_panic(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let outcome = std::panic::catch_unwind(|| {
            let _ = decompress_with_limit(&data, LIMIT);
            let mut dst = [0u8; 1024];
            let _ = decompress_into(&data, &mut dst);
            let mut out = Vec::new();
            let _ = ZReader::new(&data[..])
                .with_max_output(LIMIT as u64)
                .read_to_end(&mut out);
        });
        prop_assert!(outcome.is_ok(), "arbitrary bytes must not panic any entry point");
    }
}

//! Reused codec state must not change what a strip decodes to.
//!
//! `CodecState` pools a `WrappedInflate`, a `ZstdStream`, an `XzDecoder` and
//! the fax changing-element buffers across the chunks of one image, so a page
//! of a thousand strips allocates one window instead of a thousand. That is
//! only sound if a *reused* decoder produces exactly what a *fresh* one
//! would, on good strips and on bad ones alike — including the two shapes a
//! padded TIFF strip actually takes: trailing bytes after the frame, and
//! trailing bytes that themselves look like the start of another frame.
//!
//! The pools are also handed to several threads at once by the `rayon`
//! feature, so the last module here decodes one page's strips from eight
//! threads through one shared state and requires every result to be the one a
//! serial decode produces.

/// The same shapes for every codec: sizes that cross the block boundary, and
/// content that is compressible, incompressible and constant in turn.
#[cfg(any(feature = "zstd", feature = "lzma", feature = "deflate"))]
fn payloads() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (index, len) in [0usize, 1, 17, 1024, 65_536, 200_000].iter().enumerate() {
        out.push(match index % 3 {
            0 => (0..*len).map(|i| (i / 37 % 251) as u8).collect(),
            1 => (0..*len)
                .map(|i| ((i.wrapping_mul(2_654_435_761)) >> 13) as u8)
                .collect(),
            _ => vec![0xA5; *len],
        });
    }
    out
}

#[cfg(feature = "zstd")]
mod zstd {
    use super::payloads;
    use oxiarc_tiff::compression::{CodecContext, CodecLevel, CodecState, decode_into, encode};
    use oxiarc_tiff::{CompressionMethod, Endian};

    fn context<'a>(state: Option<&'a CodecState>, len: usize, bits: &'a [u16]) -> CodecContext<'a> {
        let mut cx = CodecContext::new(
            CompressionMethod::Zstd,
            len.max(1),
            1,
            bits,
            1,
            Endian::Little,
        );
        cx.state = state;
        cx
    }

    #[test]
    fn a_reused_decoder_matches_a_fresh_one_on_every_payload() {
        let state = CodecState::new();
        let bits = [8u16];
        let mut decoded = 0usize;
        for payload in payloads() {
            for level in [CodecLevel::Level(1), CodecLevel::Level(9)] {
                let fresh_cx = context(None, payload.len(), &bits);
                let frame = encode(&payload, &fresh_cx, level).expect("encode");
                let mut fresh = vec![0u8; payload.len()];
                let a = decode_into(&frame, &mut fresh, &fresh_cx).expect("fresh decode");

                let reused_cx = context(Some(&state), payload.len(), &bits);
                let mut reused = vec![0u8; payload.len()];
                let b = decode_into(&frame, &mut reused, &reused_cx).expect("reused decode");

                assert_eq!(a, b, "written count for {} bytes", payload.len());
                assert_eq!(fresh, reused, "bytes for {} bytes", payload.len());
                assert_eq!(fresh, payload);
                decoded += 1;
            }
        }
        assert_eq!(decoded, 12, "every payload x level pair must have run");
    }

    #[test]
    fn trailing_padding_is_never_read_as_a_second_frame() {
        // A TIFF strip's byte count is routinely rounded up, so the codec is
        // handed bytes past the frame. The decoder must stop at the frame's
        // end — with `with_multi_frame(false)` it does, and this pins it.
        let payload: Vec<u8> = (0..4096u32).map(|i| (i / 7 % 251) as u8).collect();
        let bits = [8u16];
        let cx = context(None, payload.len(), &bits);
        let frame = encode(&payload, &cx, CodecLevel::Level(3)).expect("encode");
        let second = encode(&[0u8; 32], &cx, CodecLevel::Level(3)).expect("encode");

        let mut padded = frame.clone();
        padded.extend_from_slice(&[0u8; 7]);
        let mut two_frames = frame.clone();
        two_frames.extend_from_slice(&second);

        let state = CodecState::new();
        for (label, strip) in [("padded", &padded), ("two frames", &two_frames)] {
            let mut fresh = vec![0u8; payload.len()];
            let a = decode_into(strip, &mut fresh, &cx).expect(label);
            let reused_cx = context(Some(&state), payload.len(), &bits);
            let mut reused = vec![0u8; payload.len()];
            let b = decode_into(strip, &mut reused, &reused_cx).expect(label);
            assert_eq!(a, payload.len(), "{label}: only the first frame");
            assert_eq!(a, b, "{label}");
            assert_eq!(fresh, payload, "{label}");
            assert_eq!(reused, payload, "{label}");
        }
    }

    #[test]
    fn a_corrupt_strip_does_not_poison_the_next_one() {
        // `ZstdStream` latches its fault until `reset()`. If the slot forgot
        // to reset, every strip after a damaged one would fail too.
        let payload: Vec<u8> = (0..8192u32).map(|i| (i / 13 % 251) as u8).collect();
        let bits = [8u16];
        let state = CodecState::new();
        let cx = context(Some(&state), payload.len(), &bits);
        let frame = encode(&payload, &cx, CodecLevel::Level(6)).expect("encode");

        let mut broken = frame.clone();
        for byte in broken.iter_mut().skip(8).take(24) {
            *byte ^= 0x5a;
        }
        let mut out = vec![0u8; payload.len()];
        assert!(
            decode_into(&broken, &mut out, &cx).is_err(),
            "corrupt strip"
        );

        let mut out = vec![0u8; payload.len()];
        assert_eq!(
            decode_into(&frame, &mut out, &cx).expect("the strip after a bad one"),
            payload.len()
        );
        assert_eq!(out, payload);

        // Truncation is the other fault shape: same requirement.
        let mut out = vec![0u8; payload.len()];
        assert!(decode_into(&frame[..frame.len() / 2], &mut out, &cx).is_err());
        let mut out = vec![0u8; payload.len()];
        assert_eq!(
            decode_into(&frame, &mut out, &cx).expect("after a truncated one"),
            payload.len()
        );
        assert_eq!(out, payload);
    }

    #[test]
    fn the_slot_really_holds_a_decoder_after_a_decode() {
        // Non-vacuity for every test above: without this, they would all pass
        // just as well if `decode_into` ignored the state entirely.
        let state = CodecState::new();
        assert!(format!("{state:?}").contains("ZstdSlot(0)"), "{state:?}");
        let bits = [8u16];
        let payload = vec![7u8; 512];
        let cx = context(Some(&state), payload.len(), &bits);
        let frame = encode(&payload, &cx, CodecLevel::Default).expect("encode");
        let mut out = vec![0u8; payload.len()];
        decode_into(&frame, &mut out, &cx).expect("decode");
        assert!(format!("{state:?}").contains("ZstdSlot(1)"), "{state:?}");
    }
}

#[cfg(feature = "lzma")]
mod lzma {
    use super::payloads;
    use oxiarc_tiff::compression::{CodecContext, CodecLevel, CodecState, decode_into, encode};
    use oxiarc_tiff::{CompressionMethod, Endian};

    fn context<'a>(state: Option<&'a CodecState>, len: usize, bits: &'a [u16]) -> CodecContext<'a> {
        let mut cx = CodecContext::new(
            CompressionMethod::Lzma,
            len.max(1),
            1,
            bits,
            1,
            Endian::Little,
        );
        cx.state = state;
        cx
    }

    #[test]
    fn a_reused_decoder_matches_a_fresh_one_on_every_payload() {
        let state = CodecState::new();
        let bits = [8u16];
        let mut decoded = 0usize;
        // Alternating presets change the declared dictionary size mid-stream,
        // which is exactly the case `XzDecoder`'s cache is keyed on.
        for payload in payloads() {
            for level in [CodecLevel::Level(1), CodecLevel::Level(6)] {
                let fresh_cx = context(None, payload.len(), &bits);
                let stream = encode(&payload, &fresh_cx, level).expect("encode");
                let mut fresh = vec![0u8; payload.len()];
                let a = decode_into(&stream, &mut fresh, &fresh_cx).expect("fresh decode");

                let reused_cx = context(Some(&state), payload.len(), &bits);
                let mut reused = vec![0u8; payload.len()];
                let b = decode_into(&stream, &mut reused, &reused_cx).expect("reused decode");

                assert_eq!(a, b, "written count for {} bytes", payload.len());
                assert_eq!(fresh, reused, "bytes for {} bytes", payload.len());
                assert_eq!(fresh, payload);
                decoded += 1;
            }
        }
        assert_eq!(decoded, 12, "every payload x level pair must have run");
    }

    #[test]
    fn a_corrupt_strip_does_not_poison_the_next_one() {
        let payload: Vec<u8> = (0..8192u32).map(|i| (i / 5 % 241) as u8).collect();
        let bits = [8u16];
        let state = CodecState::new();
        let cx = context(Some(&state), payload.len(), &bits);
        let stream = encode(&payload, &cx, CodecLevel::Default).expect("encode");

        let mut broken = stream.clone();
        for byte in broken.iter_mut().skip(20).take(16) {
            *byte ^= 0x33;
        }
        let mut out = vec![0u8; payload.len()];
        assert!(decode_into(&broken, &mut out, &cx).is_err());

        let mut out = vec![0u8; payload.len()];
        assert_eq!(
            decode_into(&stream, &mut out, &cx).expect("the strip after a bad one"),
            payload.len()
        );
        assert_eq!(out, payload);

        let mut out = vec![0u8; payload.len()];
        assert!(decode_into(&stream[..stream.len() / 2], &mut out, &cx).is_err());
        let mut out = vec![0u8; payload.len()];
        assert_eq!(
            decode_into(&stream, &mut out, &cx).expect("after a truncated one"),
            payload.len()
        );
        assert_eq!(out, payload);
    }

    #[test]
    fn a_thousand_strip_page_is_byte_identical_through_one_decoder() {
        // The shape the cache exists for: many small streams in a row.
        let strip: Vec<u8> = (0..4096u32).map(|i| (i / 3 % 251) as u8).collect();
        let bits = [8u16];
        let state = CodecState::new();
        let cx = context(Some(&state), strip.len(), &bits);
        let stream = encode(&strip, &cx, CodecLevel::Default).expect("encode");
        for round in 0..200 {
            let mut out = vec![0u8; strip.len()];
            assert_eq!(
                decode_into(&stream, &mut out, &cx).expect("decode"),
                strip.len(),
                "round {round}"
            );
            assert_eq!(out, strip, "round {round}");
        }
    }

    #[test]
    fn the_slot_really_holds_a_decoder_after_a_decode() {
        let state = CodecState::new();
        assert!(format!("{state:?}").contains("XzSlot(0)"), "{state:?}");
        let bits = [8u16];
        let payload = vec![9u8; 512];
        let cx = context(Some(&state), payload.len(), &bits);
        let stream = encode(&payload, &cx, CodecLevel::Default).expect("encode");
        let mut out = vec![0u8; payload.len()];
        decode_into(&stream, &mut out, &cx).expect("decode");
        assert!(format!("{state:?}").contains("XzSlot(1)"), "{state:?}");
    }
}

#[cfg(feature = "deflate")]
mod deflate {
    use super::payloads;
    use oxiarc_tiff::compression::{CodecContext, CodecLevel, CodecState, decode_into, encode};
    use oxiarc_tiff::{CompressionMethod, Endian, Leniency};

    fn context<'a>(state: Option<&'a CodecState>, len: usize, bits: &'a [u16]) -> CodecContext<'a> {
        let mut cx = CodecContext::new(
            CompressionMethod::Deflate,
            len.max(1),
            1,
            bits,
            1,
            Endian::Little,
        );
        cx.state = state;
        cx
    }

    #[test]
    fn a_reused_machine_matches_a_fresh_one_on_every_payload() {
        let state = CodecState::new();
        let bits = [8u16];
        let mut decoded = 0usize;
        for payload in payloads() {
            for level in [CodecLevel::Level(1), CodecLevel::Level(9)] {
                let fresh_cx = context(None, payload.len(), &bits);
                let stream = encode(&payload, &fresh_cx, level).expect("encode");
                let mut fresh = vec![0u8; payload.len()];
                let a = decode_into(&stream, &mut fresh, &fresh_cx).expect("fresh decode");

                let reused_cx = context(Some(&state), payload.len(), &bits);
                let mut reused = vec![0u8; payload.len()];
                let b = decode_into(&stream, &mut reused, &reused_cx).expect("reused decode");

                assert_eq!(a, b, "written count for {} bytes", payload.len());
                assert_eq!(fresh, reused, "bytes for {} bytes", payload.len());
                assert_eq!(fresh, payload);
                decoded += 1;
            }
        }
        assert_eq!(decoded, 12, "every payload x level pair must have run");
    }

    #[test]
    fn a_corrupt_strip_does_not_poison_the_next_one() {
        // A machine put back mid-stream must be reset before the next chunk
        // borrows it, or every strip after a damaged one inherits its state.
        let payload: Vec<u8> = (0..8192u32).map(|i| (i / 11 % 251) as u8).collect();
        let bits = [8u16];
        let state = CodecState::new();
        let cx = context(Some(&state), payload.len(), &bits);
        let stream = encode(&payload, &cx, CodecLevel::Default).expect("encode");

        let mut broken = stream.clone();
        for byte in broken.iter_mut().skip(4).take(20) {
            *byte ^= 0x5a;
        }
        let mut out = vec![0u8; payload.len()];
        assert!(
            decode_into(&broken, &mut out, &cx).is_err(),
            "corrupt strip"
        );
        let mut out = vec![0u8; payload.len()];
        assert_eq!(
            decode_into(&stream, &mut out, &cx).expect("the strip after a bad one"),
            payload.len()
        );
        assert_eq!(out, payload);

        let half = stream.len() / 2;
        let mut out = vec![0u8; payload.len()];
        assert!(decode_into(&stream[..half], &mut out, &cx).is_err());
        let mut out = vec![0u8; payload.len()];
        assert_eq!(
            decode_into(&stream, &mut out, &cx).expect("after a truncated one"),
            payload.len()
        );
        assert_eq!(out, payload);
    }

    #[test]
    fn a_leniency_change_rebuilds_rather_than_reuses() {
        // `verify_checksum` is a consuming builder, so a lenient chunk after a
        // strict one has to get a machine built for it — and still decode.
        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let bits = [8u16];
        let state = CodecState::new();
        let strict = context(Some(&state), payload.len(), &bits);
        let mut lenient = context(Some(&state), payload.len(), &bits);
        lenient.leniency = Leniency::Lenient;
        let stream = encode(&payload, &strict, CodecLevel::Default).expect("encode");
        for cx in [&strict, &lenient, &strict, &lenient] {
            let mut out = vec![0u8; payload.len()];
            assert_eq!(
                decode_into(&stream, &mut out, cx).expect("decode"),
                payload.len()
            );
            assert_eq!(out, payload);
        }
    }

    #[test]
    fn the_pool_really_holds_a_machine_after_a_decode() {
        let state = CodecState::new();
        assert!(
            format!("{state:?}").contains("InflateSlot([])"),
            "{state:?}"
        );
        let bits = [8u16];
        let payload = vec![3u8; 512];
        let cx = context(Some(&state), payload.len(), &bits);
        let stream = encode(&payload, &cx, CodecLevel::Default).expect("encode");
        let mut out = vec![0u8; payload.len()];
        decode_into(&stream, &mut out, &cx).expect("decode");
        assert!(
            format!("{state:?}").contains("InflateSlot([true])"),
            "{state:?}"
        );
    }

    #[test]
    fn eight_threads_through_one_state_decode_what_one_thread_does() {
        // The property the pool exists for: `rayon` hands the same
        // `CodecState` to every worker, so two chunks decode at once.
        let payload: Vec<u8> = (0..32_768u32).map(|i| (i / 17 % 251) as u8).collect();
        let bits = [8u16];
        let serial_cx = context(None, payload.len(), &bits);
        let stream = encode(&payload, &serial_cx, CodecLevel::Default).expect("encode");
        let state = CodecState::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let cx = context(Some(&state), payload.len(), &bits);
                    for _ in 0..50 {
                        let mut out = vec![0u8; payload.len()];
                        assert_eq!(
                            decode_into(&stream, &mut out, &cx).expect("threaded decode"),
                            payload.len()
                        );
                        assert_eq!(out, payload);
                    }
                });
            }
        });
        // Every borrowed machine came back; a pool that leaked one would show
        // fewer, and one that never pooled would show none.
        assert!(
            !format!("{state:?}").contains("InflateSlot([])"),
            "{state:?}"
        );
    }
}

#[cfg(feature = "ccitt")]
mod ccitt {
    use oxiarc_tiff::compression::{CodecContext, CodecLevel, CodecState, decode_into, encode};
    use oxiarc_tiff::tags::PhotometricInterpretation;
    use oxiarc_tiff::{CompressionMethod, Endian};

    const WIDTH: usize = 1728;
    const ROWS: usize = 32;

    fn context<'a>(state: Option<&'a CodecState>, bits: &'a [u16]) -> CodecContext<'a> {
        let mut cx = CodecContext::new(
            CompressionMethod::CcittFax4,
            WIDTH,
            ROWS,
            bits,
            1,
            Endian::Little,
        );
        cx.photometric = PhotometricInterpretation::WhiteIsZero;
        cx.state = state;
        cx
    }

    /// A bilevel page with runs of every length class.
    fn page() -> Vec<u8> {
        let row_bytes = WIDTH / 8;
        let mut out = vec![0u8; row_bytes * ROWS];
        for row in 0..ROWS {
            for x in 0..WIDTH {
                let black = (x / (row + 1)) % 3 == 0 || (x % 97) < row;
                if black {
                    out[row * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        out
    }

    #[test]
    fn reused_changing_element_buffers_decode_what_fresh_ones_do() {
        let data = page();
        let bits = [1u16];
        let fresh_cx = context(None, &bits);
        let coded = encode(&data, &fresh_cx, CodecLevel::Default).expect("encode");
        let state = CodecState::new();
        let reused_cx = context(Some(&state), &bits);
        for round in 0..25 {
            let mut fresh = vec![0u8; data.len()];
            let mut reused = vec![0u8; data.len()];
            let a = decode_into(&coded, &mut fresh, &fresh_cx).expect("fresh");
            let b = decode_into(&coded, &mut reused, &reused_cx).expect("reused");
            assert_eq!(a, b, "round {round}");
            assert_eq!(fresh, reused, "round {round}");
            assert_eq!(reused, data, "round {round}");
        }
        assert!(
            !format!("{state:?}").contains("FaxScratch(0)"),
            "the buffers must survive: {state:?}"
        );
    }

    #[test]
    fn eight_threads_through_one_state_decode_what_one_thread_does() {
        let data = page();
        let bits = [1u16];
        let serial_cx = context(None, &bits);
        let coded = encode(&data, &serial_cx, CodecLevel::Default).expect("encode");
        let state = CodecState::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    let cx = context(Some(&state), &bits);
                    for _ in 0..25 {
                        let mut out = vec![0u8; data.len()];
                        assert_eq!(
                            decode_into(&coded, &mut out, &cx).expect("threaded decode"),
                            data.len()
                        );
                        assert_eq!(out, data);
                    }
                });
            }
        });
    }
}

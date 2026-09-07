//! A worked example of the [`Codec`] plugin trait end to end: [`Encoder`]
//! writes a page with a custom codec registered through [`CodecRegistry`],
//! and [`Decoder`] reads it back through the same registry -- proving the
//! plugin hook (tiff-design.md D-5: LERC/WebP/JXL are reachable this way
//! without this crate vendoring them) actually wires a codec into *both*
//! halves of the public API, not only the low-level dispatch functions
//! `compression::mod.rs`'s own unit tests exercise.

use oxiarc_tiff::{
    Codec, CodecContext, CodecRegistry, ColorType, Compression, Decoder, Encoder, ImageSpec,
    Result, Tag, TiffError, UnsupportedError,
};
use std::io::Cursor;
use std::sync::Arc;

/// The registered method: [`oxiarc_tiff::CompressionMethod::Webp`] (50001)
/// -- thematically apt, since WebP is one of the codecs tiff-design.md names
/// as a plugin candidate this crate deliberately does not vendor (D-5).
const REGISTERED_METHOD: u16 = 50001;

/// The simplest *correct* codec that is not a no-op: every byte is
/// bitwise-inverted. Self-inverse, so `decode_into` and `encode` are one line
/// each, and it round-trips through the full pipeline exactly like a real
/// compressor would -- proving the registry hook reaches [`Decoder`] and
/// [`Encoder`] both, not just [`oxiarc_tiff::compression::decode_into_with`].
#[derive(Debug)]
struct InvertingCodec;

impl Codec for InvertingCodec {
    fn method(&self) -> u16 {
        REGISTERED_METHOD
    }

    fn decode_into(&self, src: &[u8], dst: &mut [u8], _cx: &CodecContext<'_>) -> Result<usize> {
        let n = src.len().min(dst.len());
        for (out, byte) in dst.iter_mut().zip(src.iter()) {
            *out = !byte;
        }
        Ok(n)
    }

    fn encode(&self, src: &[u8], _cx: &CodecContext<'_>) -> Result<Vec<u8>> {
        Ok(src.iter().map(|b| !b).collect())
    }
}

fn registry_with_inverting_codec() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    registry.register(Arc::new(InvertingCodec));
    registry
}

#[test]
fn a_registered_codec_round_trips_through_the_public_encoder_and_decoder() {
    let pixels: Vec<u8> = (0..64u32).map(|i| i as u8).collect();
    let spec = ImageSpec::new(8, 8, ColorType::Gray(8))
        .with_compression(Compression::Registered(REGISTERED_METHOD));

    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .with_codecs(registry_with_inverting_codec())
        .write_image(&spec, &pixels)
        .expect("write with a registered codec");
    let bytes = buffer.into_inner();

    // The file really carries the registered compression value, not
    // something this crate silently substituted.
    let mut probe = Decoder::new(Cursor::new(bytes.clone())).expect("probe decoder");
    let tag = probe.get_tag(Tag::Compression).expect("compression tag");
    assert_eq!(tag.first_u16(), Some(REGISTERED_METHOD));

    // Reading it back *without* the registry attached fails by name -- the
    // built-in dispatch has no codec for this value either -- rather than
    // silently returning garbage.
    let mut unregistered = Decoder::new(Cursor::new(bytes.clone())).expect("unregistered decoder");
    let err = unregistered.read_image().expect_err("no registry attached");
    assert!(matches!(
        err,
        TiffError::Unsupported(UnsupportedError::Compression(m)) if m == REGISTERED_METHOD
    ));

    // With the same registry attached, decode recovers the original pixels.
    let mut decoder = Decoder::new(Cursor::new(bytes))
        .expect("decoder")
        .with_codecs(registry_with_inverting_codec());
    let decoded = decoder.read_image().expect("read with a registered codec");
    assert_eq!(decoded.as_u8(), Some(pixels.as_slice()));
}

/// [`CodecRegistry`] is consulted *before* the built-in dispatch (documented
/// on [`CodecRegistry`] itself), so a plugin can even take over a method this
/// crate implements natively -- useful for a consumer that wants a different
/// LZW variant, say. Registering [`InvertingCodec`] for tag value 1
/// (`CompressionMethod::None`, otherwise a plain byte copy) proves the
/// override, not just the fallback-to-plugin case the first test covers.
#[test]
fn a_registered_codec_overrides_a_built_in_method() {
    #[derive(Debug)]
    struct OverridingCodec;
    impl Codec for OverridingCodec {
        fn method(&self) -> u16 {
            1 // CompressionMethod::None
        }
        fn decode_into(&self, src: &[u8], dst: &mut [u8], _cx: &CodecContext<'_>) -> Result<usize> {
            let n = src.len().min(dst.len());
            for (out, byte) in dst.iter_mut().zip(src.iter()) {
                *out = !byte;
            }
            Ok(n)
        }
        fn encode(&self, src: &[u8], _cx: &CodecContext<'_>) -> Result<Vec<u8>> {
            Ok(src.iter().map(|b| !b).collect())
        }
    }

    let pixels: Vec<u8> = vec![0, 1, 2, 3, 4, 5, 6, 7];
    let spec = ImageSpec::new(4, 2, ColorType::Gray(8)); // defaults to Compression::None
    let mut registry = CodecRegistry::new();
    registry.register(Arc::new(OverridingCodec));

    let mut buffer = Cursor::new(Vec::new());
    Encoder::new(&mut buffer)
        .expect("encoder")
        .with_codecs(registry)
        .write_image(&spec, &pixels)
        .expect("write");
    let bytes = buffer.into_inner();

    // Undecoded, the stored bytes are the bitwise-inverted pixels, not the
    // pixels themselves -- proof the override actually ran on write.
    let mut raw_probe = Decoder::new(Cursor::new(bytes.clone())).expect("decoder");
    let raw = raw_probe.read_chunk_raw(0).expect("raw chunk");
    let inverted: Vec<u8> = pixels.iter().map(|b| !b).collect();
    assert_eq!(raw, inverted);

    let mut registry = CodecRegistry::new();
    registry.register(Arc::new(OverridingCodec));
    let mut decoder = Decoder::new(Cursor::new(bytes))
        .expect("decoder")
        .with_codecs(registry);
    let decoded = decoder.read_image().expect("read with override");
    assert_eq!(decoded.as_u8(), Some(pixels.as_slice()));
}

//! Resumability: feeding the same file in any size of piece must decode to
//! exactly the same bytes.

mod common;

use common::{ChunkedReader, PngBuilder, Rng};
use oxiarc_png::{BitDepth, ColorType, Decoded, StreamingDecoder, Transformations};

fn sample_png(interlace: bool) -> (Vec<u8>, Vec<u8>) {
    let mut rng = Rng::new(0xF00D);
    let samples = rng.bytes(23 * 17 * 4);
    let png = PngBuilder::new(23, 17, ColorType::Rgba, BitDepth::Eight)
        .interlaced(interlace)
        .idat_split(37)
        .build_from_samples(&samples);
    (png, samples)
}

#[test]
fn chunked_reads_match_a_single_read() {
    for interlace in [false, true] {
        let (png, samples) = sample_png(interlace);
        let reference = oxiarc_png::decode(&png).expect("whole");
        assert_eq!(reference.data, samples);
        for chunk in [1usize, 2, 3, 5, 13, 64, 1024] {
            let image = oxiarc_png::decode_reader(ChunkedReader::new(&png, chunk))
                .unwrap_or_else(|e| panic!("chunk {chunk} interlace {interlace}: {e}"));
            assert_eq!(image.data, reference.data, "chunk {chunk}");
            assert_eq!(image.width, reference.width);
        }
    }
}

#[test]
fn interrupted_reads_are_retried() {
    let (png, _) = sample_png(false);
    let reference = oxiarc_png::decode(&png).expect("whole");
    let image = oxiarc_png::decode_reader(ChunkedReader::new(&png, 7).interrupting(3))
        .expect("interrupted");
    assert_eq!(image.data, reference.data);
}

#[test]
fn the_push_decoder_sees_the_same_events_at_every_chunk_size() {
    let (png, _) = sample_png(false);
    let reference = drive(&png, png.len());
    for chunk in [1usize, 2, 3, 9, 100] {
        assert_eq!(drive(&png, chunk), reference, "chunk size {chunk}");
    }
}

/// Feed `png` to a [`StreamingDecoder`] `chunk` bytes at a time, collecting
/// the events.
fn drive(png: &[u8], chunk: usize) -> Vec<String> {
    let mut decoder = StreamingDecoder::new();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut guard = 0;
    while pos < png.len() {
        guard += 1;
        assert!(guard < 1_000_000, "no progress");
        let end = (pos + chunk).min(png.len());
        let (consumed, event) = decoder.update(&png[pos..end], None).expect("update");
        pos += consumed;
        match event {
            Decoded::Nothing => {}
            other => out.push(format!("{other:?}")),
        }
        if consumed == 0 && decoder.is_finished() {
            break;
        }
    }
    out
}

#[test]
fn transformations_are_stable_across_chunk_sizes() {
    let mut rng = Rng::new(3);
    let samples = rng.bytes(9 * 5);
    let png = PngBuilder::new(9, 5, ColorType::Indexed, BitDepth::Eight)
        .chunk(
            oxiarc_png::chunk::PLTE,
            &(0..256u16)
                .flat_map(|i| [i as u8, (i * 3) as u8, (i * 5) as u8])
                .collect::<Vec<_>>(),
        )
        .idat_split(5)
        .build_from_samples(&samples);
    let reference = oxiarc_png::decode_with(
        &png,
        Default::default(),
        Transformations::EXPAND | Transformations::ALPHA,
    )
    .expect("whole");
    for chunk in [1usize, 4, 17] {
        let mut decoder = oxiarc_png::Decoder::new(ChunkedReader::new(&png, chunk));
        decoder.set_transformations(Transformations::EXPAND | Transformations::ALPHA);
        let mut reader = decoder.read_info().expect("read_info");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("size")];
        reader.next_frame(&mut buf).expect("frame");
        assert_eq!(buf, reference.data, "chunk {chunk}");
    }
}

#[test]
fn a_deeply_split_idat_run_is_reassembled() {
    let mut rng = Rng::new(11);
    let samples = rng.bytes(32 * 8 * 3);
    // One byte per IDAT chunk: 200+ chunks, every one crossing a zlib symbol.
    let png = PngBuilder::new(32, 8, ColorType::Rgb, BitDepth::Eight)
        .idat_split(1)
        .build_from_samples(&samples);
    let image = oxiarc_png::decode(&png).expect("decode");
    assert_eq!(image.data, samples);
}

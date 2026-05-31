use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use oxiarc_szip::{SzipParams, decode, encode};
use std::hint::black_box;

fn make_params(bpp: u8, ppb: u32, samples: usize) -> SzipParams {
    SzipParams {
        bits_per_pixel: bpp,
        pixels_per_block: ppb,
        samples,
        reference_sample_interval: ppb,
        msb: true,
        nn_preprocess: false,
        rsi_byte_align: false,
    }
}

fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");
    for n in [64usize, 256, 1024, 4096] {
        let params = make_params(8, 8, n);
        let samples: Vec<u64> = (0..n as u64).map(|i| i % 256).collect();
        group.bench_with_input(
            BenchmarkId::new("8bpp", n),
            &(samples, params),
            |b, (s, p)| b.iter(|| encode(black_box(s), black_box(p)).unwrap()),
        );
    }
    group.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode");
    for n in [64usize, 256, 1024, 4096] {
        let params = make_params(8, 8, n);
        let samples: Vec<u64> = (0..n as u64).map(|i| i % 256).collect();
        let compressed = encode(&samples, &params).unwrap();
        group.bench_with_input(
            BenchmarkId::new("8bpp", n),
            &(compressed, params),
            |b, (data, p)| b.iter(|| decode(black_box(data), black_box(p)).unwrap()),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use oxiarc_snappy::{compress, decompress};

fn bench_compress(c: &mut Criterion) {
    let data: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();

    c.bench_function("snappy_compress_64k", |b| {
        b.iter(|| {
            black_box(compress(black_box(&data)));
        });
    });

    let repeated = vec![0xABu8; 65536];
    c.bench_function("snappy_compress_64k_repeated", |b| {
        b.iter(|| {
            black_box(compress(black_box(&repeated)));
        });
    });
}

fn bench_decompress(c: &mut Criterion) {
    let data: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
    let compressed = compress(&data);

    c.bench_function("snappy_decompress_64k", |b| {
        b.iter(|| {
            black_box(decompress(black_box(&compressed))).expect("decompress should succeed");
        });
    });

    let repeated = vec![0xABu8; 65536];
    let compressed_repeated = compress(&repeated);

    c.bench_function("snappy_decompress_64k_repeated", |b| {
        b.iter(|| {
            black_box(decompress(black_box(&compressed_repeated)))
                .expect("decompress should succeed");
        });
    });
}

criterion_group!(benches, bench_compress, bench_decompress);
criterion_main!(benches);

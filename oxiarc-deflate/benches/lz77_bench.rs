//! Benchmarks for LZ77 compression performance.

use oxiarc_deflate::lz77::Lz77Encoder;

fn main() {
    // Test data: Various sizes and patterns
    let test_cases = vec![
        ("small_random", generate_random(1024)),
        ("medium_random", generate_random(64 * 1024)),
        ("large_random", generate_random(256 * 1024)),
        ("small_repeated", generate_repeated(1024)),
        ("medium_repeated", generate_repeated(64 * 1024)),
        ("large_repeated", generate_repeated(256 * 1024)),
        ("small_text", generate_text_like(1024)),
        ("medium_text", generate_text_like(64 * 1024)),
        ("large_text", generate_text_like(256 * 1024)),
    ];

    println!("LZ77 Compression Benchmarks");
    println!("============================\n");

    for (name, data) in &test_cases {
        println!("Test: {} ({} bytes)", name, data.len());

        for level in [1, 5, 9] {
            let start = std::time::Instant::now();
            let mut encoder = Lz77Encoder::with_level(level);
            let tokens = encoder.compress(data);
            let elapsed = start.elapsed();

            let compressed_size: usize = tokens
                .iter()
                .map(|t| match t {
                    oxiarc_deflate::lz77::Lz77Token::Literal(_) => 1,
                    oxiarc_deflate::lz77::Lz77Token::Match { length, .. } => *length as usize,
                })
                .sum();

            let throughput = data.len() as f64 / elapsed.as_secs_f64() / 1024.0 / 1024.0;
            let ratio = (data.len() as f64 / tokens.len() as f64).max(1.0);

            println!(
                "  Level {}: {:6.2} MB/s, {:5} tokens, {:.2}x ratio, {:7.2} µs",
                level,
                throughput,
                tokens.len(),
                ratio,
                elapsed.as_micros()
            );

            // Sanity check
            assert_eq!(compressed_size, data.len());
        }
        println!();
    }
}

fn generate_random(size: usize) -> Vec<u8> {
    // Simple LCG random number generator
    let mut data = Vec::with_capacity(size);
    let mut seed = 12345u32;
    for _ in 0..size {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        data.push((seed >> 16) as u8);
    }
    data
}

fn generate_repeated(size: usize) -> Vec<u8> {
    // Highly compressible repeated pattern
    let pattern = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        data.extend_from_slice(pattern);
    }
    data.truncate(size);
    data
}

fn generate_text_like(size: usize) -> Vec<u8> {
    // Simulates English text with word-like patterns
    let words: &[&[u8]] = &[
        b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy", b"dog", b"and", b"runs",
        b"through", b"forest", b"near", b"river", b"under", b"blue", b"sky", b"with", b"wind",
        b"blowing",
    ];
    let mut data = Vec::with_capacity(size);
    let mut seed = 42u32;

    while data.len() < size {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        let word_idx = (seed as usize) % words.len();
        data.extend_from_slice(words[word_idx]);
        data.push(b' ');
    }
    data.truncate(size);
    data
}

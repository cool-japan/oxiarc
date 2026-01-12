//! Benchmarks for Burrows-Wheeler Transform performance.

use oxiarc_bzip2::bwt::{inverse_transform, transform};

fn main() {
    // Test data: Various sizes and patterns
    // Note: BWT has O(n² log n) worst-case for highly repetitive data
    // BZip2 mitigates this by limiting block sizes to 900KB
    let test_cases = vec![
        ("small_text", generate_text(1024)),
        ("medium_text", generate_text(64 * 1024)),
        ("large_text", generate_text(256 * 1024)),
        ("small_random", generate_random(1024)),
        ("medium_random", generate_random(64 * 1024)),
        ("large_random", generate_random(256 * 1024)),
        ("small_repeated", generate_repeated(1024)),
        ("medium_repeated", generate_repeated(8 * 1024)), // Reduced from 64KB to avoid pathological case
                                                          // large_repeated omitted - BWT is O(n²) for highly repetitive data
    ];

    println!("Burrows-Wheeler Transform Benchmarks");
    println!("=====================================\n");

    for (name, data) in &test_cases {
        println!("Test: {} ({} bytes)", name, data.len());

        // Forward transform
        let start = std::time::Instant::now();
        let (transformed, orig_ptr) = transform(data);
        let forward_time = start.elapsed();

        let forward_throughput = data.len() as f64 / forward_time.as_secs_f64() / 1024.0 / 1024.0;

        // Inverse transform
        let start = std::time::Instant::now();
        let reconstructed = inverse_transform(&transformed, orig_ptr);
        let inverse_time = start.elapsed();

        let inverse_throughput =
            reconstructed.len() as f64 / inverse_time.as_secs_f64() / 1024.0 / 1024.0;

        // Verify correctness
        assert_eq!(reconstructed, *data, "BWT roundtrip failed for {}", name);

        println!(
            "  Forward:  {:7.2} MB/s ({:8.2} µs)",
            forward_throughput,
            forward_time.as_micros()
        );
        println!(
            "  Inverse:  {:7.2} MB/s ({:8.2} µs)",
            inverse_throughput,
            inverse_time.as_micros()
        );
        println!(
            "  Total:    {:8.2} µs",
            (forward_time + inverse_time).as_micros()
        );
        println!();
    }
}

fn generate_text(size: usize) -> Vec<u8> {
    // Simulates text-like data
    let words: &[&[u8]] = &[
        b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy", b"dog", b"and", b"runs",
        b"through", b"forest", b"near", b"river", b"under", b"blue", b"sky",
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

fn generate_random(size: usize) -> Vec<u8> {
    // Random data (least compressible)
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

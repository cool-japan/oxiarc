//! Fuzz target for the decompress-into-a-slice path:
//! `oxiarc_deflate::inflate_into` / `zlib_decompress_into`.
//!
//! Beyond "must not panic", this cross-checks the slice sink against the
//! growable-`Vec` decoder: whenever `inflate` succeeds and the output fits,
//! `inflate_into` must produce byte-identical output, and vice versa. A
//! divergence between the two sinks is exactly the class of bug a
//! fixed-buffer fast path can introduce.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Cap the scratch buffer so a decompression bomb cannot make the fuzzer
    // allocate without bound; a stream that needs more is expected to error.
    const CAP: usize = 1 << 20;

    let vec_result = oxiarc_deflate::inflate(data);

    let mut scratch = vec![0u8; CAP];
    let into_result = oxiarc_deflate::inflate_into(data, &mut scratch);

    match (&vec_result, &into_result) {
        (Ok(expected), Ok(n)) => {
            assert_eq!(expected.len(), *n, "length mismatch between decode paths");
            assert_eq!(
                &expected[..],
                &scratch[..*n],
                "byte mismatch between decode paths"
            );
        }
        (Ok(expected), Err(_)) => {
            // Only a genuine overflow of the fixed buffer may differ.
            assert!(
                expected.len() > CAP,
                "inflate_into rejected a stream that fits in the buffer"
            );
        }
        (Err(_), Ok(n)) => {
            panic!("inflate_into accepted ({n} bytes) a stream inflate rejected");
        }
        (Err(_), Err(_)) => {}
    }

    // The zlib wrapper must agree with its own into-slice variant too.
    let zlib_vec = oxiarc_deflate::zlib_decompress(data);
    let zlib_into = oxiarc_deflate::zlib_decompress_into(data, &mut scratch);
    if let (Ok(expected), Ok(n)) = (&zlib_vec, &zlib_into) {
        assert_eq!(expected.len(), *n);
        assert_eq!(&expected[..], &scratch[..*n]);
    }
});

//! HIGHEST PRIORITY fuzz target: `oxiarc_snappy::crc32c::crc32c`.
//!
//! `crc32c_sse42` (crc32c.rs:65-95) performs raw unaligned pointer reads
//! (`read_unaligned::<u64>`/`read_unaligned::<u32>`) over attacker-influenced
//! byte slices, walking the pointer forward in 8-/4-byte strides and then
//! reconstructing a tail slice with `core::slice::from_raw_parts`. Any
//! off-by-one in the stride/tail-length bookkeeping is a read past the end
//! of the buffer (or a dangling/overlapping slice), so this target throws a
//! wide spread of buffer lengths and alignments at the public, safe
//! `crc32c` entry point, which internally dispatches to whichever SIMD (or
//! scalar-fallback) implementation the runtime CPU supports.
//!
//! To specifically stress every length modulo 8/4/1 (i.e. every possible
//! tail-remainder branch of the SSE4.2 loop), we don't just hash `data` as
//! one buffer: we also hash every prefix length 0..=data.len(), which cheaply
//! sweeps all residues without needing a corpus tuned to specific sizes.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Whole-buffer pass (exercises whatever length the fuzzer picked).
    let _ = oxiarc_snappy::crc32c::crc32c(data);

    // Sweep every prefix so all stride/tail-remainder edge cases (mod 8,
    // mod 4, and the 0/1/2/3-byte scalar tail) get hit even from a small
    // corpus, without requiring libFuzzer to discover each length by chance.
    // Bounded to avoid quadratic blowup on large inputs from the corpus.
    let sweep_len = data.len().min(256);
    for prefix_len in 0..=sweep_len {
        let _ = oxiarc_snappy::crc32c::crc32c(&data[..prefix_len]);
    }

    // Also exercise crc32c starting from a non-zero, non-8-byte-aligned
    // offset into the buffer, since `data.as_ptr()` alignment is not
    // guaranteed to be a multiple of 8 either.
    if !data.is_empty() {
        for start in 1..data.len().min(16) {
            let _ = oxiarc_snappy::crc32c::crc32c(&data[start..]);
        }
    }

    // masked/unmasked round trip must never panic either.
    let crc = oxiarc_snappy::crc32c::crc32c(data);
    let masked = oxiarc_snappy::crc32c::mask_checksum(crc);
    let unmasked = oxiarc_snappy::crc32c::unmask_checksum(masked);
    assert_eq!(crc, unmasked, "mask/unmask round trip must be lossless");
});

//! Fuzz target for LZH/LHA decoding: `oxiarc_lzhuf::decode_lzh`.
//!
//! `decode_lzh` needs a `LzhMethod` (window size / huffman variant) and a
//! declared uncompressed size alongside the raw compressed bytes, so the
//! fuzz input is split via `arbitrary::Unstructured`: the first byte selects
//! the method, the next 4 bytes (little-endian, masked to a sane ceiling)
//! become the declared uncompressed size, and the remainder is the
//! compressed payload fed to the decoder.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_lzhuf::LzhMethod;

/// Map a raw byte onto one of the real, decodable `LzhMethod` variants.
///
/// `LzhMethod::Unknown([u8; 5])` and `LzhMethod::Lhd` are intentionally
/// excluded: the former carries no decodable payload by construction and
/// the latter denotes a directory entry, so neither is a meaningful target
/// for the byte-stream decoder itself.
fn pick_method(tag: u8) -> LzhMethod {
    match tag % 5 {
        0 => LzhMethod::Lh0,
        1 => LzhMethod::Lh1,
        2 => LzhMethod::Lh4,
        3 => LzhMethod::Lh5,
        _ => LzhMethod::Lh6,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);

    let Ok(method_tag) = unstructured.arbitrary::<u8>() else {
        return;
    };
    let Ok(raw_size) = unstructured.arbitrary::<u32>() else {
        return;
    };
    let method = pick_method(method_tag);

    // Cap the claimed uncompressed size so a bogus/huge declared size can't
    // force a gigantic allocation attempt purely from decoding metadata;
    // the decoder must still reject/accept the remaining bytes cleanly for
    // any size within this range without panicking.
    let uncompressed_size = u64::from(raw_size % (16 * 1024 * 1024));

    let payload = unstructured.take_rest();
    let _ = oxiarc_lzhuf::decode_lzh(payload, method, uncompressed_size);
});

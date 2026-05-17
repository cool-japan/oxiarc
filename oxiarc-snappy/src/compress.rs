//! Snappy block compression.
//!
//! Implements the Snappy compression algorithm using LZ77-style matching
//! with a hash table for fast match finding. The output format follows
//! the Snappy block format specification.

/// Maximum block size for Snappy compression (the framed format uses 64 KiB
/// uncompressed chunks, but the raw block format has no inherent limit).
const MAX_HASH_TABLE_SIZE: usize = 1 << 14; // 16384 entries

/// Minimum match length for a copy operation to be worthwhile.
const MIN_MATCH_LEN: usize = 4;

/// Maximum length of input that can be compressed in one block.
/// Snappy spec does not impose this, but we use it for safety.
const MAX_BLOCK_SIZE: usize = 1 << 24; // 16 MiB

/// Calculate the maximum compressed output size for a given input length.
///
/// This provides an upper bound, useful for pre-allocating buffers.
///
/// # Arguments
/// * `input_len` - Length of the uncompressed input.
///
/// # Returns
/// The maximum number of bytes the compressed output could occupy.
pub fn max_compress_len(input_len: usize) -> usize {
    // Varint-encoded length header (at most 5 bytes for u32)
    // + worst case: all literals, each 64-byte literal needs 1 tag byte + 64 data bytes
    // So worst case is input_len + input_len/64 + header
    // The Snappy spec says: 32 + input_len + input_len/6
    let varint_len = varint_encoded_len(input_len);
    varint_len + 32 + input_len + input_len / 6
}

/// Compress the input data using the Snappy block format.
///
/// # Arguments
/// * `input` - The data to compress.
///
/// # Returns
/// A vector containing the compressed data in Snappy block format.
pub fn compress(input: &[u8]) -> Vec<u8> {
    if input.is_empty() {
        // Empty input: just the varint-encoded length (0)
        return vec![0];
    }

    let mut output = Vec::with_capacity(max_compress_len(input.len()));

    // Write the uncompressed length as a varint
    encode_varint(input.len(), &mut output);

    // For very small inputs, just emit a literal
    if input.len() < MIN_MATCH_LEN {
        emit_literal(input, &mut output);
        return output;
    }

    // Process input in blocks up to MAX_BLOCK_SIZE
    let mut src_pos = 0;
    while src_pos < input.len() {
        let block_end = (src_pos + MAX_BLOCK_SIZE).min(input.len());
        let block = &input[src_pos..block_end];
        compress_block(block, src_pos, &mut output);
        src_pos = block_end;
    }

    output
}

/// Compress a single block of data.
fn compress_block(input: &[u8], _base_offset: usize, output: &mut Vec<u8>) {
    let input_len = input.len();

    if input_len < MIN_MATCH_LEN {
        emit_literal(input, output);
        return;
    }

    // Hash table: maps hash of 4-byte sequences to positions
    let hash_table_size = hash_table_size(input_len);
    let hash_shift = 32u32.saturating_sub(log2_floor(hash_table_size as u32));
    let mut hash_table = vec![0u32; hash_table_size];

    let mut literal_start = 0;

    // We need at least 4 bytes remaining to attempt a match
    let input_limit = if input_len > MIN_MATCH_LEN + 1 {
        input_len - MIN_MATCH_LEN - 1
    } else {
        emit_literal(input, output);
        return;
    };

    // Skip the first byte and start hashing from byte 1
    // (following the reference implementation's approach)
    let mut next_hash = hash4(&input[1..], hash_shift);
    let mut pos = 1;

    'outer: loop {
        // Find the next match using the hash table.
        // We use a skip strategy: start with step=1 and increase
        // to avoid spending too much time on incompressible data.
        let mut candidate;
        let mut skip = 32;
        let mut next_pos;

        loop {
            let hash = next_hash;
            let bytes_between = skip >> 5;
            skip += 1;
            next_pos = pos + bytes_between;

            if next_pos > input_limit {
                break 'outer;
            }

            candidate = hash_table[hash as usize] as usize;
            hash_table[hash as usize] = pos as u32;
            next_hash = hash4(&input[next_pos..], hash_shift);

            // Check if the candidate matches
            if candidate < pos
                && pos.wrapping_sub(candidate) <= 65535
                && input[candidate..candidate + 4] == input[pos..pos + 4]
            {
                break;
            }

            pos = next_pos;
        }

        // Emit any pending literal bytes before the match
        if literal_start < pos {
            emit_literal(&input[literal_start..pos], output);
        }

        // Extend the match as far as possible
        let match_offset = pos - candidate;
        let match_len = find_match_length(&input[candidate + 4..], &input[pos + 4..]) + 4;

        emit_copy(match_offset, match_len, output);
        pos += match_len;
        literal_start = pos;

        if pos >= input_limit {
            break;
        }

        // Insert hashes for the bytes we just matched (to improve future matching)
        // We insert at least 2 entries to keep the hash table populated
        let insert_end = (pos - 1).min(pos.saturating_sub(1));
        if pos >= 2 {
            let h = hash4(&input[pos - 2..], hash_shift);
            hash_table[h as usize] = (pos - 2) as u32;
            let h = hash4(&input[pos - 1..], hash_shift);
            hash_table[h as usize] = (pos - 1) as u32;
        }
        let _ = insert_end; // suppress unused warning

        literal_start = pos;
        next_hash = hash4(&input[pos + 1..], hash_shift);
        pos += 1;
    }

    // Emit remaining literal bytes
    if literal_start < input_len {
        emit_literal(&input[literal_start..], output);
    }
}

/// Find how many bytes match between two slices, starting from the beginning.
fn find_match_length(s1: &[u8], s2: &[u8]) -> usize {
    let limit = s1.len().min(s2.len());
    let mut i = 0;

    // Process 8 bytes at a time for speed
    while i + 8 <= limit {
        let v1 = u64::from_le_bytes([
            s1[i],
            s1[i + 1],
            s1[i + 2],
            s1[i + 3],
            s1[i + 4],
            s1[i + 5],
            s1[i + 6],
            s1[i + 7],
        ]);
        let v2 = u64::from_le_bytes([
            s2[i],
            s2[i + 1],
            s2[i + 2],
            s2[i + 3],
            s2[i + 4],
            s2[i + 5],
            s2[i + 6],
            s2[i + 7],
        ]);
        if v1 != v2 {
            // Find the first differing byte
            let xor = v1 ^ v2;
            return i + (xor.trailing_zeros() as usize / 8);
        }
        i += 8;
    }

    // Handle remaining bytes
    while i < limit && s1[i] == s2[i] {
        i += 1;
    }

    i
}

/// Emit a literal element into the output.
///
/// Snappy literal format:
/// - Tag byte: lower 2 bits = 00 (literal)
/// - Upper 6 bits encode length - 1 for lengths 1..=60
/// - For lengths > 60, the upper 6 bits encode 60..=63 and
///   additional bytes follow with the length.
fn emit_literal(literal: &[u8], output: &mut Vec<u8>) {
    let n = literal.len();
    if n == 0 {
        return;
    }

    let n_minus_1 = n - 1;

    if n_minus_1 < 60 {
        // Length fits in 6 bits of the tag byte
        // Tag type 00 (literal) is the lowest 2 bits
        output.push((n_minus_1 as u8) << 2);
    } else if n_minus_1 < 256 {
        output.push(60 << 2);
        output.push(n_minus_1 as u8);
    } else if n_minus_1 < 65536 {
        output.push(61 << 2);
        output.push(n_minus_1 as u8);
        output.push((n_minus_1 >> 8) as u8);
    } else if n_minus_1 < (1 << 24) {
        output.push(62 << 2);
        output.push(n_minus_1 as u8);
        output.push((n_minus_1 >> 8) as u8);
        output.push((n_minus_1 >> 16) as u8);
    } else {
        output.push(63 << 2);
        output.push(n_minus_1 as u8);
        output.push((n_minus_1 >> 8) as u8);
        output.push((n_minus_1 >> 16) as u8);
        output.push((n_minus_1 >> 24) as u8);
    }

    output.extend_from_slice(literal);
}

/// Emit a copy (back-reference) element into the output.
///
/// Snappy copy formats:
/// - Copy with 1-byte offset (tag 01): offset 0..2047, length 4..11
/// - Copy with 2-byte offset (tag 10): offset 0..65535, length 1..64
/// - Copy with 4-byte offset (tag 11): offset 0..2^32-1, length 1..64
fn emit_copy(offset: usize, mut length: usize, output: &mut Vec<u8>) {
    // Emit as many copy operations as needed (length may exceed single-op maximum)
    while length > 0 {
        if (4..=11).contains(&length) && offset <= 2047 {
            // Copy-1: 3-bit length, 11-bit offset (2 bytes total)
            // Tag byte: OOOOO LLL 01
            // Next byte: OOOOOOOO
            // where O = offset bits (11 total), L = (length - 4) (3 bits)
            let len_field = (length - 4) as u8;
            let offset_hi = ((offset >> 8) & 0x07) as u8;
            let offset_lo = (offset & 0xFF) as u8;
            output.push((offset_hi << 5) | (len_field << 2) | 0x01);
            output.push(offset_lo);
            return;
        } else if offset <= 65535 {
            // Copy-2: 6-bit length, 16-bit offset (3 bytes total)
            let emit_len = length.min(64);
            let len_field = (emit_len - 1) as u8;
            output.push((len_field << 2) | 0x02);
            output.push((offset & 0xFF) as u8);
            output.push(((offset >> 8) & 0xFF) as u8);
            length -= emit_len;
        } else {
            // Copy-4: 6-bit length, 32-bit offset (5 bytes total)
            let emit_len = length.min(64);
            let len_field = (emit_len - 1) as u8;
            output.push((len_field << 2) | 0x03);
            output.push((offset & 0xFF) as u8);
            output.push(((offset >> 8) & 0xFF) as u8);
            output.push(((offset >> 16) & 0xFF) as u8);
            output.push(((offset >> 24) & 0xFF) as u8);
            length -= emit_len;
        }
    }
}

/// Compress `input` using a prefix dictionary.
///
/// The dictionary seeds the hash table so matches inside it can be referenced.
/// The decoder must be supplied the same dictionary. Output is **NOT** compatible
/// with vanilla Snappy decoders; use [`crate::decompress::decompress_block_with_dict`]
/// to decode.
///
/// If `dict` is empty, the output is byte-identical to [`compress`].
///
/// The maximum dictionary size is 64 KiB; if `dict` is longer only the last
/// 64 KiB is used.
///
/// # OxiArc extension
/// This is an OxiArc-specific extension. The Snappy specification does not
/// define dictionary semantics.
pub fn compress_block_with_dict(input: &[u8], dict: &[u8]) -> Vec<u8> {
    // Clamp dict to the last 64 KiB.
    let dict = if dict.len() > 65536 {
        &dict[dict.len() - 65536..]
    } else {
        dict
    };

    // Empty-dict is byte-identical to standard compress.
    if dict.is_empty() {
        return compress(input);
    }

    if input.is_empty() {
        // Empty input with non-empty dict: just write varint(0)
        return vec![0];
    }

    let mut output = Vec::with_capacity(crate::compress::max_compress_len(input.len()));

    // Write the uncompressed length (input only, not dict).
    encode_varint(input.len(), &mut output);

    // For very small inputs, just emit literals — dict can't help.
    if input.len() < MIN_MATCH_LEN {
        emit_literal(input, output.as_mut());
        return output;
    }

    // Build a combined buffer: dict || input.
    let dict_len = dict.len();
    let combined_len = dict_len + input.len();

    // Allocate once; copy dict then input.
    let mut combined = Vec::with_capacity(combined_len);
    combined.extend_from_slice(dict);
    combined.extend_from_slice(input);

    // Determine hash table size based on combined length (covers the whole window).
    let hash_table_size = hash_table_size(combined_len);
    let hash_shift = 32u32.saturating_sub(log2_floor(hash_table_size as u32));
    let mut hash_table = vec![0u32; hash_table_size];

    // Pre-seed the hash table from the dictionary.
    // We store position + 1 so that 0 stays as the "empty" sentinel, and
    // subtract 1 when reading back.  This way dict positions are distinguishable
    // from the empty sentinel and the existing `candidate < pos` guard still works.
    //
    // Actually, the existing code uses 0 as "never matched" because all real
    // positions are >= 1 (pos starts at 1).  Dict positions are 0..dict_len-1,
    // so position 0 is a valid dict slot.  We seed positions as their raw
    // index in `combined`; the `candidate < pos` guard is satisfied because
    // dict positions (< dict_len) are always less than pos (>= dict_len + 1).
    if dict_len >= MIN_MATCH_LEN {
        let seed_limit = dict_len.saturating_sub(MIN_MATCH_LEN - 1);
        for i in 0..seed_limit {
            let h = hash4(&combined[i..], hash_shift);
            hash_table[h as usize] = i as u32;
        }
    }

    // Encode: iterate over input positions only (dict_len .. combined_len).
    let encode_start = dict_len;
    let encode_end = combined_len;

    // We need at least 4 bytes of input.
    let input_limit = if encode_end > encode_start + MIN_MATCH_LEN + 1 {
        encode_end - MIN_MATCH_LEN - 1
    } else {
        // Input too small for any match — emit as literal.
        emit_literal(input, output.as_mut());
        return output;
    };

    let mut literal_start = encode_start;
    let mut next_hash = hash4(&combined[encode_start + 1..], hash_shift);
    let mut pos = encode_start + 1;

    'outer: loop {
        let mut candidate;
        let mut skip = 32u32;
        let mut next_pos;

        loop {
            let hash = next_hash;
            let bytes_between = skip >> 5;
            skip += 1;
            next_pos = pos + bytes_between as usize;

            if next_pos > input_limit {
                break 'outer;
            }

            candidate = hash_table[hash as usize] as usize;
            hash_table[hash as usize] = pos as u32;
            next_hash = hash4(&combined[next_pos..], hash_shift);

            // Accept matches anywhere in [0, pos).
            // No 65535 cap: dict-region matches can span up to 64 KiB offset.
            // emit_copy handles large offsets with the copy-4 opcode.
            if candidate < pos && combined[candidate..candidate + 4] == combined[pos..pos + 4] {
                break;
            }

            pos = next_pos;
        }

        // Emit pending literal bytes (slice into input portion).
        if literal_start < pos {
            emit_literal(&combined[literal_start..pos], &mut output);
        }

        let match_offset = pos - candidate;
        let match_len = find_match_length(&combined[candidate + 4..], &combined[pos + 4..]) + 4;

        emit_copy(match_offset, match_len, &mut output);
        pos += match_len;
        literal_start = pos;

        if pos >= input_limit {
            break;
        }

        // Insert hashes for bytes just past the match.
        if pos >= 2 {
            let h = hash4(&combined[pos - 2..], hash_shift);
            hash_table[h as usize] = (pos - 2) as u32;
            let h = hash4(&combined[pos - 1..], hash_shift);
            hash_table[h as usize] = (pos - 1) as u32;
        }

        literal_start = pos;
        next_hash = hash4(&combined[pos + 1..], hash_shift);
        pos += 1;
    }

    // Emit remaining literal bytes.
    if literal_start < encode_end {
        emit_literal(&combined[literal_start..encode_end], &mut output);
    }

    output
}

/// Compute a hash of 4 bytes at the given position.
/// Uses a multiplicative hash to distribute values well.
fn hash4(data: &[u8], shift: u32) -> u32 {
    debug_assert!(data.len() >= 4);
    let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    // Knuth multiplicative hash
    v.wrapping_mul(0x1E35A7BD) >> shift
}

/// Determine the hash table size for a given input length.
/// Returns a power of two, capped at MAX_HASH_TABLE_SIZE.
fn hash_table_size(input_len: usize) -> usize {
    let mut size = 256;
    while size < MAX_HASH_TABLE_SIZE && size < input_len {
        size *= 2;
    }
    size
}

/// Compute floor(log2(n)) for n > 0.
fn log2_floor(n: u32) -> u32 {
    if n == 0 {
        return 0;
    }
    31 - n.leading_zeros()
}

/// Encode a usize as a varint and append to the output.
///
/// Snappy uses a standard LEB128-style varint encoding for the
/// uncompressed length header.
fn encode_varint(mut value: usize, output: &mut Vec<u8>) {
    loop {
        if value < 128 {
            output.push(value as u8);
            return;
        }
        output.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

/// Calculate the number of bytes needed to encode a varint.
fn varint_encoded_len(value: usize) -> usize {
    let mut n = value;
    let mut len = 1;
    while n >= 128 {
        n >>= 7;
        len += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_varint_encoding() {
        let mut buf = Vec::new();
        encode_varint(0, &mut buf);
        assert_eq!(buf, [0]);

        buf.clear();
        encode_varint(127, &mut buf);
        assert_eq!(buf, [127]);

        buf.clear();
        encode_varint(128, &mut buf);
        assert_eq!(buf, [0x80, 0x01]);

        buf.clear();
        encode_varint(300, &mut buf);
        assert_eq!(buf, [0xAC, 0x02]);
    }

    #[test]
    fn test_max_compress_len() {
        assert!(max_compress_len(0) > 0);
        assert!(max_compress_len(100) > 100);
        assert!(max_compress_len(1_000_000) > 1_000_000);
    }

    #[test]
    fn test_compress_empty() {
        let result = compress(b"");
        assert_eq!(result, vec![0]);
    }

    #[test]
    fn test_compress_small() {
        let input = b"abc";
        let compressed = compress(input);
        // Should start with varint(3) = 0x03
        assert_eq!(compressed[0], 3);
        // Then a literal tag + "abc"
        assert!(compressed.len() > 1);
    }

    #[test]
    fn test_compress_repeated() {
        let input = vec![b'A'; 1000];
        let compressed = compress(&input);
        // Repeated data should compress well
        assert!(compressed.len() < input.len());
    }

    #[test]
    fn test_hash4_deterministic() {
        let data = [0x01, 0x02, 0x03, 0x04, 0x05];
        let h1 = hash4(&data, 18);
        let h2 = hash4(&data, 18);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_find_match_length() {
        let s1 = b"abcdefgh";
        let s2 = b"abcdefgh";
        assert_eq!(find_match_length(s1, s2), 8);

        let s2 = b"abcdXfgh";
        assert_eq!(find_match_length(s1, s2), 4);

        let s2 = b"Xbcdefgh";
        assert_eq!(find_match_length(s1, s2), 0);
    }

    #[test]
    fn test_emit_literal_small() {
        let mut out = Vec::new();
        emit_literal(b"Hello", &mut out);
        // Tag: (5-1) << 2 | 0 = 16
        assert_eq!(out[0], 16);
        assert_eq!(&out[1..], b"Hello");
    }

    #[test]
    fn test_emit_literal_large() {
        let data = vec![0x42; 256];
        let mut out = Vec::new();
        emit_literal(&data, &mut out);
        // Should use the 60-tag format with one extra length byte
        assert_eq!(out[0], (60 << 2));
        assert_eq!(out[1], 255); // 256 - 1 = 255
    }

    #[test]
    fn test_emit_copy_short() {
        let mut out = Vec::new();
        emit_copy(10, 4, &mut out);
        // Should use copy-1 format (2 bytes)
        assert_eq!(out.len(), 2);
        assert_eq!(out[0] & 0x03, 0x01); // copy-1 tag
    }

    #[test]
    fn test_emit_copy_medium() {
        let mut out = Vec::new();
        emit_copy(3000, 20, &mut out);
        // offset > 2047, should use copy-2 format (3 bytes)
        assert_eq!(out.len(), 3);
        assert_eq!(out[0] & 0x03, 0x02); // copy-2 tag
    }

    #[test]
    fn test_log2_floor() {
        assert_eq!(log2_floor(1), 0);
        assert_eq!(log2_floor(2), 1);
        assert_eq!(log2_floor(3), 1);
        assert_eq!(log2_floor(4), 2);
        assert_eq!(log2_floor(16384), 14);
    }
}

//! Differential test of the 0.4.2 prefix/suffix decoder against the
//! pre-0.4.2 per-code-`Vec` decoder, on inputs neither round-trip nor
//! oracle tests can reach.
//!
//! The rewrite added two `memcpy` fast paths (a whole string copied back
//! from its earlier occurrence in the output, and "parent + one suffix
//! byte") plus a shared sink so the `Vec` and slice entry points run the
//! same loop. Round-trip tests only ever feed those paths *well-formed*
//! streams this crate produced. This test feeds them arbitrary bytes,
//! corrupted streams and truncations, and requires the two independent
//! implementations to agree exactly — same bytes, and the same
//! success/failure classification.
//!
//! `legacy_decode` below is the algorithm oxiarc-lzw shipped before 0.4.2
//! (`Vec<Vec<u8>>` code table, one `to_vec()` per emitted code), kept
//! deliberately naive: it is the reference, so it must not share code with
//! the implementation under test.

use oxiarc_lzw::{decompress_tiff, decompress_tiff_into};

// ---------------------------------------------------------------------------
// Reference implementation (pre-0.4.2 algorithm, TIFF parameters inlined)
// ---------------------------------------------------------------------------

struct LegacyBitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    buffer: u32,
    bits_in_buffer: u8,
}

impl<'a> LegacyBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            buffer: 0,
            bits_in_buffer: 0,
        }
    }

    fn read_bits(&mut self, count: u8) -> Option<u16> {
        while self.bits_in_buffer < count && self.byte_pos < self.data.len() {
            self.buffer = (self.buffer << 8) | u32::from(self.data[self.byte_pos]);
            self.byte_pos += 1;
            self.bits_in_buffer += 8;
        }
        if self.bits_in_buffer < count {
            return None;
        }
        let shift = self.bits_in_buffer - count;
        let mask = (1u32 << count) - 1;
        let value = (self.buffer >> shift) & mask;
        self.bits_in_buffer -= count;
        Some(value as u16)
    }
}

struct LegacyDictionary {
    table: Vec<Vec<u8>>,
    next_code: u16,
    current_bits: u8,
}

impl LegacyDictionary {
    fn new() -> Self {
        let mut dict = Self {
            table: Vec::with_capacity(4096),
            next_code: 0,
            current_bits: 9,
        };
        dict.reset();
        dict
    }

    fn reset(&mut self) {
        self.table.clear();
        self.current_bits = 9;
        for byte in 0..256u16 {
            self.table.push(vec![byte as u8]);
        }
        self.table.push(Vec::new()); // ClearCode
        self.table.push(Vec::new()); // EOI
        self.next_code = 258;
    }

    fn add(&mut self, string: Vec<u8>) {
        if self.next_code > 4095 {
            return;
        }
        self.table.push(string);
        self.next_code += 1;
        if self.current_bits < 12 && self.next_code >= (1 << self.current_bits) - 1 {
            self.current_bits += 1;
        }
    }

    fn get(&self, code: u16) -> Option<&[u8]> {
        self.table.get(code as usize).map(|entry| entry.as_slice())
    }

    fn is_full(&self) -> bool {
        self.next_code > 4095
    }
}

/// The pre-0.4.2 decode loop. `None` stands for any error.
fn legacy_decode(input: &[u8], expected_size: usize) -> Option<Vec<u8>> {
    let mut dict = LegacyDictionary::new();
    let mut reader = LegacyBitReader::new(input);
    let mut output = Vec::new();
    let mut prev_code: Option<u16> = None;

    while output.len() < expected_size {
        let code = reader.read_bits(dict.current_bits)?;
        if code == 256 {
            dict.reset();
            prev_code = None;
            continue;
        }
        if code == 257 {
            break;
        }

        let string = if code < dict.next_code {
            dict.get(code)?.to_vec()
        } else if code == dict.next_code {
            let prev = prev_code?;
            let previous = dict.get(prev)?;
            let mut grown = previous.to_vec();
            grown.push(*previous.first()?);
            grown
        } else {
            return None;
        };

        output.extend_from_slice(&string);

        if let Some(prev) = prev_code {
            if !dict.is_full() {
                let previous = dict.get(prev)?;
                let mut entry = previous.to_vec();
                entry.push(*string.first()?);
                dict.add(entry);
            }
        }

        prev_code = Some(code);
    }

    output.truncate(expected_size);
    Some(output)
}

// ---------------------------------------------------------------------------
// Differential driver
// ---------------------------------------------------------------------------

/// Compare all three decode paths on one (input, limit) pair.
fn compare(label: &str, input: &[u8], limit: usize) {
    let reference = legacy_decode(input, limit);

    let via_vec = decompress_tiff(input, limit);
    let mut buffer = vec![0xA5u8; limit + 8];
    let via_into = decompress_tiff_into(input, &mut buffer[..limit]);

    match (&reference, &via_vec) {
        (Some(expected), Ok(actual)) => assert_eq!(
            expected, actual,
            "[{label}] limit {limit}: Vec path disagrees with the pre-0.4.2 decoder"
        ),
        (None, Err(_)) => {}
        (Some(expected), Err(err)) => panic!(
            "[{label}] limit {limit}: pre-0.4.2 decoded {} bytes, 0.4.2 failed: {err}",
            expected.len()
        ),
        (None, Ok(actual)) => panic!(
            "[{label}] limit {limit}: pre-0.4.2 rejected the stream, 0.4.2 produced {} bytes",
            actual.len()
        ),
    }

    // The slice path must agree with the Vec path, byte for byte, and must
    // never write outside the slice it was given.
    match (&via_vec, &via_into) {
        (Ok(expected), Ok(written)) => {
            assert_eq!(
                *written,
                expected.len(),
                "[{label}] limit {limit}: `_into` wrote {written} bytes, Vec produced {}",
                expected.len()
            );
            assert_eq!(
                &buffer[..*written],
                &expected[..],
                "[{label}] limit {limit}: `_into` and Vec disagree"
            );
        }
        (Err(_), Err(_)) => {}
        (vec_result, into_result) => panic!(
            "[{label}] limit {limit}: entry points disagree on success: \
             Vec = {vec_result:?}, into = {into_result:?}"
        ),
    }
    assert!(
        buffer[limit..].iter().all(|&byte| byte == 0xA5),
        "[{label}] limit {limit}: `_into` wrote past the end of its buffer"
    );
}

fn xorshift(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn arbitrary_bytes_decode_identically() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    for case in 0..400 {
        let len = (xorshift(&mut seed) % 600) as usize + 1;
        let mut input = Vec::with_capacity(len);
        while input.len() < len {
            input.extend_from_slice(&xorshift(&mut seed).to_le_bytes());
        }
        input.truncate(len);
        for limit in [0usize, 1, 7, 64, 300, 4096] {
            compare(&format!("random-{case}"), &input, limit);
        }
    }
}

#[test]
fn streams_with_a_high_clear_code_density_decode_identically() {
    // Bytes biased toward 0x80/0x00 produce many ClearCode (256) and
    // low-code sequences, which exercises table resets and the KwKwK path
    // far more often than uniform random bytes do.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    for case in 0..200 {
        let len = (xorshift(&mut seed) % 900) as usize + 8;
        let mut input = Vec::with_capacity(len);
        while input.len() < len {
            for byte in xorshift(&mut seed).to_le_bytes() {
                input.push(match byte % 4 {
                    0 => 0x80,
                    1 => 0x00,
                    2 => 0x01,
                    _ => byte,
                });
            }
        }
        input.truncate(len);
        for limit in [1usize, 33, 512, 5000] {
            compare(&format!("clear-heavy-{case}"), &input, limit);
        }
    }
}

#[test]
fn corrupted_and_truncated_real_streams_decode_identically() {
    // Real streams this crate produced, then damaged: the fast paths only
    // trigger on long strings, which random bytes rarely build.
    let payloads: Vec<Vec<u8>> = vec![
        b"The quick brown fox jumps over the lazy dog. ".repeat(120),
        vec![b'Q'; 20_000],
        (0..30_000u32).map(|index| (index % 7) as u8).collect(),
        (0..12_000u32).map(|index| (index % 251) as u8).collect(),
        b"ABAB".repeat(4000),
    ];

    for (index, payload) in payloads.iter().enumerate() {
        let stream = oxiarc_lzw::compress_tiff(payload).expect("encode");

        // Undamaged, at several limits including past the end.
        for limit in [0, 1, payload.len() / 2, payload.len(), payload.len() + 64] {
            compare(&format!("clean-{index}"), &stream, limit);
        }

        // Truncations.
        let stride = (stream.len() / 60).max(1);
        for cut in (1..stream.len()).step_by(stride) {
            compare(&format!("cut-{index}-{cut}"), &stream[..cut], payload.len());
        }

        // Single-bit and byte corruptions.
        let stride = (stream.len() / 40).max(1);
        for position in (0..stream.len()).step_by(stride) {
            for mask in [0x01u8, 0x10, 0x80, 0xFF] {
                let mut corrupted = stream.clone();
                corrupted[position] ^= mask;
                compare(
                    &format!("corrupt-{index}-{position}-{mask:02x}"),
                    &corrupted,
                    payload.len(),
                );
            }
        }
    }
}

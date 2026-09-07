//! Non-last `.xz` block filters: Delta and the branch/call/jump (BCJ)
//! converters.
//!
//! An `.xz` block header carries a chain of one to four filters, listed in
//! the order an encoder applied them; the last one is the compression
//! filter (LZMA2 here). Decoding therefore runs the compression filter
//! first and then undoes the preceding filters **in reverse order**.
//!
//! Before this module existed, the reader parsed the filter list only to
//! find LZMA2's dictionary-size property and silently ignored every other
//! filter — which produced *silently wrong output* for any stream that used
//! one. That is not hypothetical: libtiff writes TIFF `Compression = 34925`
//! strips as `Delta(dist=1) + LZMA2`, so every LZMA-compressed TIFF decoded
//! to horizontally-differenced pixels. Unknown or unsupported filter IDs are
//! now a hard error instead.
//!
//! # Filters implemented
//!
//! | ID | Filter | Status |
//! |---|---|---|
//! | 0x03 | Delta | decode + encode |
//! | 0x04 | BCJ x86 | decode + encode |
//! | 0x05 | BCJ PowerPC (big endian) | decode + encode |
//! | 0x06 | BCJ IA-64 | decode + encode |
//! | 0x07 | BCJ ARM | decode + encode |
//! | 0x08 | BCJ ARM-Thumb | decode + encode |
//! | 0x09 | BCJ SPARC | decode + encode |
//! | 0x0A | BCJ ARM64 | decode + encode |
//! | 0x0B | BCJ RISC-V | rejected with a named error (see below) |
//!
//! The RISC-V converter (added in XZ Utils 5.6) is deliberately **not**
//! guessed at: it is the one filter whose exact transform this crate cannot
//! reproduce and verify against a reference, and a filter that is *almost*
//! right corrupts data silently. A stream that uses it is rejected with
//! [`OxiArcError::UnsupportedMethod`] naming the filter, which a caller can
//! act on; no oxiarc encoder ever emits it, and it never appears in any of
//! the container formats oxiarc reads.
//!
//! Every implemented converter is validated against the real `xz` CLI in
//! `tests/xz_module.rs` (feature `xz-oracle`) and round-tripped hermetically
//! in this module's unit tests.

use oxiarc_core::error::{OxiArcError, Result};

/// Delta filter ID.
pub(crate) const FILTER_DELTA: u64 = 0x03;
/// BCJ x86 filter ID.
pub(crate) const FILTER_BCJ_X86: u64 = 0x04;
/// BCJ PowerPC filter ID.
pub(crate) const FILTER_BCJ_POWERPC: u64 = 0x05;
/// BCJ IA-64 filter ID.
pub(crate) const FILTER_BCJ_IA64: u64 = 0x06;
/// BCJ ARM filter ID.
pub(crate) const FILTER_BCJ_ARM: u64 = 0x07;
/// BCJ ARM-Thumb filter ID.
pub(crate) const FILTER_BCJ_ARMTHUMB: u64 = 0x08;
/// BCJ SPARC filter ID.
pub(crate) const FILTER_BCJ_SPARC: u64 = 0x09;
/// BCJ ARM64 filter ID.
pub(crate) const FILTER_BCJ_ARM64: u64 = 0x0A;
/// BCJ RISC-V filter ID (recognised, not implemented — see the module docs).
pub(crate) const FILTER_BCJ_RISCV: u64 = 0x0B;

/// A parsed non-last filter from a block header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum XzFilter {
    /// Delta with a distance of 1..=256 bytes.
    Delta {
        /// Byte distance (already decoded from `props + 1`).
        distance: usize,
    },
    /// A branch/call/jump converter with its start offset.
    Bcj {
        /// Filter ID (one of the `FILTER_BCJ_*` constants).
        id: u64,
        /// Start offset from the filter properties (0 when absent).
        start_offset: u32,
    },
}

impl XzFilter {
    /// Parse a non-last filter from its ID and property bytes.
    ///
    /// # Errors
    ///
    /// [`OxiArcError::UnsupportedMethod`] for a filter this crate does not
    /// implement (including RISC-V BCJ and any unknown ID), and
    /// [`OxiArcError::CorruptedData`] for a malformed property field.
    pub(crate) fn parse(id: u64, props: &[u8]) -> Result<Self> {
        match id {
            FILTER_DELTA => {
                // xz spec 5.3.2: exactly one property byte, distance - 1.
                let [byte] = props else {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!(
                            "XZ Delta filter has {} property bytes (exactly 1 required)",
                            props.len()
                        ),
                    ));
                };
                Ok(Self::Delta {
                    distance: usize::from(*byte) + 1,
                })
            }
            FILTER_BCJ_X86 | FILTER_BCJ_POWERPC | FILTER_BCJ_IA64 | FILTER_BCJ_ARM
            | FILTER_BCJ_ARMTHUMB | FILTER_BCJ_SPARC | FILTER_BCJ_ARM64 => {
                // xz spec 5.3.3: either no properties, or a 4-byte
                // little-endian start offset.
                let start_offset = match props {
                    [] => 0,
                    [a, b, c, d] => u32::from_le_bytes([*a, *b, *c, *d]),
                    _ => {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!(
                                "XZ BCJ filter 0x{id:02X} has {} property bytes (0 or 4 required)",
                                props.len()
                            ),
                        ));
                    }
                };
                let alignment = bcj_alignment(id);
                if start_offset % alignment != 0 {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!(
                            "XZ BCJ filter 0x{id:02X} start offset {start_offset} is not a \
                             multiple of its {alignment}-byte alignment"
                        ),
                    ));
                }
                Ok(Self::Bcj { id, start_offset })
            }
            FILTER_BCJ_RISCV => Err(OxiArcError::UnsupportedMethod {
                method: "XZ BCJ RISC-V filter (0x0B)".to_string(),
            }),
            other => Err(OxiArcError::UnsupportedMethod {
                method: format!("XZ filter 0x{other:02X}"),
            }),
        }
    }

    /// Undo this filter over a whole block's decompressed data, in place.
    pub(crate) fn decode(self, data: &mut [u8]) {
        match self {
            Self::Delta { distance } => delta_decode(data, distance),
            Self::Bcj { id, start_offset } => bcj_code(id, start_offset, false, data),
        }
    }

    /// Apply this filter over a whole block, in place (test-only: the
    /// writer emits plain LZMA2, so this exists to prove every converter
    /// round-trips).
    #[cfg(test)]
    pub(crate) fn encode(self, data: &mut [u8]) {
        match self {
            Self::Delta { distance } => delta_encode(data, distance),
            Self::Bcj { id, start_offset } => bcj_code(id, start_offset, true, data),
        }
    }
}

/// Instruction alignment of a BCJ filter, which its start offset must
/// respect (xz spec 5.3.3).
fn bcj_alignment(id: u64) -> u32 {
    match id {
        FILTER_BCJ_POWERPC | FILTER_BCJ_ARM | FILTER_BCJ_SPARC | FILTER_BCJ_ARM64 => 4,
        FILTER_BCJ_ARMTHUMB => 2,
        FILTER_BCJ_IA64 => 16,
        // x86 (and anything else that reaches here) has no alignment rule.
        _ => 1,
    }
}

// ---------------------------------------------------------------------------
// Delta
// ---------------------------------------------------------------------------

/// Undo delta encoding: `out[i] = in[i] + out[i - distance]`, with an
/// all-zero history before the start of the buffer.
fn delta_decode(data: &mut [u8], distance: usize) {
    if distance == 0 {
        return;
    }
    for i in distance..data.len() {
        data[i] = data[i].wrapping_add(data[i - distance]);
    }
}

/// Apply delta encoding: `out[i] = in[i] - in[i - distance]`.
#[cfg(test)]
fn delta_encode(data: &mut [u8], distance: usize) {
    if distance == 0 {
        return;
    }
    for i in (distance..data.len()).rev() {
        data[i] = data[i].wrapping_sub(data[i - distance]);
    }
}

// ---------------------------------------------------------------------------
// BCJ dispatch
// ---------------------------------------------------------------------------

/// Run a branch converter over `data`. `now_pos` is the filter's start
/// offset; `is_encoder` selects the direction.
fn bcj_code(id: u64, now_pos: u32, is_encoder: bool, data: &mut [u8]) {
    match id {
        FILTER_BCJ_X86 => x86_code(now_pos, is_encoder, data),
        FILTER_BCJ_POWERPC => powerpc_code(now_pos, is_encoder, data),
        FILTER_BCJ_IA64 => ia64_code(now_pos, is_encoder, data),
        FILTER_BCJ_ARM => arm_code(now_pos, is_encoder, data),
        FILTER_BCJ_ARMTHUMB => armthumb_code(now_pos, is_encoder, data),
        FILTER_BCJ_SPARC => sparc_code(now_pos, is_encoder, data),
        FILTER_BCJ_ARM64 => arm64_code(now_pos, is_encoder, data),
        // Unreachable: `XzFilter::parse` rejects every other ID.
        _ => {}
    }
}

/// `true` for the two byte values an x86 relative-call target may start
/// with once converted (liblzma's `Test86MSByte`).
#[inline]
fn test86_ms_byte(byte: u8) -> bool {
    byte == 0x00 || byte == 0xFF
}

/// x86 CALL/JMP (0xE8/0xE9) relative-to-absolute converter.
fn x86_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    const MASK_TO_ALLOWED_STATUS: [bool; 8] = [true, true, true, false, true, false, false, false];
    const MASK_TO_BIT_NUMBER: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];

    if buffer.len() < 5 {
        return;
    }

    let mut prev_mask: u32 = 0;
    // liblzma's `lzma_simple_x86_*_init` seeds `prev_pos` with
    // `(uint32_t)(-5)`, and `x86_code` then normalises it with
    // `if (now_pos - prev_pos > 5) prev_pos = now_pos - 5;`. Both reduce to
    // `now_pos - 5` here, since a whole block is filtered in one call.
    let mut prev_pos: u32 = now_pos.wrapping_sub(5);
    let limit = buffer.len() - 5;
    let mut pos = 0usize;

    while pos <= limit {
        let byte = buffer[pos];
        if byte != 0xE8 && byte != 0xE9 {
            pos += 1;
            continue;
        }

        let current = now_pos.wrapping_add(pos as u32);
        let offset = current.wrapping_sub(prev_pos);
        prev_pos = current;

        if offset > 5 {
            prev_mask = 0;
        } else {
            for _ in 0..offset {
                prev_mask &= 0x77;
                prev_mask <<= 1;
            }
        }

        let high = buffer[pos + 4];
        if test86_ms_byte(high)
            && MASK_TO_ALLOWED_STATUS[((prev_mask >> 1) & 0x7) as usize]
            && (prev_mask >> 1) < 0x10
        {
            let mut src = (u32::from(high) << 24)
                | (u32::from(buffer[pos + 3]) << 16)
                | (u32::from(buffer[pos + 2]) << 8)
                | u32::from(buffer[pos + 1]);

            let dest;
            loop {
                let delta = current.wrapping_add(5);
                let candidate = if is_encoder {
                    src.wrapping_add(delta)
                } else {
                    src.wrapping_sub(delta)
                };

                if prev_mask == 0 {
                    dest = candidate;
                    break;
                }

                // `prev_mask` is always even here (the shift loop above
                // runs at least once, because two matches can never share a
                // position) and `prev_mask &= 0x77` clears bit 3 before
                // every shift, so bit 4 can never be set at this point and
                // `prev_mask >> 1` stays below 8. liblzma indexes this
                // table exactly the same way; the invariant is exercised by
                // `bcj_converters_never_panic_on_hostile_bytes`.
                let i = MASK_TO_BIT_NUMBER[(prev_mask >> 1) as usize];
                let b = (candidate >> (24 - i * 8)) as u8;
                if !test86_ms_byte(b) {
                    dest = candidate;
                    break;
                }

                src = candidate ^ ((1u32 << (32 - i * 8)) - 1);
            }

            buffer[pos + 4] = (((dest >> 24) & 1).wrapping_sub(1) ^ 0xFFFF_FFFF) as u8;
            buffer[pos + 3] = (dest >> 16) as u8;
            buffer[pos + 2] = (dest >> 8) as u8;
            buffer[pos + 1] = dest as u8;
            pos += 5;
            prev_mask = 0;
        } else {
            prev_mask |= 0x01;
            if test86_ms_byte(high) {
                prev_mask |= 0x10;
            }
            pos += 1;
        }
    }
}

/// PowerPC big-endian `bl` converter.
fn powerpc_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    let mut i = 0usize;
    while i + 4 <= buffer.len() {
        if (buffer[i] & 0xFC) == 0x48 && (buffer[i + 3] & 0x03) == 1 {
            let src = ((u32::from(buffer[i]) & 3) << 24)
                | (u32::from(buffer[i + 1]) << 16)
                | (u32::from(buffer[i + 2]) << 8)
                | (u32::from(buffer[i + 3]) & !3u32);

            let pc = now_pos.wrapping_add(i as u32);
            let dest = if is_encoder {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            };

            buffer[i] = 0x48 | ((dest >> 24) & 0x03) as u8;
            buffer[i + 1] = (dest >> 16) as u8;
            buffer[i + 2] = (dest >> 8) as u8;
            buffer[i + 3] = (buffer[i + 3] & 0x03) | (dest as u8 & !3u8);
        }
        i += 4;
    }
}

/// ARM (A32) `bl` converter.
fn arm_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    let mut i = 0usize;
    while i + 4 <= buffer.len() {
        if buffer[i + 3] == 0xEB {
            let src = ((u32::from(buffer[i + 2]) << 16)
                | (u32::from(buffer[i + 1]) << 8)
                | u32::from(buffer[i]))
                << 2;

            let pc = now_pos.wrapping_add(i as u32).wrapping_add(8);
            let dest = if is_encoder {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            } >> 2;

            buffer[i + 2] = (dest >> 16) as u8;
            buffer[i + 1] = (dest >> 8) as u8;
            buffer[i] = dest as u8;
        }
        i += 4;
    }
}

/// ARM-Thumb (T32) `bl` converter.
fn armthumb_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    let mut i = 0usize;
    while i + 4 <= buffer.len() {
        if (buffer[i + 1] & 0xF8) == 0xF0 && (buffer[i + 3] & 0xF8) == 0xF8 {
            let src = (((u32::from(buffer[i + 1]) & 7) << 19)
                | (u32::from(buffer[i]) << 11)
                | ((u32::from(buffer[i + 3]) & 7) << 8)
                | u32::from(buffer[i + 2]))
                << 1;

            let pc = now_pos.wrapping_add(i as u32).wrapping_add(4);
            let dest = if is_encoder {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            } >> 1;

            buffer[i + 1] = 0xF0 | ((dest >> 19) & 0x7) as u8;
            buffer[i] = (dest >> 11) as u8;
            buffer[i + 3] = 0xF8 | ((dest >> 8) & 0x7) as u8;
            buffer[i + 2] = dest as u8;
            i += 2;
        }
        i += 2;
    }
}

/// SPARC `call` converter.
fn sparc_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    let mut i = 0usize;
    while i + 4 <= buffer.len() {
        let matches = (buffer[i] == 0x40 && (buffer[i + 1] & 0xC0) == 0x00)
            || (buffer[i] == 0x7F && (buffer[i + 1] & 0xC0) == 0xC0);
        if matches {
            let src = ((u32::from(buffer[i]) << 24)
                | (u32::from(buffer[i + 1]) << 16)
                | (u32::from(buffer[i + 2]) << 8)
                | u32::from(buffer[i + 3]))
                << 2;

            let pc = now_pos.wrapping_add(i as u32);
            let mut dest = if is_encoder {
                pc.wrapping_add(src)
            } else {
                src.wrapping_sub(pc)
            } >> 2;

            dest =
                (0x4000_0000u32.wrapping_sub(dest & 0x40_0000)) | 0x4000_0000 | (dest & 0x3F_FFFF);

            buffer[i] = (dest >> 24) as u8;
            buffer[i + 1] = (dest >> 16) as u8;
            buffer[i + 2] = (dest >> 8) as u8;
            buffer[i + 3] = dest as u8;
        }
        i += 4;
    }
}

/// ARM64 `bl` / `adrp` converter.
fn arm64_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    let mut i = 0usize;
    while i + 4 <= buffer.len() {
        let mut instr =
            u32::from_le_bytes([buffer[i], buffer[i + 1], buffer[i + 2], buffer[i + 3]]);
        let mut pc = now_pos.wrapping_add(i as u32);

        if (instr >> 26) == 0x25 {
            // BL: 26-bit word-scaled immediate.
            let src = instr;
            instr = 0x9400_0000;
            pc >>= 2;
            if !is_encoder {
                pc = 0u32.wrapping_sub(pc);
            }
            instr |= src.wrapping_add(pc) & 0x03FF_FFFF;
            buffer[i..i + 4].copy_from_slice(&instr.to_le_bytes());
        } else if (instr & 0x9F00_0000) == 0x9000_0000 {
            // ADRP: only values within +/-512 MiB are converted.
            let src = ((instr >> 29) & 3) | ((instr >> 3) & 0x001F_FFFC);
            if (src.wrapping_add(0x0002_0000) & 0x001C_0000) != 0 {
                i += 4;
                continue;
            }
            instr &= 0x9000_001F;
            pc >>= 12;
            if !is_encoder {
                pc = 0u32.wrapping_sub(pc);
            }
            let dest = src.wrapping_add(pc);
            instr |= (dest & 3) << 29;
            instr |= (dest & 0x0003_FFFC) << 3;
            instr |= 0u32.wrapping_sub(dest & 0x0002_0000) & 0x00E0_0000;
            buffer[i..i + 4].copy_from_slice(&instr.to_le_bytes());
        }
        i += 4;
    }
}

/// IA-64 (Itanium) bundle converter.
fn ia64_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    const BRANCH_TABLE: [u32; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4,
        0, 0,
    ];

    let mut i = 0usize;
    while i + 16 <= buffer.len() {
        let template = u32::from(buffer[i] & 0x1F);
        let mask = BRANCH_TABLE[template as usize];

        let mut slot = 0u32;
        let mut bit_pos = 5u32;
        while slot < 3 {
            if ((mask >> slot) & 1) != 0 {
                let byte_pos = (bit_pos >> 3) as usize;
                let bit_res = bit_pos & 7;

                let mut instruction: u64 = 0;
                for j in 0..6usize {
                    instruction += u64::from(buffer[i + j + byte_pos]) << (8 * j);
                }

                let mut inst_norm = instruction >> bit_res;
                // liblzma's `ia64.c`: opcode 5 (IP-relative branch) with
                // an all-zero 3-bit field at bits 9..11. Verified against
                // liblzma via `tests/xz_module.rs`.
                let is_branch = ((inst_norm >> 37) & 0xF) == 0x5 && ((inst_norm >> 9) & 0x7) == 0;

                if is_branch {
                    let mut src = ((inst_norm >> 13) & 0x000F_FFFF) as u32;
                    src |= (((inst_norm >> 36) & 1) as u32) << 20;
                    src <<= 4;

                    let pc = now_pos.wrapping_add(i as u32);
                    let dest = if is_encoder {
                        pc.wrapping_add(src)
                    } else {
                        src.wrapping_sub(pc)
                    } >> 4;

                    inst_norm &= !(0x008F_FFFFu64 << 13);
                    inst_norm |= u64::from(dest & 0x000F_FFFF) << 13;
                    inst_norm |= u64::from(dest & 0x0010_0000) << (36 - 20);

                    instruction &= (1u64 << bit_res) - 1;
                    instruction |= inst_norm << bit_res;

                    for j in 0..6usize {
                        buffer[i + j + byte_pos] = (instruction >> (8 * j)) as u8;
                    }
                }
            }
            slot += 1;
            bit_pos += 41;
        }
        i += 16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-instruction stream with many filter-matching
    /// patterns, so the converters actually fire.
    fn pattern_bytes(len: usize, marker: &[u8]) -> Vec<u8> {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut out = Vec::with_capacity(len + 16);
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            out.extend_from_slice(marker);
            out.extend_from_slice(&seed.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    fn all_filters() -> Vec<XzFilter> {
        let mut filters = vec![
            XzFilter::Delta { distance: 1 },
            XzFilter::Delta { distance: 4 },
            XzFilter::Delta { distance: 256 },
        ];
        for id in [
            FILTER_BCJ_X86,
            FILTER_BCJ_POWERPC,
            FILTER_BCJ_IA64,
            FILTER_BCJ_ARM,
            FILTER_BCJ_ARMTHUMB,
            FILTER_BCJ_SPARC,
            FILTER_BCJ_ARM64,
        ] {
            filters.push(XzFilter::Bcj {
                id,
                start_offset: 0,
            });
        }
        filters
    }

    #[test]
    fn every_filter_round_trips() {
        let markers: [&[u8]; 6] = [
            &[0xE8, 0x00, 0x00, 0x00],
            &[0x48, 0x00, 0x00, 0x01],
            &[0x00, 0x00, 0x00, 0xEB],
            &[0x40, 0x00, 0x00, 0x00],
            &[0x00, 0xF0, 0x00, 0xF8],
            &[0x00, 0x00, 0x00, 0x94],
        ];
        for filter in all_filters() {
            for marker in markers {
                for len in [0usize, 1, 5, 16, 17, 64, 1024, 4099] {
                    let original = pattern_bytes(len, marker);
                    let mut buffer = original.clone();
                    filter.encode(&mut buffer);
                    filter.decode(&mut buffer);
                    assert_eq!(buffer, original, "{filter:?} len {len}");
                }
            }
        }
    }

    #[test]
    fn short_buffers_are_left_alone() {
        for filter in all_filters() {
            for len in 0..5usize {
                let original: Vec<u8> = (0..len as u8).collect();
                let mut buffer = original.clone();
                filter.decode(&mut buffer);
                if matches!(filter, XzFilter::Bcj { .. }) {
                    assert_eq!(buffer, original, "{filter:?} len {len}");
                }
            }
        }
    }

    #[test]
    fn parse_accepts_valid_properties() {
        assert_eq!(
            XzFilter::parse(FILTER_DELTA, &[0]).expect("delta dist 1"),
            XzFilter::Delta { distance: 1 }
        );
        assert_eq!(
            XzFilter::parse(FILTER_DELTA, &[255]).expect("delta dist 256"),
            XzFilter::Delta { distance: 256 }
        );
        assert_eq!(
            XzFilter::parse(FILTER_BCJ_X86, &[]).expect("x86 without props"),
            XzFilter::Bcj {
                id: FILTER_BCJ_X86,
                start_offset: 0
            }
        );
        assert_eq!(
            XzFilter::parse(FILTER_BCJ_ARM, &[8, 0, 0, 0]).expect("arm with start offset"),
            XzFilter::Bcj {
                id: FILTER_BCJ_ARM,
                start_offset: 8
            }
        );
    }

    #[test]
    fn parse_rejects_malformed_and_unsupported_filters() {
        assert!(XzFilter::parse(FILTER_DELTA, &[]).is_err());
        assert!(XzFilter::parse(FILTER_DELTA, &[0, 0]).is_err());
        assert!(XzFilter::parse(FILTER_BCJ_X86, &[0, 0]).is_err());
        // A misaligned start offset for a 4-byte-aligned architecture.
        assert!(XzFilter::parse(FILTER_BCJ_ARM, &[2, 0, 0, 0]).is_err());
        // RISC-V is recognised but explicitly unsupported.
        let err = XzFilter::parse(FILTER_BCJ_RISCV, &[]).expect_err("riscv rejected");
        assert!(err.to_string().contains("RISC-V"), "{err}");
        // Unknown IDs are rejected, never silently ignored.
        assert!(XzFilter::parse(0x1234, &[]).is_err());
    }

    // -----------------------------------------------------------------
    // liblzma cross-validation (feature `xz-oracle`)
    // -----------------------------------------------------------------

    /// Per-filter differential test against liblzma itself, through
    /// CPython's `lzma` module.
    ///
    /// The container-level oracle in `tests/xz_module.rs` proves the whole
    /// pipeline agrees with the `xz` CLI, but it cannot see the *filtered*
    /// intermediate, so a converter that never fires would pass it
    /// silently. This test extracts that intermediate — compress with
    /// `[filter, LZMA2]`, decompress with `[LZMA2]` alone — and compares it
    /// against [`XzFilter::encode`] byte for byte, then checks
    /// [`XzFilter::decode`] inverts it. It also asserts each converter
    /// actually transformed something, so the comparison is meaningful.
    ///
    /// Self-skips when `python3` (with `lzma`) is unavailable.
    /// `FILTER_ARM64` is not exposed by CPython's module; ARM64 is covered
    /// by the `xz` CLI test in `tests/xz_module.rs`.
    #[cfg(feature = "xz-oracle")]
    #[test]
    fn filters_match_liblzma_byte_for_byte() {
        use std::process::Command;

        const PY: &str = r#"
import base64, os, sys, lzma

FILTERS = {
    "delta1": [{"id": lzma.FILTER_DELTA, "dist": 1}],
    "delta4": [{"id": lzma.FILTER_DELTA, "dist": 4}],
    "delta256": [{"id": lzma.FILTER_DELTA, "dist": 256}],
    "x86": [{"id": lzma.FILTER_X86}],
    "powerpc": [{"id": lzma.FILTER_POWERPC}],
    "ia64": [{"id": lzma.FILTER_IA64}],
    "arm": [{"id": lzma.FILTER_ARM}],
    "armthumb": [{"id": lzma.FILTER_ARMTHUMB}],
    "sparc": [{"id": lzma.FILTER_SPARC}],
}
LZMA2 = {"id": lzma.FILTER_LZMA2, "preset": 1}

name = sys.argv[1]
payload = base64.b64decode(sys.stdin.read())
chain = FILTERS[name] + [LZMA2]
packed = lzma.compress(payload, format=lzma.FORMAT_RAW, filters=chain)
filtered = lzma.decompress(packed, format=lzma.FORMAT_RAW, filters=[LZMA2])
sys.stdout.write(base64.b64encode(filtered).decode())
"#;

        let python_ok = Command::new("python3")
            .args(["-c", "import lzma, base64"])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !python_ok {
            eprintln!("[xz-oracle] python3 with `lzma` unavailable; skipping (self-skip)");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "oxiarc_xz_filters_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let script = dir.join("filters.py");
        std::fs::write(&script, PY).expect("write script");

        // Payloads dense in each architecture's branch encodings, plus a
        // pseudo-random one, so every converter has something to convert.
        let markers: [&[u8]; 8] = [
            &[0xE8, 0x00, 0x00, 0x00],
            &[0xE9, 0xFF, 0xFF, 0xFF],
            &[0x48, 0x00, 0x10, 0x01],
            &[0x10, 0x20, 0x30, 0xEB],
            &[0x11, 0xF0, 0x22, 0xF8],
            &[0x40, 0x00, 0x12, 0x34],
            &[0x16, 0x00, 0x00, 0x00],
            &[0x7F, 0xC0, 0x00, 0x00],
        ];
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        for marker in markers {
            let mut data = Vec::with_capacity(32 * 1024);
            let mut seed = 0x2545_F491_4F6C_DD1Du64;
            while data.len() < 32 * 1024 {
                data.extend_from_slice(marker);
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                data.extend_from_slice(&seed.to_le_bytes());
                data.extend_from_slice(marker);
            }
            data.truncate(32 * 1024);
            payloads.push(data);
        }
        // Pure noise: exercises the "must not fire" side of every rule.
        let mut noise = Vec::with_capacity(32 * 1024);
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        while noise.len() < 32 * 1024 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            noise.extend_from_slice(&seed.to_le_bytes());
        }
        payloads.push(noise);

        let cases: [(&str, XzFilter); 9] = [
            ("delta1", XzFilter::Delta { distance: 1 }),
            ("delta4", XzFilter::Delta { distance: 4 }),
            ("delta256", XzFilter::Delta { distance: 256 }),
            (
                "x86",
                XzFilter::Bcj {
                    id: FILTER_BCJ_X86,
                    start_offset: 0,
                },
            ),
            (
                "powerpc",
                XzFilter::Bcj {
                    id: FILTER_BCJ_POWERPC,
                    start_offset: 0,
                },
            ),
            (
                "ia64",
                XzFilter::Bcj {
                    id: FILTER_BCJ_IA64,
                    start_offset: 0,
                },
            ),
            (
                "arm",
                XzFilter::Bcj {
                    id: FILTER_BCJ_ARM,
                    start_offset: 0,
                },
            ),
            (
                "armthumb",
                XzFilter::Bcj {
                    id: FILTER_BCJ_ARMTHUMB,
                    start_offset: 0,
                },
            ),
            (
                "sparc",
                XzFilter::Bcj {
                    id: FILTER_BCJ_SPARC,
                    start_offset: 0,
                },
            ),
        ];

        let mut checked = 0usize;
        for (name, filter) in cases {
            let mut fired = false;
            for payload in &payloads {
                let reference = run_python(&script, name, payload);
                assert_eq!(reference.len(), payload.len(), "[{name}] length changed");

                let mut mine = payload.clone();
                filter.encode(&mut mine);
                assert_eq!(mine, reference, "[{name}] encode differs from liblzma");

                filter.decode(&mut mine);
                assert_eq!(&mine, payload, "[{name}] decode does not invert encode");

                if reference != *payload {
                    fired = true;
                }
                checked += 1;
            }
            assert!(
                fired,
                "[{name}] the converter never fired on any payload; the comparison would be vacuous"
            );
        }

        eprintln!("[xz-oracle] {checked} filter/payload pairs match liblzma byte for byte");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run the liblzma driver and return the filtered intermediate.
    #[cfg(feature = "xz-oracle")]
    fn run_python(script: &std::path::Path, name: &str, payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut child = Command::new("python3")
            .arg(script)
            .arg(name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn python3");
        let encoded = base64_encode(payload);
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(encoded.as_bytes())
            .expect("write payload");
        let output = child.wait_with_output().expect("wait for python3");
        assert!(
            output.status.success(),
            "python driver failed for {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        base64_decode(&output.stdout).expect("decode python output")
    }

    /// Minimal base64 (the test pipes binary through a text stream; adding
    /// a dependency for this would violate the workspace's dependency
    /// policy for a four-line helper).
    #[cfg(feature = "xz-oracle")]
    fn base64_encode(data: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for chunk in data.chunks(3) {
            let b0 = u32::from(chunk[0]);
            let b1 = chunk.get(1).copied().map_or(0, u32::from);
            let b2 = chunk.get(2).copied().map_or(0, u32::from);
            let triple = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPHABET[(triple >> 18) as usize & 63] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(triple >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[triple as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// Inverse of [`base64_encode`]; `None` on malformed input.
    #[cfg(feature = "xz-oracle")]
    fn base64_decode(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() / 4 * 3);
        let mut accumulator = 0u32;
        let mut bits = 0u32;
        for &byte in data {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' | b'\n' | b'\r' => continue,
                _ => return None,
            };
            accumulator = (accumulator << 6) | u32::from(value);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((accumulator >> bits) as u8);
            }
        }
        Some(out)
    }

    /// Second, independent per-filter oracle: the `xz` CLI's `--format=raw`
    /// mode, which can encode with `[filter, LZMA2]` and then decode with
    /// `[LZMA2]` alone, exposing the filtered intermediate.
    ///
    /// This covers **ARM64**, which CPython's `lzma` module does not expose,
    /// and cross-checks every other converter through a second code path.
    /// Each case asserts the converter actually fired, so a no-op
    /// implementation cannot pass.
    #[cfg(feature = "xz-oracle")]
    #[test]
    fn filters_match_the_xz_cli_byte_for_byte() {
        use std::process::Command;

        let xz_ok = Command::new("xz")
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if !xz_ok {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip)");
            return;
        }

        let dir = std::env::temp_dir().join(format!(
            "oxiarc_xz_filters_cli_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        // Every architecture's branch encoding, at high density.
        let mut payload = Vec::with_capacity(64 * 1024);
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let markers: [[u8; 4]; 9] = [
            [0xE8, 0x00, 0x00, 0x00],
            [0xE9, 0xFF, 0xFF, 0xFF],
            [0x48, 0x00, 0x10, 0x01],
            [0x10, 0x20, 0x30, 0xEB],
            [0x11, 0xF0, 0x22, 0xF8],
            [0x40, 0x00, 0x12, 0x34],
            [0x11, 0x22, 0x33, 0x94],
            [0x11, 0x22, 0x33, 0x90],
            [0x16, 0x00, 0x00, 0x00],
        ];
        while payload.len() < 64 * 1024 {
            for marker in markers {
                payload.extend_from_slice(&marker);
            }
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            payload.extend_from_slice(&seed.to_le_bytes());
        }
        payload.truncate(64 * 1024);

        let raw = dir.join("payload.bin");
        std::fs::write(&raw, &payload).expect("write payload");

        let cases: [(&str, XzFilter); 10] = [
            ("--delta=dist=1", XzFilter::Delta { distance: 1 }),
            ("--delta=dist=4", XzFilter::Delta { distance: 4 }),
            ("--delta=dist=256", XzFilter::Delta { distance: 256 }),
            ("--x86", bcj(FILTER_BCJ_X86)),
            ("--powerpc", bcj(FILTER_BCJ_POWERPC)),
            ("--ia64", bcj(FILTER_BCJ_IA64)),
            ("--arm", bcj(FILTER_BCJ_ARM)),
            ("--armthumb", bcj(FILTER_BCJ_ARMTHUMB)),
            ("--sparc", bcj(FILTER_BCJ_SPARC)),
            ("--arm64", bcj(FILTER_BCJ_ARM64)),
        ];

        let mut checked = 0usize;
        for (flag, filter) in cases {
            let packed = Command::new("xz")
                .args(["--format=raw", "-T1", "-c", flag, "--lzma2=preset=1"])
                .arg(&raw)
                .output()
                .expect("spawn xz encode");
            assert!(
                packed.status.success(),
                "xz {flag} failed: {}",
                String::from_utf8_lossy(&packed.stderr)
            );
            let packed_path = dir.join("packed.raw");
            std::fs::write(&packed_path, &packed.stdout).expect("write packed");

            // Decode the compression filter only: what comes out is the
            // filtered intermediate.
            let filtered = Command::new("xz")
                .args(["-d", "--format=raw", "-T1", "-c", "--lzma2=preset=1"])
                .arg(&packed_path)
                .output()
                .expect("spawn xz decode");
            assert!(
                filtered.status.success(),
                "xz -d {flag} failed: {}",
                String::from_utf8_lossy(&filtered.stderr)
            );
            let reference = filtered.stdout;
            assert_eq!(reference.len(), payload.len(), "[{flag}] length changed");
            assert_ne!(
                reference, payload,
                "[{flag}] the converter never fired; the comparison would be vacuous"
            );

            let mut mine = payload.clone();
            filter.encode(&mut mine);
            assert_eq!(mine, reference, "[{flag}] encode differs from the xz CLI");

            filter.decode(&mut mine);
            assert_eq!(mine, payload, "[{flag}] decode does not invert encode");
            checked += 1;
        }

        eprintln!(
            "[xz-oracle] {checked} filters match the `xz` CLI byte for byte (ARM64 included)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shorthand for a BCJ filter with no start offset.
    #[cfg(feature = "xz-oracle")]
    fn bcj(id: u64) -> XzFilter {
        XzFilter::Bcj {
            id,
            start_offset: 0,
        }
    }

    #[test]
    fn delta_matches_the_textbook_definition() {
        let original: Vec<u8> = (0..64u8).collect();
        let mut encoded = original.clone();
        delta_encode(&mut encoded, 1);
        assert_eq!(encoded[0], 0);
        assert!(encoded[1..].iter().all(|&b| b == 1));
        delta_decode(&mut encoded, 1);
        assert_eq!(encoded, original);
    }

    // -----------------------------------------------------------------
    // Start offsets (`start_offset` property of every BCJ filter)
    // -----------------------------------------------------------------

    /// Every BCJ converter's arithmetic is `now_pos`-relative and wraps:
    /// x86 seeds `prev_pos` with `now_pos - 5`, and each converter folds
    /// `now_pos + i` into the branch target. All of that is exercised only
    /// when `start_offset != 0`, which every other test in this crate
    /// leaves at zero. A crafted block header may set it to any aligned
    /// 32-bit value, so the wrap-around cases are reachable from untrusted
    /// input; an index or arithmetic panic there would be a decode-time
    /// denial of service.
    fn start_offsets_for(id: u64) -> Vec<u32> {
        let alignment = bcj_alignment(id);
        let mut offsets = vec![0u32];
        for candidate in [
            1u32,
            5,
            16,
            4096,
            0x0001_0000,
            0x8000_0000,
            u32::MAX - 64,
            u32::MAX - 16,
            u32::MAX - 4,
            u32::MAX,
        ] {
            // Keep only what `XzFilter::parse` would accept.
            let aligned = candidate - (candidate % alignment);
            if !offsets.contains(&aligned) {
                offsets.push(aligned);
            }
        }
        offsets
    }

    fn bcj_ids() -> [u64; 7] {
        [
            FILTER_BCJ_X86,
            FILTER_BCJ_POWERPC,
            FILTER_BCJ_IA64,
            FILTER_BCJ_ARM,
            FILTER_BCJ_ARMTHUMB,
            FILTER_BCJ_SPARC,
            FILTER_BCJ_ARM64,
        ]
    }

    #[test]
    fn bcj_converters_round_trip_at_every_start_offset() {
        let markers: [&[u8]; 7] = [
            &[0xE8, 0x00, 0x00, 0x00],
            &[0xE9, 0xFF, 0xFF, 0xFF],
            &[0x48, 0x00, 0x00, 0x01],
            &[0x00, 0x00, 0x00, 0xEB],
            &[0x40, 0x00, 0x00, 0x00],
            &[0x00, 0xF0, 0x00, 0xF8],
            &[0x00, 0x00, 0x00, 0x94],
        ];
        for id in bcj_ids() {
            for start_offset in start_offsets_for(id) {
                let filter = XzFilter::Bcj { id, start_offset };
                for marker in markers {
                    for len in [0usize, 4, 5, 15, 16, 17, 32, 64, 256, 1024, 4099] {
                        let original = pattern_bytes(len, marker);
                        let mut buffer = original.clone();
                        filter.encode(&mut buffer);
                        filter.decode(&mut buffer);
                        assert_eq!(
                            buffer, original,
                            "{filter:?} start_offset {start_offset} len {len}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn bcj_converters_never_panic_on_hostile_bytes() {
        // A dense mixture of every converter's trigger bytes, so the branch
        // that indexes `MASK_TO_BIT_NUMBER` with `prev_mask >> 1` (x86) is
        // driven hard from many `prev_mask` histories.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut data = Vec::with_capacity(8192);
        while data.len() < 8192 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let bytes = seed.to_le_bytes();
            // Bias heavily toward E8/E9/EB/48/40/F0/F8/94 so the converters
            // fire on nearly every position instead of almost never.
            for byte in bytes {
                data.push(match byte % 10 {
                    0 => 0xE8,
                    1 => 0xE9,
                    2 => 0xEB,
                    3 => 0x48,
                    4 => 0x40,
                    5 => 0xF0,
                    6 => 0xF8,
                    7 => 0x94,
                    8 => 0x00,
                    _ => byte,
                });
            }
        }

        for id in bcj_ids() {
            for start_offset in start_offsets_for(id) {
                let filter = XzFilter::Bcj { id, start_offset };
                for window in [1usize, 5, 16, 17, 63, 4096, data.len()] {
                    let mut buffer = data[..window.min(data.len())].to_vec();
                    let before = buffer.len();
                    filter.decode(&mut buffer);
                    assert_eq!(buffer.len(), before, "{filter:?} changed the block length");
                    // And the inverse must still be an inverse.
                    let decoded = buffer.clone();
                    filter.encode(&mut buffer);
                    filter.decode(&mut buffer);
                    assert_eq!(buffer, decoded, "{filter:?} start_offset {start_offset}");
                }
            }
        }
    }

    #[test]
    fn delta_never_panics_on_any_distance() {
        for distance in 1..=256usize {
            for len in [0usize, 1, 2, 255, 256, 257, 1024] {
                let original = pattern_bytes(len, &[0x01, 0x02, 0x03, 0x04]);
                let mut buffer = original.clone();
                delta_encode(&mut buffer, distance);
                delta_decode(&mut buffer, distance);
                assert_eq!(buffer, original, "distance {distance} len {len}");
            }
        }
    }
}

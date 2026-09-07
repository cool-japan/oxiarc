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
//! | 0x0B | BCJ RISC-V | decode + encode |
//!
//! An unknown filter ID is a hard error; nothing is ever silently ignored.
//!
//! The RISC-V converter (added in XZ Utils 5.6) is the odd one out: its
//! encoder and decoder are different scans rather than one reversible pass,
//! so an implementation can be self-consistent — round-tripping its own
//! output perfectly — while disagreeing with liblzma and silently corrupting
//! real `.xz` files. Both directions are therefore pinned against the
//! reference in `filters_match_the_xz_cli_byte_for_byte`, which compares the
//! *filtered intermediate* rather than just a round trip.
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
/// BCJ RISC-V filter ID.
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
    /// [`OxiArcError::UnsupportedMethod`] for an unknown filter ID, and
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
            | FILTER_BCJ_ARMTHUMB | FILTER_BCJ_SPARC | FILTER_BCJ_ARM64 | FILTER_BCJ_RISCV => {
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
        FILTER_BCJ_ARMTHUMB | FILTER_BCJ_RISCV => 2,
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
        FILTER_BCJ_RISCV => riscv_code(now_pos, is_encoder, data),
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

// ---------------------------------------------------------------------------
// BCJ RISC-V
// ---------------------------------------------------------------------------
//
// Unlike every other converter in this module, the RISC-V encoder and decoder
// are *not* the same scan run in two directions: the encoder rewrites an
// AUIPC+inst2 pair into a marker instruction (AUIPC with `rd = x2`) followed
// by a big-endian absolute address, and the decoder recognises that marker.
// Because arbitrary data can already contain something that looks like the
// marker, each direction also performs the *opposite* conversion on such
// "fake" pairs, which is what keeps the transform bijective on non-code
// input. Getting only one direction right therefore still corrupts data, so
// both are validated against liblzma and the `xz` CLI byte for byte (see this
// module's `xz-oracle` tests).

/// Read a little-endian `u32` at `index` (the caller guarantees four bytes).
#[inline]
fn read_u32_le(buffer: &[u8], index: usize) -> u32 {
    u32::from_le_bytes([
        buffer[index],
        buffer[index + 1],
        buffer[index + 2],
        buffer[index + 3],
    ])
}

/// Write a little-endian `u32` at `index` (the caller guarantees four bytes).
#[inline]
fn write_u32_le(buffer: &mut [u8], index: usize, value: u32) {
    buffer[index..index + 4].copy_from_slice(&value.to_le_bytes());
}

/// `true` when `auipc` and `inst2` are **not** a convertible AUIPC pair.
///
/// Two conditions are tested at once (liblzma's `NOT_AUIPC_PAIR`):
///
/// * AUIPC's `rd` [11:7] must equal `inst2`'s `rs1` [19:15] — the 8-bit left
///   shift lines those two fields up, so the XOR leaves them zero only when
///   the registers match;
/// * `inst2`'s lowest two opcode bits must both be set, i.e. it must not be a
///   16-bit compressed instruction — subtracting 3 clears exactly those two
///   bits when they are set.
///
/// The mask keeps only those seven bits, so a non-zero result means "not a
/// pair". Deliberately relaxed: any 32-bit instruction is accepted as
/// `inst2`, which costs almost nothing in ratio and keeps the filter small.
#[inline]
fn not_auipc_pair(auipc: u32, inst2: u32) -> bool {
    ((auipc << 8) ^ inst2.wrapping_sub(3)) & 0x000F_8003 != 0
}

/// `true` when `auipc` is **not** in the encoder's special (already
/// converted) format (liblzma's `NOT_SPECIAL_AUIPC`).
///
/// The special format is AUIPC with `rd == x2` whose bits 12 and 13 hold the
/// two set opcode bits of the packed `inst2`. Subtracting `0x3117` zeroes the
/// AUIPC opcode, the `x2` register field and those two bits when all three
/// match; the shift by 18 drops the bits above them so that any surviving bit
/// makes the left-hand side larger than the right. The right-hand side is
/// non-zero unless `inst2_rs1` is `x0` or `x2`, which is how the reverse
/// conversion's own precondition is folded into the same comparison.
#[inline]
fn not_special_auipc(auipc: u32, inst2_rs1: u32) -> bool {
    (auipc.wrapping_sub(0x3117) << 18) >= (inst2_rs1 & 0x1D)
}

/// RISC-V branch converter (filter 0x0B, XZ Utils 5.6+).
fn riscv_code(now_pos: u32, is_encoder: bool, buffer: &mut [u8]) {
    if is_encoder {
        riscv_encode(now_pos, buffer);
    } else {
        riscv_decode(now_pos, buffer);
    }
}

/// Convert pc-relative JAL and AUIPC+inst2 pairs to absolute addresses.
///
/// The scan steps two bytes at a time because the C extension's 16-bit
/// instructions may sit between 32-bit ones, and it stops eight bytes before
/// the end: an instruction pair that would not fit entirely inside the block
/// is left alone (which is also what liblzma's chunked coder does with its
/// unfiltered tail).
fn riscv_encode(now_pos: u32, buffer: &mut [u8]) {
    if buffer.len() < 8 {
        return;
    }
    let limit = buffer.len() - 8;
    let mut i = 0usize;

    while i <= limit {
        let first = buffer[i];

        if first == 0xEF {
            // JAL. Only `rd = x1` (ra) and `rd = x5` (t0) are converted:
            // those are the calls. `JAL x0` is a plain jump, whose absolute
            // address rarely repeats, and requiring the register keeps false
            // matches on non-code data down.
            let b1 = u32::from(buffer[i + 1]);
            if b1 & 0x0D != 0 {
                i += 2;
                continue;
            }

            let b2 = u32::from(buffer[i + 2]);
            let b3 = u32::from(buffer[i + 3]);
            let pc = now_pos.wrapping_add(i as u32);

            // Gather the J-type immediate, which the encoding scatters over
            // bits [31:12] in four pieces, into a plain address.
            let addr = (((b1 & 0xF0) << 8)
                | ((b2 & 0x0F) << 16)
                | ((b2 & 0x10) << 7)
                | ((b2 & 0xE0) >> 4)
                | ((b3 & 0x7F) << 4)
                | ((b3 & 0x80) << 13))
                .wrapping_add(pc);

            // Store it big endian: addresses that differ only in their low
            // bytes then share a longer common prefix, which compresses
            // slightly better.
            buffer[i + 1] = ((b1 & 0x0F) | ((addr >> 13) & 0xF0)) as u8;
            buffer[i + 2] = (addr >> 9) as u8;
            buffer[i + 3] = (addr >> 1) as u8;

            i += 4;
        } else if first & 0x7F == 0x17 {
            // AUIPC
            let mut inst = u32::from(first)
                | (u32::from(buffer[i + 1]) << 8)
                | (u32::from(buffer[i + 2]) << 16)
                | (u32::from(buffer[i + 3]) << 24);

            if inst & 0xE80 != 0 {
                // AUIPC's rd is neither x0 nor x2, so this may be a real
                // pair. (The bitmask is the branch-free form of
                // `rd != 0 && rd != 2`.)
                let inst2 = read_u32_le(buffer, i + 4);

                if not_auipc_pair(inst, inst2) {
                    // Skip six bytes rather than four: the pair test also
                    // looks at the low bits of `buffer[i + 6]`, and a later
                    // conversion starting inside this instruction could turn
                    // *this* position into a valid pair and desync the
                    // decoder. Six is safe because a conversion at those
                    // positions never rewrites the bits the test reads.
                    i += 6;
                    continue;
                }

                // Absolute address = AUIPC's upper 20 bits + inst2's
                // sign-extended 12-bit immediate + pc. AUIPC does not zero
                // the low 12 bits of its result, which is why inst2 has to
                // be folded in here rather than converted on its own.
                let addr = (inst & 0xFFFF_F000)
                    .wrapping_add(inst2 >> 20)
                    .wrapping_sub((inst2 >> 19) & 0x1000)
                    .wrapping_add(now_pos.wrapping_add(i as u32));

                // The marker instruction: [6:0] the AUIPC opcode, [11:7]
                // `rd = x2` (the stack pointer, so a real AUIPC almost never
                // uses it), [31:12] the low 20 bits of inst2 — everything
                // but its immediate, which the address below replaces.
                inst = 0x17 | (2 << 7) | (inst2 << 12);
                write_u32_le(buffer, i, inst);
                buffer[i + 4..i + 8].copy_from_slice(&addr.to_be_bytes());
            } else {
                // AUIPC's rd is x0 or x2. x0 marks a landing pad (LPAD),
                // whose 20-bit field is a label rather than an address, and
                // is never converted. x2 is the marker the branch above
                // writes, so input that already looks like the special
                // format must get the *opposite* conversion here to keep the
                // filter bijective on arbitrary bytes. This "fake" form is a
                // simplification of the decoder's real one: no sign
                // extension, no address arithmetic, little endian.
                let fake_rs1 = inst >> 27;
                if not_special_auipc(inst, fake_rs1) {
                    i += 4;
                    continue;
                }

                let fake_addr = read_u32_le(buffer, i + 4);
                let fake_inst2 = (inst >> 12) | (fake_addr << 20);
                inst = 0x17 | (fake_rs1 << 7) | (fake_addr & 0xFFFF_F000);
                write_u32_le(buffer, i, inst);
                write_u32_le(buffer, i + 4, fake_inst2);
            }

            i += 8;
        } else {
            i += 2;
        }
    }
}

/// Undo [`riscv_encode`].
fn riscv_decode(now_pos: u32, buffer: &mut [u8]) {
    if buffer.len() < 8 {
        return;
    }
    let limit = buffer.len() - 8;
    let mut i = 0usize;

    while i <= limit {
        let first = buffer[i];

        if first == 0xEF {
            // JAL with rd = x1 or x5: scatter the absolute address back into
            // the J-type immediate's four pieces.
            let b1 = u32::from(buffer[i + 1]);
            if b1 & 0x0D != 0 {
                i += 2;
                continue;
            }

            let b2 = u32::from(buffer[i + 2]);
            let b3 = u32::from(buffer[i + 3]);
            let pc = now_pos.wrapping_add(i as u32);

            let addr = (((b1 & 0xF0) << 13) | (b2 << 9) | (b3 << 1)).wrapping_sub(pc);

            buffer[i + 1] = ((b1 & 0x0F) | ((addr >> 8) & 0xF0)) as u8;
            buffer[i + 2] =
                (((addr >> 16) & 0x0F) | ((addr >> 7) & 0x10) | ((addr << 4) & 0xE0)) as u8;
            buffer[i + 3] = (((addr >> 4) & 0x7F) | ((addr >> 13) & 0x80)) as u8;

            i += 4;
        } else if first & 0x7F == 0x17 {
            // AUIPC
            let mut inst = u32::from(first)
                | (u32::from(buffer[i + 1]) << 8)
                | (u32::from(buffer[i + 2]) << 16)
                | (u32::from(buffer[i + 3]) << 24);
            let inst2;

            if inst & 0xE80 != 0 {
                // rd is neither x0 nor x2, so this cannot be the encoder's
                // marker. If it looks like a pair, it is a "fake" one the
                // encoder converted the other way round; re-encode it.
                let packed = read_u32_le(buffer, i + 4);
                if not_auipc_pair(inst, packed) {
                    i += 6;
                    continue;
                }

                inst2 = (inst & 0xFFFF_F000).wrapping_add(packed >> 20);
                inst = 0x17 | (2 << 7) | (packed << 12);
            } else {
                // rd is x0 or x2: the real marker, if the rest matches.
                let inst2_rs1 = inst >> 27;
                if not_special_auipc(inst, inst2_rs1) {
                    i += 4;
                    continue;
                }

                // The absolute address was stored big endian.
                let addr = u32::from_be_bytes([
                    buffer[i + 4],
                    buffer[i + 5],
                    buffer[i + 6],
                    buffer[i + 7],
                ])
                .wrapping_sub(now_pos.wrapping_add(i as u32));

                // inst2 keeps the low 20 bits the marker carried and takes
                // the address's low 12 bits as its immediate; AUIPC's rd is
                // inst2's rs1 (the pair test guaranteed they are equal), and
                // its upper 20 bits must compensate for the sign extension
                // of that immediate — hence the `+ 0x800`.
                inst2 = (inst >> 12) | (addr << 20);
                inst = 0x17 | (inst2_rs1 << 7) | (addr.wrapping_add(0x800) & 0xFFFF_F000);
            }

            write_u32_le(buffer, i, inst);
            write_u32_le(buffer, i + 4, inst2);

            i += 8;
        } else {
            i += 2;
        }
    }
}

#[cfg(test)]
mod tests;

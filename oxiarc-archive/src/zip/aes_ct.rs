//! Constant-time AES primitives (S-box and GF(2^8) doubling) for the ZIP
//! encryption paths.
//!
//! # Why this module exists
//!
//! The textbook way to implement AES `SubBytes` is a 256-byte lookup table
//! indexed by the state byte. That table index is derived from the round key
//! and the plaintext, so the *memory access pattern* of the cipher depends on
//! secret data. On any CPU with a data cache, an attacker who can co-reside on
//! the machine (another process, another VM, another browser tab) can observe
//! which cache lines the table touched — the classic `prime+probe` /
//! `evict+time` attack — and recover the AES key without ever seeing the
//! plaintext. This is exactly why production libraries use either AES-NI or a
//! bitsliced software S-box.
//!
//! Every operation in this module runs the same instruction sequence with the
//! same memory accesses regardless of the values involved: there are no table
//! lookups, no data-dependent branches, and no data-dependent shift counts.
//!
//! # How the S-box works
//!
//! [`sub_bytes`] transposes the 16 state bytes into eight 16-bit *bit planes*
//! (plane `i` holds bit `i` of all 16 bytes) and evaluates a Boolean circuit
//! for the AES S-box on those planes, so all 16 substitutions happen at once.
//! The circuit is the Boyar–Peralta minimal-depth AES S-box
//! ("A new combinational logic minimization technique with applications to
//! cryptology", Boyar & Peralta 2010): a 23-gate top linear transform, a
//! 34-gate non-linear GF(2^4) inversion core, and a 27-gate bottom linear
//! transform, 113 gates in total, all `AND`/`XOR`/`NOT` on whole words.
//!
//! Correctness is not taken on faith: [`tests::sbox_matches_fips197_table`]
//! checks the circuit against the FIPS 197 S-box table for all 256 inputs, and
//! [`tests::sbox_is_position_independent`] checks that a byte substitutes the
//! same way in every one of the 16 lanes.
//!
//! # Scope
//!
//! Only the *encryption* direction is provided, because that is all WinZip AES
//! needs: AES-CTR derives its keystream with the forward cipher and XORs it in
//! both directions, so there is no inverse S-box and no `InvMixColumns` here.

/// Number of bytes in an AES block / number of bitslice lanes.
const BLOCK_LEN: usize = 16;

/// Transpose 16 bytes into 8 bit planes.
///
/// Plane `i` of the result has bit `lane` set when bit `i` of `bytes[lane]` is
/// set. The loop bounds and shift amounts are compile-time constants, so the
/// transposition is independent of the data it moves.
fn slice_in(bytes: &[u8; BLOCK_LEN]) -> [u16; 8] {
    let mut planes = [0u16; 8];
    for (lane, &byte) in bytes.iter().enumerate() {
        let value = u16::from(byte);
        for (bit, plane) in planes.iter_mut().enumerate() {
            *plane |= ((value >> bit) & 1) << lane;
        }
    }
    planes
}

/// Inverse of [`slice_in`]: gather 8 bit planes back into 16 bytes.
fn slice_out(planes: &[u16; 8]) -> [u8; BLOCK_LEN] {
    let mut bytes = [0u8; BLOCK_LEN];
    for (lane, byte) in bytes.iter_mut().enumerate() {
        let mut value = 0u8;
        for (bit, &plane) in planes.iter().enumerate() {
            value |= (((plane >> lane) & 1) as u8) << bit;
        }
        *byte = value;
    }
    bytes
}

/// Evaluate the AES S-box on eight bit planes in place.
///
/// `planes[0]` is the least-significant bit plane and `planes[7]` the most
/// significant. Every lane is substituted independently and simultaneously.
///
/// The variable names follow the Boyar–Peralta paper (`x*` inputs, `y*` top
/// linear transform, `t*`/`z*` non-linear core, `s*` outputs) so the circuit
/// can be diffed against the published listing line by line.
#[allow(clippy::many_single_char_names)]
fn sbox_planes(planes: &mut [u16; 8]) {
    // The paper numbers its inputs with x0 as the most-significant bit, the
    // opposite of the plane order used here.
    let x0 = planes[7];
    let x1 = planes[6];
    let x2 = planes[5];
    let x3 = planes[4];
    let x4 = planes[3];
    let x5 = planes[2];
    let x6 = planes[1];
    let x7 = planes[0];

    // --- Top linear transform (23 XOR gates) ---------------------------------
    let y14 = x3 ^ x5;
    let y13 = x0 ^ x6;
    let y9 = x0 ^ x3;
    let y8 = x0 ^ x5;
    let t0 = x1 ^ x2;
    let y1 = t0 ^ x7;
    let y4 = y1 ^ x3;
    let y12 = y13 ^ y14;
    let y2 = y1 ^ x0;
    let y5 = y1 ^ x6;
    let y3 = y5 ^ y8;
    let t1 = x4 ^ y12;
    let y15 = t1 ^ x5;
    let y20 = t1 ^ x1;
    let y6 = y15 ^ x7;
    let y10 = y15 ^ t0;
    let y11 = y20 ^ y9;
    let y7 = x7 ^ y11;
    let y17 = y10 ^ y11;
    let y19 = y10 ^ y8;
    let y16 = t0 ^ y11;
    let y21 = y13 ^ y16;
    let y18 = x0 ^ y16;

    // --- Non-linear core: inversion in GF(2^4) (34 AND + 21 XOR) -------------
    let t2 = y12 & y15;
    let t3 = y3 & y6;
    let t4 = t3 ^ t2;
    let t5 = y4 & x7;
    let t6 = t5 ^ t2;
    let t7 = y13 & y16;
    let t8 = y5 & y1;
    let t9 = t8 ^ t7;
    let t10 = y2 & y7;
    let t11 = t10 ^ t7;
    let t12 = y9 & y11;
    let t13 = y14 & y17;
    let t14 = t13 ^ t12;
    let t15 = y8 & y10;
    let t16 = t15 ^ t12;
    let t17 = t4 ^ t14;
    let t18 = t6 ^ t16;
    let t19 = t9 ^ t14;
    let t20 = t11 ^ t16;
    let t21 = t17 ^ y20;
    let t22 = t18 ^ y19;
    let t23 = t19 ^ y21;
    let t24 = t20 ^ y18;
    let t25 = t21 ^ t22;
    let t26 = t21 & t23;
    let t27 = t24 ^ t26;
    let t28 = t25 & t27;
    let t29 = t28 ^ t22;
    let t30 = t23 ^ t24;
    let t31 = t22 ^ t26;
    let t32 = t31 & t30;
    let t33 = t32 ^ t24;
    let t34 = t23 ^ t33;
    let t35 = t27 ^ t33;
    let t36 = t24 & t35;
    let t37 = t36 ^ t34;
    let t38 = t27 ^ t36;
    let t39 = t29 & t38;
    let t40 = t25 ^ t39;
    let t41 = t40 ^ t37;
    let t42 = t29 ^ t33;
    let t43 = t29 ^ t40;
    let t44 = t33 ^ t37;
    let t45 = t42 ^ t41;
    let z0 = t44 & y15;
    let z1 = t37 & y6;
    let z2 = t33 & x7;
    let z3 = t43 & y16;
    let z4 = t40 & y1;
    let z5 = t29 & y7;
    let z6 = t42 & y11;
    let z7 = t45 & y17;
    let z8 = t41 & y10;
    let z9 = t44 & y12;
    let z10 = t37 & y3;
    let z11 = t33 & y4;
    let z12 = t43 & y13;
    let z13 = t40 & y5;
    let z14 = t29 & y2;
    let z15 = t42 & y9;
    let z16 = t45 & y14;
    let z17 = t41 & y8;

    // --- Bottom linear transform (27 XOR gates, 4 of them complemented) ------
    // The four `!` complements below fold in the affine constant 0x63 of the
    // FIPS 197 S-box definition `S(a) = A * a^-1 + 0x63`.
    let t46 = z15 ^ z16;
    let t47 = z10 ^ z11;
    let t48 = z5 ^ z13;
    let t49 = z9 ^ z10;
    let t50 = z2 ^ z12;
    let t51 = z2 ^ z5;
    let t52 = z7 ^ z8;
    let t53 = z0 ^ z3;
    let t54 = z6 ^ z7;
    let t55 = z16 ^ z17;
    let t56 = z12 ^ t48;
    let t57 = t50 ^ t53;
    let t58 = z4 ^ t46;
    let t59 = z3 ^ t54;
    let t60 = t46 ^ t57;
    let t61 = z14 ^ t57;
    let t62 = t52 ^ t58;
    let t63 = t49 ^ t58;
    let t64 = z4 ^ t59;
    let t65 = t61 ^ t62;
    let t66 = z1 ^ t63;
    let t67 = t64 ^ t65;
    let s0 = t59 ^ t63;
    let s6 = !(t56 ^ t62);
    let s7 = !(t48 ^ t60);
    let s3 = t53 ^ t66;
    let s4 = t51 ^ t66;
    let s5 = t47 ^ t65;
    let s1 = !(t64 ^ s3);
    let s2 = !(t55 ^ t67);

    planes[7] = s0;
    planes[6] = s1;
    planes[5] = s2;
    planes[4] = s3;
    planes[3] = s4;
    planes[2] = s5;
    planes[1] = s6;
    planes[0] = s7;
}

/// Apply the AES `SubBytes` transformation to a whole state, in constant time.
pub(crate) fn sub_bytes(state: &mut [u8; BLOCK_LEN]) {
    let mut planes = slice_in(state);
    sbox_planes(&mut planes);
    *state = slice_out(&planes);
}

/// Apply the AES `SubWord` transformation (the S-box on four bytes) used by the
/// key schedule, in constant time.
///
/// The four bytes occupy the low four lanes of a full 16-lane bitslice; the
/// remaining twelve lanes are zero and their results are discarded. Padding
/// with zeros is safe precisely because the circuit is lane-independent — a
/// property [`tests::sbox_is_position_independent`] pins down.
pub(crate) fn sub_word(word: [u8; 4]) -> [u8; 4] {
    let mut block = [0u8; BLOCK_LEN];
    block[..4].copy_from_slice(&word);
    sub_bytes(&mut block);
    [block[0], block[1], block[2], block[3]]
}

/// Multiply by `x` (i.e. by 2) in GF(2^8) with the AES reduction polynomial
/// `x^8 + x^4 + x^3 + x + 1`, without branching on the input.
///
/// The conditional reduction is expressed as a mask: `(value >> 7)` is 0 or 1,
/// and `wrapping_neg` turns that into `0x00` or `0xff`, which then selects
/// either `0x00` or `0x1b` to XOR in. The textbook `if value & 0x80 != 0`
/// version is a secret-dependent branch inside `MixColumns`.
pub(crate) fn xtime(value: u8) -> u8 {
    let reduce = (value >> 7).wrapping_neg() & 0x1b;
    (value << 1) ^ reduce
}

/// Multiply by 3 in GF(2^8): `3 * v = (2 * v) ^ v`.
pub(crate) fn xtime3(value: u8) -> u8 {
    xtime(value) ^ value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The FIPS 197 (AES) substitution table.
    ///
    /// Kept **only** as a test oracle. Production code must never index a table
    /// with secret data; that is the entire point of this module.
    const SBOX_ORACLE: [u8; 256] = [
        0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab,
        0x76, 0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4,
        0x72, 0xc0, 0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71,
        0xd8, 0x31, 0x15, 0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2,
        0xeb, 0x27, 0xb2, 0x75, 0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6,
        0xb3, 0x29, 0xe3, 0x2f, 0x84, 0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb,
        0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf, 0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45,
        0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8, 0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5,
        0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2, 0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44,
        0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73, 0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a,
        0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb, 0xe0, 0x32, 0x3a, 0x0a, 0x49,
        0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79, 0xe7, 0xc8, 0x37, 0x6d,
        0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08, 0xba, 0x78, 0x25,
        0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a, 0x70, 0x3e,
        0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e, 0xe1,
        0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
        0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb,
        0x16,
    ];

    /// Exhaustive equivalence proof: the gate circuit and the FIPS 197 table
    /// agree on every one of the 256 possible input bytes.
    #[test]
    fn sbox_matches_fips197_table() {
        for value in 0..=255u8 {
            let mut state = [value; BLOCK_LEN];
            sub_bytes(&mut state);
            let expected = SBOX_ORACLE[value as usize];
            for (lane, &got) in state.iter().enumerate() {
                assert_eq!(
                    got, expected,
                    "S-box mismatch for input {value:#04x} in lane {lane}"
                );
            }
        }
    }

    /// A byte must substitute identically no matter which of the 16 lanes it
    /// sits in — the property that lets [`sub_word`] zero-pad safely and that
    /// guarantees the bitslice transpose is not mixing lanes.
    #[test]
    fn sbox_is_position_independent() {
        for lane in 0..BLOCK_LEN {
            for value in 0..=255u8 {
                let mut state = [0u8; BLOCK_LEN];
                state[lane] = value;
                sub_bytes(&mut state);
                assert_eq!(
                    state[lane], SBOX_ORACLE[value as usize],
                    "lane {lane} substituted {value:#04x} differently"
                );
                for (other, &byte) in state.iter().enumerate() {
                    if other != lane {
                        assert_eq!(
                            byte, SBOX_ORACLE[0],
                            "zero lane {other} did not map to S(0)"
                        );
                    }
                }
            }
        }
    }

    /// `sub_word` is the S-box applied bytewise, exactly as FIPS 197 §5.2 says.
    #[test]
    fn sub_word_matches_table() {
        let inputs = [
            [0x00, 0x01, 0x02, 0x03],
            [0xff, 0xfe, 0x80, 0x7f],
            [0xcf, 0x4f, 0x3c, 0x09],
        ];
        for word in inputs {
            let got = sub_word(word);
            let want = [
                SBOX_ORACLE[word[0] as usize],
                SBOX_ORACLE[word[1] as usize],
                SBOX_ORACLE[word[2] as usize],
                SBOX_ORACLE[word[3] as usize],
            ];
            assert_eq!(got, want, "SubWord mismatch for {word:02x?}");
        }
    }

    /// The branchless `xtime` agrees with the textbook branching definition on
    /// every input, and matches the FIPS 197 worked values.
    #[test]
    fn xtime_matches_branching_definition() {
        for value in 0..=255u8 {
            let mut branching = value << 1;
            if value & 0x80 != 0 {
                branching ^= 0x1b;
            }
            assert_eq!(xtime(value), branching, "xtime mismatch at {value:#04x}");
            assert_eq!(
                xtime3(value),
                branching ^ value,
                "xtime3 mismatch at {value:#04x}"
            );
        }
        // FIPS 197 §4.2.1: xtime(0x57) = 0xae, xtime(0xae) = 0x47.
        assert_eq!(xtime(0x57), 0xae);
        assert_eq!(xtime(0xae), 0x47);
    }

    /// The bitslice transpose must be an exact round trip for arbitrary states.
    #[test]
    fn bitslice_transpose_round_trips() {
        let mut state = [0u8; BLOCK_LEN];
        // A deterministic spread of values covering both nibbles of every lane.
        for (lane, byte) in state.iter_mut().enumerate() {
            *byte = (lane as u8).wrapping_mul(37).wrapping_add(11);
        }
        let planes = slice_in(&state);
        assert_eq!(slice_out(&planes), state);

        let all_ones = [0xffu8; BLOCK_LEN];
        assert_eq!(slice_out(&slice_in(&all_ones)), all_ones);
        let zeros = [0u8; BLOCK_LEN];
        assert_eq!(slice_out(&slice_in(&zeros)), zeros);
    }

    /// The production module must not contain a substitution table at all: the
    /// only 256-entry array in this file is the test oracle above.
    #[test]
    fn sbox_circuit_has_no_lookup_table() {
        let source = include_str!("aes_ct.rs");
        let (production, _tests) = source
            .split_once("#[cfg(test)]")
            .expect("aes_ct.rs always has a #[cfg(test)] module");
        assert!(
            !production.contains("[u8; 256]"),
            "a 256-byte table appeared in the constant-time production code"
        );
    }
}

//! The QM-coder probability estimation state machine (ITU-T T.81 Table D.3).
//!
//! One entry per state index. `qe` is the LPS sub-interval size in the
//! normalised 16-bit interval, `next_mps` / `next_lps` are the state indices
//! taken after an MPS or LPS renormalisation, and `switch_mps` says that an
//! LPS renormalisation also exchanges the meanings of MPS and LPS.
//!
//! The table is the one shared by JPEG (T.81 Table D.3) and JBIG (T.82
//! Table 24). Index 113 is T.851's fixed 0.5 estimate: it is its own
//! successor on both paths and never switches, so a bin parked there stays
//! there — which is why [`FIXED_INDEX`] can be used from a temporary.

/// One row of the probability estimation state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QmState {
    /// `Qe`: the LPS sub-interval size.
    pub(crate) qe: u16,
    /// Next state index after an LPS renormalisation.
    pub(crate) next_lps: u8,
    /// Next state index after an MPS renormalisation.
    pub(crate) next_mps: u8,
    /// Whether an LPS renormalisation exchanges MPS and LPS.
    pub(crate) switch_mps: bool,
}

/// State index of the fixed 0.5 probability estimate (T.851 clause 10.3).
pub(crate) const FIXED_INDEX: u8 = 113;

/// A statistics bin: bit 7 holds the MPS value, bits 0..=6 the state index.
pub(crate) type Bin = u8;

/// T.81 Table D.3 in full, padded to 128 rows.
///
/// The standard defines 114 states, but a statistics bin holds its index in
/// seven bits, so a corrupt bin can name any of 128. Padding the table with
/// copies of the fixed 0.5 estimate makes every one of those indices both
/// safe and harmless — and lets [`state_of`] index with a masked value that
/// the compiler can prove is in range, which is worth a few per cent on the
/// decoder's inner loop.
pub(crate) const QM_STATES: [QmState; 128] = [
    QmState {
        qe: 0x5a1d,
        next_lps: 1,
        next_mps: 1,
        switch_mps: true,
    },
    QmState {
        qe: 0x2586,
        next_lps: 14,
        next_mps: 2,
        switch_mps: false,
    },
    QmState {
        qe: 0x1114,
        next_lps: 16,
        next_mps: 3,
        switch_mps: false,
    },
    QmState {
        qe: 0x080b,
        next_lps: 18,
        next_mps: 4,
        switch_mps: false,
    },
    QmState {
        qe: 0x03d8,
        next_lps: 20,
        next_mps: 5,
        switch_mps: false,
    },
    QmState {
        qe: 0x01da,
        next_lps: 23,
        next_mps: 6,
        switch_mps: false,
    },
    QmState {
        qe: 0x00e5,
        next_lps: 25,
        next_mps: 7,
        switch_mps: false,
    },
    QmState {
        qe: 0x006f,
        next_lps: 28,
        next_mps: 8,
        switch_mps: false,
    },
    QmState {
        qe: 0x0036,
        next_lps: 30,
        next_mps: 9,
        switch_mps: false,
    },
    QmState {
        qe: 0x001a,
        next_lps: 33,
        next_mps: 10,
        switch_mps: false,
    },
    QmState {
        qe: 0x000d,
        next_lps: 35,
        next_mps: 11,
        switch_mps: false,
    },
    QmState {
        qe: 0x0006,
        next_lps: 9,
        next_mps: 12,
        switch_mps: false,
    },
    QmState {
        qe: 0x0003,
        next_lps: 10,
        next_mps: 13,
        switch_mps: false,
    },
    QmState {
        qe: 0x0001,
        next_lps: 12,
        next_mps: 13,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a7f,
        next_lps: 15,
        next_mps: 15,
        switch_mps: true,
    },
    QmState {
        qe: 0x3f25,
        next_lps: 36,
        next_mps: 16,
        switch_mps: false,
    },
    QmState {
        qe: 0x2cf2,
        next_lps: 38,
        next_mps: 17,
        switch_mps: false,
    },
    QmState {
        qe: 0x207c,
        next_lps: 39,
        next_mps: 18,
        switch_mps: false,
    },
    QmState {
        qe: 0x17b9,
        next_lps: 40,
        next_mps: 19,
        switch_mps: false,
    },
    QmState {
        qe: 0x1182,
        next_lps: 42,
        next_mps: 20,
        switch_mps: false,
    },
    QmState {
        qe: 0x0cef,
        next_lps: 43,
        next_mps: 21,
        switch_mps: false,
    },
    QmState {
        qe: 0x09a1,
        next_lps: 45,
        next_mps: 22,
        switch_mps: false,
    },
    QmState {
        qe: 0x072f,
        next_lps: 46,
        next_mps: 23,
        switch_mps: false,
    },
    QmState {
        qe: 0x055c,
        next_lps: 48,
        next_mps: 24,
        switch_mps: false,
    },
    QmState {
        qe: 0x0406,
        next_lps: 49,
        next_mps: 25,
        switch_mps: false,
    },
    QmState {
        qe: 0x0303,
        next_lps: 51,
        next_mps: 26,
        switch_mps: false,
    },
    QmState {
        qe: 0x0240,
        next_lps: 52,
        next_mps: 27,
        switch_mps: false,
    },
    QmState {
        qe: 0x01b1,
        next_lps: 54,
        next_mps: 28,
        switch_mps: false,
    },
    QmState {
        qe: 0x0144,
        next_lps: 56,
        next_mps: 29,
        switch_mps: false,
    },
    QmState {
        qe: 0x00f5,
        next_lps: 57,
        next_mps: 30,
        switch_mps: false,
    },
    QmState {
        qe: 0x00b7,
        next_lps: 59,
        next_mps: 31,
        switch_mps: false,
    },
    QmState {
        qe: 0x008a,
        next_lps: 60,
        next_mps: 32,
        switch_mps: false,
    },
    QmState {
        qe: 0x0068,
        next_lps: 62,
        next_mps: 33,
        switch_mps: false,
    },
    QmState {
        qe: 0x004e,
        next_lps: 63,
        next_mps: 34,
        switch_mps: false,
    },
    QmState {
        qe: 0x003b,
        next_lps: 32,
        next_mps: 35,
        switch_mps: false,
    },
    QmState {
        qe: 0x002c,
        next_lps: 33,
        next_mps: 9,
        switch_mps: false,
    },
    QmState {
        qe: 0x5ae1,
        next_lps: 37,
        next_mps: 37,
        switch_mps: true,
    },
    QmState {
        qe: 0x484c,
        next_lps: 64,
        next_mps: 38,
        switch_mps: false,
    },
    QmState {
        qe: 0x3a0d,
        next_lps: 65,
        next_mps: 39,
        switch_mps: false,
    },
    QmState {
        qe: 0x2ef1,
        next_lps: 67,
        next_mps: 40,
        switch_mps: false,
    },
    QmState {
        qe: 0x261f,
        next_lps: 68,
        next_mps: 41,
        switch_mps: false,
    },
    QmState {
        qe: 0x1f33,
        next_lps: 69,
        next_mps: 42,
        switch_mps: false,
    },
    QmState {
        qe: 0x19a8,
        next_lps: 70,
        next_mps: 43,
        switch_mps: false,
    },
    QmState {
        qe: 0x1518,
        next_lps: 72,
        next_mps: 44,
        switch_mps: false,
    },
    QmState {
        qe: 0x1177,
        next_lps: 73,
        next_mps: 45,
        switch_mps: false,
    },
    QmState {
        qe: 0x0e74,
        next_lps: 74,
        next_mps: 46,
        switch_mps: false,
    },
    QmState {
        qe: 0x0bfb,
        next_lps: 75,
        next_mps: 47,
        switch_mps: false,
    },
    QmState {
        qe: 0x09f8,
        next_lps: 77,
        next_mps: 48,
        switch_mps: false,
    },
    QmState {
        qe: 0x0861,
        next_lps: 78,
        next_mps: 49,
        switch_mps: false,
    },
    QmState {
        qe: 0x0706,
        next_lps: 79,
        next_mps: 50,
        switch_mps: false,
    },
    QmState {
        qe: 0x05cd,
        next_lps: 48,
        next_mps: 51,
        switch_mps: false,
    },
    QmState {
        qe: 0x04de,
        next_lps: 50,
        next_mps: 52,
        switch_mps: false,
    },
    QmState {
        qe: 0x040f,
        next_lps: 50,
        next_mps: 53,
        switch_mps: false,
    },
    QmState {
        qe: 0x0363,
        next_lps: 51,
        next_mps: 54,
        switch_mps: false,
    },
    QmState {
        qe: 0x02d4,
        next_lps: 52,
        next_mps: 55,
        switch_mps: false,
    },
    QmState {
        qe: 0x025c,
        next_lps: 53,
        next_mps: 56,
        switch_mps: false,
    },
    QmState {
        qe: 0x01f8,
        next_lps: 54,
        next_mps: 57,
        switch_mps: false,
    },
    QmState {
        qe: 0x01a4,
        next_lps: 55,
        next_mps: 58,
        switch_mps: false,
    },
    QmState {
        qe: 0x0160,
        next_lps: 56,
        next_mps: 59,
        switch_mps: false,
    },
    QmState {
        qe: 0x0125,
        next_lps: 57,
        next_mps: 60,
        switch_mps: false,
    },
    QmState {
        qe: 0x00f6,
        next_lps: 58,
        next_mps: 61,
        switch_mps: false,
    },
    QmState {
        qe: 0x00cb,
        next_lps: 59,
        next_mps: 62,
        switch_mps: false,
    },
    QmState {
        qe: 0x00ab,
        next_lps: 61,
        next_mps: 63,
        switch_mps: false,
    },
    QmState {
        qe: 0x008f,
        next_lps: 61,
        next_mps: 32,
        switch_mps: false,
    },
    QmState {
        qe: 0x5b12,
        next_lps: 65,
        next_mps: 65,
        switch_mps: true,
    },
    QmState {
        qe: 0x4d04,
        next_lps: 80,
        next_mps: 66,
        switch_mps: false,
    },
    QmState {
        qe: 0x412c,
        next_lps: 81,
        next_mps: 67,
        switch_mps: false,
    },
    QmState {
        qe: 0x37d8,
        next_lps: 82,
        next_mps: 68,
        switch_mps: false,
    },
    QmState {
        qe: 0x2fe8,
        next_lps: 83,
        next_mps: 69,
        switch_mps: false,
    },
    QmState {
        qe: 0x293c,
        next_lps: 84,
        next_mps: 70,
        switch_mps: false,
    },
    QmState {
        qe: 0x2379,
        next_lps: 86,
        next_mps: 71,
        switch_mps: false,
    },
    QmState {
        qe: 0x1edf,
        next_lps: 87,
        next_mps: 72,
        switch_mps: false,
    },
    QmState {
        qe: 0x1aa9,
        next_lps: 87,
        next_mps: 73,
        switch_mps: false,
    },
    QmState {
        qe: 0x174e,
        next_lps: 72,
        next_mps: 74,
        switch_mps: false,
    },
    QmState {
        qe: 0x1424,
        next_lps: 72,
        next_mps: 75,
        switch_mps: false,
    },
    QmState {
        qe: 0x119c,
        next_lps: 74,
        next_mps: 76,
        switch_mps: false,
    },
    QmState {
        qe: 0x0f6b,
        next_lps: 74,
        next_mps: 77,
        switch_mps: false,
    },
    QmState {
        qe: 0x0d51,
        next_lps: 75,
        next_mps: 78,
        switch_mps: false,
    },
    QmState {
        qe: 0x0bb6,
        next_lps: 77,
        next_mps: 79,
        switch_mps: false,
    },
    QmState {
        qe: 0x0a40,
        next_lps: 77,
        next_mps: 48,
        switch_mps: false,
    },
    QmState {
        qe: 0x5832,
        next_lps: 80,
        next_mps: 81,
        switch_mps: true,
    },
    QmState {
        qe: 0x4d1c,
        next_lps: 88,
        next_mps: 82,
        switch_mps: false,
    },
    QmState {
        qe: 0x438e,
        next_lps: 89,
        next_mps: 83,
        switch_mps: false,
    },
    QmState {
        qe: 0x3bdd,
        next_lps: 90,
        next_mps: 84,
        switch_mps: false,
    },
    QmState {
        qe: 0x34ee,
        next_lps: 91,
        next_mps: 85,
        switch_mps: false,
    },
    QmState {
        qe: 0x2eae,
        next_lps: 92,
        next_mps: 86,
        switch_mps: false,
    },
    QmState {
        qe: 0x299a,
        next_lps: 93,
        next_mps: 87,
        switch_mps: false,
    },
    QmState {
        qe: 0x2516,
        next_lps: 86,
        next_mps: 71,
        switch_mps: false,
    },
    QmState {
        qe: 0x5570,
        next_lps: 88,
        next_mps: 89,
        switch_mps: true,
    },
    QmState {
        qe: 0x4ca9,
        next_lps: 95,
        next_mps: 90,
        switch_mps: false,
    },
    QmState {
        qe: 0x44d9,
        next_lps: 96,
        next_mps: 91,
        switch_mps: false,
    },
    QmState {
        qe: 0x3e22,
        next_lps: 97,
        next_mps: 92,
        switch_mps: false,
    },
    QmState {
        qe: 0x3824,
        next_lps: 99,
        next_mps: 93,
        switch_mps: false,
    },
    QmState {
        qe: 0x32b4,
        next_lps: 99,
        next_mps: 94,
        switch_mps: false,
    },
    QmState {
        qe: 0x2e17,
        next_lps: 93,
        next_mps: 86,
        switch_mps: false,
    },
    QmState {
        qe: 0x56a8,
        next_lps: 95,
        next_mps: 96,
        switch_mps: true,
    },
    QmState {
        qe: 0x4f46,
        next_lps: 101,
        next_mps: 97,
        switch_mps: false,
    },
    QmState {
        qe: 0x47e5,
        next_lps: 102,
        next_mps: 98,
        switch_mps: false,
    },
    QmState {
        qe: 0x41cf,
        next_lps: 103,
        next_mps: 99,
        switch_mps: false,
    },
    QmState {
        qe: 0x3c3d,
        next_lps: 104,
        next_mps: 100,
        switch_mps: false,
    },
    QmState {
        qe: 0x375e,
        next_lps: 99,
        next_mps: 93,
        switch_mps: false,
    },
    QmState {
        qe: 0x5231,
        next_lps: 105,
        next_mps: 102,
        switch_mps: false,
    },
    QmState {
        qe: 0x4c0f,
        next_lps: 106,
        next_mps: 103,
        switch_mps: false,
    },
    QmState {
        qe: 0x4639,
        next_lps: 107,
        next_mps: 104,
        switch_mps: false,
    },
    QmState {
        qe: 0x415e,
        next_lps: 103,
        next_mps: 99,
        switch_mps: false,
    },
    QmState {
        qe: 0x5627,
        next_lps: 105,
        next_mps: 106,
        switch_mps: true,
    },
    QmState {
        qe: 0x50e7,
        next_lps: 108,
        next_mps: 107,
        switch_mps: false,
    },
    QmState {
        qe: 0x4b85,
        next_lps: 109,
        next_mps: 103,
        switch_mps: false,
    },
    QmState {
        qe: 0x5597,
        next_lps: 110,
        next_mps: 109,
        switch_mps: false,
    },
    QmState {
        qe: 0x504f,
        next_lps: 111,
        next_mps: 107,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a10,
        next_lps: 110,
        next_mps: 111,
        switch_mps: true,
    },
    QmState {
        qe: 0x5522,
        next_lps: 112,
        next_mps: 109,
        switch_mps: false,
    },
    QmState {
        qe: 0x59eb,
        next_lps: 112,
        next_mps: 111,
        switch_mps: true,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
    QmState {
        qe: 0x5a1d,
        next_lps: 113,
        next_mps: 113,
        switch_mps: false,
    },
];

/// The state machine row for a statistics bin.
#[inline(always)]
pub(crate) fn state_of(bin: Bin) -> &'static QmState {
    // Seven bits of index into a 128-row table: in range by construction.
    &QM_STATES[(bin & 0x7F) as usize]
}

/// The MPS value a bin currently predicts.
#[inline]
pub(crate) fn mps_of(bin: Bin) -> u8 {
    bin >> 7
}

/// The bin that follows an MPS renormalisation.
#[inline]
pub(crate) fn after_mps(bin: Bin, state: &QmState) -> Bin {
    (bin & 0x80) ^ state.next_mps
}

/// The bin that follows an LPS renormalisation, including the MPS/LPS
/// exchange when the state machine calls for it.
#[inline]
pub(crate) fn after_lps(bin: Bin, state: &QmState) -> Bin {
    let switch = if state.switch_mps { 0x80 } else { 0 };
    (bin & 0x80) ^ switch ^ state.next_lps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_one_row_per_state_and_is_padded_to_128() {
        assert_eq!(QM_STATES.len(), 128);
        // Rows 114..128 are padding and must all be the fixed estimate, so
        // that a corrupt bin index degrades to "no information" rather than
        // to some arbitrary probability.
        for row in &QM_STATES[114..] {
            assert_eq!(row, &QM_STATES[usize::from(FIXED_INDEX)]);
        }
    }

    /// Spot values from T.81 Table D.3, at both ends and at the three
    /// "switch" rows in the middle.
    #[test]
    fn spot_values_match_the_standard() {
        assert_eq!(
            QM_STATES[0],
            QmState {
                qe: 0x5a1d,
                next_lps: 1,
                next_mps: 1,
                switch_mps: true
            }
        );
        assert_eq!(
            QM_STATES[13],
            QmState {
                qe: 0x0001,
                next_lps: 12,
                next_mps: 13,
                switch_mps: false
            }
        );
        assert_eq!(
            QM_STATES[14],
            QmState {
                qe: 0x5a7f,
                next_lps: 15,
                next_mps: 15,
                switch_mps: true
            }
        );
        assert_eq!(
            QM_STATES[87],
            QmState {
                qe: 0x2516,
                next_lps: 86,
                next_mps: 71,
                switch_mps: false
            }
        );
        assert_eq!(
            QM_STATES[112],
            QmState {
                qe: 0x59eb,
                next_lps: 112,
                next_mps: 111,
                switch_mps: true
            }
        );
    }

    /// Structural invariants the decoder relies on: every `Qe` is a proper
    /// sub-interval of the normalised 0x8000..=0x10000 interval, and every
    /// successor index exists.
    #[test]
    fn every_row_is_structurally_sound() {
        for (index, state) in QM_STATES.iter().enumerate() {
            assert!(state.qe < 0x8000, "row {index} has Qe >= 0x8000");
            assert!(state.qe > 0, "row {index} has Qe == 0");
            assert!((state.next_lps as usize) < QM_STATES.len(), "row {index}");
            assert!((state.next_mps as usize) < QM_STATES.len(), "row {index}");
        }
    }

    /// The fixed estimate must be a fixed point of the state machine, or
    /// `decode_fixed`/`encode_fixed` could not use a temporary bin.
    #[test]
    fn the_fixed_estimate_is_its_own_successor() {
        let state = state_of(FIXED_INDEX);
        assert_eq!(state.next_mps, FIXED_INDEX);
        assert_eq!(state.next_lps, FIXED_INDEX);
        assert!(!state.switch_mps);
        assert_eq!(after_mps(FIXED_INDEX, state), FIXED_INDEX);
        assert_eq!(after_lps(FIXED_INDEX, state), FIXED_INDEX);
    }

    #[test]
    fn the_switch_flag_exchanges_the_mps_meaning() {
        // Row 0 switches; row 1 does not.
        let switching = state_of(0);
        assert_eq!(after_lps(0x00, switching), 0x81);
        assert_eq!(after_lps(0x80, switching), 0x01);
        let plain = state_of(1);
        assert_eq!(after_lps(0x81, plain), 0x8E);
        assert_eq!(after_mps(0x81, plain), 0x82);
    }

    #[test]
    fn a_corrupt_bin_index_lands_on_the_fixed_estimate() {
        assert_eq!(state_of(0x7F), state_of(FIXED_INDEX));
        assert_eq!(mps_of(0x80), 1);
        assert_eq!(mps_of(0x7F), 0);
    }
}

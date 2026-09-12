//! The QM arithmetic decoding procedure (ITU-T T.81 Annex D.2) and the byte
//! source that feeds it.
//!
//! Entropy-coded bytes reach the decoder with `0xFF 0x00` stuffing in place,
//! exactly as in a Huffman scan, but the end of the segment behaves
//! differently: T.81 D.2.6 lets the decoder run past the last real byte, and
//! hitting a marker there is legal rather than a truncation. The convention —
//! libjpeg's, and the one every conforming encoder is written against — is to
//! supply zero bytes from the marker onwards until the scan's units are all
//! decoded.
//!
//! The `C` register keeps the code base and the bit buffer in one word with a
//! *floating* cut point tracked by `ct`, which is libjpeg's arrangement: it
//! costs one variable shift per decision and saves renormalising `C`.

use super::qm::{Bin, FIXED_INDEX, after_lps, after_mps, mps_of, state_of};

/// The arithmetic decoder over one entropy-coded segment.
pub(crate) struct ArithDecoder<'a> {
    data: &'a [u8],
    /// Index of the next byte to shift into `c`.
    pos: usize,
    /// Marker code the byte source stopped at, if any.
    marker: Option<u8>,
    /// Offset of the `0xFF` that starts the pending marker.
    marker_at: usize,
    /// `true` once the byte source can produce nothing but zeros.
    stopped: bool,
    /// Code register: interval base in the high bits, bit buffer in the low.
    c: u32,
    /// Interval size register.
    a: u32,
    /// Bit shift counter; `-16` until the first two bytes have been read.
    ct: i32,
    /// Set when a decision sequence overflowed its legal magnitude, which
    /// T.81 leaves undefined and libjpeg turns into "stop decoding".
    fault: bool,
    /// Number of zero bytes fabricated past the end of the real data.
    fabricated: u64,
}

impl<'a> ArithDecoder<'a> {
    /// A decoder positioned at the start of `data`.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            marker: None,
            marker_at: data.len(),
            stopped: false,
            c: 0,
            a: 0,
            ct: -16,
            fault: false,
            fabricated: 0,
        }
    }

    /// Byte offset just past everything the decoder consumed, i.e. where the
    /// caller should resume marker parsing.
    pub(crate) fn byte_offset(&self) -> usize {
        if self.marker.is_some() {
            self.marker_at
        } else {
            self.pos.min(self.data.len())
        }
    }

    /// Whether a decision sequence ran past its legal magnitude.
    pub(crate) fn faulted(&self) -> bool {
        self.fault
    }

    /// Record a magnitude or spectral overflow (T.81 leaves both undefined;
    /// libjpeg warns and stops).
    pub(crate) fn fail(&mut self) {
        self.fault = true;
    }

    /// How many zero bytes were fabricated past the end of the real data.
    ///
    /// Deliberately **not** a truncation signal, and deliberately not public
    /// even inside the crate: measured over the whole `cjpeg -arithmetic`
    /// oracle corpus, a *conforming* stream can need as many as 94 of them
    /// (the QM decoder keeps asking for data while it renormalises, long
    /// after the encoder's flush wrote its last useful byte). T.81 D.2.6
    /// makes that legal, so short arithmetic data decodes to noise rather
    /// than to an error — exactly as it does in libjpeg. Kept for the byte
    /// source's own tests.
    #[cfg(test)]
    pub(crate) fn fabricated_bytes(&self) -> u64 {
        self.fabricated
    }

    /// Pull one entropy byte, honouring `0xFF 0x00` stuffing and stopping at
    /// a marker (T.81 D.2.6, and the note in libjpeg's `arith_decode`).
    #[inline(always)]
    fn next_byte(&mut self) -> u8 {
        if self.stopped {
            self.fabricated += 1;
            return 0;
        }
        let Some(&byte) = self.data.get(self.pos) else {
            self.stopped = true;
            self.pos = self.data.len();
            self.fabricated += 1;
            return 0;
        };
        if byte != 0xFF {
            self.pos += 1;
            return byte;
        }
        let start = self.pos;
        let mut probe = self.pos + 1;
        while self.data.get(probe) == Some(&0xFF) {
            probe += 1;
        }
        match self.data.get(probe) {
            // A trailing `0xFF` run with no code: nothing usable is left.
            None => {
                self.stopped = true;
                self.pos = start;
                self.fabricated += 1;
                0
            }
            Some(0) => {
                self.pos = probe + 1;
                0xFF
            }
            Some(&code) => {
                self.marker = Some(code);
                self.marker_at = start;
                self.pos = start;
                self.stopped = true;
                self.fabricated += 1;
                0
            }
        }
    }

    /// Decode one binary decision against the adaptive bin `st`.
    ///
    /// This is T.81's `DECODE` (D.2.4) with the conditional exchange of
    /// D.2.5 and the renormalisation of D.2.6 folded in at the top, so that
    /// the renormalisation of one decision is paid for by the next.
    #[inline(always)]
    pub(crate) fn decode(&mut self, st: &mut Bin) -> u8 {
        // Renormalisation and data input, T.81 D.2.6.
        while self.a < 0x8000 {
            self.ct -= 1;
            if self.ct < 0 {
                let data = self.next_byte();
                self.c = (self.c << 8) | u32::from(data);
                self.ct += 8;
                if self.ct < 0 {
                    self.ct += 1;
                    if self.ct == 0 {
                        // Two initial bytes are in: the interval becomes
                        // 0x10000 once the shift below has run.
                        self.a = 0x8000;
                    }
                }
            }
            self.a <<= 1;
        }

        debug_assert!((0..=8).contains(&self.ct));
        let mut sv = *st;
        let state = state_of(sv);
        let qe = u32::from(state.qe);

        // Decode and estimation, T.81 D.2.4 and D.2.5.
        self.a -= qe;
        let temp = self.a << self.ct;
        if self.c >= temp {
            self.c -= temp;
            if self.a < qe {
                // Conditional exchange: the MPS is coded in the LPS interval.
                self.a = qe;
                *st = after_mps(sv, state);
            } else {
                self.a = qe;
                *st = after_lps(sv, state);
                sv ^= 0x80;
            }
        } else if self.a < 0x8000 {
            if self.a < qe {
                *st = after_lps(sv, state);
                sv ^= 0x80;
            } else {
                *st = after_mps(sv, state);
            }
        }
        mps_of(sv)
    }

    /// Decode one decision at the fixed 0.5 estimate (T.81 D.2.3's
    /// non-adaptive bin, libjpeg's `fixed_bin`).
    ///
    /// State 113 is a fixed point of the estimation machine, so a temporary
    /// bin is indistinguishable from a persistent one; `qm`'s
    /// `the_fixed_estimate_is_its_own_successor` test pins that.
    #[inline]
    pub(crate) fn decode_fixed(&mut self) -> u8 {
        let mut bin: Bin = FIXED_INDEX;
        let bit = self.decode(&mut bin);
        debug_assert_eq!(bin, FIXED_INDEX);
        bit
    }

    /// Position the decoder on the next marker, discarding any buffered bits.
    ///
    /// Returns the marker code, or `None` when the segment simply ended.
    pub(crate) fn seek_marker(&mut self) -> Option<u8> {
        if let Some(code) = self.marker {
            return Some(code);
        }
        let mut i = self.pos;
        while i < self.data.len() {
            if self.data[i] != 0xFF {
                i += 1;
                continue;
            }
            let start = i;
            let mut probe = i + 1;
            while self.data.get(probe) == Some(&0xFF) {
                probe += 1;
            }
            match self.data.get(probe) {
                None => break,
                Some(0) => i = probe + 1,
                Some(&code) => {
                    self.marker = Some(code);
                    self.marker_at = start;
                    self.pos = start;
                    self.stopped = true;
                    return Some(code);
                }
            }
        }
        self.pos = self.data.len();
        self.stopped = true;
        None
    }

    /// Consume a restart marker and re-initialise the coder (T.81 D.2.7).
    ///
    /// Returns `false` when the next marker is not an `RSTn`, which means the
    /// scan data ended early.
    pub(crate) fn take_restart(&mut self) -> bool {
        match self.seek_marker() {
            Some(code) if (0xD0..=0xD7).contains(&code) => {
                let mut probe = self.marker_at + 1;
                while self.data.get(probe) == Some(&0xFF) {
                    probe += 1;
                }
                self.pos = (probe + 1).min(self.data.len());
                self.marker = None;
                self.marker_at = self.data.len();
                self.stopped = false;
                self.c = 0;
                self.a = 0;
                self.ct = -16;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arith::encoder::ArithEncoder;

    /// Every decision an encoder writes must come back out of the decoder,
    /// against the same bin sequence.
    fn round_trip(bits: &[u8], bins: usize) {
        let mut encoder = ArithEncoder::new();
        let mut states = vec![0u8; bins];
        for (i, &bit) in bits.iter().enumerate() {
            encoder.encode(&mut states[i % bins], bit);
        }
        let data = encoder.finish();

        let mut decoder = ArithDecoder::new(&data);
        let mut states = vec![0u8; bins];
        for (i, &bit) in bits.iter().enumerate() {
            assert_eq!(decoder.decode(&mut states[i % bins]), bit, "bit {i}");
        }
    }

    #[test]
    fn a_single_bin_round_trips() {
        let bits: Vec<u8> = (0..500).map(|i| u8::from(i % 7 == 0)).collect();
        round_trip(&bits, 1);
    }

    #[test]
    fn many_bins_round_trip() {
        let bits: Vec<u8> = (0..4000u32)
            .map(|i| u8::from(i.wrapping_mul(2654435761) % 5 == 0))
            .collect();
        round_trip(&bits, 64);
    }

    #[test]
    fn all_zero_and_all_one_streams_round_trip() {
        round_trip(&[0u8; 1000], 1);
        round_trip(&[1u8; 1000], 1);
        round_trip(&[], 1);
    }

    #[test]
    fn the_fixed_bin_round_trips() {
        let bits: Vec<u8> = (0..300).map(|i| u8::from(i % 3 == 0)).collect();
        let mut encoder = ArithEncoder::new();
        for &bit in &bits {
            encoder.encode_fixed(bit);
        }
        let data = encoder.finish();
        let mut decoder = ArithDecoder::new(&data);
        for (i, &bit) in bits.iter().enumerate() {
            assert_eq!(decoder.decode_fixed(), bit, "bit {i}");
        }
    }

    #[test]
    fn a_marker_stops_the_byte_source_and_zeros_follow() {
        let data = [0x12u8, 0x34, 0xFF, 0xD9, 0x56];
        let mut decoder = ArithDecoder::new(&data);
        assert_eq!(decoder.next_byte(), 0x12);
        assert_eq!(decoder.next_byte(), 0x34);
        assert_eq!(decoder.next_byte(), 0);
        assert_eq!(decoder.next_byte(), 0);
        assert_eq!(decoder.seek_marker(), Some(0xD9));
        assert_eq!(decoder.byte_offset(), 2);
        assert_eq!(decoder.fabricated_bytes(), 2);
    }

    #[test]
    fn stuffing_yields_a_literal_ff() {
        let data = [0xFFu8, 0x00, 0xFF, 0xFF, 0x00, 0x07];
        let mut decoder = ArithDecoder::new(&data);
        assert_eq!(decoder.next_byte(), 0xFF);
        assert_eq!(decoder.next_byte(), 0xFF);
        assert_eq!(decoder.next_byte(), 0x07);
        assert_eq!(decoder.byte_offset(), 6);
    }

    #[test]
    fn a_restart_marker_reinitialises_the_registers() {
        let data = [0x11u8, 0xFF, 0xD3, 0x22, 0x33];
        let mut decoder = ArithDecoder::new(&data);
        assert_eq!(decoder.next_byte(), 0x11);
        assert!(decoder.take_restart());
        assert_eq!(decoder.ct, -16);
        assert_eq!(decoder.next_byte(), 0x22);

        let data = [0x11u8, 0xFF, 0xD9];
        let mut decoder = ArithDecoder::new(&data);
        assert!(!decoder.take_restart(), "EOI is not a restart");
    }

    #[test]
    fn a_trailing_ff_run_is_not_a_marker() {
        let data = [0x01u8, 0xFF, 0xFF];
        let mut decoder = ArithDecoder::new(&data);
        assert_eq!(decoder.next_byte(), 0x01);
        assert_eq!(decoder.next_byte(), 0);
        assert_eq!(decoder.seek_marker(), None);
        assert_eq!(decoder.byte_offset(), 3);
    }
}

//! The QM arithmetic encoding procedure (ITU-T T.81 Annex D.1) and the byte
//! sink that drains it.
//!
//! The sink is the interesting half. A carry out of the `C` register has to
//! propagate into bytes that were already produced, so a byte is never
//! emitted until it can no longer change: the most recent non-`0xFF` byte is
//! held in `buffer`, any run of `0xFF` bytes behind it is counted in `stacked`
//! (they become `0x00` if the carry arrives), and trailing `0x00` bytes are
//! counted in `pending_zeros` so that the "Pacman" termination of T.81 D.1.8
//! can discard them. `0xFF` bytes are stuffed with a following `0x00` on the
//! way out, exactly as in a Huffman scan.

use super::qm::{Bin, FIXED_INDEX, after_lps, after_mps, mps_of, state_of};

/// The arithmetic encoder for one entropy-coded segment.
pub(crate) struct ArithEncoder {
    out: Vec<u8>,
    /// Code register.
    c: u32,
    /// Interval size register.
    a: u32,
    /// Count of stacked `0xFF` bytes that a carry would turn into `0x00`.
    stacked: u64,
    /// Count of `0x00` bytes held back for the "Pacman" termination.
    pending_zeros: u64,
    /// Bit shift counter: bits still to accumulate before the next byte.
    ct: i32,
    /// The most recent byte that has not been committed, or `None` when the
    /// encoder has not produced one yet.
    buffer: Option<u8>,
}

impl Default for ArithEncoder {
    fn default() -> Self {
        Self::new()
    }
}

impl ArithEncoder {
    /// A fresh encoder, initialised per T.81 D.1.7 (`INITENC`).
    pub(crate) fn new() -> Self {
        Self {
            out: Vec::new(),
            c: 0,
            a: 0x10000,
            stacked: 0,
            pending_zeros: 0,
            ct: 11,
            buffer: None,
        }
    }

    /// Bytes produced so far.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.out
    }

    /// Emit one byte, stuffing a `0x00` after `0xFF` as T.81 B.1.1.5 requires.
    #[inline]
    fn emit(&mut self, byte: u8) {
        self.out.push(byte);
        if byte == 0xFF {
            self.out.push(0x00);
        }
    }

    /// Emit a raw byte with no stuffing (used for the `RSTn` marker itself).
    #[inline]
    fn emit_raw(&mut self, byte: u8) {
        self.out.push(byte);
    }

    /// Flush the `0x00` bytes that were held back for the Pacman rule.
    #[inline]
    fn flush_pending_zeros(&mut self) {
        for _ in 0..self.pending_zeros {
            self.out.push(0x00);
        }
        self.pending_zeros = 0;
    }

    /// Propagate a carry into the held-back bytes and start a new run.
    #[inline]
    fn carry_over(&mut self) {
        if let Some(byte) = self.buffer {
            self.flush_pending_zeros();
            // `byte` cannot be 0xFF here: a 0xFF byte is stacked, never
            // buffered, so `byte + 1` cannot wrap.
            let carried = byte.wrapping_add(1);
            self.emit(carried);
        }
        // The carry turns every stacked 0xFF into 0x00, and those zeros are
        // themselves discardable at the end of the segment.
        self.pending_zeros += self.stacked;
        self.stacked = 0;
    }

    /// Commit the held byte and any stacked `0xFF` run, no carry pending.
    #[inline]
    fn commit_run(&mut self) {
        match self.buffer {
            Some(0) => self.pending_zeros += 1,
            Some(byte) => {
                self.flush_pending_zeros();
                self.emit(byte);
            }
            None => {}
        }
        if self.stacked > 0 {
            self.flush_pending_zeros();
            for _ in 0..self.stacked {
                self.emit(0xFF);
            }
            self.stacked = 0;
        }
    }

    /// Move one completed byte out of the `C` register (T.81 D.1.6).
    #[inline]
    fn byte_out(&mut self) {
        let temp = self.c >> 19;
        if temp > 0xFF {
            self.carry_over();
            // The three spacer bits in `C` guarantee the new byte is not
            // 0xFF here (Pennebaker & Mitchell, page 160).
            self.buffer = Some((temp & 0xFF) as u8);
        } else if temp == 0xFF {
            self.stacked += 1;
        } else {
            self.commit_run();
            self.buffer = Some((temp & 0xFF) as u8);
        }
        self.c &= 0x7FFFF;
        self.ct += 8;
    }

    /// Encode one binary decision against the adaptive bin `st`.
    ///
    /// T.81 D.1.4 (`CODE`) with the conditional exchange of D.1.5 and the
    /// renormalisation of D.1.6.
    #[inline(always)]
    pub(crate) fn encode(&mut self, st: &mut Bin, bit: u8) {
        let sv = *st;
        let state = state_of(sv);
        let qe = u32::from(state.qe);

        self.a -= qe;
        if bit != mps_of(sv) {
            // Less probable symbol; exchange the sub-intervals when the LPS
            // interval is the larger of the two.
            if self.a >= qe {
                self.c += self.a;
                self.a = qe;
            }
            *st = after_lps(sv, state);
        } else {
            if self.a >= 0x8000 {
                return;
            }
            if self.a < qe {
                self.c += self.a;
                self.a = qe;
            }
            *st = after_mps(sv, state);
        }

        loop {
            self.a <<= 1;
            self.c <<= 1;
            self.ct -= 1;
            if self.ct == 0 {
                self.byte_out();
            }
            if self.a >= 0x8000 {
                break;
            }
        }
    }

    /// Encode one decision at the fixed 0.5 estimate.
    ///
    /// State 113 is a fixed point of the estimation machine, so a temporary
    /// bin behaves exactly like libjpeg's persistent `fixed_bin`.
    #[inline]
    pub(crate) fn encode_fixed(&mut self, bit: u8) {
        let mut bin: Bin = FIXED_INDEX;
        self.encode(&mut bin, bit);
        debug_assert_eq!(bin, FIXED_INDEX);
    }

    /// Terminate the code string (T.81 D.1.8, `FLUSH`), leaving the encoder
    /// ready to be re-initialised.
    fn flush(&mut self) {
        // Clear the final bits of C, choosing the value in the interval with
        // the most trailing zeros.
        let temp = (self.a.wrapping_sub(1).wrapping_add(self.c)) & 0xFFFF_0000;
        if temp < self.c {
            self.c = temp.wrapping_add(0x8000);
        } else {
            self.c = temp;
        }
        self.c <<= self.ct;
        if self.c & 0xF800_0000 != 0 {
            self.carry_over();
        } else {
            self.commit_run();
        }
        // Discard_final_zeros: only bytes that carry information are written.
        if self.c & 0x07FF_F800 != 0 {
            self.flush_pending_zeros();
            self.emit(((self.c >> 19) & 0xFF) as u8);
            if self.c & 0x0007_F800 != 0 {
                self.emit(((self.c >> 11) & 0xFF) as u8);
            }
        }
        self.pending_zeros = 0;
        self.buffer = None;
        self.stacked = 0;
    }

    /// Re-initialise the coder registers, as T.81 D.1.7 requires after a
    /// restart marker.
    fn reinit(&mut self) {
        self.c = 0;
        self.a = 0x10000;
        self.stacked = 0;
        self.pending_zeros = 0;
        self.ct = 11;
        self.buffer = None;
    }

    /// Terminate the current interval, write `RSTn`, and start a new one.
    pub(crate) fn restart(&mut self, index: u8) {
        self.flush();
        self.emit_raw(0xFF);
        self.emit_raw(0xD0 | (index & 7));
        self.reinit();
    }

    /// Terminate the segment and return its bytes.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arith::decoder::ArithDecoder;

    #[test]
    fn an_empty_segment_produces_no_bytes() {
        assert!(ArithEncoder::new().finish().is_empty());
    }

    /// Every `0xFF` an arithmetic scan emits must be followed by a stuffed
    /// zero, or a decoder would read it as a marker.
    #[test]
    fn every_ff_byte_is_stuffed() {
        // Drive the coder hard enough that 0xFF bytes appear.
        let mut encoder = ArithEncoder::new();
        let mut bins = [0u8; 8];
        for i in 0..20000u32 {
            let bit = u8::from(i.wrapping_mul(2654435761) >> 28 == 0);
            encoder.encode(&mut bins[(i % 8) as usize], bit);
        }
        let data = encoder.finish();
        let ffs = data.iter().filter(|&&b| b == 0xFF).count();
        assert!(ffs > 0, "the fixture did not exercise 0xFF output");
        for (i, window) in data.windows(2).enumerate() {
            if window[0] == 0xFF {
                assert_eq!(window[1], 0x00, "unstuffed 0xFF at {i}");
            }
        }
        assert_ne!(data.last(), Some(&0xFF), "a trailing 0xFF is a marker");
    }

    #[test]
    fn restarts_split_the_stream_and_reset_the_coder() {
        let mut encoder = ArithEncoder::new();
        let mut bin = 0u8;
        for _ in 0..100 {
            encoder.encode(&mut bin, 1);
        }
        encoder.restart(0);
        let after_first = encoder.bytes().len();
        let mut bin = 0u8;
        for _ in 0..100 {
            encoder.encode(&mut bin, 1);
        }
        let data = encoder.finish();
        assert_eq!(data[after_first - 2], 0xFF);
        assert_eq!(data[after_first - 1], 0xD0);

        // Both halves decode independently, which is what a restart is for.
        let mut decoder = ArithDecoder::new(&data);
        let mut bin = 0u8;
        for _ in 0..100 {
            assert_eq!(decoder.decode(&mut bin), 1);
        }
        assert!(decoder.take_restart());
        let mut bin = 0u8;
        for _ in 0..100 {
            assert_eq!(decoder.decode(&mut bin), 1);
        }
    }
}

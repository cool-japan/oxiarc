//! Level 0: stored (uncompressed) blocks.
//!
//! A port of zlib's `deflate_stored()` for the case where the output sink is
//! unbounded (so the "not enough room in `avail_out`" branches never fire).
//! The window is still maintained, because the level and strategy can change
//! between calls and because a sync flush must know how much of the window is
//! still unwritten.

use super::config::{PENDING_BUF_SIZE, W_SIZE};
use super::tables::MAX_STORED;
use super::window::WINDOW_SIZE;
use super::{DeflateEncoder, Flush};

/// Emit `payload` as one stored block.
fn emit_stored(enc: &mut DeflateEncoder, payload_len: usize, last: bool) {
    enc.sink.send_bits(u32::from(last), 3);
    enc.sink.bi_windup();
    enc.sink.put_short(payload_len as u16);
    enc.sink.put_short(!(payload_len as u16));
}

/// Run the stored-block strategy; returns `true` when the final block was
/// written.
pub(crate) fn run(enc: &mut DeflateEncoder, input: &[u8], pos: &mut usize, flush: Flush) -> bool {
    let min_block = (PENDING_BUF_SIZE - 5).min(W_SIZE);
    let mut last = false;
    let start_pos = *pos;

    // Copy whole stored blocks straight from the window and the input.
    loop {
        let left = (enc.win.strstart as i64 - enc.block_start).max(0) as usize;
        let avail_in = input.len() - *pos;
        let total = left + avail_in;
        let len = MAX_STORED.min(total);

        if len < min_block
            && ((len == 0 && flush != Flush::Finish) || flush == Flush::None || len != total)
        {
            break;
        }

        last = flush == Flush::Finish && len == total;
        emit_stored(enc, len, last);

        let mut remaining = len;
        if left > 0 {
            let take = left.min(remaining);
            let bs = enc.block_start.max(0) as usize;
            enc.sink.out.extend_from_slice(&enc.win.buf[bs..bs + take]);
            enc.block_start += take as i64;
            remaining -= take;
        }
        if remaining > 0 {
            enc.sink
                .out
                .extend_from_slice(&input[*pos..*pos + remaining]);
            *pos += remaining;
        }
        if last {
            break;
        }
    }

    // Mirror the directly-copied bytes into the window so later levels (and
    // the pending-block path below) still see the history.
    let used = *pos - start_pos;
    if used > 0 {
        if used >= W_SIZE {
            enc.win.buf[..W_SIZE].copy_from_slice(&input[*pos - W_SIZE..*pos]);
            enc.win.strstart = W_SIZE;
            enc.win.insert = enc.win.strstart;
        } else {
            if WINDOW_SIZE - enc.win.strstart <= used {
                enc.win.strstart -= W_SIZE;
                let live = enc.win.strstart;
                enc.win.buf.copy_within(W_SIZE..W_SIZE + live, 0);
                if enc.win.insert > enc.win.strstart {
                    enc.win.insert = enc.win.strstart;
                }
            }
            let ss = enc.win.strstart;
            enc.win.buf[ss..ss + used].copy_from_slice(&input[*pos - used..*pos]);
            enc.win.strstart += used;
            let room = W_SIZE - enc.win.insert;
            enc.win.insert += used.min(room);
        }
        enc.block_start = enc.win.strstart as i64;
    }
    enc.win.raise_high_water();

    if last {
        return true;
    }
    if flush != Flush::None
        && flush != Flush::Finish
        && *pos >= input.len()
        && enc.win.strstart as i64 == enc.block_start
    {
        return false;
    }

    // Buffer whatever input is left in the window.
    let mut have = WINDOW_SIZE - enc.win.strstart;
    let avail_in = input.len() - *pos;
    if avail_in > have && enc.block_start >= W_SIZE as i64 {
        enc.block_start -= W_SIZE as i64;
        enc.win.strstart -= W_SIZE;
        let live = enc.win.strstart;
        enc.win.buf.copy_within(W_SIZE..W_SIZE + live, 0);
        have += W_SIZE;
        if enc.win.insert > enc.win.strstart {
            enc.win.insert = enc.win.strstart;
        }
    }
    let have = have.min(avail_in);
    if have > 0 {
        let ss = enc.win.strstart;
        enc.win.buf[ss..ss + have].copy_from_slice(&input[*pos..*pos + have]);
        enc.win.strstart += have;
        *pos += have;
        let room = W_SIZE - enc.win.insert;
        enc.win.insert += have.min(room);
    }
    enc.win.raise_high_water();

    // Write a stored block out of the window if one is worth writing.
    let header = (enc.sink.bit_valid as usize + 42) >> 3;
    let room = PENDING_BUF_SIZE.saturating_sub(header).min(MAX_STORED);
    let min_block = room.min(W_SIZE);
    let left = (enc.win.strstart as i64 - enc.block_start).max(0) as usize;
    let avail_in = input.len() - *pos;
    if left >= min_block
        || ((left > 0 || flush == Flush::Finish)
            && flush != Flush::None
            && avail_in == 0
            && left <= room)
    {
        let len = left.min(room);
        last = flush == Flush::Finish && avail_in == 0 && len == left;
        let bs = enc.block_start.max(0) as usize;
        emit_stored(enc, len, last);
        let payload: &[u8] = &enc.win.buf[bs..bs + len];
        enc.sink.out.extend_from_slice(payload);
        enc.block_start += len as i64;
    }

    last
}

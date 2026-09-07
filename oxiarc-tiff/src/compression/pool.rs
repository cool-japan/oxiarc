//! A pool of reusable per-chunk decoders.
//!
//! Four codecs keep scratch alive between the chunks of one image: the
//! inflate machine and its 32 KiB window, the fax changing-element buffers,
//! the [`ZstdStream`](oxiarc_zstd::ZstdStream) and the
//! [`XzDecoder`](oxiarc_lzma::xz::XzDecoder). All four hold it here rather
//! than in a single `Mutex<Option<T>>` slot, because a slot has to keep its
//! lock for as long as the decode runs — which is correct, but turns a
//! [`rayon`](crate::rayon_support) decode of such a page into a serial one
//! behind the codec's mutex. A pool locks only while an entry is taken out
//! and put back, so every worker decodes through a decoder of its own.
//!
//! Nothing crosses between chunks except the *allocations*: each codec resets
//! (or is documented not to need a reset) before it decodes, so which entry a
//! chunk happens to get cannot change what it decodes to.
//!
//! A serial decode takes and returns the same entry every time, which is
//! exactly the single-slot behaviour this replaced.

use std::sync::{Mutex, PoisonError};

/// The most idle decoders one image keeps.
///
/// A pool only ever grows to the number of chunks being decoded at the same
/// instant, so a serial decode holds exactly one and a `rayon` decode holds
/// one per busy worker. The cap bounds a machine with more cores than this;
/// past it a worker simply builds its own decoder, which is what every worker
/// did before the pools existed.
const MAX_POOLED: usize = 32;

/// Idle per-chunk decoders (or scratch buffers) of one codec.
pub(crate) struct Pool<T>(Mutex<Vec<T>>);

impl<T> Default for Pool<T> {
    fn default() -> Self {
        Self(Mutex::new(Vec::new()))
    }
}

impl<T> Pool<T> {
    /// One idle entry, if the pool holds any.
    pub(crate) fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).pop()
    }

    /// Returns an entry to the pool, up to [`MAX_POOLED`].
    ///
    /// Past the cap the entry is dropped: a machine running more than
    /// [`MAX_POOLED`] workers at once builds decoders instead of queueing.
    pub(crate) fn put(&self, entry: T) {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.len() < MAX_POOLED {
            guard.push(entry);
        }
    }

    /// Runs `body` on the idle entries, for a codec's `Debug` impl.
    pub(crate) fn inspect<R>(&self, body: impl FnOnce(&[T]) -> R) -> R {
        match self.0.lock() {
            Ok(guard) => body(&guard),
            Err(poisoned) => body(&poisoned.into_inner()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_pool_hands_out_nothing() {
        let pool = Pool::<u32>::default();
        assert_eq!(pool.take(), None);
        assert_eq!(pool.inspect(<[u32]>::len), 0);
    }

    #[test]
    fn an_entry_comes_back_out_again() {
        let pool = Pool::default();
        pool.put(7u32);
        assert_eq!(pool.inspect(<[u32]>::len), 1);
        assert_eq!(pool.take(), Some(7));
        assert_eq!(pool.take(), None);
    }

    #[test]
    fn the_pool_stops_growing_at_the_cap() {
        let pool = Pool::default();
        for value in 0..(MAX_POOLED as u32 + 8) {
            pool.put(value);
        }
        assert_eq!(pool.inspect(<[u32]>::len), MAX_POOLED);
    }

    #[test]
    fn two_workers_never_hold_the_same_entry() {
        // The property the pool exists for: a taken entry is owned, so a
        // second taker cannot see it until it is put back.
        let pool = Pool::default();
        pool.put(1u32);
        let first = pool.take();
        assert_eq!(first, Some(1));
        assert_eq!(pool.take(), None, "the entry is out on loan");
        pool.put(first.unwrap_or_default());
        assert_eq!(pool.take(), Some(1));
    }
}

//! LZW code table (dictionary) management.
//!
//! The table is stored in the classical *prefix/suffix* form used by
//! libtiff, `compress(1)` and every other production LZW implementation:
//! every code `c` above the single-byte range denotes the string
//!
//! ```text
//! string(c) = string(prefix[c]) ++ suffix[c]
//! ```
//!
//! so an entry costs a fixed eight bytes instead of an owned `Vec<u8>`, and
//! expanding a code writes its bytes straight into the caller's output
//! buffer *backwards* from the end of the string.
//!
//! # Why one array of structs
//!
//! Until 0.4.2 the four per-code fields lived in four parallel `Vec`s plus a
//! fifth `Vec<u32>` of output offsets. Decoding one code then touched five
//! or six independent cache lines (twice: once for the entry being created,
//! once for the entry being emitted), and walking a prefix chain cost two
//! bounds-checked loads from *different* arrays per output byte. libtiff
//! keeps a single array of 16-byte `code_t` records so that one load brings
//! in `next`, `length`, `firstchar` and `value` together; [`CodeEntry`] is
//! the same idea in 8 bytes, which is what closed most of the measured
//! throughput gap against `LZWDecode` (see `examples/lzw_vs_libtiff.rs`).
//!
//! Table lengths are always a power of two (`1 << max_bits`), so the hot
//! loops index with `code as usize & (len - 1)`: the mask is an identity for
//! every code the decoder accepts (validation rejects `code > next_code`
//! before any table access) and it lets the optimiser drop the bounds check
//! from the per-byte chain walk.
//!
//! The same table backs the encoder and the decoder. The encoder
//! additionally keeps an [`LzwCodeIndex`] — an open-addressed
//! `(prefix, byte) -> code` map — which replaces the old string-keyed
//! `HashMap` lookup.

use crate::config::LzwConfig;
use crate::error::{LzwError, Result};

/// One code-table entry, packed into a single `u64`.
///
/// `string(code) == string(prefix) ++ suffix`, `first` is the string's first
/// byte and `length` its byte length (0 for the reserved ClearCode/EOI
/// slots and for codes that have not been assigned yet).
///
/// ```text
///  bit  63           49 48    47      40 39     32 31         16 15          0
///      +---------------+------+---------+--------+-------------+-------------+
///      |   (unused)    | rep. |  suffix |  first |   length    |   prefix    |
///      +---------------+------+---------+--------+-------------+-------------+
/// ```
///
/// # Why packed by hand
///
/// The obvious form — a five-field `struct` — is also eight bytes, but the
/// optimiser reads and writes it *field by field*: the emitted arm64 for
/// the decode loop had four loads (`ldrh`, three `ldrb`) for the entry
/// being emitted and five stores for the entry being created, on every
/// single code. As one `u64` it is one `ldr` and one `str`, with the fields
/// extracted by `ubfx` and assembled by shifts — register work the loop has
/// spare capacity for. Creating an entry also becomes almost free: the new
/// entry keeps the parent's `first` and needs `parent.length + 1`, which is
/// a single add of `1 << 16` to the parent's word before the other fields
/// are masked in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CodeEntry(u64);

impl CodeEntry {
    /// Increment of the packed `length` field.
    const LENGTH_ONE: u64 = 1 << 16;
    /// The `length` and `first` fields, which a child entry inherits (its
    /// length incremented, its first byte unchanged).
    const INHERITED: u64 = 0x0000_00FF_FFFF_0000;

    /// Assemble an entry from its fields.
    #[inline]
    pub(crate) fn new(prefix: u16, length: u16, first: u8, suffix: u8, repeated: bool) -> Self {
        Self(
            u64::from(prefix)
                | (u64::from(length) << 16)
                | (u64::from(first) << 32)
                | (u64::from(suffix) << 40)
                | (u64::from(repeated) << 48),
        )
    }

    /// Parent code (meaningful only for learned entries).
    #[inline(always)]
    pub(crate) fn prefix(self) -> u16 {
        self.0 as u16
    }

    /// Length in bytes of this entry's string.
    #[inline(always)]
    pub(crate) fn length(self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// First byte of this entry's string.
    #[inline(always)]
    pub(crate) fn first(self) -> u8 {
        (self.0 >> 32) as u8
    }

    /// Last byte of this entry's string.
    #[inline(always)]
    pub(crate) fn suffix(self) -> u8 {
        (self.0 >> 40) as u8
    }

    /// `true` when every byte of the string equals [`Self::suffix`], which
    /// lets the decoder emit it with a fill instead of a chain walk. This
    /// is libtiff's `code_t::repeated`, and it is what makes flat image
    /// regions (long runs of one byte) as cheap as a `memset`.
    #[inline(always)]
    pub(crate) fn repeated(self) -> bool {
        (self.0 >> 48) & 1 != 0
    }
}

/// Allocation cursor and current code width — the only table state a decode
/// or encode run mutates besides the entries themselves.
///
/// Kept in its own struct so the hot loop can hold `&mut [CodeEntry]` and
/// `&mut TableCursor` at the same time (disjoint field borrows) and index
/// the entries through a fixed-length slice.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableCursor {
    /// Next code slot to be assigned.
    ///
    /// Held as a `u32` rather than a `u16` because a 16-bit configuration's
    /// table is exhausted at `next_code == 65536`, which a `u16` cannot
    /// represent: `is_full()` would never fire and `next_code += 1` would
    /// overflow.
    pub(crate) next_code: u32,
    /// Current code width in bits.
    pub(crate) current_bits: u8,
}

/// The code values and width rules derived from an [`LzwConfig`], hoisted
/// out of the config so the hot loop reads plain `Copy` scalars.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TableLimits {
    /// Code that resets the table.
    pub(crate) clear_code: u16,
    /// Code that ends the stream.
    pub(crate) eoi_code: u16,
    /// First learned code (`eoi_code + 1`).
    pub(crate) first_code: u16,
    /// Largest assignable code (`(1 << max_bits) - 1`).
    pub(crate) max_code: u16,
    /// Initial code width.
    pub(crate) min_bits: u8,
    /// Maximum code width.
    pub(crate) max_bits: u8,
    /// TIFF's early code-width change (grow one code earlier).
    pub(crate) early_change: bool,
}

impl TableCursor {
    /// Account for a freshly stored entry using the **encoder**'s width
    /// rule.
    ///
    /// TIFF uses "early change": the width increases when the next code
    /// equals `2^current_bits` (one code earlier than standard LZW).
    #[inline]
    pub(crate) fn advance_width_encode(&mut self, limits: &TableLimits) {
        if self.current_bits < limits.max_bits {
            let threshold: u32 = if limits.early_change {
                1 << self.current_bits
            } else {
                (1 << self.current_bits) + 1
            };
            if self.next_code >= threshold {
                self.current_bits += 1;
            }
        }
    }
}

/// LZW code table shared by the encoder and the decoder.
///
/// Codes below [`LzwConfig::clear_code`] are the single-byte roots; the two
/// codes at `clear_code` / `eoi_code` are reserved placeholders (length 0)
/// and are never emitted; codes from [`LzwConfig::first_code`] upwards are
/// the learned entries.
#[derive(Debug)]
pub struct LzwDictionary {
    /// One slot per code, `1 << max_bits` slots (always a power of two).
    entries: Vec<CodeEntry>,
    /// Allocation cursor and code width.
    cursor: TableCursor,
    /// Code values and width rules for this dialect.
    limits: TableLimits,
    /// Configuration this table was built from.
    config: LzwConfig,
}

impl LzwDictionary {
    /// Create a new LZW code table with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] when `config` fails
    /// [`LzwConfig::validate`].
    pub fn new(config: LzwConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self::build(config))
    }

    /// Create a table for a dialect whose initial width is below the
    /// `min_bits >= 9` floor [`LzwConfig::validate`] enforces.
    ///
    /// GIF derives its clear code, EOI code and initial width from the image
    /// descriptor's `minimum_code_size` (2..=11), so its `min_bits` is
    /// `minimum_code_size + 1` — as low as 3. Every value the table computes
    /// from `min_bits` (clear/EOI/first code, the initial width) is sound in
    /// that range; only the public configuration *policy* rejects it, so
    /// [`crate::gif_decompress`] uses this constructor instead.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] unless
    /// `2 <= min_bits <= max_bits <= LzwConfig::MAX_SUPPORTED_BITS`.
    pub(crate) fn with_small_min_bits(config: LzwConfig) -> Result<Self> {
        if config.min_bits < 2 || config.min_bits > config.max_bits {
            return Err(LzwError::InvalidBitWidth(config.min_bits));
        }
        if config.max_bits > LzwConfig::MAX_SUPPORTED_BITS {
            return Err(LzwError::InvalidBitWidth(config.max_bits));
        }
        Ok(Self::build(config))
    }

    /// Allocate the entry array and write the immutable root entries.
    fn build(config: LzwConfig) -> Self {
        let limits = TableLimits {
            clear_code: config.clear_code(),
            eoi_code: config.eoi_code(),
            first_code: config.first_code(),
            max_code: config.max_code(),
            min_bits: config.min_bits,
            max_bits: config.max_bits,
            early_change: config.early_change,
        };
        let capacity = limits.max_code as usize + 1;
        let mut dict = Self {
            entries: vec![CodeEntry::default(); capacity],
            cursor: TableCursor {
                next_code: 0,
                current_bits: config.min_bits,
            },
            limits,
            config,
        };

        // The root entries never change, so they are written once here and
        // left alone by `reset()` — that is what makes a table reset O(1).
        for (code, slot) in dict
            .entries
            .iter_mut()
            .enumerate()
            .take(limits.clear_code as usize)
        {
            let byte = code as u8;
            *slot = CodeEntry::new(0, 1, byte, byte, true);
        }

        dict.reset();
        dict
    }

    /// Reset the table to its initial state.
    ///
    /// Only the allocation cursor and the code width are rolled back: the
    /// root entries are immutable and stale entries above `next_code` are
    /// unreachable, so no memory has to be touched.
    pub fn reset(&mut self) {
        self.cursor.current_bits = self.limits.min_bits;
        self.cursor.next_code = u32::from(self.limits.first_code);
    }

    /// Number of code slots in the table (`max_code + 1`, a power of two).
    pub fn capacity(&self) -> usize {
        self.entries.len()
    }

    /// The pieces a decode or encode run mutates, borrowed disjointly.
    ///
    /// Returning the entries as a `&mut [CodeEntry]` (rather than through
    /// `&mut self` accessors) is what lets the hot loop mask its indices
    /// against a length the optimiser can see, so the per-output-byte chain
    /// walk carries no bounds check.
    pub(crate) fn parts(&mut self) -> (&mut [CodeEntry], &mut TableCursor, TableLimits) {
        (&mut self.entries, &mut self.cursor, self.limits)
    }

    /// Add an entry for the encoder: `string(prefix) ++ byte`.
    ///
    /// Returns the assigned code.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::TableFull`] when the table has no free slot.
    pub fn add_entry_encode(&mut self, prefix: u16, byte: u8) -> Result<u16> {
        if self.cursor.next_code > u32::from(self.limits.max_code) {
            return Err(LzwError::TableFull {
                max_codes: self.limits.max_code,
            });
        }
        let mask = self.entries.len().wrapping_sub(1);
        let code = self.cursor.next_code as u16;
        let index = code as usize & mask;
        let parent_index = prefix as usize & mask;
        if parent_index >= self.entries.len() || index >= self.entries.len() {
            return Err(LzwError::InvalidCode(prefix));
        }
        let parent = self.entries[parent_index];
        self.entries[index] = learn_entry(prefix, parent, byte);
        self.cursor.next_code += 1;
        // Encoder rule: the width grows one entry later than the decoder's.
        self.cursor.advance_width_encode(&self.limits);
        Ok(code)
    }

    /// Account for the phantom table entry the decoder creates while
    /// processing the encoder's final data code.
    ///
    /// The decoder adds one table entry for every code it reads after the
    /// first, including the *last* data code — but the encoder has no
    /// following input byte at that point, so it never performs a matching
    /// `add_entry_encode`. Without compensation the decoder can cross a
    /// bit-width threshold just before reading EOI while the encoder writes
    /// EOI at the old width, desynchronizing the stream at exact boundary
    /// sizes.
    ///
    /// libtiff's `LZWPostEncode` increments `free_ent` (without storing an
    /// entry) for exactly this reason; this method mirrors it. Call it after
    /// emitting the final data code and before emitting EOI.
    pub fn note_final_code(&mut self) {
        if !self.is_full() {
            self.cursor.next_code += 1;
            self.cursor.advance_width_encode(&self.limits);
        }
    }

    /// The entry for `code` (zeroed when `code` has not been assigned).
    ///
    /// A diagnostic accessor: the decode and encode loops read the entry
    /// array directly (see [`Self::parts`]) so that the optimiser can see
    /// the slice length and drop the per-byte bounds check.
    #[cfg(test)]
    #[inline]
    pub(crate) fn entry(&self, code: u16) -> CodeEntry {
        let mask = self.entries.len().wrapping_sub(1);
        self.entries
            .get(code as usize & mask)
            .copied()
            .unwrap_or_default()
    }

    /// Length in bytes of the string denoted by `code`.
    ///
    /// Returns 0 for the reserved ClearCode/EOI slots and for codes that
    /// have not been assigned yet.
    #[cfg(test)]
    #[inline]
    pub fn entry_len(&self, code: u16) -> u16 {
        self.entry(code).length()
    }

    /// First byte of the string denoted by `code`.
    #[cfg(test)]
    #[inline]
    pub fn first_byte(&self, code: u16) -> u8 {
        self.entry(code).first()
    }

    /// Expand `code` into `dst`, writing its bytes backwards from the end.
    ///
    /// `dst.len()` must equal the number of bytes to write, which may be
    /// **fewer** than `entry_len(code)`: the trailing bytes of the string
    /// are then dropped so that `dst` receives the string's first
    /// `dst.len()` bytes. That is what a decoder does when the last code of
    /// a strip expands past the end of the caller's buffer (libtiff's
    /// `LZWDecode` does the same).
    #[cfg(test)]
    #[inline]
    pub fn expand(&self, code: u16, dst: &mut [u8]) {
        let entry = self.entry(code);
        write_chain(&self.entries, entry, usize::from(entry.length()), dst);
    }

    /// Materialize the string denoted by `code` (test/diagnostic helper).
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidCode`] when `code` is outside the table.
    #[cfg(test)]
    pub fn get_string(&self, code: u16) -> Result<Vec<u8>> {
        if code as usize >= self.entries.len() {
            return Err(LzwError::InvalidCode(code));
        }
        let mut out = vec![0u8; self.entry_len(code) as usize];
        self.expand(code, &mut out);
        Ok(out)
    }

    /// Check if the table is full.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.cursor.next_code > u32::from(self.limits.max_code)
    }

    /// Get the current code width.
    #[inline]
    pub fn current_bits(&self) -> u8 {
        self.cursor.current_bits
    }

    /// Get the next code that will be assigned.
    ///
    /// Returned as a `u32`: a full 16-bit table's exhausted state is
    /// `next_code == 65536`, one past `u16::MAX`.
    #[inline]
    pub fn next_code(&self) -> u32 {
        self.cursor.next_code
    }

    /// Get the clear code.
    #[inline]
    pub fn clear_code(&self) -> u16 {
        self.limits.clear_code
    }

    /// Get the end-of-information code.
    #[inline]
    pub fn eoi_code(&self) -> u16 {
        self.limits.eoi_code
    }

    /// Get the configuration.
    #[inline]
    pub fn config(&self) -> &LzwConfig {
        &self.config
    }
}

/// Build the learned entry `string(prev) ++ value`.
///
/// `parent` must be `entries[prev]`. The decode loop already holds it (this
/// iteration's entry becomes the next iteration's parent), which is what
/// keeps entry creation at zero extra table loads.
///
/// `value` is the first byte of `string(code)` for the code that was just
/// read — except in the KwKwK case, where `code` *is* this entry and its
/// first byte is not known yet; the right byte is then the first byte of
/// `string(prev)`, i.e. this entry's own `first`. That is libtiff's
/// `(codep < free_entp) ? codep->firstchar : free_entp->firstchar`, and it
/// is why the decode loop needs one comparison rather than a separate
/// KwKwK branch.
#[inline(always)]
pub(crate) fn learn_entry(prev: u16, parent: CodeEntry, value: u8) -> CodeEntry {
    // `parent.0 + LENGTH_ONE` increments the packed length and leaves the
    // inherited `first` byte where it is; masking with `INHERITED` drops the
    // parent's own prefix, suffix and repeated bits, which are then replaced.
    //
    // A string is at most one byte longer than the number of learned
    // entries, so with the widest table this crate builds (16 bits, 65 278
    // learned slots) `length` cannot reach `u16::MAX` and the add cannot
    // carry into `first`. The wrapping add is there so that a hypothetical
    // overflow degrades into a short string rather than a panic.
    let inherited = parent.0.wrapping_add(CodeEntry::LENGTH_ONE) & CodeEntry::INHERITED;
    let repeated = ((parent.0 >> 48) & 1) & u64::from(value == parent.suffix());
    CodeEntry(inherited | u64::from(prev) | (u64::from(value) << 40) | (repeated << 48))
}

/// Write the first `dst.len()` bytes of `entry`'s string into `dst`,
/// walking the prefix chain backwards from the end of the string.
///
/// `full` is `entry.length()`. When `dst` is shorter than `full` the walk
/// starts at the ancestor whose string is exactly the prefix that fits, so
/// a partial expansion yields the string's *leading* bytes (libtiff's
/// behaviour when a strip's last code overruns the row buffer).
///
/// The chain walk is a chain of *dependent* loads — each entry gives the
/// next one's index — so its cost is latency, not throughput, and the way
/// to make it cheaper is to make it shorter. Two steps are removed here:
/// the caller passes the entry it already holds instead of having it looked
/// up again, and the walk stops one step above the root because every entry
/// on a chain carries the same `first` byte (each ancestor's string is a
/// prefix of the same string). A three-byte string therefore costs one
/// table load instead of three, and a two-byte string none at all.
#[inline(always)]
pub(crate) fn write_chain(entries: &[CodeEntry], entry: CodeEntry, full: usize, dst: &mut [u8]) {
    let want = dst.len();
    if want == 0 || full == 0 || entries.is_empty() {
        return;
    }
    let mask = entries.len() - 1;
    // Drop the tail that does not fit by walking up to the ancestor whose
    // string is exactly the prefix we keep.
    let mut node = entry;
    let mut skip = full.saturating_sub(want);
    while skip > 0 {
        node = entries[node.prefix() as usize & mask];
        skip -= 1;
    }
    let first = node.first();
    let mut index = want.min(full);
    while index > 1 {
        index -= 1;
        dst[index] = node.suffix();
        node = entries[node.prefix() as usize & mask];
    }
    dst[0] = first;
}

/// Open-addressed `(prefix code, byte) -> code` map used by the encoder.
///
/// LZW only ever asks "is `string(prefix) ++ byte` already in the table?",
/// and because every table string is unique that question is answered by the
/// `(prefix, byte)` pair alone — no byte string needs to be built, hashed or
/// stored. Slots hold `key + 1` so that zero means "empty", which makes
/// [`LzwCodeIndex::clear`] a single memset.
#[derive(Debug)]
pub struct LzwCodeIndex {
    /// `key + 1` per slot, 0 when empty.
    keys: Vec<u32>,
    /// Code stored in the matching slot.
    codes: Vec<u16>,
    /// `keys.len() - 1`; `keys.len()` is always a power of two.
    mask: usize,
}

impl LzwCodeIndex {
    /// Create an index able to hold `capacity` entries at a load factor of
    /// at most 0.5 (rounded up to a power of two, minimum 1024 slots).
    ///
    /// The probe loops in [`Self::find`] and [`Self::insert`] terminate
    /// because at least one slot is always free. The largest table this
    /// crate builds is a 16-bit one, whose `capacity` is 65 536 and whose
    /// live entry count therefore never exceeds 65 536 - 258 = 65 278; that
    /// gets 131 072 slots, so the load factor stays below 0.5. (At 12 bits
    /// it is 3 838 entries in 8 192 slots.) The `min(1 << 20)` clamp only
    /// binds for capacities above 524 288, which no valid `LzwConfig` can
    /// reach.
    pub fn with_capacity(capacity: usize) -> Self {
        let slots = capacity
            .saturating_mul(2)
            .max(1024)
            .next_power_of_two()
            .min(1 << 20);
        Self {
            keys: vec![0; slots],
            codes: vec![0; slots],
            mask: slots - 1,
        }
    }

    /// Remove every entry.
    pub fn clear(&mut self) {
        self.keys.fill(0);
    }

    /// Combine a prefix code and a suffix byte into a lookup key.
    #[inline(always)]
    fn key(prefix: u16, byte: u8) -> u32 {
        ((prefix as u32) << 8) | byte as u32
    }

    /// Initial slot for a key (Fibonacci hashing).
    #[inline(always)]
    fn slot(&self, key: u32) -> usize {
        (key.wrapping_mul(0x9E37_79B1) as usize >> 8) & self.mask
    }

    /// Look up the code for `string(prefix) ++ byte`.
    #[inline(always)]
    pub fn find(&self, prefix: u16, byte: u8) -> Option<u16> {
        let key = Self::key(prefix, byte) + 1;
        let mut slot = self.slot(key);
        loop {
            let stored = self.keys[slot];
            if stored == 0 {
                return None;
            }
            if stored == key {
                return Some(self.codes[slot]);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// Record that `string(prefix) ++ byte` has code `code`.
    ///
    /// The caller guarantees the index never holds more entries than the
    /// capacity it was built with, so the probe always terminates.
    #[inline(always)]
    pub fn insert(&mut self, prefix: u16, byte: u8, code: u16) {
        let key = Self::key(prefix, byte) + 1;
        let mut slot = self.slot(key);
        loop {
            let stored = self.keys[slot];
            if stored == 0 || stored == key {
                self.keys[slot] = key;
                self.codes[slot] = code;
                return;
            }
            slot = (slot + 1) & self.mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packed layout is load-bearing: every field must survive a round
    /// trip through the `u64`, and the word must stay eight bytes so one
    /// `ldr`/`str` moves a whole entry.
    #[test]
    fn code_entry_packs_and_unpacks_every_field() {
        assert_eq!(std::mem::size_of::<CodeEntry>(), 8);
        for &(prefix, length, first, suffix, repeated) in &[
            (0u16, 0u16, 0u8, 0u8, false),
            (1, 1, 1, 1, true),
            (258, 2, b'A', b'B', false),
            (4095, 4095, 0xFF, 0x7F, true),
            (u16::MAX, u16::MAX, u8::MAX, u8::MAX, true),
            (u16::MAX, u16::MAX, u8::MAX, u8::MAX, false),
        ] {
            let entry = CodeEntry::new(prefix, length, first, suffix, repeated);
            assert_eq!(entry.prefix(), prefix, "prefix");
            assert_eq!(entry.length(), length, "length");
            assert_eq!(entry.first(), first, "first");
            assert_eq!(entry.suffix(), suffix, "suffix");
            assert_eq!(entry.repeated(), repeated, "repeated");
        }
        assert_eq!(CodeEntry::default(), CodeEntry::new(0, 0, 0, 0, false));
    }

    /// The decode loop's bounds-check elision rests entirely on this: it
    /// masks every code with `entries.len() - 1` and uses that same mask as
    /// the largest assignable code (`decoder.rs`'s `slot_mask` / `max_code`
    /// pair). If a configuration ever produced a table whose length was not
    /// `max_code + 1`, or not a power of two, codes past the end of the
    /// table would silently *alias* into occupied slots instead of being
    /// rejected — wrong bytes, no error. Every constructor is checked here.
    #[test]
    fn the_table_length_is_always_max_code_plus_one_and_a_power_of_two() {
        let mut checked = 0usize;
        let mut check = |label: &str, dict: &LzwDictionary| {
            let capacity = dict.capacity();
            assert_eq!(
                capacity,
                usize::from(dict.config().max_code()) + 1,
                "[{label}] table length is not max_code + 1"
            );
            assert!(
                capacity.is_power_of_two(),
                "[{label}] table length {capacity} is not a power of two"
            );
            assert_eq!(
                capacity - 1,
                usize::from(dict.limits.max_code),
                "[{label}] the slot mask is not the largest assignable code"
            );
            // Every root is a one-byte run, and the two reserved slots sit
            // immediately below the first learned code.
            assert_eq!(
                dict.limits.eoi_code,
                dict.limits.clear_code + 1,
                "[{label}] the reserved codes are not adjacent"
            );
            assert_eq!(
                dict.limits.first_code,
                dict.limits.eoi_code + 1,
                "[{label}] the first learned code does not follow EOI"
            );
            assert!(
                usize::from(dict.limits.first_code) <= capacity,
                "[{label}] the first learned code is outside the table"
            );
            checked += 1;
        };

        for config in [
            LzwConfig::TIFF,
            LzwConfig::TIFF_OLD_STYLE,
            LzwConfig::TIFF_COMPAT_LSB,
            LzwConfig::GIF,
        ] {
            let dict = LzwDictionary::new(config).expect("named configuration");
            check("named", &dict);
        }
        for min_bits in 9u8..=16 {
            for max_bits in min_bits..=16 {
                let config = LzwConfig::new(min_bits, max_bits).expect("valid width range");
                let dict = LzwDictionary::new(config).expect("width range");
                check(&format!("new({min_bits},{max_bits})"), &dict);
            }
        }
        // The GIF codec's narrower initial widths, which bypass the public
        // `min_bits >= 9` policy through `with_small_min_bits`.
        for minimum_code_size in 2u8..=11 {
            let config = LzwConfig {
                min_bits: minimum_code_size + 1,
                max_bits: 12,
                use_clear_code: true,
                early_change: false,
                bit_order: crate::config::LzwBitOrder::Lsb,
            };
            let dict = LzwDictionary::with_small_min_bits(config).expect("GIF configuration");
            check(&format!("gif(mcs {minimum_code_size})"), &dict);
        }
        assert_eq!(checked, 4 + 36 + 10, "not every configuration was checked");
    }

    /// `learn_entry` builds the child by arithmetic on the parent's packed
    /// word (`+ LENGTH_ONE`, mask, or in the rest). A mask slip there would
    /// be silently wrong output on the `repeated` fill path, which has no
    /// chain walk to disagree with it — so the rules are pinned directly.
    #[test]
    fn learn_entry_inherits_length_and_first_and_recomputes_repeated() {
        // Parent "AB" (not a run), child "ABC".
        let parent = CodeEntry::new(65, 2, b'A', b'B', false);
        let child = learn_entry(300, parent, b'C');
        assert_eq!(child.prefix(), 300);
        assert_eq!(child.length(), 3, "length is the parent's plus one");
        assert_eq!(child.first(), b'A', "first is inherited from the parent");
        assert_eq!(child.suffix(), b'C');
        assert!(!child.repeated(), "parent is not a run");

        // Parent "XX" (a run), child "XXX" keeps the run...
        let run = CodeEntry::new(88, 2, b'X', b'X', true);
        let longer = learn_entry(400, run, b'X');
        assert!(
            longer.repeated(),
            "run extended by its own byte stays a run"
        );
        assert_eq!(longer.length(), 3);
        assert_eq!(longer.first(), b'X');
        assert_eq!(longer.suffix(), b'X');

        // ...but a different byte ends it.
        let broken = learn_entry(400, run, b'Y');
        assert!(!broken.repeated(), "run + a different byte is not a run");
        assert_eq!(broken.suffix(), b'Y');
        assert_eq!(broken.first(), b'X');

        // A root parent (length 1, repeated) makes a two-byte run or not.
        let root = CodeEntry::new(0, 1, b'Q', b'Q', true);
        assert!(learn_entry(81, root, b'Q').repeated());
        assert!(!learn_entry(81, root, b'R').repeated());
        assert_eq!(learn_entry(81, root, b'R').length(), 2);
    }

    /// `repeated` is what routes an entry to the fill path, so the table
    /// must set it for exactly the all-equal strings and nothing else.
    #[test]
    fn the_repeated_flag_tracks_all_equal_strings_through_the_table() {
        let mut dict = LzwDictionary::new(LzwConfig::TIFF).expect("create table");
        for code in 0..256u16 {
            assert!(dict.entry(code).repeated(), "root {code} is a one-byte run");
        }
        let aa = dict.add_entry_encode(u16::from(b'a'), b'a').expect("aa");
        let aaa = dict.add_entry_encode(aa, b'a').expect("aaa");
        let aab = dict.add_entry_encode(aa, b'b').expect("aab");
        let aabb = dict.add_entry_encode(aab, b'b').expect("aabb");
        assert!(dict.entry(aa).repeated());
        assert!(dict.entry(aaa).repeated());
        assert!(!dict.entry(aab).repeated());
        assert!(!dict.entry(aabb).repeated());
        assert_eq!(dict.get_string(aaa).expect("aaa"), b"aaa");
        assert_eq!(dict.get_string(aabb).expect("aabb"), b"aabb");
    }

    #[test]
    fn test_dictionary_init() {
        let dict = LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary");

        // Check initial single-byte codes
        for i in 0..256u16 {
            let string = dict.get_string(i).expect("get string for initial code");
            assert_eq!(string, vec![i as u8]);
        }

        // Check special codes
        assert_eq!(dict.clear_code(), 256);
        assert_eq!(dict.eoi_code(), 257);
        assert_eq!(dict.next_code(), 258);
        assert_eq!(dict.current_bits(), 9);
        assert_eq!(dict.entry_len(256), 0);
        assert_eq!(dict.entry_len(257), 0);
    }

    #[test]
    fn test_add_entry() {
        let mut dict =
            LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary for add string");

        let code = dict
            .add_entry_encode(u16::from(b'A'), b'B')
            .expect("add string AB to dictionary");
        assert_eq!(code, 258);

        let retrieved = dict.get_string(code).expect("get string for code 258");
        assert_eq!(retrieved, b"AB");
        assert_eq!(dict.first_byte(code), b'A');
        assert_eq!(dict.entry_len(code), 2);
    }

    #[test]
    fn test_expand_partial_writes_prefix() {
        let mut dict = LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary");
        let ab = dict
            .add_entry_encode(u16::from(b'A'), b'B')
            .expect("add AB");
        let abc = dict.add_entry_encode(ab, b'C').expect("add ABC");
        assert_eq!(dict.entry_len(abc), 3);

        let mut full = [0u8; 3];
        dict.expand(abc, &mut full);
        assert_eq!(&full, b"ABC");

        // Fewer bytes than the string holds: keep the leading prefix.
        let mut partial = [0u8; 2];
        dict.expand(abc, &mut partial);
        assert_eq!(&partial, b"AB");

        let mut one = [0u8; 1];
        dict.expand(abc, &mut one);
        assert_eq!(&one, b"A");
    }

    #[test]
    fn test_bit_width_increase() {
        let mut dict =
            LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary for bit width test");

        // Initially 9 bits
        assert_eq!(dict.current_bits(), 9);

        // Add entries until bit width increases. With early change, the
        // width increases when next_code == 512 (2^9); we start at 258, so
        // 254 entries are needed (258 + 254 = 512).
        for i in 0..254u16 {
            dict.add_entry_encode(i % 256, (i + 1) as u8)
                .expect("add string for bit width increase test");
        }

        assert_eq!(dict.next_code(), 512);
        assert_eq!(dict.current_bits(), 10);
    }

    #[test]
    fn test_table_full_is_reported() {
        let config = LzwConfig::new(9, 9).expect("9/9 config");
        let mut dict = LzwDictionary::new(config).expect("create 9-bit dictionary");
        // first_code 258 .. max_code 511 inclusive = 254 free slots.
        for i in 0..254u16 {
            dict.add_entry_encode(i % 256, 0).expect("fill table");
        }
        assert!(dict.is_full());
        assert!(matches!(
            dict.add_entry_encode(0, 0),
            Err(LzwError::TableFull { max_codes: 511 })
        ));
    }

    #[test]
    fn test_code_index_find_insert_clear() {
        let mut index = LzwCodeIndex::with_capacity(4096);
        assert_eq!(index.find(65, b'B'), None);
        index.insert(65, b'B', 258);
        assert_eq!(index.find(65, b'B'), Some(258));
        assert_eq!(index.find(65, b'C'), None);
        assert_eq!(index.find(66, b'B'), None);

        // Fill it to the documented capacity to exercise probing.
        for code in 0..4000u16 {
            index.insert(code, (code % 251) as u8, code);
        }
        for code in 0..4000u16 {
            assert_eq!(index.find(code, (code % 251) as u8), Some(code));
        }

        index.clear();
        assert_eq!(index.find(65, b'B'), None);
        for code in 0..4000u16 {
            assert_eq!(index.find(code, (code % 251) as u8), None);
        }
    }
}

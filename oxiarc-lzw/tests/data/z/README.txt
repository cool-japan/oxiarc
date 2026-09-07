Committed `.Z` (UNIX compress) fixtures — the exact bytes ARE the test.

widths_b09.Z .. widths_b16.Z
    `compress -b N -c` (Apple/FreeBSD compress) over the 6,000-byte payload
    that `tests/z_fixtures.rs::widths_payload` regenerates. The payload is
    chosen so the code width grows 9 -> 10 -> 11 -> 12, which is where every
    group-alignment bug shows up; the low-width files also pin the reference's
    quirk that `-b 9` still emits some 10-bit codes.

clear_b10.Z
    `compress -b 10 -c` over the 21,000-byte payload that
    `tests/z_fixtures.rs::clear_payload` regenerates: 10,000 bytes of one word
    vocabulary followed by 11,000 of another, which makes the block-mode
    compression-ratio heuristic fire exactly one ClearCode. Without a file like
    this the block-mode reset path is never exercised by a committed test.

nonblock_b12.Z
    Non-block-mode (`flags & 0x80 == 0`) stream over the `widths_payload`.
    `compress(1)` cannot produce one — it always sets block mode — so this
    was written by an independent Python model of BSD `compress`'s
    `output()`/`cl_block()` (byte-identical to the CLI in block mode across
    88 (payload, width) pairs) and verified at generation time to decode
    byte-for-byte through BOTH `gzip -dc` and `uncompress -c`. The live
    `z-oracle` suite re-checks non-block streams against both tools on every
    run, so this file is not the only evidence for that dialect.

--------------------------------------------------------------------------
Regenerating (do not edit by hand)

1. Write the two payloads. They are the byte-for-byte output of the
   generators in `tests/z_fixtures.rs`; this Python reproduces them:

     python3 - <<'PY'
     A = [b"alpha ", b"beta ", b"gamma ", b"delta ", b"epsilon ", b"zeta "]
     B = [b"one ", b"two ", b"three ", b"four ", b"five ", b"six ", b"seven "]

     def words(n, vocab):
         out, i = bytearray(), 0
         while len(out) < n:
             out += vocab[(i * i + 3 * i) % len(vocab)]
             i += 1
         return bytes(out[:n])

     def lcg(n):
         out, state = bytearray(), 1
         for _ in range(n):
             state = ((state * 1103515245 + 12345) & 0xFFFFFFFF) & 0x7FFFFFFF
             out.append((state >> 16) & 0xFF)
         return bytes(out)

     open("/tmp/widths.raw", "wb").write(
         words(2000, A) + lcg(2000) + words(2000, A))
     open("/tmp/clear.raw", "wb").write(words(10000, A) + words(11000, B))
     PY

   `tests/z_fixtures.rs::the_payload_generators_still_produce_the_bytes_the_fixtures_were_made_from`
   pins their length, prefix and byte sum, so a drift is caught immediately.

2. The nine CLI fixtures:

     for b in 09 10 11 12 13 14 15 16; do
       compress -b ${b#0} -c < /tmp/widths.raw > widths_b$b.Z
     done
     compress -b 10 -c < /tmp/clear.raw > clear_b10.Z

3. The non-block fixture, from this crate (its bytes are pinned by
   `this_crates_encoder_reproduces_the_reference_bytes_exactly`, and the
   encoder is byte-identical to `compress -b N -c` in block mode):

     oxiarc_lzw::z::compress_with_block_mode(&widths_payload(), 12, false)

   Before committing a regenerated copy, verify it externally:

     gzip -dc < nonblock_b12.Z | cmp - /tmp/widths.raw
     uncompress -c < nonblock_b12.Z | cmp - /tmp/widths.raw

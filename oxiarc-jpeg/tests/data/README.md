# `oxiarc-jpeg` test fixtures

Five small JPEG datastreams whose **exact bytes are the test**: they are the
corpus of `tests/corrupt_no_panic.rs`, which truncates, bit-flips and
length-mutates every one of them at every offset. The crate's own
`src/sample.rs` constants only cover single-MCU 8-bit baseline streams, so
without these the progressive, restart, 12-bit and lossless decoders would
never see malformed input.

They are also decoded intact by `tests/decode_api.rs`, so a fixture that stopped
being a valid JPEG could not pass unnoticed.

| File | Bytes | Frame |
|---|---|---|
| `progressive_2x2.jpg` | 877 | `SOF2`, 3 components, 4:2:0, 10 scans |
| `restart_2x2.jpg` | 961 | `SOF0`, 3 components, 4:2:0, `DRI = 1` |
| `progressive_restart.jpg` | 1004 | `SOF2` with `DRI = 1` — restart markers inside progressive scans |
| `twelve_bit_gray.jpg` | 425 | `SOF1`, 1 component, `P = 12` |
| `lossless_rgb.jpg` | 625 | `SOF3`, 3 components, predictor 1, ids `'R' 'G' 'B'` |

## Regenerating

Reproduced byte for byte by libjpeg-turbo 3.1.4.1 (`cjpeg -version`). A
different libjpeg build may pick different default Huffman tables; that is fine,
the sweep does not depend on the exact bytes, only on the frame shapes above.

```sh
python3 generate.py .          # writes src_rgb.ppm, src_gray.pgm, src_gray12.pgm
cjpeg -quality 60 -progressive -sample 2x2 -outfile progressive_2x2.jpg      src_rgb.ppm
cjpeg -quality 60 -restart 1B  -sample 2x2 -outfile restart_2x2.jpg          src_rgb.ppm
cjpeg -quality 60 -progressive -restart 1B -sample 2x2 \
                                          -outfile progressive_restart.jpg  src_rgb.ppm
cjpeg -precision 12 -quality 60           -outfile twelve_bit_gray.jpg       src_gray12.pgm
cjpeg -lossless 1                         -outfile lossless_rgb.jpg          src_rgb.ppm
rm src_rgb.ppm src_gray.pgm src_gray12.pgm
```

`cjpeg -lossless` and `cjpeg -precision` are build-time capabilities of
libjpeg-turbo; a build without them cannot regenerate the last two files.

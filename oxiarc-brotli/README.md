# oxiarc-brotli

Pure Rust Brotli compression library (RFC 7932), part of the OxiArc ecosystem.

## Features

- Quality levels 0-11 (fast to best compression)
- LZ77 with context-dependent Huffman coding
- Static dictionary with 120+ common words/phrases
- Streaming compression/decompression API
- Pure Rust — no C dependencies

## Usage

```toml
[dependencies]
oxiarc-brotli = "0.2.6"
```

## Tests

75 tests passing.

## License

Apache-2.0

## Authors

COOLJAPAN OU (Team Kitasan)

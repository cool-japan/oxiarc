# Security Policy

## Supported Versions

OxiArc follows [Semantic Versioning](https://semver.org/). Security fixes are
made against the latest `0.3.x` release. There is no long-term-support branch
prior to a 1.0 release; users should upgrade to the latest published version
to receive fixes.

| Version | Supported          |
| ------- | ------------------ |
| 0.3.x   | :white_check_mark: |
| < 0.3   | :x:                |

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, report privately using one of these channels:

- **GitHub Security Advisories** (preferred): open a
  [private advisory](https://github.com/cool-japan/oxiarc/security/advisories/new)
  on the [cool-japan/oxiarc](https://github.com/cool-japan/oxiarc) repository.
- **Email**: [contact@cooljapan.tech](mailto:contact@cooljapan.tech) with a
  subject line starting `[SECURITY] oxiarc`.

Please include:

- The affected crate(s) and version(s) (`oxiarc-cli`, `oxiarc-archive`,
  `oxiarc-core`, or one of the codec crates).
- A description of the vulnerability and its impact (e.g. memory-safety
  violation, panic/denial-of-service on untrusted input, path traversal,
  cryptographic weakness).
- Steps to reproduce, ideally a minimal test case or crafted input file.
- Whether the issue requires processing untrusted/attacker-controlled
  archives, or is exploitable via the library API directly.

## Response Process

- We aim to acknowledge reports within **5 business days**.
- We will work with the reporter to confirm the issue, assess severity, and
  develop a fix.
- Once a fix is ready, we will coordinate a disclosure timeline with the
  reporter (typically 90 days from initial report, sooner if a fix ships
  earlier or the issue is already public).
- Credit is given to reporters in the release notes/CHANGELOG unless
  anonymity is requested.

## Scope

OxiArc is a **pure Rust** archive/compression library and CLI designed to
process untrusted, attacker-controlled input (archives and compressed
streams downloaded from the network, extracted from third-party sources,
etc.). In scope for security reports:

- Memory-safety issues (`unsafe` misuse, out-of-bounds access, undefined
  behavior — including issues only detectable under Miri).
- Panics, unbounded memory/CPU consumption, or other denial-of-service
  conditions triggered by malformed/malicious archives or compressed data
  (decompression bombs, crafted headers, recursive/cyclic structures).
- Path traversal / "Zip-Slip" style issues during extraction.
- Cryptographic weaknesses in the ZIP AES/ZipCrypto encryption
  implementations (weak randomness, timing side-channels, incorrect
  authentication-tag verification).
- Symlink-related extraction issues (writing outside the intended output
  directory, following attacker-controlled symlinks).

Out of scope:

- Issues that only reproduce with `--all-features` combinations that are
  explicitly documented as experimental, or with crates outside this
  workspace.
- Denial-of-service that requires the caller to pass an unbounded
  `memory_limit`/no limit at all when processing untrusted input — see each
  crate's documentation for the recommended safe-usage pattern.

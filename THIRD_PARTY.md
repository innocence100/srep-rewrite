# Third-party attribution

`Cargo.lock` records the dependency graph resolved by Cargo and is committed
for reproducibility. The project uses these dependency crates:

## Direct

- `aes` 0.8.4 — AES-256 block encryption for the independently implemented
  SREP-VHASH-128 legacy checksum. License: MIT OR Apache-2.0. Upstream:
  <https://github.com/RustCrypto/block-ciphers>.
- `md-5` 0.10.6 — raw MD5 legacy block checksums. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/hashes>.
- `sha1` 0.10.7 — raw SHA-1 legacy checksums. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/hashes>.
- `sha2` 0.10.9 — raw SHA-512 legacy checksums. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/hashes>.
- `siphasher` 1.0.3 — SipHash-2-4 legacy checksum. License: MIT/Apache-2.0.
  Upstream: <https://github.com/jedisct1/rust-siphash>.

- `same-file` 1.0.6 — safe cross-platform file identity checks for existing
  input/output entries, including Unix inode/device and Windows volume/file-index
  identity. License: Unlicense/MIT. Upstream:
  <https://github.com/BurntSushi/same-file>.
- `twox-hash` 2.1.4 — XXH3-128 with the official default secret and seed zero,
  features `std` and `xxhash3_128`, default features disabled. Used for NG v2
  checksum ID 1. License: MIT. Upstream:
  <https://github.com/shepmaster/twox-hash>.
- `blake3` 1.8.7 — BLAKE3-256 for NG v2 checksum ID 2. License: CC0-1.0 OR
  Apache-2.0 OR Apache-2.0 WITH LLVM-exception. Upstream:
  <https://github.com/BLAKE3-team/BLAKE3>.
- `tempfile` 3.27.0 — safe exclusive private temporary files for input/output
  staging and validation spools. License: MIT OR Apache-2.0. Upstream:
  <https://github.com/Stebalien/tempfile>.
- `pulldown-cmark` 0.13.0 — CommonMark AST parsing for requirement extraction.
  License: MIT. Upstream: <https://github.com/raphlinus/pulldown-cmark>.
- `serde` 1.0.229 — requirement manifest serialization. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/serde-rs/serde>.
- `serde_json` 1.0.151 — UTF-8 JSON manifest handling. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/serde-rs/json>.
- `unicode-normalization` 0.1.25 — NFC normalization for requirement leaves.
  License: MIT OR Apache-2.0. Upstream:
  <https://github.com/unicode-rs/unicode-normalization>.

The lockfile currently resolves the direct dependency versions above exactly;
Cargo's lockfile is authoritative for all transitive versions and checksums.

## Transitive

- `arrayvec` 0.7.8 — transitive of `blake3`, fixed-capacity arrays. License:
  MIT OR Apache-2.0. Upstream: <https://github.com/bluss/arrayvec>.
- `bitflags` 2.13.1 — transitive of `rustix`, typed OS flags. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/bitflags/bitflags>.
- `cc` 1.4.4 — build dependency of `blake3`, native compilation support.
  License: MIT OR Apache-2.0. Upstream: <https://github.com/rust-lang/cc-rs>.
- `cfg-if` 1.0.4 — transitive platform selection support. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/rust-lang/cfg-if>.
- `constant_time_eq` 0.4.2 — transitive of `blake3`, constant-time equality.
  License: CC0-1.0 OR MIT-0 OR Apache-2.0. Upstream:
  <https://github.com/cesarb/constant_time_eq>.
- `cpufeatures` 0.3.1 — CPU feature detection. License:
  MIT OR Apache-2.0. Upstream: <https://github.com/RustCrypto/utils>.
- `cpufeatures` 0.2.17 — CPU feature detection. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/utils>.
- `errno` 0.3.14 — transitive of `rustix`, OS error access. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/lambda-fairy/rust-errno>.
- `fastrand` 2.5.0 — transitive of `tempfile`, fallback random support. License:
  Apache-2.0 OR MIT. Upstream: <https://github.com/smol-rs/fastrand>.
- `find-msvc-tools` 0.1.11 — transitive of `cc`, MSVC tool discovery. License:
  MIT OR Apache-2.0. Upstream: <https://github.com/rust-lang/cc-rs>.
- `getrandom` 0.4.3 — direct CandidateIndex nonce generation and transitive of
  `tempfile`, OS randomness. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/rust-random/getrandom>.
- `libc` 0.2.189 — transitive system bindings. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/rust-lang/libc>.
- `linux-raw-sys` 0.12.1 — transitive of `rustix`, Linux ABI bindings. License:
  Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT. Upstream:
  <https://github.com/sunfishcode/linux-raw-sys>.
- `once_cell` 1.21.4 — transitive of `tempfile`, one-time initialization.
  License: MIT OR Apache-2.0. Upstream: <https://github.com/matklad/once_cell>.
- `r-efi` 6.0.0 — transitive of `getrandom`, EFI bindings. License: MIT OR
  Apache-2.0 OR LGPL-2.1-or-later. Upstream: <https://github.com/r-efi/r-efi>.
- `rustix` 1.1.4 — transitive of `tempfile`, safe system APIs. License:
  Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT. Upstream:
  <https://github.com/bytecodealliance/rustix>.
- `shlex` 2.0.1 — transitive of `cc`, shell-like argument parsing. License: MIT
  OR Apache-2.0. Upstream: <https://github.com/comex/rust-shlex>.
- `windows-link` 0.2.1 — transitive of Windows system crates. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/microsoft/windows-rs>.
- `windows-sys` 0.61.2 — transitive Windows system bindings. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/microsoft/windows-rs>.
- `winapi-util` 0.1.11 — transitive of `same-file`, Windows handle wrappers.
  License: Unlicense OR MIT. Upstream:
  <https://github.com/BurntSushi/winapi-util>.
- `block-buffer` 0.10.4 — transitive digest buffer. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/utils>.
- `cipher` 0.4.4 — transitive cipher traits. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/traits>.
- `crypto-common` 0.1.7 — transitive crypto traits. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/traits>.
- `digest` 0.10.7 — transitive digest traits. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/RustCrypto/traits>.
- `generic-array` 0.14.7 — transitive fixed arrays. License: MIT. Upstream:
  <https://github.com/fizyk20/generic-array.git>.
- `inout` 0.1.4 — transitive cipher buffer. License: MIT OR Apache-2.0. Upstream:
  <https://github.com/RustCrypto/utils>.
- `typenum` 1.20.1 — transitive type-level integers. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/paholg/typenum>.
- `version_check` 0.9.5 — transitive build feature detection. License:
  MIT/Apache-2.0. Upstream: <https://github.com/SergioBenitez/version_check>.
- `itoa` 1.0.18 — transitive JSON integer formatting. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/dtolnay/itoa>.
- `memchr` 2.8.3 — transitive byte searching. License: Unlicense OR MIT. Upstream:
  <https://github.com/BurntSushi/memchr>.
- `proc-macro2` 1.0.107 — transitive procedural macro token support. License:
  MIT OR Apache-2.0. Upstream: <https://github.com/dtolnay/proc-macro2>.
- `quote` 1.0.47 — transitive procedural macro token generation. License:
  MIT OR Apache-2.0. Upstream: <https://github.com/dtolnay/quote>.
- `serde_core` 1.0.229 — transitive Serde traits. License: MIT OR Apache-2.0.
  Upstream: <https://github.com/serde-rs/serde>.
- `serde_derive` 1.0.229 — transitive Serde derive macros. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/serde-rs/serde>.
- `syn` 3.0.4 — transitive procedural macro parsing. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/dtolnay/syn>.
- `tinyvec` 1.12.0 — transitive Unicode normalization storage. License: Zlib OR
  Apache-2.0 OR MIT. Upstream: <https://github.com/Lokathor/tinyvec>.
- `tinyvec_macros` 0.1.1 — transitive tinyvec macros. License: MIT OR
  Apache-2.0 OR Zlib. Upstream: <https://github.com/Soveu/tinyvec_macros>.
- `unicase` 2.9.0 — transitive case normalization support. License: MIT OR
  Apache-2.0. Upstream: <https://github.com/seanmonstar/unicase>.
- `unicode-ident` 1.0.24 — transitive Unicode identifier data. License: (MIT OR
  Apache-2.0) AND Unicode-3.0. Upstream: <https://github.com/dtolnay/unicode-ident>.
- `zmij` 1.0.23 — transitive JSON number formatting. License: MIT. Upstream:
  <https://github.com/dtolnay/zmij>.

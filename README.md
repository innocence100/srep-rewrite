# SREP-NG

SREP-NG is a clean-room Rust implementation of the SuperREP preprocessor. In
software release **0.1.1**, the current archive format is **SREP-NG v3
(NGv3)**. NGv3 is the format written by this project and the format read by
default.

This is an early, explicitly limited preview. The compatibility policy below
is the complete promise for this release; compatibility, compression ratio,
and performance beyond it are not implied.

## Compatibility policy

- **NGv3 read/write:** supported. NGv3 archives are self-contained `.srep`
  files.
- **Historical SuperREP v1–v4:** embedded archives are read-only. They are
  accepted for migration and inspection, but this program never writes a
  historical archive.
- **NGv1 and NGv2:** rejected as `UnsupportedVersion`. This includes the
  prototype `.srep2`/NGv1 form and the retired `SREPNG2\0` form. The project
  does not decode, write, or promise compatibility with either format.
- **Split indexes:** unsupported. NGv3 does not create or consume a split
  index or sidecar.

The current normative wire specification is [`docs/FORMAT-V3.md`](docs/FORMAT-V3.md).
[`docs/FORMAT.md`](docs/FORMAT.md) and the frozen design under
[`docs/superpowers/specs/`](docs/superpowers/specs/) are historical provenance;
they are not current NGv2 contracts. The frozen design remains relevant only
where NGv3 explicitly inherits matching and resource semantics.

## Install a 0.1.1 native release artifact

Version 0.1.1 is a maintenance candidate. Publication requires independent
review and fresh native GitHub Actions evidence for Linux x86_64, Windows
x86_64 MSVC, and macOS ARM64. Expected package names are below; their presence
here does not mean they have been published. Exact target, source, compiler,
hash, and runtime details come
from each package's `BUILD-PROVENANCE.json` and `NOTICES`.

```text
srep-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
srep-v0.1.1-x86_64-pc-windows-msvc.zip
srep-v0.1.1-aarch64-apple-darwin.tar.gz
```

Each archive has one directory named after its basename without `.tar.gz` or
`.zip` and contains the
binary, release documents, `NOTICES`, `BUILD-PROVENANCE.json`, and
`PLATFORM-README.md`. `SHA256SUMS` is beside the archive.

```text
srep (srep.exe on Windows)
LICENSE
README.md
CHANGELOG.md
THIRD_PARTY.md
THIRD_PARTY.audit
NOTICES
BUILD-PROVENANCE.json
PLATFORM-README.md
```

`SHA256SUMS` is distributed beside the tarball, not inside it. Verify the
download before unpacking or running it:

```sh
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf srep-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
cd srep-v0.1.1-x86_64-unknown-linux-gnu
./srep --version
./srep --help
```

Windows PowerShell: run `Get-FileHash .\srep-v0.1.1-x86_64-pc-windows-msvc.zip -Algorithm SHA256`
and compare with the matching `SHA256SUMS` entry before using `Expand-Archive`.
In the extracted directory run `.\srep.exe --version` and `.\srep.exe --help`.
This is an x86_64 MSVC package, not an ARM64 or MinGW package.

On Apple Silicon macOS, run `shasum -a 256 srep-v0.1.1-aarch64-apple-darwin.tar.gz`
and compare with the matching entry before unpacking with `tar -xzf`.
In the extracted directory run `./srep --version` and `./srep --help`.
The macOS package is ARM64, not Intel/universal. Packages are not publisher
code-signed or notarized. macOS may require explicit approval in Privacy &
Security after verification; do not disable Gatekeeper globally.

New 0.1.1 runtime compatibility is unknown
until the native build completes. The published 0.1.0 tarball remains
unchanged; its SHA-256 is
`039db897613e6f74e2c5469fb6de7c1971029d2f14c6aa25637ae51523051401`.

## Build from source

Rust **1.88.0** is the declared minimum supported Rust version. A locked
source build on Linux is:

```sh
cargo build --locked --release
./target/release/srep --version
```

The resulting `target/release/srep` is a source build, not the release
artifact above. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for development and
verification guidance.

## Quick start

```sh
./srep compress input.bin input.bin.srep
./srep test input.bin.srep
./srep info input.bin.srep
./srep decompress input.bin.srep restored.bin
cmp input.bin restored.bin
```

`test` verifies the archive without retaining decompressed output. Always keep
the original input until `test`, decompression, and an application-level
comparison have succeeded. For irreplaceable data, make a separate backup
before archiving; an archive is not a substitute for backup copies.

The CLI also supports `--version` and `-V`, and `--help` documents the exact
options accepted by the installed binary. Compression defaults to method
**m3**, layout `index`, XXH3 checksums, and an 8 MiB block size. The `m3`
finder is a fixed-grid matcher; it is not a promise of parity with the
historical SuperREP compressor.

For a stream or an explicit output path:

```sh
./srep compress - archive.srep < input.bin
./srep decompress archive.srep restored.bin
```

Existing destinations require `--force`. Use `--force` only when replacement
is intentional. `--index=PATH` is recognized for diagnostics but returns the
unsupported split-index error and does not open the path.

## Resources and operational cautions

Matching and archive staging use bounded memory and may use temporary disk.
The default m3 settings can require substantial resources on large or highly
repetitive inputs. Plan free space for temporary files, and use the resource
options shown by `./srep --help` (`--memory`, `--temp-dir`, and
`--temp-limit`) when operating under a budget. A resource limit must fail
explicitly; it must not silently weaken matching or change the format.

For every archive, check the result before deleting or replacing the source:

1. preserve a backup of the original;
2. run `srep test archive.srep`;
3. decompress to a new destination;
4. compare the restored bytes with the original; and
5. only then perform any application-specific handoff or cleanup.

## What NGv3 provides

NGv3 supports the m0 through m5 matching methods and the Index-LZ, Future-LZ,
and I/O-LZ layouts. The method and layout are persisted in the archive header.
The archive is self-contained, validates its structure and checksums, and
does not require a sidecar index. `info` reports the archive's format,
method, layout, checksum, and size/match metadata.

This describes implemented interfaces, not a claim that NGv3 matches the
historical compressor's byte output, ratio, or runtime. The prior integration
handoff reports 12 completed rows of a 72-row comparison (12 samples times six
methods), not a
full-quality PASS. Sample 03 / m0 is incomplete because the default 256 MiB
memory budget reached an out-of-memory condition. The planned 0.2 bounded-memory
fix is not included here; no uniform skip or algorithm, ratio, or fidelity
claim follows. Those historical raw fidelity logs are no longer present in the
recreated workspace; this is a disclosed known limitation, not fresh evidence.

## Evidence and release status

Prior CI evidence at commit
[`8dca8be`](https://github.com/innocence100/srep-rewrite/actions/runs/34926545492)
covered six native jobs (Linux, Windows MSVC, and macOS ARM64 at stable and
Rust 1.88). That evidence is source validation at that commit, not evidence
that a new artifact has been produced, and it is not a substitute for the
Linux release-binary checks.

The retained m1 finder evidence used a 258 MiB input and included a repeated
block whose match-distance witness was greater than 256 MiB; it completed in
17.57 seconds. It is not full round-trip evidence and is not default-m3
performance evidence.

The historical **0.1.0** default-m3 Linux release-binary comparison **passed**. Recipe
`repeat-1mib-separated-256mib-unique-v1`, 270532608 bytes, input SHA-256
`c6c2c4a0735a006faf10ffec7dce05ecd23366a5da071f5d5dfdb3b0eb38f0a8`. Commands
used the binary defaults (no `-m` / `--method` / `--layout` / `--checksum`);
only `--temp-dir` was added. `info` reported `SREP-NG v3` / `m3` / `index` /
`xxh3`. `test`, decompress, SHA-256, and `cmp` matched. This is a correctness
round trip, not a ratio or performance claim. Artifact identity (source SHA,
compiler, binary SHA-256) is recorded in package `NOTICES` and `SHA256SUMS`,
not in this file. See [`docs/releases/v0.1.1.md`](docs/releases/v0.1.1.md),
[`docs/releases/v0.1.0.md`](docs/releases/v0.1.0.md), and
[`docs/acceptance-traceability.md`](docs/acceptance-traceability.md).

## Reporting a bug

Please include:

- the SREP-NG version from `srep --version`;
- operating system, architecture, and relevant glibc/toolchain information;
- the complete command line and method/layout/checksum options;
- whether the input was a historical archive or NGv3 archive; and
- a minimal reproducer or safe diagnostic output, without secrets or
  confidential data.

Do not attach credentials or private input data. If an archive may be damaged,
preserve the original bytes and its checksum before attempting repairs.

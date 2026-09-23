# SREP-NG

SREP-NG is a clean-room Rust implementation of the SuperREP preprocessor. In
software release **0.1.0**, the current archive format is **SREP-NG v3
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

## Install the Linux release artifact

The 0.1.0 binary package promised by this release is Linux x86_64 with the
glibc target `x86_64-unknown-linux-gnu`:

```text
srep-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
```

The archive has one directory whose name is the archive basename without
`.tar.gz`. That directory contains:

```text
srep
LICENSE
THIRD_PARTY.md
THIRD_PARTY.audit
README.md
CHANGELOG.md
NOTICES
```

`SHA256SUMS` is distributed beside the tarball, not inside it. Verify the
download before unpacking or running it:

```sh
sha256sum -c SHA256SUMS
tar -xzf srep-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
cd srep-v0.1.0-x86_64-unknown-linux-gnu
./srep --version
./srep --help
```

The Linux x86_64 package is dynamically linked (`libc.so.6`,
`ld-linux-x86-64.so.2`, `libpthread.so.0`, `libgcc_s.so.1`, `libdl.so.2`).
`readelf` on the 0.1.0 preview binary observed GNU libc symbol versions up to
**GLIBC_2.30**. That is a runtime symbol-version bound, not a distro package
name. Exact binary and tarball checksums are in the adjacent `SHA256SUMS` and
in the package `NOTICES` file; they are not duplicated here. Only the Linux
x86_64 package is promised. Windows and macOS source validation has previously
passed in native CI at historical commit `8dca8bed`, but no Windows or macOS
0.1.0 binary package is promised by these notes. This preview is not a GitHub
Release until the integrator publishes it.

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
historical compressor's byte output, ratio, or runtime. The 72-sample fidelity
comparison is intentionally deferred until after publication and is tracked as
a separate acceptance gate.

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

The default-m3 Linux release-binary comparison **passed**. Recipe
`repeat-1mib-separated-256mib-unique-v1`, 270532608 bytes, input SHA-256
`c6c2c4a0735a006faf10ffec7dce05ecd23366a5da071f5d5dfdb3b0eb38f0a8`. Commands
used the binary defaults (no `-m` / `--method` / `--layout` / `--checksum`);
only `--temp-dir` was added. `info` reported `SREP-NG v3` / `m3` / `index` /
`xxh3`. `test`, decompress, SHA-256, and `cmp` matched. This is a correctness
round trip, not a ratio or performance claim. Artifact identity (source SHA,
compiler, binary SHA-256) is recorded in package `NOTICES` and `SHA256SUMS`,
not in this file. See [`docs/releases/v0.1.0.md`](docs/releases/v0.1.0.md) and
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

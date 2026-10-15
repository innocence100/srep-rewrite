# SREP-NG

SREP-NG is a clean-room Rust implementation of the SuperREP preprocessor. In
software release **0.2.1**, the current archive format is **SREP-NG v3
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

## Install the published 0.2.1 native release artifact

Version **0.2.1** is published as a **limited prerelease** on GitHub:

<https://github.com/innocence100/srep-rewrite/releases/tag/v0.2.1>

It was published **2026-10-15T19:03:54Z** (release id `414169555`) from source
commit `6f1357fc0ebf1e35d89eceef358bcb2e6bd098ed` (annotated tag
`c11c8ce3f3f550521f9b3e952481a75c92a5d0fb`). It is marked `prerelease`, and
`releases/latest` does not point to it. The release carries three native
platform archives for Linux x86_64, Windows x86_64 MSVC, and macOS ARM64, plus
its evidence and checksum assets. Exact target, source, compiler, hash, and
runtime details come from each package's `BUILD-PROVENANCE.json` and `NOTICES`.
Download URLs, hashes, and caveats are collected in
[`docs/evidence/v0.2.1-publication.md`](docs/evidence/v0.2.1-publication.md).

```text
srep-v0.2.1-x86_64-unknown-linux-gnu.tar.gz
srep-v0.2.1-x86_64-pc-windows-msvc.zip
srep-v0.2.1-aarch64-apple-darwin.tar.gz
```

These are the three published native archives. Each archive has one directory
named after its basename without `.tar.gz` or `.zip`; that directory's root
holds the following files:

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

The package-embedded `README.md` and `CHANGELOG.md` are pre-publication
snapshots at `6f1357fc` and were intentionally not rewritten; the archives were
not repacked after the build.

`SHA256SUMS` is distributed beside the tarball, not inside it. Verify the
download before unpacking or running it:

```sh
sha256sum --ignore-missing -c SHA256SUMS
tar -xzf srep-v0.2.1-x86_64-unknown-linux-gnu.tar.gz
cd srep-v0.2.1-x86_64-unknown-linux-gnu
./srep --version
./srep --help
```

Windows PowerShell: run `Get-FileHash .\srep-v0.2.1-x86_64-pc-windows-msvc.zip -Algorithm SHA256`
and compare with the matching `SHA256SUMS` entry before using `Expand-Archive`.
In the extracted directory run `.\srep.exe --version` and `.\srep.exe --help`.
This is an x86_64 MSVC package, not an ARM64 or MinGW package.

On Apple Silicon macOS, run `shasum -a 256 srep-v0.2.1-aarch64-apple-darwin.tar.gz`
and compare with the matching entry before unpacking with `tar -xzf`.
In the extracted directory run `./srep --version` and `./srep --help`.
The macOS package is ARM64, not Intel/universal. Packages are not publisher
code-signed or notarized. macOS may require explicit approval in Privacy &
Security after verification; do not disable Gatekeeper globally.

Observed 0.2.1 runtime requirements from the actual published packages:

- **Linux x86_64 glibc:** requires at least **GLIBC_2.34** as observed from the
  binary's GLIBC symbol versions (built on Ubuntu; an observed floor, not a
  guarantee for every environment). A host with an older glibc, such as
  glibc 2.31, cannot load this binary.
- **Windows x86_64 MSVC:** native PE32+ x86_64 MSVC binary (not ARM64, not
  MinGW); observed imports include `VCRUNTIME140.dll` and Windows API-set
  runtime libraries.
- **macOS ARM64:** native arm64 Mach-O (not Intel/universal); not publisher
  code-signed and not notarized. It carries an ad-hoc Rust code signature (an
  `LC_CODE_SIGNATURE` with CodeDirectory flags `0x20002` / `CS_ADHOC`); an
  ad-hoc signature does not authenticate a publisher identity.

No other platforms were built or tested for 0.2.1. The published v0.1.0, v0.1.1,
and v0.2.0 releases are historical and unchanged; the 0.1.0 Linux tarball
SHA-256 is
`039db897613e6f74e2c5469fb6de7c1971029d2f14c6aa25637ae51523051401`.

## Build from source

Software version **0.2.1** is published; its native binaries and the 72-row
final-XZ fidelity run on the published Linux package binary are recorded in
[`docs/releases/v0.2.1.md`](docs/releases/v0.2.1.md) and
[`docs/evidence/v0.2.1-publication.md`](docs/evidence/v0.2.1-publication.md).
The accepted sparse-input m3 optimization is included in this source. The
planned 0.3.0 automatic resource policy is not implemented and changes none of
the current defaults.

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
`--memory SIZE` limits managed compression working memory: input/output
buffers, candidate storage, indexes, sorting, WIS/match selection, caches, and
their associated working control state. File pathname processing, runtime and
allocator overhead are excluded; it is not a hard cap on total process RSS, and
additional resident memory can come from those excluded categories. In
particular, `--temp-dir` is a pathname location whose length does not consume
the `--memory` working budget. The default m3 settings can require substantial
resources on large or highly repetitive inputs. Plan free space for temporary
files, and use the resource options shown by `./srep --help` (`--memory`,
`--temp-dir`, and `--temp-limit`) when operating under a budget. A resource
limit must fail explicitly; it must not silently weaken matching or change the
format.

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
historical compressor's byte output, ratio, or runtime. Version 0.2.0 adds a
bounded-memory offload path (256 MiB managed working memory, pathnames, runtime,
and allocator overhead excluded) and is **published** as a limited prerelease.
The final actual-package 72-row comparison completed 72/72 byte-identical round
trips (resource SUCCESS). Quality is judged by the user-revised
`majority-final-xz-v1` gate: all six methods pass a per-method strict majority of
final-XZ streams strictly smaller than the frozen baseline on the fixed
12-sample corpus (ties do not win; m0 11/12, m1 10/12, m2 9/12, m3 12/12,
m4 12/12, m5 11/12; coverage gate passes). Under the **previous** policy the same
unchanged historical candidate evidence read FAIL on four small-denominator rows;
that old verdict is preserved distinctly — see
`docs/evidence/v0.2.0-local-acceptance.md`. No universal large-file, uniform
skip, algorithm, ratio, or fidelity claim follows. A known 0.2.0 release
limitation is sparse-input cost: the 512 KiB sparse sample 07 remains
hours-level work for some methods. No future optimization is claimed. The prior
integration handoff's 12-row comparison and the historical sample-03/m0
out-of-memory record remain as history, not fresh evidence.

The archived local candidate evidence above and the final actual-package results
are distinct: the local candidate report was produced on a dirty, pre-version
candidate binary (`996dcecf…`), while the release evidence was produced on the
published, committed Linux package binary (`38a3a045…`). See
[`docs/evidence/v0.2.0-publication.md`](docs/evidence/v0.2.0-publication.md).

Version 0.2.1 is the current published release. It carries the accepted
sparse-input m3 optimization with NGv3 and matching semantics unchanged. Its
final actual-package 72-row comparison on the published Linux binary
(`8350d23f…`) completed 72/72 byte-identical round trips under the same
`majority-final-xz-v1` gate (65 wins / 0 ties / 7 losses; m0 11/12, m1 10/12,
m2 9/12, m3 12/12, m4 12/12, m5 11/12), with an explicit 256 MiB memory and
32 GiB temp allowance. The managed temp peak (5.615400 GiB) remains above the
unchanged 4 GiB default; no 4-GiB completion and no process-RSS cap are claimed.
See [`docs/evidence/v0.2.1-publication.md`](docs/evidence/v0.2.1-publication.md).

## Evidence and release status

Version 0.2.1 was published as a limited prerelease on 2026-10-15T19:03:54Z
from commit `6f1357fc0ebf1e35d89eceef358bcb2e6bd098ed` (release id `414169555`).
The native packaging run
[`38703862425`](https://github.com/innocence100/srep-rewrite/actions/runs/38703862425)
passed 6/6 jobs; the CI run
[`38703862408`](https://github.com/innocence100/srep-rewrite/actions/runs/38703862408)
passed 6/6 jobs on a failed-jobs-only rerun after its initial macOS MSRV attempt
ended with a lost-runner-communication annotation and no more specific cause
established. All 9 release assets download and verify; the three native archives
and the fresh actual-package evidence are summarized, with per-asset hashes and
download URLs, in
[`docs/evidence/v0.2.1-publication.md`](docs/evidence/v0.2.1-publication.md).

Version 0.2.0 was published as a limited prerelease on 2026-10-10T02:19:26Z
from commit `6ce0a32b83c625519a427c0d3365e0abfee22d7e`. The native packaging run
[`37939335761`](https://github.com/innocence100/srep-rewrite/actions/runs/37939335761)
and the CI run
[`37939335813`](https://github.com/innocence100/srep-rewrite/actions/runs/37939335813)
each passed 6/6 jobs on that exact commit. All 10 release assets download and
verify. The curated evidence, per-asset hashes, and caveats are in
[`docs/evidence/v0.2.0-publication.md`](docs/evidence/v0.2.0-publication.md).

The archived **local** evidence remains distinct from the release. The
`docs/evidence/v0.2.0-local-acceptance.md` record was produced on a dirty
pre-version candidate binary (`996dcecf…`); the older 12-row integration handoff
and the sample-03/m0 out-of-memory record are history. None of those are the
final actual-package result.

The historical **0.1.0** default-m3 Linux release-binary comparison **passed**. Recipe
`repeat-1mib-separated-256mib-unique-v1`, 270532608 bytes, input SHA-256
`c6c2c4a0735a006faf10ffec7dce05ecd23366a5da071f5d5dfdb3b0eb38f0a8`. Commands
used the binary defaults (no `-m` / `--method` / `--layout` / `--checksum`);
only `--temp-dir` was added. `info` reported `SREP-NG v3` / `m3` / `index` /
`xxh3`. `test`, decompress, SHA-256, and `cmp` matched. This is a correctness
round trip, not a ratio or performance claim. Artifact identity (source SHA,
compiler, binary SHA-256) is recorded in package `NOTICES` and `SHA256SUMS`,
not in this file. See [`docs/releases/v0.2.0.md`](docs/releases/v0.2.0.md), [`docs/releases/v0.1.1.md`](docs/releases/v0.1.1.md),
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

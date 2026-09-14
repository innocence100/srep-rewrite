# SREP-NG

SREP-NG is a clean-room Rust 2024 implementation of the SuperREP preprocessor.
This tree writes self-contained **NG v3** `.srep` archives and strictly reads
NG v3 plus embedded historical SuperREP `.srep` v1–v4 archives. Stage 8 implements m0–m5 finders with a
deterministic RAM-or-spill CandidateIndex, including the m3/m4/m5 REP overlay, while the
library also provides a candidate-driven path for all three NG v3 layouts.
Prototype NG v1 / `.srep2` archives and retired NG v2 (`SREPNG2\0`) archives are
rejected as `UnsupportedVersion`. NG v2 is retired exactly like prototype NG v1:
the `compress_v2*` compatibility APIs and the NG v2 reader/writer are gone, and
no compatibility surface emits or decodes NG v2.

The code is original and does not wrap, port, or shell out to the historical C++
implementation for normal archive operations. The separate legacy fixture
generator CLI is the only migration tool that invokes a caller-supplied
historical binary; it is append-only and never installs generated files into
this tree. `unsafe` is forbidden in both the library and the binary.

The approved normative wire and algorithm source for the current NG v3 format is
`docs/FORMAT-V3.md`; it must not contradict the frozen historical design at
`docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md`. That
frozen document remains the authoritative baseline only for the inherited
matching algorithms and shared resource semantics, not for its retired NG v2
wire format or any v2 compatibility surface. `docs/FORMAT.md` is the historical
NG v2 wire reference, retained for history and not a current normative source.

Stage 6 evidence includes a committed deterministic corpus and retained
`-hash=md5` SREP 3.93a m3/m4 archives. Stage 7 evidence adds committed
M5 discriminator corpus and retained old M5 archive. The comparable M5 command
is `srep -v0 -b8k -l16 -d0 -m5o -hash=md5` with no `-c` override, so old M5
derives `L=8`; its legacy `BASE_LEN` header byte is recorded separately. The
new vector uses the same minimum, derived `L=8`, 8 KiB block, I/O layout, and
XXH3 checksum and a forced-L M4 discriminator archive. Comparable Stage 6 vectors use `L=16`,
`minimum_match=16`, `block_size=8KiB`, no overlay, and I/O layout semantics on
both sides; m3 `L=3/min=7` remains a separate new-only conformance vector. The
 evidence scripts strictly validate JSON keys, hashes, legacy headers, metrics,
the m4 backward witness, scalar tamper rejection, and round trips offline. An
 optional old-binary runs verify the exact historical executable SHA256 and
reproduces the retained archive bytes read-only.

The historical NG v2 byte-comparison scripts (`compare-fixed-finders.py`,
`compare-m5-finder.py`) retain frozen NG v2 hashes, headers, and metrics as
offline provenance only. Their active decoder path now replays the recorded
options through the current CLI, requires `SREPNG3\0`, checks current NGv3
semantic metrics and witnesses, and round-trips the result. They deliberately
do not compare current NGv3 bytes or sizes with retired NGv2 pins, and make no
NGv2 output-compatibility claim. Optional old-binary reproduction remains a
separate read-only historical check.

## Quick start

```sh
cargo build --release
./target/release/srep compress input.bin
./target/release/srep info input.bin.srep
./target/release/srep test input.bin.srep
./target/release/srep decompress --force input.bin.srep
```

Compression defaults to `input.srep`. Decompression removes only an exact
terminal lowercase `.srep` extension (so `foo.srep.extra` and `foo.SREP` require
an explicit output). Existing destinations require `--force`. Input/output
collisions are rejected. `-` selects stdin or stdout, and an explicit output is
required when a default path cannot be inferred. Inferred names use native path
APIs and preserve non-UTF-8 and Windows WTF-8 path components.

## Commands and options

- `srep compress [OPTIONS] INPUT [OUTPUT]`
- `srep decompress [OPTIONS] INPUT [OUTPUT]`
- `srep info [OPTIONS] INPUT`
- `srep test [OPTIONS] INPUT` (verifies checksums and decoding without retaining output)

Compression options implemented by the NG v3 writer:

- `--layout=index|future|io` (default `index`)
- `--checksum=xxh3|blake3` (default `xxh3`)
- `--block-size SIZE` (default `8MiB`)
- `--min-match SIZE` (default `512`; persisted in the header)
- `--method=m0|m1|m2|m3|m4|m5` and `-m0` through `-m5` persist method identity;
  `m0` performs representative matching, m1/m2 perform CDC matching, m3/m4
  perform fixed matching, and m5 performs exhaustive matching
- `--seed-size SIZE`, `--target-chunk SIZE`, `--max-distance SIZE`,
  `--rep-overlay`, `--rep-distance SIZE`, and `--rep-min-match SIZE`
- `--memory SIZE`, `--temp-dir PATH`, `--temp-limit SIZE`, and
  `--output-limit SIZE`
- `--force`, `--quiet`/`-q`
- `--index=PATH` and `-index=PATH` are recognized only to return
  `UnsupportedLegacySplitIndex`; the path is never opened

The CLI `compress` command uses the selected m0 through m5 finder. These methods
use complete history by default and accept `--max-distance` as an
inclusive candidate-distance limit. The default method is m3 and performs real
fixed-grid matching, while m5 scans every target position and all aligned source
seeds. Library callers can use
`compress_with_candidates` to provide checked candidate intervals; the encoder
validates their implied bytes, normalizes them deterministically, and emits real
references. Sizes accept bytes (`B`), binary
kilobytes (`K`, `KB`, `KiB`), mebibytes (`M`, `MB`, `MiB`), and gibibytes
(`G`, `GB`, `GiB`), case-insensitively; fractional sizes are rejected.

## Architecture and safety

The library exposes `CompressionConfig` (`Config` remains a compatibility alias),
structured `Error`/`ErrorKind` with stable codes and message IDs, and
`compress` / `compress_with_candidates` / `decompress` / `inspect` /
`inspect_matches` / `verify`. The candidate writer uses one shared Match IR,
authoritative input equality checks, deterministic weighted normalization, and
canonical Index-LZ, Future-LZ, and I/O-LZ records. The decoder validates the
80-byte header, layout records, references, per-block CRC32C, plaintext and
encoded global digests, fixed tail locators, and exact termination. NG v3 has
no record framing and does not claim a per-record hash.
Resource-aware match inspection and normalization return reservation-owning
collections. Use their slice, iterator, indexing, or `Deref` views and drop the
result to release its memory reservation. `inspect_matches`, including the
default convenience API, returns the reservation-owning object directly.

Fixed protocol headers are parsed from stack buffers. Variable payloads are
structurally length-checked before allocation and charged to the working-memory
budget while live. The total uncompressed stream is not charged to RAM.
Scalable Stage3 collections and working buffers use reservation-owning
`BudgetedVec<T>` values, and allocation growth may fail with
`MemoryBudgetExceeded`. `BudgetedVec<T>` grows transactionally while retaining
the old allocation and reservation; successful growth reconciles the
reservation to observed capacity, while failed growth preserves the original
elements. Temporary growth can therefore raise high-water usage without
raising steady-state current usage. Candidate archives are encoded into a private
temporary spool and are not copied to caller output until planning and encoding
succeed, so a resource failure leaves caller output unchanged.

File output is first written to an OS-randomized, exclusively-created sibling
 temporary file with restrictive permissions, flushed, and `sync_all`'d before publication. Temporary input, validation spools, and CandidateIndex runs use the same OS-safe naming and cleanup policy. Every operation uses one shared temporary budget; actual spool/staging/index bytes are reserved before growth. Visible CandidateIndex runs use only the exact 64-byte `SREPIDX1` persisted-order header and 104-byte checksummed records. Query and compaction scratch uses the separate self-describing `SREPQRY1` format with authoritative identity/persisted-query order IDs; scratch is RAII-cleaned on normal return and callback failure. Without `--force`, publication is
an atomic hard-link creation at the destination. With `--force`, publication
atomically renames the closed temporary file over any existing non-directory
entry, replacing the directory entry itself (not a symlink target). Directories
are always refused. On failure only the temporary file is removed. Input/output
identity is checked before processing, including Unix and Windows hard-link
aliases.

## Current implementation status

 | Capability | Status |
|---|---|
  | Write self-contained NG v3 | implemented; m0-m5 finders and candidate API |
| Read NG v3 references | implemented, strict |
| Retired NG v2 (`SREPNG2\0`) | rejected as `UnsupportedVersion`; no writer/reader/compat surface |
| Prototype NG v1 / `.srep2` | rejected as `UnsupportedVersion` |
| Historical SuperREP `.srep` v1–v4 | embedded read-only decoding implemented; independently validated matrix and corruption evidence |
| Shared Match IR and normalization | implemented for supplied candidates |
| Full three-layout match semantics | implemented for supplied candidates |
  | `m0` matching | implemented; deterministic RAM-or-spill CandidateIndex |
  | `m1`/`m2` matching | implemented; exact CDC boundaries, BLAKE3-128 filter, RAM-or-spill CandidateIndex |
 | `m3`/`m4` matching | implemented; fixed-grid exact matching and REP overlay |
  | `m5` matching | implemented; exhaustive fixed-polynomial finder and REP overlay, RAM-or-spill CandidateIndex |
  | Spill / paged CandidateIndex | implemented; bounded memtables spill deterministic temporary runs; exact-key queries and fan-in-16 compaction use bounded temporary storage |
  | Requirement extractor and audit | implemented; 515-unit manifest with curated semantic mappings and real test/evidence existence checks |

Stage8 includes logical 64-bit position coverage beyond 256 MiB and an ignored
release acceptance for actual m1 discovery across a repeated segment more than
256 MiB behind its source. Candidate-driven archive staging is tested separately
and is not treated as finder-discovery evidence.

Requirement traceability is audit-only and does not change the public CLI or
library interfaces. Run `cargo run --locked --bin requirement-audit -- --check`
to validate the versioned manifest, canonical payloads/IDs, curated mappings,
real Rust test targets/items, and safe repository-relative evidence paths. See
`docs/acceptance-traceability.md` for the actionable checklist and honest
outstanding acceptance gates. This audit does not claim that all mapped tests
prove every behavior in their requirement; the true 72-sample fidelity gate,
native Windows/macOS Actions evidence, and final reviewer PASS remain
outstanding.

## Development

The minimum supported Rust version is **1.88.0**. The crate uses `if let`
chains, which stabilized in 1.88; 1.85 is no longer sufficient. Local
Linux commands against that exact toolchain:

```sh
cargo +1.88.0 check --locked --all-targets --all-features
cargo +1.88.0 build --locked --release
cargo +1.88.0 test --locked --all-targets --all-features
```

Those commands are local Linux evidence only. Native Windows and macOS
compile, link, and test results require GitHub Actions
(`windows-latest` / `macos-latest`); a Linux `--target` cross-check is
not a substitute.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

See `CONTRIBUTING.md`, `docs/FORMAT.md`, `CHANGELOG.md`, and `THIRD_PARTY.md`.
The lockfile attribution audit is run with `sh scripts/audit-lock-licenses.sh`.

### Legacy fixture tooling

The committed legacy corpus is authoritative and is never replaced by tooling.
`sh scripts/regenerate-legacy-fixtures.sh --validate` performs a read-only
structural and decoder-backed validation. Developers with the historical binary
may create a retained, append-only corpus with:

```sh
sh scripts/regenerate-legacy-fixtures.sh --generate \
  --old-binary /absolute/path/to/srep
```

The generator exclusively creates a random output leaf directly under the
physical `/tmp/opencode` root, named `srep-legacy-generate.<secret>` or
`srep-legacy-differential.<secret>`; callers cannot select or overwrite the
output path, and deterministic destination arguments are rejected. Differential evidence can be generated with
`--differential --old-binary /absolute/path/to/srep`. The generator creates
randomized `.work-<secret>/evidence-<secret>` directories directly under the
trusted root and retains them on success and failure. Differential outputs are
retained in that evidence directory. It holds the trusted root, output, work,
and evidence directories as descriptors, records their device/inode identities
immediately after creation, and reopens each pathname relative to its held
parent before writes and before reporting success. All private
historical-binary input and output arguments are
`/proc/self/fd/<held-work-fd>/<random-name>` paths; final `vN-checksum.srep`
names are created only after the private result is closed and verified, and
are never passed to the historical binary. Installation uses exclusive
fd-relative creation. The generated corpus must contain exactly the committed
entry names, and every entry must be a no-follow regular file. Failure
diagnostics print textual retained paths only after a final identity check;
otherwise they report held identities and label pathname text untrusted. If an
anchored root or output/work/evidence directory is replaced, the command fails
without printing a success path.
Generated directories, including partial output and evidence from a failed
run, are retained for inspection and are never installed or deleted
automatically. Updating committed fixtures is a separate, human-reviewed
operation outside this tool.

Windows CI runs native tests and release builds on every Windows runner, plus
GNU-target `cargo check` and Clippy. GNU-target tests and builds run only when
the runner provides `mingw32-gcc`; otherwise the native Windows evidence and
GNU check/Clippy evidence remain the supported Stage2 coverage. The workflow
also configures native Linux/Windows/macOS jobs at MSRV 1.88.0 and stable;
this tree does not treat local cross-compilation as that evidence.

# Changelog

All notable changes to this project are documented here. The 0.1.0 preview is
still unreleased: a published version heading will be added only when the
release integrator has confirmed the artifact and its required evidence.

## Unreleased

- Prepared the limited SREP-NG software 0.1.0 preview around the current
  **NGv3** archive format. NGv3 is the read/write format; historical
  SuperREP v1–v4 archives remain embedded read-only inputs.
- Retired NGv1 and NGv2 are rejected as `UnsupportedVersion`. The project does
  not write or promise compatibility with either retired NG format, and split
  indexes remain unsupported.
- The CLI's agreed release interface includes `--version`/`-V` and help for
  `--seed-size` and `--target-chunk`; the default compression method is m3.
- The Linux release package is scoped to
  `srep-v0.1.0-x86_64-unknown-linux-gnu.tar.gz`. Its archive-root contents
  (`README.md`, `CHANGELOG.md`, and the other contracted files) and adjacent
  `SHA256SUMS` verification procedure are documented in the README.
- Added release guidance to back up originals, run archive checks, and compare
  decompressed output before removing source data. The default m3 matcher may
  require substantial RAM and temporary disk on large inputs.
- Prior native CI evidence at commit `8dca8bed` (Actions run
  `34926545492`) remains **source-validation** evidence for that commit only.
  It is not evidence that a 0.1.0 artifact has been packaged or published.
  The retained m1 result used a 258 MiB input and included a match-distance
  witness greater than 256 MiB; it completed in 17.57 seconds. It is a finder
  witness, not full round-trip evidence or a default-m3 performance result.
- The default-m3 Linux release-binary comparison passed (258 MiB deterministic
  mix; `info` reported NGv3 / m3 / index / xxh3; SHA-256 and byte compare
  matched). That is a correctness round trip, not a ratio or performance
  claim. The 72-sample fidelity gate remains deferred until after publication.
- The Linux x86_64 preview binary is dynamically linked and observed GNU libc
  symbol versions up to **GLIBC_2.30**. That is a runtime symbol-version bound,
  not a distro package name. Exact checksums live in `SHA256SUMS` and package
  `NOTICES`, not in this changelog.

## Historical development notes (retained from 8dca8bed)

The bullets below are the pre-0.1.0 `Unreleased` notes as of commit
`8dca8bedc4cd53e268f8f0998871ca3670a497e7`. They are **not** current 0.1.0
claims. Some describe the then-current NG v2 writer, which has since been
retired; current software writes NGv3 only.

- Raised the crate MSRV from 1.85 to 1.88.0. The source uses `if let`
  chains (`src/config.rs`, `src/reference.rs`), which stabilized in
  Rust 1.88; locked dependency `rust-version` metadata remains at 1.85
  and does not require a higher compiler.
- Stage 8 disk spill is implemented: m0-m5 finder history uses deterministic
  budgeted RAM-or-spill CandidateIndex runs with canonical identity handling,
  bounded fan-in compaction, and transactional cleanup and retry behavior.
- Earlier Stage5 state: m1 rolling CDC and m2 order-1 CDC used exact per-block
  boundaries, whole-chunk BLAKE3-128 filtering, exact byte confirmation,
  complete-history semantics, and the deterministic budgeted RAM-or-spill
  CandidateIndex.
  m3/m4 fixed-grid finders and the m3/m4/m5 REP overlay are implemented. M5
  exhaustive matching uses the checked derived seed formula, packed slice
  filtering, exact confirmation, independent extension, and a RAM index.
- Stage 6 review fixes add the validated effective minimum for REP overlays,
  checksum-valid below-threshold decoder rejection, and reproducible m3/m4
  old-vs-new evidence vectors. The historical 3.93a binary is recorded
  read-only; its archive bytes are not stable across runs, so evidence gates
  compare semantic metrics and round trips rather than archive-byte identity.
- Stage 6 evidence review fixes replace incomparable vectors with committed
  same-parameter m3/m4 I/O vectors (`L=16`, minimum 16, 8 KiB blocks), retained
  deterministic MD5 legacy archives, strict duplicate-key/schema/hash/tamper
  validation, and an explicit m4 backward-extension witness. The separate
  m3 `L=3/min=7` vector remains new-only conformance evidence.
- Stage 5 review evidence adds production-core boundary hooks, independent
  m1/m2 state-vector checks, and complete-path digest-collision confirmation.
  The historical `/home/test/.opencode/archiving-tools/srep/bin/srep` is SREP
  3.93a beta and supports m1 fixed-window CDC and m2 order-1 CDC. In the
  retained Stage 5 development smoke corpus and options, its old m1/m2 runs
  yielded zero matches; those runs establish round-trip behavior only and are
  not a semantic-fidelity or ratio baseline.
- Round-2 review fixes: legacy Future-LZ and embedded Index-LZ metadata and
  deliveries use bounded disk-backed stores, output limits are checked before
  reconstruction, terminator handling is strict, and fixture/smoke validation
  is hermetic.
- Earlier Stage3 state: shared public Match IR and deterministic weighted
  normalization for supplied candidates, authoritative equality validation,
  and strict candidate-driven Index-LZ, Future-LZ, and I/O-LZ reference
  encode/decode. The Stage 4 m0 finder and Stage 5 m1/m2 finders use that path;
  m3/m4/m5 and disk spill were implemented in later stages.
- Round-1 legacy reader fixes: embedded v1-v4 decoding, all six legacy
  checksums, committed 24-row fixtures, and bounded private staging were added.
  Split indexes and new match generation remain deferred.
- Added strict embedded legacy `.srep` v1-v4 read support. New compression
  continues to emit only NG v2; split indexes and match generation remain
  deferred.
- Round-2 review fixes: shared working-memory reservations now cover metadata
  and live block buffers; DataBlock structure is checked from its stack header
  before payload allocation; and release smoke coverage was expanded.
- Replaced prototype NG v1 / `.srep2` output with self-contained NG v2
  archives (`.srep` suffix).
- Added XXH3-128 (`twox-hash` 2.1.4) and BLAKE3-256 checksums with the exact
  v2 domains, widths, and low64-LE/high64-LE XXH3 serialization.
- Added structured errors with stable codes, message IDs, and CLI
  `srep: ID: summary[: context]` output.
- Added archive dispatch that rejects prototype NG v1 as `UnsupportedVersion`
  and routes embedded legacy v1-v4 archives to the strict read-only decoder.
- Earlier pre-Stage8 state: library configuration used `Method`, `Layout`, `Checksum`,
  `CompressionConfig`, `RepConfig`, and `ResourceConfig`. Match IR, full
  three-layout match semantics, and m0-m5 matching were implemented; spill
  indexes were added in Stage 8.
- Preserved atomic sibling temporary publication, force/no-force races,
  symlink-entry replacement, hard-link collision checks, and native-path
  `.srep` suffix handling.
- Documented the NG v2 writer, embedded legacy reader, unsupported split
  indexes, and Stage 8 disk-spill behavior.

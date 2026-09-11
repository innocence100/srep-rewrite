# Changelog

## Unreleased

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

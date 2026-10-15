# Changelog

All notable changes to this project are documented here.

## Unreleased

Planned 0.3.0 work: an automatic resource policy / resource defaults. This is a
**plan only**. No 0.3.0 feature, resource-policy behavior, default change, version
tag, or package is implemented or released here, and nothing in 0.2.1 changes the
existing 256 MiB managed-memory or 4 GiB temp-limit defaults. The current
release boundary is described in
[`docs/releases/v0.2.1.md`](docs/releases/v0.2.1.md).

## 0.2.1 — 2026-10-15 (published limited prerelease)

**Status: PUBLISHED as a limited prerelease.** Release
<https://github.com/innocence100/srep-rewrite/releases/tag/v0.2.1>, id
`414169555`, published 2026-10-15T19:03:54Z from source commit
`6f1357fc0ebf1e35d89eceef358bcb2e6bd098ed` (annotated tag
`c11c8ce3f3f550521f9b3e952481a75c92a5d0fb`, which peels to that commit). It is
marked `prerelease` and is not the repository's `latest` release. All nine
assets (three native archives plus six evidence/checksum assets) download and
verify; the native packaging run `38703862425` passed 6/6 jobs, and the CI run
`38703862408` passed 6/6 jobs on a failed-jobs-only rerun after its initial
macOS MSRV attempt ended with a GitHub lost-runner-communication annotation and
no more specific cause established. Hashes, download URLs, runtime notes, and
caveats are in
[`docs/evidence/v0.2.1-publication.md`](docs/evidence/v0.2.1-publication.md).

- Sparse-input m3 optimization (source R-P3 review PASS; final evidence review
  R-P4 PASS): end-order fusion during pass 2, compact authenticated
  candidate/DP rows, and a bounded optional predecessor directory. NGv3 and
  matching semantics are unchanged. On a same-host frozen sample-07 (512 KiB) m3
  comparison of **v0.2.0-labelled binaries**, the archive is byte-identical to
  the v0.2.0 release; elapsed compression fell from 10,638.203 s to 4,298.326 s
  and managed temp peak fell from 9,646,868,416 B to 6,029,489,688 B. That
  comparison is **inherited development evidence**, not a measurement of the
  0.2.1 package, and is not generalized to other inputs. The measured candidate
  peak still exceeds the unchanged 4-GiB default; no 4-GiB pass is claimed and no
  default was raised. See `sparse-optimization-inherited.json` and
  `docs/evidence/post020-sparse-investigation.md`.
- Actual-package acceptance on the downloaded Linux package binary
  `8350d23f7f7c13af96cd1deb156ff6e10554bcbd993726c12c0e549a2b942b0e` (`srep
  0.2.1`) under explicit `--memory 256MiB --temp-limit 32GiB`: 72/72 rows
  completed with byte-identical round trips; `majority-final-xz-v1` PASS — m0
  11/12, m1 10/12, m2 9/12, m3 12/12, m4 12/12, m5 11/12, global 65 wins / 0
  ties / 7 losses, `coverage_geomean ≥ 0.95` for all six (minimum observed
  `0.999748156801`); all 72 per-row final-XZ sizes reproduced the accepted
  optimization candidate's measurements. Managed temp peak was 6,029,489,688 B
  (5.615400 GiB), above the unchanged 4 GiB default; managed RAM peak was
  268435456 B. `--memory` is managed working memory, not a cap on process RSS.
  Report `fidelity-report-release.json`, SHA-256 `85e29e01…`.
- 258 MiB default-settings m3/index/xxh3 round trip on the same package: PASS,
  no method/layout/checksum, memory, or temp-limit override (`--temp-dir` only);
  input/output SHA-256
  `c6c2c4a0735a006faf10ffec7dce05ecd23366a5da071f5d5dfdb3b0eb38f0a8`; archive
  SHA-256 `1bfa2d31be0afbf0880f5a0e4098f9c593b9f643a50523c03b84db4d81f44fc9`.
  Report `large-report-release.json`, SHA-256 `161c6db3…`.
- Native all-target test totals per platform on the six-job run (both the 1.88.0
  and stable jobs, 26 targets each): Linux x86_64 498 passed / 0 failed / 2
  ignored; Windows x86_64 MSVC 493 / 0 / 2; macOS ARM64 499 / 0 / 2. The Windows
  job reported the GNU linker unavailable, so no GNU/MinGW runtime coverage is
  claimed.
- Observed runtime requirements: Linux max GLIBC symbol requirement
  **GLIBC_2.34** (built on Ubuntu 24.04/glibc 2.39; acceptance ran the unmodified
  binary in an Ubuntu 24.04 container because the glibc 2.31 host cannot load
  it); Windows native PE32+ MSVC (`VCRUNTIME140.dll` and API-set imports; not
  ARM64/MinGW); macOS ARM64 native, not publisher code-signed and not notarized,
  carrying an ad-hoc Rust signature (`LC_CODE_SIGNATURE`, flags `0x20002`).
- The published packages' embedded `README.md`/`CHANGELOG.md` are
  pre-publication snapshots at `6f1357fc` and were intentionally not repacked.
- The separate 0.2.0 documentation closeout (runtime requirements, accurate
  install links, and separated archived-vs-actual-package evidence) was a
  documentation-only revision and made no functionality claim.
- Nothing here changes the published v0.1.0, v0.1.1, or v0.2.0 release records;
  no tags were moved and no assets were replaced.

## 0.2.0 — 2026-10-10 (published limited prerelease)

**Status: PUBLISHED as a limited prerelease.** Release
<https://github.com/innocence100/srep-rewrite/releases/tag/v0.2.0>, published
2026-10-10T02:19:26Z from source commit
`6ce0a32b83c625519a427c0d3365e0abfee22d7e` (annotated tag
`eb4d54deee5db1c0a1335f880a6b16aeaceead99`). It is marked `prerelease` and is
not the repository's `latest` release. All 10 assets download and verify; the
native packaging run `37939335761` and CI run `37939335813` each passed 6/6
jobs on that commit. See `docs/evidence/v0.2.0-publication.md` for hashes and
caveats. This document does not alter any existing tag or asset.

- Bounded-memory offload path for internal generation. `--memory` is defined as
  the managed compression working memory: input/output buffers, candidate
  storage, indexes, sorting, WIS/match selection, caches, and their working
  control state. File pathname processing, runtime and allocator overhead are
  excluded, so it is not a hard cap on total process RSS.
- File pathnames (configured `temp_dir`, resolved absolute parent, per-file
  `PathBuf`/`TempPath`) do not consume the `--memory` working budget; a long
  pathname cannot itself trigger a memory-policy rejection. Fixed control owners
  (ledgers, cleanup registry slots, directory/per-file control) remain charged.
- Native release workflow retargeted to `ci/native-release/v0.2.0` with version
  default/fallback artifact `0.2.0`; CI runner allowance raised to 360 minutes
  for all three jobs (the full suite exceeds 50 minutes plus Linux
  debug/release runs).
- Quality policy `majority-final-xz-v1` (explicit user revision): on the fixed
  12-sample corpus a method passes when strictly more than half its complete
  rows have a final-XZ stream strictly smaller than the frozen baseline (ties
  do not win), with `coverage_geomean >= 0.95`, complete 72 rows and
  round-trip integrity retained. The old `size_geomean <= 1.05` and per-row
  `raw_size <= 1.10` limits are retained as diagnostics, not gates; the old
  immutable FAIL report is preserved unchanged.
- No algorithm, ratio, fidelity, or performance claim is made here. Local
  Linux evidence on the dirty pre-version candidate binary (`996dcecf…`) is
  recorded (490-test suite green; 258 MiB round trip PASS; candidate 72-row run
  resource-complete at 72/72 round trips; new-policy verdict PASS, old-policy
  verdict FAIL preserved distinctly). The final **actual-package** evidence on
  the published Linux package binary (`38a3a045…`) is separate: a 72-row
  report (SHA-256 `402ca803…`) with 65 wins / 0 ties / 7 losses and all six
  methods passing the strict-majority gate, plus a 258 MiB default-settings
  round trip PASS. Packaging and publication are complete for 0.2.0.
- **Known release limitation:** a 512 KiB sparse input (sample 07) remains
  hours-level work for some methods. This is disclosed as a limitation of the
  0.2.0 release; no future optimization or performance improvement is claimed.

## 0.1.1 — 2026-09-29 (published limited prerelease)

**Status: PUBLISHED as a limited prerelease.** Release id `399148285`, published
2026-09-29T12:57:56Z from source commit
`8b0e9056fad4e91e63b0cac169afb0ade322cf90` (annotated tag
`bd728b1dc61c9669bd09ced5e4525e70149c2e33`). It is marked `prerelease`. Five
assets are published (three native archives plus `release-provenance.json` and
`SHA256SUMS`) and remain unchanged. The notes below record what 0.1.1 contained
at publication; the 72-row fidelity gap described here was a known limitation of
that release, not a hidden pass, and was carried forward and addressed separately
in 0.2.0.

- Continued maintenance toward 0.1.1 without changing the NGv3 algorithm or
  claiming historical-compressor parity. Native Linux, Windows MSVC, and macOS
  ARM64 packages are built only by the six-job native GitHub Actions workflow;
  source and tooling commits are recorded from `github.sha` and in provenance.
- Preserved NGv3 read/write, historical SuperREP v1–v4 read-only compatibility,
  NGv1/NGv2 rejection, split-index rejection, default m3, MSRV 1.88, and the
  original-preservation warning. The 0.1.0 Linux artifact and tag/assets are
  historical and are not replaced.
- The prior integration handoff reports 12 of 72 fidelity rows, not a
  full-quality pass; its raw logs are no longer available locally. Sample 03 / m0
  remains incomplete after the default 256 MiB memory budget reached an
  out-of-memory condition. The bounded-memory fix belongs to 0.2 and is not
  included here; no rows are uniformly skipped to hide the gap.

- Version and release packaging metadata are aligned at 0.1.1. Portable
  packaging records actual compiler, registry notices, Rust copyright notices,
  source/tooling hashes, binary/archive hashes, extracted version/help, and
  multi-block compress/info/test/decompress/hash smoke evidence.
- The observed runtime bound and hashes for the 0.1.1 packages are recorded in
  each package's `NOTICES` and `BUILD-PROVENANCE.json`. The observed GLIBC_2.30
  bound and 0.1.0 SHA-256 values belong only to the already-published 0.1.0
  Linux asset, not to 0.1.1.

## 0.1.0 — 2026-09-23 (published limited Linux preview)

The following are historical release notes. Fidelity was deferred at publication;
the subsequent incomplete comparison is disclosed above, not claimed as a pass.

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

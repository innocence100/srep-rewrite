# Post-0.2.0 sparse workload optimization investigation

## Scope and result

Task P1 was to materially reduce elapsed time and managed temporary peak for
frozen `fidelity-v1-07` while retaining the exact finder, validation,
normalization, WIS, archive, and resource contracts. This investigation is
**implementation accepted; source R-P3, native/CI gates, and final evidence
review R-P4 passed (optimization round 4/5, global 7). The 4-GiB m3 fit was a
parent stretch target, not the assignment gate**. Exact pipeline and scheduler
optimizations were measured: the unchanged four-match archive result was
reproduced, managed temp peak fell from
9,646,868,416 to 6,029,489,688 B, and elapsed time fell from 10,638.203 to
4,298.326 s in a controlled same-host comparison whose two binaries are both
Rust 1.88.0 builds (the host default toolchain is 1.96.0; see
`docs/evidence/post020-sparse-investigation.md`). The final candidate's measured
managed temp peak, 6,029,489,688 B (5.615400 GiB), still exceeds the unchanged
4-GiB default, and an earlier interim candidate probe at that default failed
with `SREP_E_TEMP_LIMIT`; the final candidate itself was not re-run at 4 GiB.

The assignment tree began clean at `6ce0a32b83c625519a427c0d3365e0abfee22d7e`.
The reviewed candidate/source and release-tooling identity is
`4af679fd4292c3f864db1eb717e48f504e58e78a`; its production-source diff SHA-256
is `0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca`.
The initial cache-sizing experiment was reverted after it failed to help. The
implemented changes are exact representation and ordering transformations:
m3 feeds its distinct representative intervals directly to the exact end-order
sorter instead of first materializing an interval-order table, and WIS uses an
exact charged predecessor directory with binary-search fallback. DP rows omit a
redundant objective count field. The Pass 2 sorter record likewise omits
provenance that exact representative selection does not need, but it keeps
every candidate field including ordinal and is written only after that
unreduced row has been hashed (see the barrier wording below). No candidate is
pruned; all distinct lengths and ordinals remain present.

## Contract invariants for any follow-up

* Keep complete qualifying-emission order, provenance and insertion ordinals.
* Preserve interval representative selection, including the minimum source and
  earliest ordinal for identical keys; keep distinct lengths.
* Do not discard a hypothesis because it is long or repetitive. In particular,
  `[0;512] + [1] + [0;513] + [1]` must retain `(0,514,513)`, not replace it by
  `(0,513,512)`.
* Keep the approved internal m3 two-pass authoritative validation. Pass 1 hashes
  the complete unreduced 96-byte transcript and issues the certificate that
  authorizes Pass 2. Pass 2 hashes every unreduced row during enumeration,
  before that row's representative reduction or sorter flush; the finished full
  transcript/input/session/epoch equality check then completes before the
  schedule input is exposed, not before any reduction.
* Preserve external/non-m3 physical and semantic validation error precedence.
* Keep WIS ordered by sum(`len - 25`), covered bytes, fewer matches, and full
  forward-chain lexicographic `(dst, src, Reverse(len), ordinal)`.
* Keep managed memory at 256 MiB and temporary storage at the existing 4-GiB
  default. Managed memory is the ledgered contract, not an RSS claim.
* Preserve NGv3 and selected-IR bytes, cleanup, cancellation, integrity, and FD
  behavior. No unsafe code or dependencies.

## Baseline profile diagnosis

The immutable release report and profile capture are in the old release tree;
they were read only. For sample07 the released profile reports:

| Method | End-to-end elapsed | Peak managed temp | Notable work |
| --- | ---: | ---: | --- |
| m0 | 12,756.771 s | 356,833,920 B | 2,143,419,054 candidate events |
| m3 | 11,644.848 s | 9,646,868,416 B | 266,033,850 events, 50,241,373 normalized intervals |
| m4 | 3,777.736 s | 297,404,672 B | 266,033,850 candidate events |
| m5 | 3,355.557 s | 317,646,784 B | 534,417,274 candidate events |

For m3, the released first authoritative pass itself was 387.995 s; second
replay/interval preparation reached 2,877.053 s and 50,241,373 representatives.
The released peak was 9,646,868,416 B (**8.984346 GiB**). In the controlled
same-host Rust 1.88.0 comparison the baseline reached `schedule_done` at
10,638.131 s and `complete` at 10,638.133 s (wall 10,638.203 s), with the same
9,646,868,416 B peak. The candidate reached `m3_pass2_end_order` in 2,774.079 s
with 50,241,373 rows and peak 5,627,563,544 B, then `schedule_done` at
4,298.256 s and wall 4,298.326 s, peak 6,029,489,688 B. The exact per-event
figures and report hash are recorded in
`docs/evidence/post020-sparse-investigation.md`. The final candidate was not
re-run at the 4-GiB default; its measured peak already exceeds that default.

## Implemented exact transformations

* **Direct end-order m3 table.** Pass 2 still scans every emitted row in
  original order, hashing each unreduced row before its representative
  reduction or sorter flush, and keeps the same smallest `(src, ordinal)` for
  each exact `(dst,len)` key; evictions still go to the exact external sorter.
  The completed transcript/certificate/input/epoch equality check runs before
  the schedule input is exposed. The sort comparator is the established exact
  WIS end-order key. This eliminates an interval-order table plus a full second
  end-sort/materialization.
* **Compact authenticated sort row.** For the internal m3 Pass 2 sorter,
  provenance is irrelevant to exact representative selection; the distinct
  candidate tuple (including ordinal) is retained with its record authentication
  envelope. Interval/end rows are 56 instead of 64 bytes. External validation
  continues using full candidate rows and its prior error precedence.
* **Compact authenticated DP row.** The score identity `covered = gain + 25*count`
  makes `count` derivable. The v2 record keeps gain, coverage, best-node,
  include-parent, and include-depth, validates the identity on write, and checks
  checked subtraction/divisibility on read. This narrows the record 96 to 64 B.
* **Predecessor directory.** A cancellable sequential pass builds an optional
  charged coordinate-to-exclusive-row-prefix directory only if its checked u64
  size fits a bounded share of currently free managed memory. Admission leaves
  at least half of current free memory, or 16 MiB (clamped to free bytes), for
  later WIS allocations. Coordinate-size conversion or memory admission failure
  uses the prior exact binary search; I/O/cancel/corruption errors propagate.
  Direct tests compare every coordinate to a separate end-prefix scan and force
  near-budget fallback.

The WIS recurrence, full-chain comparator, DP and reverse-chain integrity
barriers, backtrace, selected emission, and objective remain unchanged. A
prototype of group-compressed WIS was not retained: an early implementation
failed the exhaustive brute-force oracle and existing fault-order tests, so it
was removed rather than weakening those tests or leaving an unverified route.

## Follow-up opportunities (not blockers to completed source/native gates)

The 32-GiB diagnostic still peaks above the 4-GiB default; further exact storage
reduction or a proven streaming external-memory WIS design could improve temp
use further.
These are follow-up/stretch opportunities, not blockers to this assignment:

1. Evaluate a streaming/external-memory WIS design that preserves the exact
   score and full-chain tie comparator and proves its peak-live-file accounting
   under the same limit. Include a counterexample oracle and byte-for-byte IR
   and archive comparison.
2. Separately evaluate whether m3's proven periodic exact-interval structure
   permits a theorem-backed compressed schedule input. It must run only after
   the full m3 transcript/certificate validation, retain every WIS-relevant
   candidate, and establish full-chain lexicographic equivalence. The sparse
   sample is not itself a proof and cannot justify pruning.
3. Keep all source-index runs, validation passes, and table integrity reads
   covered by cancellation, fault-injection, FD, cleanup, and temp-ledger tests.
4. Compare the actual frozen sample and sample03 before/after serially on the
   same runtime, recording exact archive/IR hashes. No 4-GiB pass is claimed.

The P1 assignment authorizes this shared internal architecture scope. No public
interface was changed. D1-owned published-release docs, Cargo files, and the
release tree were not modified. A prototype group-compressed WIS route was
removed after failing the exhaustive oracle and fault-order tests; the retained
production scheduler is the established row-per-candidate route with the
independently tested bounded predecessor directory.

## Evidence and limitations

The frozen recipe is defined by `scripts/fidelity.py` and the checked-in corpus
manifest (sample SHA-256
`0bdbc925b10ebdb128d72b651d56dbae7f539e16b6b2c24e87c837cebc0bc8df`). An
existing report source path in the old JSON no longer exists, so that path was
not treated as input evidence. The frozen sample was reconstructed with the
repository recipe in the external perf directory and its hash checked before
the attempted compression.

The baseline release binary requires GLIBC 2.34; its actual released execution
occurred in the retained Ubuntu 24.04 container. The controlled baseline is a
fresh Rust 1.88.0 build of the frozen source, compared serially against the
Rust 1.88.0 candidate on this same Ubuntu 20.04 host. The verified archive is
SHA-256
`ab697abb85843423e917e58e582b27df32facc6878041585986cb8e28fec4c2a`, identical
to the retained released m3 sample07 archive. The same-host harness ran
compress/info/test/decompress and compared archive identity under its 32-GiB
diagnostic budget. The final candidate was not re-run at the 4-GiB default; its
measured peak already exceeds that default, and earlier **interim**, not
identity-bound, 4-GiB probes failed with `SREP_E_TEMP_LIMIT`. Invalid-path
artifacts remain excluded.

A durable local verification pass (V1) completed on 2026-10-14: all 25 commands
exit 0 at MSRV Rust 1.88.0, the source diff SHA-256 was unchanged
`0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca`, the
all-target suite recorded 498 passed / 0 failed / 2 ignored across 26 targets
per run (the library target alone is 258 plus 1 ignored, not the whole total),
and six release timing gates passed. The V1 sparse harness had 59/59 tests; the
later final integration check recorded under `r2-harness` passed 66/66 tests.
Record: `/tmp/opencode/srep-post020-verify/status.json` (SHA-256
`ea26cbd856a7ce444f3ba370075c13d36c78fac7997a6458a53a495549e8807e`).

Native release workflow [run 38510825260](https://github.com/innocence100/srep-rewrite/actions/runs/38510825260)
and push CI [run 38510825006](https://github.com/innocence100/srep-rewrite/actions/runs/38510825006)
both completed successfully, six of six jobs each, on source/tooling commit
`4af679fd4292c3f864db1eb717e48f504e58e78a`. Native optimized-test steps and
timing gates passed for Linux, Windows MSVC, and macOS ARM64 with stable and
Rust 1.88.0. Every suite's per-target totals, profile (debug/release), elapsed
time, ignored-test identities, artifact provenance, and smoke evidence are
recorded in `docs/evidence/post020-sparse-investigation.md` and the external
receipt `/tmp/opencode/srep-post020-native/verification-receipt.json`.

Source R-P3, both CI runs, and independent final evidence review R-P4 are PASS;
the implementation task is accepted. The source change remains unreleased. CI artifacts are
verification outputs named for manifest version 0.2.0, not replacements for or
byte-identity claims about published v0.2.0 assets. No new release, tag, or
published package was created. Historical source-baseline and benchmark
bindings above remain as originally recorded.

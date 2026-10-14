# P1 sparse optimization implementation and evidence

Status: **SOURCE R-P3 REVIEW PASS; native release and push CI PASS (6/6 jobs
each); final evidence review R-P4 PASS (optimization round 4/5, global 7)**.
Exact candidate and
output semantics were preserved. A controlled same-host sample07 m3 comparison,
a full72 fidelity re-evaluation, a 258-MiB round trip, and a durable
all-target/CLI verification run are recorded below with specific artifacts. The
final sample07 m3 candidate still peaks above the unchanged 4-GiB temp default;
that fit was a parent stretch target, with no changed resource default and no
4-GiB pass claim.
Native Windows/macOS validation was completed by native GitHub Actions runners;
no Windows/macOS binary was executed on this Linux host.

## Identity and environment

* Baseline source: `6ce0a32b83c625519a427c0d3365e0abfee22d7e`.
* Frozen corpus recipe: `scripts/fidelity.py`, `fidelity-v1-07`, vm-disk-sparse,
  524,288 bytes, seed 16,646.
* Actual same-host benchmark input
  `/tmp/opencode/srep-post020-perf/samehost-final/work/fidelity-v1-07.bin`
  (the report's `sample.source`/`input.path`): length 524288 and SHA-256
  `0bdbc925b10ebdb128d72b651d56dbae7f539e16b6b2c24e87c837cebc0bc8df`. The
  unused `/tmp/opencode/srep-post020-perf/fidelity-v1-07.bin` copy is not the
  benchmark input.
* Published release binary SHA-256:
  `38a3a04535b8db36c10c466c3243bf8e9f1477ce505630ddb73af2afc2faea1c`.
  It requires GLIBC 2.34 and cannot run on this Ubuntu 20.04 (GLIBC 2.31) host;
  the published binary's actual acceptance run was in the retained Ubuntu 24.04
  container.
* Candidate binary: Rust 1.88.0 release build at
  `/tmp/opencode/srep-post020/target/release/srep`, SHA-256
  `d8408a3049ada27307fc448a31bd03ed7825feb407a6c0d1f71fbb5aa617f29a`; its ELF
  `.comment` reports `rustc version 1.88.0 (6b00bc388 2025-06-23)`.
* Baseline binary: fresh Rust 1.88.0 release build of the base commit at
  `/tmp/opencode/srep-post020-integration/target/release/srep`, SHA-256
  `ceba8e8eec33ce6265f21c5b8f87f36782a879376ee5401ee73a5be92eaff522`; its ELF
  `.comment` also reports Rust 1.88.0.
* The host default toolchain is Rust 1.96.0, which is what the benchmark
  harness records in its `host.toolchain` field, but **both** compared binaries
  are Rust 1.88.0 builds. The comparison is not a 1.96.0 comparison.
* Runtime stage profiles are enabled with `SREP_OFFLOAD_PROFILE=1`.

## Historical release reference

The published release report/profile remain read-only historical context. The
controlled same-host comparison is recorded separately below.

| Method | Elapsed (ms) | Peak managed temp (B) |
| --- | ---: | ---: |
| m0 | 12,756,771 | 356,833,920 |
| m3 | 11,644,848 | 9,646,868,416 |
| m4 | 3,777,736 | 297,404,672 |
| m5 | 3,355,557 | 317,646,784 |

Historical m3 produced 266,033,850 qualifying events and 50,241,373 reduced
intervals. Its transcript-authoritative first pass took 387,995 ms; the second
pass and old intermediate interval preparation took 2,877,053 ms; the WIS stage
completed at 11,644,845 ms. Managed-temp peak 9,646,868,416 B is **8.984346
GiB**, not 9.646 GiB. These are managed-ledger bytes, not physical disk or RSS.

## Controlled same-host sample07 comparison

Baseline source was freshly extracted from `6ce0a32b83c625519a427c0d3365e0abfee22d7e`
to `/tmp/opencode/srep-post020-integration` and not modified. Baseline and
candidate binaries were built serially with Rust 1.88.0, default release
settings, on the same host; the host default toolchain (1.96.0) was not used
for either binary. Baseline binary SHA-256 is
`ceba8e8eec33ce6265f21c5b8f87f36782a879376ee5401ee73a5be92eaff522`; candidate
binary SHA-256 is
`d8408a3049ada27307fc448a31bd03ed7825feb407a6c0d1f71fbb5aa617f29a`. Candidate
production-source diff SHA-256 is
`0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca`.

The two binaries ran serially on the same verified frozen input (524,288 bytes,
SHA-256 `0bdbc925b10ebdb128d72b651d56dbae7f539e16b6b2c24e87c837cebc0bc8df`),
same m3/index/xxh3/8KiB/min512 arguments, and explicit 256MiB/32GiB budgets.
Report `/tmp/opencode/srep-post020-perf/samehost-final/report.json` (SHA-256
`2de4e10935225eed4304331f5af2eec9e3c3cfe0aee063228345eca5fad39b50`) is
`pass=true`/`complete=true`; the harness strictly validated profiles and info,
tested each archive, decompressed and compared the input, compared archive
identity, and verified both private temp directories empty.

| Measurement | Fresh baseline | Candidate | Candidate / baseline |
| --- | ---: | ---: | ---: |
| m3 compress wall | 10,638.203 s | 4,298.326 s | 0.404046 |
| m3 `schedule_done` | 10,638.131 s | 4,298.256 s | 0.404044 |
| peak managed temp | 9,646,868,416 B | 6,029,489,688 B | 0.625020 |
| managed logical I/O | 30,633,034,000,684 B | 6,441,746,945,156 B | 0.210288 |
| managed RAM peak | 268,435,456 B | 268,435,456 B | 1.000000 |
| archive bytes | 3,003 | 3,003 | exact |

The candidate's `m3_pass2_end_order` event was at 2,774.079 s with peak
5,627,563,544 B, and its `complete` event at 4,298.258 s. The baseline's
analogous `m3_pass2_intervals` event was at 2,802.682 s with peak
6,431,426,240 B; its `complete` event was at 10,638.133 s. These are separate
per-method profile events, not a mixed before/after row.

Both archive SHA-256 values are
`ab697abb85843423e917e58e582b27df32facc6878041585986cb8e28fec4c2a`. Both
selected 4 semantic matches, 521,728 covered bytes, and 2,560 literal bytes
(2,816-byte payload, 64 blocks). Thus same-host elapsed fell 59.6%, managed temp
peak 37.5%, and logical I/O 79.0%, with unchanged archive identity and summary.
Same-host elapsed is the primary comparison. The explicit 32-GiB limit is
diagnostic; no 4-GiB success is claimed.

The earlier interim `--temp-limit 32GiB` diagnostic
(`/tmp/opencode/srep-post020-perf/fidelity-v1-07-m3-endonlyreduce.log`) ran on a
different, earlier candidate build (not the candidate binary above) and reached
`m3_pass2_end_order` at 2,008.175 s and `complete` at 3,469.739 s with peak
6,029,489,688 B. Those values are **interim and not the final candidate** and
must not be mixed with the table above.

## Production design and equivalence argument

1. The m3 pipeline still performs the original complete Pass 1 and Pass 2
   enumeration, unreduced 96-byte transcript, authoritative semantic checks,
   and certificate/session/input/epoch binding. Pass 1 finishes the full
   transcript and issues the certificate; Pass 2 hashes every unreduced row
   during enumeration before that row's representative reduction, and its
   finished full transcript/input/session/epoch equality check completes before
   the schedule input is returned.
2. It retains the exact representative key `(dst,len)` and exact winner
   `(smallest src, earliest insertion ordinal)`. Distinct lengths remain
   separate; collisions evict to the exact external sorter. No candidate is
   dropped for being long or repeated.
3. For internal m3 only, representative rows are fed directly to the exact WIS
   end-order sorter `(dst+len,dst,src,Reverse(len),ordinal)`, removing the prior
   interval-order table and second full external end-sort. Equal `(dst,len)`
   alternatives remain adjacent in both orders, so the same representative
   reducer applies.
4. The m3 Pass 2 private sorter record omits provenance unused by exact
   `(dst,len)` representative selection, but retains every candidate field
   including ordinal and is hashed as part of the per-row unreduced transcript
   before that row's reduction or flush. The Pass 1 certificate authorizes
   Pass 2; Pass 2 hashes all unreduced rows during enumeration and completes the
   full transcript/input/session/epoch equality check before the schedule input
   is returned. The transcript golden remains unreduced and unchanged. The
   external/non-m3 canonical validation route and its error precedence are
   unchanged.
5. DP count is algebraically determined by `covered = gain + 25*count`; the new
   authenticated row omits only this redundant field and validates checked
   subtraction/divisibility. All decision, parent, depth, objective, and tie
   fields remain present.
6. WIS now builds a budgeted coordinate-to-prefix directory in one ordered pass
   and uses O(1) predecessor lookup. The full input still passes the existing
   order/semantic scan. Directory admission uses a checked size and is capped
   to leave at least half of current free memory, or 16MiB clamped to free bytes,
   for later mandatory WIS allocations. If conversion/admission fails, the old
   exact binary-search path remains. Cancellation is checked while building it.
   Equal score comparisons still use the prior complete forward-chain walk.

The original archived m3 profile had four selected matches. The new diagnostic
archive reports the same four, coverage, literals, block count, and size; the
archive SHA-256 exactly matches the released sample07 m3 archive.

## Final frozen sample runs and 4-GiB status

Every same-host final command used the checked input and the same frozen
options: method m3, index, xxh3, 8KiB block, min-match 512, 256MiB managed
memory, explicit `--temp-limit 32GiB`, `SREP_OFFLOAD_PROFILE=1`, and a private
temp directory. The final candidate's compress-stage managed peak in that run
was 6,029,489,688 B (5.615400 GiB), which already exceeds the unchanged 4-GiB
default. The final candidate was **not** re-run at `--temp-limit 4GiB`, so this
record does not assert a directly verified 4-GiB failure on the final binary.

An earlier interim candidate build was probed at `--temp-limit 4GiB`:
`/tmp/opencode/srep-post020-perf/fidelity-v1-07-m3-final4.log` ended at
2,666.145 s with exit 14 `SREP_E_TEMP_LIMIT`, profile `complete ... ok=false`,
peak 4,294,967,272 B, max RSS 75,348 KiB, and no output archive. A repeat
`fidelity-v1-07-m3-final4-verified.log` ended at 2,662.190 s with the same
budget and exit. Those logs are not bound to a recorded binary identity and are
treated as **interim, non-final** evidence only. The 4-GiB fit was a parent
stretch target, not a user acceptance gate, and the default budget was not
raised.

Archive identity for the final candidate comes from the same-host report above:
SHA-256 `ab697abb85843423e917e58e582b27df32facc6878041585986cb8e28fec4c2a`,
3003 bytes, 64 blocks, 4 semantic matches, 521,728 covered bytes, 2,560 literal
bytes, 2,816-byte payload. `srep info`, `srep test`, and decompression all
exited 0 in the harness stages, and the harness compared the recovered bytes to
the verified input. The older endonlyreduce diagnostic
(`/tmp/opencode/srep-post020-perf/fidelity-v1-07-m3-endonlyreduce.log`) is the
interim build noted above, not the final candidate.

## Verification status

### Completed durable verification (V1)

Durable independent verification (V1) completed on 2026-10-14, from
`2026-10-14T02:56:21.311+00:00` to `2026-10-14T03:56:51.484+00:00` UTC, using
the MSRV Rust 1.88.0 toolchain (`cargo 1.88.0`), `RUST_TEST_THREADS=1`, and
`RUSTDOCFLAGS=-Dwarnings`. Source diff SHA-256 was
`0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca` before and
after (unchanged), and its release decoder rebuild SHA-256 is
`d8408a3049ada27307fc448a31bd03ed7825feb407a6c0d1f71fbb5aa617f29a`, matching
the same-host candidate binary.

Machine record: `/tmp/opencode/srep-post020-verify/status.json` (SHA-256
`ea26cbd856a7ce444f3ba370075c13d36c78fac7997a6458a53a495549e8807e`), with
per-command logs `01-fmt.log` … `25-diff_check.log` and the summary
`/tmp/opencode/srep-post020-verify/verification-summary.md`. All 25 commands
exited 0 (`status: passed`).

| # | Command | Log |
| --- | --- | --- |
| 01 | `cargo fmt --all -- --check` | `01-fmt.log` |
| 02 | `cargo check --locked --all-targets --all-features` | `02-check.log` |
| 03 | `cargo run --locked --bin requirement-audit -- --check` | `03-requirement_audit.log` |
| 04 | `cargo clippy --locked --all-targets --all-features -- -D warnings` | `04-clippy.log` |
| 05 | `cargo test --locked --all-targets --all-features -- --test-threads=1` (debug) | `05-test_debug.log` |
| 06 | `cargo test --locked --release --all-targets --all-features -- --test-threads=1` | `06-test_release.log` |
| 07 | `cargo test --locked --doc` | `07-test_doc.log` |
| 08 | `cargo doc --locked --no-deps` | `08-doc.log` |
| 09 | `cargo build --locked --release` | `09-build_release.log` |
| 10 | `cargo build --locked --bin srep --example stage6-fixed-metrics --example stage7-m5-metrics` | `10-build_binaries_examples.log` |
| 11 | `sh scripts/audit-lock-licenses.sh` | `11-audit_lock_licenses.log` |
| 12 | `sh scripts/test-license-audit.sh` | `12-test_license_audit.log` |
| 13 | `python3 scripts/test-ci-yaml.py` | `13-test_ci_yaml.log` |
| 14 | `python3 scripts/test-native-release-workflow.py` | `14-test_native_release_workflow.log` |
| 15 | `python3 scripts/test-fidelity-evaluator.py` | `15-test_fidelity_evaluator.log` |
| 16 | `python3 scripts/test-benchmark-sparse.py` | `16-test_benchmark_sparse.log` |
| 17 | `sh -n scripts/regenerate-legacy-fixtures.sh` | `17-shell_syntax.log` |
| 18 | `python3 -m compileall -q scripts` | `18-python_compileall.log` |
| 19 | `python3 scripts/compare-fixed-finders.py --decoder target/debug/srep` | `19-compare_fixed_finders.log` |
| 20 | `python3 scripts/compare-m5-finder.py --decoder target/debug/srep --metrics-binary target/debug/examples/stage7-m5-metrics` | `20-compare_m5_finder.log` |
| 21 | `python3 scripts/validate-legacy-fixtures.py --acceptance --decoder target/debug/srep` | `21-validate_legacy_fixtures.log` |
| 22 | `python3 scripts/run-legacy-corruptions.py target/debug/srep` | `22-legacy_corruptions.log` |
| 23 | `python3 scripts/test-legacy-tools.py` | `23-legacy_tools.log` |
| 24 | `sh scripts/release-smoke.sh` | `24-release_smoke.log` |
| 25 | `git diff --check` | `25-diff_check.log` |

`requirement-audit --check` reported `515 extracted; 363 active; 152 retired
historical` and passed. fmt, check, Clippy, docs, builds, license audits, CI
YAML/native-workflow/fidelity/sparse-benchmark tests, shell syntax, Python
compileall, finder comparisons, legacy acceptance/corruption/tool checks,
release smoke, and diff check all passed with logged output.

### All-target test accounting

Parsed independently from the raw result lines in `05-test_debug.log` and
`06-test_release.log`: **each** run recorded 26 test-target result lines
totalling **498 passed, 0 failed, 2 ignored**. The `src/lib.rs` target alone is
258 passed, 0 failed, 1 ignored; 258 is not the all-target total (an earlier
draft mislabeled it as such). The two ignored manual Rust tests are
`offload::generation_tests::profile_sample03_m3_intervals_only` and
`stage8::ignored_m1_finder_discovers_a_match_across_256_mib_history`. Doc tests
(`07-test_doc.log`) are counted separately: 0 passed, 0 failed, 2 ignored.

### Release performance gates

All six release timing gates were present and passed in `06-test_release.log`:
`tests/fixed_performance.rs` 5/5 (frozen sample03 m3 and m4 under their probes),
`tests/m0.rs` 15/15 (including
`m0_repeated_128k_block_has_bounded_completion_and_coverage` and
`m0_frozen_exact_repeat_compresses_end_to_end_within_bound`), and
`tests/m5_performance.rs` 4/4 (including the far-distance 30 s and 180 s
probes). The frozen sample03 regression archive SHA-256 remains
`1f3f7e77621721e48c31ffd648d59382c5a504d34aafbc8c2fb66294b361c292`.

### Benchmark evidence

* Full 72-row fidelity re-evaluation:
  `/tmp/opencode/srep-post020-perf/full72-attempt2/fidelity-report.json`
  (SHA-256 `1dba0d08596a295e1dd7b2e5034d7449489c23e49cbe3e01f38920aa198f46de`),
  `complete=true`, `pass=true`, quality policy `majority-final-xz-v1`. All 72
  rows round-trip and all 360 recorded stages exit 0; the corpus/baseline
  manifest hashes and all 72 rows' per-method metrics are identical to the
  immutable published `fidelity-report-release.json` (SHA-256 `402ca803…`).
  This is a fidelity re-evaluation, not a resource profile.
* 258-MiB default-method round trip:
  `/tmp/opencode/srep-post020-perf/large258/report/report.json` (SHA-256
  `509bb034aa930871e3badde0d2401b2e039c725ff057a7ae23b62414f08e2bf8`), recipe
  `repeat-1mib-separated-256mib-unique-v1`, 270,532,608 bytes, default
  method/layout/checksum with explicit `--memory 256MiB --temp-limit 4GiB`,
  `state PASSED`. It records input and output SHA-256
  `c6c2c4a0735a006faf10ffec7dce05ecd23366a5da071f5d5dfdb3b0eb38f0a8`, archive
  SHA-256 `1bfa2d31be0afbf0880f5a0e4098f9c593b9f643a50523c03b84db4d81f44fc9`,
  and a `cmp` step exit 0. This is a recorded round-trip/hash comparison; the
  raw input and output were removed after the runner, so it is not a fresh
  independent byte compare performed now.
* Sparse benchmark harness (initial V1): `16-test_benchmark_sparse.log` records
  `Ran 59 tests in 18.181s` / `OK` (59 passed, 0 failed). This was superseded
  after the S1 launch-window fix; it remains the immutable V1 result. The
  final integration check is recorded below. Both test runs cover strict
  profile/info gates, binary/input identity, bounded timeout process-group
  cleanup, serial baseline/candidate invocation, and archive/round-trip/temp
  checks. The earlier
  `/tmp/opencode/srep-post020-perf/benchmark-harness-tests-final.log` (41 tests)
  is **historical and obsolete** and is not the current harness result.
* Real-profile revalidation:
  `/tmp/opencode/srep-post020-harness/revalidation.json` re-parses the same-host
  compress profiles. Both the baseline (`m3_pass2_intervals`) and candidate
  (`m3_pass2_end_order`) targets are `PASS` with `final_ok=true`,
  `malformed_count=0`, and unchanged hashes, and the source report SHA-256
  `2de4e10935225eed4304331f5af2eec9e3c3cfe0aee063228345eca5fad39b50` is
  unchanged before/after. Both full72 reports are `N/A-NO-PROFILE` (0
  `OFFLOAD_PROFILE` lines), so no full72 resource claim is made.

### Final S1 launch-window integration recheck

With production Rust source unchanged, `python3 -m compileall -q scripts`
passed and `python3 scripts/test-benchmark-sparse.py` passed all **66 tests**
in 17.700 s (0 failures). Durable output:
`/tmp/opencode/srep-post020-verify/r2-harness/tests.out`; hashes and captured
tool/binary/source identities are recorded in
`/tmp/opencode/srep-post020-verify/r2-harness/verification.json`. The retained
profile revalidation also exited 0:
`/tmp/opencode/srep-post020-verify/r2-harness/revalidation.json`. Both baseline
(`m3_pass2_intervals`) and candidate (`m3_pass2_end_order`) profiles are
`PASS`, `final_ok=true`, with zero malformed events and unchanged raw profile
hashes; the source report hash is unchanged. The production-source diff remains
SHA-256 `0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca`;
debug/release candidate binary hashes remain the V1 values. `git diff --check`
passed. No Rust suites were repeated because the Rust source is unchanged. This
refreshes harness/profile validation only; it does not alter the V1 25-command
evidence or any retained benchmark report/profile.

### Native and regular CI evidence (source R-P3 and final evidence R-P4 PASS)

Both workflows checked out immutable source and release tooling at
`4af679fd4292c3f864db1eb717e48f504e58e78a` (the reviewed production-source diff
from `6ce0a32b83c625519a427c0d3365e0abfee22d7e` remains SHA-256
`0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca`). All six
jobs succeeded in each run; no required job was skipped or failed:

* Native release workflow run [38510825260](https://github.com/innocence100/srep-rewrite/actions/runs/38510825260),
  source `4af679fd4292c3f864db1eb717e48f504e58e78a`: six native host/toolchain
  checks and optimized test steps passed. The three Rust 1.88.0 jobs also passed
  package/extract smoke and uploaded their platform artifacts. Stable matrix
  entries intentionally do not package/upload artifacts; those skipped
  per-step packaging actions are not skipped required jobs.
* Push CI run [38510825006](https://github.com/innocence100/srep-rewrite/actions/runs/38510825006),
  same source SHA: `windows-test`, `verify`, and all four native OS/toolchain
  test jobs passed. `verify` includes independent debug and release all-target
  profiles; both are recorded separately below.

The counts below are sums of every Cargo test-target summary line in each
profile, not just the `src/lib.rs` target. Per-target distribution differs by
host. Each all-target profile has 26 summaries (258/0/1 is the library target,
not the overall count); ignored tests in both all-target runs are
`offload::generation_tests::profile_sample03_m3_intervals_only` (manual
sample03 enumeration-volume diagnostic) and
`stage8::ignored_m1_finder_discovers_a_match_across_256_mib_history` (manual
>256-MiB m1 finder acceptance). The first is unrelated to archive/validation
bypass and the second is the long manual history diagnostic; neither is an
asserted all-target test result. CI `verify` doc-tests have 0 passed/0 failed/2
ignored and are separately summarized below.

| Workflow / job profile | Host / target | Rust | 26-target all-target total (passed / failed / ignored) | Sum of target seconds |
| --- | --- | --- | ---: | ---: |
| Native release `115678691489` | Ubuntu / x86_64 Linux | 1.88.0 | 498 / 0 / 2 | 283.33 |
| Native release `115678691528` | macOS / aarch64 Darwin | 1.88.0 | 499 / 0 / 2 | 411.53 |
| Native release `115678691531` | Windows / x86_64 MSVC | 1.88.0 | 493 / 0 / 2 | 425.13 |
| Native release `115678691545` | Ubuntu / x86_64 Linux | stable | 498 / 0 / 2 | 256.74 |
| Native release `115678691582` | macOS / aarch64 Darwin | stable | 499 / 0 / 2 | 394.38 |
| Native release `115678691408` | Windows / x86_64 MSVC | stable | 493 / 0 / 2 | 489.51 |
| Push CI `115678686639` | Ubuntu / x86_64 Linux | 1.88.0 | 498 / 0 / 2 | 2446.38 |
| Push CI `115678686688` | macOS / aarch64 Darwin | 1.88.0 | 499 / 0 / 2 | 5663.31 |
| Push CI `115678686725` | Windows / x86_64 MSVC | 1.88.0 | 493 / 0 / 2 | 1773.16 |
| Push CI `115678686486` | macOS / aarch64 Darwin | stable | 499 / 0 / 2 | 5473.99 |
| Push CI `115678686372` | Windows / windows-test (GNU target checks plus native Windows tests) | stable | 493 / 0 / 2 | 2477.45 |
| Push CI `115678686452` | Ubuntu / x86_64 Linux `verify`, debug profile | stable | 498 / 0 / 2 | 1169.48 |
| Push CI `115678686452` | Ubuntu / x86_64 Linux `verify`, release profile | stable | 498 / 0 / 2 | 215.62 |

The native-release run's `optimized native tests without weakening timing
assertions` step passed in all six jobs. These are the workflow's optimized
release tests and their configured timing gates; no assertion was weakened.
The CI matrix counts are an additional debug/test evidence profile, not
substitute figures for those optimized native-release results.

### Native package validation and preservation

The native workflow's three uploaded development artifacts each contain a
package archive and accompanying `smoke.json`. Each smoke record shows version
`srep 0.2.0`, two successful round trips, ten successful CLI steps, matching
input/restored hashes, and all steps exit 0. The archives contain platform
binary, `BUILD-PROVENANCE.json`, `LICENSE`, `NOTICES`, `README.md`,
`PLATFORM-README.md`, `CHANGELOG.md`, `THIRD_PARTY.md`, and `THIRD_PARTY.audit`.
Embedded provenance binds source and tooling to
`4af679fd4292c3f864db1eb717e48f504e58e78a`, clean worktrees, and version
0.2.0. The run API's SHA-256 is for the enclosing Actions artifact; separate
SHA-256 values in the evidence receipt are for the extracted package archive,
and each package's `SHA256SUMS` agrees with that archive digest.

These are **unreleased development-CI artifacts**, version-labeled 0.2.0 for
the tested manifest. They are not the published v0.2.0 packages and must not
be represented as replacements or identical bytes. No tag/release/asset write
occurred. At native evidence capture, `main` was
`f77e1068085446687b2dc30c6ebec004c9a098f1`; post-review fast-forward handling
is separate and is not asserted here. At capture, the v0.1.0, v0.1.1, and
v0.2.0 release assets and refs matched the prior publication receipt.

Raw run/job JSON, Actions log ZIPs, extracted native artifacts, and a structured
per-job receipt are preserved outside the repository under
`/tmp/opencode/srep-post020-native/`; the structured receipt is
`verification-receipt.json` (the chronological native-evidence receipt predates
the final R-P4 verdict). Source implementation acceptance is complete; the
optimization remains unreleased. No optimization release, version tag, or new
published package is claimed.

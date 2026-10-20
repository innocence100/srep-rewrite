> Sanitized historical derivative; not a new verification or current status. See ../README.md and ../manifest.json for provenance and redactions. Embedded hashes identify original evidence/assets, not this derivative. Paths and commands are historical references, not replay-ready instructions.

# Persisted [redacted internal task] run summary

- Repository: `<workspace>/post-v0.2.0`
- HEAD: `f77e1068085446687b2dc30c6ebec004c9a098f1`
- Rust/Cargo: 1.88.0 (`rustc 1.88.0 (6b00bc388 2025-06-23)`)
- Started: `2026-10-14T02:56:21.311+00:00`; completed: `2026-10-14T03:56:51.484+00:00`
- Runner PID: [redacted internal identifier]; final status: **passed**, exit 0.
- All 25 commands listed in `status.json` completed, each with exit code 0. `RUST_TEST_THREADS=[redacted environment value]` (single test thread) for test invocations; Rust commands were explicitly run via `rustup run 1.88.0`. `RUSTDOCFLAGS=[redacted environment value]` (warnings denied) was set for the runner.
- Source diff SHA-256 before and after: `0e8d8c1ee23565c09115b00074821270d4c12222066efcaf7862ec4465dc4bca` (same as expected; no source edits during verification).
- Debug decoder SHA-256 at completion: `a70c489864bbb29322c1d0481ffc869e2a62d3a3eb5d577442a6947610e65eac`.
- Release decoder SHA-256 after release tests/build/release-smoke: `d8408a3049ada27307fc448a31bd03ed7825feb407a6c0d1f71fbb5aa617f29a` (matches the supplied expected identity).

## Test accounting (raw Cargo all-target logs)

Across the 26 test-target result records for **each** all-target run: 498 passed, 0 failed, 2 ignored. Do not describe 258 as the all-target total: that is only the library target.

- Debug: `05-test_debug.log`; passed (exit 0).
- Release: `06-test_release.log`; passed (exit 0).
- In each run, library target: 258 passed, 0 failed, 1 ignored; all remaining binary/integration/example target result records are retained in the raw log.
- Manual ignored Rust tests (also ignored in the release run):
  - `offload::generation_tests::profile_sample03_m3_intervals_only` — diagnostic.
  - `ignored_m1_finder_discovers_a_match_across_256_mib_history` — manual >256 MiB acceptance.
- Doc tests: `07-test_doc.log`; 0 passed, 0 failed, 2 ignored (separate from all-target aggregate). Native manual ignored doctests are reported there.
- Sparse benchmark script tests: `16-test_benchmark_sparse.log`; **59 passed**, 0 failed (`Ran 59 tests ... OK`).

## Release performance gates observed

The three required release test modules all ran and passed in `06-test_release.log`:

- `tests/fixed_performance.rs`: 5/5 passed, including frozen sample 03 M3, M3 full compression regression, and frozen sample 03 M4.
- `tests/m0.rs`: 15/15 passed, including `m0_frozen_exact_repeat_compresses_end_to_end_within_bound` and `m0_repeated_128k_block_has_bounded_completion_and_coverage`.
- `tests/m5_performance.rs`: 4/4 passed, including `far_distance_2mib_finder_completes_without_oracle` and `far_distance_root_cause_is_independent_extension_not_interval_state`.

## Commands and immutable per-command logs

Exact argv, start/end times, exit codes, and paths are recorded per command in `<evidence>/post-v0.2.0/linux-verification/status.json`. Full stdout/stderr logs are `01-fmt.log` through `25-diff_check.log` in the same directory. The complete required set passed: fmt; locked check; requirement audit; Clippy; debug/release all-target tests; doc tests; warning-denying docs; release build; binary/examples build; lock/license audits; CI YAML/native workflow/fidelity/sparse benchmark tests; shell syntax/Python compileall; fixed/M5 finder comparisons; legacy acceptance/corruption/tool checks; release smoke; and diff check.

Windows/macOS native validation was not run on this Linux host; it remains subject to native GitHub Actions evidence.

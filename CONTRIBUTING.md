# Contributing

Keep changes focused, safe-code only, and consistent with the current NGv3
contract. The normative wire specification is
[`docs/FORMAT-V3.md`](docs/FORMAT-V3.md). Do not edit that frozen specification
as part of a documentation-only release change. `docs/FORMAT.md` is the
historical, retired NGv2 reference; it is provenance, not a current writer,
reader, or compatibility contract. The frozen capability-fidelity design is
also historical provenance. NGv3 inherits only the matching and resource
semantics explicitly carried into `FORMAT-V3.md`.

The current source contract is:

- NGv3 is read/write and self-contained;
- historical SuperREP v1–v4 archives are embedded read-only inputs;
- NGv1 and NGv2 are rejected as `UnsupportedVersion`;
- split indexes are unsupported; and
- the release default is method m3, with an 8 MiB default block size.

The published official v0.3.0 release auto-resolves `srep compress` memory and temp budgets when
not explicitly supplied. Non-compress commands and the library resource default
remain 256 MiB / 4 GiB; existing 0.2.1 artifacts retain those fixed defaults.
Do not describe this 0.3.0 policy as behavior of the existing 0.2.1
binaries. The detector is best-effort, not a global memory/RSS cap; see
`docs/releases/v0.3.0.md` for known limits and publication status.

Do not describe historical NGv2 behavior as current behavior. Keep the
explicit compatibility policy honest: no unsupported compatibility,
ratio, performance, or fidelity promise may be inferred from a retained
historical fixture or a source-only platform check.

## Local development

The declared MSRV is **1.88.0**. On Linux, a normal source build is:

```sh
cargo build --locked --release
```

Before submitting code changes, use the repository's applicable formatting,
lint, test, documentation, and release-smoke checks. Native Windows and macOS
compile, link, and test evidence must come from GitHub Actions on the relevant
native runners. A Linux cross-compilation check is only a compile hint and is
not Windows or macOS verification. The published 0.3.0 packages target Linux
x86_64 glibc, Windows x86_64 MSVC, and macOS ARM64. Measure runtime requirements
on each actual package; the historical 0.1.0 GLIBC_2.30 observation does not
prove the compatibility of a new build, and the 0.2.0, 0.2.1, and 0.3.0 Linux packages'
observed GLIBC_2.34 floor is likewise only an observed value for those
artifacts.

For documentation-only changes, perform read-only checks that do not rebuild or
rewrite shared artifacts. In particular, verify that examples use the current
CLI names, that `CHANGELOG.md` keeps any genuinely new work under `Unreleased`
until publication (published releases carry their real publication date and
status; historical notes must remain labeled as history, not as current release
claims), and that the completed actual-package 72-row comparisons (0.2.0,
0.2.1, and 0.3.0) are not misrepresented as universal quality or performance. The archived
local candidate evidence and the final actual-package results are separate
records; do not merge them. The historical 0.1.0 default-m3 round trip is
recorded in the release notes as a correctness check; do not turn those raw
timings into a performance claim.

The fidelity evaluator intentionally uses explicit fixed budgets so its
measurements remain comparable to the historical frozen baseline. The large-file
roundtrip harness, by contrast, omits resource flags unless requested and binds
the actual `srep: resources:` diagnostic; never report fixed defaults for an
unconfigured 0.3.0 binary.

## Archive safety

Changes affecting archive operations must preserve the operational rule that
callers back up originals and validate a round trip before deleting source
data. Resource limits must fail explicitly; they must not silently reduce
matching completeness or alter the archive format. The default m3 matcher can
use substantial memory and temporary disk on large inputs, so examples should
mention `--memory`, `--temp-dir`, and `--temp-limit` where relevant.

Add a regression test before fixing behavior. Preserve structural-error
precedence: fixed headers and exact declared lengths are checked before
allocations or memory reservations. Do not change Cargo files, the lockfile,
CI, or generated artifacts for a docs-only release update.

## Performance policy

Wall-clock budgets belong to the optimized (release) profile. They exist to
catch real throughput regressions against the shipped binary, which is
compiled with optimizations; they are not a functional contract of a debug
build. In the three timing suites (`tests/fixed_performance.rs`,
`tests/m5_performance.rs`, `tests/m0.rs`) each individual wall-clock assertion
is guarded by `if !cfg!(debug_assertions)` so that it runs and is enforced only
when `debug_assertions` is off (the stock `release` profile; the stock `dev`
profile keeps it on). The existing `Duration`/`Instant` timing expressions are
retained in both profiles and existing diagnostic output is unchanged; the
`fixed_performance` and m5 tests still print their timing lines, while the m0
tests compute a timer for the release guard without printing it. Do not add new
measurement or output code merely to report timing.

This split is deliberate and is the only sanctioned form of it:

- Debug builds MUST still run **all** correctness, round-trip, oracle,
  resource-accounting, exact-fixture, and hash/byte-identity checks. Only the
  wall-clock comparison is skipped; no test body, early return, `#[ignore]`, or
  `debug_assert` substitution is permitted.
- Release builds MUST still enforce the original numeric budgets unchanged
  (m3 < 120 s, m4 < 180 s, m5 finder < 30 s, m5 2 MiB far distance < 180 s,
  m0 repeat < 5 s, m0 exact-repeat < 30 s). Do not raise, weaken, or delete
  them, and do not change Cargo profile settings to move the boundary.
- Do not gate an entire test function on `cfg!(debug_assertions)`, return
  early, or skip a test; keep the measurement and the correctness work
  unconditional.

The boundary is the compiler's `debug_assertions` flag under the stock `dev`
and `release` profiles; this repository sets no `[profile.*]` overrides, so
`cargo test` runs the guarded path and `cargo test --release` runs the enforced
path. The release-time gate is the Native release workflow's optimized
`cargo test --locked --release`; the ordinary `ci.yml` debug jobs intentionally
exercise the functional path at debug speed.

## Release documentation

The native artifact names, archive-root file list, and external `SHA256SUMS`
verification command are part of the release interface. The package root must
contain `srep`, `LICENSE`, `THIRD_PARTY.md`, `THIRD_PARTY.audit`, `README.md`,
`CHANGELOG.md`, `NOTICES`, `BUILD-PROVENANCE.json`, and `PLATFORM-README.md`
(Windows uses `srep.exe`); dependency notices are collected by the release
integration owner. The SPDX map in `THIRD_PARTY.audit` is an attribution
checklist, not a certification that every legal requirement is satisfied.
For a future release, all six native jobs (three OSes × stable/MSRV) and three
extracted package smoke tests are required. They run the release-profile tests,
where the original wall-clock budgets remain enforced (see "Performance
policy"). The separate `ci.yml` debug-profile checks remain unchanged in command
structure and intentionally run the functional path at debug speed; a genuine
failure there must be reported, not relabeled as a release-profile pass. Do not
describe a release as published without an actual, verified artifact. The
already-published 0.3.0, 0.2.1, 0.2.0, 0.1.1, and 0.1.0 artifacts remain immutable; do
not repack, move tags, or rewrite their assets.

The 2026-10-17 closeout records official 0.3.0 publication and metadata-only
0.2.1 promotion in [`docs/evidence/v0.3.0-publication.md`](docs/evidence/v0.3.0-publication.md).
Package source/tooling stays `9d47cf9`; later documentation commits are not
package rebuilds. Preserve archived prepublication snapshots (including
`READY_NOT_PUBLISHED` provenance) and link the separate actual publication
receipt rather than retrospectively rewriting them.

Push builds use the workflow commit (`github.sha`) for both source and tooling.
Manual dispatch requires an explicit source ref, full expected source SHA, and
version; tooling still comes from the workflow commit. Use
`scripts/package-release.py --build` for the portable release contract, not the
historical Linux-only shell packager. The Python packager requires clean source
and tooling checkouts and rejects unvalidated Cargo configuration. Never use
test-only license skips for distributable packages. Source review precedes the
CI push; independent publication review follows actual artifact/runtime checks.

When reporting a bug, include the version, platform/architecture, complete
options, and a safe reproducer. Remove credentials and confidential input
before sharing diagnostics.

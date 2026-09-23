# Contributing

Keep changes focused, safe-code only, and consistent with the current NGv3
contract. The normative wire specification is
[`docs/FORMAT-V3.md`](docs/FORMAT-V3.md). Do not edit that frozen specification
as part of a documentation-only release change. `docs/FORMAT.md` is the
historical, retired NGv2 reference; it is provenance, not a current writer,
reader, or compatibility contract. The frozen capability-fidelity design is
also historical provenance. NGv3 inherits only the matching and resource
semantics explicitly carried into `FORMAT-V3.md`.

The current software contract is:

- NGv3 is read/write and self-contained;
- historical SuperREP v1–v4 archives are embedded read-only inputs;
- NGv1 and NGv2 are rejected as `UnsupportedVersion`;
- split indexes are unsupported; and
- the release default is method m3, with an 8 MiB default block size.

Do not describe historical NGv2 behavior as current behavior. Keep the
limited-preview compatibility policy honest: no unsupported compatibility,
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
not Windows or macOS verification. The 0.1.0 binary package promise is Linux
x86_64 glibc only. The preview binary is dynamically linked and observed GNU
libc symbol versions up to GLIBC_2.30; do not invent a distro package name.

For documentation-only changes, perform read-only checks that do not rebuild or
rewrite shared artifacts. In particular, verify that examples use the current
CLI names, that `CHANGELOG.md` keeps the current work under `Unreleased` until
publication (historical notes must remain labeled as history, not as current
0.1.0 claims), and that the 72-sample fidelity evidence is not claimed as a
pre-publication result. The default-m3 Linux candidate round trip is recorded
in the release notes as a correctness check; do not turn those raw timings
into a performance claim.

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

## Release documentation

The Linux artifact name, archive-root file list, and external `SHA256SUMS`
verification command are part of the release interface. The package root must
contain `srep`, `LICENSE`, `THIRD_PARTY.md`, `THIRD_PARTY.audit`, `README.md`,
`CHANGELOG.md`, and `NOTICES`; dependency notices are collected by the release
integration owner. The SPDX map in `THIRD_PARTY.audit` is an attribution
checklist, not a certification that every legal requirement is satisfied.
Windows and macOS may remain optional validation evidence, but must not be
described as published binaries without an actual artifact.

When reporting a bug, include the version, platform/architecture, complete
options, and a safe reproducer. Remove credentials and confidential input
before sharing diagnostics.

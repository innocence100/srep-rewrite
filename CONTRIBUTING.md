# Contributing

Keep changes focused and safe-code only. The approved normative specification
is `docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md`.
Update `docs/FORMAT.md` for format summaries, but do not contradict
the spec and do not overwrite its approved status.

The writer emits NG v2 archives. Legacy v1-v4 support is strictly read-only for
embedded archives; split indexes remain unsupported. Stage 8 implements m0-m5
with a deterministic RAM-or-spill CandidateIndex and budgeted temporary runs. m3/m4/m5 matching
includes the optional m0 REP overlay.
Prototype NG v1 remains rejected.

The declared MSRV is exactly **1.88.0** (raised from 1.85 because this
tree uses `if let` chains, which stabilized in 1.88). On Linux, install
and exercise that toolchain with:

```sh
rustup toolchain install 1.88.0
cargo +1.88.0 check --locked --all-targets --all-features
cargo +1.88.0 build --locked --release
cargo +1.88.0 test --locked --all-targets --all-features
```

Native Windows and macOS compile, link, and test evidence must come from
GitHub Actions (`windows-latest` / `macos-latest`). Local Linux
`--target x86_64-pc-windows-*` checks are compile-only hints, not
Windows verification. macOS cross-compilation from Linux is not
supported here; use native `macos-latest` Actions only.

Add a regression test before fixing behavior. Run formatting, clippy with
warnings denied, all tests, and a release build before submitting changes:

```sh
cargo fmt --all -- --check
sh scripts/audit-lock-licenses.sh
sh scripts/test-license-audit.sh
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
cargo test --doc
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
sh scripts/release-smoke.sh
```

The CI workflow repeats these Linux gates and also runs Windows formatting,
checks, clippy, tests, and a release build. A native matrix covers Linux,
Windows, and macOS at 1.88.0 and stable (Linux/Windows stable full suites
stay on the dedicated jobs). Configure Actions credentials to obtain
native Win/Mac evidence; this document does not claim those remote runs.

Malformed-input changes must preserve structural-error precedence: fixed
headers and exact declared lengths are validated before memory reservations.

Legacy fixture regeneration is append-only. The committed fixture directory is
read-only to the tooling. Use `--validate` for the hermetic check. The
`--generate --old-binary ABS` and `--differential --old-binary ABS` modes each
create a new random output leaf directly under the physical `/tmp/opencode`
tree; deterministic destination arguments are rejected with usage status 2.
`.work-<secret>/evidence-<secret>` directories are created directly under the
trusted root, and differential outputs are retained in that evidence
directory. The generator CLI owns
historical-binary invocation. It gives
that binary only randomized `/proc/self/fd/<held-work-fd>/<random-name>` input
and output paths, then installs verified results with exclusive fd-relative
creation under the held output descriptor; final `vN-checksum.srep` names are
never passed to the historical binary. Randomized `.work-*` and `evidence-*`
directories are retained on both success and failure. Failure output includes
retained output and, when created, evidence path and identity only when final
identity checks pass; anchor replacement fails without treating any substituted
pathname as trusted. Root, output, work, evidence, and random token identities
are checked after each historical child and immediately before success.
Generated output is validated as an exact all-entry corpus of no-follow regular
files.
Generated output is never deleted, moved, overwritten, or installed
automatically, including after a failed generation; review and any manual
fixture update happen separately.

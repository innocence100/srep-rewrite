> Sanitized historical derivative; not a new verification or current status. See ../README.md and ../manifest.json for provenance and redactions. Embedded hashes identify original evidence/assets, not this derivative. Paths and commands are historical references, not replay-ready instructions.

# Final operational verification — PASS

Verified UTC: 2026-10-17T23:31:46.610940+00:00

Main: `161719b40258c8ccd4222869ad53b97dbb7b5693`; actual Git parent `9d47cf9b910278aff0517771ccd728ca84c4b531`.
Previous remote main: `e36599b6a20a8ddc77933ef7af327ceb873a08fc` (not the docs commit parent).

CI [39072690573](https://github.com/innocence100/srep-rewrite/actions/runs/39072690573): six of six jobs PASS on exact main, attempt 1.

| Job | Result |
| --- | --- |
| verify | success |
| Native windows-latest Rust 1.88.0 | success |
| Native macos-latest Rust 1.88.0 | success |
| windows-test | success |
| Native macos-latest Rust stable | success |
| Native ubuntu-latest Rust 1.88.0 | success |

Raw full log ZIP SHA-256: `728500adb46a7734526ca30e38508b513c0954eea71819725d196380b2585f04`.
JSON receipt SHA-256: `90cb20d232728f6478aa6e5a0fe3fd57346e54af1c6de3213dac2be8b78ee450`.

Both 0.2.1 and 0.3.0 official; 0.3.0 latest. Release 0.3.0 ID 416081575 published 2026-10-17T21:43:32Z. Its annotated tag 0db31748c3391ebd4c74b614e0234dad2fa285ed still points to 9d47cf9b910278aff0517771ccd728ca84c4b531.

0.2.1 ID 414169555 retains original publication 2026-10-15T19:03:54Z; metadata-only promotion observation was 2026-10-17T21:42:18.241Z–21:42:19.123Z. Same original nine assets and tag; approved official body unchanged.

Production/tests/scripts/Cargo/workflows match the published candidate; the commit adds only the seven reviewed docs changes. Exact-source native 39034446863 and CI 39034446833 were already six-job PASS. Full72 report 7ac02bf… remains the reviewed exact-package 72/360 PASS, 65/0/7, matching 0.2.1.

All expected local HEADs/statuses preserved; original dirty tree has 35 tracked dirty entries and the specified stash. Captured dirty-file hashes were unchanged from start to end of this verification. Earlier-session byte identity cannot be asserted without an earlier per-file baseline. Full status/hashes are in state-verification.json.

No new release downloads: live asset identities match prior verified 18-download postpublication evidence. Older 0.1.0/0.1.1/0.2.0 metadata/assets/refs remain unchanged. No remote writes or reruns performed.

## Limitations

- Windows GNU check/Clippy is compile-only; optional GNU linker test/build skipped.
- Linux actual acceptance used fallback 256MiB / auto temp, not physical-total 50% runtime proof.
- Budget selection is not RAM-utilization redesign, RSS cap, disk preallocation or runtime growth.
- Incomplete Linux cgroups, RLIMIT_AS, Windows Job and macOS container limits remain detection limitations.
- Linux GLIBC_2.34 floor; macOS ad-hoc signature/no publisher identity/not notarized.
- Historical sparse improvements are development timing, not universal or fresh 0.3.0 speedups.

archived verification [internal model/reviewer/session routing redacted]; the original reports separate independent review PASS, not self-authored approval.

No further implementation loop is required.

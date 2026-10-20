# Historical evidence archive — sanitized derivatives

This docs-only backup preserves 19 small historical design, verification and
publication records, curated on **2026-10-20** against repository base
`161719b40258c8ccd4222869ad53b97dbb7b5693`. It is **not a new verification,
independent approval, release, benchmark, or normative implementation plan**.
Reported results and timestamps belong to the original records. No historical
test, download, CI run or publication command was rerun to make this archive.

## Index

| Period | Sanitized documents |
| --- | --- |
| v0.1.1 publication | [Completion receipt](v0.1.1/publication-complete.json); [public verification](v0.1.1/public-verification.json) |
| v0.2.0 publication | [Publication receipt](v0.2.0/publication-receipt.json) |
| Post-v0.2.0 development | [Linux command status](post-v0.2.0/linux-verification-status.json); [Linux summary](post-v0.2.0/linux-verification-summary.md); [native/CI receipt](post-v0.2.0/native-verification-receipt.json); [final-main receipt](post-v0.2.0/final-main-verification-receipt.json) |
| v0.2.1 original publication | [Original prerelease receipt](v0.2.1/original-prerelease-receipt.json) |
| v0.3.0 design | [Full resource-defaults revision 5](v0.3.0/resource-defaults-rev5.md); [structured design evidence](v0.3.0/resource-defaults-rev5-evidence.json) |
| v0.3.0 actual-package acceptance | [258 MiB explicit fixed-budget report](v0.3.0/large258-fixed-report.json); [fresh sample07 status](v0.3.0/sparse07-fresh-status.json) |
| v0.3.0 publication chronology | [v0.2.1 promotion observation](v0.3.0/v0.2.1-promotion-observation.json); [draft creation](v0.3.0/draft-create-observation.json); [publish command result](v0.3.0/publish-command.json); [draft check](v0.3.0/draft-check-receipt.json); [published check](v0.3.0/published-check-receipt.json) |
| v0.3.0 final main | [JSON receipt](v0.3.0/final-main-receipt.json); [narrative receipt](v0.3.0/final-main-receipt.md) |

Related repository context: [resource-defaults plan summary](../../plans/v0.3.0-resource-defaults.md),
[post-v0.2.0 investigation](../post020-sparse-investigation.md),
[v0.2.0 publication](../v0.2.0-publication.md),
[v0.2.1 original publication](../v0.2.1-publication.md),
[v0.3.0 publication and promotion](../v0.3.0-publication.md), and
[v0.3.0 release notes](../../releases/v0.3.0.md).

## Chronology and interpretation

- The full revision-5 proposal and its evidence describe design-time restrictions,
  pending implementation and open facts. These are preserved historical states,
  not current instructions or new acceptance gates. Internal work assignments
  are replaced by descriptive technical roles; technical ownership contracts,
  pseudocode, decisions and limitations remain.
- Post-v0.2.0 native packages labelled `0.2.0` are development verification
  outputs, **not** the published v0.2.0 binaries. The fixed-budget sparse timing
  comparison is same-host/sample07/m3 development evidence, not a universal
  speedup or a new v0.3.0 measurement.
- The v0.2.1 receipt records its **original prerelease** publication on
  2026-10-15. The separate 2026-10-17 promotion observation records its subsequent
  official status. Original `published_at` is not promotion time. Draft/latest
  states and the original versus promoted body hashes intentionally differ.
- The v0.3.0 final-main receipt follows the publication checks and records main
  `161719b40258c8ccd4222869ad53b97dbb7b5693`. The release binary source remains
  `9d47cf9b910278aff0517771ccd728ca84c4b531`; a later docs commit is not a rebuild.
- The 258 MiB report here explicitly supplies **256 MiB / 32 GiB** budgets.
  Fresh sample07 omits memory/temp budget overrides; the retained final receipt
  states Linux acceptance observed fallback memory / auto temp, not proof of
  half-physical-total runtime selection. The archive does not infer unrecorded
  measurements from that status file.
- Automatic compression selection is separate from fixed library/non-compress
  defaults (256 MiB / 4 GiB). Budgets are not whole-process RSS caps, disk
  reservations, runtime growth or a RAM-utilization algorithm redesign. Hidden
  cgroups, conservative fallback, RLIMIT_AS, Windows Job and macOS container
  limitations remain. Historical Windows/macOS results refer to native Actions,
  not local Linux cross-compilation; optional GNU linking skips remain skips.

## Provenance, privacy and hash semantics

[manifest.json](manifest.json) lists the exact 19 derivative paths, generic
source experiment/basename labels, original and sanitized byte sizes, separate
SHA-256 values, and per-file redaction categories/counts. Manifest paths are
relative to this directory. The README and manifest are the only additional
files; the manifest does not attempt to hash itself.

**An original SHA-256 is provenance for the original bytes, not a checksum of
the sanitized copy.** The original and derivative bytes differ. Embedded hashes
and cross-references were not rewritten to impersonate checksums of new copies.
For example, the narrative final-main receipt's JSON checksum still identifies
the original JSON receipt, and the post-v0.2.0 final receipt still identifies its
original prior receipt. Use `sanitized_sha256` to verify these public files.
Asset/package/binary hashes, source/tag/commit identities, public GitHub run/job/
release/asset IDs, numeric technical results and actual timestamps are retained.
Original-source hashes were computed only for the 19 explicitly selected files.

Sanitization replaces personal host path prefixes with stable logical
`<workspace>/...` or `<evidence>/...` labels; removes personal attribution;
replaces internal task, reviewer, session, producer/model-routing and process
identifiers; and redacts recorded environment assignment values while retaining
their technical meaning (single-threaded tests and warning-denying docs).
The phrase **archived verification** is an archive label replacing internal
attribution, **not a claim about the original producer**. Existing review PASS
statements remain historical reported outcomes, not approval of this backup.
Each whole metadata-field replacement counts once even if it contains multiple
internal identifiers; path and role substitutions count occurrences. No-redaction
records still receive an archive notice and canonical JSON formatting.

JSON uses UTF-8, sorted keys, two-space indentation and a final LF; this is a
documented deterministic serialization, not an RFC 8785 claim. An added
`_archive_notice` distinguishes each JSON derivative. Markdown receives the same
notice above the original body. These are documentary derivatives, not drop-in
machine inputs for historical validators.

**Commands and paths are historical references, not replay-ready instructions.**
The retained `/acceptance/...` paths describe the original technical container
layout, not files shipped here. Logical labels in commands are not shell-safe
substitutions. Plain filenames, original evidence paths, and log references do
not imply those artifacts are included or still available. Original schema and
field names are retained except internal worker-role keys renamed descriptively.

No binaries, large logs, polling snapshots, API user objects, release attachments,
credentials, private configuration, or superseded offload plan are included.
Published assets are referenced, not duplicated or modified. This backup cannot
independently establish historical execution from the selected receipts alone;
full logs/assets and live GitHub state were not revalidated. Original files remain
unchanged. Independent privacy/provenance audit and publication are separate steps.

## Lightweight integrity check

From the repository root, the following read-only check verifies the public
derivative checksums and sizes (it cannot verify unavailable original sources):

```sh
python3 - <<'PY'
import hashlib
import json
from pathlib import Path

root = Path('docs/evidence/archive')
manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
assert manifest['document_count'] == len(manifest['files']) == 19
expected = {'README.md', 'manifest.json'}
for entry in manifest['files']:
    path = Path(entry['path'])
    assert not path.is_absolute() and '..' not in path.parts
    assert entry['path'] not in expected
    expected.add(entry['path'])
    data = (root / path).read_bytes()
    assert len(data) == entry['sanitized_bytes']
    assert hashlib.sha256(data).hexdigest() == entry['sanitized_sha256']
    if path.suffix == '.json':
        json.loads(data)
assert all(not p.is_symlink() for p in root.rglob('*'))
files = [p for p in root.rglob('*') if p.is_file()]
assert {p.relative_to(root).as_posix() for p in files} == expected
assert all(p.stat().st_size < 65536 for p in files)
assert sum(p.stat().st_size for p in files) < 200 * 1024
print('19 derivative checksums and sizes verified; exact 21-file archive')
PY
```

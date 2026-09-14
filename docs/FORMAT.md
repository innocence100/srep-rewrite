# SREP-NG v2 stream format (historical, retired)

> **HISTORICAL — RETIRED, NOT CURRENT NORMATIVE.**
> This document describes the **retired** SREP-NG v2 wire format. NG v2 is no
> longer supported for reading or writing: the `SREPNG2\0` magic is recognized
> and rejected as `UnsupportedVersion`, exactly like prototype NG v1, and the
> `compress_v2*` writer APIs are removed. There is no active NG v2 reader,
> writer, or compatibility surface. The current normative format is
> **[`FORMAT-V3.md`](FORMAT-V3.md)**; this file is retained only as the frozen
> historical reference for the v2 wire bytes its provenance evidence was built
> on. Every "writes", "reads", or "supported" statement below is historical and
> does **not** describe current behavior.

The historical normative source for SREP-NG v2 was
`docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md`.
This file summarized the NG v2 records that the CLI and candidate-driven
library API wrote, and the records that were strictly decoded at the time. It
is preserved for history and must not be read as a current contract.

The historical CLI wrote self-contained NG v2 archives. m0 used representative
matching, m1/m2 used their CDC finders, m3/m4 used fixed-grid finders, and m5
used exhaustive
fixed-polynomial matching. The library's historical
`compress_with_candidates` API additionally wrote canonical reference records
from validated candidates. Prototype NG v1 (`SREPNG\0\x01`,
including `.srep2`) was rejected as `UnsupportedVersion`. In the current
production boundary, the retired NG v2 magic `SREPNG2\0` is likewise rejected
as `UnsupportedVersion`; it is never decoded. Historical SuperREP
signatures `0x26351817 0x50455253` remain recognized by the dispatcher, and
their v1–v4 decoding is strict and read-only for embedded archives; split
indexes remain unsupported and the writer never emits a legacy format. The
historical SuperREP **version 2** is a distinct legacy container with that
different signature and stays readable; it is **not** NG v2.

All integers are unsigned little-endian. Wire positions, lengths, offsets,
sizes, and counts are `u64`. Checksums were selected once in the ArchiveHeader
and used for ordinary records, DataBlock representation-plus-semantics, and the
archive semantic digest.

---

**The remainder of this document is the retained historical NG v2 wire
reference. It is history, not current normative text.** No statement below
promises that NG v2 is written, read, or supported today; the present tense is
the original 2026 documentation and is preserved unchanged for provenance.

## Archive layout

```text
ArchiveHeader                  80 bytes, magic SREPNG2\0, version 2
MethodParameters               record 0x01, payload 64 bytes
LayoutMetadata                 record 0x05, payload 64 bytes
BlockDirectory                 record 0x02, Index-LZ only
DataBlock block 0..n-1         record 0x03, one per nonempty block
IndexSection                   record 0x04, Index-LZ only
ArchiveSummary                 record 0x06, payload 88 (XXH3) or 104 (BLAKE3)
Trailer                        64 bytes, magic SREPNGT2
end of input
```

Empty archives have `block_count=0` and no DataBlock records. Index-LZ still
has an empty BlockDirectory and empty IndexSection.

## ArchiveHeader (80 bytes)

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic `SREPNG2\0` |
| 8 | 1 | Version `2` |
| 9 | 1 | Flags, exactly `0` |
| 10 | 1 | Checksum ID: `1` XXH3-128 or `2` BLAKE3-256 |
| 11 | 1 | Layout: `1` Index, `2` Future, `3` I/O |
| 12 | 1 | Method `0..=5` |
| 13 | 1 | Semantic flags: bit 0 REP overlay; bits 1–7 zero |
| 14 | 2 | Reserved, zero |
| 16 | 8 | Block size |
| 24 | 8 | Minimum match |
| 32 | 8 | Seed size (method-specific) |
| 40 | 8 | Target chunk (method-specific) |
| 48 | 8 | Maximum distance; `0` = complete history |
| 56 | 8 | Checksum seed, exactly `0` |
| 64 | 8 | Header record count, exactly `2` |
| 72 | 8 | Header byte length, exactly `80` |

Defaults for new archives are method m3, layout Index, checksum XXH3, 8 MiB
blocks, and minimum match 512.

## Records

Every record is `type u8`, flags `0`, reserved `0`, `payload_len u64`, payload,
then the selected checksum. Ordinary record checksums cover ASCII
`SREPNG2-RECORD\0` + 12-byte frame + payload.

CLI DataBlock semantics are method-dependent:

- For m0, Index/Future/I/O DataBlocks may contain canonical matches and
  literals produced by the representative finder. IndexSection and
  BlockDirectory contain the corresponding canonical match ranges/counts.
- For m5, Index/Future/I/O use the same canonical reference plans as the other
  real finders; the M5 finder supplies exhaustive candidates before normalization.
- The candidate API likewise emits canonical reference layouts from supplied
  candidates.

The strict decoder also accepts candidate-generated FutureRegister entries,
I/O tag-1 match fragments, and nonzero IndexSection matches. Canonical origins,
coverage, source availability, checksums, summary counters, and semantic digest
are validated; malformed fields are never ignored.

## CandidateIndex temporary runs

Finder history uses RAM memtables and private spill runs under the configured
temporary directory. Each run is published only after flush, `sync_all`, and
close, and its full physical size is reserved from the shared temporary budget.
The run has no trailer and its exact length is `64 + 104 * count` bytes.

The 64-byte header is `SREPIDX1`, followed by schema `u16=1`, record size
`u16=104`, flags `u32=0`, count `u64`, generation `u64`, nonce `[16]`, and an
XXH3-128 checksum over `SREP-IDX-HDR\0` plus header bytes `0..48`. Each 104-byte
record contains schema `u16=1`, kind `u8`, key length `u8`, metadata length
`u8`, flags `u8=0`, reserved `u16=0`, position `u64`, insertion ordinal `u64`,
32-byte zero-padded key, 32-byte zero-padded metadata, and an XXH3-128 checksum
over `SREP-IDX-REC\0` plus record bytes `0..88`. Readers validate every field,
padding byte, checksum, physical length, and persisted sort order before use.

Visible `Run` state is exclusively this `SREPIDX1` persisted-order format; no
out-of-band order selector is associated with a visible run. Query and
compaction intermediates use the separate self-describing `SREPQRY1` scratch
format and are never placed in the visible run set.

### SREPQRY1 scratch

Scratch files are exactly 64 bytes plus `104 * count` record bytes and have no
trailer. Their header fields are:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic `SREPQRY1` |
| 8 | 2 | Version `1` |
| 10 | 2 | Record size `104` |
| 12 | 1 | Order ID: `1` identity, `2` persisted-query |
| 13 | 1 | Flags `0` |
| 14 | 2 | Reserved `0` |
| 16 | 8 | Record count |
| 24 | 8 | Transaction generation |
| 32 | 16 | Random nonce |
| 48 | 16 | XXH3-128 checksum |

The scratch checksum covers `SREP-QRY-HDR\0` followed by header bytes `0..48`.
Order ID is authoritative: order 1 is `(key_kind, complete key field, position,
complete metadata field, insertion ordinal)` and order 2 is the persisted query
order `(key_kind, complete key field, position desc, insertion ordinal asc,
complete metadata field)`. Scratch readers reject wrong magic, order ID,
checksum, exact length, shape, padding, or sorted order. Scratch files are
randomly named, restrictive-permission, shared-budget, RAII-owned files and do
not survive a successful query or compaction.

DataBlock trailing checksums cover ASCII `SREPNG2-BLOCK\0` + frame + encoded
payload + `block_id`/`dst_start`/`uncompressed_len` + reconstructed bytes.
Mutating DataBlock metadata therefore fails even when reconstructed bytes could
match.

ArchiveSummary digest covers ASCII `SREPNG2-ARCHIVE\0` + 80 header bytes + 64
MethodParameters payload bytes + 64 LayoutMetadata payload bytes + reconstructed
uncompressed bytes. XXH3-128 is serialized as low64 LE then high64 LE.

## CLI semantics

CLI-generated archives persist method/layout/checksum/min-match in the header.
The m0 path searches for representative matches, m1/m2 search for whole CDC
chunk matches, m3/m4 search fixed-grid seeds, and m5 searches every target seed
against every aligned source seed. The default m3 path emits real references.
When REP overlay is enabled, the persisted effective minimum for combined
candidate validation is `min(base minimum, rep minimum)`; base m3/m4 discovery
and overlay m0 discovery retain their independent thresholds.

Stage 7's retained M5 evidence uses old `srep -v0 -b8k -l16 -d0 -m5o
-hash=md5` with no `-c` option. In the historical CLI, `-c` overrides the
primary chunk/seed size; omitting it is required for M5's derived `L=8`.
The legacy header `BASE_LEN` byte is a separate decoder minimum field and is
not the M5 seed size. A forced-L M4 archive is retained with a different hash
and zero matches to prove that the corrected command selects M5. The retained
M5 `old_result` uses decoded semantic coverage/literals (`32`/`46` for the
78-byte corpus); the historical `-i` encoded-byte metric is tracked separately
as `16`. The retained M5 comparison is directed evidence and does not claim
full historical fidelity.

## Legacy validation evidence

`scripts/validate-legacy-fixtures.py` independently parses the committed
legacy matrix and recomputes its hashes, descriptors, physical frame and
statistics metrics, fragment ranges, and v4 footer ranges without invoking
the Rust reader or the historical binary. `source_gap0_count` counts a zero
gap between records in one source group; an initial group gap is excluded.
`trailing_literal_bytes` counts bytes after the final match in a physical
block, not all literal payload bytes. The committed special corpus includes
the historical 112-byte and 1,675-byte VHASH archives copied from the
read-only migration evidence paths; the latter original was recovered with
the old decoder and then copied into this tree.

For v4, a missing body/index byte is classified as `TruncatedArchive` only
when footer-derived ranges and a bounded structural frame probe consistently
show a short physical region. A fully present altered footer, size entry, or
body field remains `CorruptIndex`, preventing malformed present bytes from
being accepted as truncation. Named runtime mutations are listed in
`tests/fixtures/legacy/corruptions.json` and executed by
`scripts/run-legacy-corruptions.py`.

Fixture regeneration is append-only. `scripts/regenerate-legacy-fixtures.sh`
`--validate` is read-only; its `--generate` and `--differential` modes reject
deterministic destination arguments and create a random output leaf directly under physical
`/tmp/opencode`, write files exclusively, and retain both successful and
partial output. The generator CLI owns the historical-binary invocation. It
holds the trusted root, output, work, and evidence directories as descriptors,
captures device/inode identities immediately after each creation, reopens each
held pathname relative to its held parent before use, and performs installation
with exclusive fd-relative creation. It creates randomized
`.work-<secret>/evidence-<secret>` directories directly under the trusted root
that are retained on success and failure. Historical
binary arguments are limited to `/proc/self/fd/<held-work-fd>/<random-name>`
private files; final `vN-checksum.srep` names are never passed to that binary.
After each historical child and immediately before success, root/output/work/
evidence identities and the random output token are validated. The generated
corpus is an exact all-entry set and every entry must be a no-follow regular
file. On failure, textual retained paths are reported only after final checks
pass; otherwise held identities are reported and pathname text is explicitly
untrusted. If the trusted root or output/work/evidence directory is substituted,
the operation fails nonzero and emits no success path. No mode replaces, moves,
removes, or installs a committed fixture root. The generated corpus is
validated with this parser and the Rust decoder before success is reported;
differential decoder output is retained in the generated `evidence-<secret>`
directory.

## Candidate API semantics

`compress_with_candidates` validates each candidate's implied bytes using
overlapping LZ semantics, omits candidates with `len <= 25`, and applies
deterministic weighted interval scheduling. The selected canonical triples drive
all three layouts. `inspect_matches` returns validated NG v2 canonical triples.
The m0 through m5 finders use the deterministic RAM-or-spill CandidateIndex.
Paged query runs and fan-in-16 compaction are budgeted through the shared
temporary storage context.

## Decoder errors

Unknown magic is `CorruptHeader`. Prototype NG v1 is `UnsupportedVersion`.
Recognized legacy signatures are decoded by the strict embedded legacy reader;
`UnsupportedVersion`. Truncation after a recognized header is
`TruncatedArchive`. Unknown types/flags/reserved, bad order/cardinality,
malformed lengths, and trailing data map to the structured categories in
Section 12 of the design spec. CLI output is
`srep: <Message ID>: <summary>[: context]` with the specified exit codes.

Codec working memory is bounded per live block/payload and metadata buffer;
fixed headers are stack-parsed and variable payload reservations occur only
after exact structural length validation;
the total uncompressed stream is not charged to RAM. All public-writer
decompression stages output in one private seekable spool and only copies it
after summary, trailer, and trailing-byte validation succeeds; this applies to
literal and reference records in every supported layout.
Input spools, validation spools, and CLI atomic staging share one temporary
budget. Reservations are made before growth, released on cleanup, and use
OS-randomized exclusive temporary files with restrictive permissions.
Reference DataBlock parsing performs fixed-header/count and seekable physical
record validation before reserving nested collections. Parsed Index, Future,
and I/O layouts reserve only the collection types used by that layout; unused
register or operation collections remain empty and zero-capacity. `inspect_matches`
returns its reservation-owning `InspectedMatches` value directly.

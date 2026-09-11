# SREP Capability-Fidelity Design

**Status:** Approved normative wire/algorithm specification; independent design review including v4 observable-error, overlay effective-minimum, and index-scratch clarifications `PASS`; implementation pending
**Date:** 2026-08-30
**Scope:** Continuation of the Rust 2024 `srep-rewrite` prototype. This document is self-contained and defines the target architecture, algorithms, wire format, compatibility boundary, tests, and acceptance evidence.

The prior broad design, v4 observable-error clarification, overlay-minimum clarification, and index-scratch clarification received independent design review `PASS`. The document records the approved architecture; implementation and final gates remain outstanding. Because the overlay-minimum and index-scratch clarifications change normative hard units, the implementation stage MUST regenerate the versioned requirement manifest and its `spec_sha256`/IDs and revalidate affected evidence mappings; the hard-unit balance and version-manifest implications are intentional and acknowledged.

## 1. Normative language and governing objective

The terms **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT**, and **MAY** are normative. A hard requirement is required for acceptance. A statement identified as **future performance work** is not required before the fidelity gates and MUST NOT change candidate completeness, confirmation rules, reconstructed bytes, persisted semantics, or deterministic ordering.

The implementation MUST continue the prototype as a maintainable Rust 2024 SREP implementation that is easy to build and safe to engineer. Compression capability fidelity is the primary objective, ahead of runtime optimization. The implementation MUST:

<!-- hard-unit kind="list" -->
- read legacy embedded-index `.srep` archives in versions 1, 2, 3, and 4;
- write only the new SREP-NG version 2 format defined here;
- reject the current experimental SREP-NG v1 format rather than preserve or convert it;
- restore the original `m0` through `m5` semantics;
- support Index-LZ, Future-LZ, and I/O-LZ;
- use complete logical history by default for every method, including `m0`;
- support an explicit maximum distance for bounded semantics;
- support bounded RAM through temporary disk indexes and spools; and
- fail explicitly on resource exhaustion, malformed input, unsupported input, or publication failure rather than silently weakening behavior.
<!-- /hard-unit -->

This specification describes target behavior and does not assert present implementation status. Existing behavior is evidence only where explicitly labeled as observed black-box baseline in Appendix A.

## 2. Compatibility boundary

### 2.1 Archive dispatch

The archive dispatcher MUST identify the input before decoding it:

<!-- hard-unit kind="list" -->
1. A recognized legacy signature/header selects the strict legacy reader.
2. The distinct SREP-NG v2 magic selects the NG v2 reader.
3. The current experimental SREP-NG v1 magic, including the existing `.srep2` format, is rejected as `UnsupportedVersion`. It MUST NOT be read, converted, or treated as a legacy archive.
4. Any other signature is rejected as `CorruptHeader` (code 6).
<!-- /hard-unit -->

### 2.2 Embedded and split indexes

A successful new archive MUST be one self-contained file. Permanent external/split indexes, including legacy and v2 `-index=FILE` archives, are not supported. An internal temporary index, input spool, pending-data spill, or output staging file is allowed and is not an archive sidecar.

The legacy format has no reliable in-band marker distinguishing an archive that expects a sidecar from a corrupt embedded-index archive. An archive-only reader MUST NOT guess, probe, or open a sidecar. It MUST report `CorruptIndex` when the embedded bytes, ranges, or footer declarations do not validate. Duplicate `--index`/`-index` detection takes precedence and is `InvalidConfiguration`. `UnsupportedLegacySplitIndex` is returned only when the caller supplies exactly one otherwise syntactically valid legacy sidecar option; the CLI MUST reject that option without opening the supplied sidecar. This behavior is required by the observed split-index evidence in Appendix A.

### 2.3 CLI compatibility

The CLI MUST preserve `-m0`, `-m1`, `-m2`, `-m3`, `-m4`, and `-m5` as stable method aliases and MUST provide semantic long option names. Old pure-performance switches MAY be redesigned or removed. Compatibility with every old CLI switch or old performance switch is not a requirement.

The CLI MUST accept exactly one case-sensitive command as the first operand:

<!-- hard-unit kind="list" -->
- `compress`;
- `decompress`;
- `info`;
- `test`.
<!-- /hard-unit -->

The CLI MUST provide exactly these semantic selectors, resource options, and I/O options, with the defaults and validity rules below:

<!-- hard-unit kind="list" -->
- `--method=m0|m1|m2|m3|m4|m5|rep|rolling-cdc|order1-cdc|fixed-digest|reread|exhaustive`;
- `-m0`, `-m1`, `-m2`, `-m3`, `-m4`, and `-m5` as aliases for `rep`, `rolling-cdc`, `order1-cdc`, `fixed-digest`, `reread`, and `exhaustive` respectively;
- `--layout=index|future|io`, default `index`;
- `--checksum=xxh3|blake3`, default `xxh3`;
- `--block-size SIZE`, default `8MiB`;
- `--min-match SIZE`, default `512` for m0/m3/m4/m5 and `32` for m1/m2;
- `--seed-size SIZE`, valid only for m3 and m4, default equal to `--min-match`;
- `--target-chunk SIZE`, valid only for m1 and m2, default `4096`;
- `--max-distance SIZE`, omitted by default meaning complete history;
- `--rep-overlay`, valid only for m3, m4, and m5, default disabled;
- `--rep-distance SIZE`, requiring `--rep-overlay`, default `512MiB` when overlay is enabled;
- `--rep-min-match SIZE`, requiring `--rep-overlay`, default `512` when overlay is enabled;
- `--memory SIZE`;
- `--temp-dir PATH`;
- `--temp-limit SIZE`;
- `--output-limit SIZE`;
- `--force`, default disabled;
- `--quiet` and `-q`, default disabled;
- `--index=PATH` and `-index=PATH`, recognized sidecar selectors; exactly one otherwise syntactically valid occurrence is rejected as `UnsupportedLegacySplitIndex` without opening `PATH`;
- positional `INPUT` and optional positional `OUTPUT`.
<!-- /hard-unit -->

`SIZE` is an unsigned decimal integer with an optional unit and no fraction, sign, whitespace, or exponent. Units are case-insensitive binary powers of 1024: omitted or `B` means 1; `K`, `KB`, and `KiB` mean 1024; `M`, `MB`, and `MiB` mean 2^20; `G`, `GB`, and `GiB` mean 2^30. A SIZE that is empty, non-decimal, fractional, uses any other unit, or overflows `u64` is `InvalidConfiguration`. `PATH` is a native OS path. The token `-` as `INPUT` means stdin; the token `-` as `OUTPUT` means stdout.

Default `OUTPUT` for `compress` is `INPUT` with the suffix `.srep` appended. Stdin compression requires an explicit `OUTPUT`. Default `OUTPUT` for `decompress` is `INPUT` with a terminal `.srep` suffix removed; stdin decompression or an `INPUT` without that terminal suffix requires an explicit `OUTPUT`. `info` and `test` take `INPUT` only. `--force` is valid for `compress` and `decompress` file `OUTPUT` and means atomically replace an existing non-directory destination; without `--force`, an existing destination is `InvalidConfiguration`. `--force` is invalid for stdout. `--quiet`/`-q` suppress non-error human-readable status on stderr; they MUST NOT suppress structured error output.

The default method is m3 / `fixed-digest`. Method selectors are mutually exclusive. Repeating any method selector, including repeating the same selector or mixing `--method=` with `-mN`, is `InvalidConfiguration`; there is no last-wins behavior. Duplicate option detection takes precedence over sidecar recognition. Each of `--layout`, `--checksum`, `--block-size`, `--min-match`, `--seed-size`, `--target-chunk`, `--max-distance`, `--rep-overlay`, `--rep-distance`, `--rep-min-match`, `--memory`, `--temp-dir`, `--temp-limit`, `--output-limit`, `--force`, `--quiet`/`-q`, and `--index`/`-index` may appear at most once. A repeated `--index`, repeated `-index`, mixed `--index`/`-index` pair, or any other repeated selector is `InvalidConfiguration` and MUST be returned before `UnsupportedLegacySplitIndex`. Exactly one otherwise syntactically valid `--index=PATH` or `-index=PATH` is a recognized sidecar selector and MUST return `UnsupportedLegacySplitIndex` without opening `PATH`, rather than being classified as an unknown option. Layout, command, method, and option names are case-sensitive; an unknown or case-variant name among those is `InvalidConfiguration`. An unknown or case-variant `--checksum` name is `UnknownChecksum`, not `InvalidConfiguration`. `--method`, `-m0` through `-m5`, `--layout`, `--checksum`, `--block-size`, `--min-match`, `--seed-size`, `--target-chunk`, `--max-distance`, `--rep-overlay`, `--rep-distance`, and `--rep-min-match` are compression-only; supplying any of them to `decompress`, `info`, or `test` is `InvalidConfiguration`. `--force` is valid only for `compress` and `decompress` file output. `--memory`, `--temp-dir`, `--temp-limit`, `--output-limit`, and `--quiet`/`-q` are valid for every command. `--seed-size` is invalid for m0, m1, m2, and m5; m0 derives its seed from minimum match, m1 uses 48, m2 uses zero, and m5 derives its seed by its checked formula. `--target-chunk` is invalid for methods other than m1 and m2. The rep options have the validity rules in Section 4.3. A method-specific option supplied for a method that rejects it is `InvalidConfiguration` and MUST be rejected before opening input or creating output. Operational resource defaults are those in Section 10.0.

## 3. Target architecture

The public library configuration is:

<!-- hard-unit kind="formula" -->
```rust
enum Command { Compress, Decompress, Info, Test }

struct CliOptions {
    command: Command,
    input: PathBuf,           // "-" means stdin
    output: Option<PathBuf>,  // "-" means stdout
    force: bool,              // default false
    quiet: bool,              // default false
    compression: CompressionConfig,
}

struct CompressionConfig {
    method: Method,
    layout: Layout,
    checksum: Checksum,
    block_size: u64,
    min_match: u64,
    seed_size: Option<u64>,
    target_chunk: Option<u64>,
    max_distance: Option<u64>,
    rep_overlay: Option<RepConfig>,
    resources: ResourceConfig,
}

enum Method { M0Rep, M1RollingCdc, M2Order1Cdc, M3FixedDigest, M4Reread, M5Exhaustive }

struct RepConfig {
    distance: u64,
    min_match: u64,
}

enum Layout { Index, Future, Io }

enum Checksum { Xxh3, Blake3 }

struct ResourceConfig {
    memory: u64,
    temp_dir: PathBuf,
    temp_limit: u64,
    output_limit: u64,
}
```
<!-- /hard-unit -->

<!-- hard-unit kind="paragraph" -->
The defaults are `M3FixedDigest`, `Index`, `Xxh3`, block size 8 MiB, minimum match 512 bytes, `seed_size=Some(min_match)`, `target_chunk=None`, complete history (`max_distance=None`, encoded as wire zero), no REP overlay, `force=false`, `quiet=false`, `temp_dir` equal to the process platform temporary directory, and the operational defaults in Section 10.0. `M1RollingCdc` and `M2Order1Cdc` override the default minimum to 32 bytes, set `target_chunk=Some(4096)`, and keep `seed_size=None`. `M0Rep`, m3, m4, and m5 default to 512-byte minimum and `target_chunk=None`. For m0, `seed_size` is not a caller option and remains `None` while the header seed field equals `minimum_match`. For m1, `seed_size` is not a caller option and remains `None` while the header seed field is 48. For m2, `seed_size` is not a caller option and remains `None` while the header seed field is 0. For m5, the method derives its seed and stores that derived nonzero `L` in the header seed field while `CompressionConfig.seed_size` remains `None` unless a caller illegally supplies one, which the library MUST reject. Supplying `seed_size` for m0, m1, m2, or m5, or supplying `target_chunk` for any method other than m1 and m2, is `InvalidConfiguration`. `CliOptions.force` and `CliOptions.quiet` are CLI publication/logging controls; they MUST NOT be persisted as archive semantics. The library MUST reject invalid option combinations before opening input or creating output.
<!-- /hard-unit -->

All temporary resources share one `temp_limit`: input spools, CandidateIndex visible runs and private query/compaction scratch, pending Future-LZ spill, output spools, and atomic staging output are charged against one current allocated-physical-byte reservation total. A reservation is required before growth; releasing bytes lowers the current reservation total but does not lower the recorded high-water statistic. If direct seekable output staging alone exceeds the limit, compression fails. There is no separate spool budget. `CompressionConfig` applies to compression; decoder APIs accept the same `ResourceConfig`, and `info`/`test` use decoder resource limits. Only semantic fields are persisted in v2.

The large prototype codec implementation MUST be decomposed into cohesive modules with these boundaries:

<!-- hard-unit kind="formula" -->
```text
CLI/library
    -> archive dispatcher
        -> legacy reader OR NG v2 reader/writer
            -> DataSource and storage services
            -> method-specific MatchFinder
            -> shared Match IR
            -> normalization and encoded-cost selection
            -> Index-LZ OR Future-LZ OR I/O-LZ layout encoder/decoder
```
<!-- /hard-unit -->

### 3.1 Responsibilities

<!-- hard-unit kind="list" -->
- **CLI/library:** Parse options; expose structured results and errors; select input, output, method, and layout; report run statistics; and publish successful file output atomically. The library MUST never print, exit the process, or choose a weaker method or layout implicitly.
- **Archive dispatcher:** Detect format; select legacy reader or NG v2; reject prototype v1 and unsupported sidecars; and enforce format-specific limits.
- **DataSource:** Provide checked random access through `read_at` for seekable input. Stdin MUST be spooled when whole-history, reread, or another random-access behavior requires it. The v2 design uses DataSource random access or an owned spool for m0.
- **CandidateIndex:** Store and query candidate positions through the interface in Section 7.
- **MatchFinder:** Implement exactly one method's semantic candidate enumeration. It MUST depend on `DataSource`, `CandidateIndex`, and semantic parameters, not on archive layout.
- **Match IR:** Be the sole semantic representation passed from matching to normalization and layout encoding.
- **Normalizer:** Validate candidates, select a canonical non-overlapping set using the layout-independent cost in Section 6, and assign stable origin match IDs.
- **Layout encoder/decoder:** Serialize and reconstruct the normalized IR using one of the three layouts. Layout code MUST NOT search for matches or change the selected IR.
- **Storage services:** Provide the hot cache, RAM index, deterministic paged sorted-run index, safe temporary-resource manager, pending Future-LZ spill, and seekable history/output sinks.
<!-- /hard-unit -->

The same method and semantic parameters MUST produce byte-identical Match IR sequences regardless of selected layout and regardless of RAM versus paged index storage.

## 4. Configuration and semantic defaults

### 4.1 Common defaults

The default archive block size is **8 MiB**. Blocks partition the uncompressed input into half-open intervals:

<!-- hard-unit kind="formula" -->
```text
[B_j, B_{j+1}), where B_j = j * block_size
```
<!-- /hard-unit -->

The final block may be shorter. Empty input has zero blocks. A nonempty input has `ceil(input_len / block_size)` blocks. Block boundaries are archive structure and do not change the logical dictionary or the candidate history unless a method explicitly defines a reset, as `m1` and `m2` do.

The default minimum match length is:

<!-- hard-unit kind="list" -->
- **512 bytes** for `m0`, `m3`, `m4`, and `m5`;
- **32 bytes** for `m1` and `m2`.
<!-- /hard-unit -->

The default target/average chunk size for `m1` and `m2` is **4096 bytes**. The default REP overlay parameters are `rep_distance=512 MiB`, `rep_min_match=512 bytes`, and `rep_region_size=max(1,floor(rep_min_match/8))`. Every semantic parameter that affects boundaries, candidate keys, alignment, extension, distance, rounding, or overlay behavior MUST be persisted in the v2 header and records.

The default logical dictionary is complete input history for every method. `max_distance=0` means complete history. A nonzero maximum distance is an explicit semantic restriction and MAY change the Match IR. RAM and temporary-disk budgets are operational and MUST NOT be persisted as matching semantics.

### 4.2 Distance modes

`--max-distance SIZE` explicitly limits `m0` and all other methods. The distance test is inclusive: a candidate at `before - SIZE` remains eligible. `-m0 --max-distance=512MiB` reproduces the old default finite-window scope.

There is no implicit finite-window default for `m0`. By default, `m0` uses the paged CandidateIndex and DataSource over complete history, retaining its REP matching algorithm but not restricting its history. A finite-distance run MAY still spool input or use `read_at`; finite semantics remain the same regardless of the access strategy. SREP-NG v2 does not define a single-pass m0 mode. A future single-pass optimization, if proposed, requires a separately reviewed semantic design because arbitrary independent backward extension cannot in general be made exactly equivalent to seekable `--max-distance` semantics.

### 4.3 REP overlay configuration

`m3`, `m4`, and `m5` MAY enable the optional m0 REP overlay with `--rep-overlay`. The overlay has independent semantic parameters:

<!-- hard-unit kind="list" -->
- `rep_distance`, default 512 MiB, set by `--rep-distance`;
- `rep_min_match`, default 512 bytes, set by `--rep-min-match`; and
- `rep_region_size = max(1, floor(rep_min_match / 8))`.
<!-- /hard-unit -->

`--rep-overlay` is a boolean semantic selector valid only for m3, m4, and m5. `--rep-distance` and `--rep-min-match` require `--rep-overlay`. m0 is not an overlay and MUST reject all overlay options. m1 and m2 MUST reject `--rep-overlay` and both overlay parameter options. For an overlay, `--max-distance` remains the global cap for all candidates; the effective overlay distance is `rep_distance` when `max_distance=0`, otherwise `min(rep_distance, max_distance)`. The independent `rep_distance` remains persisted so the selected overlay configuration is reproducible. Overlay fields are zero when the overlay is disabled. The ArchiveHeader semantic flag bit 0 is exactly the `--rep-overlay` selector and MUST match MethodParameters flags.

The persisted effective archive match minimum is:

<!-- hard-unit kind="formula" -->
```text
effective_min_match =
  if REP overlay enabled: min(ArchiveHeader.minimum_match, MethodParameters.rep_min_match)
  else: ArchiveHeader.minimum_match
```
<!-- /hard-unit -->

The fields used by this formula MUST already have passed their checked wire-range and method/overlay validation, and both participating minimums MUST be positive. `ArchiveHeader.minimum_match` is the base minimum. The effective minimum is derived from these already persisted and checksummed fields; no per-match provenance bit or separate effective-minimum wire field is added, and this clarification makes no wire-format change.

## 5. Shared hash primitives and exact confirmation

### 5.1 Polynomial rolling hash

All internal candidate hashing that uses the polynomial primitive MUST use this exact fixed function over bytes:

<!-- hard-unit kind="formula" -->
```text
H(b[0..n]) = fold starting at H=0:
             H = H * 153191 + u64(b[i]) modulo 2^64
```
<!-- /hard-unit -->

For a length-`n` rolling window, removing the outgoing byte uses:

<!-- hard-unit kind="formula" -->
```text
outgoing * 153191^n modulo 2^64
```
<!-- /hard-unit -->

The power MUST be computed with checked loop/index arithmetic and wrapping multiplication that is explicitly defined modulo `2^64`; it MUST NOT depend on host integer overflow behavior. Every hash tie is resolved by the lower absolute position, then the lower insertion ordinal if an index entry tie remains.

The polynomial hash is only a candidate key or filter. It MUST NOT be treated as proof of equality.

### 5.2 Internal strong digest

For `m1`, `m2`, and `m3`, the internal strong candidate digest is BLAKE3 truncated to the first 16 bytes of the BLAKE3-256 digest. Digest equality only filters candidates. Before every match emission, and during every digest-by-digest extension, the implementation MUST perform an exact byte comparison through DataSource. A digest collision MUST therefore produce no false match.

`m4` and `m5` use polynomial seed keys plus exact DataSource byte comparison; their candidate filters are not match proof. `m4` performs reread confirmation and extension using no BLAKE3 digest. `m0` uses polynomial keys plus exact byte comparison. No method may emit a match without authoritative byte equality for every emitted byte.

## 6. Match IR and layout-independent normalization

### 6.1 IR

The semantic Match IR is:

<!-- hard-unit kind="formula" -->
```rust
struct Match {
    src: u64,
    dst: u64,
    len: u64,
}
```
<!-- /hard-unit -->

During matching, every candidate additionally carries a deterministic `insertion_ordinal` and canonical key metadata. After normalization, duplicate selected triples are removed and the remaining selected triples are assigned an `origin_match_id` in canonical IR order `(dst asc, src asc, len desc)`. The insertion ordinal is used to resolve equal schedules but is not required as a persisted field in the IndexSection match triple.

Every accepted candidate and every selected match MUST satisfy:

<!-- hard-unit kind="list" -->
- `src < dst`;
- a selected match has `len >= effective_min_match`;
- all additions, subtractions, products, divisions, conversions, and interval endpoints use checked arithmetic;
- `dst + len` does not overflow and is within the input length;
- source reads are valid under LZ overlap semantics; and
- its destination interval is inside the input. Only the selected matches after normalization MUST be sorted and non-overlapping, and the resulting literals plus matches MUST reconstruct exactly the source input.
<!-- /hard-unit -->

Candidate generation MUST return an error for invalid arithmetic, impossible source/destination relationships, or resource exhaustion. It MUST NOT silently drop candidates, reduce search depth, shorten the dictionary, switch method, or switch layout. Finder-specific generation retains provenance rules: a base m3/m4/m5 candidate MUST meet the base `minimum_match` and its method-specific rounding rules, while an REP overlay m0 candidate MUST meet `rep_min_match`. The normalizer and every layout encoder accept the combined candidate set down to `effective_min_match`; they MUST NOT require candidate provenance. This is sufficient for decoding, but does not prove provenance. Integrity and finder tests establish that the encoder applied the source-specific generation rules.

### 6.2 Canonical virtual encoding cost

Normalization uses one virtual encoding independent of layout:

<!-- hard-unit kind="list" -->
- each literal byte costs exactly **1**;
- each match instruction costs exactly **25**, consisting of one logical tag byte plus three `u64` fields;
- framing, record, block, index, and run overhead are deliberately excluded.
<!-- /hard-unit -->

A candidate with `len <= 25` has no positive gain and MUST be omitted. For each remaining candidate, gain is `len - 25`. The normalizer MUST select a non-overlapping set maximizing:

<!-- hard-unit kind="formula" -->
```text
sum(len - 25) over selected matches
```
<!-- /hard-unit -->

It MUST solve this as deterministic weighted interval scheduling over the complete candidate set, not as a longest-match or greedy-only choice. Intervals are `[dst, dst+len)`.

The tie order between equally optimal schedules is:

<!-- hard-unit kind="list" -->
1. greater total gain;
2. greater total covered bytes;
3. fewer selected matches; and
4. lexicographically smaller selected sequence of tuples `(dst asc, src asc, len desc, insertion_ordinal asc)`.
<!-- /hard-unit -->

The sequence comparison is over selected matches in ascending `dst`; if one sequence is a strict prefix of another after the preceding tie criteria, the shorter sequence is lexicographically smaller. The schedule is therefore deterministic for every complete candidate set.

This virtual cost is the only cost used for semantic IR selection. Layout-specific encoders MAY split a match at destination block boundaries for representation, but they MUST preserve its `origin_match_id`, original `(src,dst,len)`, and semantic statistics. Fragments MUST NOT participate in normalization and MUST NOT change match count, covered bytes, literal bytes, or the selected IR. All claims that layout-specific encoded costs feed IR selection are expressly excluded.

### 6.3 Statistics

Semantic statistics are calculated from original normalized triples, never from layout fragments:

<!-- hard-unit kind="list" -->
- `semantic_match_count` is the number of selected original triples;
- `covered_bytes` is the checked sum of their original lengths;
- `literal_bytes = uncompressed_len - covered_bytes`;
- `origin_match_id` is the zero-based canonical selected-triple position; and
- layout operation counts and serialized byte counts are separate representation statistics.
<!-- /hard-unit -->

The three layouts MUST have identical semantic match count, covered bytes, literal bytes, and origin-triple sequence for a given input and semantic configuration.

## 7. CandidateIndex and deterministic history

### 7.1 Interface and visibility

The matching layer MUST use an interface equivalent to:

<!-- hard-unit kind="formula" -->
```text
insert(key_kind, key_bytes, position, insertion_ordinal, metadata_bytes)
candidates(key_kind, key_bytes, before, max_distance) -> deterministic sequence
finish_epoch()
```
<!-- /hard-unit -->

The logical identity of an index entry is exactly `(key_kind, key_bytes, position, metadata_bytes)`. Re-inserting the exact identity is idempotent and retains the first, minimum insertion ordinal. Entries with the same key kind, key bytes, and position but distinct metadata are distinct entries. Compaction deduplicates exact identities only and retains the minimum insertion ordinal among duplicate records.

Canonical key and metadata encodings are:

<!-- hard-requirements -->
| Method | `key_kind` | `key_bytes` | `metadata_bytes` |
|---|---:|---|---|
| m0 | 0 | polynomial hash `u64` LE, 8 bytes | empty |
| m1 | 1 | `chunk_len u64 LE` followed by BLAKE3-128, 24 bytes | empty |
| m2 | 2 | `chunk_len u64 LE` followed by BLAKE3-128, 24 bytes | empty |
| m3 | 3 | `L u64 LE` followed by BLAKE3-128, 24 bytes | empty |
| m4 | 4 | `L u64 LE` followed by polynomial hash `u64` LE, 16 bytes | empty |
| m5 | 5 | `L u64 LE` followed by polynomial hash `u64` LE, 16 bytes | eight four-bit slice fingerprints packed into one `u32` LE; slice 0 occupies bits 0–3, slice 1 bits 4–7, through slice 7 bits 28–31 |
| REP overlay | 0 | polynomial hash `u64` LE, 8 bytes | empty |

The finder MUST query the exact key kind and exact key bytes. The MatchFinder assigns `insertion_ordinal` before storage. It is a checked global counter over the method's deterministic source-seed enumeration order. Each finder enumerates source seeds in increasing absolute position; overlay candidates continue the same counter after base-method candidates. A failed insert is an error, not an absent candidate.

A query snapshot includes every successfully inserted entry visible before the call, including current-epoch inserts. `finish_epoch()` creates a durability/visibility checkpoint and MUST NOT hide prior current-epoch inserts. Query filtering requires `position < before`; when `max_distance != 0`, it additionally requires inclusive `before-position <= max_distance`. Arithmetic overflow is rejected. There is no semantic query result cap.

The returned candidate sequence is sorted exactly by position descending, insertion ordinal ascending, and metadata bytes lexicographically ascending. Visible immutable run files sort by:

<!-- hard-unit kind="formula" -->
```text
(key_kind, key_bytes lexicographic, position descending,
 insertion_ordinal ascending, metadata_bytes lexicographic)
```
<!-- /hard-unit -->

RAM and spill implementations MUST produce byte-identical sequences. Compaction MUST be stable, deduplicate exact identities only, retain their minimum ordinal, and preserve the specified query sequence and candidate set.

### 7.2 RAM and paged implementations

The RAM implementation MAY retain entries directly. The paged implementation MUST be a deterministic sorted-run/LSM-like index containing a compact RAM memtable, immutable sorted runs, deterministic query merging, and deterministic compaction. The sparse directory is operational only: it is rebuilt or validated from run records and is not persisted in the temporary run schema. This keeps the visible run file schema exact.

<!-- hard-unit kind="paragraph" -->
For CandidateIndex, an unqualified `run` means a visible immutable run. Every visible run exposed to CandidateIndex queries, `finish_epoch()`, or compaction input/output MUST use the exact `SREPIDX1` 64-byte header and 104-byte record schema below and MUST use persisted order. The explicitly named private query/compaction scratch is the sole exception to the word `run` in this subsection; it is an operational scratch file, not a visible immutable run, and it MUST NOT be appended to `self.runs` or treated as a persisted run.
<!-- /hard-unit -->

<!-- hard-requirements -->
| CandidateIndex artifact | Magic/header | Record order | Visibility and lifetime |
|---|---|---|---|
| Visible immutable run | Exact `SREPIDX1` 64-byte header; exact 104-byte SREP-IDX-REC records | Persisted order `(key_kind, key_bytes, position desc, insertion_ordinal asc, metadata_bytes)` | Visible to queries, `finish_epoch()`, and compaction; retained only as indexed state |
| Private query/compaction scratch | Exact `SREPQRY1` 64-byte header; exact 104-byte SREP-IDX-REC records | Header-authorized identity order (`order_id=1`) or persisted-query order (`order_id=2`) | Local transaction only; never visible as a run or retained after successful operation |
<!-- /hard-requirements -->

Every CandidateIndex run record is exactly 104 bytes:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 1 | `key_kind` |
| 3 | 1 | `key_len` |
| 4 | 1 | `metadata_len` |
| 5 | 1 | Flags, exactly `0` |
| 6 | 2 | Reserved, exactly `0` |
| 8 | 8 | `position` |
| 16 | 8 | `insertion_ordinal` |
| 24 | 32 | `key_bytes`, zero-padded after `key_len` |
| 56 | 32 | `metadata_bytes`, zero-padded after `metadata_len` |
| 88 | 16 | XXH3-128 checksum |

The record checksum covers exactly ASCII bytes `SREP-IDX-REC\0` followed by the first 88 record bytes. The reader MUST validate `key_len<=32`, `metadata_len<=32`, zero padding, allowed key/metadata shapes from the table above, schema, flags, reserved bytes, and checksum before using the record.

Every visible immutable run file has an exact 64-byte header and no trailer. The header has no persisted minimum/maximum kind or key summaries; its fields are:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | Magic ASCII `SREPIDX1` |
| 8 | 2 | Version `1` |
| 10 | 2 | Record size `104` |
| 12 | 4 | Flags, exactly `0` |
| 16 | 8 | `record_count` |
| 24 | 8 | `run_generation` |
| 32 | 16 | Random run nonce |
| 48 | 16 | Header XXH3-128 checksum |

The visible-run header checksum covers exactly ASCII bytes `SREP-IDX-HDR\0` followed by header bytes 0 through 47. A visible run's exact file length MUST be `64 + 104*record_count`, using checked arithmetic; no run trailer exists. The implementation MUST flush, fsync, close, and validate a visible run before making it visible to queries or compaction. The nonce MUST be generated for each run and is not a semantic ordering key. Memory pressure MAY change cache size, page residency, and compaction frequency only. It MUST NOT change candidate set, candidate order, normalized IR, or output bytes. Temporary budget exhaustion MUST return `TempBudgetExceeded`; inability to continue in RAM without an available spill path MUST return `MemoryBudgetExceeded`.

### 7.2.1 Private query/compaction scratch format

Private scratch is a self-describing operational format. It is not an alternate visible run schema, an archive format, or a persistent compatibility surface.

<!-- hard-requirements -->
| Offset | Size | Field and required value |
|---:|---:|---|
| 0 | 8 | Magic ASCII `SREPQRY1` |
| 8 | 2 | Version `1` |
| 10 | 2 | Record size `104` |
| 12 | 1 | `order_id`: `1` identity order or `2` persisted-query order |
| 13 | 1 | Flags, exactly `0` |
| 14 | 2 | Reserved, exactly `0` |
| 16 | 8 | `count`, the number of 104-byte records |
| 24 | 8 | `transaction_generation` |
| 32 | 16 | Random scratch nonce |
| 48 | 16 | XXH3-128 header checksum |
<!-- /hard-requirements -->

The private scratch header is exactly 64 bytes and has no trailer. Its checksum covers exactly ASCII bytes `SREP-QRY-HDR\0` followed by header bytes 0 through 47. XXH3-128 uses the same serialized representation as the visible-run checksum. The exact scratch file length MUST be `64 + 104*count`, using checked arithmetic. `order_id` is authoritative and MUST be one of exactly `1` or `2`; the order MUST NOT be inferred from the filename, transaction state, or observed records.

<!-- hard-requirements -->
| `order_id` | Name | Exact ascending file order | Operational purpose |
|---:|---|---|---|
| 1 | Identity | `(key_kind, key_bytes, position, metadata_bytes, insertion_ordinal)` | External bounded exact deduplication during compaction |
| 2 | Persisted-query | `(key_kind, key_bytes, position desc, insertion_ordinal asc, metadata_bytes)` | Validated query materialization and query/visible-run merge output |
<!-- /hard-requirements -->

The order-1 tuple is exact. In both order definitions, `key_bytes` and `metadata_bytes` mean complete fixed canonical comparison fields `(validated_length, 32-byte zero-padded field)` from the 104-byte record; comparison MUST NOT use an out-of-band length or order. Thus the concrete order-1 projection is `(key_kind, (key_len, key_bytes[0..32]), position, (metadata_len, metadata_bytes[0..32]), insertion_ordinal)`, and order 2 uses the same key and metadata projections in the stated persisted-query order. The logical identity remains `(key_kind, key_bytes, position, metadata_bytes)`, with logical bytes interpreted through those validated lengths. Consequently, exact identity duplicates are adjacent in order 1, and the lowest `insertion_ordinal` is retained. Distinct metadata remains distinct. Order 2 is exactly the visible persisted order and the query callback order.

<!-- hard-unit kind="list" -->
- Scratch records reuse the exact SREP-IDX-REC encoding, checksum domain, fixed shape, key/metadata length checks, zero-padding checks, and allowed key/metadata shape validation defined above.
- Before consumption, the reader MUST validate the scratch magic, version, record size, order ID, flags, reserved bytes, count, transaction generation, nonce, header checksum, exact file length, every record checksum and shape, and the complete sorted order selected by the header's `order_id`.
- A file whose records are sorted under the other order, or whose order ID is changed without resorting the records, MUST be rejected. The reader MUST NOT reinterpret an order-1 file as order 2 or an order-2 file as order 1.
- Compaction MAY write order-1 scratch, deduplicate only adjacent exact identities while retaining the minimum ordinal, and then write a new visible run in exact `SREPIDX1` persisted order. Relabeling an order-1 scratch file as a visible run is forbidden.
- A query callback MUST consume either validated order-2 scratch or a direct validated merge of visible runs. It MUST receive the exact canonical candidate sequence and exact deduplicated identities; scratch ordering and deduplication MUST NOT alter RAM-equivalent results.
<!-- /hard-unit -->

<!-- hard-unit kind="list" -->
- Scratch exists only inside the local query or compaction transaction. It MUST never be appended to `self.runs`, exposed to callbacks as a persistent run, or retained after successful query/compaction completion.
- Scratch creation MUST use a random exclusive name and restrictive `0600` permissions where the platform exposes them. Scratch and visible-run growth MUST reserve physical bytes from the shared `TempBudget` before allocation or file growth.
- Scratch and newly written visible files MUST be complete, flushed, fsynced, closed, and validated before a query callback, `finish_epoch()` visibility change, compaction state commit, or any final callback observes them. Corruption or validation failure MUST be reported before callback or state commit.
- RAII ownership MUST clean every scratch and uncommitted new visible path created by the transaction on success, error, cancellation, and panic on a best-effort basis, without removing an unrelated pre-existing path. No scratch file may survive successful operation.
- The transaction MUST preserve the pre-operation visible-run set, memtable, and generation until all required scratch and new visible files have passed validation. On any failure, scratch and uncommitted new visible files MUST be dropped and the visible runs, memtable, and generation MUST remain unchanged.
<!-- /hard-unit -->

The final compaction output MUST be converted to and validated as a visible `SREPIDX1` run in exact persisted order before it is committed to CandidateIndex state. This private scratch clarification changes no visible run bytes, persisted archive schema, archive semantics, or archive compatibility boundary.

### 7.2.2 CandidateIndex scratch and budget accounting

Private query/compaction scratch is included wherever this specification says that CandidateIndex temporary storage shares the one `temp_limit`; it does not create a second budget or a persistent index artifact. The transaction generation in its header binds scratch to the local operation and MUST be checked before consumption. Any reference elsewhere to “every run file” means every visible immutable `SREPIDX1` run and excludes only the explicitly named private query/compaction scratch format in this subsection.

### 7.3 DataSource and complete history

A seekable DataSource MUST support checked `read_at(position, length)` operations. Whole-history matching and reread methods on stdin MUST spool stdin into an owned temporary DataSource before matching. Compression MAY spool or use `read_at` even when `max_distance` is finite; finite distance does not select a one-pass mode. No method may substitute a short recent-history ring for complete history when `max_distance=0`.

The default complete history does not require retaining all bytes in RAM. The DataSource MAY read from the original file or a private spool, and the CandidateIndex MAY page independently. A future one-pass mode, if later proposed, requires a separately reviewed semantic design and is outside this v2 specification.

### 7.4 Temporary ownership

Temporary resources MUST use RAII ownership, random exclusive names, and restrictive creation permissions on platforms that expose restrictive temporary-file creation. The manager MUST clean only paths it created, on success, error, cancellation, and panic on a best-effort basis. It MUST never remove an unrelated pre-existing path. A temp-limit failure MUST occur before silently creating additional unaccounted storage.

File output MUST be staged and atomically published only after complete encoding, flushing, verification, and close success. A resource, temp, checksum, decode, or I/O error MUST NOT publish partial file output.

## 8. Exact method semantics

All methods use absolute input positions. Archive block boundaries are those in Section 4.1. A method's candidate index contains only source positions that have become visible under its enumeration rules; it MUST never query future positions. The candidate sequence for a finder is the concatenation of its explicitly defined enumeration phases in this order: source-seed insertion for the current searchable prefix, target-seed query in ascending target position, then any method-defined extension/overlay phase. Within each phase, absolute positions are ascending; the MatchFinder assigns ordinals before calling `insert`, and overlay candidates continue the same counter after base-method candidates.

### 8.1 m0 / REP

`m0` retains REP algorithm semantics and uses complete history by default through DataSource and CandidateIndex. A finite history exists only when selected through `--max-distance`; finite mode may still spool or use `read_at`.

Let:

<!-- hard-unit kind="formula" -->
```text
R = max(1, floor(finder_min_match / 8))
```
<!-- /hard-unit -->

For standalone m0, `finder_min_match=minimum_match`; for an REP overlay, `finder_min_match=rep_min_match` and `R=rep_region_size`. For the default minimum match of 512, `R=64`. Partition absolute input positions into regions `[kR,(k+1)R)`. In each region, consider every window start `x` in that region for which `[x,x+R)` exists. Compute the polynomial hash of those `R` bytes. The representative is the start with maximum hash; equal maximum hashes select the lowest absolute start. The representative MUST be inserted only after the entire region's eligible window starts have been considered.

The target scan considers every target window start `p` for which `[p,p+R)` exists, in ascending absolute position. It queries all equal-key source representatives, not only recent representatives. Candidate positions are filtered by `src < p` and the configured effective distance.

For every returned representative with source start `s` and target start `p`, exact-compare the `R`-byte seed. Backward extension is independent for this candidate and uses no state from another candidate. Let `b` be the largest checked nonnegative integer satisfying `b <= s`, `b <= p`, and equality of the `b` source/target bytes immediately preceding `s` and `p`; `b` is also limited by the input beginning. The configured maximum-distance check applies to the candidate distance `p-s`; shifting both starts equally preserves that positive distance, so distance does not impose a separate backward boundary. Set `src=s-b` and `dst=p-b`. The final candidate MUST retain `src < dst`, which follows from `s < p` and equal shifting. Extend forward byte by byte from the seed end to the first mismatch or input end, using exact DataSource comparison and checked endpoints. Emit the candidate only when its final length is at least the configured finder minimum, which is `minimum_match` for standalone m0 and `rep_min_match` for an REP overlay. Overlap is valid and is confirmed according to the byte-at-a-time LZ rule.

This complete per-candidate generation deliberately does not reproduce the old greedy `last_match_end` behavior. The weighted interval normalizer, and only that normalizer, resolves destination conflicts after the complete candidate set has been generated; retaining REP semantics does not justify dropping candidates during matching.

### 8.2 m1 / rolling CDC

`m1` uses the following exact rolling state machine independently for each nonempty archive block. Archive-block reset is a target v2 deterministic boundary; old stripe-parallelization implementation details are not semantic. If `block_len <= 48`, emit exactly one forced chunk `[block_start,block_end)` and perform no natural-boundary test. Otherwise initialize the window to `data[block_start..block_start+48)`, initialize `H` to its polynomial hash, and set `last_boundary=block_start`. The initial unrolled window is never tested.

For each `p` from `block_start+48` while `p < block_end`, roll first by removing byte `data[p-48]` and adding byte `data[p]`. After the roll, `H` covers `[p-47,p+1)`, and the trigger byte at offset `p` is included in that hash. Then test:

<!-- hard-unit kind="formula" -->
```text
H > u64::MAX - floor(u64::MAX / target_chunk)
and
p - last_boundary >= min_chunk
```
<!-- /hard-unit -->

When the test hits, the boundary is at `p` and the completed chunk is `[last_boundary,p)`. The triggering byte at offset `p` participates in the hash but belongs to the next chunk. Set `last_boundary=p`. Do not reset the rolling hash at a natural boundary. The loop stops before `block_end`; force `[last_boundary,block_end)` only when it is nonempty. Thus the trigger byte belongs to the forced or subsequent chunk, and no empty chunk is emitted. Empty input has no chunks. The initial window is not tested even when the block length is exactly 48. The implementation MUST use strict `>` and checked arithmetic.

Index each nonempty chunk by `(chunk_length, BLAKE3-128(chunk))`. For every equal-key source chunk, use CandidateIndex canonical ordering, exact-compare the complete chunk bytes, and emit a whole-chunk match only after exact equality. A chunk match has `src` and `dst` at chunk starts and `len` equal to the full chunk length; it is not byte-extended beyond the chunk. Digest collision equality alone is never sufficient. Required semantic vectors include block lengths 48, 49, and `48+min_chunk`, with forced hash values that exercise both hit and no-hit paths.

### 8.3 m2 / order-1 CDC

`m2` is distinct from m1 and intentionally preserves the old predictor-state quirk. Archive-block reset is the deterministic v2 choice; the old stripe-parallelization implementation details are not semantic. At the start of each nonempty archive block and after each boundary, initialize:

<!-- hard-unit kind="formula" -->
```text
prev = 0
predict[0..256] = 0
h = 0
last_boundary = block_start
```
<!-- /hard-unit -->

Scan each byte at absolute zero-based offset `p`, including `p=block_start`, in order. Set `c=data[p]`, then:

<!-- hard-unit kind="formula" -->
```text
wrong = (c != predict[prev])
multiplier = 271828182 if wrong else 314159265
h = (h + u32(c) + 1) * multiplier modulo 2^32
predict[prev] = c
prev = c
```
<!-- /hard-unit -->

A natural boundary is triggered exactly when the prospective completed chunk `[last_boundary,p)` has length `p-last_boundary >= min_chunk` and:

<!-- hard-unit kind="formula" -->
```text
h > u32::MAX - floor(u32::MAX / target_chunk)
```
<!-- /hard-unit -->

The boundary is at `p`; the triggering byte at offset `p` belongs to the next chunk. Reset `prev`, `predict`, and `h` immediately after processing `p` and before scanning `p+1`, and set `last_boundary=p`. The byte at `p` is not replayed through the reset state. The segmentation state is only a boundary detector and is not required to summarize every byte in the resulting chunk; consequently, the first byte of the new chunk is intentionally absent from the reset predictor state. This is the observed/source-defined old semantic rule.

Scan through `p < block_end`, starting at the block start. Force `[last_boundary,block_end)` only when it is nonempty. Empty blocks produce no chunk. Required semantic vectors include a first eligible hit at `p=block_start+min_chunk` and consecutive post-reset hits, proving that the trigger is not replayed and that the exact `p-last_boundary` eligibility is used. Index and confirm m2 chunks exactly as m1: key by `(length, BLAKE3-128(chunk))`, enumerate all canonical equal-key candidates, exact-compare the complete chunk, and emit only whole-chunk matches. The order-1 state and m1 rolling state MUST remain separate.

### 8.4 m3 / fixed digest

Let `L = header.seed_size`. `L` MUST be nonzero. By default `L=minimum_match`. Source seeds begin at every absolute multiple `kL` for which `[kL,kL+L)` exists. Destination seed starts are every absolute position `p` for which `[p,p+L)` exists; destination seeds need not be aligned. An m3 seed query MUST enumerate target starts in ascending absolute position and evaluate every matching source candidate returned by CandidateIndex.

Index source seeds by `(L, BLAKE3-128(seed))`. For each destination seed, query all canonical equal-key source candidates, require `src < dst`, exact-compare the seed, and extend forward only in complete `L`-byte units. Each extension unit is first filtered by BLAKE3-128 and then exact-compared byte-for-byte. Stop at the first digest or byte mismatch or input end.

Let `q_min = ceil_div(minimum_match,L)`, where the ceiling division is checked and defined as `q_min = (minimum_match / L) + (minimum_match % L != 0 ? 1 : 0)`. A candidate is exactly `(src=kL, dst=p, len=qL)` with `q >= q_min`, so `qL >= minimum_match`. The header MAY set `L < minimum_match`; this checked q-minimum rule remains authoritative. The source start and length follow fixed-block alignment/rounding semantics while the destination may be non-aligned. No backward extension is performed by the base m3 method. Every unit is digest-filtered and then byte-compared exactly. Without REP overlay, these rules are the complete m3 semantics. With REP overlay, overlay candidates are separate m0 candidates; the round restriction is disabled only for overlay candidates and remains in force for base m3 candidates. Required vectors include minimum matches not divisible by `L`, including `L=3, minimum_match=7`.

### 8.5 m4 / reread

Let `L = header.seed_size`, defaulting to `minimum_match`, and require `L != 0`. Source seeds begin at every absolute multiple `kL` with `L` bytes available. Destination seed starts are every byte position `p` with `L` bytes available. Query all canonical equal-key candidates, exact-confirm the seed by rereading DataSource, and do not use a digest equality as proof. An m4 seed query MUST enumerate target starts in ascending absolute position and evaluate every matching source candidate returned by CandidateIndex.

For each seed candidate with source start `s` and target start `p`, exact-confirm the seed, then extend backward independently for this candidate. Candidate generation MUST use no cross-candidate interval state and MUST NOT use `last_match_end`. Let `b` be the largest checked nonnegative value no greater than `s` and no greater than `p`, and limited by the available equal bytes immediately before `s` and `p` and by the input beginning. There is no seed-length backward cap. The configured maximum-distance check applies to the candidate distance `p-s`; shifting both starts by `b` preserves that positive distance, so it does not impose a separate backward boundary. Set `src=s-b`, `dst=p-b`, preserving `src<dst`, and extend forward byte-by-byte to the first mismatch or input end. The final candidate's `src`, `dst`, and `len` include the backward extension. Every source byte read for confirmation and extension MAY be reread from the original or spooled input; it MUST be exact. All canonical equal candidates are evaluated and passed to the normalizer, but a base m4 candidate MUST have `len >= minimum_match` and an overlay m0 candidate MUST have `len >= rep_min_match`; no single-candidate cap is semantic. This complete independent generation is a deliberate completeness improvement over the old greedy `last_match_end` behavior; it retains fixed-seed/reread capability semantics while leaving all destination conflict resolution to the weighted interval normalizer.

### 8.6 m5 / exhaustive

Let `minimum_match` be checked before calculation. Require `minimum_match >= 2`, then compute the original checked formula:

<!-- hard-unit kind="formula" -->
```text
k = floor(log2(minimum_match + 1))
L = 2^(k - 1)
```
<!-- /hard-unit -->

The addition `minimum_match+1`, logarithm result, subtraction, and shift MUST be checked, and the implementation MUST reject any input for which `minimum_match+1` overflows or `L=0`. The supported hard limits for `minimum_match`, `L`, and input sizes are separate resource/configuration limits; m5 adds no rejection rule based on comparing `2L` with `minimum_match`. The implementation MUST NOT reject `2L > minimum_match`, and `minimum_match=2^k-1` is valid. From `2^k <= minimum_match+1 < 2^(k+1)` and `L=2^(k-1)`, it follows that `minimum_match >= 2^k-1 = 2L-1`; equivalently, `minimum_match >= 2L-1`. Therefore every interval of length at least `2L-1` contains a complete source-grid-aligned L-byte seed.

Source seeds begin at every absolute multiple `kL` with `L` bytes available. Target seed starts are every byte position with `L` bytes available. Index source seeds by `(L, H(seed))`, using the fixed polynomial hash as a candidate key. For every target seed, query **all** equal-key source positions in CandidateIndex canonical order; there is no candidate count or search-depth cap.

Apply the eight-slice filter to each candidate. Split the `L`-byte seed into eight deterministic contiguous slices: if `L = 8q+r`, the first `r` slices have length `q+1` and the remaining slices have length `q`. The supported hard minimum `minimum_match >= 2` gives `L>=1`; if any slice is empty because a configured L is smaller than eight, its fingerprint is the polynomial hash of the empty slice and it remains a defined filter component. Store the low four bits of each slice's polynomial hash. A candidate is rejected when any slice fingerprint differs. Equal fingerprints never confirm a candidate. After the filter, exact-compare the complete `L`-byte seed, then extend backward and forward byte-for-byte exactly as m4. Deduplicate identical final `(src,dst,len)` triples and emit every base m5 candidate whose final length is at least `minimum_match`, or every overlay m0 candidate whose final length is at least `rep_min_match`, to the normalizer.

The completeness argument is normative: every interval of length at least `minimum_match` also has length at least `2L-1`, so it contains a complete aligned source seed; the corresponding target seed position is scanned because every target position is examined. All candidates that can meet the minimum therefore reach exact confirmation. The implementation MUST NOT cap candidate count, hash probes, or extension candidates as a semantic shortcut. Required tests cover minimums around powers of two, specifically 3, 6, 7, 8, 15, 16, 511, and 512. Temporary exhaustion fails explicitly.

### 8.7 REP overlay execution

For `m3`, `m4`, or `m5` with overlay enabled, execute the m0 algorithm from Section 8.1 with `rep_min_match`, `rep_region_size`, and effective overlay distance from Section 4.3. Overlay candidates MUST meet `rep_min_match`; base candidates retain their method-specific base `minimum_match` and rounding rules. Overlay candidates and base-method candidates share the same normalizer and insertion-ordinal discipline, which accepts candidates down to `effective_min_match` without requiring provenance. `m1` and `m2` MUST reject overlay configuration before matching begins.

### 8.8 Optimization boundary

SIMD, prefetching, bit filters, threading, vectorized comparison, parallel search, and other performance transformations are future performance work. They MUST be postponed until the fidelity gate passes and MUST preserve exact candidate sets, all required confirmations, overlap semantics, deterministic order, and resource-failure behavior.

## 9. Layouts and reconstruction

All layout encoders consume one normalized canonical IR. For identical input and semantic configuration, Index-LZ, Future-LZ, and I/O-LZ MUST have exactly the same original match triples, match count, covered bytes, and literal bytes.

All layout decoders MUST derive `effective_min_match` from the validated ArchiveHeader and MethodParameters fields before validating persisted semantic matches. Every persisted semantic match or reassembled origin triple MUST have `len >= effective_min_match`; a representation fragment MAY be shorter only when it is part of such an origin triple. The decoder does not and cannot infer base-versus-overlay provenance from the wire Match IR, and no provenance bit is added.

### 9.1 Index-LZ

Index-LZ is the default layout. DataBlock records contain literal runs. The complete embedded match index is in an IndexSection after all DataBlock records and before ArchiveSummary. The BlockDirectory identifies each DataBlock and the range of matches whose destination starts in that block.

The decoder MUST first obtain and validate the tail IndexSection using the trailer. A seekable archive jumps to the index; a nonseekable archive input is spooled before tail lookup. It then reconstructs destination blocks into a seekable HistorySink. An atomic output temp file MAY be the HistorySink. A generic nonseekable writer, including stdout, MUST use a private seekable output spool, fully verify the reconstruction, and copy the verified bytes afterward.

The decoder loads matches whose destination starts in the current block and carries active matches across later blocks. For each active match at destination offset `i`, it reads `HistorySink[src+i]` and writes one byte at a time. Because `src < dst`, the read location is already generated; overlap naturally repeats the distance period. A match crossing a block boundary remains one semantic origin triple and one active state, not a new semantic match. The decoder MUST validate all literal gaps, match coverage, source bounds, destination bounds, checksums, summary counters, and final digest.

### 9.2 Future-LZ

Future-LZ transforms each normalized backward reference into exactly one FutureRegister attached to the DataBlock containing its source start. For a match `(src,dst,len)`, define:

<!-- hard-unit kind="formula" -->
```text
d = dst - src
p = min(len, d)
```
<!-- /hard-unit -->

The register is:

<!-- hard-unit kind="formula" -->
```text
(origin_match_id, source_start=src, destination=dst,
 total_len=len, period_len=p)
```
<!-- /hard-unit -->

Registers are ordered by `(source_start, destination, origin_match_id)`. The decoder reads registrations before reconstructing their source block. It creates a collector for `[source_start, source_start+p)`, retains the period bytes as they are reconstructed, and feeds every reconstructed byte to all active collectors in absolute order. Since `p <= d`, the collector completes no later than destination `dst`. At `dst`, the decoder outputs `total_len` bytes by cycling the collected period bytes. This is exactly the byte-at-a-time overlapping LZ result.

A registration whose source span crosses a source block remains active across blocks. A registration with source and destination in the same block is installed before that block is reconstructed, so it can collect source bytes and deliver at its destination during the same block. A registration whose delivery crosses destination blocks remains active until its final byte. The normalized IR guarantees that no more than one delivery covers any target byte; a decoder MUST reject conflicting delivery coverage.

Future origin IDs are assigned by canonical IR order: IDs are exactly `0..semantic_match_count-1`, with ID zero assigned to the first canonical triple and the final ID equal to `semantic_match_count-1`. Physical FutureRegister entries are sorted by `(source_start, destination, origin_match_id)` for delivery. The strict decoder MUST accumulate each register's metadata in an ID-indexed RAM or temporary-spill table and MUST require every ID in `0..semantic_match_count-1` exactly once, with no duplicate or missing ID. No FutureRegister exists when `semantic_match_count=0`. After all registration metadata has been seen, or during final archive validation if delivery was streamed earlier, the decoder MUST iterate the ID table in ID order and require canonical normalized IR order `(dst asc, src asc, len desc)`, `src < dst`, `total_len >= effective_min_match`, destination non-overlap, exact `period_len=min(total_len,destination-src)`, exact counters, and exact correspondence to the declared block/source ownership. It MUST reconstruct and retain the decoded semantic IR for matrix assertions. Delivery MAY proceed before this final metadata validation, but any malformed metadata MUST return an error and MUST prevent successful completion.

Pending state under RAM pressure MAY spill to a private temporary resource. Spill state MUST include the origin ID, source/destination/length/period metadata, collector progress, and collected period bytes, with checked versioned lengths and checksums. Failure before the configured temporary byte budget is exhausted maps to `TemporaryStorageFailure`; failure because the configured temporary byte budget would be exceeded maps to `TempBudgetExceeded`. Neither failure may shorten the period or discard a registration.

Literal runs fill every target interval not delivered by a normalized match. Each reconstructed block is checked against its DataBlock representation-plus-semantics checksum before a direct stdout decoder emits that block. A final summary or archive-digest failure may occur after earlier stdout bytes have been emitted; stdout cannot be rolled back and the error MUST still be returned. File output remains atomic and is not published after such a failure.

### 9.3 I/O-LZ

I/O-LZ interleaves literal runs and backward-reference fragments in destination order. The decoder reads already-generated bytes from a seekable output sink and copies matches byte by byte, including overlap.

For stdout or any generic nonseekable destination, the decoder MUST use a private temporary output spool, verify all blocks and the final summary, and only then copy verified bytes to the destination. It MUST never replace complete history with a short dictionary merely because the public writer is nonseekable.

### 9.4 Canonical literal representation

Index-LZ and Future-LZ MUST encode exactly one LiteralRun for each maximal uncovered destination interval intersected with a DataBlock. Clipping at block boundaries means runs in one block are maximal within that block; they MUST be ordered, nonadjacent, positive-length, and contain no zero-length run. In Future-LZ, all FutureRegister entries precede all literal runs in the DataBlock payload as specified below. Register source ordering is independent of literal-run ordering.

I/O-LZ MUST encode each maximal uncovered destination interval clipped to a DataBlock as exactly one tag-0 literal operation. Adjacent literal operations are forbidden. All operations MUST be strictly destination ordered. These canonical literal rules prevent layout encoders from introducing representation-only fragmentation and are validated independently of semantic match selection.

### 9.5 I/O fragment reassembly

The strict I/O-LZ decoder MUST group match fragments by `origin_match_id`. IDs MUST be exactly `0..semantic_match_count-1`, each exactly once as a reassembled origin, with no duplicate or missing origin. In global destination operation order, each origin's fragments MUST form one uninterrupted semantic coverage sequence: no literal or fragment belonging to another origin may occur between its fragments. The first fragment establishes the origin's `src` and `dst`; each next fragment MUST begin at the prior fragment's `src+len` and `dst+len`. A split is permitted if and only if the previous fragment ended exactly at a DataBlock destination end and more of that origin match remains. There is no gratuitous split within a block, and a one-block match has one fragment. The decoder MUST retain a per-origin reassembly table until every origin is complete; it MUST reject a fragment that appears after its origin sequence was closed or that leaves a nonempty origin incomplete at end of stream.

After reassembly, every origin length MUST be at least `effective_min_match`, the canonical triples in ID order MUST satisfy normalized order and non-overlap, and reassembled counters MUST equal LayoutMetadata, ArchiveSummary, and the captured IR. The decoder MUST retain the reassembled semantic IR for matrix assertions. The record boundary itself does not interrupt a sequence; only the required destination-block boundary may split it.

## 10. SREP-NG v2 wire format

All persisted positions, lengths, offsets, sizes, counts, and schema values are little-endian. Fields specified as `u64` are unsigned little-endian 64-bit values. Every parser and encoder MUST use checked arithmetic and MUST apply allocation/resource limits before allocating from an untrusted length. No canonical field has alternate encodings.

### 10.0 Wire limits versus operational limits

The following are wire hard limits. A v2 archive or configuration exceeding one is invalid, regardless of local RAM, disk, or caller policy:

<!-- hard-unit kind="list" -->
- `MAX_UNCOMPRESSED = 2^63-1`; every position and endpoint MUST be at most `MAX_UNCOMPRESSED`;
- block size is `1 KiB..=1 GiB`, with default 8 MiB;
- minimum match is `2..=1 GiB`; m1/m2 minimum chunk is also positive within this range;
- m1/m2 target chunk is `32..=1 GiB` and MUST be at least its minimum chunk;
- every nonzero seed size is `1..=1 GiB`; m2's seed field is exactly zero;
- maximum distance is zero or at most `MAX_UNCOMPRESSED`;
- record payload, archive total, counts, and all checked formulas are at most `MAX_UNCOMPRESSED` and must fit `u64`;
- overlay distance is positive and at most `MAX_UNCOMPRESSED`, and overlay minimum is `2..=1 GiB`; and
- an I/O literal operation has `literal_len <= u32::MAX-16`, because `encoded_len=16+literal_len` is a `u32`.
<!-- /hard-unit -->

Violations in fixed/header scalar fields or MethodParameters are `CorruptHeader`; violations in IndexSection or BlockDirectory schemas and ranges are `CorruptIndex`; body-record, Summary, DataBlock framing, and other record structural wire-limit violations are `CorruptRecord`. These classifications are independent of local resource availability.

Operational limits are local decoder or compressor policy: `output_limit`, memory budget, and one shared temporary budget. Input spools, CandidateIndex visible runs, private query/compaction scratch, pending Future-LZ spill, output spools, and atomic staging output all charge the same `temp_limit`; there is no separate spool budget. A structurally valid archive exceeding `output_limit` maps to `OutputLimitExceeded`; exceeding the memory budget maps to `MemoryBudgetExceeded`; exceeding the shared temporary budget maps to `TempBudgetExceeded`; no wire violation maps to an operational-limit variant. Defaults are `output_limit=MAX_UNCOMPRESSED`; `memory=max(64 MiB, min(1 GiB, floor(physical_memory/4)))`; and `temp_limit=max(4 GiB, 4*memory)`, with available disk used only as a preflight cap/check and never as a semantic limit. If physical-memory or disk queries are unavailable, defaults are 256 MiB memory and 4 GiB temporary storage. Callers MAY override these operational defaults. None is persisted as a matching semantic. Reservations cover current allocated physical bytes, and the high-water statistic is retained after release for run statistics; the limit itself applies to the current shared reservation total.

### 10.1 ArchiveHeader: exact 80 bytes

The v2 magic is eight bytes `SREPNG2\0`, distinct from the prototype v1 magic. The fixed ArchiveHeader is exactly 80 bytes:

<!-- hard-requirements -->
| Offset | Size | Field and required value |
|---:|---:|---|
| 0 | 8 | Magic ASCII bytes `SREPNG2\0` |
| 8 | 1 | Version `2` |
| 9 | 1 | Header flags, exactly `0` in v2 |
| 10 | 1 | Checksum ID: `1` XXH3-128 or `2` BLAKE3-256 |
| 11 | 1 | Layout: `1` Index-LZ, `2` Future-LZ, `3` I/O-LZ |
| 12 | 1 | Method: `0` through `5` |
| 13 | 1 | Semantic flags: bit 0 REP overlay; bits 1–7 exactly zero |
| 14 | 2 | Reserved, exactly zero |
| 16 | 8 | Block size |
| 24 | 8 | Minimum match length |
| 32 | 8 | Primary seed size; method-specific validation below |
| 40 | 8 | Primary target chunk size; method-specific validation below |
| 48 | 8 | Maximum distance; `0` means complete history |
| 56 | 8 | Checksum seed, exactly `0` in v2 |
| 64 | 8 | Header record count, exactly `2` |
| 72 | 8 | Header byte length, exactly `80` |

The header flags MUST be exactly zero; there are no optional header flag bits in v2. Header semantic bit 0 MUST match MethodParameters flags. The REP bit MUST be zero for m0, m1, and m2, and MAY be one only for m3, m4, or m5. `max_distance=0` is the complete-history representation. `checksum_seed` is zero and cannot be changed through the CLI.

The default block size is 8 MiB. The wire limits in Section 10.0 are exhaustive for block size, minimum match, seed, target, distance, total output, record payload, archive total, counts, and operation sizes. A declared value outside those limits, an invalid method/layout combination, or a nonzero reserved field is rejected as `CorruptHeader`.

Header seed/target validation is exact:

<!-- hard-requirements -->
| Method | Header minimum match | Header seed size | Header target chunk | Other required fields |
|---|---:|---:|---:|---|
| m0 | default 512; positive | `minimum_match` | `0` | MethodParameters CDC/REP fields all zero; `R` derives from minimum |
| m1 | default 32; positive | `48` | positive; default 4096 | `cdc_window=48`, `cdc_min_chunk=minimum_match` |
| m2 | default 32; positive | `0` | positive; default 4096 | `cdc_window=0`, `cdc_min_chunk=minimum_match`, order table 256 |
| m3 | default 512; positive | nonzero `L`, default `minimum_match` | `0` | CDC fields zero; REP fields depend on overlay; effective minimum derives from validated fields |
| m4 | default 512; positive | nonzero `L`, default `minimum_match` | `0` | CDC fields zero; REP fields depend on overlay; effective minimum derives from validated fields |
| m5 | default 512; at least 2 | derived nonzero `L` | `0` | CDC fields zero; slice count 8; REP fields depend on overlay; effective minimum derives from validated fields |

For m1 and m2, `cdc_min_chunk` MUST equal the header minimum match. For m5, the header seed MUST equal the checked formula in Section 8.6, and `minimum_match` MUST be at least 2. When REP is enabled, `rep_min_match` MUST be positive and the checked effective minimum MUST be the minimum of the header minimum and `rep_min_match`; when REP is disabled, `rep_min_match` and `rep_region_size` MUST be zero and the effective minimum MUST equal the header minimum. Header or MethodParameters values that do not match these rules are rejected.

### 10.2 Record framing and cardinality

Every length-delimited record has exactly this framing, followed by one checksum of the selected width:

<!-- hard-requirements -->
| Offset in record | Size | Field |
|---:|---:|---|
| 0 | 1 | Type |
| 1 | 1 | Flags |
| 2 | 2 | Reserved, exactly zero |
| 4 | 8 | Payload length `payload_len` |
| 12 | `payload_len` | Payload bytes |
| `12 + payload_len` | checksum width | Selected checksum bytes |

The record total length is checked `12 + payload_len + checksum_width`. The checksum width is 16 bytes for checksum ID 1 and 32 bytes for checksum ID 2. Every v2 record's `flags` field is exactly zero; no record flag bits are defined. There is **one checksum selection in the ArchiveHeader**, and that one ID is normatively used at both granularities: ordinary record integrity, DataBlock representation-plus-semantics integrity, and the full archive semantic digest. There are no independently selectable record and archive checksum IDs, and there is no CRC.

Defined record types are:

<!-- hard-unit kind="list" -->
- `0x01` `MethodParameters`, mandatory exactly once;
- `0x02` `BlockDirectory`, mandatory exactly once for Index-LZ and forbidden for Future-LZ and I/O-LZ;
- `0x03` `DataBlock`, one per nonempty archive block, in increasing block ID;
- `0x04` `IndexSection`, mandatory exactly once for Index-LZ and forbidden for Future-LZ and I/O-LZ;
- `0x05` `LayoutMetadata`, mandatory exactly once; and
- `0x06` `ArchiveSummary`, mandatory exactly once after body/index records.
<!-- /hard-unit -->

`header_record_count` is exactly 2: record type `0x01` followed by record type `0x05`. This required header-record order is intentional; it is not a global numeric type ordering. The full required order is:

<!-- hard-unit kind="formula" -->
```text
ArchiveHeader
MethodParameters
LayoutMetadata
BlockDirectory                  (Index-LZ only)
DataBlock block 0..block_count-1
IndexSection                    (Index-LZ only)
ArchiveSummary
fixed Trailer
end of input
```
<!-- /hard-unit -->

For an empty archive, `block_count=0` and there are no DataBlock records. Index-LZ still has an empty BlockDirectory and empty IndexSection; Future-LZ and I/O-LZ have neither. Every nonempty archive has exactly one DataBlock per block. Unknown types are always rejected; v2 defines no skippable optional record type. Unknown flags, nonzero reserved fields, duplicate or missing mandatory records, bad order, invalid cardinality, overflowed offsets, out-of-range lengths, malformed payloads, and trailing data are rejected.

### 10.3 Record checksum domains

For `MethodParameters`, `LayoutMetadata`, `BlockDirectory`, `IndexSection`, and `ArchiveSummary`, the trailing checksum covers exactly this byte sequence:

<!-- hard-unit kind="formula" -->
```text
ASCII bytes "SREPNG2-RECORD\0"
12-byte serialized record frame
payload bytes
```
<!-- /hard-unit -->

The 12-byte frame includes type, flags, two zero reserved bytes, and little-endian `payload_len`. The domain has no length prefix and no additional terminator beyond the stated NUL byte.

A DataBlock uses the same selected checksum algorithm and width but has one trailing representation-plus-semantics checksum and no CRC. Its trailing checksum covers exactly this concatenated byte sequence, with no omitted field, no extra terminator, and no length prefix:

<!-- hard-unit kind="formula" -->
```text
ASCII domain bytes "SREPNG2-BLOCK\0"
exact 12-byte DataBlock record frame
the exact encoded DataBlock payload bytes, including the 48-byte common header and every layout-specific FutureRegister, I/O operation, and LiteralRun byte
block identity fields in this order:
  block_id as u64 LE
  dst_start as u64 LE
  uncompressed_len as u64 LE
exact reconstructed uncompressed block bytes in destination order
```
<!-- /hard-unit -->

The 12-byte frame is type `0x03`, flags `0`, reserved `0`, and little-endian `payload_len`. The encoded payload included in this domain is the exact on-wire payload, so a mutation of any FutureRegister field, I/O fragment identity or source metadata, LiteralRun offset/length/bytes, or common DataBlock header field changes the checksum even when reconstructed destination bytes happen to remain identical. The encoder computes this checksum after the encoded payload and reconstructed source bytes are known. The decoder validates the structural frame and payload length first, reconstructs the block, concatenates the domain in the order above, and then compares the trailing checksum. There is no duplicate DataBlock checksum in BlockDirectory or payload, there is no second checksum ID, and there is no CRC. This single checksum protects representation, identity, and reconstructed bytes together; a representation-only mutation MUST NOT preserve a valid archive.

### 10.4 MethodParameters payload: exact 64 bytes

The MethodParameters payload is exactly 64 bytes:

<!-- hard-requirements -->
| Offset | Size | Field and required value |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 1 | Method, equal to ArchiveHeader method |
| 3 | 1 | Flags, bit 0 equal to ArchiveHeader REP bit; bits 1–7 zero |
| 4 | 4 | Reserved, zero |
| 8 | 8 | `rep_distance`; zero without overlay, default 512 MiB with overlay |
| 16 | 8 | `rep_min_match`; zero without overlay, default 512 with overlay |
| 24 | 8 | `rep_region_size`; zero without overlay, otherwise `max(1,floor(rep_min_match/8))` |
| 32 | 8 | `cdc_window`; 48 for m1, 0 otherwise |
| 40 | 8 | `cdc_min_chunk`; header minimum for m1/m2, 0 otherwise |
| 48 | 8 | `order1_table_entries`; 256 for m2, 0 otherwise |
| 56 | 8 | `slice_count`; 8 for m5, 0 otherwise |

The payload length MUST be exactly 64. Every field is checked against the method table in Section 10.1 and the overlay rules in Section 4.3. With overlay enabled, `rep_min_match` MUST be positive and within the Section 10.0 overlay-minimum range; without overlay, it MUST be zero. The checked `effective_min_match` formula in Section 4.3 is evaluated only after these checks and after validating the positive ArchiveHeader minimum. No method-specific parameter may be silently supplied through an unvalidated extra byte.

### 10.5 LayoutMetadata payload: exact 64 bytes

The LayoutMetadata payload is exactly 64 bytes:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 1 | Layout, equal to ArchiveHeader layout |
| 3 | 1 | Flags, exactly zero |
| 4 | 4 | Reserved, zero |
| 8 | 8 | `uncompressed_len` |
| 16 | 8 | `block_count` |
| 24 | 8 | `data_record_count`, exactly `block_count` |
| 32 | 8 | `semantic_match_count` |
| 40 | 8 | `covered_bytes` |
| 48 | 8 | `literal_bytes` |
| 56 | 8 | `encoded_operation_count` |

The payload length MUST be exactly 64. `covered_bytes` is the checked sum of original normalized match lengths. `literal_bytes` MUST equal `uncompressed_len - covered_bytes`. `encoded_operation_count` is zero for Index-LZ, the number of FutureRegister entries for Future-LZ, and the number of I/O operations (literal or match fragment) for I/O-LZ. For Index-LZ, the per-block `operation_count` is zero even though the layout uses IndexSection matches. These counts are representation counts and do not replace semantic match count.

### 10.6 BlockDirectory payload: exact Index-LZ schema

BlockDirectory is present only for Index-LZ. Its payload is exactly:

<!-- hard-unit kind="formula" -->
```text
16-byte header
64 * block_count bytes of entries
```
<!-- /hard-unit -->

The 16-byte header is:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 2 | Entry size `64` |
| 4 | 4 | Reserved, zero |
| 8 | 8 | `block_count`, equal to LayoutMetadata |

Each entry is exactly 64 bytes, with eight little-endian `u64` fields in this order:

<!-- hard-requirements -->
| Entry offset | Field |
|---:|---|
| 0 | `block_id` |
| 8 | `dst_start` |
| 16 | `uncompressed_len` |
| 24 | `data_record_offset`, offset of the DataBlock type byte |
| 32 | `data_record_total_len`, including 12-byte frame, payload, and checksum |
| 40 | `literal_run_count` |
| 48 | `first_starting_match_index` |
| 56 | `starting_match_count` |

The exact payload length is checked as `16 + 64 * block_count`. Block IDs are contiguous from zero. Destination ranges are contiguous, start at zero, and have lengths equal to DataBlock declarations. Data record offsets and total lengths point exactly to DataBlock records in the archive and do not overlap any other record. The starting-match range partitions the IndexSection entries whose `dst` begins in that block. A match that crosses blocks appears once, in the range for the block containing its destination start.

### 10.7 Common DataBlock payload header: exact 48 bytes

Every DataBlock payload begins with exactly this 48-byte header:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 1 | Layout, equal to ArchiveHeader layout |
| 3 | 1 | Flags, exactly zero |
| 4 | 4 | Reserved, zero |
| 8 | 8 | `block_id` |
| 16 | 8 | `dst_start` |
| 24 | 8 | `uncompressed_len` |
| 32 | 8 | `literal_run_count` |
| 40 | 8 | `operation_count` |

DataBlocks are contiguous and increasing. Every non-final DataBlock has exactly the configured block size; the final DataBlock has length in `1..=block_size`. Empty input has zero DataBlocks. The payload length is the checked sum of 48, all layout-specific entries, and all literal-run bytes.

A LiteralRun has this exact encoding and is used by Index-LZ and Future-LZ:

<!-- hard-requirements -->
| Field | Size |
|---|---:|
| `dst_offset` relative to DataBlock start | 8 |
| `len` | 8 |
| literal bytes | exactly `len` |

`len` is positive. Literal runs are ordered by `dst_offset`, non-overlapping, within the block, and contain exactly their declared number of bytes. Literal runs cover every destination byte not filled by a semantic match delivery.

#### Index-LZ DataBlock payload

After the 48-byte common header, the payload contains exactly `literal_run_count` LiteralRuns and no other bytes. `operation_count` MUST be zero. The IndexSection supplies matches, including matches that begin in another block and remain active in this block.

#### Future-LZ DataBlock payload

After the common header, the payload contains exactly `operation_count` fixed-size FutureRegister entries followed by exactly `literal_run_count` LiteralRuns. In Future-LZ, a literal run is owned by the block where its destination interval occurs; a FutureRegister is owned by the block where its source start occurs. The FutureRegister list is therefore not a destination operation stream. Each FutureRegister is exactly 40 bytes:

<!-- hard-requirements -->
| Entry offset | Size | Field |
|---:|---:|---|
| 0 | 8 | `origin_match_id` |
| 8 | 8 | `source_offset`; `source_start = DataBlock.dst_start + source_offset` |
| 16 | 8 | absolute `destination` |
| 24 | 8 | `total_len` |
| 32 | 8 | `period_len` |

The exact Future payload length is `48 + 40 * operation_count + sum(16 + literal_len)` over literal runs. Registers are ordered by `(source_start, destination, origin_match_id)`. A register MUST identify exactly one canonical normalized triple: `source_start=src`, `destination=dst`, `total_len=len`, and `period_len=min(len,dst-src)`. `source_offset` MUST identify a source start inside the attached block. The collector may span later source blocks. The register's destination and delivery range MUST be validated against the canonical origin triple and block ranges.

#### I/O-LZ DataBlock payload

After the common header, the payload contains exactly `operation_count` destination-ordered operations. Each operation begins with an 8-byte header:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | Tag: `0` literal or `1` match fragment |
| 1 | 1 | Flags, exactly zero |
| 2 | 2 | Reserved, exactly zero |
| 4 | 4 | `encoded_len`, total operation bytes including this 8-byte header |

A literal operation has tag `0`, body `len:u64` followed by exactly `len` literal bytes, and therefore `encoded_len = 16 + len`. Its `len` MUST be positive, the checked value `16 + len` MUST fit in `u32` (because `encoded_len` is `u32`), and `literal_run_count` equals the number of tag-0 operations.

A match-fragment operation has tag `1`, body of four little-endian `u64` fields `(origin_match_id, src, dst, len)`, and therefore `encoded_len = 40`. Its fragment `len` is positive and may be shorter than `effective_min_match` because it may be a representation fragment. Every fragment MUST map to the same origin ID and the exact contiguous subrange of its canonical triple. A semantic match crossing a destination block is split only at block boundaries; each fragment carries the same origin ID with adjusted `src`, `dst`, and `len`, and the fragments reassemble the original triple exactly.

The exact I/O payload length is `48 + sum(encoded_len)` over all operations. Operations cover the DataBlock destination interval exactly, with no gaps or overlaps, in ascending destination order. A decoder MUST reject a wrong encoded length, unknown tag, nonzero flags/reserved fields, invalid fragment identity, malformed operation, or operation stream that does not reproduce the declared block.

### 10.8 IndexSection payload: exact Index-LZ schema

IndexSection is present only for Index-LZ. Its payload has a 32-byte header:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 2 | Match entry size `24` |
| 4 | 2 | Range entry size `24` |
| 6 | 2 | Reserved, zero |
| 8 | 8 | `match_count` |
| 16 | 8 | `block_count` |
| 24 | 8 | Reserved, zero |

Then come exactly `match_count` 24-byte match entries, followed by exactly `block_count` 24-byte range entries. Each match entry is three little-endian `u64` fields `(src,dst,len)`. Match entries are in canonical origin ID order, ascending `(dst, src, len descending)` from the normalizer after duplicate triples are removed; their origin IDs are their zero-based entry indices. Each range entry is three little-endian `u64` fields `(block_id, first_starting_match_index, starting_match_count)`.

The exact payload length is checked as:

<!-- hard-unit kind="formula" -->
```text
32 + 24 * (match_count + block_count)
```
<!-- /hard-unit -->

with checked multiplication and addition. The ranges are contiguous by block ID and partition the match entries whose destination starts in each block. Every match has `src < dst`, `len >= effective_min_match`, checked `dst+len`, valid source history, and a destination interval inside the total input. Cross-block matches appear once. IndexSection count/range values MUST equal BlockDirectory and LayoutMetadata values.

### 10.9 ArchiveSummary payload: exact schema

ArchiveSummary is record type `0x06`. Its payload has a fixed 72-byte prefix followed by exactly `digest_len` bytes:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 2 | Schema `1` |
| 2 | 1 | Checksum ID, equal to ArchiveHeader |
| 3 | 1 | `digest_len`: `16` for XXH3-128 or `32` for BLAKE3-256 |
| 4 | 4 | Reserved, zero |
| 8 | 8 | `uncompressed_len` |
| 16 | 8 | `block_count` |
| 24 | 8 | `semantic_match_count` |
| 32 | 8 | `covered_bytes` |
| 40 | 8 | `literal_bytes` |
| 48 | 8 | `total_data_record_bytes` |
| 56 | 8 | `index_section_total_record_bytes` (`0` when not Index-LZ) |
| 64 | 8 | `total_record_count` |
| 72 | `digest_len` | full semantic archive digest |

The exact payload length is 88 bytes for XXH3-128 and 104 bytes for BLAKE3-256. `total_data_record_bytes` is the checked sum of the total serialized lengths of every DataBlock record, from its type byte through its trailing checksum, with each length equal to `12 + payload_len + checksum_width`; it excludes ArchiveHeader, all other records, and Trailer. It is zero for an empty archive. The decoder recomputes it from actual DataBlock record boundaries. Index-LZ requires the BlockDirectory sum of `data_record_total_len` to equal it; Future-LZ and I/O-LZ sum the observed DataBlock record lengths and compare. `total_record_count` counts every framed record including the ArchiveSummary itself and excluding ArchiveHeader and Trailer. Define `I=1` for Index-LZ and `I=0` for Future-LZ or I/O-LZ. It therefore equals `2 + I + block_count + I + 1`, or `block_count + 3 + 2I`.

The full semantic digest is computed over exactly:

<!-- hard-unit kind="formula" -->
```text
ASCII bytes "SREPNG2-ARCHIVE\0"
exact 80 ArchiveHeader bytes
exact 64 MethodParameters payload bytes
exact 64 LayoutMetadata payload bytes
all reconstructed uncompressed bytes in order
```
<!-- /hard-unit -->

It is serialized using the selected algorithm's archive representation. For XXH3-128 this is low64 little-endian followed by high64 little-endian. For BLAKE3-256 this is the 32 raw digest bytes in digest order. The Summary record itself also has its ordinary record checksum. The decoder MUST verify structural record checksums, reconstruct and verify each DataBlock representation-plus-semantics checksum, recompute all counters, recompute this semantic digest, and compare the Summary before reporting success. A FutureRegister or I/O source-metadata mutation that leaves reconstructed bytes unchanged still fails the DataBlock checksum because that checksum includes the exact encoded payload. A summary digest mismatch is `ChecksumMismatch` (code 11); a summary counter mismatch is `CorruptRecord` (code 8), unless the mismatch is an index-derived count/range inconsistency, which is `CorruptIndex` (code 9). The digest detects corruption but is not authentication against an attacker who recomputes it.

### 10.10 Checksum IDs and implementation research decision

V2 checksum ID `1` is XXH3-128 using `twox-hash` version 2.1.4 with `default-features=false` and features `std` and `xxhash3_128`. It uses the fixed official default secret and seed zero. Its serialized bytes are exactly low64 LE then high64 LE. V2 checksum ID `2` is BLAKE3-256. There is no v2 checksum-none mode and no second independently selectable checksum ID. The CLI selector is exactly `--checksum=xxh3|blake3`, defaults to `xxh3`, and rejects every other checksum name; the library exposes the corresponding checksum enum.

`twox-hash` was selected over `xxhash-rust` for maintainability and safety governance, not because `xxhash-rust` is considered incorrect. Both are mature, current, and correct. `twox-hash` is older and more downloaded and provides an explicit MSRV. The implementation audit MUST document unsafe blocks, cross-platform output behavior, and property comparisons with official C on Linux, Windows, and macOS. Miri runs MUST cover 32-bit and big-endian targets where available. `xxhash-rust` is smaller, closer to C, has more reverse dependencies and direct C comparisons, but has an unresolved internal unsafe-semantics governance concern and a compile-time SIMD focus.

The project MUST maintain its own golden vectors generated from official C. Vectors MUST cover XXH3 oneshot and streaming APIs, equivalent chunkings, boundary lengths, 32-bit and 64-bit targets, endian byte representation, and low/high 64-bit serialization. BLAKE3 vectors MUST cover its exact v2 serialization. RapidHash MAY be used as an internal replaceable CandidateIndex hash only; it MUST NOT be persisted or used as match confirmation.

### 10.11 Fixed Trailer: exact 64 bytes

The Trailer is not a record and has no additional checksum layer. It is exactly 64 bytes:

<!-- hard-requirements -->
| Offset from trailer start | Size | Field |
|---:|---:|---|
| 0 | 8 | Trailer magic ASCII `SREPNGT2` |
| 8 | 1 | Trailer version `2` |
| 9 | 1 | Trailer flags, exactly zero |
| 10 | 2 | Reserved, zero |
| 12 | 8 | ArchiveSummary record offset, at its type byte |
| 20 | 8 | ArchiveSummary record total length |
| 28 | 8 | IndexSection record offset, zero unless Index-LZ |
| 36 | 8 | IndexSection record total length, zero unless Index-LZ |
| 44 | 8 | Total archive length including Trailer |
| 52 | 8 | `BodyEnd` offset, immediately after last DataBlock and before IndexSection or Summary |
| 60 | 4 | Reserved, zero |

All offsets point to record type bytes and all record lengths include 12-byte framing, payload, and selected checksum. For non-Index-LZ layouts, both IndexSection fields are zero. For Index-LZ, both are nonzero and point to the single IndexSection record. `BodyEnd` is the byte immediately after the final DataBlock in a nonempty archive. For an empty Index-LZ archive it is the byte immediately after the empty BlockDirectory; for an empty Future-LZ or I/O-LZ archive it is the byte immediately after LayoutMetadata. The reader MUST verify all trailer fields against actual file length, record boundaries, record order, cross-record counts, and selected checksums. Missing trailer, malformed magic/version/flags, overflow, invalid range, inconsistent zero/nonzero index fields, total-length mismatch, or any trailing byte is rejected.

## 11. Legacy v1-v4 reader

The legacy reader is independent of the v2 writer and MUST be a strict little-endian decoder. It MUST parse the exact byte schema below, use checked arithmetic and allocation limits for every untrusted field, and accept only versions 1 through 4. It MUST never warn and continue after malformed metadata or a checksum failure.

### 11.1 Legacy archive header: exact bytes

The legacy archive header is exactly four little-endian `u32` words followed immediately by exactly `seed_len` raw seed bytes:

<!-- hard-requirements -->
| Word offset | Size | Field |
|---:|---:|---|
| 0 | 4 | Signature word `0x26351817` |
| 4 | 4 | Signature word `0x50455253` |
| 8 | 4 | Packed metadata |
| 12 | 4 | `BASE_LEN` |
| 16 | `seed_len` | Raw seed bytes |

The packed metadata is decoded from its four bytes in low-to-high order:

<!-- hard-unit kind="formula" -->
```text
version       = packed & 0xff
hash_id       = (packed >> 8) & 0xff
seed_len      = (packed >> 16) & 0xff
encoded_bias  = (packed >> 24) & 0xff
digest_len    = (encoded_bias + 16) & 0xff
```
<!-- /hard-unit -->

The addition is modulo 256 exactly as shown. In particular, `encoded_bias=0xf8` yields `digest_len=8`, the legacy SipHash width. `seed_len` bytes follow the 16-byte header without padding. BASE_LEN policy is version-specific: v1 and v2 MUST have `BASE_LEN>0`, including an empty or no-match archive under this strict practical policy; v3 and v4 MAY have `BASE_LEN=0`, as valid Future-LZ and embedded Index-LZ archives may use one-byte physical source fragments. For each v3/v4 physical record, the checked expression `len_minus_base + BASE_LEN` MUST be greater than zero. All header values MUST satisfy the checksum table and the selected version's layout rules. A signature mismatch maps to `CorruptHeader`; a version outside 1..4 maps to `UnsupportedVersion`; an invalid checksum combination maps to `UnknownChecksum`. Existing real v4 fixtures with `BASE_LEN=0` are valid evidence and MUST be retained.

### 11.2 Exact legacy checksum metadata

Legacy checksum IDs and their exact header-derived metadata are:

<!-- hard-requirements -->
| ID | Algorithm | Required encoded bias | Seed length | Digest/field width | Required interpretation |
|---:|---|---:|---:|---:|---|
| 0 | MD5 | `0x00` | 0 | 16 | Compare canonical 16 raw MD5 digest bytes |
| 1 | none | `0x00` | 0 | 16 | Ignore the 16-byte stored block field as opaque legacy data; perform no checksum comparison |
| 2 | SHA-1 | `0x04` | 0 | 20 | Compare canonical 20 raw SHA-1 digest bytes |
| 3 | SHA-512 | `0x30` | 0 | 64 | Compare canonical 64 raw SHA-512 digest bytes |
| 4 | SREP-VHASH-128 | `0x00` | 32 | 16 | Exact hash-only VHASH output defined in Section 11.2, serialized as two legacy little-endian `u64` words in emitted order |
| 5 | SipHash-2-4 | `0xf8` | 16 | 8 | Compare one SipHash output encoded as one little-endian `u64`; the bias formula yields width 8 |

The `hash_id`, `encoded_bias`, `seed_len`, and computed `digest_len` MUST exactly match one row. Any other ID or combination is rejected. The legacy ID 1 no-checksum field is the only accepted absence of verification; v2 never permits checksum-none. The checksum bytes occur in every legacy block immediately after its 12-byte block header and before inline statistics/literal bytes. The raw header seed bytes are passed to the selected seeded checksum exactly as stored, without transformation or padding. Checksums are verified only after the block has been reconstructed. The checksum domain is exactly the reconstructed uncompressed block bytes, with no header, seed, statistics, or framing bytes included.

ID 4 is exact `SREP-VHASH-128`, not generic nonce-based VMAC. Its 32 seed bytes are exactly an AES-256 key `K`; AES block encryption is FIPS-197. It is hash-only VHASH with no nonce or pad addition, tag length 128, `FAVOR-ENDIAN=LITTLE`, and `L1KEYLEN/VMAC_NHBYTES=32768` bits / 4096 bytes. The algorithm is based on Section 5 of the Krovetz/Dai draft and its public-domain 17-Apr-2008 reference implementation:

<!-- hard-unit kind="list" -->
- draft: <https://www.fastcrypto.org/vmac/draft-krovetz-vmac-01.txt>
- reference C: <https://www.fastcrypto.org/vmac/vmac.c>
- reference header: <https://www.fastcrypto.org/vmac/vmac.h>
<!-- /hard-unit -->

The detailed draft algorithm, the fixed parameters below, and the vectors below are normative. The upstream vmac/vhash implementation is public domain, but the SREP wrapper and its custom parameterization MUST be independently implemented or audited; no all-rights-reserved SREP wrapper code may be copied.

#### 11.2.1 SREP-VHASH-128 normative pseudocode

All arithmetic in this pseudocode is on explicitly sized unsigned values. `u64` and `u128` additions and products wrap modulo `2^64` and `2^128` only where stated; `p127=2^127-1` and `p64=2^64-257` reductions are mathematical modular reductions. The implementation MUST use a mathematically equivalent representation and MUST NOT rely on limb layout or host endianness.

<!-- hard-unit kind="list" -->
1. **KDF.** Construct a 16-byte AES input block that is initially all zeros except byte 0, which is the KDF index (`0x80` for NH keys, `0xC0` for polynomial keys, `0xE0` for L3 keys). After every AES-256 encryption, including rejected L3 trials, increment **only byte 15 modulo 256**. Bytes 1 through 14 remain zero for every block. There is **no carry** into earlier bytes, so AES block 256 of a family reuses the same counter as AES block 0 of that family. Concatenate the AES-256 outputs. This matches the public-domain Krovetz/Dai `vmac_set_key` loop (`((unsigned char *)in)[15] += 1`) and is **not** a 120-bit big-endian integer counter.
2. **NH keys.** Generate 257 AES-256 blocks (4,112 bytes) with KDF index `0x80`. For output `j=0`, use bytes 0 through 4,095; for output `j=1`, use bytes 16 through 4,111. Parse each 8-byte AES-output slice as a **big-endian** `u64` using the reference `get64BE` conversion. These are the overlapping iter-0 and iter-1 NH keys. The NH key array therefore contains 514 big-endian `u64` words; words 512 and 513 are the wrap-around AES block whose counter equals the first NH AES block.
3. **Polynomial keys.** Zero the 16-byte AES input block, set byte 0 to `0xC0`, and generate AES blocks with the same byte-15 modulo-256 increment. Parse four consecutive **big-endian** `u64` words with `get64BE` and mask each with `mpoly=0x1fffffff1fffffff`. Stream 0 uses words 0 and 1; stream 1 uses words 2 and 3.
4. **L3 keys.** Zero the 16-byte AES input block, set byte 0 to `0xE0`, and generate AES blocks with the same byte-15 modulo-256 increment. Parse each block as two **big-endian** `u64` words with `get64BE`. Accept a pair only if both words are `< p64`; stream 0 receives the first accepted pair and stream 1 receives the next. Advance byte 15 after every trial, including rejected trials.
5. **NH.** For each complete 4,096-byte message segment and the final tail, zero-pad only the final partial segment to the next 16-byte boundary. Parse **message** words as little-endian `u64` (`get64PE` with `VMAC_PREFER_BIG_ENDIAN=0`). NH **keys** remain the big-endian KDF words from step 2. For stream `j`, sum modulo `2^128`:

   ```text
    nh_j = sum over i of
           (m[2i] + k_j[2i] mod 2^64) *
           (m[2i+1] + k_j[2i+1] mod 2^64)
    ```

   Then clear the top two bits, which is reduction modulo `2^126`. The second stream uses the NH key shifted by 16 bytes, exactly as the reference's iter-1 computation.
6. **L2.** For each stream, initialize the accumulator with its masked polynomial key when the message has no NH output. For a nonempty message, add the first NH result to that key as the initial accumulator, then apply one polynomial step per subsequent NH result:

   ```text
    acc = (acc * poly_key + nh) mod p127
    ```

   The 128-bit accumulator is an integer represented by its exact mathematical value; it is not a host-endian struct. This first-result handling and empty-message key handling are required reference behavior.
7. **Length and L3.** Let `remainder = message_len mod 4096` and `remainder_bits = remainder * 8`. Empty input has `remainder=0` and `remainder_bits=0`. A full-block message whose length is a positive multiple of 4096, including length 4096, also has `remainder=0` and `remainder_bits=0`; no partial NH tail is processed and the L3 length contribution is zero. A partial tail of `remainder` bytes is zero-padded only to the next 16-byte boundary before NH; the L3 length contribution remains `remainder_bits`, not the padded length. Add `remainder_bits << 64` to each L2 value modulo `p127`, then apply the draft L3-HASH with that stream's accepted `p64` key pair. L3 converts the 128-bit value `x` into `m1=floor(x/(2^64-2^32))` and `m2=x mod (2^64-2^32)`, and returns `((m1+k1)*(m2+k2)) mod p64`. The high-level words are `res=L3(stream0)` and `tagl=L3(stream1)`.
8. **Output.** Reset all VHASH state for each legacy block checksum. Serialize exactly `res.to_le_bytes()` followed by `tagl.to_le_bytes()`. This is the two-word little-endian output emitted by the legacy implementation: the first eight bytes are `res` in little-endian order and the second eight bytes are `tagl` in little-endian order.
<!-- /hard-unit -->

Empty-input behavior is exact: no NH output is produced, each L2 accumulator is the masked polynomial key, `remainder_bits=0`, and L3 is applied to those keys. Full-block `remainder=0` behavior is exact: every complete 4096-byte NH result is consumed, no tail NH is performed, and L3 receives `remainder_bits=0`. Partial-tail zero padding never contributes extra remainder bits. Incremental `vhash_update` calls MUST pass only positive multiples of 4096 bytes; the final remainder, including empty remainder after a full-block prefix, is passed to the final `vhash`.

The zero-padded message buffer MUST be at least 16-byte aligned where required by the public reference and MUST contain zero bytes through the next 16-byte boundary. One-shot `vhash` and incremental `vhash_update` plus final `vhash` MUST produce identical 16-byte output for every vector in Section 11.2.2.

This custom SREP-VHASH-128 differs from standard default 128-byte-NH VHASH and from nonce-based VMAC-128. ID 4 MUST be independently implemented safely or selected from an auditable crate configured exactly to these parameters. Public-domain reference differential tests and committed vectors MUST cover empty input, lengths 1, 15, 16, 17, 4095, 4096, 4097, real block lengths, at least three distinct keys, and the existing archive seed. The old binary/reference generates the vector bytes; these vectors and the fixed algorithm are the authority for legacy compatibility. The old decoder's behavior of warning for unknown hash IDs is not compatible with this reader; this reader rejects unknown IDs.

#### 11.2.2 Normative generated vectors

The following vectors were independently generated with the public-domain `vmac.c`/`vmac.h` reference, configured with `VMAC_TAG_LEN=128`, `VMAC_KEY_LEN=256`, `VMAC_NHBYTES=4096`, `VMAC_USE_OPENSSL=1`, and hash-only `vhash`. OpenSSL 1.1.1f supplied AES-256. `K0` is bytes `00` through `1f`; `K1` is 32 zero bytes; `K2` is bytes `ff` through `e0` descending. For each row, `M_n[i]=(i*131+17) mod 256`. The digest column is lowercase `res_le || tagl_le`. These exact bytes are normative:

<!-- hard-requirements -->
| Key | n | Digest `res_le || tagl_le` |
|---|---:|---|
| K0 | 0 | `2eca48aefe4117ae20f2769dbdfe8de3` |
| K0 | 1 | `aebfc5095e65fdc1a95b509b186e53bd` |
| K0 | 15 | `2d5a98646d8f36eb1ae9aa4196fd762a` |
| K0 | 16 | `b73b99cadcaf7b1da7a5af7434e74a58` |
| K0 | 17 | `7aec06e5465bb76cbce2c23334eb11d9` |
| K0 | 4095 | `4b85e8153acc14a3c435b708fa88a6a6` |
| K0 | 4096 | `5d9a25548b5a82a6e84e87c5704ae401` |
| K0 | 4097 | `82396c89fd1e58762c8c9b8d4502b5bb` |
| K1 | 0 | `4827d443f5773a0b23ac4c6be4b03f67` |
| K1 | 1 | `dd70f6c48a19e66074e622ea73f460e8` |
| K1 | 15 | `f1b1821ab671d8ab0b4b29dca6f918b3` |
| K1 | 16 | `4b7aac6d5111d382f1a7fd8210b4ac02` |
| K1 | 17 | `06e3cd6366ff58f53c12c67c8253a0c2` |
| K1 | 4095 | `67ee988e726d950013d4bb8dd96255af` |
| K1 | 4096 | `640817c8a472fda17359f45b1a95f8a4` |
| K1 | 4097 | `21077486253a269eb04176c17ab300e3` |
| K2 | 0 | `683e9378e4024ee7239278b91ae6bc05` |
| K2 | 1 | `7a631b51a6ba3cd66a9ef2118c067adb` |
| K2 | 15 | `4ddfc05d5e5148fb56ef11481651b9c3` |
| K2 | 16 | `823806e710a389c6fd1cd6644eda8f98` |
| K2 | 17 | `695525c8e43f5ab010404218c7a5515c` |
| K2 | 4095 | `320aa79597a5151f932eacc97dffd548` |
| K2 | 4096 | `89bf1ed93c224d93ec00f2409c76d734` |
| K2 | 4097 | `82dc88ce641347fa995a052da1770817` |

The harness was compiled and run after verifying parent `/tmp/opencode` and creating `/tmp/opencode/srep-vhash-harness`. Public-domain `vmac.c` and `vmac.h` were fetched from the URLs above; the temporary header copy was configured so `VMAC_TAG_LEN`, `VMAC_KEY_LEN`, `VMAC_NHBYTES`, `VMAC_USE_OPENSSL`, and `VMAC_HASH_ONLY` honoured the compile definitions. OpenSSL 1.1.1f supplied AES-256. The compile command was:

```text
gcc -std=gnu11 -O2 -Wall -Wextra \
  -DVMAC_TAG_LEN=128 -DVMAC_KEY_LEN=256 -DVMAC_NHBYTES=4096 \
  -DVMAC_USE_OPENSSL=1 -DVMAC_HASH_ONLY=1 \
  -I/tmp/opencode/srep-vhash-harness \
  /tmp/opencode/srep-vhash-harness/vmac.c \
  /tmp/opencode/srep-vhash-harness/vectors.c \
  -lcrypto -o /tmp/opencode/srep-vhash-harness/vectors
```

The harness allocated 16-byte-aligned buffers with zero padding, ran one-shot `vhash`, and ran incremental `vhash_update` splits using chunk requests `[1,15,16,4095,4096]`. All 24 one-shot/incremental pairs were byte-identical, including the empty-input rows and the 4096/4097-byte rows.

The historical binary cross-check used the migrated old encoder `/home/test/.opencode/archiving-tools/srep/bin/srep` (SREP 3.93a beta) with default VHASH/VMAC and without `-hash-`. The deterministic original was 256 bytes `M_256[i]=(i*131+17) mod 256`, SHA-256 `118b38101a6d90a15bec907a04acac42e0f213be5dd3f0731967e0349a3d2b27`. The command was:

```text
/home/test/.opencode/archiving-tools/srep/bin/srep -v0 \
  /tmp/opencode/srep-vhash-harness/hist/original.bin \
  /tmp/opencode/srep-vhash-harness/hist/original.bin.srep
```

Parsed archive values were:

<!-- hard-unit kind="table" -->
| Field | Exact value |
|---|---|
| archive SHA-256 | `44a1ade734b038104240df30280ca5b1320291d5dc70555093cb82521d1732c4` |
| archive length | `360` |
| packed header | `0x00200404` |
| parsed version/hash/seed/BASE_LEN | `4/4/32/0` |
| 32-byte seed `K` | `0f5acc75c23306230270e4f1e2ac3525f4d18b32bba18f76686b7e9555dd275e` |
| block uncompressed length | `256` |
| stored ID4 digest | `623842bf997fd783fe3abcdbab4305a7` |
<!-- /hard-unit -->

The independent public-domain harness reproduced that stored digest exactly as `res_le || tagl_le` for both one-shot and incremental hashing of the original 256 bytes. The checksum domain is the reconstructed uncompressed block; the 12-byte legacy block header is not hashed.

Long historical archives that exercise at least one full 4096-byte NH block were generated with the same encoder, default VHASH/VMAC, `-m3 -b8mb -v0`, and no `-hash-`. Each input is `M_n[i]=(i*131+17) mod 256`. Parsed fields and independent-harness reproduction:

<!-- hard-unit kind="table" -->
| n | packed | seed `K` | stored ID4 digest | orig SHA-256 | harness oneshot/incremental |
|---:|---|---|---|---|---|
| 4095 | `0x00200404` | `4fb32443d528a88263036a03a329e83fb60c8cad72b5f823126713e5884deb38` | `cf5707a6fca43381caeb2161e2c5045c` | `d8d5daa4611f92aff29d032c9876a120ce7a4a7d19e2c4986b8c3c1af6e85c1a` | exact match |
| 4096 | `0x00200404` | `aefc733df24360ef00b7403ab141eaae3552884c485f444d898be4ff54181966` | `5c4e6d9ce4345c76f97fba98c6fb330f` | `c741eee93580a334bae702208e5c0595d8e32eb07d562c52d56ca5f86e78f8a8` | exact match |
| 4097 | `0x00200404` | `57260813db7992c42bf0bb3ddb6b755cc5fc54555c45bed688fd8c80216fe62b` | `d8a5b44516b723ed332df24ac5e13949` | `6b828a41dabc57049f86a8a1b0ceac3e4112258b236a527395c8116c99424ffd` | exact match |
| 8192 | `0x00200404` | `08fb1cda79bbe7804dfcbb67516b0dc93c9f407a8f979a46ef826954f8889159` | `f0494a0189b8f8fb8dee16434837f6c9` | `0dd21bd42470a439e676fa1e6e4706bb7909712910d854e229671a51a26a3941` | exact match |
<!-- /hard-unit -->

These lengths distinguish KDF variants because NH-key setup performs 257 AES-256 blocks. Verified on K0 and on the n=4096/n=8192 archive seeds: byte-15 modulo-256 increment without carry reuses AES-input `80 00..00` at i=0 and i=256, while a 120-bit carrying counter would use `80 .. 00 01 00` at i=256 and a different ciphertext. For the n=4096 seed, no-carry AES-256 block 256 is BE words `1ced9c8098cbabae 5858782ca3e1339f`; the carrying counter yields different BE words `b5f059ef397cb213 ...`; little-endian parsing of the no-carry ciphertext also differs (`aeabcb98809ced1c ...`). The historical stored digest equals the no-carry/`get64BE` public-domain `vhash`, not a carry or little-endian-NH-key variant. The 24 vectors above were produced by that same historical algorithm and therefore already agree with the corrected KDF/endian rules.

These vectors, the empty-input and full-block remainder rules, the byte-15 modulo-256 KDF, big-endian NH-key parsing, the output word order, the historical archive tuples, and the independent generation evidence are normative; they do not claim the target implementation already exists. No stale external FreeArc fixture is used.

### 11.3 Legacy block frame and common validation

Every legacy block has exactly this fixed 12-byte little-endian header, followed by the computed `digest_len` checksum bytes, then its block data:

<!-- hard-requirements -->
| Offset | Size | Field |
|---:|---:|---|
| 0 | 4 | `literal_bytes` |
| 4 | 4 | `uncompressed_len` |
| 8 | 4 | `inline_stat_bytes` |
| 12 | `digest_len` | Raw checksum/opaque field |
| `12 + digest_len` | `inline_stat_bytes` | Fixed-width match statistics |
| after statistics | `literal_bytes` | Literal byte stream |

The legacy stat representation is fixed-width little-endian `u32`; it has no varints. The exact block payload length is `digest_len + inline_stat_bytes + literal_bytes` after the 12-byte header. `literal_bytes`, `uncompressed_len`, and `inline_stat_bytes` MUST fit the configured resource limits. The decoder MUST validate the block's reconstructed length, literal consumption, statistic consumption, match bounds, output bounds, checksum, and checked total before accepting it.

The literal stream is consumed by the layout algorithm, not by an implicit token parser. A literal run is copied from the next bytes of this stream at the operation point that requires it. Any remaining literal bytes are copied only at the final literal operation. Exact consumption is required. For v1/v2 I/O-LZ, invalid literal consumption maps to `CorruptRecord`; for v3 Future-LZ, invalid literal consumption maps to `CorruptRecord`; for v4 Index-LZ, invalid literal consumption or index-linked coverage maps to `CorruptIndex`.

### 11.4 Version 1 rounded I/O-LZ records

Version 1 uses only rounded I/O-LZ records and historically corresponds to the fixed-digest m3 model without REP overlay. `BASE_LEN=L` MUST be positive. Under the strict practical policy in Section 11.1, this remains required even for empty or no-match v1 archives. `inline_stat_bytes` MUST be divisible by the 12-byte record width. The inline statistics are exactly repeated records, and their count is `inline_stat_bytes / 12`:

<!-- hard-unit kind="formula" -->
```text
(literal_len, distance_units, len_units_minus1)
```
<!-- /hard-unit -->

Each field is one little-endian `u32`. Let `basic` be the current output cursor before the record, initially the block destination start. For one record, perform the old operational loop exactly:

<!-- hard-unit kind="list" -->
1. Copy exactly `literal_len` bytes from the literal stream to output at `basic`; checked `actual_dst = basic + literal_len`.
2. Round only the source address, not the write address:
   `rounded_dst = floor(actual_dst / L) * L`.
3. Compute `src = rounded_dst - distance_units * L`, checked.
4. Compute `len = (len_units_minus1 + 1) * L`, checked.
5. Copy `len` bytes to output beginning at the actual destination `actual_dst`, one byte at a time using overlapping LZ semantics.
6. Set `basic = actual_dst + len`.
<!-- /hard-unit -->

Thus the match writes at `actual_dst=basic+literal_len`, while only its source is based on `floor(actual_dst/L)*L`; it does not write at `rounded_dst`. `distance_units * L` and `(len_units_minus1+1)*L` MUST be checked. The source MUST be before the actual destination, available under the decoder's LZ overlap rule, and the match end MUST fit the declared block/output. Literal copying and match writing MUST not cause the rounded source computation to move before available output or create an inconsistent cursor. A record that would do so is corrupt. After all 3-stat records have been processed, copy every remaining literal byte from the block literal stream at the current cursor to reach the declared uncompressed length. Only after this final literal copy MUST the decoder require that the final cursor equals the declared destination end and that literal consumption equals exactly `literal_bytes`; an immediate pre-final-copy cursor equality is not valid.

### 11.5 Version 2 unrounded I/O-LZ records

Version 2 uses unrounded I/O-LZ records. `BASE_LEN` MUST be positive under the strict practical policy in Section 11.1, including an empty or no-match v2 archive. `inline_stat_bytes` MUST be divisible by 16. The record count is `inline_stat_bytes / 16`. Each record is exactly four little-endian `u32` values:

<!-- hard-unit kind="formula" -->
```text
(literal_len, distance_lo, distance_hi, len_minus_base)
```
<!-- /hard-unit -->

Decode `distance = u64(distance_lo) | (u64(distance_hi) << 32)`. For each record, copy `literal_len` literal bytes at the current output cursor, then set `actual_dst = cursor + literal_len`, `src = actual_dst - distance`, and `len = u64(len_minus_base) + BASE_LEN`. Require `distance>0`, `src<actual_dst`, checked source/endpoints, and `len>0`. Copy the match at `actual_dst` byte by byte with overlap, then set the cursor to `actual_dst+len`. After all records, copy the remaining literal stream bytes; the final cursor MUST equal the declared uncompressed length and literal consumption MUST equal `literal_bytes`.

### 11.6 Version 3 Future-LZ and version 4 Index-LZ source records

Version 3 uses four-stat Future-LZ records inline. Version 4 uses the same four-stat source-record encoding inside its embedded IndexSection; version 4's archive layout remains Index-LZ, and these records are not DataBlock operations. In v3, the record count for a block is `inline_stat_bytes / 16`; in v4, the associated footer size gives the same count as `stat_size / 16`. Each record has four little-endian `u32` values:

<!-- hard-unit kind="formula" -->
```text
(source_gap, distance_lo, distance_hi, len_minus_base)
```
<!-- /hard-unit -->

Decode `distance = u64(distance_lo) | (u64(distance_hi) << 32)`. For the records associated with a source block, initialize `source_cursor` to that block's absolute source start. For each record:

<!-- hard-unit kind="list" -->
1. `src = source_cursor + source_gap`, checked;
2. `dst = src + distance`, checked;
3. `len = u64(len_minus_base) + BASE_LEN`, checked;
4. require `dst>src`, require `block_start <= src < block_end`, require `len <= block_end-src` so the entire physical source fragment is contained in its associated source block, require the source period span `[src,src+min(len,distance))` to remain within that source block, and require all source/destination spans within archive/output limits and source visibility under Future-LZ semantics;
5. register the independently delivered physical source fragment for `(src,dst,len)`; and
6. set `source_cursor=src`.
<!-- /hard-unit -->

Version 3 stores these records inline before the block's literal stream. Version 4 stores no inline statistics (`inline_stat_bytes=0`); its future records are in the embedded IndexSection region described in Section 11.7. Legacy records do not carry origin IDs. Each v3/v4 stat record is an independent physical source fragment wholly contained in its associated source block: `block_start <= src < block_end` and `len <= block_end-src`. Source positions in each group MUST be nondecreasing. The source-gap records MUST satisfy the exact old source cursor equations; `source_gap=0` is valid, so consecutive records MAY share a source position, and a committed golden MUST include that same-source-position case. The old encoder commonly produced successive clipped records with the same distance for a semantic match whose physical source spans crossed source blocks, but the legacy wire has no origin IDs. The decoder MUST NOT infer or require any relationship between records. For each physical fragment define `d=distance` and `p=min(len,d)`. Its collector uses only source bytes within the associated source block and never crosses that block because the fragment is wholly contained there. The decoder independently delivers `len` bytes by cycling that fragment period at `dst`, preserving overlap. Literal bytes fill every target hole, including holes in target blocks crossed by a delivery. For both v3 and v4, reconstruction MUST end with no pending physical-fragment delivery whose own source, destination, or fragment state is invalid; a valid clean EOF or valid v4 footer boundary has no unresolved pending delivery.

The Future decoder MUST retain exact active delivery metadata and period bytes in RAM or a checked temporary spill, and MUST deliver pending physical-fragment records in destination order while reconstructing. A physical source fragment never crosses its associated source block, but its target delivery MAY cross target block boundaries; same-block source/destination delivery is also valid. Resource or temp failure is explicit and never causes a shortened dictionary. The decoded output is checked against each block checksum and the full output limits.

### 11.7 Version 4 body, index, and footer: exact bytes

Version 4 body block headers have the same 12-byte `(literal_bytes, uncompressed_len, inline_stat_bytes)` shape and checksum placement as Section 11.3, but every body block MUST have `inline_stat_bytes=0`. The body consists of exactly `block_count` body block records, each header, digest field, and literal bytes, ending at `index_start`.

The final 24 bytes of a v4 archive are six little-endian `u32` words:

<!-- hard-requirements -->
| Word offset from footer start | Field |
|---:|---|
| 0 | `stat_total_lo` |
| 4 | `stat_total_hi` |
| 8 | `footer_size` |
| 12 | `footer_version`, exactly `1` |
| 16 | `~SREP_SIGNATURE`, exactly `0xAFBAADAC` |
| 20 | `~BULAT`, exactly `0xD9CAE7E8` |

Decode `stat_total = u64(stat_total_lo) | (u64(stat_total_hi) << 32)`. Set and validate these exact physical ranges, in this order:

<!-- hard-unit kind="formula" -->
```text
footer_start      = file_len - 24
size_array_bytes  = footer_size - 24
sizes_start       = file_len - footer_size
index_start       = sizes_start - stat_total
body              = [full_header_end, index_start)
stats             = [index_start, sizes_start)
size_array        = [sizes_start, footer_start)
footer            = [footer_start, file_len)
```
<!-- /hard-unit -->

The decoder MUST first require `footer_size >= 24`, checked non-underflowing subtraction, `footer_start >= full_header_end`, `sizes_start >= full_header_end`, `index_start >= full_header_end`, `footer_start <= file_len`, and `size_array_bytes % 4 == 0`. It MUST then parse the footer, parse the size array, parse stats, and parse the body. The size array contains exactly `size_array_bytes / 4` little-endian `u32` values; this count is `block_count`. Each size MUST be divisible by 16, the checked sum of all sizes MUST equal `stat_total`, and the stats range contains exactly the concatenation of those per-block groups. The body range ends exactly at `index_start`; the stats range begins exactly at `index_start`, and no stats byte is part of the body. The decoder MUST reject any reversed or overlapping range, arithmetic underflow/overflow, range outside the file, block-count mismatch, stat-size mismatch, checksum mismatch, a parser-established required byte missing at EOF, or extra byte.

Once the complete 16-byte legacy fixed header and its descriptor-declared seed bytes have been read and validated as version 4, the format itself establishes that a fixed 24-byte footer is mandatory at EOF. Therefore, if the total physical archive length is less than `full_header_end + 24`, the decoder MUST return `TruncatedArchive` (code 7). This fixed-footer minimum does not classify every later structural inconsistency or internal deletion as truncation. After valid footer fields have established the exact size-array, stats, index, and body ranges, physical EOF or a physical range shorter than any declared required range is `TruncatedArchive` (code 7). If the final 24-byte footer area is physically available but its fields or resulting ranges do not validate, the result is `CorruptIndex` (code 9).

The error class for a v4 byte string is determined only by the bytes and the parser state reached while validating them. It MUST NOT depend on the operation, edit history, test-harness knowledge, filename, corpus identity, or any other information outside the byte string and parser state. `TruncatedArchive` is permitted only when the parser has already established from a valid enclosing or fixed structure that exactly `N` bytes are required at a concrete range and physical EOF occurs before those `N` bytes are available. This includes the mandatory fixed-footer minimum after the complete validated v4 header and seed; a footer/size structure that has already been validly identified and whose exact declared range extends beyond EOF; and a body block frame that validly declares digest or literal bytes past its known body boundary or EOF. If the final 24 bytes are physically available at the footer position but the footer version, markers, `stat_total`, `footer_size`, or the resulting range equations do not validate, the result is `CorruptIndex`, not a speculative truncation. Invalid size-array entries, size sums, alignment, ranges, body/index coverage, and other v4 index-structure violations are likewise `CorruptIndex`. A present block digest mismatch is `ChecksumMismatch`; other body structural violations use the existing `CorruptRecord` or `InvalidMatch` mappings.

<!-- hard-unit kind="list" -->
- An internal deletion that shifts later bytes, without an independently valid enclosing declaration proving which byte is missing, is observational structural corruption. It follows the first error actually detected from the resulting bytes: `CorruptIndex`, `CorruptRecord`, `InvalidMatch`, or `ChecksumMismatch` as applicable; it is not automatically `TruncatedArchive` merely because an external tool knows that a byte was deleted.
<!-- /hard-unit -->

<!-- hard-unit kind="formula" -->
```text
W = O(A + B + S)
A = physical archive bytes read or scanned, with each range taking only a constant number of passes
B = decoded block-count entries
S = physical statistic records
```
<!-- /hard-unit -->

<!-- hard-unit kind="list" -->
- Total parser/classifier work MUST satisfy the formula above; RAM and temporary storage MUST remain within the approved budgets. Fixed tail checks of at most a constant number of bytes are permitted.
- A decoder MUST NOT repeatedly reparse archive ranges per candidate, scan every possible deleted-byte position or value, or introduce multiplicative archive-size * field-value or archive-size * block behavior.
- A decoder MUST NOT hardcode fixture lengths or checksum widths, or use filename/corpus knowledge to choose an error class.
<!-- /hard-unit -->

For non-ambiguity, two different edit histories that produce identical final bytes, including one history that deletes a byte and another that mutates bytes in place, MUST produce the same `ErrorKind` and stable error code. The decoder observes only the final bytes and its parser state; it cannot and MUST NOT classify the histories separately.

Each size-array entry associates one sequential stats group with one body block in body order. Each group contains independent physical source fragments in nondecreasing source position. A source-gap cursor begins at that block's source start; `source_gap=0` is valid, so consecutive records MAY share a source position, and the required same-source-position golden in Section 13.1 applies to v4 as well as v3. Every fragment MUST satisfy `block_start <= src < block_end` and `len <= block_end-src`; its source span cannot cross the associated source block. The old encoder commonly produced successive clipped records with the same distance for a semantic match whose physical source spans crossed source blocks, but the legacy wire has no origin IDs. The decoder MUST NOT infer or require any relationship between records. Each physical record is independently validated by its source block, positive length, distance, destination, output bounds, and group ordering; it collects only its own source bytes and delivers only its own fragment with its own `p=min(len,distance)` period. A fragment's delivery may cross target block boundaries and remains pending until its destination bytes are delivered. The v4 IndexSection is the stats range plus size array and footer; no external file is consulted.

### 11.8 Legacy termination and split-index rule

Versions 1 through 3 consist of complete block frames until clean EOF. A partial 1-, 2-, or 3-byte block-header read, an incomplete 12-byte block header, incomplete digest, incomplete statistic, incomplete literal stream, or incomplete pending Future delivery is rejected. An old two-zero-`u32` terminator MAY be accepted only when it is exactly the final 8 bytes, has no digest, payload, or trailing byte after it, and there are no pending impossible matches; a clean EOF without that marker is also accepted after all required blocks and pending state validate. No other terminator is accepted. Version 4 termination is the exact footer/index/body boundary in Section 11.7 and has no optional marker.

The archive-only reader MUST NOT distinguish a split-index archive from a corrupt embedded-index archive by probing a sidecar. Invalid embedded bytes or ranges return `CorruptIndex`. Repeated `--index`/`-index` or mixed repetitions return `InvalidConfiguration` first. Exactly one otherwise syntactically valid explicit legacy sidecar option returns `UnsupportedLegacySplitIndex`, and the CLI rejects that option without opening the sidecar. This limitation is structural: the observed split form and embedded form have identical footer declarations.

### 11.9 Version/layout matrix and legacy acceptance evidence

The exact version/layout mapping is:

<!-- hard-unit kind="table" -->
| Version | Required layout and semantic distinction |
|---:|---|
| 1 | Rounded I/O-LZ; historically m3 with no REP overlay |
| 2 | Unrounded I/O-LZ |
| 3 | Future-LZ |
| 4 | Embedded-index Index-LZ |
<!-- /hard-unit -->

All six checksum IDs in Section 11.2 are valid for each of these four version/layout combinations. The legacy golden manifest MUST list at least one valid archive for every `(version 1..4, checksum ID 0..5)` pair, for 24 minimum rows. It MUST include records exercising v1 rounded source addressing, v2 unrounded addressing, v3 Future-LZ delivery, v4 embedded Index-LZ ranges, v3/v4 `BASE_LEN=0`, cross-source-block physical fragments without inferred linkage, consecutive same-source-position records with `source_gap=0`, exact v4 physical ranges, and v1 trailing literals. Existing real SREP-VHASH-128 v4 samples are retained as required fixtures, including the 112-byte and 1,675-byte samples; their `BASE_LEN=0` and SREP-VHASH-128 header cases are explicitly covered. The corruption matrix MUST mutate every header word, packed-byte field, seed byte, BASE_LEN, block-header word, checksum byte, statistic field, literal boundary, v4 footer word, per-block size, stat total, and every computed range, and MUST record the structured error. For legacy checksum ID 1 (`none`), a mutation confined solely to the opaque 16-byte checksum field MUST be accepted and MUST produce unchanged output. For ID 1, mutations to signature, packed metadata, BASE_LEN, block-header lengths, statistics, or literal framing MUST reject when they make structure, semantics, or lengths invalid. A mutation confined to literal data that leaves valid structure may reconstruct different output with no checksum-based rejection and MUST be recorded as accepted undetectable corruption. A mutation confined solely to the opaque checksum field is always accepted and output remains unchanged. For checksum IDs 0, 2, 3, 4, and 5, checksum, seed, and message mutations MUST reject as `ChecksumMismatch`, except a vector is invalid if its mutation accidentally leaves the digest unchanged; deterministic vectors MUST avoid such coincidences. New output from every accepted legacy archive MUST decode to bytes identical to the old decoder output.

All legacy lengths, positions, match fields, digest sizes, index sizes, output totals, allocation requests, and checksum reads MUST use checked arithmetic and explicit limits. Legacy I/O-LZ to stdout uses the private spool required by Section 9.3.

### 11.10 Preserved capability versus deliberate improvement

The target preserves the semantic capability envelope and core method rules, not accidental misses or performance artifacts of the old implementation:

<!-- hard-unit kind="table" -->
| Preserved semantic capability | Deliberate completeness/maintainability improvement |
|---|---|
| m0 local-max representative and exact backward/forward extension | all collision survivors rather than an old 12-probe hash-chain limit |
| m1 48-byte rolling threshold and trigger/chunk semantics | global weighted normalizer rather than greedy `last_match_end` |
| m2 exact predictor hash and triggering-byte quirk | mandatory byte comparison after digest filtering |
| m3 fixed source grid, digest units, and rounded length rule | deterministic archive-block reset rather than stripe/thread artifacts |
| m4 polynomial fixed-seed key/filter, DataSource reread, byte confirmation, and exact extension | m4 has no digest-based confirmation; its exact reread is the authority |
| m5 original checked L formula, every target start, eight-slice filter, and exact extension | deterministic sorted-run spill without changing candidates |
| CDC archive-block deterministic reset | complete history by default instead of an implicit finite window |
<!-- /hard-unit -->

The m0 and m4/m5 independent backward generation and global normalizer are deliberate completeness improvements over old greedy `last_match_end`; they do not claim byte-identical matches or old compressed bytes. Each improvement MUST preserve or expand the defined target candidate envelope and MUST be covered by directed old-versus-new semantic differential vectors plus the aggregate fidelity gate. Fidelity targets the capability envelope and core semantic model, not old performance artifacts or accidental candidate omissions.

Legacy macro-equation self-audit: for v1, the documented loop copies `literal_len`, computes `actual_dst=basic+literal_len`, computes only the source rounding `floor(actual_dst/L)*L`, writes the match at `actual_dst`, and advances `basic=actual_dst+len`. For v2, the documented loop computes `actual_dst=cursor+literal_len`, `src=actual_dst-distance`, writes at `actual_dst`, and advances by `len`. For v3/v4, every physical record computes `src=source_cursor+source_gap`, `dst=src+distance`, `len=len_minus_base+BASE_LEN`, then sets `source_cursor=src`; the fragment is rejected unless its source span lies wholly in the associated source block. These equations are the direct golden-test oracle for the cited legacy macro behavior.

## 12. Error model

The library MUST return structured errors and MUST never print or terminate the process. The stable error variants, numeric CLI codes, stable ASCII message IDs, and fixed summary templates are:

<!-- hard-requirements -->
| Code | Variant | Message ID | Stable summary |
|---:|---|---|---|
| 2 | `InvalidConfiguration` | `SREP_E_INVALID_CONFIG` | invalid configuration |
| 3 | `UnsupportedVersion` | `SREP_E_UNSUPPORTED_VERSION` | unsupported archive version |
| 4 | `UnsupportedLegacySplitIndex` | `SREP_E_SPLIT_INDEX` | external legacy index is unsupported |
| 5 | `UnknownChecksum` | `SREP_E_UNKNOWN_CHECKSUM` | unknown checksum |
| 6 | `CorruptHeader` | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| 7 | `TruncatedArchive` | `SREP_E_TRUNCATED` | truncated archive |
| 8 | `CorruptRecord` | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| 9 | `CorruptIndex` | `SREP_E_CORRUPT_INDEX` | corrupt embedded index |
| 10 | `InvalidMatch` | `SREP_E_INVALID_MATCH` | invalid match |
| 11 | `ChecksumMismatch` | `SREP_E_CHECKSUM` | checksum mismatch |
| 12 | `OutputLimitExceeded` | `SREP_E_OUTPUT_LIMIT` | output limit exceeded |
| 13 | `MemoryBudgetExceeded` | `SREP_E_MEMORY_LIMIT` | memory budget exceeded |
| 14 | `TempBudgetExceeded` | `SREP_E_TEMP_LIMIT` | temporary budget exceeded |
| 15 | `NonSeekableNeedsSpool` | `SREP_E_NEEDS_SPOOL` | seekable input or spool required |
| 16 | `TemporaryStorageFailure` | `SREP_E_TEMP_STORAGE` | temporary storage failure |
| 17 | `InputIo` | `SREP_E_INPUT_IO` | input I/O failure |
| 18 | `OutputIo` | `SREP_E_OUTPUT_IO` | output I/O failure |
| 19 | `AtomicPublish` | `SREP_E_ATOMIC_PUBLISH` | atomic publication failed |

CLI human-readable output MUST be exactly `srep: <Message ID>: <Stable summary>` with an optional context suffix `: <context>`. The ID and stable summary are fixed ASCII; context is not part of the stable message identity.

<!-- hard-unit kind="paragraph" -->
Error classification MUST be a function only of the final input bytes and the parser state reached while validating them. It MUST NOT depend on how a test, tool, or caller produced those bytes, including whether an operation was described as a deletion or an in-place mutation. `TruncatedArchive` is limited to a physical EOF before bytes that the parser has already proved are required by a valid enclosing or fixed structure at a concrete range. A final byte string with physically present but invalid structure follows its first observable structural, match, or checksum failure instead. Two edit histories that produce identical final bytes MUST therefore produce the same `ErrorKind`, numeric code, message ID, and summary.
<!-- /hard-unit -->

The first-detection mapping below is exhaustive. Each row assigns exactly one stable variant, numeric code, message ID, and summary template from the table above. Broader rows do not apply when a more specific row matches. Unsupported algorithm or version is distinct from corrupt encoding and from local resource policy: a recognized but unimplemented selector or version is `UnsupportedVersion` or `UnknownChecksum`; a recognized version/ID whose encoding is malformed is a corrupt-header/record/index error; exceeding caller `output_limit`, `memory`, or `temp_limit` is an operational-limit error and MUST NOT be used for a wire-validity failure.

<!-- hard-requirements -->
| First detected field or condition | Variant | Code | Message ID | Summary template |
|---|---|---:|---|---|
| Unknown CLI command, unknown or case-variant option name, repeated selector, mutually exclusive method selectors, invalid SIZE/PATH, method-specific option rejected for the selected method, `--force` on stdout, missing required INPUT/OUTPUT, or other CLI configuration conflict | `InvalidConfiguration` | 2 | `SREP_E_INVALID_CONFIG` | invalid configuration |
| Repeated `--index`, repeated `-index`, or mixed `--index`/`-index` repetitions | `InvalidConfiguration` | 2 | `SREP_E_INVALID_CONFIG` | invalid configuration |
| Library `CompressionConfig` combination rejected before I/O, including method/layout/checksum/seed/target/overlay/limit conflicts | `InvalidConfiguration` | 2 | `SREP_E_INVALID_CONFIG` | invalid configuration |
| Existing file OUTPUT without `--force` | `InvalidConfiguration` | 2 | `SREP_E_INVALID_CONFIG` | invalid configuration |
| Exactly one otherwise syntactically valid explicit `--index=PATH` or `-index=PATH` sidecar selector | `UnsupportedLegacySplitIndex` | 4 | `SREP_E_SPLIT_INDEX` | external legacy index is unsupported |
| Experimental SREP-NG v1 / `.srep2` magic | `UnsupportedVersion` | 3 | `SREP_E_UNSUPPORTED_VERSION` | unsupported archive version |
| Recognized legacy signature with version outside 1..4 | `UnsupportedVersion` | 3 | `SREP_E_UNSUPPORTED_VERSION` | unsupported archive version |
| Recognized SREP-NG magic with version other than 2 | `UnsupportedVersion` | 3 | `SREP_E_UNSUPPORTED_VERSION` | unsupported archive version |
| CLI `--checksum` name other than `xxh3` or `blake3` | `UnknownChecksum` | 5 | `SREP_E_UNKNOWN_CHECKSUM` | unknown checksum |
| NG v2 ArchiveHeader checksum ID other than 1 or 2 | `UnknownChecksum` | 5 | `SREP_E_UNKNOWN_CHECKSUM` | unknown checksum |
| Legacy packed `hash_id`/`encoded_bias`/`seed_len`/`digest_len` combination not equal to one Section 11.2 row | `UnknownChecksum` | 5 | `SREP_E_UNKNOWN_CHECKSUM` | unknown checksum |
| Unknown magic or signature, including neither legacy signature nor SREP-NG magic | `CorruptHeader` | 6 | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| NG v2 ArchiveHeader magic recognized but flags, reserved bytes, layout enum, method enum, semantic flags, `checksum_seed`, `header_record_count`, `header_byte_length`, or other fixed header scalars invalid | `CorruptHeader` | 6 | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| NG v2 header wire-limit violation for block size, minimum match, seed, target, distance, overlay, or other Section 10.0/10.1 scalars | `CorruptHeader` | 6 | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| MethodParameters schema, length, method, flags, reserved bytes, or method-table values invalid | `CorruptHeader` | 6 | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| Legacy signature words invalid or packed header unreadable as a legacy header | `CorruptHeader` | 6 | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| Legacy `BASE_LEN` violating the version-specific policy in Section 11.1 | `CorruptHeader` | 6 | `SREP_E_CORRUPT_HEADER` | corrupt archive header |
| Physical EOF before bytes that a valid parser state has established are required at a concrete range, including total v4 archive length less than `full_header_end + 24` after a complete validated 16-byte header and descriptor-declared seed, a missing fixed Trailer, truncated record frame, truncated payload/checksum, truncated legacy block, or a validly identified v4 declared range extending beyond EOF | `TruncatedArchive` | 7 | `SREP_E_TRUNCATED` | truncated archive |
| Record type unknown, flags nonzero, reserved nonzero, payload_len invalid, record order wrong, mandatory record missing, forbidden record present, or cardinality wrong | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| LayoutMetadata schema, length, layout, flags, reserved bytes, or non-index counters invalid | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| DataBlock frame, schema, flags, reserved bytes, payload length, canonical literal encoding, or I/O operation framing invalid | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| ArchiveSummary schema, length, flags, reserved bytes, or non-index counters invalid | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| Trailer magic, version, flags, reserved bytes, or total-length field invalid when the Trailer bytes are present | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| Any trailing byte after a complete Trailer or after a complete legacy terminator/EOF | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| Legacy v1/v2/v3 body framing, literal consumption, or block-header length fields invalid | `CorruptRecord` | 8 | `SREP_E_CORRUPT_RECORD` | corrupt archive record |
| BlockDirectory or IndexSection framing, schema, count, range, offset, or index-derived LayoutMetadata/Summary counter invalid | `CorruptIndex` | 9 | `SREP_E_CORRUPT_INDEX` | corrupt embedded index |
| Trailer IndexSection offset/length fields inconsistent, including nonzero fields on non-Index layouts or zero fields on Index-LZ | `CorruptIndex` | 9 | `SREP_E_CORRUPT_INDEX` | corrupt embedded index |
| Physically present v4 footer fields, size-array entries/sums/alignment, `stat_total`, range equations, per-block stat sizes, body/index boundary, or embedded index coverage invalid, including a split-looking archive with no explicit sidecar option | `CorruptIndex` | 9 | `SREP_E_CORRUPT_INDEX` | corrupt embedded index |
| IndexSection or BlockDirectory match triple `src`, `dst`, `len`, effective-minimum, overlap, or coverage invariant invalid | `InvalidMatch` | 10 | `SREP_E_INVALID_MATCH` | invalid match |
| Decoded FutureRegister, I/O fragment, or legacy statistic source/destination/length/coverage invariant invalid | `InvalidMatch` | 10 | `SREP_E_INVALID_MATCH` | invalid match |
| Ordinary record trailing checksum mismatch | `ChecksumMismatch` | 11 | `SREP_E_CHECKSUM` | checksum mismatch |
| DataBlock representation-plus-semantics checksum mismatch | `ChecksumMismatch` | 11 | `SREP_E_CHECKSUM` | checksum mismatch |
| ArchiveSummary semantic digest mismatch | `ChecksumMismatch` | 11 | `SREP_E_CHECKSUM` | checksum mismatch |
| Present legacy reconstructed-block digest/checksum mismatch for IDs 0, 2, 3, 4, or 5 | `ChecksumMismatch` | 11 | `SREP_E_CHECKSUM` | checksum mismatch |
| Local `output_limit` exceeded by a structurally valid reconstruction or compression | `OutputLimitExceeded` | 12 | `SREP_E_OUTPUT_LIMIT` | output limit exceeded |
| Local RAM budget exceeded | `MemoryBudgetExceeded` | 13 | `SREP_E_MEMORY_LIMIT` | memory budget exceeded |
| Shared temporary budget exceeded | `TempBudgetExceeded` | 14 | `SREP_E_TEMP_LIMIT` | temporary budget exceeded |
| Required seek or spool cannot be provided as a local policy, distinct from budget exhaustion | `NonSeekableNeedsSpool` | 15 | `SREP_E_NEEDS_SPOOL` | seekable input or spool required |
| Temporary-resource creation, read, write, fsync, or cleanup failure before the shared temporary budget is exceeded | `TemporaryStorageFailure` | 16 | `SREP_E_TEMP_STORAGE` | temporary storage failure |
| OS input read, open, or stat failure | `InputIo` | 17 | `SREP_E_INPUT_IO` | input I/O failure |
| OS output write, flush, or close failure | `OutputIo` | 18 | `SREP_E_OUTPUT_IO` | output I/O failure |
| Atomic publication failure after a successful encode | `AtomicPublish` | 19 | `SREP_E_ATOMIC_PUBLISH` | atomic publication failed |

The first detected error wins in this exact order: CLI/library configuration, including duplicate option detection; exactly one valid legacy sidecar selector as `UnsupportedLegacySplitIndex`; dispatch/header including unsupported version and unknown checksum; Trailer presence for seekable NG v2; record framing/order/cardinality; index ranges; reconstruction/match invariants; per-record and DataBlock checksums; Summary counters then Summary digest; termination/trailing bytes. After a complete 16-byte legacy fixed header and its descriptor-declared seed have been read and validated as version 4, the format establishes the mandatory fixed footer, so total physical length less than `full_header_end + 24` maps to `TruncatedArchive` / 7. After valid footer fields establish exact ranges, EOF or a physical range shorter than a declared required range also maps to `TruncatedArchive` / 7. When the final 24-byte footer area is physically available but footer fields or equations are invalid, the first detected result is `CorruptIndex` / 9. An internal deletion with no valid declaration proving the missing byte is classified from the resulting bytes by the first detected `CorruptIndex`, `CorruptRecord`, `InvalidMatch`, or `ChecksumMismatch`, as applicable. Structural malformed input is never reclassified as a local resource failure. The CLI maps variants to the exact codes and message IDs in the tables; platform-specific OS text MAY be contextual only. The CLI and library MUST never silently skip checksum verification, omit candidates, lower search completeness, change history scope, switch method, or choose a weaker layout.

## 13. Tests, corpus, metrics, and fidelity gates

### 13.1 Semantic and safety tests

The implementation MUST include:

<!-- hard-unit kind="list" -->
- unit tests for every m0-m5 semantic rule, including defaults and exact boundary vectors;
- m1 rolling vectors for block lengths 48, 49, and `48+min_chunk`, including forced hash hits and misses;
- m2 exact vectors for the first eligible hit and consecutive post-reset hits, including the non-replayed trigger byte;
- m0 independent backward-extension tests without cross-candidate interval state;
- m3 fixed source alignment/rounding tests, nondivisible minimum/L vectors, and m4 independent backward-extension tests;
- m5 checked-formula, power-boundary, eight-slice, non-aligned-repeat, and completeness tests for minimums 3, 6, 7, 8, 15, 16, 511, and 512;
- direct legacy goldens for v3 and v4 with `BASE_LEN=0`, including checked `len_minus_base + BASE_LEN` and `len>0`;
- direct legacy goldens for physical v3/v4 source-block fragments, including cross-source-block semantic matches, preserved distance, clipped lengths, ordered groups, pending delivery, and at least one golden whose consecutive physical records share the same source position with `source_gap=0`;
- direct v4 goldens for exact footer ranges, including `footer_start`, `sizes_start`, `index_start`, body, stats, size-array, and footer boundaries;
- first-detection corruption tests based only on resulting bytes and parser observations: terminal prefix cuts that leave a complete validated v4 header and seed but total less than `full_header_end + 24`, or that otherwise demonstrably interrupt a fixed footer or already identified required region, return `TruncatedArchive` / 7; physically present invalid v4 footer fields, size-array entries, sums, alignments, ranges, or body/index coverage return `CorruptIndex` / 9; and shifted internal-deletion cases assert their observable structural or checksum result rather than universally expecting `TruncatedArchive` / 7;
- direct v1 goldens with trailing literal bytes after the final 3-stat record, requiring final literal copy and exact output/literal consumption;
- digest-collision tests for m1/m2/m3 proving exact byte comparison prevents false matches, plus polynomial-key collision tests for m0/m4/m5;
- Match IR property tests for checked bounds, `src < dst`, overlap, destination non-overlap, deterministic tie rules, and exact reconstruction;
- REP-minimum directed tests for `rep_min_match <`, `>`, and `=` base `minimum_match`, plus no-overlay controls, across Index-LZ, Future-LZ, and I/O-LZ; base candidates MUST satisfy base finder rules, overlay candidates MUST satisfy `rep_min_match`, combined normalization MUST accept down to `effective_min_match`, and persisted matches below that effective minimum MUST be rejected;
- malformed-header/MethodParameters tests for zero, out-of-range, inconsistent, and below-effective persisted match minima, with the resulting structured `CorruptHeader` or `InvalidMatch` classification recorded;
- RAM versus forced-spill byte-identical candidate sequences and IR;
- exact CandidateIndex visible-run and private-scratch schema tests, including `SREPIDX1`/`SREPQRY1` magic, exact 64-byte headers, 104-byte records, header checksums, `order_id=1` identity order, and `order_id=2` persisted-query order;
- private-scratch tests proving that order is header-authoritative: wrong order ID, records sorted under the other order, out-of-band order assumptions, malformed header fields, wrong file length, bad record checksum, nonzero padding, invalid key/metadata shape, and unsorted records all reject before consumption;
- exact bounded-dedup tests proving adjacent identity duplicates retain minimum insertion ordinal, distinct metadata remains distinct, and final compaction converts to validated `SREPIDX1` persisted order without changing the visible candidate set;
- query/compaction lifecycle tests proving scratch is local, random exclusive `0600`, charged to the shared TempBudget, absent from `self.runs`, invisible to persistent-run callbacks, removed after success, and cleaned on failure, cancellation, and panic;
- transaction failure tests proving scratch or new-visible-file corruption, flush/fsync/close/validation failure, and callback-precondition failure occur before callback/state commit and leave visible runs, memtable, and generation unchanged;
- all three layouts consuming identical canonical IR and semantic statistics;
- full v2 matrix tests for 6 methods x 3 layouts x 2 checksum IDs;
- empty and nonempty cases in every matrix class;
- malformed/fuzz tests that never panic;
- memory, temp-limit, output-limit, no-silent-degradation tests, and a fidelity-coverage aggregation test that six 0.91 positive-coverage samples plus six `old_covered=0` samples yield geometric mean exactly 0.91 and fail 0.95;
- native-path, CLI, complete configuration-selector including `--force`/`--quiet`/`-q` and INPUT/OUTPUT, checksum-selector, rep-overlay, resource-option, method-specific option rejection, repeated `--index`/`-index`/mixed-index `InvalidConfiguration` tests, a single valid `--index=PATH` or `-index=PATH` `UnsupportedLegacySplitIndex` test, and stable-error-code tests;
- temp ownership, cleanup, cancellation, and panic-best-effort tests;
- private query/compaction scratch boundary tests, including shared-budget high-water accounting, successful-operation non-survival, visible-run-only callback inputs, and exact RAM equality;
- requirement-extractor tests for CommonMark parsing, hard-unit and hard-table markers, CLI selector/default lists, governing capability list, rolling-hash removal formula, statistics list, wire limits, record types/order, completion and acceptance lists, weighted-scheduling tie criteria, the m5 L formula, length-prefixed nested-list canonicalization, Unicode NFC and whitespace normalization, heading paths, duplicate detection, stable IDs including the collision form `REQ-<16 hex>-<64 hex>`, and stale/missing traceability mappings;
- atomic publication tests; and
- a whole-history repeat farther than 256 MiB proving the old prototype window limit is removed.
<!-- /hard-unit -->

### 13.2 Exact v2 matrix assertions

Each row of the 6 x 3 x 2 v2 matrix MUST:

<!-- hard-unit kind="list" -->
1. capture one canonical method Match IR before layout or checksum selection;
2. encode the archive from that same captured IR for the row's layout and checksum;
3. strict-decode the archive;
4. validate every ordinary record checksum, every DataBlock representation-plus-semantics checksum, Summary digest, recomputed counters, Trailer, termination, and all offsets;
5. reconstruct bytes exactly equal to the input;
6. assert that the decoded semantic IR and statistics equal the captured IR and statistics;
7. assert that all three layout rows for the same method/input have byte-identical semantic IR, match count, covered bytes, and literal bytes; and
8. assert rejection of directed malformed siblings for record framing, reserved/flag violations, checksum corruption, trailer corruption, summary mismatch, layout-specific range errors, and every persisted semantic match below `effective_min_match`;
9. for m3, m4, and m5, cover overlay disabled and overlay-enabled configurations with `rep_min_match <`, `>`, and `=` the base `minimum_match` in all three layouts, asserting identical captured/decoded semantic IR and statistics for each configuration;
<!-- /hard-unit -->

Malformed cases MAY be shared by an equivalence class, but the test manifest MUST map each malformed rule to at least one matrix row and record the resulting structured error. The matrix MUST include an empty input and a nonempty input for every method/layout/checksum class.

### 13.3 Legacy matrix and corpus

Legacy goldens MUST cover all 24 version/checksum rows from Section 11.2, their directed corruptions, the existing SREP-VHASH-128 samples, and all mapped layouts. The repository CI MUST NOT require the old binary. Optional tooling MAY regenerate or audit goldens, but committed golden files and their SHA-256 values are authoritative.

<!-- hard-unit kind="list" -->
- The legacy mutation matrix MUST retain broad coverage of every field and boundary listed in Section 11.9 and MUST record the exact expected observable structured error, accepted result, or unchanged output for each resulting byte string.
- Matrix expectations MUST be derived from the resulting bytes and parser observations, not from the operation type used to create a mutation. When different operation histories are observationally indistinguishable, the matrix MUST NOT require an operation-specific classification; in particular, an internal byte deletion MUST NOT be required to return `TruncatedArchive` / 7 without a valid parser-established missing range.
- The checksum-ID-1 (`none`) acceptance and rejection rules in Section 11.9 remain unchanged, including acceptance of an opaque checksum-field mutation with unchanged output and acceptance of structurally valid literal-data mutations as undetectable corruption.
<!-- /hard-unit -->

A versioned corpus manifest MUST include a generation recipe and SHA-256 for every sample. The nine required category values are `exact`, `nonaligned`, `insert-delete`, `order1`, `far-distance`, `multiversion-tree`, `vm-disk-sparse`, `compressed-block-repeat`, and `random-incompressible`. Required categories are:

<!-- hard-unit kind="list" -->
- exact repeats;
- non-aligned repeats;
- insertion/delete shifts;
- order-1 context changes;
- far-distance matches;
- multiversion trees;
- VM/disk sparse data;
- compressed-block repeats; and
- random/incompressible data.
<!-- /hard-unit -->

### 13.4 Metrics and exact fidelity aggregation

The fidelity population is frozen before implementing or tuning m0-m5, in a dedicated baseline stage or Stage 1. The repository MUST commit nonempty `tests/fidelity/corpus-v1.json` and `tests/fidelity/legacy-baseline-v1.json` before method implementation begins. Their SHA-256 values, representative labels, and old baseline measurements MUST be independently reviewed and recorded before any new implementation result is evaluated. Changing representative labels, generation recipes, sample bytes, old metrics, the xz version, or the xz command is a baseline rebase and requires independent reviewer `PASS` before new fidelity results are accepted.

Each corpus entry MUST have exactly the fields `id`, nonempty `categories`, nonempty `methods`, `representative`, `generator`, `version`, `args`, `sha256`, and `size`. `categories` is a nonempty array from the required category enum in this section; `methods` is a nonempty array of method names; `representative` is a frozen boolean; `sha256` is the lowercase SHA-256 of the exact sample bytes; and `size` is the exact byte count. For every method `m0` through `m5`, the frozen representative set MUST be nonempty and MUST include at least one representative in each of the nine required categories and at least 12 distinct representative samples total. Independently, each method's frozen **positive-coverage** population, defined as representatives with `old_covered > 0` for that method, MUST be nonempty and MUST contain at least 6 distinct samples. A sample MAY serve multiple methods and categories, but every `(method,category)` pair MUST be covered. An empty method population, an empty positive-coverage population for any method, a missing corpus or baseline file, a missing category for any method, or a selectively omitted frozen representative is a hard failure; new results MUST NOT be evaluated against a vacuous, missing, or partially omitted population. Random/incompressible samples MAY have `old_covered=0` and remain frozen representatives for size/incompressible reporting.

The legacy baseline manifest MUST record, for every method and every frozen representative, `old_covered`, `old_literals`, `old_matches`, `old_preprocessor_size`, `old_final_size`, the old binary SHA-256, the exact xz version, and the exact xz command. Representative flags, sample bytes, generators, arguments, and old metrics MUST be validated against the frozen corpus/baseline manifests before calculating ratios. New results cannot influence population selection, and every frozen representative MUST be used. Fidelity old/new runs use the default Index-LZ layout and new default XXH3 checksum. Layout and checksum matrices are tested separately and are not multiplied into the fidelity population. A failed or missing old or new run is a hard evidence failure, not an omitted sample.

Every method report records match-covered bytes, literal bytes, match count, preprocessor size, and final size after exactly this fixed backend command:

<!-- hard-unit kind="formula" -->
```text
xz --format=raw --threads=1 --lzma2=dict=64MiB,lc=3,lp=0,pb=2,mode=normal,nice=273,mf=bt4
```
<!-- /hard-unit -->

The baseline records the exact xz version and command. A version mismatch is an evidence error unless the baseline is explicitly rebased and reviewed. Preprocessor size is reported but is not itself a hard ratio gate unless a later versioned policy adds that gate.

Coverage aggregation uses only the frozen positive-coverage population. For a method, let `P` be the nonempty set of frozen representatives with `old_covered > 0` for that method. Samples with `old_covered = 0` MUST remain in the frozen representative set, final-size geometric mean, per-sample 110% size gate, and incompressible overhead report, and MUST NOT enter coverage aggregation and MUST NOT be assigned a coverage ratio. For each sample in `P`, let `old_covered` and `new_covered` be exact integer byte counts. A negative `new_covered` is an evidence error. If `old_covered > 0` and `new_covered = 0`, coverage_ratio is exactly 0 and the method coverage gate fails. Coverage retention for samples in `P` is:

<!-- hard-unit kind="formula" -->
```text
coverage_ratio = min(1, new_covered / old_covered)   for each sample in P
method_coverage_geomean = exp(mean_{s in P}(ln(coverage_ratio_s)))
method_coverage_geomean >= 0.95
```
<!-- /hard-unit -->

Improvements above the old coverage are capped at 1 so they cannot mask another sample's loss. The mean is over equally weighted members of `P` only, `ln` is calculated from exact ratios with at least 12 decimal digits for the report/gate, and no sample ratio is pre-rounded. The reviewer counterexample is normative: six samples with coverage_ratio 0.91 and six frozen representatives with `old_covered=0` MUST aggregate to exactly 0.91, not `sqrt(0.91)`, and MUST fail the 0.95 gate.

For final compressed sizes, every frozen representative, including `old_covered=0` samples, is used. `old_final_size` MUST be greater than zero. For each sample:

<!-- hard-unit kind="formula" -->
```text
size_ratio = max(1, new_final_size / old_final_size)
method_size_geomean = exp(mean(ln(size_ratio))) <= 1.05
```
<!-- /hard-unit -->

Improvements are capped at 1 and cannot mask regressions. Every representative sample MUST satisfy raw, unrounded `new_final_size/old_final_size <= 1.10`. Categories are equally sample-weighted: each sample has one weight, and categories are not weighted as groups. Random/incompressible samples MAY be representatives; when `old_covered=0`, they contribute to size/incompressible reporting only.

Incompressible overhead is bounded and reported separately. Reports are emitted as machine JSON and a human-readable table. CI MAY use a signed baseline manifest instead of an old executable.

### 13.5 Fidelity hard gate

For each method, the hard gate requires semantic directed tests, exact matrix assertions, a nonempty frozen positive-coverage population, geometric mean coverage at least 95% over that positive-coverage population only, geometric mean final size no more than 105% over all frozen representatives, and every representative sample no more than 110%. Better results are allowed. Forced-spill and all-RAM results must be equal wherever indexing is used. Layout reports must have identical IR statistics.

## 14 Requirement traceability

The implementation repository MUST maintain a versioned requirement manifest generated from this normalized specification. The extractor MUST parse the document as a CommonMark AST using `pulldown-cmark` version `0.13.0` with `Options::ENABLE_TABLES` enabled and all other `pulldown-cmark` options disabled. Traversal is depth-first in event order. The extractor maintains the current heading path as the sequence of heading texts from level 1 through the current heading, joined later by ASCII ` > `. A new heading of level `N` truncates the path to levels `< N` and then appends the new heading text.

Hard units are AST-contained subtrees delimited by HTML comments. The exact start marker bytes are `<!-- hard-unit kind="KIND" -->` where `KIND` is one of `paragraph`, `list`, `formula`, or `table`. The exact end marker bytes are `<!-- /hard-unit -->`. The marked subtree is the contiguous AST nodes between those comments under the same heading. Marker comments themselves are excluded from the hashed payload and exist only to bound the subtree. Markers MUST NOT nest inside another hard-unit. A malformed, unclosed, overlapping, nested, or unknown-kind marker is an extractor failure. Each material AST node belongs to at most one extracted unit: a node inside a hard-unit is extracted only as part of that unit; unmarked MUST prose is extracted only when it is not inside any hard-unit.

Canonical serialization of a marked subtree is defined below. Integer fields in structural encodings are unsigned little-endian `u32` values. A length-prefixed byte string is the `u32` little-endian byte count of the following payload, immediately followed by exactly that many bytes. The token alphabet is ASCII.

**Leaf text normalization** applies to heading-path text, paragraph text, list-item paragraph text, inline code text, and table-cell text, independently for each leaf before it is length-prefixed:

<!-- hard-unit kind="list" -->
1. Decode as UTF-8.
2. Apply Unicode Normalization Form C (NFC).
3. Replace every Unicode whitespace sequence (`White_Space=Yes`) with one ASCII space `U+0020`.
4. Trim leading and trailing ASCII spaces.
<!-- /hard-unit -->

**Formula/code policy** is separate: a `kind="formula"` unit hashes the exact UTF-8 bytes of the fenced or indented code content after NFC, with no whitespace collapsing, including interior newlines, excluding the fence language tag and the fence delimiter lines. Ordinary unmarked code blocks remain excluded.

**Paragraph unit.** Kind token `paragraph`. Payload is one length-prefixed NFC/whitespace-normalized rendered paragraph, including inline code as literal code text and excluding link/image destinations.

**List unit.** Kind token `list`. The extractor traverses the list AST structurally and MUST NOT flatten nested lists into parent item text before hashing. Serialization is the concatenation of these fields, in this order:

<!-- hard-unit kind="formula" -->
```text
ASCII magic "SREP-REQ-LIST"
u32 version = 1
u8 list_kind: 0 unordered, 1 ordered
u32 start: CommonMark ordered start, or 1 for unordered
u32 item_count
then, for each item in order, an item record:
  u32 ordinal               // 0-based index among siblings
  u32 depth                 // 0 for the marked list's own items
  u32 child_block_count
  then child_block_count block records in source order, each:
    u8 block_kind           // 1=paragraph, 2=nested-list, 3=formula/code
    if paragraph: length-prefixed normalized leaf text
    if nested-list: the complete recursive list encoding of that child list
    if formula/code: length-prefixed exact NFC formula bytes under the formula policy
```
<!-- /hard-unit -->

A nested list is encoded with the same `SREP-REQ-LIST` record, `depth` increased by one for its items. Changing ordered versus unordered, ordered `start`, item count, nesting, item order, or any leaf text changes the encoding. Delimiter-like characters inside item text cannot collide with structure because every text field is length-prefixed.

**Formula unit.** Kind token `formula`. Payload is one length-prefixed exact NFC formula/code byte string under the formula policy.

**Table.** A marked `kind="table"` unit or a `<!-- hard-requirements -->` table is one hard table. CommonMark tables enabled by `pulldown-cmark` `Options::ENABLE_TABLES` have exactly one header row; additional rows are body rows. Multi-row headers do not exist in this grammar. The extractor MUST reject a table marker that is not immediately adjacent under the same heading, that marks a table with no header row, or that marks a table whose header and body together have no cell structure. A hard table with a header and zero body rows is valid: it emits only a `table-schema` unit whose `body_row_count` is `0` and whose membership list is empty. Overflow of any checked `u32` ordinal, count, or length is an extractor failure.

`heading_local_table_ordinal` is the 0-based index of that table among **all** parsed Markdown tables under the exact same heading, in source/AST order, including unmarked observational tables. Hard-table membership of a table does not change how ordinals are assigned. Inserting, deleting, or reordering any Markdown table under that heading MAY change later ordinals and is intended to stale those later `table-schema` IDs.

Every hard table emits exactly one extracted unit of kind `table-schema`, then one extracted unit of kind `table-row` per body row, in document order. The standalone `table-row` structural payload MUST remain exactly `u32 cell_count` followed by that many length-prefixed normalized cell strings. That encoding excludes heading bytes, kind bytes, ordinal, header text, alignment, and sibling rows, so the required 240-byte synthetic `table-row` golden below is unchanged. A body row whose `cell_count` differs from the schema `column_count` is an extractor failure.

The `table-schema` structural bytes bind header identity and complete ordered body-row membership. They are the concatenation of:

<!-- hard-unit kind="formula" -->
```text
ASCII magic "SREP-REQ-TSCH"
u32 schema_version = 1
u32 heading_local_table_ordinal   // 0-based among ALL parsed Markdown tables under this heading
u32 column_count                  // header cell count; each body row MUST equal this count
then, for each header column in left-to-right order:
  u8 alignment                    // 0=none, 1=left, 2=center, 3=right from the delimiter row
  length-prefixed NFC/whitespace-normalized header cell bytes
u32 body_row_count                // checked; 0 is valid for an empty hard table
then, for each body row in source order:
  u32 body_row_ordinal            // 0-based in this table; MUST equal the loop index
  u32 row_payload_len             // byte length of that row's standalone table-row payload
  32 bytes SHA-256(standalone table-row payload)
```
<!-- /hard-unit -->

The SHA-256 in each membership slot is over the exact standalone canonical `table-row` payload bytes (`u32 cell_count` plus the length-prefixed normalized cells). It MUST NOT be computed over the requirement heading/kind wrapper and MUST NOT be a truncated requirement ID. The 32 digest bytes are stored raw, not hex. `row_payload_len` MUST equal the length of those same standalone payload bytes.

Header and body cell strings use the same leaf-text normalization as paragraphs: UTF-8 decode, NFC, Unicode whitespace collapse, trim. Inline code inside a cell is literal code text; link and image destinations are excluded. Empty cells encode as length prefix `0` with no payload bytes. Escaped pipes are the post-parse AST cell text, so a GFM `\|` is one literal `|` code point after parsing and is then normalized as leaf text. Delimiter-row dash counts are not hashed; only the four alignment values above are part of the CommonMark table AST and enter the schema.

Changing any of the following MUST change the `table-schema` payload and ID and stale its evidence mapping: header cell text, column count, column order, any column alignment, heading-local table ordinal, body-row count, body-row order, which rows belong to the table, or any member row's standalone payload. It MUST NOT, by itself, change an unchanged body row's standalone `table-row` structural bytes or that row's requirement ID. Reordering rows, inserting or deleting a row, moving an unchanged row between two same-heading tables, or swapping two same-heading identical-header tables with different bodies MUST change every affected `table-schema` ID while an unchanged moved/reordered row keeps the same `table-row` ID.

Unmarked extraction remains in force only for unmarked MUST prose outside every hard-unit: a requirement unit is one entire unmarked paragraph or one unmarked list item whose rendered plain text contains the uppercase token `MUST` or `MUST NOT` as a whole word delimited by nonletters (`[^A-Za-z]` or string boundaries). Nested unmarked list items are separate units. Unmarked fenced or indented code blocks, HTML blocks, HTML comments, autolinks, link and image destinations, and heading nodes are excluded from unmarked-unit text. Soft breaks and hard breaks become a single ASCII space before unmarked-paragraph leaf normalization. Tables are not extracted by default. A table becomes a hard table when either a `kind="table"` hard-unit surrounds it or the HTML comment whose exact bytes are `<!-- hard-requirements -->` is the last nonblank AST node before that table under the same heading. Ordinary explanatory code examples remain excluded unless wrapped in a `kind="formula"` hard-unit.

Every material normative list, normative fenced formula/algorithm, and hard table whose content changes interoperability, CLI defaults or options, algorithm semantics, wire validity, resource or error semantics, compatibility, fidelity gates, completion gates, or acceptance evidence is wrapped in a hard-unit or a `<!-- hard-requirements -->` table. Changing any byte inside a marked selector, default, list item, formula, governing capability item, rolling-hash removal term, statistic definition, wire-limit item, record type, record-order formula, completion condition, acceptance-evidence item, Section 11.9 version/layout compatibility cell, hard-table header cell, hard-table body cell, hard-table row order, or hard-table membership changes that unit's canonical payload bytes and therefore its requirement ID and stale evidence mapping. A header-only, alignment-only, ordinal-only, reorder-only, insert/delete, or inter-table move MUST change each affected `table-schema` ID and MUST NOT, by itself, change an unchanged body row's `table-row` ID.

The canonical payload hashed for each requirement is an arbitrary byte sequence, not necessarily valid UTF-8. Leaf heading, paragraph, list-item, inline-code, and table-cell strings are still NFC/whitespace-normalized UTF-8 before they are length-prefixed, and kind tokens remain ASCII, but little-endian `u32` length prefixes and structural fields MAY introduce bytes that are not valid UTF-8. The canonical payload is exactly the concatenation of these three fields, in this order, with no extra NUL or other separator:

<!-- hard-unit kind="formula" -->
```text
length-prefixed heading_path_bytes
length-prefixed kind_ascii_bytes
canonical_structural_unit_bytes
```
<!-- /hard-unit -->

`heading_path_bytes` are the NFC/whitespace-normalized heading path joined by ASCII ` > `, encoded as UTF-8. `kind_ascii_bytes` are the exact ASCII kind token `"paragraph"`, `"list-item"`, `"list"`, `"formula"`, `"table-schema"`, or `"table-row"`. `canonical_structural_unit_bytes` are the canonical serialized unit bytes defined above. Field boundaries are the length prefixes. A marked `kind="list"` unit is one unit of kind `list`, not one unit per item. A marked `kind="formula"` unit is one unit of kind `formula`. A marked `kind="paragraph"` unit is one unit of kind `paragraph`. A hard table contributes one unit of kind `table-schema` and zero or more units of kind `table-row`. The diagnostic same-heading ordinal used for unmarked MUST paragraphs does not enter the hash; the heading-local table ordinal and ordered membership list are part of `table-schema` structural bytes only and are not part of any `table-row` payload. The hash algorithm is SHA-256 over those exact canonical payload bytes. The extractor version token in the manifest is `"6"`.

The stable ID is:

<!-- hard-unit kind="formula" -->
```text
REQ-<first 16 lowercase hexadecimal characters of SHA-256(canonical payload bytes)>
```
<!-- /hard-unit -->

If two extracted units produce the same 16-hex prefix, the extractor MUST compare complete canonical payload bytes. Distinct payloads that collide in the 16-hex prefix are disambiguated as `REQ-<16 hex>-<64 hex>` using the lowercase hexadecimal SHA-256 of the complete canonical payload bytes. Identical complete canonical payload bytes under the same heading path and kind are a specification error and the extractor MUST fail. A `table-schema` unit and a `table-row` unit under the same heading are distinct because their kind tokens differ. Multiple `table-row` units under the same heading are distinct when their standalone structural cell bytes differ. Two hard tables under the same heading with identical headers remain distinct when `heading_local_table_ordinal` or ordered membership differs; swapping those tables or moving a row between them MUST yield distinct `table-schema` payloads. Duplicate identical standalone `table-row` payloads under the same heading and kind are a specification error. This document MUST keep every same-path same-kind unit textually unique so the collision suffix is unused for currently extracted units. The accepted ID grammar is exactly this regular expression:

<!-- hard-unit kind="formula" -->
```text
^REQ-[0-9a-f]{16}(-[0-9a-f]{64})?$
```
<!-- /hard-unit -->

The exact JSON-safe representation of the hashed bytes is lowercase hexadecimal of the full canonical payload. The required field name is `canonical_payload_hex`. There is no `normalized_text` field in the exact schema; a UTF-8 text reconstruction of the hashed bytes is neither required nor sufficient, because those bytes need not be valid UTF-8. `heading` is a UTF-8 diagnostic/path string. It MUST NOT be used to recover hashed bytes: binary recovery is hex-decoding `canonical_payload_hex`, and the extractor MUST recompute the canonical payload from this specification. The empty hex string would decode to an empty payload and is permitted by the hex grammar only because that grammar describes any even-length lowercase hex string; heading plus kind prefixes make every extracted unit's payload nonempty, so empty `canonical_payload_hex` MUST be rejected for extracted units. `canonical_payload_hex` MUST match exactly:

<!-- hard-unit kind="formula" -->
```text
^([0-9a-f]{2})*$
```
<!-- /hard-unit -->

Decoding that hex MUST reproduce exactly the hashed canonical payload bytes. The manifest file is `tests/requirements.json` and MUST be UTF-8 JSON with this exact schema:

<!-- hard-unit kind="formula" -->
```text
{
  "spec_path": "docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md",
  "spec_sha256": "<64 lowercase hex SHA-256 of the exact spec file bytes>",
  "extractor_version": "6",
  "requirements": [
    {
      "id": "<ID matching ^REQ-[0-9a-f]{16}(-[0-9a-f]{64})?$>",
      "heading": "<UTF-8 diagnostic heading path; not used to recover hashed bytes>",
      "kind": "paragraph" | "list-item" | "list" | "formula" | "table-schema" | "table-row",
      "canonical_payload_hex": "<even-length lowercase hex matching ^([0-9a-f]{2})*$>",
      "tests": ["<rust test name or integration test binary>"],
      "evidence": ["<repository-relative evidence path>"]
    }
  ]
}
```
<!-- /hard-unit -->

`tests` and `evidence` arrays MUST both be present; at least one of them MUST be nonempty for every requirement, including every `table-schema` unit and every `table-row` unit. `SHOULD` and `MAY` statements are not hard requirements unless they appear in a hard-unit, hard-table schema, hard-table row, or extractable `MUST`/`MUST NOT` unit. The recorded `spec_sha256` MUST equal the SHA-256 of the current spec file bytes. Manifest validation MUST recompute every unit from the current specification AST, including every `table-schema` from that table's header, alignments, heading-local ordinal, body-row count, source order, and the full 32-byte SHA-256 of each standalone `table-row` payload. It MUST hex-decode each `canonical_payload_hex`, compare the decoded bytes with that freshly extracted canonical payload byte-for-byte, and verify that `id` equals `REQ-` plus the first 16 lowercase hexadecimal characters of SHA-256 of those exact bytes, using the collision suffix when required by the ID grammar. Updating `spec_sha256` alone MUST NOT make a stale `table-schema` or `table-row` ID, payload, tests array, or evidence mapping pass. Every `table-schema` requirement MUST map to tests or evidence that validate the header and the complete ordered row membership; each `table-row` still maps separately.

CI MUST run the extractor against this specification and fail when an extracted ID is missing from the manifest, the manifest contains an ID not currently extracted, `spec_sha256` is stale, a mapped test or evidence path is missing, a hard-unit or hard-table marker is malformed, `canonical_payload_hex` is missing or fails the hex grammar, decoded payload bytes differ from fresh extractor output, `id` is not SHA-256 of those exact bytes, or two same-path same-kind units are not unique. Duplicate IDs from distinct heading paths, or from different kinds under the same heading, are valid only when their canonical payload bytes differ. Required extractor golden assertions MUST cover all of the following, and an edit inside any named unit MUST change that unit's ID and fail the stale evidence mapping: the governing capability list; the rolling-hash removal formula; the semantic statistics list; the wire hard-limit list; the record-type list; the mandatory record-order formula; the completion-evidence list; the acceptance-evidence list; the CLI command list; the CLI selector/default list; the weighted-scheduling tie-criteria list; the m5 L formula; the Section 11.9 version/layout compatibility `table-schema` and each of its four `table-row` units, such that changing any version/layout cell MUST change the corresponding `table-row` ID and stale the evidence mapping, and changing any header cell MUST change the `table-schema` ID without changing unchanged row IDs; the Section 11.2.2 256-byte historical-archive tuple `table-schema` and each of its seven `table-row` units; the Section 11.2.2 long VHASH historical-archive tuple `table-schema` and each of its four `table-row` units for n=4095/4096/4097/8192; the Section 11.10 preserved-versus-improvement `table-schema` and each of its seven `table-row` units; at least one other hard table's schema and rows; a synthetic 16-hex prefix collision ID matching the regex with the `-` plus 64-hex suffix; and a stale `spec_sha256`. Required structural goldens MUST additionally prove distinct encodings and IDs, cryptographic digest collisions aside, for: a flat list versus a nested list; a one-item list versus a two-item list; unordered versus ordered; ordered `start=1` versus `start=2`; changed nesting depth; item text containing delimiter-like characters such as newlines, ` | `, or `SREP-REQ-LIST`; a header-cell edit that changes the `table-schema` ID while leaving unchanged body-row payloads identical; an alignment-only delimiter edit that changes the `table-schema` ID while leaving unchanged body-row payloads identical; a body-row reorder that changes the `table-schema` ID while each reordered row keeps the same standalone `table-row` ID; a row insertion and a row deletion that change the `table-schema` ID; a move of an unchanged row between two equal-width same-heading tables that changes both `table-schema` IDs while that row's `table-row` ID stays the same; a swap of two same-heading identical-header tables that have different bodies, changing both `table-schema` IDs; and a member-row content change that changes both that `table-row` ID and the owning `table-schema` ID.

Required binary-representation goldens MUST use this exact synthetic table-row construction, independent of surrounding prose, with heading bytes the ASCII string `ExtractorGolden`, kind bytes the ASCII string `table-row`, and structural unit bytes `u32 cell_count = 1` followed by one length-prefixed cell whose payload is 200 bytes of ASCII `A` (`0x41`). The structural length prefix 200 is little-endian `c8 00 00 00`. The complete canonical payload is therefore:

<!-- hard-unit kind="formula" -->
```text
0f 00 00 00
45 78 74 72 61 63 74 6f 72 47 6f 6c 64 65 6e
09 00 00 00
74 61 62 6c 65 2d 72 6f 77
01 00 00 00
c8 00 00 00
41 x 200
```
<!-- /hard-unit -->

Those 240 bytes are not valid UTF-8 because `c8` is an invalid UTF-8 continuation at that offset. Their exact lowercase hex, which is the required `canonical_payload_hex` value, is:

<!-- hard-unit kind="formula" -->
```text
0f000000457874726163746f72476f6c64656e090000007461626c652d726f7701000000c80000004141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141
```
<!-- /hard-unit -->

SHA-256 of those exact bytes is `2ecd3c7354df617f7d6300d1b21e4716f43dbe8b84097f3d2fbb83bac5519db1`, so the non-colliding ID is `REQ-2ecd3c7354df617f`. A JSON object containing only that `canonical_payload_hex` string MUST round-trip to the same lowercase hex, hex-decoding MUST recover the same 240 bytes, and SHA-256/`id` MUST be identical before and after that JSON round trip. Introducing `table-schema` MUST NOT change this `table-row` construction, hex, SHA-256, or ID.

Required table-schema goldens MUST use this exact synthetic two-column, two-row construction, independent of surrounding prose. Heading bytes are the ASCII string `ExtractorTableGolden`. Kind bytes are the ASCII string `table-schema`. Structural bytes are magic `SREP-REQ-TSCH`, `schema_version = 1`, `heading_local_table_ordinal = 0`, `column_count = 2`, both alignments `0` (none), header cells ASCII `ColA` and `ColB`, `body_row_count = 2`, then two membership slots. Row 0 is standalone payload `u32 cell_count = 2` plus length-prefixed ASCII `x` and `y` (`0200000001000000780100000079`, 14 bytes, SHA-256 `f1a60a0754f86fdecaf7b85ed007120bbbf4e2a8cbc4ecf9ff918667ea322daf`). Row 1 is standalone payload `u32 cell_count = 2` plus length-prefixed ASCII `p` and `q` (`0200000001000000700100000071`, 14 bytes, SHA-256 `1774cd2f9a9eb2184cc745f25ed86165dc9d10405bfdc937d05d0a7f8f19f0f1`). The complete canonical payload is 167 bytes:

<!-- hard-unit kind="formula" -->
```text
14 00 00 00
45 78 74 72 61 63 74 6f 72 54 61 62 6c 65 47 6f 6c 64 65 6e
0c 00 00 00
74 61 62 6c 65 2d 73 63 68 65 6d 61
53 52 45 50 2d 52 45 51 2d 54 53 43 48
01 00 00 00
00 00 00 00
02 00 00 00
00 04 00 00 00 43 6f 6c 41
00 04 00 00 00 43 6f 6c 42
02 00 00 00
00 00 00 00 0e 00 00 00 f1 a6 0a 07 54 f8 6f de ca f7 b8 5e d0 07 12 0b bb f4 e2 a8 cb c4 ec f9 ff 91 86 67 ea 32 2d af
01 00 00 00 0e 00 00 00 17 74 cd 2f 9a 9e b2 18 4c c7 45 f2 5e d8 61 65 dc 9d 10 40 5b fd c9 37 d0 5d 0a 7f 8f 19 f0 f1
```
<!-- /hard-unit -->

Their exact lowercase hex is:

<!-- hard-unit kind="formula" -->
```text
14000000457874726163746f725461626c65476f6c64656e0c0000007461626c652d736368656d61535245502d5245512d545343480100000000000000020000000004000000436f6c410004000000436f6c4202000000000000000e000000f1a60a0754f86fdecaf7b85ed007120bbbf4e2a8cbc4ecf9ff918667ea322daf010000000e0000001774cd2f9a9eb2184cc745f25ed86165dc9d10405bfdc937d05d0a7f8f19f0f1
```
<!-- /hard-unit -->

SHA-256 of those exact bytes is `bf4791a2423d28bbf09ee16e138b32cf1fcf94949f2a7acfae337bd0704048a8`, so the non-colliding ID is `REQ-bf4791a2423d28bb`. The paired first body row uses the same heading bytes, kind `table-row`, and standalone structural bytes `0200000001000000780100000079`. Its complete canonical payload hex is `14000000457874726163746f725461626c65476f6c64656e090000007461626c652d726f770200000001000000780100000079`, SHA-256 `14a41f4e5664d6df7fe0b5b95d8cdeb0c03b78e748e5d64226bf3721d3edb6cb`, ID `REQ-14a41f4e5664d6df`. Replacing only the first header cell `ColA` with `ColA2` MUST change the schema ID to `REQ-522eaee069683585` (SHA-256 `522eaee0696835857614f8500ff6531d7641b18144d5d8d19132894a7391777c`) and MUST leave that body-row payload, SHA-256, and ID unchanged. Reordering the two membership slots to `p|q` then `x|y` MUST change the schema ID to `REQ-8d3df8d9ba7ceaac` (SHA-256 `8d3df8d9ba7ceaac1c5f72d2956609ac1b7aef37b710a2f99de7f97797ed5551`) and MUST leave both standalone `table-row` IDs unchanged. The 240-byte `ExtractorGolden` `table-row` SHA-256 and ID MUST remain exactly as specified above.

Required malformed-hex goldens MUST reject each of the following as invalid manifest payload representation: uppercase hex (`C8000000` or any uppercase A-F in `canonical_payload_hex`); odd-length hex (`c800000`); nonhex (`c800000g`); and a well-formed lowercase hex string whose decoded bytes do not match the requirement `id` or do not equal the freshly extracted canonical payload. The final audit MUST use the prompt-to-artifact checklist generated from this manifest. This traceability mechanism is a target implementation requirement; its manifest and tests do not yet exist merely because this document specifies them.

## 15 Staged delivery and review discipline

Every stage MUST use TDD, an implementation subagent, specification review, and code-quality review. All findings MUST be fixed and re-reviewed before the next stage begins.

<!-- hard-unit kind="list" -->
1. Foundation refactor, v2 primitives, checksums, and prototype-v1 rejection.
2. Legacy v1-v4 reader, including independently implemented/audited SREP-VHASH-128 support.
3. Shared Match IR and all three v2 layouts.
4. m0.
5. m1 and m2.
6. m3 and m4.
7. m5.
8. Deterministic sorted-run spill index, private query/compaction scratch, and the farther-than-256-MiB test.
9. CLI, documentation, migration behavior, and fidelity gate.
10. Profile-only performance improvements after fidelity is proven.
<!-- /hard-unit -->

A stage is not complete because code exists. Its tests, specification review, and code-quality review must pass, with no unresolved findings.

## 16. Final completion gates

Completion evidence MUST include:

<!-- hard-unit kind="list" -->
- Cargo build on Linux, Windows, and macOS;
- stable Rust and the declared MSRV;
- `cargo fmt --all -- --check`;
- `cargo clippy --all-targets --all-features -- -D warnings`;
- all tests and applicable release-mode tests;
- a release build;
- documentation validation;
- dependency and legal audit;
- the complete legacy matrix across all 24 version/checksum rows and mapped layouts;
- the complete v2 6-method x 3-layout x 2-checksum matrix;
- whole-history and explicit max-distance behavior;
- forced-spill/all-RAM equality;
- exact visible `SREPIDX1` run and private `SREPQRY1` scratch schemas, including authoritative order IDs and rejection of wrong interpretation;
- validated order-1 bounded deduplication, order-2 query consumption, final conversion to visible persisted order, and exact RAM equality;
- shared temporary-budget reservations, random-exclusive `0600` scratch creation, RAII cleanup/non-survival, and query/compaction lifecycle evidence;
- failure evidence showing malformed or incomplete scratch/new-visible files fail before callbacks or state commit and preserve visible runs, memtable, and generation;
- all semantic, 5%, and 10% fidelity gates;
- checksum golden vectors and cross-platform audit;
- final reviewer result `PASS` with no unresolved findings; and
- documentation claims limited to tested behavior and recorded evidence.
<!-- /hard-unit -->

## 17. Acceptance evidence

The implementation review MUST retain reproducible evidence that does not require the historical binary in CI. The evidence package MUST include:

<!-- hard-unit kind="list" -->
- Linux, Windows, and macOS build results on stable Rust and the declared MSRV;
- formatting, check, clippy, complete-test, release-build, documentation, dependency, and legal-audit results;
- XXH3 oneshot/streaming/chunking/boundary/word-size/endian vectors and BLAKE3 serialization vectors;
- legacy v1-v4 results for all six checksum IDs, mapped layouts, existing SREP-VHASH-128 samples, split-option rejection, and directed corruptions;
- every v2 matrix row, with record checksums, block checksums, summary digest, counters, trailer, termination, and exact reconstruction recorded;
- exact canonical IR comparisons across layouts and RAM/spill implementations;
- whole-history, max-distance, nonseekable input, stdout spool, budget-failure, cleanup, and atomic-publication results;
- CandidateIndex scratch schema, order-authority, bounded-dedup, shared-budget, lifecycle, callback/commit ordering, rollback, and successful-operation cleanup results;
- the versioned corpus manifest, generation recipe, SHA-256 values, exact method metrics, xz version, and fixed command; and
- an independent final reviewer record whose result is `PASS` and which has no unresolved findings.
<!-- /hard-unit -->

Evidence MUST distinguish tested target behavior from the observed baseline in Appendix A. A document or report MUST NOT claim a target behavior without corresponding test or recorded evidence.

## 18. Explicit non-goals and future work

The following are explicit non-goals:

<!-- hard-unit kind="list" -->
- permanent external index sidecars;
- reading or converting current prototype SREP-NG v1;
- byte-identical legacy compressed output;
- compatibility with every old CLI or performance switch;
- authentication, signatures, or protection against malicious checksum recomputation; and
- SIMD, threading, prefetching, or other performance tuning before fidelity.
<!-- /hard-unit -->

Checksums detect corruption and validation failures but do not authenticate an archive. Temporary spools and indexes are implementation resources, not supported archive artifacts. Future performance work may optimize a validated algorithm only after fidelity is proven and may not change its candidate set, confirmation semantics, history scope, IR, or format meaning. A future feature that changes archive semantics, checksum interpretation, candidate completeness, or layout interoperability requires a new design decision and format review.

## Appendix A: observed black-box baseline

This appendix records already observed black-box behavior and is not a claim about the target implementation. On an 8,683,520-byte structured corpus, all 18 old combinations of `m0` through `m5` with Index-LZ, Future-LZ, and I/O-LZ round-tripped. The observed archive-size ranges were:

| Method | Observed archive-size range |
|---|---:|
| m0 | 525,612–525,688 |
| m1 | 970,814–970,874 |
| m2 | 1,268,757–1,268,817 |
| m3 | 531,024–531,320 |
| m4 | 525,018–525,206 |
| m5 | 525,018–525,206 |

The observed legacy mapping was v1 rounded I/O-LZ, v2 I/O-LZ, v3 Future-LZ, and v4 embedded Index-LZ. Existing real SREP-VHASH-128 v4 samples were 112 bytes and 1,675 bytes, with `BASE_LEN=0`.

A split-index black-box observation is also retained: a 1,860,000-byte input produced a split main archive of 2,056 bytes plus a 112-byte sidecar, while the embedded form was 2,168 bytes. Both forms had identical footer declarations. This demonstrates that the archive-only reader has no reliable in-band marker for split indexing and therefore MUST follow Section 2.2 rather than probe a sidecar.

These observations seed regression fixtures and corpus design. They do not relax any target requirement.

## Appendix B: factual completeness checklist and review state

Design status: approved normative wire/algorithm specification; v4 observable-error, overlay-minimum, and index-scratch clarifications have independent review `PASS`; implementation pending.

The following checklist records content present in the approved architecture. The prior broad design, v4 observable-error clarification, and overlay-minimum clarification received independent review `PASS`. No implementation completion is claimed. Implementation evidence, tests, matrices, fidelity gates, and final implementation review remain outstanding:

<!-- hard-unit kind="list" -->
- Overlay effective minimum, provenance-independent normalization, and independent review `PASS` status are defined.
- Legacy embedded-index v1-v4 reading and NG v2-only writing are defined.
- Prototype NG v1 rejection and permanent split-index policy are defined.
- Whole-history default and explicit max-distance are defined; m0 uses random access or owned spools.
- Exact shared defaults, polynomial hash, digest filtering, and byte confirmation are defined.
- m0-m5 algorithms, candidate completeness, REP overlay, alignment, and boundary rules are defined.
- Candidate identity, exact key/metadata encodings, insertion ordinals, visibility, query filtering, visible `SREPIDX1` ordering, private `SREPQRY1` scratch schemas/order IDs, bounded deduplication, lifecycle, and compaction are defined; the index-scratch clarification has independent review `PASS`.
- Layout-independent weighted normalization and fragment accounting are defined.
- Wire and operational limits, checksum selectors, and stable error codes are defined.
- Requirement extraction, stable IDs, manifest mapping, and CI traceability are defined.
- Future-LZ registration, collector, overlap period, cross-block state, spill, stdout, and atomic-file behavior are defined.
- Index-LZ history source, seekable sink, cross-block active matches, and stdout spool are defined.
- V2 ArchiveHeader, records, exact payload schemas, checksums, Summary, Trailer, cardinality, and termination are defined.
- Legacy checksum metadata, exact SREP-VHASH-128 construction with 24 independent vectors, byte-15 modulo-256 no-carry KDF, big-endian NH-key parsing, historical v4 binary cross-checks at 256/4095/4096/4097/8192 bytes, version matrix, no-checksum mutation rules, and split detection limits are defined.
- DataBlock trailing checksum covers domain bytes, frame, encoded payload, identity fields, and reconstructed bytes, with no CRC.
- Exhaustive first-detection error contract, coverage geometric mean over `old_covered>0` only, complete hard-unit coverage of material lists/formulas, length-prefixed list canonicalization, collision ID regex, complete public CLI/library configuration, duplicate-index `InvalidConfiguration` precedence, and `source_gap=0` same-source-position goldens are defined.
- The Section 11.9 version/layout compatibility table, the Section 11.2.2 256-byte historical-archive tuple table, the Section 11.2.2 long VHASH historical-archive tuple table, and the Section 11.10 preserved-versus-improvement table are marked extractable `kind="table"` hard-units. Each emits one `table-schema` unit plus one `table-row` unit per body row. `table-schema` bytes bind heading-local ordinal among all parsed Markdown tables under that heading, headers, alignments, body-row count, source order, and the full 32-byte SHA-256 of each standalone row payload. Changing a header cell, alignment, ordinal, row order, membership, or member-row content changes the schema ID; an unchanged row keeps its standalone `table-row` ID.
- Canonical payloads are specified as arbitrary hashed byte sequences represented in the exact manifest schema as `canonical_payload_hex`; extractor_version is `"6"`; accepted kinds include `table-schema` and `table-row`; heading is diagnostic UTF-8 only; `normalized_text` is not part of the exact schema; required goldens include the unchanged 240-byte `c8 00 00 00` `table-row` JSON hex round-trip, the synthetic membership-binding `table-schema` construction and its reorder/header-modified SHA/IDs, and uppercase/odd/nonhex/mismatch hex rejects. Manifest validation MUST recompute schema membership from the AST and MUST NOT accept a stale schema solely because `spec_sha256` was updated.
- Structured errors, semantic tests, exact matrix assertions, corpus, metrics, and fidelity formulas are defined.
- Staged delivery, final gates, acceptance evidence, and explicit non-goals are defined.
- The observed baseline and split-index evidence are labeled as observations rather than current implementation claims.
- The prior broad design, v4 observable-error clarification, overlay-minimum clarification, and index-scratch clarification received independent design review `PASS`. Implementation completion is not claimed, and implementation evidence, tests, matrices, fidelity gates, and final implementation review remain outstanding.
<!-- /hard-unit -->

# SREP-NG v3 stream format

**Status:** normative v3 wire specification; the implementation is present and
under acceptance verification, but this document does not claim release
verification or final review approval. Current software reads and writes this
format through the v3 integration paths. This document defines a distinct NG
v3 wire version; it does not mutate the frozen historical v2 design, whose
retired wire and compatibility surface are described in Section 1.

The algorithm and resource-semantics rules shared with the older design are
inherited from the frozen historical specification
[`superpowers/specs/2026-08-30-srep-capability-fidelity-design.md`](superpowers/specs/2026-08-30-srep-capability-fidelity-design.md),
especially its Sections 4, 6, and 8. That document remains the authoritative
baseline for matching algorithms and resource accounting, but it is a frozen
historical design: its NG v2 wire format, NG v2 reader/writer, and any v2
compatibility surface are retired and are not part of this v3 contract. This
document inherits only the algorithm semantics and shared resource rules, never
the old v2 codec or its compatibility promises. [`FORMAT.md`](FORMAT.md) is the
retired NG v2 wire reference, retained for history and marked historical; it is
not a current normative source. This document is authoritative only for
the v3 bytes and the v3 layout-delivery rules stated below. English field
names, byte tables, grammar, and MUST statements are normative. 中文说明只
帮助理解，不增加字段或语义；字段名、顺序、长度和校验规则以英文规范为准。

## 1. Compatibility, layouts, and I/O

The eight-byte magic `SREPNG3\0` selects v3. The dispatcher MUST continue to
read the historical SuperREP embedded-index archives in versions 1 through 4
read-only, and MUST recognize the retired NG v2 magic `SREPNG2\0` and reject it
as `UnsupportedVersion`. NG v2 is retired exactly like the experimental NG v1
magic, including the `.srep2` format: both recognized NG magics are rejected as
`UnsupportedVersion` and are never guessed, converted, decoded, or treated as
v3. The historical SuperREP version 2 is a distinct legacy container with a
different signature (see the historical boundary in
[`FORMAT.md`](FORMAT.md)) and remains readable under the legacy reader;
it is not NG v2. Any other signature is `CorruptHeader`.

V3 is one self-contained file. It has no permanent split index or sidecar, no
block directory, no current-index wrapper, no per-block range table, no
independent block-access directory, and no generic record wrapper. Its three
layouts are retained:

* **Index-LZ** (`layout=1`) stores raw literal bytes in logical blocks and one
  compact global match table after all blocks and before the fixed tail.
* **Future-LZ** (`layout=2`) stores source-owned registers before raw literals
  in each logical block.
* **I/O-LZ** (`layout=3`) stores destination-ordered inline operations.

All three layouts consume the same normalized original Match IR and preserve
the same original triples, semantic match count, covered bytes, and literal
bytes. A logical block is not a dictionary reset. Only m1 and m2 reset their
CDC state at each block, exactly as in v2. The default block size is **8 MiB**;
the valid range is **1 KiB through 1 GiB**, inclusive. The final block may be
shorter, and empty input has zero blocks.

For decoder I/O, only Index-LZ requires the tail and index to be obtained before
block reconstruction. A seekable input seeks to the tail; a non-seekable Index
input is first copied into a private input spool. Future-LZ and I/O-LZ parse
their block bodies sequentially and then read the fixed tail; they MUST NOT
force an input spool merely to find the tail. This input-spool choice is
separate from the seekable history sink needed for overlapping copies.

Index-LZ and I/O-LZ decoding to a non-seekable output, including stdout, uses a
private seekable verified output spool before any public bytes are copied.
Future-LZ MAY send a block to stdout after its body, plaintext delivery, and
CRC32C have passed; a later global digest failure can therefore occur after
partial stdout output. File output for every layout is staged and atomically
published only after every required check succeeds.

## 2. Conventions, limits, and inherited matching

All fixed-width unsigned integers are little-endian. V3 variable integers are
shortest ULEB128: seven payload bits per byte, least-significant group first,
bit 7 set on every nonfinal byte. Values below `2^7` use one byte, below
`2^14` use two, and so on. `MAX_WIRE = 2^63-1`; it fits in at most nine bytes.
A tenth byte, a continuation after byte nine, a value above `MAX_WIRE`, or a
non-shortest encoding is invalid. Every arithmetic operation, conversion,
offset, length, and endpoint is checked.

`MAX_WIRE` bounds positions, lengths, counts, section lengths, archive length,
and all derived offsets. A valid selected match has `src < dst`,
`len >= effective_min_match`, `len > 25` (the v2 normalizer's positive-gain
rule), checked `dst+len` within the input, and valid overlapping LZ source
bytes. The number of selected triples therefore satisfies the explicit bound

```text
M <= floor(T / effective_min_match)
M <= floor(T / 26)
```

for plaintext length `T`, with `M=0` when `T=0`. This is a decoder sanity
bound, not a candidate cap. The decoder validates actual triples, destination
non-overlap, source history, coverage, and all represented bytes; it does not
rerun the finder and does not infer finder provenance.

V2 m0 through m5 algorithms remain unchanged: m0 keeps representative
selection and independent backward extension; m1 and m2 keep their distinct
CDC state machines; m3 and m4 keep fixed-grid and reread rules; m5 keeps its
checked derived seed, eight-slice filter, exact confirmation, and extension.
Complete logical history is the default for every method. A nonzero
`max_distance` is an explicit inclusive distance restriction. The REP overlay
and effective-minimum rule are those of v2 Section 4.3. RAM and temporary-disk
budgets MUST NOT shorten history, reduce matching completeness, change a method,
or change the normalized IR.

## 3. ArchiveHeader: exact 80 bytes

The fixed header is the sole persistence location for global method, layout,
hash selection, block size, minimum, seed, target, distance, REP parameters,
and plaintext length. Counts and block locations are derived from these fields
and the body/index; they are not repeated in a summary record or every block.
This does not prohibit a layout delimiter: Future-LZ requires a per-block
`register_count` to delimit its register section, while Index-LZ and I/O-LZ
derive item boundaries from the validated index or expected output interval.

| Offset | Size | Field | Required value/meaning |
|---:|---:|---|---|
| 0 | 8 | `magic` | ASCII `SREPNG3\0` |
| 8 | 1 | `version` | `3` |
| 9 | 1 | `flags` | zero; no flags defined |
| 10 | 1 | `checksum_id` | `1` XXH3-128 or `2` BLAKE3-256 |
| 11 | 1 | `layout` | `1` Index-LZ, `2` Future-LZ, `3` I/O-LZ |
| 12 | 1 | `method` | `0..=5` |
| 13 | 1 | `semantic_flags` | bit 0 REP overlay; bits 1--7 zero |
| 14 | 2 | `reserved` | zero |
| 16 | 8 | `block_size` | 1 KiB through 1 GiB, inclusive |
| 24 | 8 | `minimum_match` | positive, v2 method-validated |
| 32 | 8 | `seed_size` | m0=`minimum_match`, m1=`48`, m2=`0`, m3/m4=`L`, m5=derived `L` |
| 40 | 8 | `target_chunk` | positive for m1/m2, zero otherwise |
| 48 | 8 | `max_distance` | zero means complete history |
| 56 | 8 | `rep_distance` | zero unless REP overlay is enabled |
| 64 | 8 | `rep_min_match` | zero unless REP overlay is enabled |
| 72 | 8 | `uncompressed_length` | plaintext length `T` |

The table is exactly 80 bytes; no header-length field is serialized. The
required byte value is `3` for I/O-LZ. `checksum_id=1` selects XXH3-128 with
seed zero and 16-byte digests. `checksum_id=2` selects unkeyed BLAKE3-256 and
32-byte digests. The same one selected algorithm is used for both global
digests.

For m1/m2, `target_chunk` defaults to 4096 and `seed_size` is fixed as shown.
For m3/m4/m5, REP parameters are positive when enabled and
`rep_region_size=max(1,floor(rep_min_match/8))` is derived, not serialized.
`rep_min_match` is `2..=1 GiB`; `rep_distance` and `max_distance` are at most
`MAX_WIRE`. The persisted `rep_distance` is only an m0 REP-overlay generation
cap and remains independent even when the effective overlay distance is
`min(rep_distance,max_distance)` for nonzero global `max_distance`. A decoded
base match is checked only against global `max_distance` (when nonzero), not
against `rep_distance`; base matches farther than the overlay generation cap
are valid. The writer/caller MUST validate all v2 method, minimum, seed, target,
distance, overlay, resource, and output options before I/O creation. A decoder
validates the header after reading its fixed 80 bytes and before interpreting
fields or allocating from them.

## 4. Archive order and fixed tail

The file order is:

```text
ArchiveHeader (80 bytes)
DataBlockBody 0 .. DataBlockBody N-1, each in logical block order
[IndexSection, Index-LZ only]
ArchiveTail (fixed width, immediately at EOF)
```

There are no MethodParameters, LayoutMetadata, ArchiveSummary, DataBlock
prefix, per-block length field, or other record. The body boundaries are
derived from the layout grammar and expected plaintext coverage. The Index
tail locator is used before Index-LZ bodies; Future-LZ and I/O-LZ body parsing
is sequential.

Let `w=16` for XXH3 or `w=32` for BLAKE3. The tail is exactly `44+2w` bytes:
76 bytes for XXH3 and 108 bytes for BLAKE3.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic ASCII `SREPNGT3` |
| 8 | 1 | `version` = `3` |
| 9 | 1 | `flags` = `0` |
| 10 | 2 | `reserved` = zero |
| 12 | 8 | `archive_length` |
| 20 | 8 | `index_offset`, zero unless Index-LZ |
| 28 | 8 | `index_total_length`, zero unless Index-LZ |
| 36 | 8 | `body_end` |
| 44 | `w` | `plaintext_digest` |
| `44+w` | `w` | `encoded_digest` |

All tail arithmetic is checked. Define `tail_start=archive_length-(44+2w)`;
underflow is invalid. `archive_length` MUST equal the physical file length.
For Index-LZ, `tail_start=archive_length-(44+2w)` is checked first;
`80 <= index_offset`, `body_end=index_offset`,
`index_total_length >= 1`, and

```text
index_offset + index_total_length = tail_start
```

For Future-LZ and I/O-LZ, `index_offset=0`, `index_total_length=0`, and
`body_end=tail_start`. The sequential body parser MUST consume exactly the
derived blocks and their CRCs, then read the fixed tail as the next structure;
it MUST NOT force an input spool merely to locate that tail. No IndexSection
may occur. The tail has no checksum of its own; the global encoded digest
protects its non-self bytes.

## 5. Per-block CRC32C and plaintext delivery

Every nonempty logical block has a body ending in exactly four bytes of CRC32C.
There is no DataBlock frame, physical length prefix, block ID field, or encoded
per-block hash. Block ID, `block_start=block_id*block_size`, and
`block_len` are derived. With `N=0` for `T=0`, otherwise the checked formula
`N=(T/block_size) + (T%block_size != 0 ? 1 : 0)` is used; every nonfinal block
has `block_len=block_size` and the
final block has `T-block_id*block_size` bytes. A block body may be fully covered
by references and then consist only of its CRC (or, for Future-LZ, its required
register metadata followed by CRC).

CRC32C parameters are:

* normal polynomial `0x1EDC6F41`;
* reflected polynomial `0x82F63B78`;
* `init=0xFFFFFFFF`, `xorout=0xFFFFFFFF`;
* reflected input and output; and
* four-byte little-endian serialization.

The CRC covers only the exact plaintext bytes delivered to that block, in
increasing absolute destination order. It contains no block identity, source
identity, domain label, operation, literal metadata, encoded digest, or
recovery information. A decoder MUST verify every CRC and MUST still perform
all global digest, length, count, and EOF checks. Empty input has zero blocks
and zero CRC fields.

## 6. Shared raw literal rule

Index-LZ and Future-LZ contain raw literal bytes only; there are no literal-run
headers, gap fields, or literal lengths on wire. The encoder computes maximal
uncovered target intervals after clipping them to each logical block and emits
their bytes concatenated in increasing target order. The decoder derives the
same intervals from validated matches, registrations, pending deliveries, and
the expected block interval, then consumes exactly that many bytes before CRC.
It MUST NOT scan literal bytes for a header. Zero literal bytes means the block
is fully covered. A literal interval cannot cross a block boundary; two literal
intervals clipped into different blocks are allowed to be adjacent across that
boundary, while adjacent literal operations within one I/O block are forbidden.

## 7. Index-LZ and compact global match table

An Index-LZ body is its derived raw literal-byte concatenation followed by
CRC32C. All selected matches occur exactly once in the global IndexSection.
Cross-block destination delivery remains one original triple and one active
copy state. There is no independent block-access table.

The IndexSection has no type, schema, wrapper, checksum, or table-length field,
and no per-block range array. Its exact bytes are:

```text
match_count ULEB128
triple[0 .. match_count)
```

`match_count=0` is exactly one byte `00`, so an empty IndexSection has length
one. The tail's `index_total_length` supplies the section's physical end and is
therefore at least one. The
decoder reads the shortest count, bounds it by `MAX_WIRE`, by the explicit
non-overlap bound in Section 2, and, when section bytes are available, by
`floor(available_section_bytes/3)` because each triple has at least three
one-byte fields. It then reads
exactly that many triples and requires the cursor to equal `tail_start`.

Triples are in canonical normalized order `(dst asc, src asc, len desc)` and
are each three shortest ULEB128 values:

```text
dst_gap  = dst - previous_match_end
distance = dst - src
total_len = len
previous_match_end = dst + len
```

The first `previous_match_end` is zero. `dst_gap=0` is valid only when the
new interval begins at the preceding interval's end; destination intervals
MUST still be non-overlapping. `distance>0`, `src=dst-distance`, all endpoints
are checked, and `len>=effective_min_match` and `len>25`. Duplicate triples,
out-of-order triples, impossible source history, overflow, overrun, or
underconsumed/trailing table bytes are `CorruptIndex` or `InvalidMatch` using
the existing error distinction. The zero-based origin ID is the canonical
triple position and is not a wire field.

For Index-LZ, the decoder obtains and validates this table first. It then
derives each block's literal byte length from the validated global matches and
consumes that exact byte count plus four CRC bytes. A preliminary boundary
check ensures each block cursor cannot pass `body_end`; after every body the
observed cursor MUST be `<= body_end`, and equality is required only after all
`N` bodies have been consumed. The final cursor MUST equal
`body_end=index_offset`.

For example, with `block_size=1024`, `T=1025`, and one partial final block, a
literal-only Index-LZ body starts at cursor 80, consumes 1024 literal bytes and
four CRC bytes, and ends at cursor 1108. The partial final body consumes one
literal byte and four CRC bytes, ending at cursor 1113. The one-byte empty
IndexSection starts at 1113, so `index_offset=body_end=1113` and the tail starts
at 1114. The first block's observed cursor is only an intermediate
`<= body_end` check; equality is required after all two bodies. Any fully
reference-covered middle block contributes exactly its four-byte CRC to the
body and no literal bytes.

## 8. Future-LZ registers, collectors, and pending state

Future-LZ is source-owned. A block body is:

```text
register_count ULEB128
register[0 .. register_count)
raw literal bytes (exact derived uncovered bytes)
CRC32C LE
```

Each register is exactly three shortest ULEB128 values:

```text
source_offset ULEB128
distance      ULEB128
total_len     ULEB128
```

The register belongs to the block containing its source start:
`src=block_start+source_offset`, `dst=src+distance`, and
`period_len=min(total_len,distance)` is derived. `source_offset < block_len`,
`src<dst`, source/destination endpoints, global `max_distance` semantics,
effective minimum, `len>25`, and
overlapping LZ validity are required. The source span may cross source-block
boundaries; ownership is by source start only.

`register_count` is a required syntactic delimiter, not a redundant global
summary. The decoder validates it before allocation and reads exactly that
many register triples. When the remaining physical body is known, it also
requires `register_count <= floor((remaining_bytes-4)/3)` when at least four
remaining bytes are known; during incremental streaming it applies the
corresponding `MAX_WIRE`, output, memory, and shared
temporary-budget limits while reading one register at a time. It MUST NOT
blindly preallocate from an untrusted count. After the count and registers,
the raw literal length is derived from target coverage and the body must have
exactly that many literal bytes followed by CRC. The parser applies a
preliminary remaining-byte check before each required register/literal/CRC
read and an observed cursor check after the body.

### 8.1 Installation and collectors

At the start of each block, the decoder MUST read, validate, and **install all
current-block registers before emitting any plaintext from that block**. This
includes any carried match delivery that would otherwise produce the first
byte. A register whose source begins in the current block is installed before
those source bytes are delivered. There is no prepass over future blocks.

Every register has a collector for the source period
`[src, src+period_len)`. The collector first obtains every period byte, across
source-block boundaries if necessary, and MUST mark the period complete before
the target delivery begins. Already-produced history bytes may be read through
the history sink, and every newly produced plaintext byte is then fed in
absolute order to **all active collectors**, not merely the collector that is
currently scheduled. This includes bytes produced by literals, carried
deliveries, and current-register deliveries. Thus a current register whose
source starts at a block start and whose bytes are produced by a carried
delivery is collected correctly: the register was installed before that carry.

Only after a collector's period is complete may its target bytes be delivered.
Delivery cycles through the collected period bytes, preserving the unchanged
overlapping LZ semantics. Each produced byte is appended to the history sink,
fed to all active collectors, and assigned to its absolute target position.
The block CRC is calculated over exactly the block's delivered plaintext after
all target bytes for that block are complete.

### 8.2 Canonical order, identity, and pending references

The encoder's physical register order is source order `(source_start asc,
destination asc, origin_match_id asc)`. Since `origin_match_id` is derived and
not serialized, the wire comparison projection is
`(owning_block_id asc, source_offset asc, destination asc, total_len desc)`;
identical triples are forbidden. The decoder may parse the stream sequentially
and retain metadata in bounded RAM or deterministic temporary spill. **After**
the stream has been consumed, it sorts the complete validated register set by
canonical destination order `(dst asc, src asc, len desc)` and derives IDs
`0..M-1`. It does not prepass future registers and does not use a wire ID.

The decoder validates that every register maps to one complete canonical
origin, with no duplicate, omitted, or incomplete represented target, and that
the resulting IR is destination-non-overlapping. This is actual metadata and
byte validation, not a claim that sorting can detect an externally intended
missing origin when the remaining set is tautologically sortable.

Pending state includes source, destination, total length, derived period,
current progress, collector progress, and collected period bytes. It may spill
only under the one shared temporary budget. Budget failure is explicit and
never drops a registration, weakens history, or changes the representation.
All current registers are installed before any current-block plaintext; all
pending references are completed or explicitly rejected at EOF.

## 9. I/O-LZ operations and match carry

I/O-LZ has no operation-count field. Its body consists of operations in strict
destination order until the current block's expected plaintext interval is
full, followed by CRC32C:

```text
literal: tag 0x00, literal_len ULEB128, literal_bytes[literal_len]
match:   tag 0x01, distance ULEB128, total_len ULEB128
CRC32C LE
```

There is no destination field, origin ID, continuation flag, generic operation
header, or repeated match length. Each new match writes `total_len` exactly
once. A new match starts at the current implicit destination, requires
`distance>0`, `src=dst-distance`, `src<dst`, `total_len>=effective_min_match`,
and `total_len>25`, and must satisfy all global endpoint and v2 source-history
checks. A literal length is positive and MUST fit entirely in the remaining
current block. Only a literal overshoot is intrinsically a block-body error.

When a new match reaches the end of the current block before its total length
is exhausted, the decoder copies exactly the smaller of remaining match bytes
and remaining block bytes, checks that block's CRC, and carries source,
destination, period, total length, and progress. The next block resumes that
active copy before parsing any new operation. A fully covered block with only a
carried match has no operation bytes, only CRC. A new match may overshoot a
block; its continuation is implicit. A new operation encountered while a
carried copy is unfinished is `CorruptRecord`.

Within one block, operations cover the destination interval exactly and in
strict order. Adjacent literal operations within one block are forbidden; the
canonical encoder emits one maximal uncovered literal operation after clipping
to that block. Literal operations in successive blocks may be adjacent because
clipping at a block boundary is intentional. A match continuation contributes
no new operation or fields. Two touching independent origins remain two
distinct tag-1 starts at operation boundaries, even when source and destination
endpoints touch.

## 10. Tail arithmetic, validation order, and errors

### 10.1 Writer order

The caller/writer MUST validate configuration, including all v2
method/layout/hash, minimum, seed, target, REP, block-size, max-distance,
resource, and output rules, before opening input or creating output. Decoder
header validation necessarily occurs after reading the fixed 80-byte header and
before interpreting its values or allocating from them. The writer then computes the complete
normalized IR, writes blocks/index/tail to private staging, computes both
digests over their exact domains, flushes and closes staging, and publishes a
file destination atomically. A write, resource, CRC, digest, or validation
failure MUST NOT publish partial file output.

### 10.2 Index decoder order

For Index-LZ, after header validation the decoder reads the fixed tail from
EOF (spooling non-seekable input first), checks `tail_start`, archive length,
flags, reserved bytes, width, and locator equations, then validates the entire
IndexSection count and triple stream. It performs these tail/index checks
before reading or reconstructing any block. It then derives each block interval,
parses the exact raw literal byte count plus CRCs, and requires the final body cursor
to equal `body_end=index_offset`. Every decoded distance is checked against the
global v2 `max_distance` rule when that header value is nonzero, inclusively;
no finder provenance is inferred and a base match may be farther than the
overlay's generation cap. A physically incomplete or absent Index-LZ tail is
not repaired or distinguished by speculative scans: natural EOF while reading
the required fixed tail is `TruncatedArchive`; otherwise an invalid or
indeterminate tail is `CorruptRecord`.

### 10.3 Future/I/O decoder order

For Future-LZ and I/O-LZ, the decoder validates the 80-byte header and all
header bounds first, then derives `N` with the checked formula in Section 5 and
parses exactly N sequential bodies. Future installs all current registers
before plaintext; I/O resumes active copy before new operations. Each body CRC
and exact output interval is checked. Only after the body parser reaches the
observed body boundary does the decoder read exactly `44+2w` tail bytes,
validate their magic, version, flags, reserved bytes, archive length, locators,
and global digests, require `body_end=tail_start`, and require physical EOF
immediately after the tail. No tail seek or input spool is required for this
order. Natural EOF while reading the required tail is `TruncatedArchive`; a
present or indeterminate malformed tail is `CorruptRecord`; a trailing byte
after a complete tail is rejected.

### 10.4 Preliminary and observed boundary checks

Every checked derived block start/end and body cursor is compared with a known
upper boundary when one is available. A preliminary minimum-byte check is made
before reserving or reading a required CRC or register/literal byte range, and
an observed cursor check is made after every block. Future-LZ reads and
validates each register triple before reserving that register's storage; a
declared count does not cause count-sized allocation. For Index-LZ, the
validated tail gives `body_end=index_offset` before parsing. For Future-LZ/I/O-LZ,
the header-derived N and layout grammar give each next boundary incrementally;
the final tail confirms the observed body end. EOF before bytes already
established as required is `TruncatedArchive`, not a guessed checksum result.

### 10.5 Existing structured errors

V3 uses only the existing `ErrorKind` values in `src/error.rs`: `InvalidConfiguration`,
`UnsupportedVersion`, `UnknownChecksum`, `CorruptHeader`, `TruncatedArchive`,
`CorruptRecord`, `CorruptIndex`, `InvalidMatch`, `ChecksumMismatch`,
`OutputLimitExceeded`, `MemoryBudgetExceeded`,
`TempBudgetExceeded`, `NonSeekableNeedsSpool`, `TemporaryStorageFailure`,
`InputIo`, `OutputIo`, and `AtomicPublish`, as applicable. In particular:

* recognized NG v1 and recognized retired NG v2 (`SREPNG2\0`) are
  `UnsupportedVersion`; an unknown checksum ID is
  `UnknownChecksum`; malformed fixed header fields are `CorruptHeader`;
* recognized `SREPNG3\0` magic with header `version != 3` is
  `UnsupportedVersion`. A supported v3 version with invalid flags, reserved
  bytes, or other fixed scalars is `CorruptHeader`; an unknown checksum ID
  remains `UnknownChecksum`;
* natural EOF while a required v3 structure is being read is
  `TruncatedArchive`; an invalid or indeterminate tail is `CorruptRecord`;
  invalid caller configuration is `InvalidConfiguration` and is distinct from
  malformed archive bytes;
* a present but malformed tail magic/version/flags/reserved/archive length or
  `body_end` is `CorruptRecord`; a present but inconsistent Index-LZ locator
  equation or IndexSection bytes, including nonzero Index locators on a
  non-Index layout, is `CorruptIndex`;
* malformed body grammar, operation/register counts, literal consumption,
  carry state, or CRC placement is `CorruptRecord`; invalid reconstructed
  match invariants use `InvalidMatch`;
* CRC or either global digest mismatch is `ChecksumMismatch`; and resource or
  I/O failures retain their existing operational kind.

Unknown flags/tags, nonzero reserved fields, duplicate sections, malformed
shortest-varints, zero-length items, truncation, underconsumption,
overconsumption, and trailing bytes are never silently ignored. Tail
classification is fail-closed: the reader performs no recovery, output-based
guessing, or byte-by-byte speculative scan to prove truncation. Only natural
EOF at the required read site produces `TruncatedArchive`.

The resource ledger is one shared current-reservation total. Before any
allocation or temporary-file growth, the writer/decoder reserves the checked
physical bytes for input spools, history, metadata, Future collectors and
pending state, output verification/staging, and atomic publication. RAII
ownership releases each reservation and cleans only paths created by that
operation on success, error, cancellation, or panic. `MemoryBudgetExceeded`,
`TempBudgetExceeded`, and `OutputLimitExceeded` are policy failures, not wire
errors and never authorize semantic degradation or partial file publication.

## 11. The exactly two global digests

The digest algorithm is selected once in the header. There is no separate
record/hash selector, no individual IndexSection hash, no block-encoded hash,
no identity hash, no authentication tag, and no recovery code. CRC32C remains
the only per-block checksum and covers plaintext only.

### 11.1 Plaintext digest

The plaintext digest is unkeyed with this exact domain:

```text
plaintext_digest = H("SREPNG3-PLAINTEXT\0" || plaintext[0..T])
```

The ASCII label including NUL is 18 bytes. `plaintext[0..T]` is the actual
ordered reconstructed plaintext, not a literal-only stream and not metadata.
This digest intentionally does not bind archive metadata; the encoded digest
does. The empty input domain is exactly the 18 bytes
`53 52 45 50 4e 47 33 2d 50 4c 41 49 4e 54 45 58 54 00`.

### 11.2 Encoded archive digest

The encoded digest covers every non-self physical archive byte exactly once:
the full 80-byte header, every DataBlock body and CRC, the complete IndexSection
when present, the first 44 bytes of the tail, and the plaintext digest bytes.
The exact non-circular domain is:

```text
encoded_digest = H(
    "SREPNG3-ENCODED\0" ||
    archive[0 .. tail_start + 44 + w]
)
```

The encoded label including NUL is 16 bytes. The archive slice ends immediately
after the plaintext digest and immediately before the encoded digest. It
includes the tail's `archive_length`, both index locators, `body_end`, and
the plaintext digest, but excludes only its own digest bytes. It therefore
binds every physical metadata and body byte without circularity. Hashing uses
the exact bytes on disk, not reconstructed equivalents.

For XXH3-128, serialize numeric output as low64 little-endian followed by
high64 little-endian. For BLAKE3-256, serialize the 32 digest bytes in digest
order. These digests detect accidental corruption and representation changes;
they are not authentication.

## 12. Structural examples

Examples in this section are valid structural examples, not claims that current
software implements v3. Digest values are supplied separately by the
independent vector evidence; no complete archive golden is invented here.

### 12.1 Empty archives

For `T=0`, `N=0` and body end is 80. An empty Index-LZ archive has a one-byte
IndexSection `00` at offset 80, `index_offset=80`, `index_total_length=1`,
`body_end=80`, `tail_start=81`, and archive lengths:

```text
XXH3:   80 + 1 + 76  = 157
BLAKE3: 80 + 1 + 108 = 189
```

An empty Future-LZ or I/O-LZ archive has no IndexSection, both index locators
zero, `body_end=tail_start=80`, and archive lengths:

```text
XXH3:   80 + 76  = 156
BLAKE3: 80 + 108 = 188
```

The plaintext digest hashes exactly the stated 18-byte label. The encoded
digest hashes the exact header, any empty index byte, the 44-byte tail prefix,
and that plaintext digest, excluding only the encoded digest field.

### 12.2 Legal `abc` no-match example

Use `block_size=1024`, method m1, `minimum_match=32`, `seed_size=48`,
`target_chunk=4096`, complete history, and `T=3`. There is one partial block.
The Index-LZ body is raw bytes `61 62 63` followed by CRC32C(abc), and its
IndexSection is exactly one byte `00`. The Future-LZ body is
`register_count=00`, raw bytes `61 62 63`, then CRC32C(abc). The I/O-LZ body
is one literal operation `00 03 61 62 63`, then CRC32C(abc). No layout has an
operation count or block prefix. All global digest and exact EOF checks remain
required.

### 12.3 Two touching independent origins

With `block_size=1024` and `T>=1152`, use valid positive 64-byte origins that
are distinct positive-gain starts whose intervals touch at the block boundary:

```text
(src=0,  dst=1024, len=64)
(src=64, dst=1088, len=64)
```

The first destination interval is `[1024,1088)` and the second is
`[1088,1152)`, so they touch at destination offset 1088 (not at the block
boundary 1024); their source intervals `[0,64)` and `[64,128)` likewise touch.
Neither origin is merged in this supplied canonical-IR/layout proof. This does
not claim that the normalizer always preserves both: if a merged candidate
`(src=0,dst=1024,len=128)` is present in the candidate pool, its larger v2
virtual gain wins; if that candidate is absent, these two triples may remain
selected. The Index triples
are `(dst_gap=1024,distance=1024,len=64)` and
`(dst_gap=0,distance=1024,len=64)`. I/O-LZ uses two tag-1 starts. A decoder
must preserve both canonical triples and their separate operation boundary.

### 12.4 Cross-block I/O carry

With `block_size=1024`, `T=3300`, and a selected match
`(src=0,dst=1000,len=2200)`, block 0 emits any literal prefix through 1000,
then one match start and 24 match bytes to its boundary. Blocks 1 and 2 are
fully covered by the carried match and therefore contain only CRC bytes. Block
3 begins at 3072: it resumes 128 carried match bytes, then emits a 100-byte
literal suffix, then CRC. No continuation fields or repeated total length are
present. This is legal because only a literal must fit its current block; the
match carries until its global end.

### 12.5 Future register whose source crosses blocks

With `block_size=1024` and `T>=2164`, install both registers in block 0 before
any plaintext is emitted:

```text
A: (source_offset=0,    distance=1000, total_len=1100)
   src=0,    dst=1000, len=1100, source period [0,1000)
B: (source_offset=1000, distance=1100, total_len=64)
   src=1000, dst=2100, len=64,   source period [1000,1064)
```

Register B's source period crosses the source-block boundary at 1024. Register
A's delivery produces the first 24 period bytes `[1000,1024)` in the remainder
of block 0 and the next 40 bytes `[1024,1064)` at the start of block 1. Because
B was installed before that carried delivery and every produced byte is fed to
all active collectors, B's period is complete before B's target begins in
block 2. The origins are distinct and all CRCs cover their exact block slices.
This demonstrates source collection across blocks and installation before
carried delivery without a Future prepass. A separate same-block overlapping
register may be `(source_offset=100,distance=20,total_len=64)`, giving source
period `[100,120)` and target `[120,184)` in block 0; it cycles its 20
collected bytes under the same overlap rule. The examples use absolute byte
intervals, so full-block coverage and every remaining literal byte offset are
derived and checkable without hidden framing.

### 12.6 CRC32C known vector

For ASCII `123456789`, bytes are `31 32 33 34 35 36 37 38 39`, numeric CRC32C
is `0xE3069283`, and wire bytes are `83 92 06 E3`.

## 13. Independent digest-vector and acceptance evidence

The implementation review MUST retain a small external vector artifact under
the controlled `/tmp/opencode` review directory. It must calculate both
plaintext domains (empty and `abc`) and both encoded domains for the concrete
empty Index-LZ and `abc` Index-LZ structures above, using the exact XXH3 seed-0
and BLAKE3 definitions. The artifact must record input-domain bytes, algorithm,
digest width, and digest bytes; it must not be presented as full codec evidence.
The existing official-C XXH3 fixture and existing BLAKE3 implementation may
be used only as hash primitives in this external mini-harness, without changing
Cargo files or repository fixtures. CRC32C(123456789) is independently checked
against Section 12.6.

The current review harness is separate from the codec and uses the already
resolved `twox-hash` and `blake3` libraries in a unique `/tmp/opencode`
project. It is hash-domain evidence only, not an independent implementation
of either hash and not full v3 codec verification; its successful command and
output are retained as review evidence, not as runtime v3 evidence.

The v3 acceptance matrix covers the full 3-layout × 2-algorithm matrix for:

1. empty input;
2. the legal `abc` no-match archive;
3. a partial final block and a multi-block literal/reference archive;
4. seekable and non-seekable input, with Index input spooling only where
    required and Future/I/O sequential parsing without forced input spooling;
5. seekable output, Index/I/O verified-spool output, and Future stdout late
   failure behavior;
6. touching independent origins, cross-block I/O carry, Future source-period
   collection across blocks, and current-register-before-carry installation;
7. CRC mutation, plaintext-digest mutation, encoded-digest mutation, header
   and tail locator mutation, malformed varints, zero/adjacent literal rules,
   bad counts, overrun, unresolved carry, and trailing bytes; and
8. dispatch: historical SuperREP v1–v4 read-only, retired NG v1 and retired
    NG v2 (`SREPNG2\0`) rejection as `UnsupportedVersion`, unknown checksum,
    unknown flags, and exact existing `ErrorKind` mappings. For every layout,
    include both
    `max_distance` equal to the encoded match distance and a just-beyond-limit
    case; for REP-enabled m3/m4/m5, include a valid base match farther than
    `rep_distance` while retaining the global distance rule.

For each layout and hash algorithm, the empty and nonempty cases MUST also
cover a non-seekable input/output path, natural EOF while reading the required
fixed tail, an invalid or indeterminate tail after the required bytes are
present, and a trailing byte after an otherwise complete tail. Natural EOF is
`TruncatedArchive`; an invalid or indeterminate tail is fail-closed as
`CorruptRecord`. The reader performs no recovery, body-size guessing, or
speculative byte scan to distinguish these cases. A metadata
mutation that leaves plaintext unchanged MUST be tested with a valid metadata
change: in the m1 `abc` specimen, change `target_chunk` from `4096` to `8192`
at header offset 40 as little-endian `u64`. Keep the body CRC, plaintext digest,
and stored encoded digest unchanged. Decoded plaintext, CRC, and recomputed
plaintext digest remain unchanged; recomputed encoded digest differs, so the
stored encoded digest MUST fail as `ChecksumMismatch`, not as a header error.
Run this case under both algorithms and every applicable layout.

The matrix separately mutates header byte 8 to version `4` and expects
`UnsupportedVersion`, and mutates the tail version to an invalid value and
expects `CorruptRecord`. This matrix is implementation acceptance criteria, not runtime evidence in this
document. No full v3 codec build, test, performance, xz, Windows, or macOS
claim is made by this specification.

## 14. Size accounting

Let `R_j` be the physical byte length of block `j`'s layout body plus CRC, and
let `I` be the IndexSection length (zero for Future-LZ/I/O-LZ). Then:

```text
archive_length = 80 + sum(R_j) + I + (44 + 2w)
```

Every nonempty block contributes exactly four CRC bytes. Index-LZ adds one
shortest ULEB128 count and three shortest ULEB128 values per original match.
Future-LZ adds one register count plus three shortest ULEB128 fields per
register. I/O-LZ adds one tag and shortest length/distance fields for each
literal or new match; match continuations add no fields. Raw literal bytes
occur exactly once. Empty Index-LZ has `I=1`; empty Future/I/O has `I=0`.

Compared with v2, v3 removes the v2 repeated common block header, BlockDirectory,
per-block literal headers, fixed-width Future registers, fixed-width I/O
operation fields, LayoutMetadata/ArchiveSummary records, and v2 trailer
wrappers. It adds one plaintext-only CRC32C per nonempty block and two fixed-tail
global digest values. This is a byte-accounting formula only; it makes no xz or
performance-success claim.

V3 is complete as a normative wire specification. Implementation acceptance
and release verification remain separate gates and are not claimed by this
document.

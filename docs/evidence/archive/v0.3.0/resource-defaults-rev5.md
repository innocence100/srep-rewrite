> Sanitized historical derivative; not a new verification or current status. See ../README.md and ../manifest.json for provenance and redactions. Embedded hashes identify original evidence/assets, not this derivative. Paths and commands are historical references, not replay-ready instructions.

# srep 0.3.0 — Automatic compression resource defaults (design proposal, rev 5)

Status: [redacted internal task] design/research. **No production code, no Git changes, no agents, no 0.3
execution** until the 0.2.1 base is published. Rev 5 resolves the single remaining [redacted internal task]
MED finding (staging/publish ownership). Revs 1–4 are superseded (clean rewrite).

Source of truth: accepted repository at `<workspace>/post-v0.2.0` (read-only),
`HEAD 1777d4728113e997af32f2d7a9c553de3b0b06a9`, clean. Manifest `package.version` is
still `0.2.0` (`tests/release_cli.rs:85`). Original dirty worktree `srep-rewrite` not read
or touched.

---

## 1. Scope and invariants

`srep compress` auto-selects `--memory` / `--temp-limit` when absent. Non-compress commands
and library `ResourceConfig::default()` stay fixed (256 MiB / 4 GiB). Explicit flags win.
No archive-format, version-metadata, quality-gate, algorithm, or collision/atomic-publish
change. Diagnostics are stderr-only; stdout archive bytes unaffected.

**"physical" = total physical RAM** (never current cgroup usage, never `MemAvailable`).
There is **no global resource guarantee** (§13).

---

## 2. Design decision table (rev 5)

| ID | Decision |
|---|---|
| R1 | Physical probe: `Known(u64>0)` \| `Invalid` \| `Unavailable`. Physical 0 never a base. |
| R2 | Hard probe: `{ identified, known_min, indeterminate }`. Finite cgroup **0 is a valid constraint**; `max`/v1 sentinel = no limit at that visible level; absent = no level. |
| R3 | v1 and v2 finite visible constraints aggregated by independent **minimum**; finite v1 never clears uncertain v2/child. |
| R4 | `identified && known_min==None && !indeterminate` = all examined visible levels readable and `max` → physical/2 (bounded by visible caps); **no hidden-ancestor / globally-unlimited promise**; never a finite `h`. |
| R5 | `indeterminate=true` when any expected visible level is missing/unreadable/malformed; retained even if a finite ancestor is known. |
| R6 | Indeterminate → `min(256 MiB, known_min/2 if any, physical/2 if Known)` + warning; never host physical/2. |
| R7 | Physical failure with a known finite hard limit → `min(256 MiB, hard/2)` + warning. |
| R8 | Result 0 (cgroup 0 / tiny finite half) → `CompressionConfig::validate()` → `SREP_E_INVALID_CONFIG`, no panic. |
| R9 | Temp known regular: `max(32 GiB, len*10)`; overflow → `InvalidConfig` naming `--temp-limit`, **before any staging creation**. No 32 GiB substitution. |
| R10 | Temp unknown (stdin/FIFO/special): fixed 32 GiB; stdin never fstat'd, even when redirected from a regular file. |
| R11 | Explicit `--temp-limit` bypasses multiplication/stat policy (open errors still surface); explicit `--memory` bypasses detection. Warnings are **per-resource**. |
| R12 | Same opened file handle fstat'd for size and read. |
| R13 | No free-disk startup reject; managed temp ledger and real write errors preserved. |
| R14 | **No root-visibility heuristic.** Missing expected file where legitimate global absence is unproven → `indeterminate` → conservative fallback always (ordinary host may fall back). Root detection not solved. |
| R15 | Diagnostics: `srep: resources: memory=<bytes> (<origin>) temp_limit=<bytes> (<origin>)` on stderr when `!quiet`; fallback warnings always on stderr. |
| R16 | `warning_for(failure, resolved_bytes)` embeds the actual resolved bytes; that resource's origin is `fallback`. |
| R17 | RLIMIT_AS unsupported; **no** termination/allocation-failure/whole-RSS promise; Windows/mac job limits unsupported and disclosed. |
| R18 | Library `ResourceConfig::default()` unchanged; harnesses must not misreport CLI auto values as fixed defaults. |
| **R19** | **The orchestrating helper owns only resolve→apply→validate→diagnostics; it does not own the sink. A transaction closure owns the reader/sink/`force` and runs the existing `temporary_output` + `finish_temporary_transaction` publish path unchanged. `compress_into` borrows `&mut sink`; the owned sink and the `Result` go to the existing finish/publish. No `StagingFactory` trait.** |

---

## 3. Contract

### 3.1 Probe states

```
PhysicalProbe = Known(u64>0) | Invalid | Unavailable
HardProbe     = { identified: bool, known_min: Option<u64>, indeterminate: bool }
```

No `RootVisibility`; path shape / mount root / namespace inode are not used (R14).

### 3.2 Memory resolution (no explicit `--memory`)

```
if !hard.identified:
    physical Known(p) -> (p/2,  auto,     no warning)
    else              -> (256 MiB, fallback, warning(physical_failure, 256 MiB))
else:
    phys_half = physical Known(p) ? Some(p/2) : None
    if hard.indeterminate:                                 # conservative (R6)
        v = 256 MiB
        if known_min Some(h): v = min(v, h/2)
        if phys_half Some(x): v = min(v, x)
        -> (v, fallback, warning(indeterminate, v))
    else if known_min Some(h):
        physical Known(p) -> (min(p,h)/2,        auto,     no warning)
        else              -> (min(256 MiB, h/2), fallback, warning(physical_failure, v))  # R7
    else:                                                  # all visible levels max (R4)
        physical Known(p) -> (p/2,               auto,     no warning)
        else              -> (256 MiB,           fallback, warning(physical_failure, 256 MiB))
```

`v == 0` → `CompressionConfig::validate()` fails with `SREP_E_INVALID_CONFIG` (R8), no panic.

### 3.3 Temporary resolution (no explicit `--temp-limit`)

```
Stdin | NonRegular                     -> 32 GiB
Regular, len.checked_mul(10)==Some(s)  -> max(32 GiB, s)
Regular, len.checked_mul(10)==None     -> Err(InvalidConfig(
      "input is too large to derive a temporary limit; specify --temp-limit"))   # R9
```

Raised during resolution, before the transaction closure runs.

### 3.4 Explicit flags / per-resource warnings

Explicit `--memory`: memory origin `explicit`, no memory warning (temp unaffected).
Explicit `--temp-limit`: temp origin `explicit`, no overflow; does **not** suppress an
automatic memory fallback warning.

---

## 4. Dependency (unchanged)

```toml
[target.'cfg(any(target_os = "windows", target_os = "macos"))'.dependencies]
system-memory = "=0.2.1"
```

Pinned published sources (docs.rs 0.2.1). `MIT`, `edition 2024` (implies rustc ≥ 1.85 only;
1.88.0 is an **open fact** requiring a real CI compile). Safe API:
`Snapshot::get() -> Result<Snapshot, Option<errno::Errno>>`,
`macos::try_get_total_physical_memory() -> Result<u64, Errno>`; the panicking
`total()/available()/used()` are never called. Internal `unsafe` like every FFI dependency.
New lock packages: `system-memory 0.2.1`, `mach2 0.6.0`, `rustc_version 0.4.1`,
`semver 1.0.27`; `errno 0.3.14`, `libc 0.2.189`, `windows-sys 0.61.2` already present.
Rejected: `sysinfo 0.39.6` (rust_version 1.95), `sys-info 0.9.1` (unmaintained/C/unsafe),
external commands. Attribution is a reported requirement, not guaranteed here.

---

## 5. Architecture and ownership

```
src/platform/
  mod.rs      # contract, cfg dispatch, HostProbe trait, production probe
  resolve.rs  # pure resolution + diagnostics
  linux.rs    # pure /proc parsers + bounded, path-checked cgroup collector
```

| Worker | Owns |
|---|---|
| **CLI CLI injector** | `src/main.rs`: explicit-flag retention, pre-open checks, single-open input, orchestration helper (§6) + injectable `HostProbe`/transaction closure, diagnostics, help text, binary-private orchestration tests |
| **platform platform tests** | `src/platform/**` parsers/collector/resolver + fixtures |
| **harness subprocess/harness** | `tests/auto_resources.rs`, CLI additions, `scripts/fidelity.py`, `scripts/large-file-roundtrip.py` |
| **dependencies deps/metadata** | `Cargo.toml`, `Cargo.lock`, `THIRD_PARTY.*`; audits |

Parent owns the version transition and native release after the 0.2.1 publication. Merge
order dependencies → platform → CLI → harness. No new requirements or quality gates.

---

## 6. Orchestration helper — actual ownership (R19)

**What was wrong before:** the helper called `StagingFactory.publish(sink, force)`, but the
compress step consumed the sink (`compress_into(... sink) -> Result<()>`), so no sink
remained to publish and the helper never actually published.

**Rev 5:** the orchestrating helper does **not** own the sink. It performs
resolve → apply → validate → diagnostics and then invokes a **transaction closure** which
owns the reader, the sink, and `force`, and runs the existing staging + publish path
unchanged.

```rust
// src/platform/mod.rs  (contract, unchanged from rev 4)
pub(crate) trait HostProbe { fn physical(&self) -> PhysicalProbe; fn hard(&self) -> HardProbe; }
pub(crate) fn resolve_compress_with(probe: &dyn HostProbe,
                                    explicit_memory: Option<u64>,
                                    explicit_temp_limit: Option<u64>,
                                    input: InputFacts) -> crate::Result<Resolved>;
pub(crate) fn memory_from_probes(physical: PhysicalProbe, hard: &HardProbe) -> (u64, Option<Failure>);
pub(crate) fn temp_from_input(kind: &InputKind, logical_len: Option<u64>) -> crate::Result<u64>;
pub(crate) fn warning_for(failure: Failure, resolved_bytes: u64) -> String;
```

```rust
// src/main.rs

/// Orchestration owns ONLY resolve->apply->validate->diagnostics.
/// `transaction` owns the reader + sink + force and performs the real publish.
pub(crate) fn run_compress_orchestration<F>(
    base_config: &CompressionConfig,
    explicit_memory: Option<u64>,
    explicit_temp_limit: Option<u64>,
    input: InputFacts,
    probe: &dyn HostProbe,
    quiet: bool,
    transaction: F,                        // FnOnce(&CompressionConfig) -> Result<()>
) -> crate::Result<()>
where
    F: FnOnce(&CompressionConfig) -> crate::Result<()>,
{
    let resolved = resolve_compress_with(probe, explicit_memory, explicit_temp_limit, input)?; // may Err; transaction untouched
    let mut config = base_config.clone();
    config.resources.memory = resolved.memory;
    config.resources.temp_limit = resolved.temp_limit;
    config.validate()?;                    // may Err; transaction untouched
    emit_diagnostics(quiet, &resolved);    // stderr only
    transaction(&config)                   // closure owns sink + existing finish/publish
}
```

Production call — the closure captures `reader`, `path`, and `force` by move and reuses the
existing `write_archive` / `compress_stream` transaction:

```rust
let transaction = move |config: &CompressionConfig| -> Result<()> {
    if output == OsStr::new("-") {
        compress_stream_with(config, options.quiet, reader, io::stdout().lock())   // no staging
    } else {
        write_archive(Path::new(&output), force, reader, config)                   // owns sink + publishes
    }
};
run_compress_orchestration(&options.config, explicit_memory, explicit_temp_limit,
                           input_facts, &HostOsProbe, options.quiet, transaction)
```

Existing transaction internals are **unchanged** (no atomic-publish redesign):

```rust
fn write_archive<R: Read>(path: &Path, force: bool, input: R, config: &CompressionConfig) -> Result<()> {
    let context = ResourceContext::with_resources(&config.resources)?;
    let (temp, writer) = temporary_output(path)?;              // owned sink
    let writer = BudgetedWriter::new(writer, &context.temp)?;
    finish_temporary_transaction(path, temp, writer, force, |writer| {   // operation borrows &mut writer
        compress_into(input, config, writer, &context)         // borrows; does NOT consume the sink
    })                                                         // finish_temporary OWNS writer + Result -> publish or cleanup
}
```

Ownership descriptor (precise):
- `transaction` is `FnOnce(&CompressionConfig) -> Result<()>`; it **owns** the reader, the
  destination path, `force`, and (internally) the `BudgetedWriter<NamedTempFile>` sink for
  its whole lifetime.
- The helper holds **no** sink and passes the finalized config by shared reference only.
- `compress_into(input, config, sink: &mut W, context: &ResourceContext)` **borrows**
  `&mut`; the borrow ends before `finish_temporary`, which **owns** the sink and the
  operation `Result`, then either persists (`force` from the closure; no separate force
  argument in the helper) or performs the existing failure cleanup. `compress_into` is a
  thin wrapper over the existing `compress_with_context`.
- On helper-side failure (resolve/validate), `transaction` is dropped uncalled; nothing is
  opened. No `StagingFactory` trait is introduced; the seam is the closure.

`run_compress` ordering (pre-open checks retained):

1. parse options; positional checks; `--force`/stdout checks;
2. if input path and output path: `reject_input_collision` + `path_exists(output)` checks
   **before** any input open (a blocking FIFO open cannot pre-empt them);
3. `open_input_once(path, explicit_temp.is_some())` → one open + fstat (R12);
4. build `input_facts`; call `run_compress_orchestration(..., transaction)`;
5. configuration semantic-flag validation (method/layout/checksum) unchanged.

The lower-level `compress_into` is allowed only as the borrowed-sink operation inside the
production transaction; tests must exercise `run_compress_orchestration` (not a fake
`Resolved`) for control flow.

---

## 7. Linux detection (no root heuristic)

### 7.1 Physical (pure)

Bounded read of `/proc/meminfo` (cap ~1 MiB); `MemTotal:` KiB → `checked_mul(1024)` →
`Known`; parsed 0 → `Invalid`; missing/non-numeric/overflow → `Unavailable`. Never
`MemAvailable`.

### 7.2 cgroup v1/v2 (visible levels only)

1. `parse_cgroup_self`: v2 `0::<path>` and/or v1 `memory` entries.
2. `parse_mountinfo`: cgroup2 / cgroup(memory) mounts; `mount_point`, `root`,
   octal-unescape `\040 \011 \012 \134`.
3. For each expected visible level (process path → mount root), bounded read of the limit
   file:
   - v2 `memory.max`: present integer → bytes (0 valid); present `"max"` → no limit at this
     level; missing/unreadable/malformed → `indeterminate`.
   - v1 `memory.limit_in_bytes`: present integer → bytes (0 valid); present sentinel
     (≥ 2^62, incl. `9223372036854771712`) → no limit at this level; missing/unreadable/
     malformed → `indeterminate`.
4. Aggregate: `identified` = any source; `known_min` = min finite across both v1 and v2;
   `indeterminate` = OR over expected visible levels, retained regardless of a finite value.
5. Outcomes: any missing/uncertain → conservative fallback (R6); else finite → `known_min`;
   else all readable `max` → limit-free visible (R4), physical/2 with no hidden-ancestor
   promise.

No path-shape/root/namespace inference. `cgroup.type`/`cgroup.controllers` not decisive.

Path checks (failure → that level `indeterminate`, never open outside the mount):
| Check | Rule |
|---|---|
| traversal | reject any `..` component |
| under-root | path must start with mount `root` (or be root-relative) |
| escape | `canonicalize` joined path must remain under the cgroup mount point |
| bounded | reject/limit reads exceeding the byte cap |
| trust | kernel files trusted; fixtures cover traversal, symlink escape, missing, oversized, escaped mount |

### 7.3 macOS / Windows / unsupported

- macOS: `system_memory::macos::try_get_total_physical_memory()`; `Ok(0)` → `Invalid`.
- Windows: `system_memory::Snapshot::get().map(|s| s.total)`; `Ok(0)` → `Invalid`;
  `total()` never called.
- Hard limit `identified=false` on Windows/macOS/other targets (disclosed, no false guarantee).
- RLIMIT_AS unsupported; **no** termination/allocation-failure/whole-RSS promise (R17).

---

## 8. Preserved behaviours

Collision detection, atomic publish/`finish_temporary*`/`BudgetedWriter`, `ResourceContext`
zero checks, non-compress defaults, `--force`/`--quiet`/duplicate handling, non-UTF-8 paths,
`--output-limit`, configuration semantic-flag validation, and help regression assertions are
unchanged.

---

## 9. Diagnostics (per-resource)

- Success (`!quiet`), stderr, one line: `srep: resources: memory=<bytes> (<origin>)
  temp_limit=<bytes> (<origin>)`, per-field origin ∈ `{auto, explicit, fallback}`.
- Warnings, stderr, always (even `--quiet`), bytes from R16:
  - `automatic memory detection failed (physical memory unavailable); using <bytes> B temporarily`
  - `automatic memory detection failed (invalid physical memory reading); using <bytes> B temporarily`
  - `container memory limit could not be determined; using <bytes> B temporarily`
- Per-resource: explicit `--temp-limit` does not suppress an automatic memory fallback
  warning; explicit `--memory` emits no memory warning even if temp is auto.
- `--quiet` suppresses the resources line and existing informational lines, not warnings.
  Diagnostics are stderr-only; stdout archive bytes unchanged.

---

## 10. Tests

No environment backdoor: injection via `HostProbe`, `InputFacts`, and the transaction
closure. No multi-EiB filesystem fixtures.

### 10.1 Pure resolution (`memory_from_probes`, `temp_from_input`)

| physical | hard | expected |
|---|---|---|
| `Known(64 GiB)` | `identified, known_min=4 GiB` | `2 GiB`, auto |
| `Known(4 GiB)` | `identified, known_min=2 GiB` | `1 GiB`, auto |
| `Invalid`/`Unavailable` | `identified, known_min=4 GiB` | `256 MiB`, fallback + warning |
| `Invalid`/`Unavailable` | `identified, known_min=100 MiB` | `50 MiB`, fallback + warning |
| `Invalid`/`Unavailable` | `identified, known_min=0` | `0` → validate fails |
| `Known(8 GiB)` | `identified, known_min=0` | `0` → validate fails |
| `Invalid`/`Unavailable` | `Absent` | `256 MiB`, fallback + warning |
| `Known(1)` | trusted/unlimited or finite | `0` → validate fails, no panic |
| `Known(u64::MAX)` | `Absent` | `u64::MAX/2`, no overflow |
| `Known(64 GiB)` | `indeterminate, known_min=None` | `256 MiB`, fallback + warning |
| `Known(256 MiB)` | `indeterminate, known_min=None` | `128 MiB`, fallback + warning |
| `Known(64 GiB)` | `identified, known_min=None, indeterminate=false` | `32 GiB`, auto, no warning |
| `Unavailable` | `identified, known_min=None, indeterminate=false` | `256 MiB`, fallback + warning |
| `Invalid` | `indeterminate, known_min=50 MiB` | `25 MiB`, fallback + warning |

Temp: `Regular,100 MiB`→32 GiB; `Regular,0`→32 GiB; `Regular,4 GiB`→40 GiB;
`Regular,8 GiB`→80 GiB; `Regular,u64::MAX`→`Err` naming `--temp-limit`;
`Stdin`/`NonRegular`→32 GiB. Warning bytes == resolved memory and origin `fallback`.

### 10.2 Collector fixtures (platform) — no path-only unlimited rows

| Fixture | Expected |
|---|---|
| visible root finite (2 GiB), process at root | `known_min=2 GiB`, `indeterminate=false` |
| visible root `max`, all levels readable `max` | `known_min=None`, `indeterminate=false` → physical/2 |
| missing root file | `indeterminate=true` → fallback |
| namespace self `/foo` with a missing expected level | `indeterminate=true` → fallback |
| missing child + finite ancestor (3 GiB) | `known_min=3 GiB`, `indeterminate=true` retained |
| visible level finite + another missing | `indeterminate`; `min(256 MiB, known/2, physical/2)` |
| finite v1 + uncertain/missing v2 | `indeterminate=true` |
| v1 sentinel + v2 absent | `known_min=None`, not indeterminate → physical/2 |
| traversal / symlink escape / oversized / escaped mount | `indeterminate` |
| meminfo present / missing / non-numeric / zero | Known / Unavailable / Unavailable / Invalid |

### 10.3 CLI orchestration tests (through `run_compress_orchestration`)

| Test | Setup | Assert |
|---|---|---|
| overflow stops staging | large `InputFacts.logical_len`, no explicit temp | helper `Err`; transaction opens **0**, publishes **0**; error names `--temp-limit` |
| explicit temp bypasses overflow | large length + `Some(temp)` | helper `Ok`; opens **1**, publishes **1** |
| zero auto budget → validation | probe `Known(1)`/`hard Known(0)` | helper `Err(InvalidConfig)`; opens **0**, publishes **0** |
| explicit memory skips detector | probe whose methods panic; `Some(mem)` | no panic; opens 1, publishes 1; memory origin `explicit` |
| compress failure | transaction returns `Err` after opening | opens 1, **publishes 0** (cleanup only) |
| publish failure | transaction returns `Err` at publish | opens 1, publishes 0, `Err` propagates; unchanged dest |
| fallback quiet warnings | probe `Unavailable`/`Absent`; `quiet=true` | resources line suppressed; memory warning bytes == resolved |
| mixed origins | `Some(mem)` + auto temp | `memory=… (explicit) temp_limit=… (auto)`, no memory warning |

The transaction test double counts `open`/`publish`; production uses the real
`write_archive`/`compress_stream_with`. Tests assert control flow only through the
orchestration helper, never a fabricated `Resolved`.

### 10.4 harness subprocess / harness

- Real CLI: ordinary file, stdin, FIFO (`#[cfg(unix)]`), explicit overrides, **stdin
  redirected from a regular file** (still unknown → 32 GiB).
- Assert the `srep: resources:` stderr line and round-trip; stdout archive unchanged;
  compress/publish failure preserves destination and leaves no staging.
- `scripts/fidelity.py`: comparison runs pass explicit fixed `--memory 256MiB` and fixed
  `--temp-limit`; `defaults` block reflects only library defaults; no quality-metric change.
- `scripts/large-file-roundtrip.py`: explicit fixed values, or bind reported fields to the
  actual resolved runtime values; remove any 256 MiB default claim when flags are omitted.
- Update success-stderr expectations for the resources line (version/help unaffected).

---

## 11. Cross-platform verification

Linux cannot produce Windows/macOS evidence. `.github/workflows/native-release.yml` runs 6
native jobs (ubuntu/windows/macos × MSRV `1.88.0`/`stable`) plus package smokes. **HARD
STOP:** no local cross-claim; the 1.88.0 build of `system-memory` is verified there, not
deduced from `edition 2024`. Requires parent-authorized Actions on a new 0.3.0 branch after
the 0.2.1 publication.

---

## 12. Open facts

1. Exact `license`/`repository` via `cargo metadata --locked`.
2. Whether `windows-sys` feature `Win32_System_SystemInformation` adds a crate.
3. Real 1.88.0 compile/build/test on linux/windows/macos (CI).
4. No total-only non-panicking Windows API in 0.2.1; recheck newer pins.
5. Exact v1 unlimited sentinel threshold; lock ≥ 2^62 with fixtures.
6. Target CI containers' visible-level layout (may force conservative fallback).

---

## 13. Acknowledged limitations (no global guarantee)

1. Best-effort; not a container/job/cgroup/RSS cap. Hidden ancestors unobservable;
   all-visible-`max` carries no hidden-ancestor promise.
2. Missing/unreadable expected level (including unproven legitimate global-root absence)
   forces conservative fallback; ordinary hosts may fall back.
3. Windows Job Object / macOS container limits unsupported.
4. RLIMIT_AS unsupported; no termination/allocation-failure/whole-RSS promise.
5. Indeterminate → 256 MiB may under-utilise.
6. `len*10` overflow fails by design.
7. `system-memory` single-maintainer risk; pinned and isolated.
8. Literal `mkfifo` classification may need a test-only dependency.
9. 32-bit targets: `u64` limits, no preallocation; primary targets 64-bit.

---

## 14. Estimate and checklist

dependencies S; platform M–L; CLI S–M; harness S–M; native validation CI-bound. ~3–5 engineer-days plus CI.

1. dependencies: target-gated pin + lockfile + `THIRD_PARTY.*`; audits.
2. platform: parsers/collector/resolver + fixtures (§10.2).
3. CLI: pre-open checks, single-open input, orchestration helper + transaction closure
   (R19 ownership), diagnostics, help text, §10.3 tests.
4. harness: CLI tests, stdin-redirect/FIFO/override, failure/publish tests, harness corrections.
5. Local Linux: fmt, check, clippy, `cargo test --bin srep`, targeted tests (no cross-claim).
6. Parent: 0.3.0 native-release branch, 6 jobs + package smokes, after 0.2.1 publication.

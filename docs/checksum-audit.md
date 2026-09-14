# Checksum implementation audit (SREP-NG v3; historical NGv2 vectors retained)

**Scope:** this document audits the `twox-hash` 2.1.4 implementation of
XXH3-128, which is the implementation selected in §10.10 for v2 checksum ID 1.
It also records the independent golden-vector evidence for both XXH3-128 and
BLAKE3-256. It does **not** perform a full unsafe audit of `blake3`; BLAKE3 is
covered here only as far as its official test vectors and v2 serialization are
pinned. A separate `blake3` dependency audit is out of scope for this branch
and is not claimed below.

The project's own sources use `#![forbid(unsafe_code)]`. That says nothing
about dependency safety: `twox-hash` contains `unsafe`, and this document
audits the XXH3-128 path on its own terms rather than inferring safety from the
project's own lint.

Design spec: `docs/superpowers/specs/2026-08-30-srep-capability-fidelity-design.md`
section 10.10.

## 1. Selected implementations and immutable pins

| Role | Crate / source | Version | Artifact hash | Features |
| --- | --- | --- | --- | --- |
| Production XXH3-128 | `twox-hash` | `=2.1.4` | `5283634e518fe9e82c7b20520bb4bc209009fd16c82077c802f8111ecbb0117a` (crates.io `.crate` sha256) | `default-features=false`, `std`, `xxhash3_128` |
| Production BLAKE3-256 | `blake3` | `1.8.7` (lock) | version/license/upstream listed in the existing `THIRD_PARTY.md` and `THIRD_PARTY.audit` at the repository root (no artifact hash recorded there) | default |
| Independent XXH3-128 reference | Cyan4973/xxHash `v0.8.2` | tag commit `bbb27a5efb85b92a0486cf361a8635715a53f6ba` | `xxhash.h` `be275e9d…99c43`, `xxhash.c` `685ac6e9…8e70` | official default secret, seed 0 |

The `twox-hash` version/features are declared in `Cargo.toml` and resolved in
`Cargo.lock`; this branch does not modify those files. The C reference pins and
the release tarball hash (`baee0c6a…79c4`) are recorded in
`scripts/checksum-provenance.txt` and re-verified on every golden check.

Serialization contract (both IDs): XXH3-128 is emitted as **low64
little-endian then high64 little-endian**, which is *not* the big-endian
`XXH128_canonicalFromHash` form. BLAKE3-256 is the raw 32-byte digest in digest
order. `src/checksum.rs::encode_xxh3` implements the split and is covered by
`tests/checksum_vectors.rs`.

## 2. `twox-hash` 2.1.4 unsafe inventory (XXH3-128 path)

Within the crates.io registry snapshot of the crate (`twox-hash-2.1.4/src/`),
79 lines contain the word `unsafe`; 72 of those are actual constructs
(48 `unsafe { … }` blocks, 15 `unsafe fn`, 7 `unsafe impl`, 2 `unsafe trait`)
and the remaining 7 are mentions in lints or comments. That count covers the
whole crate, including algorithms this project does not enable. Only the
subset reachable with `features = ["std","xxhash3_128"]` is in scope below.
The relevant blocks and their safety arguments:

### 2.1 `Secret` representation — `src/xxhash3/secret.rs`

- `Secret::new_unchecked` (line 33): `mem::transmute::<&[u8], &Secret>`.
  `Secret` is `#[repr(transparent)]` over `[u8]`, so the cast preserves
  layout. **Safety obligation:** caller guarantees `len >= SECRET_MINIMUM_LENGTH
  (136)`. `Secret::new` (line 16) performs that check before constructing, and
  `DEFAULT_SECRET` in `src/xxhash3.rs:62` is the 192-byte official secret, so
  the obligation holds for the production default path.
- `Secret::stripe` (line 64): `&*self.0.get_unchecked(i * 8..).as_ptr().cast()`.
  **Safety obligation:** `i < n_stripes()`. All internal call sites
  (`large.rs:203,267`, `streaming.rs:474`) iterate stripes in bounds; the
  function is `pub unsafe`, so the obligation is not propagated to safe code.
- `reassert_preconditions` (line 108): `assert_unchecked(self.is_valid())`.
  `assert_unchecked` is UB if the predicate is false; it is only reached after
  construction has validated the length, per the comment.

### 2.2 Slice chunking — `src/xxhash3.rs` `SliceBackport`

- `bp_as_chunks` / `bp_as_chunks_mut` / `bp_as_rchunks` (lines ~308–341) use
  `split_at_unchecked` / `split_at_mut_unchecked` followed by
  `slice::from_raw_parts[_mut]`.
  **Safety argument (documented inline):** the split index is `(len / N) * N`
  or `len - (len / N) * N`, which is always `<= len`; `N != 0` is asserted
  first; the element count passed to `from_raw_parts` is `len / N`; array and
  element alignment are identical. This is a backport of
  `slice::as_chunks`; the arithmetic cannot exceed the source length.

### 2.3 Streaming buffer — `src/xxhash3/streaming.rs`

- `unsafe { Secret::new_unchecked(secret) }` (line 105) on `Default`/`new`
  paths where the secret is the 192-byte default.
- The blocks at lines 216 and 309 assert the internal invariant
  `buffer_usage <= buffer.len()` via `assert_unchecked` (UB if violated); the
  value is maintained by the copy logic immediately around each assertion.
- The `get_unchecked_mut(..input.len())` at line 282 is guarded by the
  documented `input.len() < 2 * STRIPE_BYTES < buffer.len()` invariant.
- `split_at_unchecked` at line 265 mirrors the in-bounds reasoning in 2.2; the
  `FixedBuffer`/`FixedMutBuffer` unsafe traits (lines 10–34) require an
  `AsRef<[u8]>`/`AsMut<[u8]>` of exactly `N`, enforced by impls for
  `[u8; N]` and `Box<[u8]>` only where the length is fixed by construction.
- `secret.stripe(*current_stripe)` at line 474 is in bounds because
  `current_stripe` is derived from `n_stripes`.

### 2.4 SIMD dispatch — `src/xxhash3/large.rs` and `large/{avx2,sse2,neon}.rs`

- `do_sse2`/`do_avx2`/`do_neon` are `unsafe fn` with
  `#[target_feature(enable = "...")]`. The dispatch (lines 87–122) gates each
  call on `is_x86_feature_detected!("avx2"/"sse2")` (x86_64) or
  `is_aarch64_feature_detected!("neon")` (aarch64), or on an explicit
  `_internal_xxhash3_force_*` cfg used only by tests/benches.
  **Safety argument:** the generic function is only entered when the CPU
  feature was positively detected at runtime (or the build explicitly forced
  it), satisfying each function's `# Safety` contract.
- `Impl::new_unchecked` in `large/sse2.rs:14`, `large/avx2.rs:14`,
  `large/neon.rs:15` is called only from those feature-gated wrappers.
- `blocks.next_back().unwrap_unchecked()` (`large.rs:163`) relies on the
  caller having established a nonempty block iterator; used only after the
  length has passed the `> CUTOFF` checks.
- The scalar fallback (`large/scalar.rs`) contains one unsafe block, but only
  on `aarch64` (and not under Miri): an inline `umaddl` asm with
  `options(pure, nomem, nostack)` that computes `lhs32 * rhs32 + acc` from its
  arguments and writes only its output register. On every other architecture
  (including x86_64 and 32-bit targets) the safe `wrapping_mul`/`wrapping_add`
  path is used, so the fallback's unsafe surface is aarch64-only.

### 2.5 What this audit does *not* prove

These are code-reading + runtime-contract arguments, not a formal proof. In
particular:

- `assert_unchecked` and `unwrap_unchecked` are UB if their preconditions are
  ever violated; the audit relies on the crate's own invariants and on the
  upstream test suite.
- The audit was performed on the crates.io registry snapshot of `twox-hash`
  2.1.4 at the pinned `.crate` hash. A future `twox-hash` release is not
  covered.
- No Miri run has been recorded for `twox-hash` 2.1.4 in this branch (see
  section 5); Miri evidence is a CI follow-up.

## 3. Cross-platform output behaviour

XXH3 is defined over bytes and uses little-endian scalar loads plus
architecture-specific vector code. The Rust implementation:
- reads/writes through `u32::from_le_bytes` / `u64::from_le_bytes` (see
  `secret.rs` and the wave accumulators), so results are endian-independent;
- selects SSE2/AVX2/NEON at runtime behind feature detection and falls back to
  scalar (see §2.4), so the algorithm is *designed* to produce identical output
  across CPU feature levels;
- does not depend on pointer width for the output words (accumulators are
  `u64`; `totalLen` is a `u64` even on 32-bit targets).

The C golden generator is likewise compiled from the portable reference and
prints the low/high words little-endian explicitly, so the fixture is
byte-identical regardless of host endianness.

**What was actually tested.** On the host used for this branch (x86_64 with
AVX2 and SSE2), the Rust production path selected the AVX2 implementation, and
`tests/checksum_vectors.rs` verified it against the official C reference. This
establishes agreement between the official C reference and the dispatch that
this host selected. It does **not** prove that the scalar, SSE2, or NEON
variants produce the same output, because none of those variants was forced or
executed here. The crate provides `_internal_xxhash3_force_scalar`,
`_internal_xxhash3_force_sse2`, `_internal_xxhash3_force_avx2`, and
`_internal_xxhash3_force_neon` cfg hooks; running the vectors under each forced
variant is a CI follow-up (see §5) rather than a claim made by this document.

**Residual risk:** output equality across dispatch variants, 32-bit targets,
and big-endian targets is argued from the implementation's use of
little-endian loads and its runtime dispatch, but only the AVX2 path on a
64-bit little-endian host is observed here. See §5 for the evidence status.

## 4. Independent golden evidence

The portable entry point is `scripts/checksum-goldens.py` (standard library
only); `scripts/checksum-generate-goldens.sh` is a thin shell wrapper that
delegates to it. It has two modes:

- `--check` (the default): regenerate into a private temporary file and
  byte-compare against the committed fixture. Exit 0 only on an exact match,
  exit 2 otherwise. **It never writes the committed fixture**, so a CI step
  that regenerates and then tests cannot silently bless changed C output.
- `--write` / `--regenerate`: publish the generated bytes to the fixture path
  via a same-directory temporary file and atomic replace.

The pipeline:

1. fetches (or reuses a cached) official xxHash `v0.8.2` release tarball into
   the work directory (`--work-dir`, else `SREP_CHECKSUM_CACHE`, else
   `RUNNER_TEMP`, else a unique directory under `/tmp/opencode`),
2. verifies the tarball (bounded download, hashed before extraction) and both
   source files against the fixed SHA-256 pins,
3. compiles `scripts/checksum-generate-c-goldens.c` against that reference,
   selecting the C compiler from `--cc` / `SREP_CHECKSUM_CC` / `CC` / the usual
   `cl`, `clang`, `gcc`, `cc` search, and passing extra flags from `--cflags`
   (repeatable, shell-quoted) and `CFLAGS`,
4. emits the fixture bytes and either byte-compares (`--check`) or atomically
   publishes (`--write`) them.

The generator, for every vector, computes `XXH3_128bits` (oneshot) and
compares it to `XXH3_128bits_reset/update/digest` over 21 chunk sizes
(1,2,3,7,8,15,16,31,63,64,127,128,240,241,255,256,257,512,1024,4096,65536).
It aborts with a non-zero exit if any chunking disagrees. It then serializes
`low64.to_le_bytes() || high64.to_le_bytes()` and records the numeric low/high
words, the serialized bytes, and an FNV-1a-64 anchor of the input.

Coverage (51 vectors): profiles `xor` (nonuniform), `zeros`, `repeated`,
`descend`; lengths covering the canonical XXH3 cutovers
`0,1,2,3,4,8,15,16,17,31,32,33,63,64,65,127,128,129,239,240,241,255,256,257,
511,512,1023,1024,4096,65537,262144` (the last is a large multi-block
streaming case). Every length is below 2^20, so the fixture does not exercise
the reference's counters near a 32-bit `size_t` limit; 32-bit *execution* is a
separate CI concern (§5) and is not established by these lengths.

`tests/checksum_vectors.rs` (6 tests) parses this fixture and compares it
against the Rust production path:
- `fixture_has_broad_boundary_and_profile_coverage`
- `xxh3_oneshot_matches_independent_c_goldens`
- `xxh3_streaming_digest_matches_independent_c_goldens_across_chunkings`
- `encode_xxh3_matches_fixture_word_order`
- `xxh3_explicit_known_empty_vector_low_high_order`
- `blake3_official_vectors_and_v2_serialization`

The BLAKE3 cases use the official `(0..=250)`-cycle test vectors for lengths
0, 1, and 1024 and assert the exact v2 raw-digest serialization.

The fixture contains no host paths, host names, or timestamps; the only
provenance fields are the upstream project/tag/commit and source hashes. The
committed fixture hash is
`855e5431bb34fa40d85c1ed4b0c915d610cc27fceb480462142a2892d2b40ffe`.

### Reproducing locally (non-mutating by default)

```sh
scripts/checksum-generate-goldens.sh --check   # default; verifies, writes nothing
cargo test --locked --test checksum_vectors
```

Required tools: `python3` (stdlib only), a C compiler, and network access for
the first fetch (or an existing cached tarball with `--no-network`). The
compiler and flags are explicit:

```sh
# Linux: cc, and a separate word-size flag token
scripts/checksum-generate-goldens.sh --check --cc cc --cflags=-m32

# macOS: clang is the system compiler
scripts/checksum-generate-goldens.sh --check --cc clang

# Windows (after the VS developer environment is initialized): use cl
scripts/checksum-generate-goldens.sh --check --cc cl
```

## 5. Native evidence status (honest)

| Claim | Evidence in this branch | Status |
| --- | --- | --- |
| Rust XXH3-128 (AVX2 dispatch) == official C, 64-bit LE Linux | `cargo test --locked --test checksum_vectors` (6/6) | **Verified natively** |
| C oneshot == C streaming over 21 chunkings | generator exits 0; mismatch is fatal | **Verified natively** |
| low64-LE/high64-LE serialization | fixture invariant + Rust tests | **Verified natively** |
| BLAKE3-256 official vectors | `blake3_official_vectors_and_v2_serialization` | **Verified natively** |
| `--check` is non-mutating and detects drift | mutation test, §5.1 | **Verified natively** |
| Rust XXH3-128 scalar / SSE2 forced variants | not forced here | **Not yet — CI** |
| 32-bit target output | not run here | **Not yet — CI** |
| Big-endian target output | not run here | **Not yet — CI** |
| Miri over the dependency | not run here | **Not yet — CI** |
| Windows/macOS native output | not attempted locally (policy: native runner only) | **Not yet — CI** |

### 5.1 Non-mutating/mutation evidence

`scripts/checksum-generate-goldens.sh --fixture <copy>` returns exit 2 on a
mutated fixture and does not touch the committed file. Verified locally for a
one-nibble digest change and for a changed provenance `tag_commit`, in both
cases preserving the committed fixture hash
`855e5431bb34fa40d85c1ed4b0c915d610cc27fceb480462142a2892d2b40ffe`.

### 5.2 Platform/toolchain status

A cheap 32-bit compiler probe was performed: `cc -m32` can *derive* 32-bit
code but this host has no 32-bit runtime or headers, so both compiling the
generator and linking a trivial program fail
(`fatal error: bits/libc-header-start.h: No such file or directory`;
`cannot find Scrt1.o`). No Rust `i686`/big-endian target is installed either
(`rustup target list --installed` shows only `x86_64-unknown-linux-gnu` and
`x86_64-pc-windows-gnu`), and the Miri component is not installed. These are
therefore deferred to CI, not faked. Native Windows and macOS runs must go
through GitHub Actions (`windows-latest` / `macos-latest`); this branch does
not edit CI. The Python wrapper is standard-library only and uses `RUNNER_TEMP`
when present, but it has **not** been executed on Windows or macOS here; that
portability claim is untested and must be confirmed on the native runners.

### Handoff for the integration owner

Minimal integration command and required environment per runner:

```sh
# Any 64-bit runner with python3 and a C compiler (Linux/macOS):
scripts/checksum-generate-goldens.sh --check          # non-mutating verification
cargo test --locked --test checksum_vectors

# macOS: select clang explicitly if CC is not already set:
SREP_CHECKSUM_CC=clang scripts/checksum-generate-goldens.sh --check

# Windows: run after the VS developer environment is initialized so `cl` is on
# PATH; the wrapper auto-detects cl, or pass it explicitly:
#   scripts/checksum-generate-goldens.sh --check --cc cl
# RUNNER_TEMP is respected for the work directory; python3 must be available.
```

Required environment/tools: `python3` (stdlib), a C compiler
(`cl`/`clang`/`gcc`/`cc`), network access for the first fetch (or a cached
pinned tarball used with `--no-network`), and `RUNNER_TEMP` on hosted runners.
`--cflags` passes extra flags such as a word-size flag as separate tokens.

For 32-bit / big-endian / Miri / forced-dispatch coverage, add these to the
matrix (no repository CI change is made here):

```sh
# 32-bit target (needs a 32-bit toolchain / multilib):
cargo test --locked --test checksum_vectors --target i686-unknown-linux-gnu

# Big-endian target (needs a BE toolchain or cross runner):
cargo test --locked --test checksum_vectors --target s390x-unknown-linux-gnu

# Miri over the dependency, where available on the toolchain:
cargo +nightly miri test --locked --test checksum_vectors

# Forced dependency dispatch variants, e.g. scalar on x86_64:
#   RUSTFLAGS='--cfg _internal_xxhash3_force_scalar' cargo test --locked --test checksum_vectors
```

The C fixture is byte-order independent, so the same expected values apply on
every target; only the ability to *run* the target differs.

## 6. Caveats

- This branch does not add or change checksum algorithms; it adds independent
  evidence and this audit. The `twox-hash` dependency version and features are
  declared in `Cargo.toml`/`Cargo.lock`, which are untouched here.
- The project's `#![forbid(unsafe_code)]` applies to `srep`'s own sources and
  says nothing about dependency safety; the dependency audit above is the
  relevant evidence.
- No Miri, forced-dispatch, Windows, macOS, 32-bit, or big-endian run is
  claimed. The "not yet" rows are integration requirements, not completed
  checks.
- BLAKE3 is not fully audited here; only its official test vectors and v2
  serialization are pinned.

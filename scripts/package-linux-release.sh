#!/bin/sh
# Reproducible Linux release packager for SREP-NG 0.1.0.
# Stages the contracted tarball layout and honest NOTICES. Does not publish.
#
# Provenance rules (F1):
#   --build captures actual cargo/rustc/source at build time and binds it to
#   the produced binary SHA. --bin reuses an executable only with a sidecar
#   whose binary_sha256 matches, or with SREP_ALLOW_UNPROVENANCED_BIN=1
#   (candidate only). Packaging-time rustc/HEAD are labeled separately from
#   validated build provenance. Expected SREP_SOURCE_SHA mismatch rejects.
#   Dirty vs clean is observed; a clean tree is never labeled dirty.
#
# Archive rules (F2): tar and gzip statuses are both checked; outputs are
# written to *.tmp then renamed; a failed pipe leaves no publishable artifact.
set -eu

usage() {
    cat <<'EOF'
Usage: package-linux-release.sh [options]

  --repo DIR            Repository root (default: parent of this script)
  --bin PATH            Reuse an existing srep binary (requires provenance)
  --provenance PATH     JSON sidecar bound to the reused binary SHA
  --build               cargo build --release --locked --bin srep (fresh provenance)
  --out-dir DIR         Directory for tar.gz + SHA256SUMS
  --stage-dir DIR       Staging directory parent
  --stage-only          Stage layout + NOTICES; do not write the tarball
  --unpack-smoke        After tar exists, extract and run scripts/release-smoke.sh
  --skip-licenses       Omit crate + rust-std texts (tests only)
  --skip-rust-std       Omit rustc COPYRIGHT-library.html (tests only)
  --help                Show this help

Environment (typically from interface/env.sh):
  CARGO / CARGO_BIN, RUSTC / RUSTC_BIN, CARGO_HOME, CARGO_TARGET_DIR, TMPDIR
  SREP_ARTIFACT_DIR, SREP_VERSION, SREP_SOURCE_SHA, SREP_TARGET_TRIPLE
  SREP_ALLOW_UNPROVENANCED_BIN=1  candidate --bin without sidecar (tests/preview)
  SREP_TAR_BIN / SREP_GZIP_BIN    injectable tar/gzip (failure tests)
  SREP_RUSTC_SYSROOT              override rustc --print sysroot (tests)

Unpack smoke (after a real tarball exists; uses extracted binary, not target/):
  tar -tzf "$OUT/srep-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
  smoke=$(mktemp -d "${TMPDIR:-/tmp}/srep-unpack.XXXXXX")
  tar -C "$smoke" -xzf "$OUT/srep-v0.1.0-x86_64-unknown-linux-gnu.tar.gz"
  inner="$smoke/srep-v0.1.0-x86_64-unknown-linux-gnu/srep"
  sha256sum "$inner"
  SREP_RELEASE_BIN="$inner" sh "$REPO/scripts/release-smoke.sh"
  sha256sum -c "$OUT/SHA256SUMS"
EOF
}

repo=""
bin=""
provenance_path=""
do_build=0
out_dir=""
stage_dir=""
stage_only=0
skip_licenses=0
skip_rust_std=0
do_unpack_smoke=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --repo) repo=$2; shift 2 ;;
        --bin) bin=$2; shift 2 ;;
        --provenance) provenance_path=$2; shift 2 ;;
        --build) do_build=1; shift ;;
        --out-dir) out_dir=$2; shift 2 ;;
        --stage-dir) stage_dir=$2; shift 2 ;;
        --stage-only) stage_only=1; shift ;;
        --unpack-smoke) do_unpack_smoke=1; shift ;;
        --skip-licenses) skip_licenses=1; shift ;;
        --skip-rust-std) skip_rust_std=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *)
            printf '%s\n' "unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ -z "$repo" ]; then
    repo=$(CDPATH= cd -- "$script_dir/.." && pwd)
fi
repo=$(CDPATH= cd -- "$repo" && pwd)

version=${SREP_VERSION:-0.1.0}
triple=${SREP_TARGET_TRIPLE:-x86_64-unknown-linux-gnu}
base="srep-v${version}-${triple}"
artifact_name="${base}.tar.gz"

if [ -z "$out_dir" ]; then
    out_dir=${SREP_ARTIFACT_DIR:-"$repo/dist"}
fi
mkdir -p "$out_dir"
out_dir=$(CDPATH= cd -- "$out_dir" && pwd)

if [ -z "$stage_dir" ]; then
    stage_dir="$out_dir/staging"
fi
mkdir -p "$stage_dir"
stage_dir=$(CDPATH= cd -- "$stage_dir" && pwd)
payload="$stage_dir/$base"
rm -rf "$payload"
mkdir -p "$payload"

cargo_bin=${CARGO_BIN:-${CARGO:-cargo}}
rustc_bin=${RUSTC_BIN:-${RUSTC:-rustc}}
tar_bin=${SREP_TAR_BIN:-tar}
gzip_bin=${SREP_GZIP_BIN:-gzip}

resolve_exec() {
    candidate=$1
    if [ -x "$candidate" ]; then
        readlink -f -- "$candidate"
        return 0
    fi
    found=$(command -v "$candidate" 2>/dev/null || true)
    if [ -n "$found" ] && [ -x "$found" ]; then
        readlink -f -- "$found"
        return 0
    fi
    printf '%s\n' "$candidate"
}

bind_cargo_rustc() {
    chosen=$(resolve_exec "$rustc_bin")
    if [ ! -x "$chosen" ]; then
        printf '%s\n' "selected rustc is not executable: $rustc_bin" >&2
        exit 2
    fi
    rustc_bin=$chosen
    if [ -n "${RUSTC:-}" ]; then
        existing=$(resolve_exec "$RUSTC")
        if [ "$existing" != "$chosen" ]; then
            printf '%s\n' "RUSTC ($RUSTC -> $existing) conflicts with selected rustc ($chosen); refusing to record the wrong compiler" >&2
            exit 2
        fi
    fi
    export RUSTC=$chosen
}

packaging_rustc_version=$("$rustc_bin" --version 2>/dev/null || printf '%s\n' "unknown")
packaging_rustc_verbose=$("$rustc_bin" -vV 2>/dev/null || true)
packaging_host=$(printf '%s\n' "$packaging_rustc_verbose" | awk -F': ' '/^host:/{print $2}')
packaging_rustc_release=$(printf '%s\n' "$packaging_rustc_verbose" | awk -F': ' '/^release:/{print $2}')
packaging_rustc_commit=$(printf '%s\n' "$packaging_rustc_verbose" | awk -F': ' '/^commit-hash:/{print $2}')

head_sha="unknown"
git_describe="not a git checkout"
dirty="unknown"
commit_time="unknown"
if git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    head_sha=$(git -C "$repo" rev-parse HEAD)
    if git -C "$repo" status --porcelain --untracked-files=all | grep -q .; then
        dirty="dirty"
    else
        dirty="clean"
    fi
    commit_time=$(git -C "$repo" show -s --format=%cI HEAD)
    git_describe=$(git -C "$repo" describe --always --abbrev=12)
fi

if [ -n "${SREP_SOURCE_SHA:-}" ] && [ "$dirty" = "clean" ] && [ "$head_sha" != "$SREP_SOURCE_SHA" ]; then
    printf '%s\n' "expected source SHA ${SREP_SOURCE_SHA} does not match clean HEAD ${head_sha}" >&2
    exit 2
fi

provenance_kind="none"
build_command=""
build_rustc=""
build_rustc_release=""
build_rustc_commit=""
build_source_sha=""
build_worktree=""
build_note=""
built_unix=""
build_rustc_bin=""
build_rustc_sysroot=""

if [ "$do_build" -eq 1 ]; then
    bind_cargo_rustc
    packaging_rustc_version=$("$RUSTC" --version 2>/dev/null || printf '%s\n' "unknown")
    packaging_rustc_verbose=$("$RUSTC" -vV 2>/dev/null || true)
    packaging_host=$(printf '%s\n' "$packaging_rustc_verbose" | awk -F': ' '/^host:/{print $2}')
    packaging_rustc_release=$(printf '%s\n' "$packaging_rustc_verbose" | awk -F': ' '/^release:/{print $2}')
    packaging_rustc_commit=$(printf '%s\n' "$packaging_rustc_verbose" | awk -F': ' '/^commit-hash:/{print $2}')
    if [ -z "${CARGO_HOME:-}" ] || [ -z "${CARGO_TARGET_DIR:-}" ] || [ -z "${TMPDIR:-}" ]; then
        printf '%s\n' "CARGO_HOME, CARGO_TARGET_DIR, and TMPDIR must be set for an isolated build" >&2
        exit 2
    fi
    (
        CDPATH= cd -- "$repo" &&
            RUSTC="$RUSTC" "$cargo_bin" build --release --locked --bin srep
    )
    bin=${CARGO_TARGET_DIR}/release/srep
    provenance_kind="fresh-build"
    build_command="RUSTC=$RUSTC $cargo_bin build --release --locked --bin srep"
    build_rustc=$("$RUSTC" --version)
    build_rustc_release=$("$RUSTC" -vV | awk -F': ' '/^release:/{print $2}')
    build_rustc_commit=$("$RUSTC" -vV | awk -F': ' '/^commit-hash:/{print $2}')
    build_rustc_bin=$RUSTC
    build_rustc_sysroot=$("$RUSTC" --print sysroot)
    build_source_sha=$head_sha
    build_worktree=$dirty
    built_unix=$(date -u +%s)
    if [ "$dirty" = "clean" ]; then
        build_note="Fresh --build from clean HEAD ${head_sha} with the rustc recorded below."
    else
        build_note="Fresh --build from dirty worktree at HEAD ${head_sha}. Not a unique committed source SHA."
    fi
    if [ -n "${SREP_SOURCE_SHA:-}" ] && [ "$dirty" = "clean" ] && [ "$head_sha" != "$SREP_SOURCE_SHA" ]; then
        printf '%s\n' "fresh build source mismatch: HEAD ${head_sha} != SREP_SOURCE_SHA ${SREP_SOURCE_SHA}" >&2
        exit 2
    fi
elif [ -n "$bin" ]; then
    if [ ! -x "$bin" ]; then
        printf '%s\n' "srep binary is not executable: $bin" >&2
        exit 2
    fi
    binary_sha_now=$(sha256sum -- "$bin" | awk '{print $1}')
    if [ -n "$provenance_path" ]; then
        if [ ! -f "$provenance_path" ]; then
            printf '%s\n' "provenance sidecar missing: $provenance_path" >&2
            exit 2
        fi
        prov_json=$(python3 - "$provenance_path" "$binary_sha_now" <<'PY'
import json
import os
import re
import sys

path, actual_sha = sys.argv[1], sys.argv[2]
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")

try:
    data = json.loads(open(path, encoding="utf-8").read())
except (OSError, json.JSONDecodeError) as error:
    sys.stderr.write("provenance sidecar is not valid JSON: %s\n" % error)
    sys.exit(2)
if not isinstance(data, dict):
    sys.stderr.write("provenance sidecar must be a JSON object\n")
    sys.exit(2)

def require_str(key):
    if key not in data:
        sys.stderr.write("provenance missing required field %s\n" % key)
        sys.exit(2)
    value = data[key]
    if not isinstance(value, str) or not value.strip():
        sys.stderr.write("provenance field %s must be a nonempty string\n" % key)
        sys.exit(2)
    return value.strip()

def require_abs(key):
    value = require_str(key)
    if not value.startswith("/"):
        sys.stderr.write("provenance field %s must be an absolute path\n" % key)
        sys.exit(2)
    return value

prov_sha = require_str("binary_sha256")
if not HEX64.fullmatch(prov_sha):
    sys.stderr.write("provenance binary_sha256 must be 64 lowercase hex chars\n")
    sys.exit(2)
if prov_sha != actual_sha:
    sys.stderr.write(
        "provenance binary_sha256 %s does not match reused binary %s\n"
        % (prov_sha, actual_sha)
    )
    sys.exit(2)

src = require_str("source_sha")
if not HEX40.fullmatch(src):
    sys.stderr.write("provenance source_sha must be 40 lowercase hex chars\n")
    sys.exit(2)
expected = os.environ.get("SREP_SOURCE_SHA")
if expected is not None:
    if not HEX40.fullmatch(expected):
        sys.stderr.write("SREP_SOURCE_SHA must be 40 lowercase hex chars (nonempty)\n")
        sys.exit(2)
    if expected != src:
        sys.stderr.write(
            "provenance source_sha %s does not match SREP_SOURCE_SHA %s\n"
            % (src, expected)
        )
        sys.exit(2)

compiler = data.get("compiler") or data.get("rustc")
if not isinstance(compiler, str) or not compiler.strip():
    sys.stderr.write("provenance missing required field compiler\n")
    sys.exit(2)
compiler = compiler.strip()
if not compiler.startswith("rustc "):
    sys.stderr.write("provenance compiler must start with 'rustc '\n")
    sys.exit(2)
release = require_str("compiler_release")
commit = require_str("compiler_commit")
if not HEX40.fullmatch(commit):
    sys.stderr.write("provenance compiler_commit must be 40 lowercase hex chars\n")
    sys.exit(2)
worktree = require_str("worktree")
if worktree not in {"clean", "dirty"}:
    sys.stderr.write("provenance worktree must be 'clean' or 'dirty'\n")
    sys.exit(2)
command = require_str("build_command")
note = require_str("note")
compiler_bin = require_abs("compiler_bin") if "compiler_bin" in data else require_abs("rustc_bin")
sysroot = require_abs("compiler_sysroot")
built = data.get("built_unix", "")
if built != "" and not isinstance(built, (int, str)):
    sys.stderr.write("provenance built_unix must be an integer unix timestamp when present\n")
    sys.exit(2)
if isinstance(built, str) and built and not built.isdigit():
    sys.stderr.write("provenance built_unix must be an integer unix timestamp when present\n")
    sys.exit(2)

json.dump(
    {
        "build_command": command,
        "build_rustc": compiler,
        "build_rustc_release": release,
        "build_rustc_commit": commit,
        "build_source_sha": src,
        "build_worktree": worktree,
        "built_unix": "" if built == "" else str(built),
        "build_note": note,
        "build_rustc_bin": compiler_bin,
        "build_rustc_sysroot": sysroot,
    },
    sys.stdout,
    separators=(",", ":"),
)
print()
PY
        ) || {
            printf '%s\n' "provenance sidecar rejected" >&2
            exit 2
        }
        extract_prov() {
            printf '%s\n' "$prov_json" | python3 -c 'import json,sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1"
        }
        build_command=$(extract_prov build_command)
        build_rustc=$(extract_prov build_rustc)
        build_rustc_release=$(extract_prov build_rustc_release)
        build_rustc_commit=$(extract_prov build_rustc_commit)
        build_source_sha=$(extract_prov build_source_sha)
        build_worktree=$(extract_prov build_worktree)
        built_unix=$(extract_prov built_unix)
        build_note=$(extract_prov build_note)
        build_rustc_bin=$(extract_prov build_rustc_bin)
        build_rustc_sysroot=$(extract_prov build_rustc_sysroot)
        provenance_kind="sidecar"
    elif [ "${SREP_ALLOW_UNPROVENANCED_BIN:-}" = "1" ]; then
        provenance_kind="unprovenanced-bin"
        build_command="not captured (reused --bin without sidecar)"
        build_note="UNPROVENANCED reused binary. Packaging-time rustc/HEAD below are NOT the validated compiler/source of this executable. Supply --provenance bound to the binary SHA, or --build from a clean revision."
        build_rustc="unknown (not bound to this binary)"
        build_rustc_release=""
        build_rustc_commit=""
        build_source_sha="unknown"
        build_worktree="unknown"
    else
        printf '%s\n' "--bin requires --provenance PATH bound to the binary SHA, or SREP_ALLOW_UNPROVENANCED_BIN=1 for a candidate" >&2
        exit 2
    fi
else
    printf '%s\n' "specify --build or --bin" >&2
    exit 2
fi

if [ ! -x "$bin" ]; then
    printf '%s\n' "srep binary is not executable: $bin" >&2
    exit 2
fi

cp -a "$bin" "$payload/srep"
chmod 0755 "$payload/srep"

required_docs="LICENSE README.md CHANGELOG.md THIRD_PARTY.md THIRD_PARTY.audit"
for name in $required_docs; do
    if [ ! -f "$repo/$name" ]; then
        printf '%s\n' "required package file missing: $repo/$name" >&2
        exit 2
    fi
    cp -a "$repo/$name" "$payload/$name"
done

binary_sha=$(sha256sum -- "$payload/srep" | awk '{print $1}')

source_claim=$head_sha
if [ "$dirty" = "dirty" ]; then
    source_note="Packaging worktree is dirty relative to HEAD ${head_sha}. Packaging-time HEAD is not a unique committed source SHA."
elif [ -n "${SREP_SOURCE_SHA:-}" ] && [ "$head_sha" = "$SREP_SOURCE_SHA" ]; then
    source_note="Packaging worktree is clean at accepted source SHA ${SREP_SOURCE_SHA}."
else
    source_note="Packaging worktree is clean at HEAD ${head_sha}."
fi

glibc_bound="not a dynamically linked ELF, or no GLIBC symbol versions found"
file_desc=$(file -b -- "$payload/srep" || true)
if command -v readelf >/dev/null 2>&1; then
    glibc_versions=$(readelf -W -s -- "$payload/srep" 2>/dev/null | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' | sort -u -V || true)
    if [ -n "$glibc_versions" ]; then
        glibc_max=$(printf '%s\n' "$glibc_versions" | tail -n 1)
        glibc_bound="dynamic GLIBC symbol versions observed; maximum GLIBC_${glibc_max} (runtime must provide at least this GNU libc symbol version)"
    fi
fi

utc_now=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
source_date=${SOURCE_DATE_EPOCH:-}
if [ -z "$source_date" ] && [ "$commit_time" != "unknown" ]; then
    source_date=$(date -u -d "$commit_time" +%s 2>/dev/null || true)
fi
if [ -z "$source_date" ]; then
    source_date=$(date -u +%s)
fi

notices="$payload/NOTICES"
{
    printf '%s\n' "SREP-NG Linux release notices"
    printf '%s\n' "=============================="
    printf '%s\n' ""
    printf '%s\n' "Product: SREP-NG"
    printf '%s\n' "Version: ${version}"
    printf '%s\n' "Binary name: srep"
    printf '%s\n' "Target: ${triple}"
    printf '%s\n' "Profile: release"
    printf '%s\n' "Locked: --locked"
    printf '%s\n' "Binary SHA-256: ${binary_sha}"
    printf '%s\n' "file(1): ${file_desc}"
    printf '%s\n' "Runtime libc bound: ${glibc_bound}"
    printf '%s\n' ""
    printf '%s\n' "Validated build provenance"
    printf '%s\n' "--------------------------"
    printf '%s\n' "Provenance kind: ${provenance_kind}"
    printf '%s\n' "Build command: ${build_command}"
    printf '%s\n' "Build compiler: ${build_rustc}"
    printf '%s\n' "Build compiler release: ${build_rustc_release}"
    printf '%s\n' "Build compiler commit: ${build_rustc_commit}"
    printf '%s\n' "Build rustc bin: ${build_rustc_bin}"
    printf '%s\n' "Build rustc sysroot: ${build_rustc_sysroot}"
    printf '%s\n' "Build source SHA: ${build_source_sha}"
    printf '%s\n' "Build worktree: ${build_worktree}"
    printf '%s\n' "Built unix: ${built_unix}"
    printf '%s\n' "Build note: ${build_note}"
    printf '%s\n' ""
    printf '%s\n' "Packaging-time metadata (not a substitute for build provenance)"
    printf '%s\n' "-------------------------------------------------------------"
    printf '%s\n' "Packaging UTC: ${utc_now}"
    printf '%s\n' "Packaging rustc: ${packaging_rustc_version}"
    printf '%s\n' "Packaging rustc release: ${packaging_rustc_release}"
    printf '%s\n' "Packaging rustc commit: ${packaging_rustc_commit}"
    printf '%s\n' "Packaging host triple: ${packaging_host}"
    printf '%s\n' "Packaging HEAD: ${head_sha}"
    printf '%s\n' "Packaging git describe: ${git_describe}"
    printf '%s\n' "Packaging worktree: ${dirty}"
    printf '%s\n' "Packaging source note: ${source_note}"
    printf '%s\n' "SOURCE_DATE_EPOCH used for archive mtime: ${source_date}"
    printf '%s\n' ""
    printf '%s\n' "License: MIT (see LICENSE); third-party SPDX map: THIRD_PARTY.md / THIRD_PARTY.audit"
    printf '%s\n' "Preview caveat: v0.1 preview. 72-case fidelity vs historical C srep is deferred until after publication (possible 0.1.1 patch)."
    printf '%s\n' "This NOTICES file does not contain its own SHA-256 or the tarball SHA-256 (those would be self-referential)."
    printf '%s\n' ""
} > "$notices"

if [ "$dirty" = "dirty" ]; then
    {
        printf '%s\n' "This packaging tree is dirty. Rebuild the tarball after the integration commit so the source SHA is a unique clean revision."
        printf '%s\n' ""
    } >> "$notices"
fi

if [ "$skip_licenses" -eq 0 ]; then
    if [ -z "${CARGO_HOME:-}" ]; then
        printf '%s\n' "CARGO_HOME must be set to harvest crate license texts" >&2
        exit 2
    fi
    python3 - "$repo" "$cargo_bin" "$CARGO_HOME" "$notices" <<'PY'
import json
import pathlib
import subprocess
import sys

repo = pathlib.Path(sys.argv[1])
cargo = sys.argv[2]
cargo_home = pathlib.Path(sys.argv[3])
notices = pathlib.Path(sys.argv[4])

metadata = json.loads(
    subprocess.check_output(
        [
            cargo,
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--manifest-path",
            str(repo / "Cargo.toml"),
        ],
        cwd=repo,
    )
)

license_names = (
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "LICENSE-MIT.md",
    "COPYRIGHT",
    "COPYING",
    "NOTICE",
    "UNLICENSE",
    "AUTHORS",
)

packages = []
for package in metadata["packages"]:
    if package.get("source") is None:
        continue
    packages.append(package)
packages.sort(key=lambda item: (item["name"], item["version"]))

lines = [
    "Third-party crate license and copyright texts",
    "---------------------------------------------",
    "",
    "The following texts are copied from crate sources in the isolated Cargo",
    "registry. SPDX identifiers alone are not a substitute for these texts.",
    "Registry crate count is not the complete redistributed-component set;",
    "Rust standard library / rustc runtime notices follow separately.",
    "",
]

missing = []
for package in packages:
    name = package["name"]
    version = package["version"]
    license_id = package.get("license") or "UNKNOWN"
    manifest = pathlib.Path(package["manifest_path"])
    crate_dir = manifest.parent
    lines.append(f"### {name} {version}")
    lines.append(f"SPDX: {license_id}")
    lines.append(f"manifest: {manifest}")
    authors = package.get("authors") or []
    if authors:
        lines.append("authors: " + "; ".join(authors))
    lines.append("")

    found = []
    if crate_dir.is_dir():
        for entry in sorted(crate_dir.iterdir()):
            if not entry.is_file():
                continue
            upper = entry.name.upper()
            if entry.name in license_names or upper.startswith("LICENSE") or upper in {
                "COPYRIGHT",
                "COPYING",
                "NOTICE",
                "UNLICENSE",
                "AUTHORS",
            }:
                found.append(entry)

    if not found:
        missing.append(f"{name} {version} ({crate_dir})")
        lines.append(
            "NO LICENSE/COPYRIGHT/AUTHORS file found in the crate source directory."
        )
        lines.append("")
        continue

    for path in found:
        text = path.read_text(encoding="utf-8", errors="replace")
        lines.append(f"---- {path.name} ----")
        lines.append(text.rstrip())
        lines.append("")

if missing:
    lines.append("Missing license files (recorded honestly, not skipped silently):")
    for item in missing:
        lines.append(f"- {item}")
    lines.append("")

lines.append(f"registry_crate_count={len(packages)}")
lines.append("")

notices.write_text(notices.read_text(encoding="utf-8") + "\n".join(lines) + "\n", encoding="utf-8")
if missing:
    sys.stderr.write(
        "warning: {} crate(s) lacked LICENSE/COPYRIGHT files; listed in NOTICES\n".format(
            len(missing)
        )
    )
PY
    {
        printf '%s\n' ""
        printf '%s\n' "Linux x86_64-unknown-linux-gnu applicability"
        printf '%s\n' "-------------------------------------------"
        printf '%s\n' "r-efi 6.0.0 is a getrandom optional dependency compiled only for"
        printf '%s\n' "cfg(all(target_os = \"uefi\", getrandom_backend = \"efi_rng\"))."
        printf '%s\n' "The Linux candidate binary does not link r-efi (NEEDED shared"
        printf '%s\n' "libraries are libc, ld-linux, libpthread, libgcc_s, and libdl;"
        printf '%s\n' "no r-efi/efi symbols). The crate tarball has no LICENSE* file;"
        printf '%s\n' "AUTHORS contains MIT, Apache-2.0, and LGPL-2.1+ grant text plus"
        printf '%s\n' "copyright. Because this Linux package does not redistribute r-efi"
        printf '%s\n' "object code, the LGPL alternative is not selected for the Linux"
        printf '%s\n' "binary. THIRD_PARTY.audit is an SPDX attribution checklist, not a"
        printf '%s\n' "certification that every legal requirement is satisfied."
        printf '%s\n' ""
    } >> "$notices"
else
    printf '%s\n' "Crate license texts omitted (--skip-licenses)." >> "$notices"
fi

if [ "$skip_rust_std" -eq 0 ] && [ "$skip_licenses" -eq 0 ]; then
    # COPYRIGHT must come from the rustc that built the binary, not a
    # coincidentally selected packaging rustc.
    sysroot=${SREP_RUSTC_SYSROOT:-}
    copyright_binding="test-override SREP_RUSTC_SYSROOT"
    if [ -z "$sysroot" ] && [ -n "${build_rustc_sysroot:-}" ]; then
        sysroot=$build_rustc_sysroot
        if [ "$provenance_kind" = "fresh-build" ]; then
            copyright_binding="fresh-build rustc sysroot (${build_rustc_bin:-unknown})"
        else
            copyright_binding="sidecar compiler_sysroot (validated build toolchain)"
        fi
    fi
    if [ -z "$sysroot" ] && [ -n "${build_rustc_bin:-}" ] && [ -x "$build_rustc_bin" ]; then
        sysroot=$("$build_rustc_bin" --print sysroot)
        copyright_binding="build rustc --print sysroot ($build_rustc_bin)"
    fi
    if [ -z "$sysroot" ] && [ -n "$build_rustc_commit" ] && [ "$build_rustc_commit" = "$packaging_rustc_commit" ]; then
        sysroot=$("$rustc_bin" --print sysroot)
        copyright_binding="packaging rustc (commit matches validated build rustc $build_rustc_commit)"
    fi
    if [ -z "$sysroot" ]; then
        printf '%s\n' "cannot bind COPYRIGHT-library.html: validated build rustc sysroot unknown and packaging rustc commit (${packaging_rustc_commit:-empty}) does not match build rustc commit (${build_rustc_commit:-empty})" >&2
        exit 2
    fi
    rust_lib_copy="$sysroot/share/doc/rust/COPYRIGHT-library.html"
    if [ ! -f "$rust_lib_copy" ]; then
        printf '%s\n' "rustc COPYRIGHT-library.html missing: $rust_lib_copy" >&2
        exit 2
    fi
    {
        printf '%s\n' "Rust standard library and rustc runtime components"
        printf '%s\n' "--------------------------------------------------"
        printf '%s\n' "Cargo.lock registry harvest does not include the statically"
        printf '%s\n' "linked Rust standard library or rustc-distributed runtime"
        printf '%s\n' "crates such as gimli (DWARF) and addr2line (backtrace)."
        printf '%s\n' "The following file is copied from the rustc sysroot of the"
        printf '%s\n' "validated *build* toolchain (not merely the packaging rustc):"
        printf '%s\n' "copyright_binding: ${copyright_binding}"
        printf '%s\n' "sysroot: ${sysroot}"
        printf '%s\n' "source: ${rust_lib_copy}"
        printf '%s\n' "build rustc: ${build_rustc}"
        printf '%s\n' "build rustc commit: ${build_rustc_commit}"
        printf '%s\n' "packaging rustc: ${packaging_rustc_version}"
        printf '%s\n' "packaging rustc commit: ${packaging_rustc_commit}"
        printf '%s\n' ""
        printf '%s\n' "---- COPYRIGHT-library.html ----"
        cat "$rust_lib_copy"
        printf '%s\n' ""
    } >> "$notices"
    python3 - "$notices" <<'PY'
import sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(encoding="utf-8", errors="replace")
lower = text.lower()
missing = [name for name in ("gimli", "copyright") if name not in lower]
if "addr2line" not in lower and "addr2line" not in text:
    # COPYRIGHT-library.html may name gimli while addr2line lives in COPYRIGHT.html;
    # still require an explicit runtime-component note in this file.
    if "addr2line" not in text:
        pass
if "gimli" not in lower:
    sys.stderr.write("NOTICES missing gimli from rustc COPYRIGHT-library.html\n")
    sys.exit(2)
if "copyright notices for the rust standard library" not in lower:
    sys.stderr.write("NOTICES missing rust standard library copyright heading\n")
    sys.exit(2)
PY
elif [ "$skip_licenses" -eq 1 ] || [ "$skip_rust_std" -eq 1 ]; then
    printf '%s\n' "Rust standard library COPYRIGHT-library.html omitted (test skip)." >> "$notices"
fi

chmod 0644 "$payload/NOTICES"
chmod 0644 "$payload/LICENSE" "$payload/README.md" "$payload/CHANGELOG.md" \
    "$payload/THIRD_PARTY.md" "$payload/THIRD_PARTY.audit"
chmod 0755 "$payload/srep"

manifest="$stage_dir/${base}.stage.json"
python3 - "$manifest" "$payload" "$bin" "$binary_sha" "$head_sha" "$dirty" "$stage_only" "$artifact_name" "$out_dir" "$provenance_kind" "$build_source_sha" <<'PY'
import json
import pathlib
import sys

(
    manifest,
    payload,
    original_bin,
    binary_sha,
    head_sha,
    dirty,
    stage_only,
    artifact_name,
    out_dir,
    provenance_kind,
    build_source_sha,
) = sys.argv[1:]
payload = pathlib.Path(payload)
names = sorted(path.name for path in payload.iterdir())
json.dump(
    {
        "payload": str(payload),
        "original_bin": original_bin,
        "binary_sha256": binary_sha,
        "packaging_head": head_sha,
        "packaging_worktree": dirty,
        "provenance_kind": provenance_kind,
        "build_source_sha": build_source_sha,
        "stage_only": stage_only == "1",
        "artifact_name": artifact_name,
        "out_dir": out_dir,
        "entries": names,
    },
    open(manifest, "w", encoding="utf-8"),
    indent=2,
    sort_keys=True,
)
print(manifest)
PY

printf '%s\n' "staged $payload"
printf '%s\n' "binary_sha256 $binary_sha"
printf '%s\n' "packaging_worktree $dirty HEAD $head_sha"
printf '%s\n' "provenance_kind $provenance_kind"

print_unpack_commands() {
    printf '%s\n' "unpack + release-smoke (extracted binary, not target/):"
    printf '%s\n' "  tar -tzf \"$out_dir/$artifact_name\""
    printf '%s\n' "  smoke=\$(mktemp -d \"\${TMPDIR:-/tmp}/srep-unpack.XXXXXX\")"
    printf '%s\n' "  tar -C \"\$smoke\" -xzf \"$out_dir/$artifact_name\""
    printf '%s\n' "  inner=\"\$smoke/$base/srep\""
    printf '%s\n' "  sha256sum \"\$inner\""
    printf '%s\n' "  SREP_RELEASE_BIN=\"\$inner\" sh \"$repo/scripts/release-smoke.sh\""
    printf '%s\n' "  sha256sum -c \"$out_dir/SHA256SUMS\""
}

if [ "$stage_only" -eq 1 ]; then
    printf '%s\n' "stage-only: tarball not created (defer until docs freeze / post-commit rebuild)"
    print_unpack_commands
    if [ "$do_unpack_smoke" -eq 1 ]; then
        printf '%s\n' "--unpack-smoke ignored with --stage-only (no tarball)" >&2
        exit 2
    fi
    exit 0
fi

tarball="$out_dir/$artifact_name"
sums="$out_dir/SHA256SUMS"
tmp_tar="$out_dir/.${artifact_name}.tar.tmp"
tmp_gz="$out_dir/.${artifact_name}.gz.tmp"
rm -f "$tmp_tar" "$tmp_gz"

fail_archive() {
    rm -f "$tmp_tar" "$tmp_gz"
    # Do not leave a publishable tarball or checksum from a failed pipe.
    printf '%s\n' "$1" >&2
    exit 2
}

# Deterministic gzip: no timestamp, no original name. Archive root equals $base.
# Check tar and gzip independently; write atomically.
COPYFILE_DISABLE=1
if ! "$tar_bin" \
    --sort=name \
    --owner=0 \
    --group=0 \
    --numeric-owner \
    --mtime="@${source_date}" \
    --format=gnu \
    -C "$stage_dir" \
    -cf "$tmp_tar" "$base"
then
    fail_archive "tar failed; not publishing $tarball"
fi
if ! "$gzip_bin" -n -9 -c "$tmp_tar" > "$tmp_gz"
then
    fail_archive "gzip failed; not publishing $tarball"
fi
if ! "$gzip_bin" -t "$tmp_gz"
then
    fail_archive "gzip -t failed; not publishing $tarball"
fi
layout=$("$gzip_bin" -dc "$tmp_gz" | "$tar_bin" -tf -) || fail_archive "tar list of staged gzip failed"
printf '%s\n' "$layout" | grep -q "^${base}/srep$" || fail_archive "tarball missing ${base}/srep"
printf '%s\n' "$layout" | grep -q "^${base}/NOTICES$" || fail_archive "tarball missing ${base}/NOTICES"
rm -f "$tmp_tar"
# Replace destination only after the staged gzip validated.
mv -f "$tmp_gz" "$tarball"
(
    CDPATH= cd -- "$out_dir" &&
        sha256sum -- "$artifact_name" > "${sums}.tmp" &&
        mv -f "${sums}.tmp" "$sums"
)

printf '%s\n' "wrote $tarball"
printf '%s\n' "wrote $sums"
print_unpack_commands

if [ "$do_unpack_smoke" -eq 1 ]; then
    smoke_root=${TMPDIR:-/tmp}
    smoke=$(mktemp -d "${smoke_root}/srep-unpack.XXXXXX")
    "$tar_bin" -C "$smoke" -xzf "$tarball"
    inner="$smoke/$base/srep"
    if [ ! -x "$inner" ]; then
        printf '%s\n' "extracted binary missing: $inner" >&2
        exit 2
    fi
    inner_sha=$(sha256sum -- "$inner" | awk '{print $1}')
    printf '%s\n' "unpack_smoke_bin=$inner"
    printf '%s\n' "unpack_smoke_sha256=$inner_sha"
    printf '%s\n' "unpack_smoke_cmd=SREP_RELEASE_BIN=$inner sh $repo/scripts/release-smoke.sh"
    set +e
    SREP_RELEASE_BIN="$inner" sh "$repo/scripts/release-smoke.sh"
    smoke_exit=$?
    set -e
    printf '%s\n' "release-smoke.sh exit=$smoke_exit"
    if [ "$smoke_exit" -ne 0 ]; then
        printf '%s\n' "release-smoke.sh failed with exit=$smoke_exit" >&2
        exit "$smoke_exit"
    fi
    (
        CDPATH= cd -- "$out_dir" &&
            sha256sum -c SHA256SUMS
    )
fi

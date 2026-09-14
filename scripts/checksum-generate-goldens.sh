#!/usr/bin/env bash
# Thin shell entry point for the independent XXH3-128 golden fixture.
#
# All real work lives in the portable Python wrapper
# scripts/checksum-goldens.py (stdlib only). This script exists so existing
# invocations and shell-based CI steps keep working; it must stay trivial and
# portable.
#
# Default behaviour is a NON-MUTATING check: the fixture is regenerated into a
# private temporary file and byte-compared against the committed fixture, so a
# "regenerate + test" CI step cannot silently bless changed C output. Use
# --write (or --regenerate) to publish.
#
# Usage:
#   scripts/checksum-generate-goldens.sh                 # check (default)
#   scripts/checksum-generate-goldens.sh --check         # check
#   scripts/checksum-generate-goldens.sh --write         # publish fixture
#   scripts/checksum-generate-goldens.sh --fixture PATH  # explicit artifact
#
# Compiler selection: --cc, else SREP_CHECKSUM_CC, else CC, else cl/clang/gcc/cc.
# Extra flags: --cflags '<flags>' (repeatable) or CFLAGS. Pass word-size/target
# flags as separate tokens (e.g. --cflags '-m32'), not inside CC.
#
# Work directory: --work-dir, else SREP_CHECKSUM_CACHE, else RUNNER_TEMP, else a
# unique directory under /tmp/opencode.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

PYTHON_BIN="${PYTHON:-python3}"
if ! command -v "${PYTHON_BIN}" >/dev/null 2>&1; then
    PYTHON_BIN="python"
fi

exec "${PYTHON_BIN}" "${SCRIPT_DIR}/checksum-goldens.py" "$@"

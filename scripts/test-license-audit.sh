#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/srep-license-audit.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

cp "$root/Cargo.toml" "$root/Cargo.lock" "$root/THIRD_PARTY.md" "$root/THIRD_PARTY.audit" "$tmp/"
SREP_AUDIT_ROOT="$tmp" sh "$root/scripts/audit-lock-licenses.sh" >/dev/null

sed '1s/0.7.8/0.0.0/' "$root/THIRD_PARTY.audit" > "$tmp/THIRD_PARTY.audit.bad"
mv "$tmp/THIRD_PARTY.audit.bad" "$tmp/THIRD_PARTY.audit"
if SREP_AUDIT_ROOT="$tmp" sh "$root/scripts/audit-lock-licenses.sh" >/dev/null 2>&1; then
    printf '%s\n' 'license audit negative test unexpectedly passed' >&2
    exit 1
fi

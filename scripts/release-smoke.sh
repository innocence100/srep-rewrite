#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
bin="$root/target/release/srep"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/srep-smoke.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

printf 'SREP Phase 1 release smoke\n' > "$tmp/input"
: > "$tmp/empty"
dd if=/dev/zero of="$tmp/multiblock" bs=1024 count=3 2>/dev/null

for layout in index future io; do
    for checksum in xxh3 blake3; do
        archive="$tmp/${layout}-${checksum}.srep"
        output="$tmp/${layout}-${checksum}.out"
        "$bin" compress --layout="$layout" --checksum="$checksum" --block-size=1KiB "$tmp/multiblock" "$archive"
        "$bin" info "$archive" > "$tmp/info"
        "$bin" test "$archive"
        "$bin" decompress "$archive" "$output"
        cmp "$tmp/multiblock" "$output"
    done
done

for layout in index io; do
    corrupt="$tmp/${layout}-corrupt.srep"
    cp "$tmp/${layout}-xxh3.srep" "$corrupt"
    size=$(wc -c < "$corrupt")
    truncate -s "$((size - 70))" "$corrupt"
    set +e
    "$bin" decompress "$corrupt" "$tmp/${layout}-corrupt.out" 2> "$tmp/${layout}-corrupt.error"
    status=$?
    set -e
    test "$status" -ne 0
    test ! -e "$tmp/${layout}-corrupt.out"
done

"$bin" compress "$tmp/input"
"$bin" decompress "$tmp/input.srep" "$tmp/explicit-output"
cmp "$tmp/input" "$tmp/explicit-output"
"$bin" decompress --force "$tmp/input.srep"

"$bin" compress - "$tmp/pipeline.srep" 2>/dev/null <<'EOF'
stdin/stdout pipeline
EOF
"$bin" decompress "$tmp/pipeline.srep" - > "$tmp/pipeline-output"
printf 'stdin/stdout pipeline\n' | cmp - "$tmp/pipeline-output"

printf 'SREPNG\000\001' > "$tmp/prototype.srep"
set +e
"$bin" decompress "$tmp/prototype.srep" "$tmp/rejected" 2> "$tmp/error"
status=$?
set -e
test "$status" -eq 3
grep 'SREP_E_UNSUPPORTED_VERSION' "$tmp/error" >/dev/null
test ! -e "$tmp/rejected"

for legacy in "$root"/tests/fixtures/legacy/v1-md5.srep "$root"/tests/fixtures/legacy/v3-md5.srep "$root"/tests/fixtures/legacy/v4-vmac.srep; do
    "$bin" test "$legacy"
    "$bin" info "$legacy" > "$tmp/legacy-info"
    out="$tmp/$(basename "$legacy").out"
    "$bin" decompress "$legacy" "$out"
    cmp "$root/tests/fixtures/legacy/original.bin" "$out"
done

future="$tmp/future-late.srep"
future_bad="$tmp/future-late-bad.srep"
future_output="$tmp/future-late.out"
"$bin" compress --layout=future --checksum=xxh3 --block-size=1KiB "$tmp/multiblock" "$future"
cp "$future" "$future_bad"
python3 - "$future_bad" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[-65] ^= 1
path.write_bytes(data)
PY
set +e
"$bin" decompress "$future_bad" - > "$future_output" 2> "$tmp/future-late.error"
status=$?
set -e
test "$status" -eq 11
grep 'SREP_E_CHECKSUM' "$tmp/future-late.error" >/dev/null
test ! -s "$future_output"

sidecar="$tmp/not-opened"
set +e
"$bin" compress --index="$sidecar" "$tmp/input" 2> "$tmp/sidecar-error"
status=$?
set -e
test "$status" -eq 4
test ! -e "$sidecar"

set +e
"$bin" compress --block-size=1B --index="$sidecar" "$tmp/input" 2> "$tmp/invalid-sidecar-error"
status=$?
set -e
test "$status" -eq 2
test ! -e "$sidecar"

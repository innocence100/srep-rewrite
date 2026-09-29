#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
generator=$root/scripts/generate-legacy-fixtures.py
target=${CARGO_TARGET_DIR:-$root/target}
case "$target" in /*) ;; *) target=$root/$target ;; esac
decoder=$target/debug/srep

usage() {
    printf '%s\n' \
        "usage: $0 --validate" \
        "       $0 --differential --old-binary ABS" \
        "       $0 --generate --old-binary ABS" >&2
}

require_old_binary() {
    case "$1" in /*) ;; *) printf '%s\n' 'old binary path must be absolute' >&2; exit 2 ;; esac
    [ -x "$1" ] || { printf '%s\n' 'old binary is not executable' >&2; exit 2; }
}

case "${1:-}" in
    --validate)
        [ "$#" -eq 1 ] || { usage; exit 2; }
        python3 "$root/scripts/validate-legacy-fixtures.py" --acceptance --decoder "$decoder"
        ;;
    --differential)
        [ "$#" -eq 3 ] && [ "$2" = "--old-binary" ] || { usage; exit 2; }
        old=$3
        require_old_binary "$old"
        python3 "$generator" --mode differential --project "$root" --old-binary "$old" --decoder "$decoder"
        ;;
    --generate)
        [ "$#" -eq 3 ] && [ "$2" = "--old-binary" ] || { usage; exit 2; }
        old=$3
        require_old_binary "$old"
        python3 "$generator" --mode generate --project "$root" --old-binary "$old" --decoder "$decoder"
        ;;
    *)
        usage
        exit 2
        ;;
esac

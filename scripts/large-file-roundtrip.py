#!/usr/bin/env python3
"""Default-m3 large-file roundtrip for the Linux release binary.

Recipe (default): 258 MiB deterministic mix — 1 MiB repeated block, 256 MiB
unique non-zero data, then the same 1 MiB block. Not sparse zeros.

CLI compress uses the binary defaults (m3 / index / xxh3). The driver never
passes -m, --method, --layout, or --checksum.

Resource budgets: the driver passes no --memory/--temp-limit by default, so a
0.3.0 binary auto-selects them. The report binds the *actual* resolved bytes
and origins from the `srep: resources:` compress stderr line instead of
claiming a fixed 256 MiB/4 GiB default. Passing --memory/--temp-limit logs
explicit budgets; a legacy binary without the diagnostic is labelled from its
`--version` fixed built-ins or reported `unresolved`.

This process does not self-timeout. Callers that need a wall clock must set
their own (integration may use 86400000 ms). Use --detach for durable
pid/status/logs.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

MIB = 1024 * 1024
GIB = 1024 * MIB
DEFAULT_SIZE = 258 * MIB
REPEAT_LEN = 1 * MIB
UNIQUE_LEN = 256 * MIB
RECIPE_ID = "repeat-1mib-separated-256mib-unique-v1"
REPEAT_SEED = b"srep-ng-v0.1.0-repeat-block\n"
UNIQUE_SEED = b"srep-ng-v0.1.0-unique-region\n"

#: 0.3.0 CLI resource diagnostic, emitted on compress stderr.  `origin` is one
#: of auto|explicit|fallback.  This is the only trustworthy source for the
#: budgets the binary actually applied when the driver passes no overrides.
#:
#: Parsing iterates `str.splitlines()` (so Windows `\r\n` is tolerated) and
#: fullmatches a single logical line, rather than relying on a multiline regex
#: that would silently miss a trailing `\r`.
_RESOURCES_LINE_RE = re.compile(
    r"srep: resources: memory=(\d+) \((auto|explicit|fallback)\) "
    r"temp_limit=(\d+) \((auto|explicit|fallback)\)"
)
_RESOURCES_PREFIX = "srep: resources:"
U64_MAX = 2**64 - 1

#: `srep --version` prints exactly one line: `srep <semver>`.
VERSION_LINE_RE = re.compile(r"^srep (\d+\.\d+\.\d+)\s*$")

#: Built-in fixed budgets for released binaries that predate 0.3.0 resource
#: diagnostics.  Deliberately a small explicit version lookup: an unknown
#: version reports `unresolved` rather than guessing a host-derived budget.
#: Never used to label a 0.3.0 run, whose actual values come from stderr; and
#: never used when a resources line was attempted but rejected (fail-closed).
KNOWN_BINARY_DEFAULTS = {
    "0.2.1": {
        "memory_bytes": 256 * MIB,
        "temp_limit_bytes": 4 * GIB,
    },
}

#: Mirror the CLI `parse_size` grammar exactly: no internal whitespace, binary
#: suffixes only (`B`, `K`/`KB`/`KiB`, `M`/`MB`/`MiB`, `G`/`GB`/`GiB`).  There
#: is no decimal and no TiB/T suffix.  `parse_size_bytes` also rejects u64
#: overflow like the CLI does.
_SIZE_RE = re.compile(r"^(\d+)(b|k|kb|kib|m|mb|mib|g|gb|gib)?$", re.IGNORECASE)
_SIZE_FACTORS = {
    "": 1,
    "b": 1,
    "k": 1024,
    "kb": 1024,
    "kib": 1024,
    "m": 1024**2,
    "mb": 1024**2,
    "mib": 1024**2,
    "g": 1024**3,
    "gb": 1024**3,
    "gib": 1024**3,
}


def parse_size_bytes(text: str) -> int | None:
    """Parse the CLI SIZE grammar to bytes, or None.

    Mirrors `srep`'s parser: no whitespace, binary suffixes only, u64 range.
    """
    match = _SIZE_RE.match(text)
    if match is None:
        return None
    unit = (match.group(2) or "").lower()
    value = int(match.group(1)) * _SIZE_FACTORS[unit]
    return value if value <= U64_MAX else None


def _resources_candidate_lines(text: str) -> list[str]:
    return [line for line in text.splitlines() if line.startswith(_RESOURCES_PREFIX)]


def assess_resources_stderr(text: str) -> str | None:
    """Fail-closed check of an attempted resources diagnostic.

    Returns None when there is no resources line at all, or exactly one valid
    line.  Returns a human-readable error when a line was attempted but is
    unusable: duplicate lines, malformed shape/origin, zero value, negative
    (which cannot match the shape), or a value beyond u64.  Callers must fail
    the measurement in that case rather than falling back to a known-version
    default.
    """
    candidates = _resources_candidate_lines(text)
    if not candidates:
        return None
    if len(candidates) > 1:
        return f"duplicate resources diagnostics ({len(candidates)} lines)"
    line = candidates[0]
    match = _RESOURCES_LINE_RE.fullmatch(line)
    if match is None:
        return f"malformed resources diagnostic: {line!r}"
    for name, raw in (("memory", match.group(1)), ("temp_limit", match.group(3))):
        value = int(raw)
        if value == 0:
            return f"{name} resources value is zero"
        if value > U64_MAX:
            return f"{name} resources value overflows u64: {value}"
    return None


def parse_resources_line(text: str) -> dict[str, int | str] | None:
    """Return the actual `srep: resources:` values, or None.

    Strict: exactly one well-formed line.  Use `assess_resources_stderr` first
    to distinguish "no line" from "attempted but invalid".
    """
    candidates = _resources_candidate_lines(text)
    if len(candidates) != 1:
        return None
    match = _RESOURCES_LINE_RE.fullmatch(candidates[0])
    if match is None:
        return None
    return {
        "memory_bytes": int(match.group(1)),
        "memory_origin": match.group(2),
        "temp_limit_bytes": int(match.group(3)),
        "temp_limit_origin": match.group(4),
    }


def parse_version_line(stdout: str) -> str | None:
    for line in stdout.splitlines():
        match = VERSION_LINE_RE.match(line)
        if match is not None:
            return match.group(1)
    return None


#: Verification scope per resolved_source.  Only an actually observed stderr
#: diagnostic counts as *verified*; explicit flags and a legacy version lookup
#: are truthful "configured"/"inferred" scopes but are NOT execution proof.
_VERIFICATION_SCOPE = {
    "compress_stderr_resources": "stderr_observed",
    "explicit_flags": "configured_flags",
    "known_binary_defaults": "legacy_version_lookup",
    "partial_explicit_flags": "partial",
    "unresolved": "unresolved",
    "invalid_diagnostic": "invalid",
}
_VERIFIED_SOURCES = {
    "compress_stderr_resources",
}


def build_resource_report(
    *,
    temp_dir: Path,
    memory: str | None,
    temp_limit: str | None,
    compress_stderr: str | None,
    binary_version: str | None,
) -> dict[str, Any]:
    """Describe the budgets the binary actually applied, honestly.

    Preference order:
    1. the 0.3.0 resources diagnostic on compress stderr (actual bytes/origins);
    2. for a binary without that line, explicit flags the driver passed;
    3. for a known legacy binary version and no flags, its fixed built-ins;
    4. otherwise `unresolved` with null bytes.

    The driver never claims it forced 256 MiB/4 GiB when it passed nothing, and
    never reports `verified: true` for a partial/unresolved measurement.  A
    resources line that was attempted but invalid must be rejected by
    `assess_resources_stderr` *before* this function is used; this function
    simply treats it as "no line".
    """
    resolved: dict[str, int | str | None] = {
        "memory_bytes": None,
        "memory_origin": None,
        "temp_limit_bytes": None,
        "temp_limit_origin": None,
    }
    parsed = parse_resources_line(compress_stderr) if compress_stderr else None
    if parsed is not None:
        resolved.update(parsed)
        resolved_source = "compress_stderr_resources"
        default_kind = None
    else:
        memory_bytes = parse_size_bytes(memory) if memory is not None else None
        temp_limit_bytes = parse_size_bytes(temp_limit) if temp_limit is not None else None
        # A flag that is unparseable or zero is not a valid explicit budget: the
        # CLI rejects it before running, so a mocked run must not report an
        # `explicit` origin (or zero bytes) for it.
        memory_valid = memory_bytes is not None and memory_bytes > 0
        temp_limit_valid = temp_limit_bytes is not None and temp_limit_bytes > 0
        if memory_valid:
            resolved["memory_bytes"] = memory_bytes
            resolved["memory_origin"] = "explicit"
        if temp_limit_valid:
            resolved["temp_limit_bytes"] = temp_limit_bytes
            resolved["temp_limit_origin"] = "explicit"
        if memory_valid and temp_limit_valid:
            resolved_source = "explicit_flags"
            default_kind = None
        elif memory_valid or temp_limit_valid:
            resolved_source = "partial_explicit_flags"
            default_kind = None
        elif binary_version in KNOWN_BINARY_DEFAULTS:
            known = KNOWN_BINARY_DEFAULTS[binary_version]
            resolved["memory_bytes"] = known["memory_bytes"]
            resolved["memory_origin"] = "binary_default"
            resolved["temp_limit_bytes"] = known["temp_limit_bytes"]
            resolved["temp_limit_origin"] = "binary_default"
            resolved_source = "known_binary_defaults"
            default_kind = f"binary_builtin_fixed_v{binary_version}"
        else:
            resolved_source = "unresolved"
            default_kind = "unresolved"

    # Observations must agree with the driver's own flags, both in bytes and in
    # origin: a provided flag REQUIRES `explicit`; an omitted resource must not
    # claim `explicit`, and the origin must be one the policy can actually
    # produce (memory: auto|fallback; temp: auto only -- temp has no fallback).
    mismatches: list[str] = []
    mismatch_detail: list[str] = []
    if parsed is not None:
        memory_ok = True
        if memory is not None:
            flag_bytes = parse_size_bytes(memory)
            if flag_bytes is None or flag_bytes != parsed["memory_bytes"]:
                memory_ok = False
                mismatch_detail.append(
                    f"memory: provided {memory!r} but observed "
                    f"{parsed['memory_bytes']} ({parsed['memory_origin']})"
                )
            elif parsed["memory_origin"] != "explicit":
                memory_ok = False
                mismatch_detail.append(
                    f"memory: provided flag requires origin 'explicit' but observed "
                    f"'{parsed['memory_origin']}'"
                )
        elif parsed["memory_origin"] not in ("auto", "fallback"):
            memory_ok = False
            mismatch_detail.append(
                f"memory: omitted but observed origin '{parsed['memory_origin']}' "
                "(allowed auto|fallback)"
            )
        if not memory_ok:
            mismatches.append("memory")
        temp_ok = True
        if temp_limit is not None:
            flag_bytes = parse_size_bytes(temp_limit)
            if flag_bytes is None or flag_bytes != parsed["temp_limit_bytes"]:
                temp_ok = False
                mismatch_detail.append(
                    f"temp_limit: provided {temp_limit!r} but observed "
                    f"{parsed['temp_limit_bytes']} ({parsed['temp_limit_origin']})"
                )
            elif parsed["temp_limit_origin"] != "explicit":
                temp_ok = False
                mismatch_detail.append(
                    "temp_limit: provided flag requires origin 'explicit' but "
                    f"observed '{parsed['temp_limit_origin']}'"
                )
        elif parsed["temp_limit_origin"] != "auto":
            temp_ok = False
            mismatch_detail.append(
                f"temp_limit: omitted but observed origin "
                f"'{parsed['temp_limit_origin']}' (allowed auto)"
            )
        if not temp_ok:
            mismatches.append("temp_limit")

    return {
        "temp_dir": str(temp_dir),
        "flags": {"memory": memory, "temp_limit": temp_limit},
        "configured": {
            "memory": memory is not None,
            "temp_limit": temp_limit is not None,
        },
        "resolved": resolved,
        "resolved_source": resolved_source,
        "default_kind": default_kind,
        "binary_version": binary_version,
        "flag_vs_resolved_mismatch": mismatches,
        "flag_vs_resolved_mismatch_detail": mismatch_detail,
        "verification": _VERIFICATION_SCOPE[resolved_source],
        "verified": resolved_source in _VERIFIED_SOURCES and not mismatches,
    }


def invalid_resource_block(
    *,
    temp_dir: Path,
    memory: str | None,
    temp_limit: str | None,
    diagnostic_error: str,
) -> dict[str, Any]:
    """Explicit invalid-state resource block for an unusable diagnostic.

    A line that was attempted but is malformed/duplicate/zero/overflowing must
    never fall through to absent/version/configured resolution, and can never
    read as verified.  The error is retained on the block itself so a FAILED
    report is self-explanatory.
    """
    return {
        "temp_dir": str(temp_dir),
        "flags": {"memory": memory, "temp_limit": temp_limit},
        "configured": {
            "memory": memory is not None,
            "temp_limit": temp_limit is not None,
        },
        "resolved": {
            "memory_bytes": None,
            "memory_origin": None,
            "temp_limit_bytes": None,
            "temp_limit_origin": None,
        },
        "resolved_source": "invalid_diagnostic",
        "default_kind": "invalid_diagnostic",
        "binary_version": None,
        "flag_vs_resolved_mismatch": [],
        "flag_vs_resolved_mismatch_detail": [],
        "verification": "invalid",
        "verified": False,
        "diagnostic_error": diagnostic_error,
    }


def resolve_resource_block(
    *,
    temp_dir: Path,
    memory: str | None,
    temp_limit: str | None,
    compress_stderr: str | None,
    binary_version: str | None,
) -> tuple[dict[str, Any], str | None]:
    """Validate an attempted diagnostic, then build the resource block.

    Validation is independent of the compress exit status and happens before any
    report is built.  Returns `(block, None)` on a clean/absent diagnostic, or
    `(invalid_block, error)` when a line was attempted but is unusable.  The
    caller must fail on a non-None error.
    """
    error = assess_resources_stderr(compress_stderr or "")
    if error is not None:
        return (
            invalid_resource_block(
                temp_dir=temp_dir,
                memory=memory,
                temp_limit=temp_limit,
                diagnostic_error=error,
            ),
            error,
        )
    return (
        build_resource_report(
            temp_dir=temp_dir,
            memory=memory,
            temp_limit=temp_limit,
            compress_stderr=compress_stderr,
            binary_version=binary_version,
        ),
        None,
    )


def sha256_stream(seed: bytes, nbytes: int) -> bytes:
    """Deterministic non-zero expanding stream (SHA-256 of seed||counter)."""
    out = bytearray()
    counter = 0
    while len(out) < nbytes:
        block = hashlib.sha256(seed + counter.to_bytes(8, "little")).digest()
        out.extend(block)
        counter += 1
    return bytes(out[:nbytes])


def generate_input(path: Path, *, size: int = DEFAULT_SIZE) -> dict[str, Any]:
    if size != DEFAULT_SIZE:
        if size < REPEAT_LEN * 2:
            raise ValueError("size must be at least 2 MiB to hold two repeat blocks")
        unique = size - 2 * REPEAT_LEN
        if unique <= 0:
            raise ValueError("size must leave a positive unique region")
    else:
        unique = UNIQUE_LEN
        if size != REPEAT_LEN + UNIQUE_LEN + REPEAT_LEN:
            raise AssertionError("default recipe size must be 258 MiB")

    repeat = sha256_stream(REPEAT_SEED, REPEAT_LEN)
    unique_bytes = sha256_stream(UNIQUE_SEED, unique)
    if unique_bytes.count(0) == len(unique_bytes):
        raise AssertionError("unique region must not be all zeros")
    if unique >= 128 and unique_bytes[:64] == unique_bytes[-64:]:
        raise AssertionError("unique region must not be a trivial repeating prefix/suffix")
    if repeat == unique_bytes[:REPEAT_LEN] if unique >= REPEAT_LEN else False:
        raise AssertionError("repeat block must differ from the unique-region prefix")

    hasher = hashlib.sha256()
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as handle:
        handle.write(repeat)
        hasher.update(repeat)
        handle.write(unique_bytes)
        hasher.update(unique_bytes)
        handle.write(repeat)
        hasher.update(repeat)
        handle.flush()
        os.fsync(handle.fileno())

    digest = hasher.hexdigest()
    actual = path.stat().st_size
    if actual != size:
        raise AssertionError(f"wrote {actual} bytes, expected {size}")
    return {
        "recipe_id": RECIPE_ID if size == DEFAULT_SIZE else f"{RECIPE_ID}-scaled-{size}",
        "size_bytes": actual,
        "repeat_len": REPEAT_LEN,
        "unique_len": unique,
        "sha256": digest,
        "path": str(path),
        "sparse": False,
        "all_zeros": False,
        "repeat_block_sha256": hashlib.sha256(repeat).hexdigest(),
        "unique_sha256": hashlib.sha256(unique_bytes).hexdigest(),
    }


def file_sha256(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            hasher.update(chunk)
    return hasher.hexdigest()


def default_argv(
    binary: Path,
    command: str,
    *paths: Path,
    temp_dir: Path | None,
    memory: str | None,
    temp_limit: str | None = None,
    extra: list[str] | None = None,
) -> list[str]:
    argv = [str(binary), command]
    if extra:
        argv.extend(extra)
    if command in {"compress", "decompress"}:
        if temp_dir is not None:
            argv.extend(["--temp-dir", str(temp_dir)])
        if memory is not None:
            argv.extend(["--memory", memory])
        if temp_limit is not None:
            argv.extend(["--temp-limit", temp_limit])
    argv.extend(str(path) for path in paths)
    forbidden = {"-m", "-m0", "-m1", "-m2", "-m3", "-m4", "-m5", "--method", "--layout", "--checksum"}
    for item in argv[2:]:
        if item in forbidden or item.startswith("--method=") or item.startswith("--layout=") or item.startswith("--checksum="):
            raise AssertionError(f"default roundtrip must not override method/layout/checksum: {argv}")
    return argv


def parse_info(text: str) -> dict[str, str]:
    fields: dict[str, str] = {}
    for line in text.splitlines():
        if ":" in line:
            key, value = line.split(":", 1)
            fields[key.strip()] = value.strip()
    return fields


def write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(path)


def record_terminal_failure(report_dir: Path, error: BaseException | str) -> None:
    """Always leave FAILED status/report, including SystemExit startup errors."""
    message = str(error)
    payload = {
        "ok": False,
        "state": "FAILED",
        "error": message,
        "pid": os.getpid(),
    }
    write_json(report_dir / "status.json", payload)
    write_json(report_dir / "report.json", payload)


def resolve_existing_dir(path: Path, *, create: bool) -> Path:
    resolved = path.expanduser()
    if not resolved.is_absolute():
        resolved = Path.cwd() / resolved
    resolved = resolved.resolve()
    if create:
        resolved.mkdir(parents=True, exist_ok=True)
    return resolved


def resolve_binary(path: Path) -> Path:
    resolved = path.expanduser()
    if not resolved.is_absolute():
        resolved = Path.cwd() / resolved
    resolved = resolved.resolve()
    return resolved


def run_step(
    name: str,
    argv: list[str],
    log_dir: Path,
    *,
    env: dict[str, str] | None = None,
) -> dict[str, Any]:
    stdout_path = log_dir / f"{name}.stdout.log"
    stderr_path = log_dir / f"{name}.stderr.log"
    started = time.time()
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        proc = subprocess.run(
            argv,
            stdout=stdout,
            stderr=stderr,
            env=env,
            check=False,
        )
    ended = time.time()
    stdout_text = stdout_path.read_text(encoding="utf-8", errors="replace")
    stderr_text = stderr_path.read_text(encoding="utf-8", errors="replace")
    return {
        "name": name,
        "argv": argv,
        "exit_code": proc.returncode,
        "duration_seconds": ended - started,
        "started_unix": started,
        "ended_unix": ended,
        "stdout_path": str(stdout_path),
        "stderr_path": str(stderr_path),
        "stdout": stdout_text,
        "stderr": stderr_text,
    }


def probe_binary_version(
    binary: Path, log_dir: Path, env: dict[str, str]
) -> dict[str, Any]:
    """Run `srep --version` once and return a bound probe record.

    The record keeps the argv, exit code, stdout, stderr, and the binary hash
    observed immediately before the probe, so a binary swapped between the
    compress step and the probe cannot be silently mislabelled.  Used only to
    label the fixed built-ins of a legacy (<0.3.0) binary when no resources
    line is available.
    """
    sha_before = file_sha256(binary)
    step = run_step("version", [str(binary), "--version"], log_dir, env=env)
    step["sha256_before"] = sha_before
    step["version"] = parse_version_line(step["stdout"])
    return step


def roundtrip(
    *,
    binary: Path,
    work_dir: Path,
    report_dir: Path,
    size: int,
    temp_dir: Path | None,
    memory: str | None,
    temp_limit: str | None = None,
    keep_input: bool,
) -> dict[str, Any]:
    report_dir.mkdir(parents=True, exist_ok=True)
    work_dir.mkdir(parents=True, exist_ok=True)
    log_dir = report_dir / "logs"
    log_dir.mkdir(parents=True, exist_ok=True)
    status_path = report_dir / "status.json"
    report_path = report_dir / "report.json"

    status: dict[str, Any] = {
        "state": "RUNNING",
        "pid": os.getpid(),
        "binary": str(binary),
        "work_dir": str(work_dir),
        "started_unix": time.time(),
        "self_timeout": None,
    }
    write_json(status_path, status)
    (report_dir / "pid").write_text(str(os.getpid()) + "\n", encoding="utf-8")

    if not binary.is_file() or not os.access(binary, os.X_OK):
        message = f"release binary is not executable: {binary}"
        record_terminal_failure(report_dir, message)
        raise SystemExit(message)
    # Bind every later step (including the optional version probe) to this hash;
    # a binary swapped mid-run must fail rather than produce a mixed report.
    binary_sha_start = file_sha256(binary)

    input_path = work_dir / "input.bin"
    archive_path = work_dir / "archive.srep"
    output_path = work_dir / "output.bin"
    if temp_dir is None:
        temp_dir = work_dir / "srep-temp"
    temp_dir.mkdir(parents=True, exist_ok=True)

    recipe = generate_input(input_path, size=size)
    input_sha = recipe["sha256"]
    steps: list[dict[str, Any]] = []
    env = os.environ.copy()
    env["TMPDIR"] = str(temp_dir)
    # Populated as soon as the compress diagnostics are available; until then it
    # honestly reports the budgets as unresolved rather than assuming defaults.
    resource_block = build_resource_report(
        temp_dir=temp_dir,
        memory=memory,
        temp_limit=temp_limit,
        compress_stderr=None,
        binary_version=None,
    )

    def fail(message: str, extra: dict[str, Any] | None = None) -> dict[str, Any]:
        payload = {
            "ok": False,
            "state": "FAILED",
            "error": message,
            "recipe": recipe,
            "steps": [
                {
                    key: value
                    for key, value in step.items()
                    if key not in {"stdout", "stderr"}
                }
                for step in steps
            ],
            "input_sha256": input_sha,
            "binary": str(binary),
            "cli_notes": "compress/decompress use binary defaults; driver does not pass -m/--method/--layout/--checksum",
            "resource": resource_block,
            # A FAILED report never claims a verified resource measurement.
            "resource_verified": False,
        }
        if extra:
            payload.update(extra)
        write_json(report_path, payload)
        status.update({"state": "FAILED", "ended_unix": time.time(), "error": message})
        write_json(status_path, status)
        return payload

    compress_argv = default_argv(
        binary,
        "compress",
        input_path,
        archive_path,
        temp_dir=temp_dir,
        memory=memory,
        temp_limit=temp_limit,
    )
    compress = run_step("compress", compress_argv, log_dir, env=env)
    steps.append(compress)
    write_json(status_path, {**status, "last_step": "compress", "last_exit": compress["exit_code"]})

    # Validate any attempted resources diagnostic independently of the compress
    # exit status, and BEFORE building any report block.  A malformed/duplicate/
    # zero/overflow line must fail closed; it may never fall through to an
    # absent/version/configured resolution.
    resource_block, diagnostic_error = resolve_resource_block(
        temp_dir=temp_dir,
        memory=memory,
        temp_limit=temp_limit,
        compress_stderr=compress["stderr"],
        binary_version=None,
    )
    if diagnostic_error is not None:
        return fail(diagnostic_error, {"compress": compress})
    if compress["exit_code"] != 0:
        # The diagnostic (if any) was valid; otherwise this is explicit/unresolved
        # without false facts.  Compress failed, so the run fails.
        return fail("compress failed", {"compress": compress})

    # Bind the actual budgets from the 0.3.0 resources diagnostic.  A legacy
    # binary (no line) is labelled by its `--version` lookup when no explicit
    # flags were passed; otherwise the explicit flags are the exact budgets.
    version_probe: dict[str, Any] | None = None
    binary_version: str | None = None
    if (
        parse_resources_line(compress["stderr"]) is None
        and memory is None
        and temp_limit is None
    ):
        version_probe = probe_binary_version(binary, log_dir, env)
        if version_probe["sha256_before"] != binary_sha_start:
            return fail(
                "binary changed before the version probe",
                {"version_probe": version_probe, "binary_sha256_start": binary_sha_start},
            )
        if version_probe["exit_code"] != 0:
            version_probe = None
        else:
            binary_version = version_probe["version"]
    resource_block, diagnostic_error = resolve_resource_block(
        temp_dir=temp_dir,
        memory=memory,
        temp_limit=temp_limit,
        compress_stderr=compress["stderr"],
        binary_version=binary_version,
    )
    if diagnostic_error is not None:
        return fail(diagnostic_error, {"compress": compress})
    # An explicit/omitted flag contradicted by the binary's own diagnostic (bytes
    # or origin) is a hard failure, not merely recorded metadata, and is checked
    # before info/test/decompress.
    if resource_block["flag_vs_resolved_mismatch"]:
        return fail(
            "resources diagnostic contradicts provided flags: "
            + ", ".join(resource_block["flag_vs_resolved_mismatch"]),
            {"compress": compress, "resource": resource_block},
        )

    info_argv = default_argv(binary, "info", archive_path, temp_dir=None, memory=None)
    info = run_step("info", info_argv, log_dir, env=env)
    steps.append(info)
    write_json(status_path, {**status, "last_step": "info", "last_exit": info["exit_code"]})
    if info["exit_code"] != 0:
        return fail("info failed")
    fields = parse_info(info["stdout"])
    expected = {
        "format": "SREP-NG v3",
        "method": "m3",
        "layout": "index",
        "checksum": "xxh3",
    }
    for key, value in expected.items():
        got = fields.get(key)
        if got != value:
            return fail(
                f"info {key} expected {value!r} got {got!r}",
                {"info_fields": fields},
            )

    test_argv = default_argv(binary, "test", archive_path, temp_dir=None, memory=None)
    test = run_step("test", test_argv, log_dir, env=env)
    steps.append(test)
    write_json(status_path, {**status, "last_step": "test", "last_exit": test["exit_code"]})
    if test["exit_code"] != 0:
        return fail("test failed")

    decompress_argv = default_argv(
        binary,
        "decompress",
        archive_path,
        output_path,
        temp_dir=temp_dir,
        memory=memory,
        temp_limit=temp_limit,
    )
    decompress = run_step("decompress", decompress_argv, log_dir, env=env)
    steps.append(decompress)
    write_json(status_path, {**status, "last_step": "decompress", "last_exit": decompress["exit_code"]})
    if decompress["exit_code"] != 0:
        return fail("decompress failed")

    if not output_path.is_file():
        return fail("decompress did not produce output")
    output_size = output_path.stat().st_size
    if output_size != recipe["size_bytes"]:
        return fail(
            f"output size {output_size} != input size {recipe['size_bytes']} (input was not truncated)"
        )
    output_sha = file_sha256(output_path)
    if output_sha != input_sha:
        return fail("SHA-256 mismatch after decompress", {"output_sha256": output_sha})

    cmp_started = time.time()
    cmp = subprocess.run(["cmp", str(input_path), str(output_path)], check=False, capture_output=True)
    cmp_ended = time.time()
    steps.append(
        {
            "name": "cmp",
            "argv": ["cmp", str(input_path), str(output_path)],
            "exit_code": cmp.returncode,
            "duration_seconds": cmp_ended - cmp_started,
            "started_unix": cmp_started,
            "ended_unix": cmp_ended,
            "stdout": cmp.stdout.decode("utf-8", "replace"),
            "stderr": cmp.stderr.decode("utf-8", "replace"),
        }
    )
    if cmp.returncode != 0:
        return fail("byte compare failed")

    archive_sha = file_sha256(archive_path)
    binary_sha = file_sha256(binary)
    if binary_sha != binary_sha_start:
        return fail(
            "binary changed during the roundtrip",
            {"binary_sha256_start": binary_sha_start, "binary_sha256_end": binary_sha},
        )
    payload = {
        "ok": True,
        "state": "PASSED",
        "recipe": recipe,
        "input_sha256": input_sha,
        "output_sha256": output_sha,
        "archive_sha256": archive_sha,
        "binary_sha256": binary_sha,
        "binary_sha256_start": binary_sha_start,
        "binary": str(binary),
        "archive": str(archive_path),
        "info_fields": fields,
        "expected_defaults": expected,
        "cli": {
            "compress": compress_argv,
            "info": info_argv,
            "test": test_argv,
            "decompress": decompress_argv,
            "method_override": False,
            "layout_override": False,
            "checksum_override": False,
        },
        # Round-trip PASS is independent of resource verification: an unknown
        # budget still proves the archive round-trips but must not read as a
        # verified resource measurement.
        "resource_verified": bool(resource_block.get("verified")),
        "version_probe": version_probe,
        "resource": {
            **resource_block,
            "output_dest": str(output_path),
        },
        "steps": [
            {key: value for key, value in step.items() if key not in {"stdout", "stderr"}}
            for step in steps
        ],
        "self_timeout": None,
        "keep_input": keep_input,
    }
    write_json(report_path, payload)
    status.update({"state": "PASSED", "ended_unix": time.time(), "ok": True})
    write_json(status_path, status)
    if not keep_input:
        try:
            input_path.unlink()
            output_path.unlink()
        except OSError:
            pass
    return payload


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, type=Path, help="fresh release srep binary")
    parser.add_argument("--work-dir", required=True, type=Path)
    parser.add_argument("--report-dir", required=True, type=Path)
    parser.add_argument("--bytes", type=int, default=DEFAULT_SIZE, help="input size (default 258 MiB)")
    parser.add_argument(
        "--allow-small",
        action="store_true",
        help="permit --bytes <= 256 MiB (unit tests only; release must not use this)",
    )
    parser.add_argument("--temp-dir", type=Path, default=None, help="explicit srep --temp-dir")
    parser.add_argument(
        "--memory",
        default=None,
        help=(
            "optional --memory SIZE passed through and logged; omit to bind the "
            "binary's actual resolved value from the 0.3.0 resources line "
            "(legacy binaries are labelled by --version lookup)"
        ),
    )
    parser.add_argument(
        "--temp-limit",
        default=None,
        help=(
            "optional --temp-limit SIZE passed through and logged; omit to bind "
            "the binary's actual resolved value from the 0.3.0 resources line "
            "(legacy binaries are labelled by --version lookup)"
        ),
    )
    parser.add_argument("--keep-input", action="store_true")
    parser.add_argument(
        "--detach",
        action="store_true",
        help="re-exec detached with durable pid/status under --report-dir (no self-timeout)",
    )
    args = parser.parse_args(argv)

    if args.bytes <= 256 * MIB and not args.allow_small:
        print(
            "refusing size <= 256 MiB without --allow-small (release roundtrip must stay >256 MiB)",
            file=sys.stderr,
        )
        return 2

    # Resolve every path against the caller's cwd BEFORE any fork/chdir("/").
    report_dir = resolve_existing_dir(args.report_dir, create=True)
    work_dir = resolve_existing_dir(args.work_dir, create=True)
    temp_dir = resolve_existing_dir(args.temp_dir, create=True) if args.temp_dir else None
    binary = resolve_binary(args.bin)

    if not binary.is_file() or not os.access(binary, os.X_OK):
        message = f"release binary is not executable: {binary}"
        record_terminal_failure(report_dir, message)
        print(message, file=sys.stderr)
        return 2

    if args.detach:
        log_path = report_dir / "wrapper.log"
        pid = os.fork()
        if pid > 0:
            (report_dir / "wrapper.pid").write_text(str(pid) + "\n", encoding="utf-8")
            print(f"RUNNING pid={pid} log={log_path} status={report_dir / 'status.json'}")
            return 0
        os.setsid()
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
        os.dup2(fd, 1)
        os.dup2(fd, 2)
        if fd > 2:
            os.close(fd)
        os.chdir("/")

    try:
        payload = roundtrip(
            binary=binary,
            work_dir=work_dir,
            report_dir=report_dir,
            size=args.bytes,
            temp_dir=temp_dir,
            memory=args.memory,
            temp_limit=args.temp_limit,
            keep_input=args.keep_input,
        )
    except SystemExit as error:
        record_terminal_failure(report_dir, error)
        code = error.code
        if isinstance(code, int):
            return code if code != 0 else 1
        return 1
    except Exception as error:  # noqa: BLE001 — durable failure record
        record_terminal_failure(report_dir, error)
        raise
    print(
        json.dumps(
            {
                "ok": payload["ok"],
                "state": payload["state"],
                "report": str(report_dir / "report.json"),
            }
        )
    )
    return 0 if payload["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())

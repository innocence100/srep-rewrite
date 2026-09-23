#!/usr/bin/env python3
"""Default-m3 large-file roundtrip for the Linux release binary.

Recipe (default): 258 MiB deterministic mix — 1 MiB repeated block, 256 MiB
unique non-zero data, then the same 1 MiB block. Not sparse zeros.

CLI compress uses the binary defaults (m3 / index / xxh3). The driver never
passes -m, --method, --layout, or --checksum.

This process does not self-timeout. Callers that need a wall clock must set
their own (integration may use 86400000 ms). Use --detach for durable
pid/status/logs.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

MIB = 1024 * 1024
DEFAULT_SIZE = 258 * MIB
REPEAT_LEN = 1 * MIB
UNIQUE_LEN = 256 * MIB
RECIPE_ID = "repeat-1mib-separated-256mib-unique-v1"
REPEAT_SEED = b"srep-ng-v0.1.0-repeat-block\n"
UNIQUE_SEED = b"srep-ng-v0.1.0-unique-region\n"


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


def roundtrip(
    *,
    binary: Path,
    work_dir: Path,
    report_dir: Path,
    size: int,
    temp_dir: Path | None,
    memory: str | None,
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
            "resource": {
                "temp_dir": str(temp_dir),
                "memory_flag": memory,
                "memory_default_preferred_bytes": 256 * MIB,
                "memory_override_logged": memory is not None,
            },
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
    )
    compress = run_step("compress", compress_argv, log_dir, env=env)
    steps.append(compress)
    write_json(status_path, {**status, "last_step": "compress", "last_exit": compress["exit_code"]})
    if compress["exit_code"] != 0:
        return fail("compress failed", {"compress": compress})

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
    payload = {
        "ok": True,
        "state": "PASSED",
        "recipe": recipe,
        "input_sha256": input_sha,
        "output_sha256": output_sha,
        "archive_sha256": archive_sha,
        "binary_sha256": binary_sha,
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
        "resource": {
            "temp_dir": str(temp_dir),
            "memory_flag": memory,
            "memory_default_preferred_bytes": 256 * MIB,
            "memory_override_logged": memory is not None,
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
        help="optional --memory SIZE passed through and logged; omit to keep the 256 MiB default",
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

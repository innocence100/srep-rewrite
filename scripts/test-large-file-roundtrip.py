#!/usr/bin/env python3
"""Unit tests for scripts/large-file-roundtrip.py (no 258 MiB compress)."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "large-file-roundtrip.py"


def load_module():
    spec = importlib.util.spec_from_file_location("large_file_roundtrip", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def stub_binary(path: Path) -> Path:
    path.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        "cmd=$1\n"
        "shift\n"
        "case $cmd in\n"
        "  compress)\n"
        "    while [ $# -gt 2 ]; do shift; done\n"
        "    in=$1; out=$2\n"
        "    cp -- \"$in\" \"$out\"\n"
        "    ;;\n"
        "  info)\n"
        "    printf '%s\\n' 'format: SREP-NG v3'\n"
        "    printf '%s\\n' 'method: m3'\n"
        "    printf '%s\\n' 'layout: index'\n"
        "    printf '%s\\n' 'checksum: xxh3'\n"
        "    ;;\n"
        "  test) exit 0 ;;\n"
        "  decompress)\n"
        "    while [ $# -gt 2 ]; do shift; done\n"
        "    in=$1; out=$2\n"
        "    cp -- \"$in\" \"$out\"\n"
        "    ;;\n"
        "  *) exit 99 ;;\n"
        "esac\n",
        encoding="utf-8",
    )
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    return path


def test_default_constants(mod) -> None:
    assert mod.DEFAULT_SIZE == 258 * 1024 * 1024
    assert mod.REPEAT_LEN + mod.UNIQUE_LEN + mod.REPEAT_LEN == mod.DEFAULT_SIZE
    assert mod.DEFAULT_SIZE > 256 * 1024 * 1024


def test_scaled_recipe_is_not_sparse_and_has_repeats(mod) -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-recipe-", dir=os.environ.get("TMPDIR")))
    try:
        path = tmp / "input.bin"
        size = 3 * 1024 * 1024
        recipe = mod.generate_input(path, size=size)
        data = path.read_bytes()
        assert len(data) == size
        assert recipe["size_bytes"] == size
        assert recipe["sha256"] == hashlib.sha256(data).hexdigest()
        assert data[: mod.REPEAT_LEN] == data[-mod.REPEAT_LEN :]
        unique = data[mod.REPEAT_LEN : -mod.REPEAT_LEN]
        assert unique != b"\x00" * len(unique)
        assert unique[:64] != unique[-64:]
        assert data.count(0) < len(data) // 8
        assert recipe["sparse"] is False
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_cli_construction_never_overrides_defaults(mod) -> None:
    argv = mod.default_argv(
        Path("/tmp/srep"),
        "compress",
        Path("in"),
        Path("out"),
        temp_dir=Path("/tmp/srep-temp"),
        memory=None,
    )
    joined = " ".join(argv)
    assert argv[:2] == ["/tmp/srep", "compress"]
    assert "--temp-dir" in argv
    assert "-m" not in argv
    assert "--method" not in joined
    assert "--layout" not in joined
    assert "--checksum" not in joined
    try:
        mod.default_argv(
            Path("/tmp/srep"),
            "compress",
            Path("in"),
            extra=["-m3"],
            temp_dir=None,
            memory=None,
        )
        raise AssertionError("expected forbidden -m3")
    except AssertionError:
        pass


def test_refuse_small_without_flag() -> None:
    result = subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--bin",
            "/bin/true",
            "--work-dir",
            "/tmp/unused",
            "--report-dir",
            "/tmp/unused",
            "--bytes",
            str(1024),
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    assert result.returncode == 2
    assert "256 MiB" in result.stderr


def test_tiny_roundtrip_with_stub_binary() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-stub-", dir=os.environ.get("TMPDIR")))
    try:
        stub = stub_binary(tmp / "srep")
        work = tmp / "work"
        report = tmp / "report"
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--bin",
                str(stub),
                "--work-dir",
                str(work),
                "--report-dir",
                str(report),
                "--bytes",
                str(3 * 1024 * 1024),
                "--allow-small",
                "--keep-input",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        payload = (report / "report.json").read_text(encoding="utf-8")
        assert '"ok": true' in payload
        assert '"state": "PASSED"' in payload
        assert "-m3" not in payload
        assert "--method" not in payload
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_invalid_binary_writes_failed_status(mod) -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-badbin-", dir=os.environ.get("TMPDIR")))
    try:
        missing = tmp / "no-such-srep"
        work = tmp / "work"
        report = tmp / "report"
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--bin",
                str(missing),
                "--work-dir",
                str(work),
                "--report-dir",
                str(report),
                "--bytes",
                str(3 * 1024 * 1024),
                "--allow-small",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        assert result.returncode == 2, result.stdout + result.stderr
        status = json.loads((report / "status.json").read_text(encoding="utf-8"))
        payload = json.loads((report / "report.json").read_text(encoding="utf-8"))
        assert status["state"] == "FAILED"
        assert payload["state"] == "FAILED"
        assert payload["ok"] is False
        assert "not executable" in payload["error"]
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_detached_relative_paths_resolve_before_chdir() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-detach-", dir=os.environ.get("TMPDIR")))
    try:
        stub = stub_binary(tmp / "srep")
        # Caller cwd is tmp; pass relative names so a post-fork chdir("/") would
        # otherwise look in the filesystem root.
        result = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--bin",
                "srep",
                "--work-dir",
                "work",
                "--report-dir",
                "report",
                "--temp-dir",
                "temp",
                "--bytes",
                str(3 * 1024 * 1024),
                "--allow-small",
                "--keep-input",
                "--detach",
            ],
            check=False,
            capture_output=True,
            text=True,
            cwd=tmp,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        assert "RUNNING pid=" in result.stdout
        report = tmp / "report"
        deadline = time.time() + 30
        status_path = report / "status.json"
        while time.time() < deadline:
            if status_path.is_file():
                status = json.loads(status_path.read_text(encoding="utf-8"))
                if status.get("state") in {"PASSED", "FAILED"}:
                    break
            time.sleep(0.05)
        else:
            raise AssertionError("detached roundtrip did not finish")
        status = json.loads(status_path.read_text(encoding="utf-8"))
        payload = json.loads((report / "report.json").read_text(encoding="utf-8"))
        assert status["state"] == "PASSED", status
        assert payload["ok"] is True
        assert str(tmp / "work") in payload["recipe"]["path"]
        assert (tmp / "work" / "input.bin").is_file()
        assert not Path("/work").exists()
        assert str(report) in str(payload.get("resource", {}).get("temp_dir", "")) or (
            tmp / "temp"
        ).is_dir()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def main() -> int:
    mod = load_module()
    tests = [
        lambda: test_default_constants(mod),
        lambda: test_scaled_recipe_is_not_sparse_and_has_repeats(mod),
        lambda: test_cli_construction_never_overrides_defaults(mod),
        test_refuse_small_without_flag,
        test_tiny_roundtrip_with_stub_binary,
        lambda: test_invalid_binary_writes_failed_status(mod),
        test_detached_relative_paths_resolve_before_chdir,
    ]
    names = [
        "test_default_constants",
        "test_scaled_recipe_is_not_sparse_and_has_repeats",
        "test_cli_construction_never_overrides_defaults",
        "test_refuse_small_without_flag",
        "test_tiny_roundtrip_with_stub_binary",
        "test_invalid_binary_writes_failed_status",
        "test_detached_relative_paths_resolve_before_chdir",
    ]
    for name, test in zip(names, tests, strict=True):
        test()
        print(f"{name}: ok")
    print(f"test-large-file-roundtrip.py: PASS ({len(tests)} tests)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

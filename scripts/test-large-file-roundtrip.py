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
        "  --version)\n"
        "    printf '%s\\n' \"srep ${SREP_STUB_VERSION:-0.2.1}\"\n"
        "    ;;\n"
        "  compress)\n"
        "    while [ $# -gt 2 ]; do shift; done\n"
        "    in=$1; out=$2\n"
        "    cp -- \"$in\" \"$out\"\n"
        "    if [ -n \"${SREP_STUB_RESOURCES:-}\" ]; then\n"
        "      printf '%s\\n' \"$SREP_STUB_RESOURCES\" >&2\n"
        "    fi\n"
        "    if [ -n \"${SREP_STUB_EXIT:-}\" ]; then exit \"$SREP_STUB_EXIT\"; fi\n"
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


def run_stub_roundtrip(
    tmp: Path,
    *,
    args: list[str],
    env_overrides: dict[str, str] | None = None,
) -> tuple[subprocess.CompletedProcess, Path]:
    stub = stub_binary(tmp / "srep")
    work = tmp / "work"
    report = tmp / "report"
    env = os.environ.copy()
    if env_overrides:
        env.update(env_overrides)
    command = [
        sys.executable,
        str(SCRIPT),
        "--bin",
        str(stub),
        "--work-dir",
        str(work),
        "--report-dir",
        str(report),
        "--keep-input",
        *args,
    ]
    result = subprocess.run(command, check=False, capture_output=True, text=True, env=env)
    return result, report


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
    assert "--temp-limit" not in argv
    assert "-m" not in argv
    assert "--method" not in joined
    assert "--layout" not in joined
    assert "--checksum" not in joined
    with_limit = mod.default_argv(
        Path("/tmp/srep"),
        "compress",
        Path("in"),
        Path("out"),
        temp_dir=Path("/tmp/srep-temp"),
        memory="256MiB",
        temp_limit="32GiB",
    )
    assert with_limit[with_limit.index("--memory") + 1] == "256MiB"
    assert with_limit[with_limit.index("--temp-limit") + 1] == "32GiB"
    assert "--temp-limit" not in mod.default_argv(
        Path("/tmp/srep"),
        "info",
        Path("archive"),
        temp_dir=None,
        memory=None,
        temp_limit="32GiB",
    )
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
                "--temp-limit",
                "8GiB",
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
        parsed = json.loads(payload)
        resource = parsed["resource"]
        # A legacy stub emits no resources line, so the explicit flag is the
        # authoritative budget for temp; memory stays unresolved-by-absence.
        assert resource["flags"]["temp_limit"] == "8GiB"
        assert resource["configured"]["temp_limit"] is True
        assert resource["configured"]["memory"] is False
        assert resource["resolved_source"] == "partial_explicit_flags"
        assert resource["resolved"]["temp_limit_bytes"] == 8 * 1024**3
        assert resource["resolved"]["temp_limit_origin"] == "explicit"
        assert resource["resolved"]["memory_bytes"] is None
        # A partial measurement can round-trip PASS but must not claim
        # resource verification.
        assert resource["verified"] is False
        assert resource["verification"] == "partial"
        assert parsed["resource_verified"] is False
        compress_argv = parsed["cli"]["compress"]
        assert compress_argv[compress_argv.index("--temp-limit") + 1] == "8GiB"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_resources_line_is_bound_from_compress_stderr() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-resources-", dir=os.environ.get("TMPDIR")))
    try:
        line = "srep: resources: memory=8589934592 (auto) temp_limit=34359738368 (auto)"
        result, report = run_stub_roundtrip(
            tmp,
            args=["--bytes", str(3 * 1024 * 1024), "--allow-small"],
            env_overrides={"SREP_STUB_VERSION": "0.3.0", "SREP_STUB_RESOURCES": line},
        )
        assert result.returncode == 0, result.stdout + result.stderr
        parsed = json.loads((report / "report.json").read_text(encoding="utf-8"))
        resource = parsed["resource"]
        assert resource["resolved_source"] == "compress_stderr_resources"
        assert resource["default_kind"] is None
        assert resource["configured"] == {"memory": False, "temp_limit": False}
        assert resource["resolved"]["memory_bytes"] == 8589934592
        assert resource["resolved"]["memory_origin"] == "auto"
        assert resource["resolved"]["temp_limit_bytes"] == 34359738368
        assert resource["resolved"]["temp_limit_origin"] == "auto"
        assert resource["verified"] is True
        assert resource["verification"] == "stderr_observed"
        # The driver never ran a version probe when the line was present.
        assert resource["binary_version"] is None
        assert parsed["version_probe"] is None
        assert resource["flag_vs_resolved_mismatch"] == []
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_resources_line_preferred_over_known_legacy_version() -> None:
    # A stale 0.2.1 version string on a binary that *does* emit the 0.3.0
    # diagnostic must trust the observed line, never the version lookup.
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-prefer-", dir=os.environ.get("TMPDIR")))
    try:
        line = "srep: resources: memory=1073741824 (auto) temp_limit=34359738368 (auto)"
        result, report = run_stub_roundtrip(
            tmp,
            args=["--bytes", str(3 * 1024 * 1024), "--allow-small"],
            env_overrides={"SREP_STUB_VERSION": "0.2.1", "SREP_STUB_RESOURCES": line},
        )
        assert result.returncode == 0, result.stdout + result.stderr
        resource = json.loads((report / "report.json").read_text(encoding="utf-8"))["resource"]
        assert resource["resolved_source"] == "compress_stderr_resources"
        assert resource["default_kind"] is None
        assert resource["resolved"]["memory_bytes"] == 1073741824
        assert resource["resolved"]["temp_limit_bytes"] == 34359738368
        assert resource["binary_version"] is None
        assert resource["verified"] is True
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_missing_resources_known_legacy_version_binds_fixed_defaults() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-legacy-", dir=os.environ.get("TMPDIR")))
    try:
        result, report = run_stub_roundtrip(
            tmp,
            args=["--bytes", str(3 * 1024 * 1024), "--allow-small"],
            env_overrides={"SREP_STUB_VERSION": "0.2.1"},
        )
        assert result.returncode == 0, result.stdout + result.stderr
        parsed = json.loads((report / "report.json").read_text(encoding="utf-8"))
        resource = parsed["resource"]
        assert resource["binary_version"] == "0.2.1"
        assert resource["resolved_source"] == "known_binary_defaults"
        assert resource["default_kind"] == "binary_builtin_fixed_v0.2.1"
        assert resource["resolved"]["memory_bytes"] == 256 * 1024 * 1024
        assert resource["resolved"]["temp_limit_bytes"] == 4 * 1024**3
        assert resource["resolved"]["memory_origin"] == "binary_default"
        # Without an observed diagnostic this is inferred, not verified.
        assert resource["verified"] is False
        assert resource["verification"] == "legacy_version_lookup"
        # The version probe is recorded with argv/status and a bound hash.
        probe = parsed["version_probe"]
        assert probe is not None
        assert probe["argv"][1] == "--version"
        assert probe["exit_code"] == 0
        assert probe["version"] == "0.2.1"
        assert probe["sha256_before"] == parsed["binary_sha256"]
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_missing_resources_unknown_version_reports_unresolved() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-unknown-", dir=os.environ.get("TMPDIR")))
    try:
        result, report = run_stub_roundtrip(
            tmp,
            args=["--bytes", str(3 * 1024 * 1024), "--allow-small"],
            env_overrides={"SREP_STUB_VERSION": "9.9.9"},
        )
        assert result.returncode == 0, result.stdout + result.stderr
        parsed = json.loads((report / "report.json").read_text(encoding="utf-8"))
        resource = parsed["resource"]
        assert resource["resolved_source"] == "unresolved"
        assert resource["default_kind"] == "unresolved"
        assert resource["resolved"]["memory_bytes"] is None
        assert resource["resolved"]["temp_limit_bytes"] is None
        # No fixed 256 MiB/4 GiB is claimed when the version is unknown, and the
        # round-trip PASS is explicitly not a resource verification.
        assert "memory_default_preferred_bytes" not in resource
        assert resource["verified"] is False
        assert resource["verification"] == "unresolved"
        assert parsed["resource_verified"] is False
        assert parsed["state"] == "PASSED"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _failed_report_case(
    tmp: Path,
    *,
    stderr_line: str,
    version: str,
    flags: list[str] | None = None,
    stub_exit: int | None = None,
) -> dict:
    env = {"SREP_STUB_VERSION": version, "SREP_STUB_RESOURCES": stderr_line}
    if stub_exit is not None:
        env["SREP_STUB_EXIT"] = str(stub_exit)
    result, report = run_stub_roundtrip(
        tmp,
        args=["--bytes", str(3 * 1024 * 1024), "--allow-small", *(flags or [])],
        env_overrides=env,
    )
    assert result.returncode != 0, result.stdout + result.stderr
    payload = json.loads((report / "report.json").read_text(encoding="utf-8"))
    assert payload["ok"] is False
    assert payload["state"] == "FAILED"
    assert payload["resource_verified"] is False
    assert payload["error"], "failure must retain an error message"
    return payload


def _resources_line(
    memory_bytes: int,
    memory_origin: str,
    temp_limit_bytes: int = 32 * 1024**3,
    temp_limit_origin: str = "auto",
) -> str:
    return (
        f"srep: resources: memory={memory_bytes} ({memory_origin}) "
        f"temp_limit={temp_limit_bytes} ({temp_limit_origin})"
    )


def test_malformed_resources_line_fails_not_fallback() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-malformed-", dir=os.environ.get("TMPDIR")))
    try:
        # Even with a known 0.2.1 version string, an attempted-but-malformed
        # line must fail rather than fall back to the version's fixed defaults.
        payload = _failed_report_case(
            tmp,
            stderr_line="srep: resources: memory=not-a-number (auto) temp_limit=32 (bogus)",
            version="0.2.1",
        )
        assert "malformed resources diagnostic" in payload["error"]
        resource = payload["resource"]
        assert resource["default_kind"] == "invalid_diagnostic"
        assert resource["resolved_source"] == "invalid_diagnostic"
        assert resource["resolved"]["memory_bytes"] is None
        assert resource["verified"] is False
        assert resource["verification"] == "invalid"
        assert "malformed" in resource["diagnostic_error"]
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_invalid_diagnostic_fails_for_both_flags_and_exit_statuses() -> None:
    line = _resources_line(268435456, "auto")
    cases = [
        ("malformed", "srep: resources: memory=x (auto) temp_limit=1 (bogus)"),
        ("duplicate", f"{line}\n{line}"),
        ("zero", _resources_line(0, "auto")),
        ("overflow", _resources_line(2**64, "auto")),
    ]
    for name, stderr_line in cases:
        for stub_exit in (0, 3):
            tmp = Path(
                tempfile.mkdtemp(prefix="srep-rt-invalid-", dir=os.environ.get("TMPDIR"))
            )
            try:
                payload = _failed_report_case(
                    tmp,
                    stderr_line=stderr_line,
                    version="0.3.0",
                    flags=["--memory", "256MiB", "--temp-limit", "32GiB"],
                    stub_exit=stub_exit,
                )
                resource = payload["resource"]
                # Never falls through to explicit_flags/version/absent.
                assert resource["resolved_source"] == "invalid_diagnostic", (
                    name,
                    stub_exit,
                )
                assert resource["verified"] is False, (name, stub_exit)
                assert resource["verification"] == "invalid"
                assert payload["resource_verified"] is False
                assert resource["diagnostic_error"], (name, stub_exit)
                assert payload["error"], (name, stub_exit)
            finally:
                shutil.rmtree(tmp, ignore_errors=True)


def test_origin_validation_matrix() -> None:
    # (flags, line, should_pass)
    cases = [
        # Provided flags require explicit origins (bytes may match).
        (["--memory", "256MiB", "--temp-limit", "32GiB"],
         _resources_line(256 * 1024**2, "explicit", 32 * 1024**3, "explicit"), True),
        (["--memory", "256MiB", "--temp-limit", "32GiB"],
         _resources_line(256 * 1024**2, "auto", 32 * 1024**3, "auto"), False),
        (["--memory", "256MiB", "--temp-limit", "32GiB"],
         _resources_line(256 * 1024**2, "explicit", 32 * 1024**3, "auto"), False),
        # Omitted: memory auto|fallback allowed, temp auto only.
        ([], _resources_line(256 * 1024**2, "fallback", 32 * 1024**3, "auto"), True),
        ([], _resources_line(256 * 1024**2, "auto", 32 * 1024**3, "auto"), True),
        ([], _resources_line(256 * 1024**2, "explicit", 32 * 1024**3, "auto"), False),
        ([], _resources_line(256 * 1024**2, "auto", 32 * 1024**3, "fallback"), False),
        # Mixed provided/auto per resource.
        (["--memory", "512MiB"],
         _resources_line(512 * 1024**2, "explicit", 32 * 1024**3, "auto"), True),
        (["--memory", "512MiB"],
         _resources_line(512 * 1024**2, "auto", 32 * 1024**3, "auto"), False),
        (["--memory", "512MiB"],
         _resources_line(512 * 1024**2, "explicit", 32 * 1024**3, "fallback"), False),
        # Same bytes, wrong origin, exact provided flags.
        (["--memory", "256MiB", "--temp-limit", "32GiB"],
         _resources_line(256 * 1024**2, "auto", 32 * 1024**3, "explicit"), False),
    ]
    for index, (flags, stderr_line, should_pass) in enumerate(cases):
        tmp = Path(
            tempfile.mkdtemp(prefix="srep-rt-origin-", dir=os.environ.get("TMPDIR"))
        )
        try:
            result, report = run_stub_roundtrip(
                tmp,
                args=["--bytes", str(3 * 1024 * 1024), "--allow-small", *flags],
                env_overrides={
                    "SREP_STUB_VERSION": "0.3.0",
                    "SREP_STUB_RESOURCES": stderr_line,
                },
            )
            payload = json.loads((report / "report.json").read_text(encoding="utf-8"))
            resource = payload["resource"]
            if should_pass:
                assert result.returncode == 0, (index, result.stderr)
                assert payload["state"] == "PASSED", (index, payload.get("error"))
                assert resource["resolved_source"] == "compress_stderr_resources"
                assert resource["verified"] is True, index
                assert resource["flag_vs_resolved_mismatch"] == [], index
            else:
                assert result.returncode != 0, (index, result.stdout)
                assert payload["state"] == "FAILED", index
                assert resource["verified"] is False, index
                assert payload["resource_verified"] is False, index
                assert resource["flag_vs_resolved_mismatch"], index
                assert resource["flag_vs_resolved_mismatch_detail"], index
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


def test_duplicate_resources_lines_fail() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-duplicate-", dir=os.environ.get("TMPDIR")))
    try:
        line = _resources_line(268435456, "auto")
        payload = _failed_report_case(
            tmp, stderr_line=f"{line}\n{line}", version="0.3.0"
        )
        assert "duplicate resources diagnostics" in payload["error"]
        assert payload["resource"]["resolved_source"] == "invalid_diagnostic"
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_zero_and_overflow_resources_values_fail() -> None:
    for line, needle in (
        (_resources_line(0, "explicit"), "memory resources value is zero"),
        (
            _resources_line(268435456, "auto", 0, "auto"),
            "temp_limit resources value is zero",
        ),
        (_resources_line(2**64, "auto"), "memory resources value overflows u64"),
    ):
        tmp = Path(tempfile.mkdtemp(prefix="srep-rt-badvalue-", dir=os.environ.get("TMPDIR")))
        try:
            payload = _failed_report_case(tmp, stderr_line=line, version="0.3.0")
            assert needle in payload["error"], payload["error"]
        finally:
            shutil.rmtree(tmp, ignore_errors=True)


def test_mismatched_flags_fail_closed() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-rt-mismatch-", dir=os.environ.get("TMPDIR")))
    try:
        # The stub reports 256 MiB/4 GiB while the driver asked for 512 MiB/8 GiB.
        line = _resources_line(256 * 1024**2, "explicit", 8 * 1024**3, "explicit")
        payload = _failed_report_case(
            tmp,
            stderr_line=line,
            version="0.3.0",
            flags=["--memory", "512MiB", "--temp-limit", "8GiB"],
        )
        assert "contradicts provided flags" in payload["error"]
        assert payload["resource"]["flag_vs_resolved_mismatch"] == ["memory"]
        # Actual stderr values are still surfaced for the failure record.
        assert payload["resource"]["resolved"]["memory_bytes"] == 256 * 1024 * 1024
        assert payload["resource"]["resolved"]["temp_limit_bytes"] == 8 * 1024**3
        assert payload["resource"]["verified"] is False
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


def test_parse_helpers(mod) -> None:
    # Pure parsers: the diagnostic is only trusted when it matches exactly.
    good = "srep: resources: memory=8589934592 (auto) temp_limit=34359738368 (fallback)"
    parsed = mod.parse_resources_line(good)
    assert parsed == {
        "memory_bytes": 8589934592,
        "memory_origin": "auto",
        "temp_limit_bytes": 34359738368,
        "temp_limit_origin": "fallback",
    }
    assert mod.assess_resources_stderr(good) is None
    assert mod.parse_resources_line("compressed 1 bytes\n") is None
    assert mod.assess_resources_stderr("compressed 1 bytes\n") is None
    # Attempted-but-invalid lines are detected (fail-closed) by assess, and
    # never parsed.
    malformed = "srep: resources: memory=1 (bogus) temp_limit=2 (auto)"
    assert mod.parse_resources_line(malformed) is None
    assert mod.assess_resources_stderr(malformed) is not None
    assert mod.assess_resources_stderr(f"{good}\n{good}") is not None
    assert mod.assess_resources_stderr(
        "srep: resources: memory=0 (auto) temp_limit=32 (auto)"
    ) is not None
    # Windows CRLF is tolerated for a valid line.
    assert mod.parse_resources_line(good + "\r\n") is not None
    assert mod.parse_version_line("srep 0.2.1\r\n") == "0.2.1"
    assert mod.parse_version_line("srep 0.3.0 extra\n") is None
    # Mirror the CLI grammar exactly: binary suffixes only, no whitespace/TiB.
    assert mod.parse_size_bytes("256MiB") == 256 * 1024 * 1024
    assert mod.parse_size_bytes("4GiB") == 4 * 1024**3
    assert mod.parse_size_bytes("8G") == 8 * 1024**3
    assert mod.parse_size_bytes("8gb") == 8 * 1024**3
    assert mod.parse_size_bytes("2m") == 2 * 1024 * 1024
    assert mod.parse_size_bytes("1024") == 1024
    for invalid in ("bogus", "1TiB", "1 TiB", "1.5MiB", "", "18446744073709551616"):
        assert mod.parse_size_bytes(invalid) is None, invalid
    # Unresolved default mode never claims fixed bytes or verification.
    unresolved = mod.build_resource_report(
        temp_dir=Path("/tmp"),
        memory=None,
        temp_limit=None,
        compress_stderr=None,
        binary_version="9.9.9",
    )
    assert unresolved["resolved_source"] == "unresolved"
    assert unresolved["default_kind"] == "unresolved"
    assert unresolved["resolved"]["memory_bytes"] is None
    assert unresolved["configured"] == {"memory": False, "temp_limit": False}
    assert unresolved["verified"] is False
    assert unresolved["verification"] == "unresolved"
    # Explicit complete flags are deterministic and verified.
    explicit = mod.build_resource_report(
        temp_dir=Path("/tmp"),
        memory="512MiB",
        temp_limit="8GiB",
        compress_stderr=None,
        binary_version="9.9.9",
    )
    assert explicit["resolved_source"] == "explicit_flags"
    assert explicit["resolved"]["memory_bytes"] == 512 * 1024 * 1024
    assert explicit["resolved"]["temp_limit_bytes"] == 8 * 1024**3
    # Flags alone are a truthful "configured" scope, not observed verification.
    assert explicit["verified"] is False
    assert explicit["verification"] == "configured_flags"
    # A zero/unparseable flag is not a valid explicit budget (CLI rejects it).
    zero = mod.build_resource_report(
        temp_dir=Path("/tmp"),
        memory="0",
        temp_limit="bogus",
        compress_stderr=None,
        binary_version="9.9.9",
    )
    assert zero["resolved_source"] == "unresolved"
    assert zero["resolved"]["memory_bytes"] is None
    assert zero["resolved"]["memory_origin"] is None
    assert zero["resolved"]["temp_limit_bytes"] is None
    assert zero["verified"] is False
    # resolve_resource_block fails closed on an attempted-but-invalid line,
    # independent of exit status, and never falls through.
    bad_block, bad_error = mod.resolve_resource_block(
        temp_dir=Path("/tmp"),
        memory="256MiB",
        temp_limit="32GiB",
        compress_stderr="srep: resources: memory=oops (auto) temp_limit=1 (bogus)",
        binary_version="0.2.1",
    )
    assert bad_error is not None
    assert bad_block["resolved_source"] == "invalid_diagnostic"
    assert bad_block["verified"] is False
    assert bad_block["diagnostic_error"] == bad_error
    good_block, good_error = mod.resolve_resource_block(
        temp_dir=Path("/tmp"),
        memory=None,
        temp_limit=None,
        compress_stderr=_resources_line(8589934592, "auto", 34359738368, "auto"),
        binary_version="0.3.0",
    )
    assert good_error is None
    assert good_block["verified"] is True
    assert good_block["resolved_source"] == "compress_stderr_resources"
    # ... and a valid observed line that contradicts provided flags has no
    # diagnostic error but is a mismatch (not verified).
    mismatch_block, mismatch_error = mod.resolve_resource_block(
        temp_dir=Path("/tmp"),
        memory="256MiB",
        temp_limit="32GiB",
        compress_stderr=good,
        binary_version="0.3.0",
    )
    assert mismatch_error is None
    assert mismatch_block["verified"] is False
    assert mismatch_block["flag_vs_resolved_mismatch"] == ["memory", "temp_limit"]


def main() -> int:
    mod = load_module()
    tests = [
        lambda: test_default_constants(mod),
        lambda: test_scaled_recipe_is_not_sparse_and_has_repeats(mod),
        lambda: test_cli_construction_never_overrides_defaults(mod),
        test_refuse_small_without_flag,
        test_tiny_roundtrip_with_stub_binary,
        test_resources_line_is_bound_from_compress_stderr,
        test_resources_line_preferred_over_known_legacy_version,
        test_missing_resources_known_legacy_version_binds_fixed_defaults,
        test_missing_resources_unknown_version_reports_unresolved,
        test_malformed_resources_line_fails_not_fallback,
        test_invalid_diagnostic_fails_for_both_flags_and_exit_statuses,
        test_origin_validation_matrix,
        test_duplicate_resources_lines_fail,
        test_zero_and_overflow_resources_values_fail,
        test_mismatched_flags_fail_closed,
        lambda: test_parse_helpers(mod),
        lambda: test_invalid_binary_writes_failed_status(mod),
        test_detached_relative_paths_resolve_before_chdir,
    ]
    names = [
        "test_default_constants",
        "test_scaled_recipe_is_not_sparse_and_has_repeats",
        "test_cli_construction_never_overrides_defaults",
        "test_refuse_small_without_flag",
        "test_tiny_roundtrip_with_stub_binary",
        "test_resources_line_is_bound_from_compress_stderr",
        "test_resources_line_preferred_over_known_legacy_version",
        "test_missing_resources_known_legacy_version_binds_fixed_defaults",
        "test_missing_resources_unknown_version_reports_unresolved",
        "test_malformed_resources_line_fails_not_fallback",
        "test_invalid_diagnostic_fails_for_both_flags_and_exit_statuses",
        "test_origin_validation_matrix",
        "test_duplicate_resources_lines_fail",
        "test_zero_and_overflow_resources_values_fail",
        "test_mismatched_flags_fail_closed",
        "test_parse_helpers",
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

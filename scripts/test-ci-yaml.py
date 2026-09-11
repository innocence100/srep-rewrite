#!/usr/bin/env python3
"""Parse the workflow with the platform YAML parser when available."""
from __future__ import annotations

import copy
import json
import subprocess
import tomllib
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
MANIFEST = ROOT / "Cargo.toml"
NATIVE_JOB = "native-matrix"
VERIFY_JOB = "verify"
WINDOWS_JOB = "windows-test"
STABLE = "stable"
OS_UBUNTU = "ubuntu-latest"
OS_WINDOWS = "windows-latest"
OS_MACOS = "macos-latest"
REQUIRED_OS = (OS_UBUNTU, OS_WINDOWS, OS_MACOS)


def manifest_rust_version(path: Path = MANIFEST) -> str:
    with path.open("rb") as handle:
        data = tomllib.load(handle)
    version = data["package"]["rust-version"]
    if not isinstance(version, str) or not version:
        raise AssertionError("Cargo.toml package.rust-version must be a non-empty string")
    parts = version.split(".")
    if len(parts) != 3 or not all(part.isdigit() for part in parts):
        raise AssertionError(
            f"Cargo.toml rust-version must be an explicit major.minor.patch, got {version!r}"
        )
    return version


def load_workflow(path: Path = WORKFLOW) -> dict[str, Any]:
    result = subprocess.run(
        [
            "ruby",
            "-e",
            "require 'yaml'; require 'json'; print JSON.generate(YAML.load_file(ARGV.fetch(0)))",
            str(path),
        ],
        check=True,
        capture_output=True,
        text=True,
    )
    doc = json.loads(result.stdout)
    if not isinstance(doc, dict):
        raise AssertionError("CI workflow must parse as a mapping")
    return doc


def _as_list(value: Any) -> list[Any]:
    if value is None:
        return []
    if isinstance(value, list):
        return value
    return [value]


def expand_native_matrix(job: dict[str, Any]) -> set[tuple[str, str]]:
    strategy = job.get("strategy") or {}
    matrix = strategy.get("matrix") or {}
    os_values = [str(item) for item in _as_list(matrix.get("os"))]
    rust_values = [str(item) for item in _as_list(matrix.get("rust"))]
    combos = {(os_name, rust) for os_name in os_values for rust in rust_values}
    for item in _as_list(matrix.get("exclude")):
        if not isinstance(item, dict):
            continue
        combos.discard((str(item.get("os")), str(item.get("rust"))))
    return combos


def job_runs_on(job: dict[str, Any]) -> str | None:
    runs_on = job.get("runs-on")
    return str(runs_on) if runs_on is not None else None


def toolchain_from_steps(job: dict[str, Any]) -> str | None:
    for step in _as_list(job.get("steps")):
        if not isinstance(step, dict):
            continue
        uses = str(step.get("uses") or "")
        if "rust-toolchain" not in uses:
            continue
        with_block = step.get("with") or {}
        if uses.endswith("@stable") and "toolchain" not in with_block:
            return STABLE
        toolchain = with_block.get("toolchain")
        if toolchain is not None:
            return str(toolchain)
    return None


def dedicated_stable_jobs(jobs: dict[str, Any]) -> set[tuple[str, str]]:
    covered: set[tuple[str, str]] = set()
    verify = jobs.get(VERIFY_JOB) or {}
    if job_runs_on(verify) == OS_UBUNTU and toolchain_from_steps(verify) == STABLE:
        covered.add((OS_UBUNTU, STABLE))
    windows = jobs.get(WINDOWS_JOB) or {}
    if job_runs_on(windows) == OS_WINDOWS and toolchain_from_steps(windows) == STABLE:
        covered.add((OS_WINDOWS, STABLE))
    return covered


def covered_combinations(doc: dict[str, Any]) -> set[tuple[str, str]]:
    jobs = doc.get("jobs") or {}
    native = jobs.get(NATIVE_JOB)
    combos: set[tuple[str, str]] = set()
    if isinstance(native, dict):
        combos |= expand_native_matrix(native)
    combos |= dedicated_stable_jobs(jobs)
    return combos


def required_combinations(msrv: str) -> set[tuple[str, str]]:
    return {(os_name, channel) for os_name in REQUIRED_OS for channel in (msrv, STABLE)}


def locked_commands(job: dict[str, Any]) -> list[str]:
    commands = []
    for step in _as_list(job.get("steps")):
        if isinstance(step, dict) and isinstance(step.get("run"), str):
            commands.append(step["run"])
    return commands


def assert_native_matrix(doc: dict[str, Any], msrv: str) -> None:
    jobs = doc.get("jobs") or {}
    assert NATIVE_JOB in jobs, f"missing {NATIVE_JOB} job"
    assert "linux-macos-msrv" not in jobs, "incomplete linux-macos-msrv job must be replaced"
    native = jobs[NATIVE_JOB]
    assert job_runs_on(native) == "${{ matrix.os }}", "native matrix must run on matrix.os"
    strategy = native.get("strategy") or {}
    assert strategy.get("fail-fast") is False, "native matrix must keep fail-fast: false"
    matrix = strategy.get("matrix") or {}
    os_values = [str(item) for item in _as_list(matrix.get("os"))]
    rust_values = [str(item) for item in _as_list(matrix.get("rust"))]
    missing_os = [name for name in REQUIRED_OS if name not in os_values]
    assert not missing_os, f"native matrix missing os entries: {missing_os}"
    assert msrv in rust_values, f"native matrix rust list must include exact MSRV {msrv}"
    assert STABLE in rust_values, "native matrix rust list must include stable"
    assert rust_values.count(msrv) == 1, "MSRV must appear once as an explicit patch pin"
    combos = expand_native_matrix(native)
    assert (OS_MACOS, STABLE) in combos, "macOS stable must remain in the native matrix"
    assert (OS_WINDOWS, msrv) in combos, "Windows MSRV must remain in the native matrix"
    assert (OS_UBUNTU, STABLE) not in combos, "Linux stable full suite stays on verify"
    assert (OS_WINDOWS, STABLE) not in combos, "Windows stable full suite stays on windows-test"
    commands = locked_commands(native)
    locked = [item for item in commands if item.startswith("cargo ") and "--locked" in item]
    assert any(item.startswith("cargo check --locked") for item in locked), (
        "native matrix must cargo check --locked"
    )
    assert any("cargo build --locked" in item for item in locked), (
        "native matrix must cargo build --locked"
    )
    assert any(item.startswith("cargo test --locked") for item in locked), (
        "native matrix must cargo test --locked"
    )
    assert all("--locked" in item for item in commands if item.startswith("cargo ")), (
        "native matrix cargo steps must use --locked"
    )
    assert toolchain_from_steps(native) == "${{ matrix.rust }}", (
        "native matrix rust-toolchain must consume matrix.rust"
    )


def assert_platform_coverage(doc: dict[str, Any], msrv: str) -> None:
    covered = covered_combinations(doc)
    required = required_combinations(msrv)
    missing = sorted(required - covered)
    assert not missing, f"missing native OS/channel combinations: {missing}"
    extra_stable_linux = (OS_UBUNTU, STABLE) in expand_native_matrix(doc["jobs"][NATIVE_JOB])
    extra_stable_windows = (OS_WINDOWS, STABLE) in expand_native_matrix(doc["jobs"][NATIVE_JOB])
    assert not extra_stable_linux, "do not duplicate the Linux stable full suite"
    assert not extra_stable_windows, "do not duplicate the Windows stable full suite"


def assert_required_gates(text: str) -> None:
    required = (
        "workflow_dispatch:",
        f"{NATIVE_JOB}:",
        "runs-on: ${{ matrix.os }}",
        "fail-fast: false",
        "timeout-minutes: 45",
        "python3 scripts/test-fidelity-evaluator.py",
        "cargo run --bin requirement-audit -- --check",
        "id: mingw",
        "x86_64-w64-mingw32-gcc",
        "x86_64-w64-mingw32-dlltool",
        "available=$($available.ToString().ToLowerInvariant())",
        "if: steps.mingw.outputs.available == 'true'",
        "if: steps.mingw.outputs.available != 'true'",
        "cargo build --bin srep --example stage6-fixed-metrics",
        "python3 scripts/compare-fixed-finders.py --decoder target/debug/srep",
        "python3 scripts/compare-m5-finder.py --decoder target/debug/srep --metrics-binary target/debug/examples/stage7-m5-metrics",
    )
    missing = [value for value in required if value not in text]
    assert not missing, f"missing CI gate: {missing}"
    assert "linux-macos-msrv:" not in text, "replace the incomplete linux-macos-msrv job"
    debug_build = text.index("      - run: cargo build --bin srep --example stage6-fixed-metrics\n")
    evidence_gate = text.index(
        "      - run: python3 scripts/compare-fixed-finders.py --decoder target/debug/srep\n"
    )
    debug_tests = text.index("      - run: cargo test --all-targets --all-features\n")
    assert "      - run: cargo build\n" not in text[:evidence_gate], (
        "plain build-only arrangement is not sufficient"
    )
    assert debug_build < evidence_gate < debug_tests, (
        "Stage 6 evidence gate must follow the debug build"
    )
    jobs = ("verify:", f"{WINDOWS_JOB}:", f"{NATIVE_JOB}:")
    for name in jobs:
        assert text.count(f"  {name}") == 1, f"{name} must appear once"


def validate(doc: dict[str, Any], text: str, msrv: str) -> None:
    assert_required_gates(text)
    assert_native_matrix(doc, msrv)
    assert_platform_coverage(doc, msrv)


def mutate_missing_os(doc: dict[str, Any]) -> dict[str, Any]:
    mutated = copy.deepcopy(doc)
    matrix = mutated["jobs"][NATIVE_JOB]["strategy"]["matrix"]
    matrix["os"] = [name for name in matrix["os"] if name != OS_MACOS]
    return mutated


def mutate_missing_stable(doc: dict[str, Any]) -> dict[str, Any]:
    mutated = copy.deepcopy(doc)
    matrix = mutated["jobs"][NATIVE_JOB]["strategy"]["matrix"]
    matrix["rust"] = [item for item in matrix["rust"] if item != STABLE]
    return mutated


def mutate_msrv_drift(doc: dict[str, Any], bogus: str) -> dict[str, Any]:
    mutated = copy.deepcopy(doc)
    matrix = mutated["jobs"][NATIVE_JOB]["strategy"]["matrix"]
    matrix["rust"] = [bogus if item != STABLE else item for item in matrix["rust"]]
    return mutated


def mutate_drop_windows_msrv(doc: dict[str, Any]) -> dict[str, Any]:
    mutated = copy.deepcopy(doc)
    matrix = mutated["jobs"][NATIVE_JOB]["strategy"]["matrix"]
    matrix.setdefault("exclude", []).append({"os": OS_WINDOWS, "rust": matrix["rust"][0]})
    return mutated


def mutate_hardcode_stable_toolchain(doc: dict[str, Any]) -> dict[str, Any]:
    mutated = copy.deepcopy(doc)
    for step in _as_list(mutated["jobs"][NATIVE_JOB].get("steps")):
        if not isinstance(step, dict):
            continue
        uses = str(step.get("uses") or "")
        if "rust-toolchain" not in uses:
            continue
        with_block = step.setdefault("with", {})
        with_block["toolchain"] = STABLE
        break
    return mutated


def self_check(doc: dict[str, Any], msrv: str) -> None:
    def expect_failure(mutated: dict[str, Any], needle: str) -> None:
        try:
            assert_native_matrix(mutated, msrv)
            assert_platform_coverage(mutated, msrv)
        except AssertionError as error:
            assert needle in str(error), f"expected {needle!r} in {error}"
            return
        raise AssertionError(f"mutation was accepted; expected failure containing {needle!r}")

    expect_failure(mutate_missing_os(doc), "missing os entries")
    expect_failure(mutate_missing_stable(doc), "must include stable")
    expect_failure(mutate_msrv_drift(doc, "1.85.0"), "exact MSRV")
    expect_failure(mutate_drop_windows_msrv(doc), "Windows MSRV")
    expect_failure(mutate_hardcode_stable_toolchain(doc), "matrix.rust")


def main() -> int:
    msrv = manifest_rust_version()
    text = WORKFLOW.read_text()
    assert msrv in text, f"workflow text must mention exact manifest rust-version {msrv}"
    doc = load_workflow()
    validate(doc, text, msrv)
    self_check(doc, msrv)
    print("CI YAML parsed by Ruby Psych")
    print(f"manifest rust-version {msrv}")
    print("native combinations: " + ", ".join(f"{os}/{rust}" for os, rust in sorted(covered_combinations(doc))))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

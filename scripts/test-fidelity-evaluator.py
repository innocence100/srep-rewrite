#!/usr/bin/env python3
"""Focused tests for fidelity evaluate progress, timeouts, and incomplete reports."""
from __future__ import annotations

import argparse
import json
import math
import os
import stat
import subprocess
import sys
import tempfile
import time
import unittest.mock
from pathlib import Path


SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))
import fidelity  # noqa: E402


TRUSTED_ROOT = Path("/tmp/opencode")
FAKE_SREP = r"""#!/usr/bin/env python3
import os
import pathlib
import shutil
import subprocess
import sys
import time

command = sys.argv[1]
if command == "compress":
    archive = pathlib.Path(sys.argv[-1])
    source = pathlib.Path(sys.argv[-2])
else:
    archive = pathlib.Path(sys.argv[2])
    source = None

name = archive.name
if not name.endswith(".srep"):
    raise SystemExit(f"unexpected archive name {name}")
stem = name[:-5]
sample_id, method = stem.rsplit("-", 1)
stage = command
marker = os.environ.get("SREP_FAIL", "")
if marker == f"{sample_id}:{method}:{stage}":
    raise SystemExit("injected command failure")

sleep_marker = os.environ.get("SREP_SLEEP", "")
if sleep_marker == f"{sample_id}:{method}:{stage}":
    child = subprocess.Popen(["sleep", "30"])
    pathlib.Path(os.environ["SREP_CHILD_PID"]).write_text(
        f"{os.getpid()} {child.pid}\n", encoding="utf-8"
    )
    time.sleep(30)

if command == "compress":
    archive.write_bytes(source.read_bytes())
elif command == "info":
    size = archive.stat().st_size
    covered = size // 2
    literals = size - covered
    sys.stdout.write(
        f"original size: {size}\n"
        f"payload size: {size}\n"
        f"blocks: 1\n"
        f"semantic matches: 1\n"
        f"covered bytes: {covered}\n"
        f"literal bytes: {literals}\n"
        f"method: {method}\n"
        f"layout: index\n"
        f"checksum: xxh3\n"
    )
elif command == "test":
    pass
elif command == "decompress":
    shutil.copyfile(archive, sys.argv[3])
else:
    raise SystemExit(f"unexpected command {command}")
"""


def tiny_corpus() -> dict[str, object]:
    samples = []
    for index in range(12):
        size = 256 + index
        samples.append({
            "id": f"fidelity-v1-{index + 1:02d}",
            "categories": ["exact"],
            "methods": list(fidelity.METHODS),
            "representative": True,
            "size": size,
        })
    return {"schema": fidelity.CORPUS_VERSION, "samples": samples}


def tiny_baseline(corpus: dict[str, object]) -> dict[str, object]:
    samples = []
    for sample in corpus["samples"]:
        size = int(sample["size"])
        covered = max(1, size // 2)
        methods = {
            method: {
                "old_covered": covered,
                "old_literals": size - covered,
                "old_matches": 1,
                "old_preprocessor_size": size,
                "old_final_size": max(1, size // 4),
            }
            for method in fidelity.METHODS
        }
        samples.append({"id": sample["id"], "methods": methods})
    return {
        "schema": "srep-fidelity-baseline-v1",
        "xz_version": "xz-test",
        "xz_argv": list(fidelity.XZ_ARGS),
        "samples": samples,
    }


def write_tiny_corpus(directory: Path, manifest: dict[str, object]) -> dict[str, Path]:
    paths: dict[str, Path] = {}
    for sample in manifest["samples"]:
        path = directory / f"{sample['id']}.bin"
        path.write_bytes(b"x" * int(sample["size"]))
        paths[sample["id"]] = path
    return paths


def fake_xz_archive(archive: Path, directory: Path, *, timeout: float | None = None) -> tuple[int, str]:
    del directory, timeout
    size = archive.stat().st_size
    return max(1, size // 4), "xz-test"


def install_fake_binary(directory: Path) -> Path:
    path = directory / "fake-srep"
    path.write_text(FAKE_SREP, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    return path


def row_count(report: dict[str, object]) -> int:
    return sum(len(sample.get("methods", {})) for sample in report.get("samples", []))


def load_report(path: Path) -> dict[str, object]:
    return json.loads(path.read_text(encoding="utf-8"))


def patched_evaluate(monkey: dict[str, object]) -> unittest.mock._patch:
    return unittest.mock.patch.multiple(
        fidelity,
        ensure_corpus_manifest=lambda: monkey["corpus"],
        strict_json=lambda path: monkey["baseline"] if path == fidelity.BASELINE_MANIFEST else fidelity.strict_json(path),
        validate_baseline=lambda *args, **kwargs: None,
        write_corpus=write_tiny_corpus,
        xz_archive=fake_xz_archive,
        retained_directory=lambda prefix: Path(tempfile.mkdtemp(prefix=f"{prefix}-", dir=TRUSTED_ROOT)),
    )


def evaluate_args(binary: Path, report: Path, timeout: float = 5.0) -> argparse.Namespace:
    return argparse.Namespace(binary=binary, report=report, command_timeout=timeout)


def test_cli_contract() -> None:
    help_text = subprocess.run(
        [sys.executable, str(SCRIPTS / "fidelity.py"), "evaluate", "--help"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    assert "--binary" in help_text
    assert "--report" in help_text
    assert "--command-timeout" in help_text
    assert "180" in help_text
    rejected = subprocess.run(
        [
            sys.executable,
            str(SCRIPTS / "fidelity.py"),
            "evaluate",
            "--binary",
            "/nonexistent",
            "--command-timeout",
            "0",
        ],
        capture_output=True,
        text=True,
    )
    assert rejected.returncode != 0
    assert "FIDELITY GATE: PASS" not in rejected.stdout
    assert "positive" in rejected.stderr
    negative = subprocess.run(
        [
            sys.executable,
            str(SCRIPTS / "fidelity.py"),
            "evaluate",
            "--binary",
            "/nonexistent",
            "--command-timeout",
            "-1",
        ],
        capture_output=True,
        text=True,
    )
    assert negative.returncode != 0
    assert "FIDELITY GATE: PASS" not in negative.stdout


def test_complete_72_row_success(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "complete.json"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.object(fidelity, "_emit_progress") as progress:
            status = fidelity.evaluate(evaluate_args(binary, report_path))
    assert status == 0
    report = load_report(report_path)
    assert report["complete"] is True
    assert report["pass"] is True
    assert report["schema"] == "srep-fidelity-report-v1"
    assert len(report["samples"]) == 12
    assert row_count(report) == 72
    assert set(report["methods"]) == set(fidelity.METHODS)
    for sample in report["samples"]:
        assert set(sample["methods"]) == set(fidelity.METHODS)
        for row in sample["methods"].values():
            assert set(row) == {
                "covered",
                "literals",
                "matches",
                "preprocessor_size",
                "final_size",
            }
    for method, summary in report["methods"].items():
        assert summary["positive_population"] == 12
        assert summary["representative_population"] == 12
        assert summary["coverage_geomean"] == "1.000000000000"
        assert summary["size_geomean"] == "1.000000000000"
        assert summary["coverage_pass"] is True
        assert summary["size_pass"] is True
        assert summary["raw_size_pass"] is True
        del method
    messages = [call.args[0] for call in progress.call_args_list]
    assert any("sample fidelity-v1-01 (1/12)" in message for message in messages)
    assert any("method m0 (1/72) stage compress" in message for message in messages)
    assert any("method m5 (72/72) stage xz" in message for message in messages)
    stages = [message for message in messages if " stage " in message]
    assert len(stages) == 72 * 5


def test_midrun_failure_persists_incomplete_report(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "incomplete.json"
    env = os.environ.copy()
    env["SREP_FAIL"] = "fidelity-v1-01:m1:compress"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path))
            except subprocess.CalledProcessError:
                pass
            else:
                raise AssertionError("mid-run failure was not raised")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert "FIDELITY GATE: PASS" not in json.dumps(report)
    assert row_count(report) == 1
    assert report["samples"][0]["id"] == "fidelity-v1-01"
    assert set(report["samples"][0]["methods"]) == {"m0"}
    error = report["error"]
    assert error["sample"] == "fidelity-v1-01"
    assert error["method"] == "m1"
    assert error["stage"] == "compress"
    assert error["command"][0] == os.fspath(binary)
    assert "compress" in error["command"]
    assert error["error"]
    assert report["failures"] == [error]


def test_timeout_reaps_process_group(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "timeout.json"
    child_pid_path = directory / "child-pid"
    env = os.environ.copy()
    env["SREP_SLEEP"] = "fidelity-v1-01:m0:compress"
    env["SREP_CHILD_PID"] = os.fspath(child_pid_path)
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, timeout=0.3))
            except fidelity.CommandTimeoutError as error:
                timed_out = error
            else:
                raise AssertionError("timeout was not raised")
    deadline = time.time() + 5
    pids: list[int] = []
    while time.time() < deadline:
        if child_pid_path.exists():
            pids = [int(value) for value in child_pid_path.read_text(encoding="utf-8").split()]
            if pids and all(not Path(f"/proc/{pid}").exists() for pid in pids):
                break
        time.sleep(0.05)
    assert pids, "timed command never recorded its process group"
    living = [pid for pid in pids if Path(f"/proc/{pid}").exists()]
    assert not living, f"timeout left live processes: {living}"
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["error"]["stage"] == "compress"
    assert report["error"]["sample"] == "fidelity-v1-01"
    assert report["error"]["method"] == "m0"
    assert "timed out" in report["error"]["error"]
    assert timed_out.timeout == 0.3
    assert row_count(report) == 0


def test_run_command_timeout_kills_grandchildren(directory: Path) -> None:
    script = directory / "sleeper.py"
    pid_file = directory / "pids"
    script.write_text(
        "import os, subprocess, sys, time\n"
        "from pathlib import Path\n"
        "child = subprocess.Popen(['sleep', '30'])\n"
        "Path(sys.argv[1]).write_text(f'{os.getpid()} {child.pid}\\n')\n"
        "time.sleep(30)\n",
        encoding="utf-8",
    )
    try:
        fidelity.run_command(
            [sys.executable, os.fspath(script), os.fspath(pid_file)],
            timeout=0.3,
            check=True,
            capture_output=True,
        )
    except fidelity.CommandTimeoutError:
        pass
    else:
        raise AssertionError("sleeper was not timed out")
    deadline = time.time() + 5
    pids: list[int] = []
    while time.time() < deadline:
        if pid_file.exists():
            pids = [int(value) for value in pid_file.read_text(encoding="utf-8").split()]
            if pids and all(not Path(f"/proc/{pid}").exists() for pid in pids):
                break
        time.sleep(0.05)
    assert pids
    living = [pid for pid in pids if Path(f"/proc/{pid}").exists()]
    assert not living, f"grandchild survived timeout: {living}"


def expected_summary(rows: list[dict[str, object]]) -> dict[str, object]:
    positive = [row for row in rows if row["old"]["covered"] > 0]
    coverage_ratios = [min(1.0, row["new"]["covered"] / row["old"]["covered"]) for row in positive]
    size_ratios = [max(1.0, row["new"]["final_size"] / row["old"]["final_size"]) for row in rows]
    raw_sizes = [row["new"]["final_size"] / row["old"]["final_size"] for row in rows]
    coverage_gm = fidelity.geometric_mean(coverage_ratios)
    size_gm = fidelity.geometric_mean(size_ratios)
    worst_coverage = min(zip(coverage_ratios, positive), key=lambda item: item[0])
    worst_size = max(zip(raw_sizes, rows), key=lambda item: item[0])
    return {
        "positive_population": len(positive),
        "representative_population": len(rows),
        "coverage_geomean": f"{coverage_gm:.12f}",
        "size_geomean": f"{size_gm:.12f}",
        "worst_coverage": {"sample": worst_coverage[1]["id"], "ratio": f"{worst_coverage[0]:.12f}"},
        "worst_size": {"sample": worst_size[1]["id"], "ratio": f"{worst_size[0]:.12f}"},
        "coverage_pass": coverage_gm >= 0.95,
        "size_pass": size_gm <= 1.05,
        "raw_size_pass": all(value <= 1.10 for value in raw_sizes),
    }


def test_arithmetic_unchanged() -> None:
    rows = []
    for index in range(12):
        old_covered = 100 + index
        old_final = 50 + index
        rows.append({
            "id": f"fidelity-v1-{index + 1:02d}",
            "old": {
                "covered": old_covered,
                "literals": 20,
                "matches": 3,
                "preprocessor_size": 80,
                "final_size": old_final,
            },
            "new": {
                "covered": old_covered if index != 3 else int(old_covered * 0.9),
                "literals": 21,
                "matches": 3,
                "preprocessor_size": 81,
                "final_size": old_final if index != 7 else int(old_final * 1.08),
            },
        })
    per_method = {method: [dict(row) for row in rows] for method in fidelity.METHODS}
    summaries, failures = fidelity.summarize_methods(per_method)
    expected = expected_summary(rows)
    for method in fidelity.METHODS:
        assert summaries[method] == expected
    passed = expected["coverage_pass"] and expected["size_pass"] and expected["raw_size_pass"]
    assert (failures == []) == passed
    if failures:
        assert failures[0]["summary"] == expected
        assert failures[0]["method"] in fidelity.METHODS
    independent_gm = math.exp(
        sum(math.log(min(1.0, row["new"]["covered"] / row["old"]["covered"])) for row in rows) / len(rows)
    )
    assert summaries["m0"]["coverage_geomean"] == f"{independent_gm:.12f}"


def test_validation_failure_is_nonzero_incomplete(directory: Path) -> None:
    report_path = directory / "validate.json"
    binary = install_fake_binary(directory)

    def fail_validate(*args, **kwargs):
        raise ValueError("baseline manifest does not match the source pin")

    with unittest.mock.patch.multiple(
        fidelity,
        ensure_corpus_manifest=lambda: tiny_corpus(),
        strict_json=lambda path: tiny_baseline(tiny_corpus()),
        validate_baseline=fail_validate,
        write_corpus=write_tiny_corpus,
        xz_archive=fake_xz_archive,
    ):
        try:
            fidelity.evaluate(evaluate_args(binary, report_path))
        except ValueError as error:
            assert "source pin" in str(error)
        else:
            raise AssertionError("validation failure was not raised")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["samples"] == []
    assert report["error"]["stage"] == "validate"
    with unittest.mock.patch.object(sys, "argv", [
        "fidelity.py",
        "evaluate",
        "--binary",
        os.fspath(binary),
        "--report",
        os.fspath(directory / "cli-validate.json"),
    ]):
        with unittest.mock.patch.multiple(
            fidelity,
            ensure_corpus_manifest=lambda: tiny_corpus(),
            strict_json=lambda path: tiny_baseline(tiny_corpus()),
            validate_baseline=fail_validate,
        ):
            status = fidelity.main()
    assert status == 1


def test_progress_is_flushed() -> None:
    source = (SCRIPTS / "fidelity.py").read_text(encoding="utf-8")
    assert "print(message, flush=True)" in source
    assert "start_new_session=True" in source
    assert "os.killpg(process.pid, sig)" in source
    assert "command-timeout" in source
    assert "DEFAULT_COMMAND_TIMEOUT = 180" in source
    evaluate_source = source.split("def evaluate(", 1)[1].split("def main(", 1)[0]
    assert "subprocess.run(" not in evaluate_source
    assert "resume" not in evaluate_source
    assert "cache" not in evaluate_source
    assert "ThreadPool" not in evaluate_source


def main() -> int:
    TRUSTED_ROOT.mkdir(mode=0o700, exist_ok=True)
    test_cli_contract()
    test_progress_is_flushed()
    test_arithmetic_unchanged()
    with tempfile.TemporaryDirectory(prefix="fidelity-evaluator-tests-", dir=TRUSTED_ROOT) as name:
        directory = Path(name)
        test_complete_72_row_success(directory)
        print("test_complete_72_row_success: passed")
        test_midrun_failure_persists_incomplete_report(directory)
        print("test_midrun_failure_persists_incomplete_report: passed")
        test_timeout_reaps_process_group(directory)
        print("test_timeout_reaps_process_group: passed")
        test_run_command_timeout_kills_grandchildren(directory)
        print("test_run_command_timeout_kills_grandchildren: passed")
        test_validation_failure_is_nonzero_incomplete(directory)
        print("test_validation_failure_is_nonzero_incomplete: passed")
    print("fidelity evaluator tests: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

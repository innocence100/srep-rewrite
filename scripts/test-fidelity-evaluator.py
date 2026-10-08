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
# The descendant-liveness assertions below intentionally use Linux /proc.
# Fidelity evaluation currently runs these subprocess tests on Linux; native
# Windows/macOS CI does not claim this Linux-specific evidence.
LINUX_PROCESS_TESTS = os.name == "posix" and Path("/proc").is_dir()
FAKE_SREP = r"""#!/usr/bin/env python3
import json
import os
import pathlib
import shutil
import subprocess
import sys
import time

# Flags that consume a separate value token, so positional arguments can be
# located exactly when resource options are present.
FLAGS_WITH_VALUE = {
    "--method", "--layout", "--checksum", "--block-size", "--min-match",
    "--seed-size", "--target-chunk", "--max-distance", "--rep-distance",
    "--rep-min-match", "--memory", "--temp-dir", "--temp-limit",
    "--output-limit",
}
raw_args = sys.argv[1:]
_argv_log = os.environ.get("SREP_ARGV_LOG", "")
if _argv_log:
    with open(_argv_log, "a", encoding="utf-8") as _handle:
        _handle.write(json.dumps(raw_args) + "\n")
if raw_args and raw_args[0] == "--version":
    if os.environ.get("SREP_VERSION_HANG") == "1":
        # Print a partial line, then hang so the configured timeout must bound
        # the probe and the captured output is still retained.
        sys.stdout.write("srep partial\n")
        sys.stdout.flush()
        time.sleep(30)
    if os.environ.get("SREP_VERSION_FAIL") == "1":
        sys.stderr.write("fake-srep: version failed\n")
        raise SystemExit(9)
    sys.stdout.write("srep test\n")
    raise SystemExit(0)
command = raw_args[0]
positionals = []
i = 1
while i < len(raw_args):
    token = raw_args[i]
    if token in FLAGS_WITH_VALUE:
        i += 2
        continue
    if token.startswith("--"):
        i += 1
        continue
    positionals.append(token)
    i += 1

reject = os.environ.get("SREP_REJECT_ARG", "")
if reject and reject in raw_args:
    sys.stderr.write(f"fake-srep: reject {reject}\n")
    raise SystemExit(7)

if os.environ.get("SREP_PROFILE") == "1" and command == "compress":
    sys.stderr.write("OFFLOAD_PROFILE event=fake-compress\n")

if command == "compress":
    source = pathlib.Path(positionals[0])
    archive = pathlib.Path(positionals[1])
elif command == "decompress":
    archive = pathlib.Path(positionals[0])
    source = None
else:
    archive = pathlib.Path(positionals[0])
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

sigterm_marker = os.environ.get("SREP_SIGTERM_DIAGNOSTIC", "")
if sigterm_marker == f"{sample_id}:{method}:{stage}":
    import signal

    def _on_sigterm(signum, frame):
        sys.stderr.write("cleanup-diagnostic\n")
        sys.stderr.flush()
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, _on_sigterm)
    # Pre-timeout OFFLOAD_PROFILE line on stderr.
    sys.stderr.write("OFFLOAD_PROFILE event=fake-pretimeout\n")
    sys.stderr.flush()
    time.sleep(30)

resistant_marker = os.environ.get("SREP_EXIT_WITH_RESISTANT_CHILD", "")
if resistant_marker == f"{sample_id}:{method}:{stage}":
    child = subprocess.Popen([
        sys.executable,
        "-c",
        "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)",
    ])
    pathlib.Path(os.environ["SREP_CHILD_PID"]).write_text(
        f"{os.getpid()} {child.pid}\n", encoding="utf-8"
    )
    raise SystemExit(0)

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
    shutil.copyfile(archive, positionals[1])
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
                # The fake xz emits size//4 bytes; a baseline one byte larger
                # makes every row a legitimate strict win (new < old), so the
                # default fixture exercises the majority gate rather than all
                # ties.
                "old_final_size": max(1, size // 4) + 1,
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


def pid_is_live(pid: int) -> bool:
    """Return whether Linux still reports a runnable/non-zombie process."""
    status = Path(f"/proc/{pid}/status")
    try:
        state_line = next(
            line for line in status.read_text(encoding="utf-8").splitlines()
            if line.startswith("State:")
        )
    except (FileNotFoundError, StopIteration):
        return False
    return not state_line.split(None, 2)[1].startswith("Z")


def patched_evaluate(
    monkey: dict[str, object], *, patch_xz: bool = True
) -> unittest.mock._patch:
    targets: dict[str, object] = {
        "ensure_corpus_manifest": lambda: monkey["corpus"],
        "strict_json": lambda path: monkey["baseline"] if path == fidelity.BASELINE_MANIFEST else fidelity.strict_json(path),
        "validate_baseline": lambda *args, **kwargs: None,
        "write_corpus": write_tiny_corpus,
        "retained_directory": lambda prefix: Path(tempfile.mkdtemp(prefix=f"{prefix}-", dir=TRUSTED_ROOT)),
    }
    if patch_xz:
        targets["xz_archive"] = fake_xz_archive
    return unittest.mock.patch.multiple(fidelity, **targets)


def evaluate_args(
    binary: Path,
    report: Path,
    timeout: float = 5.0,
    **resource: object,
) -> argparse.Namespace:
    namespace = argparse.Namespace(binary=binary, report=report, command_timeout=timeout)
    for name in ("memory", "temp_dir", "temp_limit"):
        setattr(namespace, name, resource.get(name))
    return namespace


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
    assert "--memory" in help_text
    assert "--temp-dir" in help_text
    assert "--temp-limit" in help_text
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
    # Binary identity and resource provenance are recorded in every report.
    assert report["binary_identity"]["path"] == os.fspath(binary)
    assert report["binary_identity"]["sha256"] == fidelity.file_sha256(binary)
    assert report["binary_identity"]["version"] == "srep test"
    assert report["resource"]["configured"] == {
        "memory": False,
        "temp_dir": False,
        "temp_limit": False,
    }
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
        # Per-command transcripts are retained for every successful row.
        for method in fidelity.METHODS:
            commands = sample["commands"][method]
            assert [entry["stage"] for entry in commands] == [
                "compress",
                "info",
                "test",
                "decompress",
            ]
            assert all(entry["exit_code"] == 0 for entry in commands)
            assert all("stderr" in entry and "stdout" in entry for entry in commands)
    assert report["quality_policy"]["id"] == fidelity.QUALITY_POLICY_ID
    for method, summary in report["methods"].items():
        assert summary["positive_population"] == 12
        assert summary["representative_population"] == 12
        assert summary["coverage_geomean"] == "1.000000000000"
        # Legitimate winners: fake xz emits size//4 while the baseline is one
        # byte larger, so every row is a strict win.
        assert summary["size_wins"] == 12
        assert summary["size_ties"] == 0
        assert summary["size_losses"] == 0
        assert summary["majority_size_pass"] is True
        assert summary["coverage_pass"] is True
        # Diagnostics retained, clearly legacy-named (not active gates).
        assert summary["legacy_size_geomean_pass"] is True
        assert summary["legacy_raw_size_pass"] is True
        assert "size_geomean" in summary
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
    if not LINUX_PROCESS_TESTS:
        return
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
            if pids and all(not pid_is_live(pid) for pid in pids):
                break
        time.sleep(0.05)
    assert pids, "timed command never recorded its process group"
    living = [pid for pid in pids if pid_is_live(pid)]
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
    if not LINUX_PROCESS_TESTS:
        return
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
            if pids and all(not pid_is_live(pid) for pid in pids):
                break
        time.sleep(0.05)
    assert pids
    living = [pid for pid in pids if pid_is_live(pid)]
    assert not living, f"grandchild survived timeout: {living}"


def test_timeout_kills_devnull_grandchild_after_leader_exit(directory: Path) -> None:
    if not LINUX_PROCESS_TESTS:
        return
    script = directory / "devnull-sleeper.py"
    pid_file = directory / "devnull-pids"
    script.write_text(
        "import os, signal, subprocess, sys, time\n"
        "child = subprocess.Popen([sys.executable, '-c', "
        "'import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)'], "
        "stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
        "open(sys.argv[1], 'w', encoding='utf-8').write(f'{os.getpid()} {child.pid}\\n')\n"
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
        raise AssertionError("devnull sleeper was not timed out")
    deadline = time.time() + 5
    pids: list[int] = []
    while time.time() < deadline:
        if pid_file.exists():
            try:
                pids = [int(value) for value in pid_file.read_text(encoding="utf-8").split()]
            except ValueError:
                pids = []
            if pids and all(not pid_is_live(pid) for pid in pids):
                break
        time.sleep(0.05)
    assert pids, "devnull command never recorded its process group"
    living = [pid for pid in pids if pid_is_live(pid)]
    assert not living, f"devnull timeout left live processes: {living}"


def test_timeout_reaps_group_after_leader_exit(directory: Path) -> None:
    if not LINUX_PROCESS_TESTS:
        return
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "leader-exited.json"
    child_pid_path = directory / "leader-exited-pids"
    env = os.environ.copy()
    env["SREP_EXIT_WITH_RESISTANT_CHILD"] = "fidelity-v1-01:m0:compress"
    env["SREP_CHILD_PID"] = os.fspath(child_pid_path)
    started = time.monotonic()
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, timeout=0.3))
            except fidelity.CommandTimeoutError as error:
                assert error.timeout == 0.3
            else:
                raise AssertionError("leader-exited pipe holder did not time out")
    assert time.monotonic() - started < 6, "timeout cleanup was not bounded"
    deadline = time.time() + 5
    pids: list[int] = []
    while time.time() < deadline:
        if child_pid_path.exists():
            pids = [int(value) for value in child_pid_path.read_text(encoding="utf-8").split()]
            if pids and all(not pid_is_live(pid) for pid in pids):
                break
        time.sleep(0.05)
    assert pids, "leader-exited command never recorded its process group"
    living = [pid for pid in pids if pid_is_live(pid)]
    assert not living, f"leader-exited timeout left live processes: {living}"
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["samples"] == []
    assert report["error"]["stage"] == "compress"
    assert report["error"]["sample"] == "fidelity-v1-01"
    assert report["error"]["method"] == "m0"
    assert "timed out" in report["error"]["error"]


def _logged_argv(path: Path) -> list[list[str]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _argv_for(entries: list[list[str]], command: str) -> list[str]:
    for entry in entries:
        if entry and entry[0] == command:
            return entry
    raise AssertionError(f"no logged {command} argv in {entries}")


def test_default_has_no_resource_argv(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "default-argv.json"
    argv_log = directory / "default-argv.log"
    env = os.environ.copy()
    env["SREP_ARGV_LOG"] = os.fspath(argv_log)
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            status = fidelity.evaluate(evaluate_args(binary, report_path))
    assert status == 0
    commands = _logged_argv(argv_log)
    assert commands, "fake binary never logged an invocation"
    for entry in commands:
        for flag in ("--memory", "--temp-dir", "--temp-limit"):
            assert flag not in entry, f"default run emitted {flag}: {entry}"
    report = load_report(report_path)
    assert report["resource"]["memory"] is None
    assert report["resource"]["temp_dir"] is None
    assert report["resource"]["temp_limit"] is None
    assert report["resource"]["configured"] == {
        "memory": False,
        "temp_dir": False,
        "temp_limit": False,
    }
    assert report["resource"]["defaults"]["memory_bytes"] == 256 * 1024 * 1024
    assert report["resource"]["defaults"]["temp_limit_bytes"] == 4 * 1024 * 1024 * 1024


def test_resource_flags_pass_to_right_stages(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "resource-argv.json"
    argv_log = directory / "resource-argv.log"
    temp_root = directory / "resource temp dir"
    temp_root.mkdir(exist_ok=True)
    env = os.environ.copy()
    env["SREP_ARGV_LOG"] = os.fspath(argv_log)
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            status = fidelity.evaluate(
                evaluate_args(
                    binary,
                    report_path,
                    memory="512MiB",
                    temp_dir=os.fspath(temp_root),
                    temp_limit="32GiB",
                )
            )
    assert status == 0
    commands = _logged_argv(argv_log)
    compress = _argv_for(commands, "compress")
    decompress = _argv_for(commands, "decompress")
    info = _argv_for(commands, "info")
    test = _argv_for(commands, "test")
    for argv in (compress, decompress):
        assert argv[argv.index("--memory") + 1] == "512MiB"
        assert argv[argv.index("--temp-dir") + 1] == os.fspath(temp_root)
        assert argv[argv.index("--temp-limit") + 1] == "32GiB"
    for argv in (info, test):
        for flag in ("--memory", "--temp-dir", "--temp-limit"):
            assert flag not in argv, f"{argv[0]} must not receive {flag}: {argv}"
    report = load_report(report_path)
    assert report["resource"]["memory"] == "512MiB"
    assert report["resource"]["temp_dir"] == os.fspath(temp_root)
    assert report["resource"]["temp_limit"] == "32GiB"
    assert report["resource"]["configured"] == {
        "memory": True,
        "temp_dir": True,
        "temp_limit": True,
    }


def test_resource_paths_stay_single_argv_tokens(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "quoted-argv.json"
    argv_log = directory / "quoted-argv.log"
    spaced_temp = directory / "temp dir with spaces"
    spaced_temp.mkdir(exist_ok=True)
    env = os.environ.copy()
    env["SREP_ARGV_LOG"] = os.fspath(argv_log)
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            status = fidelity.evaluate(
                evaluate_args(binary, report_path, temp_dir=os.fspath(spaced_temp))
            )
    assert status == 0
    compress = _argv_for(_logged_argv(argv_log), "compress")
    index = compress.index("--temp-dir")
    # The path is one argv token, not split on the space and not shell-quoted.
    assert compress[index + 1] == os.fspath(spaced_temp)
    assert spaced_temp.is_absolute()


def test_invalid_resource_value_propagates_and_retains_stderr(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "invalid-resource.json"
    env = os.environ.copy()
    env["SREP_REJECT_ARG"] = "--memory"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, memory="not-a-size"))
            except subprocess.CalledProcessError as error:
                assert error.returncode == 7
            else:
                raise AssertionError("invalid resource value did not propagate")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert "FIDELITY GATE: PASS" not in json.dumps(report)
    assert row_count(report) == 0
    error = report["error"]
    assert error["stage"] == "compress"
    assert error["returncode"] == 7
    assert "not-a-size" in error["command"]
    failing = [entry for entry in error["commands"] if entry["stage"] == "compress"]
    assert failing and failing[0]["exit_code"] == 7
    assert "reject --memory" in failing[0]["stderr"]


def test_offload_profile_stderr_is_retained_per_command(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "profile.json"
    env = os.environ.copy()
    env["SREP_PROFILE"] = "1"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            status = fidelity.evaluate(evaluate_args(binary, report_path))
    assert status == 0
    report = load_report(report_path)
    compress = report["samples"][0]["commands"]["m0"]
    compress_entry = next(entry for entry in compress if entry["stage"] == "compress")
    assert "OFFLOAD_PROFILE event=fake-compress" in compress_entry["stderr"]


def _row_command_stages(report: dict[str, object], sample_index: int, method: str) -> list[str]:
    return [
        entry["stage"]
        for entry in report["samples"][sample_index]["commands"][method]
    ]


def test_row_records_core_stages_and_identity_separately(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "row-stages.json"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        status = fidelity.evaluate(evaluate_args(binary, report_path))
    assert status == 0
    report = load_report(report_path)
    # The patched unit harness substitutes fake_xz_archive, so the real xz
    # transcript is asserted directly by test_xz_archive_records_artifact
    # below.  Here the four srep stages plus the separately captured identity
    # are verified.
    assert _row_command_stages(report, 0, "m0") == [
        "compress",
        "info",
        "test",
        "decompress",
    ]
    assert report["binary_identity"]["version"] == "srep test"
    assert report["binary_identity"]["sha256"] == fidelity.file_sha256(binary)


def test_xz_archive_records_artifact_on_success(directory: Path) -> None:
    archive = directory / "sample.bin"
    archive.write_bytes(b"x" * 4096)
    fidelity._COMMAND_LOG.clear()
    size, version = fidelity.xz_archive(archive, directory, timeout=30)
    assert size > 0
    assert version == fidelity.xz_version()
    entries = fidelity._COMMAND_LOG.drain()
    xz_entries = [entry for entry in entries if entry["stage"] == "xz"]
    assert len(xz_entries) == 1
    entry = xz_entries[0]
    assert entry["exit_code"] == 0
    assert entry["argv"][0] == "xz"
    assert "--format=raw" in entry["argv"]
    assert entry["artifact"]["exists"] is True
    assert entry["artifact"]["bytes"] == size
    assert len(entry["artifact"]["sha256"]) == 64
    fidelity._COMMAND_LOG.clear()


def test_identity_captured_once_and_reused_across_reports(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "identity-stable.json"
    version_log = directory / "version.log"
    env = os.environ.copy()
    # Count --version invocations: identity must be probed exactly once.
    env["SREP_ARGV_LOG"] = os.fspath(version_log)
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            status = fidelity.evaluate(evaluate_args(binary, report_path))
    assert status == 0
    version_calls = [entry for entry in _logged_argv(version_log) if entry[:1] == ["--version"]]
    assert len(version_calls) == 1, f"identity probed {len(version_calls)} times"
    report = load_report(report_path)
    expected_sha = fidelity.file_sha256(binary)
    assert report["binary_identity"]["sha256"] == expected_sha
    assert report["binary_identity"]["version"] == "srep test"


def test_binary_replaced_between_rows_is_detected(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "replaced.json"
    replacement = directory / "replacement-srep"
    replacement.write_text(FAKE_SREP + "\n# changed\n", encoding="utf-8")
    replacement.chmod(replacement.stat().st_mode | stat.S_IEXEC)
    original_hash = fidelity.file_sha256(binary)
    assert fidelity.file_sha256(replacement) != original_hash

    original_run_stage = fidelity._run_stage
    state = {"count": 0, "replaced": False}

    def replacing_run_stage(command, *, timeout):
        state["count"] += 1
        # Replace the binary after the first row has begun (between rows).
        if state["count"] == 6 and not state["replaced"]:
            state["replaced"] = True
            os.replace(replacement, binary)
        return original_run_stage(command, timeout=timeout)

    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.object(fidelity, "_run_stage", replacing_run_stage):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path))
            except fidelity.BinaryReplacedError:
                pass
            else:
                raise AssertionError("binary replacement was not detected")
    assert state["replaced"] is True
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    # STRICT: a between-rows recheck failure is labelled exactly
    # identity-recheck, never a stale prior-row command stage like xz.
    assert report["error"]["stage"] == "identity-recheck"
    assert report["error"]["command"] is None
    ctx = report["error"]["identity_context"]
    assert ctx["phase"] == "row"
    assert ctx["expected_sha256"] == original_hash
    # Row 1 method m1's third stage is the 6th _run_stage call; the replacement
    # is detected at the start of the next row (m2), before its compress.
    assert ctx["sample"] == "fidelity-v1-01"
    assert ctx["method"] == "m2"
    assert ctx["row_index"] == 3
    # Completed rows remain under the ORIGINAL identity (old hash), not the
    # replacement.
    assert report["binary_identity"]["sha256"] == original_hash
    assert report["binary_identity"]["available"] is True
    assert report["binary_identity"]["sha256"] != fidelity.file_sha256(binary)


def test_identity_unavailable_before_rows_is_partial_not_pass(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "identity-missing.json"
    original_recheck = fidelity._recheck_binary_identity
    calls = {"n": 0}

    def remove_then_recheck(target, identity):
        # The version probe has already captured identity; make the binary
        # unreadable before the first row's recheck runs.
        if calls["n"] == 0:
            calls["n"] += 1
            binary.unlink()
        return original_recheck(target, identity)

    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.object(fidelity, "_recheck_binary_identity", remove_then_recheck):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path))
            except (fidelity.IdentityUnavailableError, fidelity.BinaryReplacedError):
                pass
            else:
                raise AssertionError("unavailable identity was not detected")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["samples"] == []
    # Identity WAS established before row 1; it became unavailable on disk at
    # the row recheck, so the immutable captured identity is retained.
    assert report["binary_identity"]["available"] is True
    assert report["binary_identity"]["version"] == "srep test"
    assert report["error"]["stage"] == "identity-recheck"
    assert report["error"]["identity_context"]["phase"] == "row"
    # The successful initial probe remains in the dedicated report entry even
    # though no row ran.
    assert report["identity_probe"]["stage"] == "identity-probe"
    assert report["identity_probe"]["exit_code"] == 0
    assert "srep test" in report["identity_probe"]["stdout"]


def test_identity_replaced_after_last_row_final_check(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "final-check.json"
    replacement = directory / "final-replacement"
    replacement.write_text(FAKE_SREP + "\n# changed\n", encoding="utf-8")
    replacement.chmod(replacement.stat().st_mode | stat.S_IEXEC)
    original_hash = fidelity.file_sha256(binary)

    original_recheck = fidelity._recheck_binary_identity
    calls = {"n": 0}
    # One recheck per row (72) plus the final acceptance check (73rd).
    total = 12 * len(fidelity.METHODS)

    def replace_on_final(target, identity):
        calls["n"] += 1
        if calls["n"] == total + 1:
            os.replace(replacement, binary)
        return original_recheck(target, identity)

    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.object(fidelity, "_recheck_binary_identity", replace_on_final):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path))
            except fidelity.BinaryReplacedError:
                pass
            else:
                raise AssertionError("final acceptance replacement was not detected")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["error"]["stage"] == "identity-final-check"
    assert report["error"]["command"] is None
    assert report["error"]["identity_context"]["phase"] == "final"
    assert report["error"]["identity_context"]["completed_rows"] == total
    # All 72 rows remain under the original identity.
    assert sum(len(s.get("methods", {})) for s in report["samples"]) == total
    assert report["binary_identity"]["sha256"] == original_hash


def test_identity_unavailable_after_last_row_final_check(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "final-missing.json"
    original_recheck = fidelity._recheck_binary_identity
    calls = {"n": 0}
    total = 12 * len(fidelity.METHODS)

    def remove_on_final(target, identity):
        calls["n"] += 1
        if calls["n"] == total + 1:
            binary.unlink()
        return original_recheck(target, identity)

    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.object(fidelity, "_recheck_binary_identity", remove_on_final):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path))
            except (fidelity.IdentityUnavailableError, fidelity.BinaryReplacedError):
                pass
            else:
                raise AssertionError("final acceptance unavailability was not detected")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["error"]["stage"] == "identity-final-check"
    assert report["error"]["identity_context"]["phase"] == "final"
    assert sum(len(s.get("methods", {})) for s in report["samples"]) == total
    # The identity WAS established earlier, so the block keeps it with
    # available true; only the final on-disk check failed.  The binary is now
    # gone, so its current on-disk hash is unavailable.
    assert report["binary_identity"]["available"] is True
    assert report["binary_identity"]["sha256"] is not None
    assert fidelity._safe_file_sha256(binary) is None


def test_identity_probe_timeout_bounded_and_partial(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "identity-hang.json"
    env = os.environ.copy()
    env["SREP_VERSION_HANG"] = "1"
    started = time.monotonic()
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, timeout=0.5))
            except fidelity.IdentityUnavailableError as error:
                assert error.probe is not None
                assert error.probe["termination"] == "timeout"
                assert error.sha is not None
            else:
                raise AssertionError("hanging --version was not bounded")
    assert time.monotonic() - started < 6, "identity timeout was not bounded"
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert report["samples"] == []
    # Explicit partial identity: available false, version none, partial SHA.
    assert report["binary_identity"]["available"] is False
    assert report["binary_identity"]["version"] is None
    assert report["binary_identity"]["sha256"] == fidelity.file_sha256(binary)
    # The dedicated identity-probe entry is retained with the timeout facts.
    probe = report["identity_probe"]
    assert probe["stage"] == "identity-probe"
    assert probe["termination"] == "timeout"
    assert probe["exit_code"] is None
    assert probe["argv"][-1] == "--version"
    assert "partial" in probe["stdout"]
    assert report["error"]["stage"] == "identity"


def test_successful_identity_probe_is_retained_dedicated_entry(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "identity-probe-success.json"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        status = fidelity.evaluate(evaluate_args(binary, report_path))
    assert status == 0
    report = load_report(report_path)
    probe = report["identity_probe"]
    assert probe is not None
    assert probe["stage"] == "identity-probe"
    assert probe["exit_code"] == 0
    assert probe["argv"][-1] == "--version"
    assert "srep test" in probe["stdout"]
    assert report["binary_identity"]["available"] is True
    # The probe is NOT duplicated into any per-row command list.
    for sample in report["samples"]:
        for commands in sample["commands"].values():
            assert all(entry["stage"] != "identity-probe" for entry in commands)


def test_timeout_retains_captured_output_and_offload_profile(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "timeout-capture.json"
    env = os.environ.copy()
    env["SREP_SLEEP"] = "fidelity-v1-01:m0:compress"
    env["SREP_CHILD_PID"] = os.fspath(directory / "timeout-capture-child")
    env["SREP_PROFILE"] = "1"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, timeout=0.5))
            except fidelity.CommandTimeoutError:
                pass
            else:
                raise AssertionError("compress hang was not timed out")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    commands = report["error"]["commands"]
    compress_entry = next(entry for entry in commands if entry["stage"] == "compress")
    assert compress_entry["termination"] == "timeout"
    assert compress_entry["exit_code"] is None
    assert "timed out" in compress_entry["error"]
    # The stderr printed before the hang (OFFLOAD_PROFILE) is retained.
    assert "OFFLOAD_PROFILE event=fake-compress" in compress_entry["stderr"]


def test_sigterm_child_diagnostic_retained_in_report(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "sigterm-diag.json"
    env = os.environ.copy()
    env["SREP_SIGTERM_DIAGNOSTIC"] = "fidelity-v1-01:m0:compress"
    env["SREP_CHILD_PID"] = os.fspath(directory / "sigterm-child")
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, timeout=0.5))
            except fidelity.CommandTimeoutError:
                pass
            else:
                raise AssertionError("child with SIGTERM handler was not timed out")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    compress_entry = next(
        entry for entry in report["error"]["commands"] if entry["stage"] == "compress"
    )
    # Both the pre-timeout stderr and the SIGTERM-handler diagnostic survive.
    assert "OFFLOAD_PROFILE event=fake-pretimeout" in compress_entry["stderr"]
    assert "cleanup-diagnostic" in compress_entry["stderr"]
    assert compress_entry["termination"] == "timeout"


def test_reaper_returns_most_complete_output_without_duplication(directory: Path) -> None:
    script = directory / "signal-writer.py"
    script.write_text(
        "import signal, sys, time\n"
        "def _on_term(signum, frame):\n"
        "    sys.stdout.write('REAPED-OUT\\n')\n"
        "    sys.stdout.flush()\n"
        "    raise SystemExit(0)\n"
        "signal.signal(signal.SIGTERM, _on_term)\n"
        "sys.stdout.write('PREFIX\\n')\n"
        "sys.stdout.flush()\n"
        "time.sleep(30)\n",
        encoding="utf-8",
    )
    try:
        fidelity.run_command(
            [sys.executable, os.fspath(script)],
            timeout=0.5,
            check=True,
            capture_output=True,
            text=True,
        )
    except fidelity.CommandTimeoutError as error:
        captured = error.captured_stdout or ""
        # Most complete output retained (the pre-hang prefix is not lost).
        assert "PREFIX" in captured
        # No duplicated prefix when communicate() returned the full stream.
        assert captured.count("PREFIX") == 1
        assert "REAPED-OUT" in captured
    else:
        raise AssertionError("signal-writer was not timed out")


def _install_hanging_xz(directory: Path) -> Path:
    shadow = directory / "bin-hang"
    shadow.mkdir(exist_ok=True)
    xz = shadow / "xz"
    xz.write_text(
        "#!/bin/sh\n"
        "printf 'PARTIAL-XZ-BYTES'\n"
        "echo 'xz: about to hang' >&2\n"
        "sleep 30\n",
        encoding="utf-8",
    )
    xz.chmod(xz.stat().st_mode | stat.S_IEXEC)
    return shadow


def test_xz_timeout_records_partial_artifact_and_transcript(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "xz-timeout.json"
    shadow = _install_hanging_xz(directory)
    env = os.environ.copy()
    env["PATH"] = os.fspath(shadow) + os.pathsep + env.get("PATH", "")
    with patched_evaluate(
        {"corpus": corpus, "baseline": baseline}, patch_xz=False
    ):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path, timeout=0.5))
            except fidelity.CommandTimeoutError:
                pass
            else:
                raise AssertionError("hanging xz was not timed out")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    xz_entry = next(entry for entry in report["error"]["commands"] if entry["stage"] == "xz")
    assert xz_entry["termination"] == "timeout"
    assert xz_entry["exit_code"] is None
    artifact = xz_entry["artifact"]
    assert artifact["exists"] is True
    assert artifact["bytes"] > 0
    assert len(artifact["sha256"]) == 64
    assert "xz: about to hang" in xz_entry["stderr"]


def _install_failing_xz(directory: Path) -> Path:
    shadow = directory / "bin"
    shadow.mkdir(exist_ok=True)
    xz = shadow / "xz"
    xz.write_text(
        "#!/bin/sh\n"
        "echo 'xz: injected failure' >&2\n"
        "printf 'partial'\n"
        "exit 3\n",
        encoding="utf-8",
    )
    xz.chmod(xz.stat().st_mode | stat.S_IEXEC)
    return shadow


def test_xz_failure_records_argv_status_stderr_artifact(directory: Path) -> None:
    corpus = tiny_corpus()
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "xz-fail.json"
    shadow = _install_failing_xz(directory)
    env = os.environ.copy()
    env["PATH"] = os.fspath(shadow) + os.pathsep + env.get("PATH", "")
    # Do NOT patch xz_archive here: the injected failing `xz` on PATH must be
    # what actually runs; the real xz_archive path is under test.
    with patched_evaluate(
        {"corpus": corpus, "baseline": baseline}, patch_xz=False
    ):
        with unittest.mock.patch.dict(os.environ, env, clear=False):
            try:
                fidelity.evaluate(evaluate_args(binary, report_path))
            except subprocess.CalledProcessError as error:
                assert error.returncode == 3
            else:
                raise AssertionError("xz failure did not propagate")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    commands = report["error"]["commands"]
    xz_entry = next(entry for entry in commands if entry["stage"] == "xz")
    assert xz_entry["exit_code"] == 3
    assert "xz: injected failure" in xz_entry["stderr"]
    assert xz_entry["argv"][0] == "xz"
    assert "--format=raw" in xz_entry["argv"]
    assert xz_entry["artifact"]["exists"] is True
    assert xz_entry["artifact"]["bytes"] is not None
    # Completed prior stages are retained in the same failed-row transcript.
    stages = [entry["stage"] for entry in commands]
    assert stages[:4] == ["compress", "info", "test", "decompress"]


def _policy_rows(*, wins: int, ties: int, losses: int) -> list[dict[str, object]]:
    """Synthetic per-method rows with exact win/tie/loss counts (12 total)."""
    rows = []
    assert wins + ties + losses == len(fidelity.POLICY_FIXED_SAMPLE_IDS)
    ids = list(fidelity.POLICY_FIXED_SAMPLE_IDS)
    for slot in range(wins):
        rows.append({"id": ids[slot], "old": {"covered": 10, "final_size": 100},
                     "new": {"covered": 10, "final_size": 99}})
    for slot in range(wins, wins + ties):
        rows.append({"id": ids[slot], "old": {"covered": 10, "final_size": 100},
                     "new": {"covered": 10, "final_size": 100}})
    for slot in range(wins + ties, wins + ties + losses):
        # 150/100 = 1.50 > 1.10 so the legacy raw diagnostic fires on losses
        # without affecting the strict-majority gate.
        rows.append({"id": ids[slot], "old": {"covered": 10, "final_size": 100},
                     "new": {"covered": 10, "final_size": 150}})
    return rows


def _policy_summary(wins: int, ties: int, losses: int) -> dict[str, object]:
    per_method = {method: _policy_rows(wins=wins, ties=ties, losses=losses) for method in fidelity.METHODS}
    summaries, _ = fidelity.summarize_methods(per_method)
    return dict(summaries["m0"])


def test_policy_strict_majority_boundaries() -> None:
    # Exact boundary: 7 wins / 5 losses => strict majority PASS (7*2 > 12),
    # even though a legacy diagnostic raw ratio exceeds 1.10.
    seven = _policy_summary(7, 0, 5)
    assert seven["size_wins"] == 7 and seven["size_losses"] == 5
    assert seven["majority_size_pass"] is True
    assert seven["legacy_raw_size_pass"] is False  # diagnostic only
    # Exact boundary: 6 wins / 6 ties => FAIL (6*2 is not > 12; ties do not win).
    six_ties = _policy_summary(6, 6, 0)
    assert six_ties["size_wins"] == 6 and six_ties["size_ties"] == 6
    assert six_ties["majority_size_pass"] is False
    # 6 wins / 6 losses => FAIL.
    six_losses = _policy_summary(6, 0, 6)
    assert six_losses["majority_size_pass"] is False
    # 0 wins / 12 ties => FAIL.
    no_wins = _policy_summary(0, 12, 0)
    assert no_wins["majority_size_pass"] is False
    # 11 wins / 1 loss => PASS and the loss is reported.
    eleven = _policy_summary(11, 0, 1)
    assert eleven["majority_size_pass"] is True
    assert eleven["size_losses"] == 1


def test_policy_method_failure_fails_overall_despite_global_majority() -> None:
    # 5 methods all-win, 1 method all-tie: global wins (60/72) exceed half, but
    # the per-method strict-majority gate must fail the overall result.
    per_method = {method: _policy_rows(wins=12, ties=0, losses=0) for method in fidelity.METHODS}
    per_method["m3"] = _policy_rows(wins=0, ties=12, losses=0)
    summaries, failures = fidelity.summarize_methods(per_method)
    assert summaries["m0"]["majority_size_pass"] is True
    assert summaries["m3"]["majority_size_pass"] is False
    failing_methods = {f["method"] for f in failures}
    assert failing_methods == {"m3"}


def test_policy_coverage_failure_still_fails() -> None:
    rows = _policy_rows(wins=12, ties=0, losses=0)
    # Drop coverage on a positive row far below the 0.95 geomean gate.
    rows[0]["new"]["covered"] = 1
    per_method = {method: [dict(r) for r in rows] for method in fidelity.METHODS}
    summaries, failures = fidelity.summarize_methods(per_method)
    assert summaries["m0"]["majority_size_pass"] is True
    assert summaries["m0"]["coverage_pass"] is False
    assert failures and failures[0]["method"] == "m0"


def test_policy_equal_size_is_not_a_win() -> None:
    summary = _policy_summary(0, 12, 0)
    assert summary["size_wins"] == 0
    assert summary["size_ties"] == 12
    assert summary["majority_size_pass"] is False
    for comparison in summary["majority_comparisons"]:
        assert comparison["verdict"] == "tie"


def test_policy_rejects_non_positive_zero_and_non_integer() -> None:
    # A zero new size is not a win and must be rejected.
    zero = _policy_rows(wins=7, ties=0, losses=5)
    zero[0]["new"]["final_size"] = 0
    try:
        fidelity.majority_size_verdict("m0", zero)
    except RuntimeError:
        pass
    else:
        raise AssertionError("zero new size accepted")
    # Negative old size rejected.
    neg = _policy_rows(wins=7, ties=0, losses=5)
    neg[0]["old"]["final_size"] = -5
    try:
        fidelity.majority_size_verdict("m0", neg)
    except RuntimeError:
        pass
    else:
        raise AssertionError("negative old size accepted")
    # Non-integer (float) size rejected so no float truncation can slip through.
    bad_type = _policy_rows(wins=7, ties=0, losses=5)
    bad_type[0]["new"]["final_size"] = 99.5
    try:
        fidelity.majority_size_verdict("m0", bad_type)
    except RuntimeError:
        pass
    else:
        raise AssertionError("float size accepted")
    # Empty population must raise, not silently pass.
    try:
        fidelity.majority_size_verdict("m0", [])
    except RuntimeError:
        pass
    else:
        raise AssertionError("empty population accepted")


def _fixed_population() -> dict[str, list[dict[str, object]]]:
    return {method: _policy_rows(wins=12, ties=0, losses=0) for method in fidelity.METHODS}


def test_policy_rejects_population_shape_corruption() -> None:
    # Missing row (11 not 12) must raise, not silently pass.
    missing_row = _fixed_population()
    missing_row["m0"] = missing_row["m0"][:11]
    try:
        fidelity.summarize_methods(missing_row)
    except RuntimeError:
        pass
    else:
        raise AssertionError("short population accepted")
    # Duplicate sample id must raise.
    dup = _fixed_population()
    dup["m0"][1]["id"] = dup["m0"][0]["id"]
    try:
        fidelity.summarize_methods(dup)
    except RuntimeError:
        pass
    else:
        raise AssertionError("duplicate id accepted")
    # Unexpected / wrong id must raise.
    wrong = _fixed_population()
    wrong["m0"][0]["id"] = "not-a-policy-sample"
    try:
        fidelity.summarize_methods(wrong)
    except RuntimeError:
        pass
    else:
        raise AssertionError("unexpected id accepted")
    # Missing method key must raise.
    no_method = _fixed_population()
    del no_method["m5"]
    try:
        fidelity.summarize_methods(no_method)
    except KeyError:
        pass
    else:
        raise AssertionError("missing method accepted")


def test_evaluate_rejects_malformed_corpus_duplicate_ids(directory: Path) -> None:
    # Call the PRODUCTION evaluate path with a tampered corpus (duplicate sample
    # id) so the completeness/validation guard is actually exercised, not just
    # asserted by set membership.
    corpus = tiny_corpus()
    corpus["samples"][1]["id"] = corpus["samples"][0]["id"]
    baseline = tiny_baseline(corpus)
    binary = install_fake_binary(directory)
    report_path = directory / "duplicate-corpus.json"
    with patched_evaluate({"corpus": corpus, "baseline": baseline}):
        try:
            fidelity.evaluate(evaluate_args(binary, report_path))
        except (RuntimeError, ValueError):
            pass
        else:
            raise AssertionError("duplicate-id corpus did not fail evaluate")
    report = load_report(report_path)
    assert report["complete"] is False
    assert report["pass"] is False
    assert "FIDELITY GATE: PASS" not in json.dumps(report)
    assert report.get("error"), "failure payload missing"


def test_policy_old_report_immutability_self_contained(directory: Path) -> None:
    # Self-contained: an old all-ties FAIL report (pass:false) must remain
    # unchanged across a new-policy summarize call; summarize must not mutate
    # its inputs.  This proves input immutability without referencing the
    # production workspace or claiming the absolute report is verified here.
    per_method = {method: _policy_rows(wins=0, ties=12, losses=0) for method in fidelity.METHODS}
    before = json.dumps(per_method, sort_keys=True)
    summaries, failures = fidelity.summarize_methods(per_method)
    assert summaries["m0"]["majority_size_pass"] is False
    assert failures and {f["method"] for f in failures} == set(fidelity.METHODS)
    after = json.dumps(per_method, sort_keys=True)
    assert before == after, "summarize_methods mutated its input rows"


def expected_summary(rows: list[dict[str, object]]) -> dict[str, object]:
    positive = [row for row in rows if row["old"]["covered"] > 0]
    coverage_ratios = [min(1.0, row["new"]["covered"] / row["old"]["covered"]) for row in positive]
    size_ratios = [max(1.0, row["new"]["final_size"] / row["old"]["final_size"]) for row in rows]
    raw_sizes = [row["new"]["final_size"] / row["old"]["final_size"] for row in rows]
    coverage_gm = fidelity.geometric_mean(coverage_ratios)
    size_gm = fidelity.geometric_mean(size_ratios)
    worst_coverage = min(zip(coverage_ratios, positive), key=lambda item: item[0])
    worst_size = max(zip(raw_sizes, rows), key=lambda item: item[0])
    wins = sum(1 for r in rows if r["new"]["final_size"] < r["old"]["final_size"])
    ties = sum(1 for r in rows if r["new"]["final_size"] == r["old"]["final_size"])
    losses = sum(1 for r in rows if r["new"]["final_size"] > r["old"]["final_size"])
    majority = wins * 2 > len(rows)
    return {
        "positive_population": len(positive),
        "representative_population": len(rows),
        "coverage_geomean": f"{coverage_gm:.12f}",
        "coverage_pass": coverage_gm >= 0.95,
        "size_wins": wins,
        "size_ties": ties,
        "size_losses": losses,
        "size_win_fraction": f"{wins / len(rows):.12f}",
        "majority_size_pass": majority,
        "size_geomean": f"{size_gm:.12f}",
        "worst_coverage": {"sample": worst_coverage[1]["id"], "ratio": f"{worst_coverage[0]:.12f}"},
        "worst_size": {"sample": worst_size[1]["id"], "ratio": f"{worst_size[0]:.12f}"},
        "legacy_size_geomean_pass": size_gm <= 1.05,
        "legacy_raw_size_pass": all(value <= 1.10 for value in raw_sizes),
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
        summary = dict(summaries[method])
        comparisons = summary.pop("majority_comparisons")
        assert summary == expected
        assert len(comparisons) == 12
        assert all(c["verdict"] in {"win", "tie", "loss"} for c in comparisons)
    # The active gate is coverage AND per-method strict majority.
    passed = expected["coverage_pass"] and expected["majority_size_pass"]
    assert (failures == []) == passed
    if failures:
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
    assert fidelity._command_timeout("none") is None
    assert fidelity._command_timeout("NONE") is None
    assert fidelity._command_timeout("180") == 180.0
    for invalid in ("0", "-1", "nan", "inf", "invalid"):
        try:
            fidelity._command_timeout(invalid)
        except argparse.ArgumentTypeError:
            pass
        else:
            raise AssertionError(f"accepted invalid timeout: {invalid}")
    TRUSTED_ROOT.mkdir(mode=0o700, exist_ok=True)
    test_cli_contract()
    test_progress_is_flushed()
    test_arithmetic_unchanged()
    test_policy_strict_majority_boundaries()
    print("test_policy_strict_majority_boundaries: passed")
    test_policy_method_failure_fails_overall_despite_global_majority()
    print("test_policy_method_failure_fails_overall_despite_global_majority: passed")
    test_policy_coverage_failure_still_fails()
    print("test_policy_coverage_failure_still_fails: passed")
    test_policy_equal_size_is_not_a_win()
    print("test_policy_equal_size_is_not_a_win: passed")
    test_policy_rejects_non_positive_zero_and_non_integer()
    print("test_policy_rejects_non_positive_zero_and_non_integer: passed")
    test_policy_rejects_population_shape_corruption()
    print("test_policy_rejects_population_shape_corruption: passed")
    with tempfile.TemporaryDirectory(prefix="fidelity-evaluator-tests-", dir=TRUSTED_ROOT) as name:
        directory = Path(name)
        test_complete_72_row_success(directory)
        print("test_complete_72_row_success: passed")
        test_default_has_no_resource_argv(directory)
        print("test_default_has_no_resource_argv: passed")
        test_resource_flags_pass_to_right_stages(directory)
        print("test_resource_flags_pass_to_right_stages: passed")
        test_resource_paths_stay_single_argv_tokens(directory)
        print("test_resource_paths_stay_single_argv_tokens: passed")
        test_offload_profile_stderr_is_retained_per_command(directory)
        print("test_offload_profile_stderr_is_retained_per_command: passed")
        test_row_records_core_stages_and_identity_separately(directory)
        print("test_row_records_core_stages_and_identity_separately: passed")
        test_xz_archive_records_artifact_on_success(directory)
        print("test_xz_archive_records_artifact_on_success: passed")
        test_identity_captured_once_and_reused_across_reports(directory)
        print("test_identity_captured_once_and_reused_across_reports: passed")
        test_successful_identity_probe_is_retained_dedicated_entry(directory)
        print("test_successful_identity_probe_is_retained_dedicated_entry: passed")
        test_binary_replaced_between_rows_is_detected(directory)
        print("test_binary_replaced_between_rows_is_detected: passed")
        test_identity_unavailable_before_rows_is_partial_not_pass(directory)
        print("test_identity_unavailable_before_rows_is_partial_not_pass: passed")
        test_identity_replaced_after_last_row_final_check(directory)
        print("test_identity_replaced_after_last_row_final_check: passed")
        test_identity_unavailable_after_last_row_final_check(directory)
        print("test_identity_unavailable_after_last_row_final_check: passed")
        test_identity_probe_timeout_bounded_and_partial(directory)
        print("test_identity_probe_timeout_bounded_and_partial: passed")
        test_timeout_retains_captured_output_and_offload_profile(directory)
        print("test_timeout_retains_captured_output_and_offload_profile: passed")
        test_sigterm_child_diagnostic_retained_in_report(directory)
        print("test_sigterm_child_diagnostic_retained_in_report: passed")
        test_reaper_returns_most_complete_output_without_duplication(directory)
        print("test_reaper_returns_most_complete_output_without_duplication: passed")
        test_xz_timeout_records_partial_artifact_and_transcript(directory)
        print("test_xz_timeout_records_partial_artifact_and_transcript: passed")
        test_xz_failure_records_argv_status_stderr_artifact(directory)
        print("test_xz_failure_records_argv_status_stderr_artifact: passed")
        test_invalid_resource_value_propagates_and_retains_stderr(directory)
        print("test_invalid_resource_value_propagates_and_retains_stderr: passed")
        test_midrun_failure_persists_incomplete_report(directory)
        print("test_midrun_failure_persists_incomplete_report: passed")
        test_timeout_reaps_process_group(directory)
        print("test_timeout_reaps_process_group: passed")
        test_run_command_timeout_kills_grandchildren(directory)
        print("test_run_command_timeout_kills_grandchildren: passed")
        test_timeout_kills_devnull_grandchild_after_leader_exit(directory)
        print("test_timeout_kills_devnull_grandchild_after_leader_exit: passed")
        test_timeout_reaps_group_after_leader_exit(directory)
        print("test_timeout_reaps_group_after_leader_exit: passed")
        test_evaluate_rejects_malformed_corpus_duplicate_ids(directory)
        print("test_evaluate_rejects_malformed_corpus_duplicate_ids: passed")
        test_policy_old_report_immutability_self_contained(directory)
        print("test_policy_old_report_immutability_self_contained: passed")
        test_validation_failure_is_nonzero_incomplete(directory)
        print("test_validation_failure_is_nonzero_incomplete: passed")
    print("fidelity evaluator tests: PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

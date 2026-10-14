#!/usr/bin/env python3
"""Fast unit tests for scripts/benchmark-sparse.py.

These tests never run a real srep binary or a long compression.  They use small
Python mock binaries that speak just enough of the srep CLI to exercise the
harness: identity capture, frozen-recipe input verification, serial baseline/
candidate execution, OFFLOAD_PROFILE parsing, timeout reaping, archive-identity
comparison, temp cleanup, and partial-report persistence.
"""
from __future__ import annotations

import importlib.util
import json
import os
import signal
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
WORK_ROOT = Path("/tmp/opencode")
WORK_ROOT.mkdir(mode=0o700, exist_ok=True)

spec = importlib.util.spec_from_file_location("benchmark_sparse", SCRIPTS / "benchmark-sparse.py")
assert spec is not None and spec.loader is not None
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


# A mock srep that supports --version, compress, info, test, decompress.
# Behaviour is controlled by MOCK_* environment variables and by the ROLE
# constant substituted per generated file.
MOCK_TEMPLATE = r'''#!{python}
import json
import os
import pathlib
import sys
import time

ROLE = {role!r}
FLAGS_WITH_VALUE = {{
    "--method", "--layout", "--checksum", "--block-size", "--min-match",
    "--target-chunk", "--memory", "--temp-dir", "--temp-limit", "--output-limit",
}}
raw = sys.argv[1:]
log = os.environ.get("MOCK_ARGV_LOG")
if log:
    with open(log, "a", encoding="utf-8") as handle:
        handle.write(ROLE + " " + json.dumps(raw) + "\n")

env = os.environ.get


def stage_matches(stage):
    return env("MOCK_FAIL_STAGE") == stage


if env("MOCK_TERM_CHILD") == "1":
    import signal
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    with open(env("MOCK_CHILD_PID"), "w", encoding="utf-8") as handle:
        handle.write(str(os.getpid()))
    time.sleep(30)
    raise SystemExit(0)


if raw and raw[0] == "--version":
    if env("MOCK_VERSION_FAIL"):
        sys.stderr.write("mock: version failed\n")
        raise SystemExit(9)
    if env("MOCK_VERSION_HANG"):
        sys.stdout.write("mock partial\n")
        sys.stdout.flush()
        time.sleep(30)
    sys.stdout.write("srep mock 0.0.0\n")
    raise SystemExit(0)

command = raw[0]
positionals = []
temp_dir_value = None
i = 1
while i < len(raw):
    token = raw[i]
    if token in FLAGS_WITH_VALUE:
        if token == "--temp-dir" and i + 1 < len(raw):
            temp_dir_value = raw[i + 1]
        i += 2
        continue
    if token.startswith("--"):
        i += 1
        continue
    positionals.append(token)
    i += 1

# Optional binary mutation mid-run to exercise post-run identity recheck.
if env("MOCK_MUTATE_BINARY") == ROLE and command == "compress":
    pathlib.Path(sys.argv[0]).open("a", encoding="utf-8").write("\n# mutated\n")


def emit_profile(ok):
    mode = env("MOCK_PROFILE")
    if mode == "missing":
        return
    if mode == "malformed":
        sys.stderr.write("OFFLOAD_PROFILE not-a-valid-line\n")
        return
    p1 = dict(elapsed=10, ram=100, peak=200, temp=300, tpeak=400, io=1000)
    fin = dict(elapsed=250, ram=50, peak=210, temp=0, tpeak=5000, io=2000, ok=ok)

    if mode == "ok_false":
        fin["ok"] = False
    elif mode == "ok_missing":
        fin["ok"] = None
    elif mode == "negative":
        fin["ok"] = True
        fin["ram"] = -1
    elif mode == "ram_exceeds_peak":
        fin["ok"] = True
        fin["ram"] = 999
        fin["peak"] = 100
    elif mode == "budget_exceed":
        fin["ok"] = True
        fin["peak"] = 1 << 40
    elif mode == "temp_budget_exceed":
        fin["ok"] = True
        fin["tpeak"] = 1 << 40
    elif mode == "decreasing_elapsed":
        fin["ok"] = True
        fin["elapsed"] = 5
    elif mode == "decreasing_ram_peak":
        fin["ok"] = True
        fin["peak"] = 100
    elif mode == "decreasing_temp_peak":
        fin["ok"] = True
        fin["tpeak"] = 100
    elif mode == "decreasing_io":
        fin["ok"] = True
        fin["io"] = 500
    elif mode == "ambiguous_ok":
        fin["ok"] = ("true", "false")
    elif mode == "multiple_ok":
        fin["ok"] = ("true", "true")
    elif mode == "ok_on_nonterminal":
        p1["ok"] = True

    def line(event, data):
        text = ("OFFLOAD_PROFILE event=" + event +
                " elapsed_ms=" + str(data["elapsed"]) +
                " ram=" + str(data["ram"]) + " ram_peak=" + str(data["peak"]) +
                " temp=" + str(data["temp"]) + " temp_peak=" + str(data["tpeak"]) +
                " handles=1 files=0 io=" + str(data["io"]))
        okv = data.get("ok", None)
        if isinstance(okv, tuple):
            text += "".join(" ok=" + str(v).lower() for v in okv)
        elif okv is not None:
            text += " ok=" + str(okv).lower()
        return text

    sys.stderr.write(line("pass1", p1) + "\n")
    if mode == "post_complete":
        sys.stderr.write(line("complete", fin) + "\n")
        trailing = dict(elapsed=260, ram=50, peak=220, temp=0, tpeak=5000, io=2100)
        sys.stderr.write(line("trailing", trailing) + "\n")
        return
    if mode == "duplicate_complete":
        sys.stderr.write(line("complete", fin) + "\n")
        sys.stderr.write(line("complete", fin) + "\n")
        return
    sys.stderr.write(line("complete", fin) + "\n")


if env("MOCK_SLEEP_STAGE") == command:
    if env("MOCK_START_MARKER"):
        pathlib.Path(env("MOCK_START_MARKER")).write_text("started")
    if env("MOCK_TERM_IGNORE") == ROLE and command == "compress":
        # Spawn a descendant (this same mock with MOCK_TERM_CHILD=1) that
        # ignores SIGTERM and records its PID, while the leader (which dies on
        # SIGTERM) keeps running until the timeout.  Only the group SIGKILL
        # sent after grace can reach the surviving child.
        import subprocess as _sp
        child_env = dict(os.environ)
        child_env["MOCK_TERM_CHILD"] = "1"
        _sp.Popen([sys.executable, sys.argv[0]], env=child_env)
        time.sleep(30)
    child = __import__("subprocess").Popen(["sleep", "30"])
    marker = env("MOCK_CHILD_PID")
    if marker:
        pathlib.Path(marker).write_text(f"{{os.getpid()}} {{child.pid}}\n")
    time.sleep(30)

if stage_matches(command):
    sys.stderr.write("mock: injected failure at " + command + "\n")
    raise SystemExit(8)

if command == "compress":
    source = pathlib.Path(positionals[0])
    archive = pathlib.Path(positionals[1])
    data = source.read_bytes()
    magic = b"MOCK-ARCHIVE"
    if env("MOCK_ARCHIVE_VARIANT") == ROLE:
        magic = b"MOCK-VARIANT"
    archive.write_bytes(magic + b"|" + data)
    if env("MOCK_TEMP_RESIDUE") == ROLE and temp_dir_value:
        pathlib.Path(temp_dir_value, "residue.tmp").write_bytes(b"x")
    emit_profile(True)
    raise SystemExit(0)

archive = pathlib.Path(positionals[0])

if command == "info":
    data = archive.read_bytes()
    payload = data.split(b"|", 1)[1] if b"|" in data else b""
    original = env("MOCK_INFO_SIZE") or str(len(payload))
    method = env("MOCK_INFO_METHOD") or "m3"
    sys.stdout.write("method: " + method + "\nlayout: index\nchecksum: xxh3\n")
    sys.stdout.write("original size: " + original + "\n")
    sys.stdout.write("payload size: 2816\n")
    sys.stdout.write("blocks: 64\n")
    sys.stdout.write("semantic matches: 4\n")
    sys.stdout.write("covered bytes: 521728\n")
    sys.stdout.write("literal bytes: 2560\n")
    if env("MOCK_INFO_DUP"):
        sys.stdout.write("original size: " + original + "\n")
    if env("MOCK_INFO_NEGATIVE"):
        sys.stdout.write("semantic matches: -1\n")
    raise SystemExit(0)

if command == "test":
    raise SystemExit(0)

if command == "decompress":
    data = archive.read_bytes()
    payload = data.split(b"|", 1)[1] if b"|" in data else b""
    pathlib.Path(positionals[1]).write_bytes(payload)
    raise SystemExit(0)

raise SystemExit("mock: unknown command " + command)
'''


def write_mock(directory: Path, name: str, role: str) -> Path:
    path = directory / name
    path.write_text(
        MOCK_TEMPLATE.format(python=sys.executable, role=role), encoding="utf-8"
    )
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


class BenchmarkTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="bench-sparse-test-", dir=str(WORK_ROOT))
        self.tmp = Path(self._tmp.name)
        self.baseline = write_mock(self.tmp, "baseline-srep", "baseline")
        self.candidate = write_mock(self.tmp, "candidate-srep", "candidate")
        self._saved_env = dict(os.environ)

    def tearDown(self) -> None:
        os.environ.clear()
        os.environ.update(self._saved_env)
        self._tmp.cleanup()

    _run_counter = 0

    def run_main(self, *, sample="fidelity-v1-07", methods="m3", timeout="60",
                 memory="256MiB", temp_limit="32GiB") -> int:
        BenchmarkTestBase._run_counter += 1
        n = BenchmarkTestBase._run_counter
        work = self.tmp / f"work-{n}"
        report = self.tmp / f"report-{n}.json"
        self.report_path = report
        return bench.main([
            "--baseline-bin", str(self.baseline),
            "--candidate-bin", str(self.candidate),
            "--work-dir", str(work),
            "--report", str(report),
            "--sample", sample,
            "--methods", methods,
            "--memory", memory,
            "--temp-limit", temp_limit,
            "--timeout", timeout,
        ])

    def load_report(self) -> dict:
        return json.loads(self.report_path.read_text(encoding="utf-8"))


class HappyPathTests(BenchmarkTestBase):
    def test_complete_run_reports_frozen_input_and_equal_archives(self) -> None:
        self.assertEqual(self.run_main(), 0)
        report = self.load_report()
        self.assertTrue(report["complete"])
        self.assertTrue(report["pass"])
        sample = report["sample"]
        self.assertEqual(sample["id"], "fidelity-v1-07")
        self.assertEqual(sample["bytes"], 524288)
        self.assertEqual(
            sample["sha256"],
            "0bdbc925b10ebdb128d72b651d56dbae7f539e16b6b2c24e87c837cebc0bc8df",
        )
        for label in ("baseline", "candidate"):
            binary = report["binaries"][label]
            self.assertTrue(binary["available"])
            self.assertEqual(binary["version"], "srep mock 0.0.0")
            self.assertTrue(Path(binary["path"]).is_absolute())
            run = report["runs"][label]["m3"]
            self.assertEqual(run["status"], "complete")
            self.assertTrue(run["cleanup"]["verified_empty"])
            self.assertEqual(run["input"]["bytes"], 524288)
            self.assertEqual(run["input"]["sha256"], sample["sha256"])
        comparison = report["comparison"]["methods"]["m3"]
        self.assertTrue(comparison["archive_sha256_equal"])
        # Reporting-only ratios: candidate mock is identical to baseline.
        self.assertEqual(comparison["elapsed_seconds_ratio"]["ratio"] is not None, True)
        self.assertEqual(comparison["temp_peak_bytes_ratio"]["candidate"], 5000)

    def test_frozen_argv_contract_and_exact_counts(self) -> None:
        argv_log = self.tmp / "argv.log"
        os.environ["MOCK_ARGV_LOG"] = str(argv_log)
        self.assertEqual(self.run_main(), 0)
        lines = [line.split(" ", 1) for line in argv_log.read_text().splitlines()]
        compress_args = [
            (role, json.loads(args)) for role, args in lines
            if json.loads(args)[0] == "compress"
        ]
        self.assertEqual(len(compress_args), 2)
        for role, args in compress_args:
            self.assertIn("--method", args)
            self.assertIn("m3", args)
            self.assertIn("--layout", args)
            self.assertIn("index", args)
            self.assertIn("--checksum", args)
            self.assertIn("xxh3", args)
            self.assertIn("--block-size", args)
            self.assertIn("8KiB", args)
            self.assertIn("--min-match", args)
            self.assertIn("512", args)
            self.assertIn("--memory", args)
            self.assertIn("256MiB", args)
            self.assertIn("--temp-limit", args)
            self.assertIn("32GiB", args)
            temp_index = args.index("--temp-dir") + 1
            self.assertTrue(Path(args[temp_index]).is_absolute())
            self.assertIn(role, args[temp_index])
        # Same frozen argv prefix and resource values for both binaries; only
        # the private temp dir and source/archive paths may differ.
        base = compress_args[0][1]
        cand = compress_args[1][1]
        self.assertEqual(base[1:12], cand[1:12])
        self.assertEqual(
            base[base.index("--memory") + 1], cand[cand.index("--memory") + 1]
        )
        self.assertEqual(
            base[base.index("--temp-limit") + 1], cand[cand.index("--temp-limit") + 1]
        )
        self.assertNotEqual(
            base[base.index("--temp-dir") + 1], cand[cand.index("--temp-dir") + 1]
        )

    def test_profile_parsed_final_managed_values(self) -> None:
        self.assertEqual(self.run_main(), 0)
        report = self.load_report()
        profile = report["runs"]["baseline"]["m3"]["compress_profile"]
        self.assertTrue(profile["present"])
        self.assertEqual(profile["final"]["event"], "complete")
        self.assertEqual(profile["final"]["temp_peak"], 5000)
        self.assertEqual(profile["final"]["io"], 2000)
        self.assertEqual(profile["events"][0]["elapsed_ms"], 10)


class FailureTests(BenchmarkTestBase):
    def test_nonzero_compress_writes_partial_report_nonzero_exit(self) -> None:
        os.environ["MOCK_FAIL_STAGE"] = "compress"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        self.assertFalse(report["complete"])
        self.assertFalse(report["pass"])
        self.assertIn("error", report)
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "compress-nonzero")
        self.assertEqual(run["stages"]["compress"]["exit_code"], 8)
        stderr_file = Path(run["stages"]["compress"]["stderr_file"])
        self.assertIn("injected failure", stderr_file.read_text())

    def test_info_failure(self) -> None:
        os.environ["MOCK_FAIL_STAGE"] = "info"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        self.assertEqual(report["runs"]["baseline"]["m3"]["failure"], "info-nonzero")

    def test_timeout_produces_partial_report_and_reaps_child(self) -> None:
        child_marker = self.tmp / "child.pid"
        os.environ["MOCK_SLEEP_STAGE"] = "compress"
        os.environ["MOCK_CHILD_PID"] = str(child_marker)
        self.assertEqual(self.run_main(timeout="2"), 1)
        report = self.load_report()
        self.assertFalse(report["complete"])
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "compress-timeout")
        self.assertIsNone(run["stages"]["compress"]["exit_code"])
        self.assertTrue(run["stages"]["compress"]["timed_out"])
        self.assertTrue(run["stages"]["compress"]["leader_reaped"])
        child_pid = int(child_marker.read_text().split()[1])
        # The SIGTERM/SIGKILL process-group kill must not leave the descendant.
        for _ in range(50):
            try:
                os.kill(child_pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        else:
            os.kill(child_pid, 9)
            self.fail(f"child process {child_pid} survived timeout reaping")

    def test_timeout_kills_term_ignoring_descendant_after_leader_exits(self) -> None:
        if os.name != "posix" or not hasattr(os, "killpg"):
            self.skipTest("process-group kill requires POSIX")
        child_marker = self.tmp / "term-ignore-child.pid"
        os.environ["MOCK_SLEEP_STAGE"] = "compress"
        os.environ["MOCK_TERM_IGNORE"] = "baseline"
        os.environ["MOCK_CHILD_PID"] = str(child_marker)
        self.assertEqual(self.run_main(timeout="2"), 1)
        run = self.load_report()["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "compress-timeout")
        # Wait for the marker to appear, then confirm the group SIGKILL got it.
        for _ in range(100):
            if child_marker.exists() and child_marker.read_text().strip():
                break
            time.sleep(0.05)
        child_pid = int(child_marker.read_text().strip())
        for _ in range(100):
            try:
                os.kill(child_pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        else:
            os.kill(child_pid, 9)
            self.fail(f"TERM-ignoring descendant {child_pid} survived group SIGKILL")

    def test_profile_missing_fails_whole_benchmark(self) -> None:
        os.environ["MOCK_PROFILE"] = "missing"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        self.assertFalse(report["complete"])
        self.assertFalse(report["pass"])
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "profile-invalid")
        self.assertFalse(run["compress_profile"] if "compress_profile" in run
                         else run["stages"]["compress"]["profile"]["present"])

    def test_profile_malformed_fails_whole_benchmark(self) -> None:
        os.environ["MOCK_PROFILE"] = "malformed"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        self.assertFalse(report["pass"])
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "profile-invalid")
        stage_profile = run["stages"]["compress"]["profile"]
        self.assertFalse(stage_profile["present"])
        self.assertEqual(stage_profile["malformed_count"], 1)

    def test_profile_ok_false_fails(self) -> None:
        os.environ["MOCK_PROFILE"] = "ok_false"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "profile-invalid")
        self.assertIn("ok=true", run["error"])

    def test_profile_ok_missing_fails(self) -> None:
        os.environ["MOCK_PROFILE"] = "ok_missing"
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(
            self.load_report()["runs"]["baseline"]["m3"]["failure"], "profile-invalid"
        )

    def test_profile_negative_field_fails(self) -> None:
        os.environ["MOCK_PROFILE"] = "negative"
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(
            self.load_report()["runs"]["baseline"]["m3"]["failure"], "profile-invalid"
        )

    def test_profile_current_exceeds_peak_fails(self) -> None:
        os.environ["MOCK_PROFILE"] = "ram_exceeds_peak"
        self.assertEqual(self.run_main(), 1)
        self.assertIn(
            "exceeds", self.load_report()["runs"]["baseline"]["m3"]["error"]
        )

    def test_profile_budget_exceed_fails(self) -> None:
        os.environ["MOCK_PROFILE"] = "budget_exceed"
        self.assertEqual(self.run_main(), 1)
        self.assertIn(
            "exceeds requested memory",
            self.load_report()["runs"]["baseline"]["m3"]["error"],
        )

    def test_profile_temp_peak_over_limit_fails(self) -> None:
        # A valid-looking profile whose temp_peak exceeds the configured limit.
        os.environ["MOCK_PROFILE"] = "temp_budget_exceed"
        code = bench.main([
            "--baseline-bin", str(self.baseline), "--candidate-bin", str(self.candidate),
            "--work-dir", str(self.tmp / "w-templimit"), "--report", str(self.tmp / "r-templimit.json"),
        ])
        self.assertEqual(code, 1)
        report = json.loads((self.tmp / "r-templimit.json").read_text())
        self.assertIn("exceeds temp_limit", report["runs"]["baseline"]["m3"]["error"])

    def test_archive_unequal_returns_nonzero(self) -> None:
        os.environ["MOCK_ARCHIVE_VARIANT"] = "candidate"
        self.assertEqual(self.run_main(), 3)
        report = self.load_report()
        self.assertTrue(report["complete"])
        self.assertFalse(report["pass"])
        comparison = report["comparison"]["methods"]["m3"]
        self.assertFalse(comparison["archive_sha256_equal"])

    def test_temp_cleanup_failure_is_reported(self) -> None:
        os.environ["MOCK_TEMP_RESIDUE"] = "baseline"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "temp-cleanup-failed")
        self.assertFalse(run["cleanup"]["verified_empty"])
        self.assertIn("residue.tmp", run["cleanup"]["residue"])

    def test_binary_mutation_is_detected_before_next_stage(self) -> None:
        os.environ["MOCK_MUTATE_BINARY"] = "candidate"
        self.assertEqual(self.run_main(), 1)
        report = self.load_report()
        self.assertFalse(report["complete"])
        self.assertFalse(report["pass"])
        # Mutation is caught by the per-stage identity recheck before the next
        # command runs, not only at the whole-run end.
        self.assertIn("identity", report["error"])
        run = report["runs"]["candidate"]["m3"]
        self.assertEqual(run["failure"], "identity-changed")
        self.assertIn("mutated", run["error"])


class ValidationTests(BenchmarkTestBase):
    def test_existing_nonempty_work_dir_rejected(self) -> None:
        work = self.tmp / "work-nonempty"
        work.mkdir()
        (work / "keep.txt").write_text("x")
        report = self.tmp / "r.json"
        code = bench.main([
            "--baseline-bin", str(self.baseline), "--candidate-bin", str(self.candidate),
            "--work-dir", str(work), "--report", str(report),
        ])
        self.assertEqual(code, 1)
        self.assertFalse(report.exists())
        self.assertTrue((work / "keep.txt").exists())

    def test_existing_report_rejected(self) -> None:
        report = self.tmp / "existing.json"
        report.write_text("old")
        work = self.tmp / "work-new"
        code = bench.main([
            "--baseline-bin", str(self.baseline), "--candidate-bin", str(self.candidate),
            "--work-dir", str(work), "--report", str(report),
        ])
        self.assertEqual(code, 1)
        self.assertEqual(report.read_text(), "old")

    def test_identical_binaries_rejected(self) -> None:
        code = bench.main([
            "--baseline-bin", str(self.baseline), "--candidate-bin", str(self.baseline),
            "--work-dir", str(self.tmp / "w-ident"), "--report", str(self.tmp / "r-ident.json"),
        ])
        self.assertEqual(code, 1)

    def test_unknown_sample_rejected(self) -> None:
        self.assertEqual(self.run_main(sample="does-not-exist"), 1)

    def test_corrupted_generated_input_length_and_sha_rejected(self) -> None:
        recipe = bench.resolve_sample(bench.load_fidelity_module(), "fidelity-v1-07")
        good = recipe.data
        with self.assertRaises(bench.BenchmarkError):
            recipe.verify_bytes(good[:-1])
        corrupted = good[:100] + bytes([good[100] ^ 1]) + good[101:]
        with self.assertRaises(bench.BenchmarkError):
            recipe.verify_bytes(corrupted)

    def test_default_sample_contract_drift_rejected(self) -> None:
        data = b"\0" * 524288
        recipe = bench.SampleRecipe("fidelity-v1-07", 524288, bench.sha256_bytes(data), data)
        # Directly assert the guard message path via a wrong declared SHA.
        module = bench.load_fidelity_module()
        import unittest.mock as mock

        manifest = module.strict_json(module.CORPUS_MANIFEST)
        tampered = json.loads(json.dumps(manifest))
        for sample in tampered["samples"]:
            if sample["id"] == "fidelity-v1-07":
                sample["sha256"] = "0" * 64
        with mock.patch.object(module, "strict_json", return_value=tampered):
            with self.assertRaises(bench.BenchmarkError):
                bench.resolve_sample(module, "fidelity-v1-07")

    def test_parse_size_exact_binary_units(self) -> None:
        self.assertEqual(bench.parse_size("256MiB"), 256 * 1024 * 1024)
        self.assertEqual(bench.parse_size("32GiB"), 32 * 1024 * 1024 * 1024)
        self.assertEqual(bench.parse_size("8KiB"), 8192)
        self.assertEqual(bench.parse_size("512"), 512)
        for bad in ("", "3.5MiB", "12XiB", "MiB"):
            with self.assertRaises(bench.BenchmarkError):
                bench.parse_size(bad)

    def _write_info(self, text: str) -> Path:
        path = self.tmp / "info.txt"
        path.write_text(text, encoding="utf-8")
        return path

    def test_parse_info_exact_counts(self) -> None:
        path = self._write_info(
            "method: m3\nlayout: index\nchecksum: xxh3\n"
            "original size: 524288\npayload size: 2816\nblocks: 64\n"
            "semantic matches: 4\ncovered bytes: 521728\nliteral bytes: 2560\n"
        )
        values = bench.parse_info(path, expected_size=524288, expected_method="m3")
        self.assertEqual(values["original size"], 524288)
        self.assertEqual(values["covered bytes"], 521728)
        self.assertEqual(values["literal bytes"], 2560)
        self.assertEqual(values["blocks"], 64)
        self.assertEqual(values["method"], "m3")

    def test_parse_info_rejects_missing_duplicate_negative_and_mismatch(self) -> None:
        good = (
            "method: m3\nlayout: index\nchecksum: xxh3\n"
            "original size: 524288\npayload size: 2816\nblocks: 64\n"
            "semantic matches: 4\ncovered bytes: 521728\nliteral bytes: 2560\n"
        )
        with self.assertRaises(bench.BenchmarkError):
            bench.parse_info(self._write_info("method: m3\n"),
                             expected_size=524288, expected_method="m3")
        with self.assertRaises(bench.BenchmarkError):
            bench.parse_info(
                self._write_info(good + "blocks: 64\n"),
                expected_size=524288, expected_method="m3",
            )
        with self.assertRaises(bench.BenchmarkError):
            bench.parse_info(
                self._write_info(good.replace("semantic matches: 4", "semantic matches: -1")),
                expected_size=524288, expected_method="m3",
            )
        with self.assertRaises(bench.BenchmarkError):
            bench.parse_info(
                self._write_info(good.replace("original size: 524288", "original size: 123")),
                expected_size=524288, expected_method="m3",
            )
        with self.assertRaises(bench.BenchmarkError):
            bench.parse_info(
                self._write_info(good.replace("method: m3", "method: m4")),
                expected_size=524288, expected_method="m3",
            )

    def test_info_wrong_original_size_fails_benchmark(self) -> None:
        os.environ["MOCK_INFO_SIZE"] = "123"
        self.assertEqual(self.run_main(), 1)
        run = self.load_report()["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "info-invalid")
        self.assertIn("original size", run["error"])

    def test_info_duplicate_field_fails_benchmark(self) -> None:
        os.environ["MOCK_INFO_DUP"] = "1"
        self.assertEqual(self.run_main(), 1)
        run = self.load_report()["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "info-invalid")

    def test_info_negative_field_fails_benchmark(self) -> None:
        os.environ["MOCK_INFO_NEGATIVE"] = "1"
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(
            self.load_report()["runs"]["baseline"]["m3"]["failure"], "info-invalid"
        )

    def test_info_wrong_method_fails_benchmark(self) -> None:
        os.environ["MOCK_INFO_METHOD"] = "m4"
        self.assertEqual(self.run_main(), 1)
        self.assertEqual(
            self.load_report()["runs"]["baseline"]["m3"]["failure"], "info-invalid"
        )

    def test_info_timeout_is_captured_as_failed_stage(self) -> None:
        os.environ["MOCK_SLEEP_STAGE"] = "info"
        self.assertEqual(self.run_main(timeout="2"), 1)
        report = self.load_report()
        run = report["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "info-timeout")
        self.assertTrue(run["stages"]["info"]["timed_out"])
        # The earlier compress stage must still be retained.
        self.assertIn("compress", run["stages"])

    def test_test_timeout_is_captured_as_failed_stage(self) -> None:
        os.environ["MOCK_SLEEP_STAGE"] = "test"
        self.assertEqual(self.run_main(timeout="2"), 1)
        run = self.load_report()["runs"]["baseline"]["m3"]
        self.assertEqual(run["failure"], "test-timeout")
        self.assertIn("compress", run["stages"])
        self.assertIn("info", run["stages"])

    def test_duplicate_methods_rejected(self) -> None:
        self.assertEqual(self.run_main(methods="m3,m3"), 1)

    def test_zero_budgets_rejected(self) -> None:
        self.assertEqual(self.run_main(memory="0"), 1)
        self.assertEqual(self.run_main(temp_limit="0"), 1)

    def test_missing_binary_rejected(self) -> None:
        code = bench.main([
            "--baseline-bin", str(self.tmp / "nope"),
            "--candidate-bin", str(self.candidate),
            "--work-dir", str(self.tmp / "w-missing"),
            "--report", str(self.tmp / "r-missing.json"),
        ])
        self.assertEqual(code, 1)

    def test_binary_identity_recheck_detects_swap(self) -> None:
        target = self.tmp / "swap-bin"
        target.write_bytes(b"original")
        identity = {
            "path": str(target),
            "sha256": bench.file_sha256(target),
            "version": "x",
            "available": True,
        }
        bench._recheck_identity(target, identity, phase="test")
        target.write_bytes(b"replaced")
        with self.assertRaises(bench.BenchmarkError):
            bench._recheck_identity(target, identity, phase="test")

    def test_identity_capture_version_failure(self) -> None:
        os.environ["MOCK_VERSION_FAIL"] = "1"
        with self.assertRaises(bench.BenchmarkError):
            bench._capture_identity(self.baseline, timeout=10)

    def test_method_selection_rejects_unknown(self) -> None:
        code = bench.main([
            "--baseline-bin", str(self.baseline), "--candidate-bin", str(self.candidate),
            "--work-dir", str(self.tmp / "w-method"), "--report", str(self.tmp / "r-method.json"),
            "--methods", "m9",
        ])
        self.assertEqual(code, 1)


class ProfileStrictnessTests(BenchmarkTestBase):
    """Unit tests for monotonicity and unambiguous terminal completion."""

    MEMORY = 256 * 1024 * 1024
    TEMP = 32 * 1024 * 1024 * 1024

    @staticmethod
    def _event(event, elapsed, ram, ram_peak, temp, temp_peak, io, detail=""):
        return (
            f"OFFLOAD_PROFILE event={event} elapsed_ms={elapsed} ram={ram} "
            f"ram_peak={ram_peak} temp={temp} temp_peak={temp_peak} "
            f"handles=1 files=0 io={io}{detail}"
        )

    def _validate(self, text: str) -> None:
        prof = bench.parse_profile(text)
        bench.validate_profile(
            prof, memory_bytes=self.MEMORY, temp_limit_bytes=self.TEMP
        )

    def _valid(self) -> list[str]:
        return [
            self._event("pass1", 10, 100, 200, 300, 400, 1000),
            self._event("complete", 250, 50, 210, 0, 5000, 2000, " ok=true"),
        ]

    def test_valid_monotonic_sequence_passes(self) -> None:
        self._validate("\n".join(self._valid()))

    def test_equal_values_allowed(self) -> None:
        text = "\n".join([
            self._event("a", 10, 1, 200, 1, 400, 1000),
            self._event("b", 10, 1, 200, 1, 400, 1000),
            self._event("complete", 10, 1, 200, 1, 400, 1000, " ok=true"),
        ])
        self._validate(text)

    def _reject(self, lines: list[str]) -> None:
        with self.assertRaises(bench.BenchmarkError):
            self._validate("\n".join(lines))

    def test_decreasing_elapsed_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event("complete", 5, 50, 210, 0, 5000, 2000, " ok=true")
        self._reject(lines)

    def test_decreasing_ram_peak_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event("complete", 250, 50, 100, 0, 5000, 2000, " ok=true")
        self._reject(lines)

    def test_decreasing_temp_peak_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event("complete", 250, 50, 210, 0, 100, 2000, " ok=true")
        self._reject(lines)

    def test_decreasing_io_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event("complete", 250, 50, 210, 0, 5000, 500, " ok=true")
        self._reject(lines)

    def test_final_zero_high_water_after_nonzero_rejected(self) -> None:
        # A trailing zero temp_peak/io cannot be read as a fake improvement.
        lines = self._valid()
        lines[1] = self._event("complete", 250, 0, 0, 0, 0, 0, " ok=true")
        self._reject(lines)

    def test_duplicate_complete_rejected(self) -> None:
        lines = self._valid()
        lines.append(self._event("complete", 260, 1, 215, 1, 5001, 2001, " ok=true"))
        self._reject(lines)

    def test_post_complete_event_rejected(self) -> None:
        lines = self._valid()
        lines.append(self._event("trailing", 260, 1, 215, 1, 5001, 2001))
        self._reject(lines)

    def test_missing_complete_rejected(self) -> None:
        self._reject([self._event("pass1", 10, 100, 200, 300, 400, 1000)])

    def test_ambiguous_ok_tokens_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event(
            "complete", 250, 50, 210, 0, 5000, 2000, " ok=true ok=false"
        )
        self._reject(lines)

    def test_multiple_true_ok_tokens_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event(
            "complete", 250, 50, 210, 0, 5000, 2000, " ok=true ok=true"
        )
        self._reject(lines)

    def test_ok_on_nonterminal_event_rejected(self) -> None:
        lines = self._valid()
        lines[0] = self._event("pass1", 10, 100, 200, 300, 400, 1000, " ok=true")
        self._reject(lines)

    def test_false_ok_rejected(self) -> None:
        lines = self._valid()
        lines[1] = self._event("complete", 250, 50, 210, 0, 5000, 2000, " ok=false")
        self._reject(lines)

    def test_missing_ok_rejected(self) -> None:
        self._reject(self._valid()[:1] + [
            self._event("complete", 250, 50, 210, 0, 5000, 2000)
        ])


class ProfileEndToEndTests(BenchmarkTestBase):
    """Every defective profile must fail the whole benchmark, not just parsing."""

    DEFECTS = (
        "missing", "malformed", "ok_false", "ok_missing", "negative",
        "ram_exceeds_peak", "budget_exceed", "temp_budget_exceed",
        "decreasing_elapsed", "decreasing_ram_peak", "decreasing_temp_peak",
        "decreasing_io", "duplicate_complete", "post_complete", "ambiguous_ok",
        "multiple_ok", "ok_on_nonterminal",
    )

    def test_defective_profiles_fail_whole_benchmark(self) -> None:
        for mode in self.DEFECTS:
            with self.subTest(mode=mode):
                os.environ["MOCK_PROFILE"] = mode
                self.assertEqual(self.run_main(), 1)
                report = self.load_report()
                self.assertFalse(report["complete"], mode)
                self.assertFalse(report["pass"], mode)
                run = report["runs"]["baseline"]["m3"]
                self.assertEqual(run["failure"], "profile-invalid", mode)
                self.assertIn("profile", run["stages"]["compress"], mode)


class SignalTests(BenchmarkTestBase):
    """Subprocess integration: SIGINT/SIGTERM must boundedly clean up children."""

    def _wait_for(self, predicate, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return True
            time.sleep(0.05)
        return False

    def _pid_alive(self, pid: int) -> bool:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True

    def _run_signal(self, sig: int) -> dict:
        if os.name != "posix" or not hasattr(os, "killpg"):
            self.skipTest("process-group kill requires POSIX")
        start_marker = self.tmp / f"started-{sig}"
        child_marker = self.tmp / f"child-{sig}.pid"
        work = self.tmp / f"work-sig-{sig}"
        report = self.tmp / f"report-sig-{sig}.json"
        env = dict(os.environ)
        env.update({
            "MOCK_SLEEP_STAGE": "compress",
            "MOCK_TERM_IGNORE": "baseline",
            "MOCK_START_MARKER": str(start_marker),
            "MOCK_CHILD_PID": str(child_marker),
            "PYTHONPYCACHEPREFIX": "/tmp/opencode/pycache",
        })
        process = subprocess.Popen(
            [sys.executable, str(SCRIPTS / "benchmark-sparse.py"),
             "--baseline-bin", str(self.baseline),
             "--candidate-bin", str(self.candidate),
             "--work-dir", str(work), "--report", str(report),
             "--timeout", "600"],
            env=env,
        )
        try:
            ready = self._wait_for(
                lambda: start_marker.exists()
                and child_marker.exists()
                and child_marker.read_text().strip().isdigit(),
                timeout=30,
            )
            if not ready:
                self.fail("mock compression workload did not start")
            child_pid = int(child_marker.read_text().strip())
            self.assertTrue(self._pid_alive(child_pid))
            started = time.monotonic()
            os.kill(process.pid, sig)
            try:
                exit_code = process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                self.fail(f"harness did not exit within bounds after signal {sig}")
            elapsed = time.monotonic() - started
            self.assertLess(elapsed, 25, f"signal {sig} cleanup not bounded: {elapsed}")
            self.assertNotEqual(exit_code, 0)
            self.assertEqual(exit_code, 128 + sig)
            reaped = self._wait_for(lambda: not self._pid_alive(child_pid), timeout=10)
            if not reaped:
                os.kill(child_pid, signal.SIGKILL)
                self.fail(f"workload child {child_pid} survived signal {sig}")
            payload = json.loads(report.read_text())
            self.assertFalse(payload["complete"])
            self.assertFalse(payload["pass"])
            self.assertEqual(payload["interrupted"]["signal"], sig)
            self.assertIn("interrupted", payload["error"].lower())
            run = payload["runs"]["baseline"]["m3"]
            self.assertEqual(run["failure"], "interrupted")
            return {"exit_code": exit_code, "elapsed": elapsed, "report": payload}
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)

    def test_sigint_reaps_term_ignoring_child_and_reports_interrupted(self) -> None:
        result = self._run_signal(signal.SIGINT)
        self.assertEqual(result["exit_code"], 130)

    def test_sigterm_reaps_term_ignoring_child_and_reports_interrupted(self) -> None:
        result = self._run_signal(signal.SIGTERM)
        self.assertEqual(result["exit_code"], 143)


class _SignalOnFirstWait:
    """Delegate to a real Popen, delivering an interrupt at the first wait.

    Exercises the handoff window where the child handle exists but the wait has
    not yet entered its own protected block.
    """

    def __init__(self, real, sig: int) -> None:
        self.real = real
        self.pid = real.pid
        self._sig = sig
        self._fired = False

    def wait(self, timeout=None):
        if not self._fired:
            self._fired = True
            os.kill(os.getpid(), self._sig)
        return self.real.wait(timeout=timeout)

    def poll(self):
        return self.real.poll()

    @property
    def returncode(self):
        return self.real.returncode


class LaunchWindowTests(unittest.TestCase):
    """Deterministic launch/ownership handoff: no child is ever orphaned."""

    SLEEP = [sys.executable, "-c", "import time; time.sleep(120)"]

    def setUp(self) -> None:
        if not (os.name == "posix" and hasattr(os, "killpg")):
            self.skipTest("process-group semantics require POSIX killpg")
        self._real_popen = subprocess.Popen
        self._tmp = tempfile.TemporaryDirectory(prefix="bench-launch-", dir=str(WORK_ROOT))
        self.root = Path(self._tmp.name)
        self.handlers = bench._install_interrupt_handlers()
        bench._SIGNAL_GATE.reset()
        self.children: list = []

    def tearDown(self) -> None:
        for child in self.children:
            real = getattr(child, "real", child)
            try:
                if real.poll() is None:
                    real.kill()
                    real.wait(timeout=5)
            except OSError:
                pass
        bench._SIGNAL_GATE.reset()
        bench._restore_interrupt_handlers(self.handlers)
        self._tmp.cleanup()

    def test_signal_inside_popen_window_is_recorded_and_group_reaped(self) -> None:
        import unittest.mock as mock

        holder = {}

        def fake(command, **kwargs):
            proc = self._real_popen(command, **kwargs)
            holder["proc"] = proc
            # Delivered while run_captured is deferring (inside Popen's window):
            # the gate records it, Popen still returns a usable handle.
            os.kill(os.getpid(), signal.SIGINT)
            return proc

        started = time.monotonic()
        with mock.patch.object(subprocess, "Popen", fake):
            with self.assertRaises(bench.BenchmarkInterrupted) as ctx:
                bench.run_captured(
                    self.SLEEP, timeout=30, env_extra={},
                    log_dir=self.root / "during", stage="compress",
                )
        self.assertEqual(ctx.exception.signum, signal.SIGINT)
        self.assertLess(time.monotonic() - started, 15)
        proc = holder["proc"]
        self.children.append(proc)
        # Reaped (SIGTERM) BEFORE the interruption propagated.
        self.assertIsNotNone(proc.returncode)

    def test_signal_after_assignment_before_wait_reaps_group(self) -> None:
        import unittest.mock as mock

        holder = {}

        def fake(command, **kwargs):
            proxy = _SignalOnFirstWait(self._real_popen(command, **kwargs), signal.SIGTERM)
            holder["proxy"] = proxy
            return proxy

        with mock.patch.object(subprocess, "Popen", fake):
            with self.assertRaises(bench.BenchmarkInterrupted) as ctx:
                bench.run_captured(
                    self.SLEEP, timeout=30, env_extra={},
                    log_dir=self.root / "before-wait", stage="compress",
                )
        self.assertEqual(ctx.exception.signum, signal.SIGTERM)
        proxy = holder["proxy"]
        self.children.append(proxy)
        self.assertIsNotNone(proxy.real.returncode)

    def test_fault_injected_immediately_after_assignment_reaps_group(self) -> None:
        import unittest.mock as mock

        holder = {}

        class _FaultOnce:
            """Raise once at the first wait, then delegate to the real child."""

            def __init__(self, real):
                self.real = real
                self.pid = real.pid
                self._fired = False

            def wait(self, timeout=None):
                if not self._fired:
                    self._fired = True
                    raise bench.BenchmarkInterrupted(signal.SIGINT)
                return self.real.wait(timeout=timeout)

            def poll(self):
                return self.real.poll()

        def fake_fault(command, **kwargs):
            fault = _FaultOnce(self._real_popen(command, **kwargs))
            holder["fault"] = fault
            return fault

        log_dir = self.root / "fault"
        with mock.patch.object(subprocess, "Popen", fake_fault):
            with self.assertRaises(bench.BenchmarkInterrupted):
                bench.run_captured(
                    self.SLEEP, timeout=30, env_extra={}, log_dir=log_dir, stage="compress"
                )
        fault = holder["fault"]
        self.children.append(fault.real)
        self.assertIsNotNone(fault.real.returncode)

    def test_fault_at_ownership_handoff_reaps_group(self) -> None:
        # Fault raised at the exact handoff point: handle assigned, launch
        # deferral closed, wait not yet entered.
        import unittest.mock as mock

        holder = {}

        def fake(command, **kwargs):
            proc = self._real_popen(command, **kwargs)
            holder["proc"] = proc
            return proc

        real_take = bench._SIGNAL_GATE.take_pending

        def raising_take():
            raise bench.BenchmarkInterrupted(signal.SIGTERM)

        bench._SIGNAL_GATE.take_pending = raising_take  # type: ignore[assignment]
        try:
            with mock.patch.object(subprocess, "Popen", fake):
                with self.assertRaises(bench.BenchmarkInterrupted):
                    bench.run_captured(
                        self.SLEEP, timeout=30, env_extra={},
                        log_dir=self.root / "handoff", stage="compress",
                    )
        finally:
            bench._SIGNAL_GATE.take_pending = real_take  # type: ignore[assignment]
        proc = holder["proc"]
        self.children.append(proc)
        self.assertIsNotNone(proc.returncode)

    def test_error_popen_raises_benchmark_error_without_orphan(self) -> None:
        holder = {}

        def fake(command, **kwargs):
            proc = self._real_popen(command, **kwargs)
            holder["proc"] = proc
            proc.kill()
            proc.wait(timeout=5)
            raise OSError("injected spawn failure")

        import unittest.mock as mock

        with mock.patch.object(subprocess, "Popen", fake):
            with self.assertRaises(bench.BenchmarkError):
                bench.run_captured(
                    self.SLEEP, timeout=30, env_extra={},
                    log_dir=self.root / "error", stage="compress",
                )
        proc = holder["proc"]
        self.children.append(proc)
        self.assertIsNotNone(proc.returncode)

    def test_signal_gate_records_then_take_pending_is_single(self) -> None:
        gate = bench._SignalGate()
        gate.deferring = True
        gate.handle(signal.SIGINT)
        gate.handle(signal.SIGTERM)  # second is ignored while one is pending
        self.assertEqual(gate.take_pending(), signal.SIGINT)
        self.assertIsNone(gate.take_pending())
        # Once the launch window closes, the handler raises immediately.
        gate.deferring = False
        with self.assertRaises(bench.BenchmarkInterrupted) as ctx:
            gate.handle(signal.SIGTERM)
        self.assertEqual(ctx.exception.signum, signal.SIGTERM)

    def test_child_does_not_inherit_blocked_signal_mask(self) -> None:
        if not hasattr(signal, "pthread_sigmask"):
            self.skipTest("pthread_sigmask unavailable")
        command = [
            sys.executable, "-c",
            "import signal,sys;"
            "m=signal.pthread_sigmask(signal.SIG_BLOCK, []);"
            "sys.stdout.write(','.join(sorted(getattr(s,'name',str(s)) for s in m)))",
        ]
        record = bench.run_captured(
            command, timeout=30, env_extra={}, log_dir=self.root / "mask", stage="compress"
        )
        self.assertEqual(record["exit_code"], 0)
        blocked = Path(str(record["stdout_file"])).read_text()
        self.assertNotIn("SIGINT", blocked)
        self.assertNotIn("SIGTERM", blocked)


class ParserTests(unittest.TestCase):
    def test_defaults(self) -> None:
        args = bench.build_parser().parse_args([
            "--baseline-bin", "/a", "--candidate-bin", "/b",
            "--work-dir", "/w", "--report", "/r",
        ])
        self.assertEqual(args.methods, "m3")
        self.assertEqual(args.sample, "fidelity-v1-07")
        self.assertEqual(args.memory, "256MiB")
        self.assertEqual(args.temp_limit, "32GiB")
        self.assertEqual(args.timeout, bench.DEFAULT_TIMEOUT)

    def test_timeout_must_be_positive(self) -> None:
        parser = bench.build_parser()
        for value in ("0", "-1", "nan", "inf", "abc"):
            with self.assertRaises(SystemExit):
                parser.parse_args([
                    "--baseline-bin", "/a", "--candidate-bin", "/b",
                    "--work-dir", "/w", "--report", "/r", "--timeout", value,
                ])


if __name__ == "__main__":
    unittest.main(verbosity=2)

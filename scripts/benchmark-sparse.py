#!/usr/bin/env python3
"""Same-host before/after sparse benchmark harness for two srep binaries.

This is a bounded, reproducible, non-Git-writing measurement harness.  It runs
one frozen corpus sample through a baseline binary and then a candidate binary,
SERIALLY, on the same host, with identical frozen arguments, explicit private
temporary directories, ``SREP_OFFLOAD_PROFILE=1``, and per-command argv/exit/
stdout/stderr/elapsed captures written next to the report.

The input bytes are the deterministic frozen recipe owned by the checked-in
corpus manifest and ``scripts/fidelity.py``.  This module imports that recipe by
file path and never re-implements the generator.  The input length and SHA-256
are re-verified immediately before every run, and both binaries' immutable
identity (absolute path, SHA-256, ``--version``) is captured before and
re-checked after the run.

The archive identity produced by each binary must match byte-for-byte, and each
run must complete compress -> info -> test -> decompress -> Python byte compare
with a verified-empty private temp directory.  The compression profile is
validated strictly: it must be present, its final event must be exactly
``complete`` with ``ok=true``, every retained field must be a non-negative,
internally consistent managed-ledger value, and the managed peaks must respect
the requested memory/temp budgets.  ``info`` must also report the frozen
original size and the frozen method/layout/checksum.  A timeout, command
failure, missing/malformed/invalid profile, info mismatch, archive mismatch, or
unclean temp directory yields a durable partial report and a nonzero exit.  No
"SUCCESS" banner is printed for an incomplete run.

Only baseline-then-candidate final metrics are reported as a ratio; this harness
makes no absolute performance assertion and does not raise the binary's budgets.

Report schema ``srep-sparse-benchmark-v1`` retains the original fields and adds
non-breaking evidence fields: per-stage ``stdout_file``/``stderr_file`` plus
bounded ``stdout_preview``/``stderr_preview`` and byte counts (full transcripts
stay on disk), the compression stage's parsed ``profile`` (including
``malformed_count`` and an ``ok`` boolean per event), and ``leader_reaped`` on a
bounded timeout record.

This tool does not background itself.  A long-running caller is expected to
``setsid nohup`` it.  It never touches credentials.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent
ROOT = SCRIPTS_DIR.parent
FIDELITY_PATH = SCRIPTS_DIR / "fidelity.py"

# Frozen sample contract for the default focused benchmark.
DEFAULT_SAMPLE = "fidelity-v1-07"
DEFAULT_SAMPLE_SIZE = 524288
DEFAULT_SAMPLE_SHA256 = (
    "0bdbc925b10ebdb128d72b651d56dbae7f539e16b6b2c24e87c837cebc0bc8df"
)
DEFAULT_METHODS = "m3"
DEFAULT_MEMORY = "256MiB"
DEFAULT_TEMP_LIMIT = "32GiB"
DEFAULT_TIMEOUT = 18000.0
PROFILE_ENV = {"SREP_OFFLOAD_PROFILE": "1"}
VALID_METHODS = ("m0", "m1", "m2", "m3", "m4", "m5")
#: Frozen layout/checksum/block parameters shared by both binaries.
FROZEN_LAYOUT = "index"
FROZEN_CHECKSUM = "xxh3"
FROZEN_BLOCK_SIZE = "8KiB"

#: Reap grace after the timeout SIGTERM before escalating to SIGKILL, and the
#: bounded wait after SIGKILL.  Mirrors the conservative fidelity harness.
REAP_GRACE_SECONDS = 2.0
#: Bytes retained inline (as a preview) from each command stream.  Full streams
#: are always preserved on disk next to the report; only this many bytes enter
#: the JSON report, so a chatty or hostile binary cannot balloon the report.
PREVIEW_BYTES = 8192

_PROFILE_RE = re.compile(
    r"^OFFLOAD_PROFILE event=(?P<event>\S+)"
    r" elapsed_ms=(?P<elapsed_ms>-?\d+)"
    r" ram=(?P<ram>-?\d+)"
    r" ram_peak=(?P<ram_peak>-?\d+)"
    r" temp=(?P<temp>-?\d+)"
    r" temp_peak=(?P<temp_peak>-?\d+)"
    r" handles=(?P<handles>-?\d+)"
    r" files=(?P<files>-?\d+)"
    r" io=(?P<io>-?\d+)"
    r"(?: (?P<detail>.*))?$"
)
_SIZE_RE = re.compile(r"^(?P<number>\d+)(?P<suffix>[A-Za-z]*)$")
_SIZE_MULTIPLIERS = {
    "": 1,
    "b": 1,
    "k": 1024,
    "kb": 1024,
    "kib": 1024,
    "m": 1024 * 1024,
    "mb": 1024 * 1024,
    "mib": 1024 * 1024,
    "g": 1024 * 1024 * 1024,
    "gb": 1024 * 1024 * 1024,
    "gib": 1024 * 1024 * 1024,
}


class BenchmarkError(RuntimeError):
    """A benchmark run could not complete; carries a non-passing report."""


class BenchmarkInterrupted(BaseException):
    """Raised from the SIGINT/SIGTERM handler to unwind and clean up safely.

    Deliberately a ``BaseException`` so it is never silently swallowed by the
    generic ``except Exception`` error paths: it must reach the benchmark-level
    handler that persists a durable, non-passing interrupted report, while every
    launched process group is still bounded-killed and reaped on the way up.
    """

    def __init__(self, signum: int) -> None:
        self.signum = int(signum)
        super().__init__(f"interrupted by signal {signum}")


#: Signals that must trigger bounded child cleanup rather than an abrupt exit.
_INTERRUPT_SIGNALS = (signal.SIGINT, signal.SIGTERM)


class _SignalGate:
    """Owns SIGINT/SIGTERM delivery across the child-launch ownership window.

    ``subprocess.Popen`` can create the OS child before its handle is returned.
    If the handler raised during that window, the orphaned child could never be
    reached.  While ``deferring`` is set (only around the Popen call) the handler
    records the first signal instead of raising, so the call returns a usable
    handle; the caller then aborts and cleans up.  Outside that window the
    handler raises immediately.

    This defers delivery by *recording*, not by masking, so a child never
    inherits a blocked SIGINT/SIGTERM and a real child's SIGTERM handling is
    unchanged.
    """

    def __init__(self) -> None:
        self.deferring = False
        self.pending_signum: int | None = None

    def reset(self) -> None:
        self.deferring = False
        self.pending_signum = None

    def handle(self, signum: int) -> None:
        if self.deferring:
            if self.pending_signum is None:
                self.pending_signum = int(signum)
            return
        raise BenchmarkInterrupted(signum)

    def take_pending(self) -> int | None:
        signum = self.pending_signum
        self.pending_signum = None
        return signum


_SIGNAL_GATE = _SignalGate()


def _raise_interrupt(signum, frame) -> None:  # noqa: ANN001 - signal API
    _SIGNAL_GATE.handle(signum)


def _install_interrupt_handlers() -> dict[int, object]:
    """Install SIGINT/SIGTERM handlers; return the previously installed ones.

    Only the main thread may install handlers.  If installation is impossible
    (for example a non-main-thread caller), that signal is skipped rather than
    crashing; the harness is intended to run as a main-thread CLI process.
    """
    previous: dict[int, object] = {}
    for sig in _INTERRUPT_SIGNALS:
        try:
            previous[sig] = signal.signal(sig, _raise_interrupt)
        except (OSError, ValueError):
            continue
    return previous


def _restore_interrupt_handlers(previous: dict[int, object]) -> None:
    for sig, handler in previous.items():
        try:
            signal.signal(sig, handler)
        except (OSError, TypeError, ValueError):
            pass


@contextlib.contextmanager
def _defer_interrupt_signals():
    """Block SIGINT/SIGTERM for a bounded critical section.

    Used only around the kill+reap sequence so a second Ctrl-C cannot abort the
    cleanup and leave a compressor alive.  The pending signal, if any, is
    delivered after the block is released.  On platforms without
    ``pthread_sigmask`` this is a no-op.
    """
    if os.name != "posix" or not hasattr(signal, "pthread_sigmask"):
        yield
        return
    blocked = set(_INTERRUPT_SIGNALS)
    signal.pthread_sigmask(signal.SIG_BLOCK, blocked)
    try:
        yield
    finally:
        signal.pthread_sigmask(signal.SIG_UNBLOCK, blocked)


def _signal_name(signum: int) -> str:
    try:
        return signal.Signals(signum).name
    except ValueError:
        return f"signal {signum}"


def _atomic_write(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent, prefix=f".{path.name}.", suffix=".tmp"
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _atomic_json(path: Path, value: object) -> None:
    _atomic_write(path, (json.dumps(value, indent=2) + "\n").encode("utf-8"))


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _file_sha256_or_none(path: Path) -> str | None:
    try:
        return file_sha256(path)
    except OSError:
        return None


def parse_size(value: str) -> int:
    """Strict binary-power size parser matching the srep CLI semantics."""
    if not value or any(character.isspace() for character in value):
        raise BenchmarkError(f"invalid size: {value!r}")
    match = _SIZE_RE.match(value)
    if match is None:
        raise BenchmarkError(f"invalid size: {value!r}")
    multiplier = _SIZE_MULTIPLIERS.get(match.group("suffix").lower())
    if multiplier is None:
        raise BenchmarkError(f"unknown size unit in {value!r}")
    result = int(match.group("number")) * multiplier
    if result >= 1 << 64:
        raise BenchmarkError(f"size overflows u64: {value!r}")
    return result


def load_fidelity_module(path: Path = FIDELITY_PATH):
    """Import the frozen-recipe module by file path (never by cwd/package name)."""
    if not path.is_file():
        raise BenchmarkError(f"frozen recipe module not found: {path}")
    # The recipe module imports fidelity_baseline_pin from its own directory.
    scripts_text = os.fspath(path.parent)
    added = scripts_text not in sys.path
    if added:
        sys.path.insert(0, scripts_text)
    try:
        spec = importlib.util.spec_from_file_location("_benchmark_sparse_fidelity", path)
        if spec is None or spec.loader is None:
            raise BenchmarkError(f"cannot load frozen recipe module: {path}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module
    finally:
        if added:
            try:
                sys.path.remove(scripts_text)
            except ValueError:
                pass


class SampleRecipe:
    """A resolved frozen sample: id, manifest-declared size and SHA-256."""

    def __init__(self, sample_id: str, size: int, sha256: str, data: bytes) -> None:
        self.sample_id = sample_id
        self.size = size
        self.sha256 = sha256
        self.data = data

    def verify_bytes(self, data: bytes) -> None:
        if len(data) != self.size:
            raise BenchmarkError(
                f"sample {self.sample_id} length {len(data)} != required {self.size}"
            )
        actual = sha256_bytes(data)
        if actual != self.sha256:
            raise BenchmarkError(
                f"sample {self.sample_id} SHA-256 {actual} != required {self.sha256}"
            )


def resolve_sample(module, sample_id: str) -> SampleRecipe:
    """Resolve a frozen sample from the checked-in manifest via its generator.

    The bytes are reconstructed with the manifest's own recipe, then verified
    against the manifest-declared length and SHA-256 before they are ever used.
    """
    manifest_path = module.CORPUS_MANIFEST
    manifest = module.strict_json(manifest_path)
    row = next(
        (item for item in manifest.get("samples", []) if item.get("id") == sample_id),
        None,
    )
    if row is None:
        raise BenchmarkError(f"sample {sample_id} is not in the frozen corpus manifest")
    args = row["args"]
    data = module.make_sample(
        str(args["recipe"]), {"size": int(args["size"]), "seed": int(args["seed"])}
    )
    recipe = SampleRecipe(sample_id, int(row["size"]), str(row["sha256"]), data)
    recipe.verify_bytes(data)
    if sample_id == DEFAULT_SAMPLE and (
        recipe.size != DEFAULT_SAMPLE_SIZE or recipe.sha256 != DEFAULT_SAMPLE_SHA256
    ):
        raise BenchmarkError(
            f"default sample {DEFAULT_SAMPLE} manifest contract changed "
            f"(size={recipe.size}, sha256={recipe.sha256})"
        )
    return recipe


def frozen_compress_argv(module, binary: Path, method: str, source: Path, archive: Path,
                         memory: str, temp_dir: Path, temp_limit: str) -> list[str]:
    """Build the frozen compress argv shared byte-for-byte by both binaries.

    Uses the checked-in ``fidelity.py`` argv builder so the frozen
    method/layout/checksum/block/min-match recipe cannot drift.
    """
    resource = {
        "memory": memory,
        "temp_dir": os.fspath(temp_dir),
        "temp_limit": temp_limit,
    }
    return module.new_argv(binary, method, source, archive, resource=resource)


def _decode(value: object) -> str:
    if isinstance(value, bytes):
        return value.decode("utf-8", "replace")
    if isinstance(value, str):
        return value
    return ""


def _capture_identity(binary: Path, *, timeout: float) -> dict[str, object]:
    """Capture absolute path, SHA-256, and ``--version`` before any row runs.

    A missing hash or a failing version probe is reported as unavailable, never
    as a passing identity, and raises so a partial report is still persisted.
    """
    binary = binary.resolve()
    sha = _file_sha256_or_none(binary)
    entry: dict[str, object] = {
        "path": os.fspath(binary),
        "sha256": sha,
        "version": None,
        "available": False,
    }
    if sha is None:
        entry["error"] = "binary is missing or unreadable"
        raise BenchmarkError(f"binary hash unavailable: {binary}")
    try:
        result = run_captured(
            [os.fspath(binary), "--version"], timeout=timeout, env_extra={},
            stage="identity-probe",
        )
    except _TimeoutExpired as error:
        entry["error"] = "binary --version timed out"
        raise BenchmarkError(f"binary --version failed: {binary}: {error}") from error
    except BenchmarkError as error:
        entry["error"] = str(error)
        raise BenchmarkError(f"binary --version failed: {binary}: {error}") from error
    version = str(result["stdout_preview"]).strip()
    if result["exit_code"] != 0:
        entry["version"] = version or None
        raise BenchmarkError(
            f"binary --version exited {result['exit_code']}: {binary}"
        )
    if not version:
        raise BenchmarkError(f"binary --version produced no output: {binary}")
    entry["version"] = version
    entry["available"] = True
    return entry


def _recheck_identity(binary: Path, identity: dict[str, object], *, phase: str) -> None:
    binary = binary.resolve()
    current = _file_sha256_or_none(binary)
    expected = identity.get("sha256")
    if current is None:
        raise BenchmarkError(f"binary became unreadable during {phase}: {binary}")
    if current != expected:
        raise BenchmarkError(
            f"binary mutated during {phase}: expected {expected} got {current}"
        )


class _TimeoutExpired(RuntimeError):
    """Internal marker raised by run_captured on a bounded timeout.

    Carries the partial `record` captured before the kill so the caller can
    persist stdout/stderr/termination evidence instead of losing it.
    """

    def __init__(self, message: str, record: dict[str, object]) -> None:
        super().__init__(message)
        self.record = record


def _read_preview(path: Path, limit: int = PREVIEW_BYTES) -> str:
    with path.open("rb") as stream:
        return _decode(stream.read(limit))


def run_captured(
    command: list[str],
    *,
    timeout: float,
    env_extra: dict[str, str],
    log_dir: Path | None = None,
    stage: str = "command",
    stdin=None,
    cwd: Path | None = None,
) -> dict[str, object]:
    """Run one command and bound its process group.

    Full stdout/stderr are streamed to files (per-stage files under ``log_dir``
    when given, otherwise a private temporary file that is read back as a
    bounded preview).  Only a bounded preview enters the returned record, so a
    huge stream can never enter memory or the JSON report; the files remain the
    authoritative transcript.  On timeout the process group receives SIGTERM,
    then SIGKILL after a bounded grace period.  A timeout raises
    :class:`_TimeoutExpired` carrying the same record so the caller can persist
    a partial transcript instead of losing the failed stage.
    """
    environment = dict(os.environ)
    environment.update(env_extra)
    start = time.monotonic()
    ephemeral: tempfile.TemporaryDirectory | None = None
    if log_dir is not None:
        log_dir.mkdir(parents=True, exist_ok=True)
        out_path = log_dir / f"{stage}.stdout"
        err_path = log_dir / f"{stage}.stderr"
    else:
        ephemeral = tempfile.TemporaryDirectory(prefix="bench-sparse-probe-")
        probe_root = Path(ephemeral.name)
        out_path = probe_root / "stdout"
        err_path = probe_root / "stderr"
    process: subprocess.Popen | None = None
    launched_error: OSError | None = None
    try:
        with out_path.open("wb") as out_file, err_path.open("wb") as err_file:
            # Popen can create the child before its handle is available.  Defer
            # handler raising for exactly this call so a signal delivered in
            # that window is recorded, the handle is still obtained, and the
            # ownership/cleanup paths below run.  The deferral is by recording,
            # not masking, so nothing is inherited by the child.
            _SIGNAL_GATE.deferring = True
            try:
                process = subprocess.Popen(
                    command,
                    stdin=stdin if stdin is not None else subprocess.DEVNULL,
                    stdout=out_file,
                    stderr=err_file,
                    env=environment,
                    cwd=os.fspath(cwd) if cwd is not None else None,
                    start_new_session=True,
                )
            except OSError as error:
                launched_error = error
            finally:
                _SIGNAL_GATE.deferring = False
            # Ownership is established once control returns here (or the spawn
            # already failed).  A signal recorded during Popen aborts now; the
            # outer handler kills and reaps the group before propagating.
            pending = _SIGNAL_GATE.take_pending()
            if pending is not None:
                raise BenchmarkInterrupted(pending)
            if launched_error is not None:
                raise BenchmarkError(
                    f"cannot start {command[0]}: {launched_error}"
                ) from launched_error
            process.wait(timeout=timeout)
        return _stream_record(
            command, out_path, err_path,
            exit_code=process.returncode, elapsed=time.monotonic() - start,
            timed_out=False,
        )
    except subprocess.TimeoutExpired as expired:
        reaped = _terminate_group(process) if process is not None else False
        record = _stream_record(
            command, out_path, err_path,
            exit_code=None, elapsed=time.monotonic() - start, timed_out=True,
        )
        record["leader_reaped"] = reaped
        raise _TimeoutExpired("command timed out", record) from expired
    except BaseException:
        # KeyboardInterrupt / BenchmarkInterrupted / any abnormal unwind
        # (including one raised by an injected fault or handler immediately
        # after the handle was assigned): boundedly kill and reap this command's
        # process group before propagating, so Ctrl-C or SIGTERM cannot orphan a
        # multi-hour compressor.  Only this command's own group is signalled.
        if process is not None:
            _terminate_group(process)
        raise
    finally:
        if ephemeral is not None:
            ephemeral.cleanup()


def _stream_record(command: list[str], out_path: Path, err_path: Path, *, exit_code,
                   elapsed: float, timed_out: bool) -> dict[str, object]:
    return {
        "argv": list(command),
        "exit_code": exit_code,
        "elapsed_seconds": elapsed,
        "timed_out": timed_out,
        "stdout_file": os.fspath(out_path),
        "stderr_file": os.fspath(err_path),
        "stdout_bytes": out_path.stat().st_size,
        "stderr_bytes": err_path.stat().st_size,
        "stdout_preview": _read_preview(out_path),
        "stderr_preview": _read_preview(err_path),
    }


def _signal_group(process: subprocess.Popen, sig: int) -> None:
    """Signal the original process group; fall back to the leader PID.

    ``killpg`` targets the whole group, which still exists even after the
    leader exits, so a SIGTERM-ignoring descendant is reached.  This is a
    POSIX/Linux contract; on platforms without ``killpg`` only the leader can
    be reached and the caller records that limitation.
    """
    if hasattr(os, "killpg"):
        try:
            os.killpg(process.pid, sig)
            return
        except ProcessLookupError:
            pass
        except OSError:
            pass
    try:
        if process.poll() is not None:
            return
        process.kill() if sig == signal.SIGKILL else process.terminate()
    except ProcessLookupError:
        return


def _terminate_group(process: subprocess.Popen) -> bool:
    """SIGTERM, then always SIGKILL the group after grace, then reap leader.

    The group SIGKILL is sent unconditionally after the grace window even when
    the leader already exited, because ``killpg`` still addresses the original
    group id and a TERM-ignoring descendant can otherwise survive.  The leader
    is reaped with bounded waits; there is no unbounded wait fallback.  Returns
    whether the leader was confirmed reaped (its return code is available).
    """
    # Defer further SIGINT/SIGTERM for the bounded kill window so a second
    # interrupt cannot abort cleanup and leave a compressor running.
    with _defer_interrupt_signals():
        _signal_group(process, signal.SIGTERM)
        reaped = True
        try:
            process.wait(timeout=REAP_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            reaped = False
        # The group SIGKILL is unconditional: killpg still addresses the
        # original group id even after the leader exits, so a TERM-ignoring
        # descendant is reached here.
        _signal_group(process, signal.SIGKILL)
        try:
            process.wait(timeout=REAP_GRACE_SECONDS)
            reaped = True
        except subprocess.TimeoutExpired:
            # The leader was force-killed but not confirmed reaped within grace;
            # a descendant that somehow escaped the group cannot retain our FDs
            # because streams are file-backed, not pipes.  Never wait unbounded.
            pass
    return reaped


# Non-consuming boundaries so multiple `ok=` tokens on one line are all found
# (a consuming trailing-space class would hide the second token).
_OK_RE = re.compile(r"(?<!\S)ok=(true|false)(?=\s|$)")


def _profile_event(match: re.Match[str]) -> dict[str, object]:
    detail = match.group("detail") or ""
    event: dict[str, object] = {
        "event": match.group("event"),
        "elapsed_ms": int(match.group("elapsed_ms")),
        "ram": int(match.group("ram")),
        "ram_peak": int(match.group("ram_peak")),
        "temp": int(match.group("temp")),
        "temp_peak": int(match.group("temp_peak")),
        "handles": int(match.group("handles")),
        "files": int(match.group("files")),
        "io": int(match.group("io")),
    }
    ok_values = _OK_RE.findall(detail)
    # `ok` is only meaningful when the line carries exactly one boolean token;
    # a duplicated or mixed `ok=true ok=false` line is ambiguous and yields None
    # plus a count so validation can reject it.
    event["ok"] = (ok_values[0] == "true") if len(ok_values) == 1 else None
    event["ok_count"] = len(ok_values)
    if detail:
        event["detail"] = detail
    return event


def parse_profile(stderr: str) -> dict[str, object]:
    """Parse OFFLOAD_PROFILE lines from text, distinguishing malformed/absent.

    `present` is True only when at least one well-formed line was found.
    `malformed` retains any line that starts with OFFLOAD_PROFILE but does not
    conform, so a broken profile is never silently treated as absent.
    """
    events: list[dict[str, object]] = []
    malformed: list[str] = []
    for line in stderr.splitlines():
        if not line.startswith("OFFLOAD_PROFILE"):
            continue
        match = _PROFILE_RE.match(line)
        if match is None:
            malformed.append(line)
            continue
        events.append(_profile_event(match))
    return {
        "present": bool(events),
        "events": events,
        "malformed": malformed,
        "final": events[-1] if events else None,
    }


def parse_profile_file(path: Path) -> dict[str, object]:
    """Stream a stderr transcript, retaining only parsed profile evidence.

    Malformed ``OFFLOAD_PROFILE`` lines are bounded to a small sample so a
    binary that emits millions of broken lines cannot inflate the report.
    """
    events: list[dict[str, object]] = []
    malformed: list[str] = []
    malformed_count = 0
    with path.open("r", encoding="utf-8", errors="replace") as stream:
        for line in stream:
            line = line.rstrip("\n")
            if not line.startswith("OFFLOAD_PROFILE"):
                continue
            match = _PROFILE_RE.match(line)
            if match is None:
                malformed_count += 1
                if len(malformed) < 20:
                    malformed.append(line[:512])
                continue
            events.append(_profile_event(match))
    return {
        "present": bool(events),
        "events": events,
        "malformed": malformed,
        "malformed_count": malformed_count,
        "final": events[-1] if events else None,
    }


def validate_profile(
    profile: dict[str, object],
    *,
    memory_bytes: int,
    temp_limit_bytes: int,
) -> None:
    """Strictly validate the compression profile or raise.

    A benchmark run is only valid when the profile is present and unmalformed,
    contains exactly one terminal ``complete`` event with exactly one
    unambiguous ``ok=true`` boolean, has no events after that terminal event,
    every retained field is a non-negative, internally consistent managed-ledger
    value (current <= peak), the managed peaks respect the configured budgets,
    and the cumulative ``elapsed_ms``/``ram_peak``/``temp_peak``/``io`` are
    monotonically non-decreasing across events.  Zero values remain valid where
    the contract allows them (for example a fully cleaned-up ``temp``/``files``);
    a spurious trailing zero can no longer be read as a fake improvement because
    it would break the high-water/cumulative monotonicity above.
    """
    if not profile.get("present"):
        raise BenchmarkError("OFFLOAD_PROFILE missing: no well-formed profile event")
    if profile.get("malformed_count") or profile.get("malformed"):
        raise BenchmarkError("OFFLOAD_PROFILE malformed: unparsable profile line present")
    events = profile["events"]
    if not events:
        raise BenchmarkError("OFFLOAD_PROFILE missing: no well-formed profile event")
    numeric_fields = ("elapsed_ms", "ram", "ram_peak", "temp", "temp_peak", "handles", "files", "io")
    #: Managed ledgers that are cumulative/high-water and must never decrease.
    monotonic_fields = ("elapsed_ms", "ram_peak", "temp_peak", "io")
    previous: dict[str, object] | None = None
    for index, event in enumerate(events):
        for field in numeric_fields:
            value = event.get(field)
            if not isinstance(value, int) or isinstance(value, bool) or value < 0:
                raise BenchmarkError(
                    f"OFFLOAD_PROFILE event {index} field {field} is invalid: {value!r}"
                )
        if event["ram"] > event["ram_peak"]:
            raise BenchmarkError(
                f"OFFLOAD_PROFILE event {index} ram exceeds ram_peak"
            )
        if event["temp"] > event["temp_peak"]:
            raise BenchmarkError(
                f"OFFLOAD_PROFILE event {index} temp exceeds temp_peak"
            )
        # temp/handles/files_peak may be zero on a clean run; only budgets are caps.
        if event["ram_peak"] > memory_bytes:
            raise BenchmarkError(
                f"OFFLOAD_PROFILE event {index} ram_peak {event['ram_peak']} "
                f"exceeds requested memory {memory_bytes}"
            )
        if event["temp_peak"] > temp_limit_bytes:
            raise BenchmarkError(
                f"OFFLOAD_PROFILE event {index} temp_peak {event['temp_peak']} "
                f"exceeds temp_limit {temp_limit_bytes}"
            )
        if previous is not None:
            for field in monotonic_fields:
                if event[field] < previous[field]:
                    raise BenchmarkError(
                        f"OFFLOAD_PROFILE event {index} {field} decreases from "
                        f"{previous[field]} to {event[field]}"
                    )
        previous = event
    completes = [event for event in events if event.get("event") == "complete"]
    if len(completes) != 1:
        raise BenchmarkError(
            "OFFLOAD_PROFILE must contain exactly one 'complete' event; "
            f"found {len(completes)}"
        )
    if events[-1].get("event") != "complete":
        raise BenchmarkError(
            "OFFLOAD_PROFILE has events after the terminal 'complete' event"
        )
    total_ok = sum(int(event.get("ok_count", 0)) for event in events)
    if total_ok != 1:
        raise BenchmarkError(
            "OFFLOAD_PROFILE must contain exactly one unambiguous ok token; "
            f"found {total_ok}"
        )
    final = events[-1]
    if final.get("ok_count") != 1 or final.get("ok") is not True:
        raise BenchmarkError("OFFLOAD_PROFILE terminal event must be exactly ok=true")


def _directory_empty(path: Path) -> list[str]:
    try:
        return sorted(os.listdir(path))
    except OSError as error:
        return [f"<unreadable: {error}>"]


class RunRecorder:
    """Accumulates one binary's stage captures and its failure state.

    Every stage record carries the persisted stdout/stderr file paths plus
    bounded previews.  The compression stage additionally gets a streamed
    profile parse.  A single cohesive :meth:`fail` helper records the failing
    stage and reason so no error path can leave ``failure`` unset while an
    exception propagates.
    """

    def __init__(self, label: str, log_dir: Path) -> None:
        self.label = label
        self.log_dir = log_dir
        self.stages: dict[str, object] = {}
        self.failure: str | None = None
        self.error: str | None = None

    def record(self, name: str, record: dict[str, object]) -> None:
        self.stages[name] = record

    def fail(self, stage: str, reason: str, message: str) -> BenchmarkError:
        if self.failure is None:
            self.failure = reason
            self.error = message
        return BenchmarkError(f"{self.label}: {message}")


def _partial_stage_record(
    recorder: RunRecorder, stage: str, argv: list[str]
) -> dict[str, object]:
    """Build a bounded partial stage record from files written before unwind."""
    out_path = recorder.log_dir / f"{stage}.stdout"
    err_path = recorder.log_dir / f"{stage}.stderr"
    record: dict[str, object] = {
        "argv": list(argv),
        "exit_code": None,
        "elapsed_seconds": None,
        "timed_out": False,
        "interrupted": True,
        "stdout_file": os.fspath(out_path) if out_path.exists() else None,
        "stderr_file": os.fspath(err_path) if err_path.exists() else None,
        "stdout_bytes": out_path.stat().st_size if out_path.exists() else 0,
        "stderr_bytes": err_path.stat().st_size if err_path.exists() else 0,
        "stdout_preview": _read_preview(out_path) if out_path.exists() else "",
        "stderr_preview": _read_preview(err_path) if err_path.exists() else "",
    }
    return record


def _run_stage(
    recorder: RunRecorder,
    stage: str,
    argv: list[str],
    *,
    timeout: float,
    env_extra: dict[str, str],
    pre_stage=None,
) -> dict[str, object]:
    """Run one stage with uniform timeout/error capture.

    Any timeout or spawn error is recorded against the stage (with whatever
    partial output was captured) and re-raised as a BenchmarkError, so callers
    never lose the failed stage and never leave ``recorder.failure`` unset.
    ``pre_stage`` (when given) re-validates the binary identity immediately
    before the stage command is started.
    """
    if pre_stage is not None:
        try:
            pre_stage()
        except BenchmarkError as error:
            raise recorder.fail(stage, "identity-changed", str(error)) from error
    try:
        record = run_captured(
            argv, timeout=timeout, env_extra=env_extra,
            log_dir=recorder.log_dir, stage=stage,
        )
    except BenchmarkInterrupted:
        # The child group was already bounded-killed and reaped by run_captured.
        # Persist whatever partial transcript exists for this stage, then let the
        # interruption propagate to the benchmark-level durable-report handler.
        recorder.record(stage, _partial_stage_record(recorder, stage, argv))
        raise
    except _TimeoutExpired as error:
        record = error.record
        recorder.record(stage, record)
        raise recorder.fail(stage, f"{stage}-timeout", f"{stage} timed out") from error
    except BenchmarkError as error:
        recorder.record(stage, {
            "argv": list(argv),
            "exit_code": None,
            "elapsed_seconds": None,
            "timed_out": False,
            "stdout_file": None,
            "stderr_file": None,
            "stdout_bytes": 0,
            "stderr_bytes": 0,
            "stdout_preview": "",
            "stderr_preview": str(error),
        })
        raise recorder.fail(stage, f"{stage}-spawn-error", f"{stage} failed to start: {error}")
    recorder.record(stage, record)
    return record


def roundtrip_sample(
    binary: Path,
    method: str,
    source: Path,
    directory: Path,
    *,
    memory: str,
    temp_limit: str,
    memory_bytes: int,
    temp_limit_bytes: int,
    temp_dir: Path,
    timeout: float,
    recorder: RunRecorder,
    expected_sample_size: int,
    identity: dict[str, object],
) -> dict[str, object]:
    """compress -> info -> test -> decompress -> byte compare for one binary."""
    archive = directory / f"{source.stem}-{method}.srep"
    restored = directory / f"{source.stem}-{method}.restored"
    module = load_fidelity_module()

    def check_identity() -> None:
        _recheck_identity(binary, identity, phase=f"stage-{recorder.label}-{method}")

    compress_argv = frozen_compress_argv(
        module, binary, method, source, archive, memory, temp_dir, temp_limit
    )
    compress = _run_stage(
        recorder, "compress", compress_argv, timeout=timeout, env_extra=PROFILE_ENV,
        pre_stage=check_identity,
    )
    if compress["exit_code"] != 0:
        raise recorder.fail(
            "compress", "compress-nonzero",
            f"compress exited {compress['exit_code']}",
        )
    if not archive.is_file():
        raise recorder.fail("compress", "compress-no-archive", "compress produced no archive")
    archive_sha256 = file_sha256(archive)

    # Strict profile validation gates the run before any further stage.  The
    # parsed profile is attached to the compress stage so a partial report keeps
    # the (possibly invalid) evidence.
    profile = parse_profile_file(Path(str(compress["stderr_file"])))
    compress["profile"] = profile
    try:
        validate_profile(
            profile, memory_bytes=memory_bytes, temp_limit_bytes=temp_limit_bytes
        )
    except BenchmarkError as error:
        raise recorder.fail("compress", "profile-invalid", str(error)) from error

    info_argv = [os.fspath(binary), "info", os.fspath(archive)]
    info = _run_stage(
        recorder, "info", info_argv, timeout=timeout, env_extra={}, pre_stage=check_identity
    )
    if info["exit_code"] != 0:
        raise recorder.fail("info", "info-nonzero", f"info exited {info['exit_code']}")
    try:
        info_values = parse_info(
            Path(str(info["stdout_file"])),
            expected_size=expected_sample_size,
            expected_method=method,
        )
    except BenchmarkError as error:
        raise recorder.fail("info", "info-invalid", str(error)) from error

    test_argv = [os.fspath(binary), "test", os.fspath(archive)]
    test = _run_stage(
        recorder, "test", test_argv, timeout=timeout, env_extra={}, pre_stage=check_identity
    )
    if test["exit_code"] != 0:
        raise recorder.fail("test", "test-nonzero", f"test exited {test['exit_code']}")

    decompress_argv = [
        os.fspath(binary),
        "decompress",
        "--memory",
        memory,
        "--temp-dir",
        os.fspath(temp_dir),
        "--temp-limit",
        temp_limit,
        os.fspath(archive),
        os.fspath(restored),
    ]
    decompress = _run_stage(
        recorder, "decompress", decompress_argv, timeout=timeout, env_extra={},
        pre_stage=check_identity,
    )
    if decompress["exit_code"] != 0:
        raise recorder.fail(
            "decompress", "decompress-nonzero",
            f"decompress exited {decompress['exit_code']}",
        )
    if not restored.is_file():
        raise recorder.fail(
            "decompress", "decompress-no-output", "decompress produced no restored file"
        )
    if restored.read_bytes() != source.read_bytes():
        raise recorder.fail("decompress", "roundtrip-mismatch", "round-trip byte mismatch")

    # Re-verify the frozen input after the restored-byte comparison; a mutated
    # input would make the comparison meaningless, so both corruption paths
    # fail rather than passing.
    if sha256_bytes(source.read_bytes()) != sha256_bytes(restored.read_bytes()):
        raise recorder.fail(
            "decompress", "input-mutated", "frozen input changed during the run"
        )

    # Re-hash the archive after the whole stage sequence: a mutation during
    # info/test/decompress invalidates the run rather than being reported as a
    # passing identity.
    post_sha256 = file_sha256(archive)
    if post_sha256 != archive_sha256:
        raise recorder.fail(
            "decompress", "archive-mutated",
            f"archive mutated during run (expected {archive_sha256} got {post_sha256})",
        )

    return {
        "archive": {
            "path": os.fspath(archive),
            "sha256": archive_sha256,
            "bytes": archive.stat().st_size,
        },
        "info": info_values,
        "compress_profile": profile,
    }


def parse_info(
    path: Path,
    *,
    expected_size: int,
    expected_method: str,
) -> dict[str, object]:
    """Parse and validate the required `info` counters/scalars.

    Required integer fields must each appear exactly once and be non-negative;
    ``original size`` must equal the frozen sample size; ``method``/``layout``/
    ``checksum`` must match the frozen recipe.  Missing, duplicated, negative,
    non-integer, or mismatched fields raise instead of passing.
    """
    required_ints = (
        "original size", "payload size", "blocks",
        "semantic matches", "covered bytes", "literal bytes",
    )
    required_strings = {"method", "layout", "checksum"}
    int_values: dict[str, int] = {}
    string_values: dict[str, str] = {}
    seen: dict[str, int] = {}
    with path.open("r", encoding="utf-8", errors="replace") as stream:
        for line in stream:
            line = line.rstrip("\n")
            if ": " not in line:
                continue
            key, value = line.split(": ", 1)
            if key not in required_ints and key not in required_strings:
                continue
            seen[key] = seen.get(key, 0) + 1
            if seen[key] > 1:
                raise BenchmarkError(f"duplicate info field {key!r}")
            if key in required_ints:
                try:
                    parsed = int(value)
                except ValueError as error:
                    raise BenchmarkError(
                        f"non-integer info field {key}={value!r}"
                    ) from error
                if parsed < 0:
                    raise BenchmarkError(f"negative info field {key}={parsed}")
                int_values[key] = parsed
            else:
                string_values[key] = value
    missing = (set(required_ints) | required_strings) - set(seen)
    if missing:
        raise BenchmarkError(f"incomplete info output; missing {sorted(missing)}")
    if int_values["original size"] != expected_size:
        raise BenchmarkError(
            f"info original size {int_values['original size']} != frozen {expected_size}"
        )
    if string_values["method"] != expected_method:
        raise BenchmarkError(
            f"info method {string_values['method']!r} != frozen {expected_method!r}"
        )
    if string_values["layout"] != FROZEN_LAYOUT:
        raise BenchmarkError(
            f"info layout {string_values['layout']!r} != frozen {FROZEN_LAYOUT!r}"
        )
    if string_values["checksum"] != FROZEN_CHECKSUM:
        raise BenchmarkError(
            f"info checksum {string_values['checksum']!r} != frozen {FROZEN_CHECKSUM!r}"
        )
    values: dict[str, object] = dict(int_values)
    values.update(string_values)
    return values


def _metric_ratio(baseline: float | int | None, candidate: float | int | None) -> dict[str, object]:
    """Before/after ratio for reporting only; never an assertion gate."""
    if baseline is None or candidate is None:
        return {"baseline": baseline, "candidate": candidate, "ratio": None}
    if baseline == 0:
        return {"baseline": baseline, "candidate": candidate, "ratio": None}
    return {
        "baseline": baseline,
        "candidate": candidate,
        "ratio": candidate / baseline,
    }


def _final_profile_metric(profile: dict[str, object], key: str) -> int | None:
    final = profile.get("final")
    if isinstance(final, dict) and isinstance(final.get(key), int):
        return int(final[key])
    return None


def run_one(
    label: str,
    binary: Path,
    method: str,
    source: Path,
    work_dir: Path,
    *,
    memory: str,
    temp_limit: str,
    memory_bytes: int,
    temp_limit_bytes: int,
    timeout: float,
    identity: dict[str, object],
) -> dict[str, object]:
    """Run the full round-trip for one binary and return its result block."""
    run_dir = work_dir / f"{label}-{method}"
    run_dir.mkdir(parents=True, exist_ok=False)
    temp_dir = run_dir / "temp"
    temp_dir.mkdir(parents=True, exist_ok=False)
    log_dir = run_dir / "logs"
    log_dir.mkdir(parents=True, exist_ok=False)

    recipe = _ACTIVE_RECIPE
    # Re-verify the frozen input immediately before this run.
    try:
        source_bytes = source.read_bytes()
        recipe.verify_bytes(source_bytes)
    except (OSError, BenchmarkError) as error:
        return {
            "label": label,
            "method": method,
            "binary": identity,
            "temp_dir": os.fspath(temp_dir),
            "status": "incomplete",
            "failure": "input-invalid",
            "error": str(error),
            "stages": {},
            "cleanup": {"verified_empty": not _directory_empty(temp_dir),
                        "residue": _directory_empty(temp_dir)},
        }
    input_identity = {
        "path": os.fspath(source),
        "bytes": len(source_bytes),
        "sha256": sha256_bytes(source_bytes),
    }

    recorder = RunRecorder(label, log_dir)
    result: dict[str, object] = {
        "label": label,
        "method": method,
        "binary": identity,
        "input": input_identity,
        "temp_dir": os.fspath(temp_dir),
        "status": "incomplete",
        "cleanup": {"verified_empty": False, "residue": None},
    }
    roundtrip: dict[str, object] | None = None
    try:
        roundtrip = roundtrip_sample(
            binary,
            method,
            source,
            run_dir,
            memory=memory,
            temp_limit=temp_limit,
            memory_bytes=memory_bytes,
            temp_limit_bytes=temp_limit_bytes,
            temp_dir=temp_dir,
            timeout=timeout,
            recorder=recorder,
            expected_sample_size=recipe.size,
            identity=identity,
        )
    except BenchmarkInterrupted as error:
        # The child group is already bounded-killed by run_captured.  Finalize
        # this partial run, attach it to the exception so the benchmark handler
        # can persist it, then keep unwinding.
        if recorder.failure is None:
            recorder.failure = "interrupted"
            recorder.error = f"interrupted by {_signal_name(error.signum)}"
        _finalize_run_result(result, recorder, temp_dir, None)
        error.run_result = result  # type: ignore[attr-defined]
        raise
    except BenchmarkError as error:
        result["error"] = str(error)
    except Exception as error:  # noqa: BLE001 - keep the run's partial evidence
        # Any unexpected failure must still produce a durable, non-passing run
        # record rather than escaping and losing the captured stages.
        if recorder.failure is None:
            recorder.failure = "unexpected-error"
            recorder.error = f"{type(error).__name__}: {error}"
        result["error"] = recorder.error
    _finalize_run_result(result, recorder, temp_dir, roundtrip)
    return result


def _finalize_run_result(
    result: dict[str, object],
    recorder: RunRecorder,
    temp_dir: Path,
    roundtrip: dict[str, object] | None,
) -> dict[str, object]:
    result["stages"] = recorder.stages
    result["failure"] = recorder.failure
    if recorder.error and "error" not in result:
        result["error"] = recorder.error
    residue = _directory_empty(temp_dir)
    result["cleanup"] = {"verified_empty": not residue, "residue": residue}
    if roundtrip is not None and recorder.failure is None and not residue:
        result.update(roundtrip)
        result["status"] = "complete"
    elif recorder.failure is None and roundtrip is not None and residue:
        result["failure"] = "temp-cleanup-failed"
        result["error"] = "private temp directory not empty after run"
    elif recorder.failure is None:
        # Defensive: an unexpected exception left no explicit failure state.
        result["failure"] = "unknown-error"
        result.setdefault("error", "run did not complete")
    return result


_ACTIVE_RECIPE: SampleRecipe | None = None


def _check_work_dir(work_dir: Path, report: Path) -> None:
    if work_dir.exists():
        if not work_dir.is_dir():
            raise BenchmarkError(f"work dir is not a directory: {work_dir}")
        contents = os.listdir(work_dir)
        if contents:
            raise BenchmarkError(
                f"refusing existing non-empty work dir {work_dir}: {sorted(contents)[:5]}"
            )
    if report.exists():
        raise BenchmarkError(f"refusing to overwrite existing report: {report}")


def benchmark(args: argparse.Namespace) -> int:
    global _ACTIVE_RECIPE
    baseline_bin = args.baseline_bin.resolve()
    candidate_bin = args.candidate_bin.resolve()
    work_dir = args.work_dir.resolve()
    report = args.report.resolve()

    # Process-group timeout semantics (SIGTERM then SIGKILL the whole group)
    # are a POSIX contract.  Refuse to run where that cannot be honored rather
    # than pretending a native Windows/macOS guarantee this harness does not
    # provide or test.
    if os.name != "posix" or not hasattr(os, "killpg"):
        raise BenchmarkError(
            "process-group timeout semantics require a POSIX platform with "
            "killpg; refusing to run on this platform"
        )

    if baseline_bin == candidate_bin:
        raise BenchmarkError("baseline and candidate binaries must differ")
    for label, binary in (("baseline", baseline_bin), ("candidate", candidate_bin)):
        if not binary.is_file():
            raise BenchmarkError(f"{label} binary not found: {binary}")

    methods = [item.strip() for item in args.methods.split(",") if item.strip()]
    if not methods:
        raise BenchmarkError("--methods must select at least one method")
    unsupported = [method for method in methods if method not in VALID_METHODS]
    if unsupported:
        raise BenchmarkError(f"unsupported methods: {unsupported}")
    if len(set(methods)) != len(methods):
        raise BenchmarkError(f"duplicate methods selected: {methods}")

    # Validate sizes up front with the same binary-power semantics as the CLI,
    # and require strictly positive budgets (zero is not a usable budget).
    memory_bytes = parse_size(args.memory)
    temp_limit_bytes = parse_size(args.temp_limit)
    if memory_bytes <= 0:
        raise BenchmarkError("--memory must be a positive size")
    if temp_limit_bytes <= 0:
        raise BenchmarkError("--temp-limit must be a positive size")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        raise BenchmarkError("--timeout must be a positive number of seconds")

    # Install bounded SIGINT/SIGTERM handling before any child can be spawned.
    # A signal raises BenchmarkInterrupted, which unwinds through every
    # process-group kill/reap and lands at the handler below to persist a
    # durable, non-passing report.  Handlers are restored in the finally.
    handlers = _install_interrupt_handlers()
    _SIGNAL_GATE.reset()
    report_data: dict[str, object] | None = None
    try:
        _check_work_dir(work_dir, report)
        work_dir.mkdir(parents=True, exist_ok=True)
        logs_root = work_dir / "logs"
        logs_root.mkdir(parents=True, exist_ok=True)

        module = load_fidelity_module()
        try:
            recipe = resolve_sample(module, args.sample)
        except BenchmarkError:
            _remove_created_work_dir(work_dir)
            raise
        _ACTIVE_RECIPE = recipe
        source = work_dir / f"{recipe.sample_id}.bin"
        _atomic_write(source, recipe.data)
        # Re-verify the just-written input before the identity capture.
        recipe.verify_bytes(source.read_bytes())

        report_data = {
            "schema": "srep-sparse-benchmark-v1",
            "sample": {
                "id": recipe.sample_id,
                "bytes": recipe.size,
                "sha256": recipe.sha256,
                "generator": "scripts/fidelity.py:make_sample",
                "source": os.fspath(source),
            },
            "frozen": {
                "methods": methods,
                "layout": FROZEN_LAYOUT,
                "checksum": FROZEN_CHECKSUM,
                "block_size": FROZEN_BLOCK_SIZE,
                "memory": args.memory,
                "memory_bytes": memory_bytes,
                "temp_limit": args.temp_limit,
                "temp_limit_bytes": temp_limit_bytes,
                "timeout_seconds": args.timeout,
                "profile_env": PROFILE_ENV,
            },
            "host": _host_metadata(),
            "binaries": {"baseline": None, "candidate": None},
            "runs": {"baseline": {}, "candidate": {}},
            "comparison": {},
            "pass": False,
            "complete": False,
        }

        def persist() -> None:
            _atomic_json(report, report_data)

        identity: dict[str, dict[str, object]] = {}
        try:
            for label, binary in (("baseline", baseline_bin), ("candidate", candidate_bin)):
                captured = _capture_identity(binary, timeout=args.timeout)
                identity[label] = captured
                report_data["binaries"][label] = captured
            persist()

            for label, binary in (("baseline", baseline_bin), ("candidate", candidate_bin)):
                for method in methods:
                    # Re-check the binary identity and the frozen input immediately
                    # before every binary run, not only once at the very end.
                    _recheck_identity(binary, identity[label], phase=f"pre-{label}-{method}")
                    recipe.verify_bytes(source.read_bytes())
                    run = run_one(
                        label,
                        binary,
                        method,
                        source,
                        work_dir,
                        memory=args.memory,
                        temp_limit=args.temp_limit,
                        memory_bytes=memory_bytes,
                        temp_limit_bytes=temp_limit_bytes,
                        timeout=args.timeout,
                        identity=identity[label],
                    )
                    report_data["runs"][label].setdefault(method, run)
                    persist()
                    if run["status"] != "complete":
                        raise BenchmarkError(
                            f"{label} {method} did not complete: {run.get('failure')}"
                        )
                    # Confirm the binary is still the captured identity after each
                    # run, so a post-stage mutation fails before the next run.
                    _recheck_identity(binary, identity[label], phase=f"post-{label}-{method}")
                    # The input must still be the frozen bytes after every run.
                    recipe.verify_bytes(source.read_bytes())

            comparison = _compare_runs(report_data["runs"], methods)
            report_data["comparison"] = comparison
            report_data["complete"] = True
            report_data["pass"] = comparison["archives_equal"]
            persist()

            _print_summary(report_data, methods)
            if not comparison["archives_equal"]:
                print("SPARSE BENCHMARK: ARCHIVE MISMATCH", file=sys.stderr)
                return 3
            print("SPARSE BENCHMARK: COMPLETE (archive identity equal)")
            return 0
        except Exception as error:  # noqa: BLE001 - durable partial report required
            # Any failure (timeout, nonzero stage, unexpected I/O) leaves an
            # explicitly incomplete, non-passing report and a nonzero exit; a
            # "SUCCESS" banner is never printed for an incomplete run.
            report_data["complete"] = False
            report_data["pass"] = False
            report_data["error"] = str(error)
            try:
                persist()
            except OSError:
                pass
            print(f"benchmark-sparse: ERROR: {error}", file=sys.stderr)
            return 1
    except BenchmarkInterrupted as error:
        # A SIGINT/SIGTERM interrupted the run.  The in-flight run (if any) was
        # already killed and reaped by run_captured and finalized by run_one;
        # merge its partial result and persist an explicitly interrupted,
        # non-passing report with a nonzero exit.
        partial = getattr(error, "run_result", None)
        if report_data is not None and isinstance(partial, dict):
            label = partial.get("label")
            method = partial.get("method")
            runs = report_data.get("runs")
            if isinstance(runs, dict) and label in runs and isinstance(method, str):
                runs[label].setdefault(method, partial)
        name = _signal_name(error.signum)
        if report_data is not None:
            report_data["complete"] = False
            report_data["pass"] = False
            report_data["interrupted"] = {"signal": error.signum, "name": name}
            report_data["error"] = f"interrupted by {name}"
            try:
                _atomic_json(report, report_data)
            except OSError:
                pass
        print(
            f"benchmark-sparse: INTERRUPTED by {name}; partial report retained",
            file=sys.stderr,
        )
        return 128 + error.signum
    finally:
        _restore_interrupt_handlers(handlers)


def _compare_runs(runs: dict[str, object], methods: list[str]) -> dict[str, object]:
    comparison: dict[str, object] = {"archives_equal": True, "methods": {}}
    for method in methods:
        baseline = runs["baseline"][method]  # type: ignore[index]
        candidate = runs["candidate"][method]  # type: ignore[index]
        if baseline.get("status") != "complete" or candidate.get("status") != "complete":
            comparison["archives_equal"] = False
            comparison["methods"][method] = {"complete": False}
            continue
        baseline_archive = baseline["archive"]
        candidate_archive = candidate["archive"]
        equal = (
            baseline_archive["sha256"] == candidate_archive["sha256"]
            and baseline_archive["bytes"] == candidate_archive["bytes"]
        )
        if not equal:
            comparison["archives_equal"] = False
        baseline_profile = baseline["compress_profile"]
        candidate_profile = candidate["compress_profile"]
        comparison["methods"][method] = {
            "complete": True,
            "archive_sha256_equal": equal,
            "baseline_archive_sha256": baseline_archive["sha256"],
            "candidate_archive_sha256": candidate_archive["sha256"],
            # Before/after managed-ledger ratios, reporting only, not gates.
            "elapsed_seconds_ratio": _metric_ratio(
                baseline["stages"]["compress"]["elapsed_seconds"],
                candidate["stages"]["compress"]["elapsed_seconds"],
            ),
            "temp_peak_bytes_ratio": _metric_ratio(
                _final_profile_metric(baseline_profile, "temp_peak"),
                _final_profile_metric(candidate_profile, "temp_peak"),
            ),
            "ram_peak_bytes_ratio": _metric_ratio(
                _final_profile_metric(baseline_profile, "ram_peak"),
                _final_profile_metric(candidate_profile, "ram_peak"),
            ),
            "io_bytes_ratio": _metric_ratio(
                _final_profile_metric(baseline_profile, "io"),
                _final_profile_metric(candidate_profile, "io"),
            ),
        }
    return comparison


def _host_metadata() -> dict[str, object]:
    metadata: dict[str, object] = {
        "platform": sys.platform,
        "python": sys.version.split()[0],
        "machine": os.uname().machine if hasattr(os, "uname") else None,
    }
    toolchain: dict[str, str] = {}
    # Record only tool versions actually reported by a present toolchain; do not
    # guess a Rust toolchain when none is installed.
    for key, command in (("rustc", ["rustc", "--version"]), ("cargo", ["cargo", "--version"])):
        executable = shutil.which(command[0])
        if executable is None:
            continue
        try:
            result = subprocess.run(
                command, check=False, capture_output=True, text=True, timeout=10
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if result.returncode == 0 and result.stdout.strip():
            toolchain[key] = result.stdout.strip().splitlines()[0]
    metadata["toolchain"] = toolchain
    metadata["uname"] = os.uname().release if hasattr(os, "uname") else None
    return metadata


def _remove_created_work_dir(work_dir: Path) -> None:
    # Only remove the directory this invocation created and populated; a
    # pre-existing non-empty dir was rejected before creation.
    try:
        shutil.rmtree(work_dir)
    except OSError:
        pass


def _print_summary(report: dict[str, object], methods: list[str]) -> None:
    print(f"sample: {report['sample']['id']} ({report['sample']['bytes']} bytes)")
    for label in ("baseline", "candidate"):
        binary = report["binaries"][label]
        print(f"{label}: {binary.get('path')} sha256={binary.get('sha256')} "
              f"version={binary.get('version')!r}")
    for method in methods:
        entry = report["comparison"]["methods"].get(method, {})
        if not entry.get("complete"):
            print(f"{method}: incomplete")
            continue
        print(
            f"{method}: archive_equal={entry['archive_sha256_equal']} "
            f"elapsed_ratio={_ratio_text(entry['elapsed_seconds_ratio'])} "
            f"temp_peak_ratio={_ratio_text(entry['temp_peak_bytes_ratio'])} "
            f"ram_peak_ratio={_ratio_text(entry['ram_peak_bytes_ratio'])} "
            f"io_ratio={_ratio_text(entry['io_bytes_ratio'])}"
        )


def _ratio_text(ratio: dict[str, object]) -> str:
    value = ratio.get("ratio")
    return "n/a" if value is None else f"{value:.6f}"


def _positive_timeout(value: str) -> float:
    try:
        timeout = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("timeout must be a positive number") from error
    if not math.isfinite(timeout) or timeout <= 0:
        raise argparse.ArgumentTypeError("timeout must be a positive number")
    return timeout


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Bounded same-host before/after sparse benchmark for two srep binaries."
    )
    parser.add_argument("--baseline-bin", required=True, type=Path,
                        help="absolute path to the baseline srep binary")
    parser.add_argument("--candidate-bin", required=True, type=Path,
                        help="absolute path to the candidate srep binary")
    parser.add_argument("--work-dir", required=True, type=Path,
                        help="new (or empty) private directory for artifacts")
    parser.add_argument("--report", required=True, type=Path,
                        help="path for the JSON report (must not already exist)")
    parser.add_argument("--methods", default=DEFAULT_METHODS,
                        help=f"comma-separated methods (default: {DEFAULT_METHODS})")
    parser.add_argument("--sample", default=DEFAULT_SAMPLE,
                        help=f"frozen corpus sample id (default: {DEFAULT_SAMPLE})")
    parser.add_argument("--memory", default=DEFAULT_MEMORY,
                        help=f"frozen --memory value (default: {DEFAULT_MEMORY})")
    parser.add_argument("--temp-limit", default=DEFAULT_TEMP_LIMIT,
                        help=f"frozen --temp-limit value (default: {DEFAULT_TEMP_LIMIT})")
    parser.add_argument("--timeout", type=_positive_timeout, default=DEFAULT_TIMEOUT,
                        metavar="SECONDS",
                        help=f"per-command timeout in seconds (default: {DEFAULT_TIMEOUT:g})")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    try:
        return benchmark(args)
    except BenchmarkError as error:
        print(f"benchmark-sparse: ERROR: {error}", file=sys.stderr)
        return 1
    except BenchmarkInterrupted as error:
        # A signal delivered before the benchmark could build a report (for
        # example during argument-independent validation) still exits nonzero
        # with the conventional signal status.
        name = _signal_name(error.signum)
        print(f"benchmark-sparse: INTERRUPTED by {name}", file=sys.stderr)
        return 128 + error.signum


if __name__ == "__main__":
    raise SystemExit(main())

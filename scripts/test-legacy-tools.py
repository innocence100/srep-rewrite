#!/usr/bin/env python3
"""Focused Stage2 tests for the retained legacy-corpus tooling."""
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests" / "fixtures" / "legacy"
VALIDATOR = ROOT / "scripts" / "validate-legacy-fixtures.py"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")
DECODER = (TARGET if TARGET.is_absolute() else ROOT / TARGET) / "debug" / "srep"
OLD = Path("/home/test/.opencode/archiving-tools/srep/bin/srep")
TRUSTED_ROOT = Path("/tmp/opencode")
OUTPUT_RE = re.compile(r"^srep-legacy-(generate|differential)\.[0-9a-f]{32}$")
WORK_RE = re.compile(r"^\.work-[0-9a-f]{32}$")
EVIDENCE_RE = re.compile(r"^evidence-[0-9a-f]{32}$")
PRIVATE_RE = re.compile(r"^/proc/self/fd/(\d+)/([^/]+)$")


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(command, check=False, **kwargs)


def tree_digest(root: Path) -> str:
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix().encode()
        details = os.lstat(path)
        digest.update(relative + b"\0" + stat.S_IFMT(details.st_mode).to_bytes(4, "little"))
        if stat.S_ISREG(details.st_mode):
            digest.update(path.read_bytes())
    return digest.hexdigest()


def generator(*args: str) -> list[str]:
    return ["python3", str(ROOT / "scripts" / "generate-legacy-fixtures.py"), *args]


def wrapper(*args: str) -> list[str]:
    return [str(ROOT / "scripts" / "regenerate-legacy-fixtures.sh"), *args]


def assert_rejected(command: list[str], *, status: int | None = None) -> subprocess.CompletedProcess:
    result = run(command, capture_output=True, text=True)
    assert result.returncode != 0, result.stdout + result.stderr
    if status is not None:
        assert result.returncode == status, result.stdout + result.stderr
    return result


def make_old(path: Path, *, fail: bool = False) -> Path:
    path.write_text(
        "#!/usr/bin/env python3\n"
        "import json\n"
        "import os\n"
        "import pathlib\n"
        "import sys\n"
        + "private = sys.argv[-2:]\n"
        + "fds = []\n"
        + "for name in os.listdir('/proc/self/fd'):\n"
        + "    try:\n"
        + "        fds.append((int(name), os.readlink('/proc/self/fd/' + name)))\n"
        + "    except FileNotFoundError:\n"
        + "        pass\n"
        + "observation = {'argv': sys.argv[1:], 'cwd': os.getcwd(), 'env': dict(os.environ), 'fds': fds}\n"
        + "pathlib.Path(private[-1]).write_text(json.dumps(observation), encoding='utf-8')\n"
        + "assert all(argument.startswith('/proc/self/fd/') for argument in private)\n"
        + "matches = [remainder for argument in private for remainder in [argument[14:].split('/', 1)] if len(remainder) == 2]\n"
        + "assert matches and matches[0][0] == matches[1][0]\n"
        + "assert all(pathlib.Path(argument).parent.is_dir() for argument in private)\n"
        + "work_fd = int(matches[0][0])\n"
        + "assert set(observation['fds']) <= {0, 1, 2, work_fd}\n"
        + "assert observation['cwd'] == '/'\n"
        + "assert not any(key.startswith('SREP_') for key in observation['env'])\n"
        + ("raise SystemExit(17)\n" if fail else "")
    )
    path.chmod(0o700)
    return path


def make_fixture_old(path: Path) -> Path:
    path.write_text(
        "#!/usr/bin/env python3\n"
        "import pathlib\n"
        "import sys\n"
        f"fixture = pathlib.Path({str(FIXTURE)!r})\n"
        "args = sys.argv[1:]\n"
        "source, destination = args[-2:]\n"
        "assert source.startswith('/proc/self/fd/')\n"
        "assert destination.startswith('/proc/self/fd/')\n"
        "if args[0] == '-d':\n"
        "    pathlib.Path(destination).write_bytes((fixture / 'original.bin').read_bytes())\n"
        "else:\n"
        "    versions = {'-m3o': '1', '-m4o': '2', '-m4f': '3', '-m4': '4'}\n"
        "    checksums = {\n"
        "        '-hash=md5': 'md5', '-hash-': 'none', '-hash=sha1': 'sha1',\n"
        "        '-hash=sha512': 'sha512', '-hash=vmac': 'vmac',\n"
        "        '-hash=siphash': 'siphash',\n"
        "    }\n"
        "    version = next(versions[value] for value in args if value in versions)\n"
        "    checksum = next(checksums[value] for value in args if value in checksums)\n"
        "    pathlib.Path(destination).write_bytes((fixture / f'v{version}-{checksum}.srep').read_bytes())\n"
    )
    path.chmod(0o700)
    return path


def make_observing_old(path: Path, real: Path) -> Path:
    path.write_text(
        "#!/usr/bin/env python3\n"
        "import hashlib\n"
        "import json\n"
        "import os\n"
        "import pathlib\n"
        "import sys\n"
        f"real = {str(real)!r}\n"
        "private = sys.argv[-2:]\n"
        "fds = []\n"
        "for name in os.listdir('/proc/self/fd'):\n"
        "    try:\n"
        "        fds.append((int(name), os.readlink('/proc/self/fd/' + name)))\n"
        "    except FileNotFoundError:\n"
        "        pass\n"
        "observation = {'argv': sys.argv[1:], 'cwd': os.getcwd(), 'env': dict(os.environ), 'fds': fds}\n"
        "assert all(argument.startswith('/proc/self/fd/') for argument in private)\n"
        "assert not any(key.startswith('SREP_') for key in observation['env'])\n"
        "name = 'child-observation-' + hashlib.sha256(private[-1].encode()).hexdigest() + '.json'\n"
        "(pathlib.Path(private[0]).parent / name).write_text(json.dumps(observation), encoding='utf-8')\n"
        "os.execv(real, [real, *sys.argv[1:]])\n"
    )
    path.chmod(0o700)
    return path


def make_mutating_old(path: Path, state: Path, real: Path) -> Path:
    path.write_text(
        "#!/usr/bin/env python3\n"
        "import os\n"
        "import pathlib\n"
        "import subprocess\n"
        "import sys\n"
        f"real = {str(real)!r}\n"
        f"state = pathlib.Path({str(state)!r})\n"
        "target_root = None\n"
        "if len(sys.argv) > 1 and sys.argv[1] == '-d':\n"
        "    count = int(state.read_text()) if state.exists() else 0\n"
        "    state.write_text(str(count + 1))\n"
        "    if count == 23:\n"
        "        target = pathlib.Path(target_root) / 'v1-md5.srep'\n"
        "        with target.open('r+b') as stream:\n"
        "            stream.seek(0)\n"
        "            stream.write(bytes([stream.read(1)[0] ^ 1]))\n"
        "os.execv(real, [real, *sys.argv[1:]])\n"
    )
    path.chmod(0o700)
    return path


def make_output_binding_hook(wrapper_path: Path) -> Path:
    hook = wrapper_path.with_name("bind-output-hook")
    hook.write_text(
        "#!/usr/bin/env python3\n"
        "import pathlib\n"
        "import sys\n"
        f"wrapper = pathlib.Path({str(wrapper_path)!r})\n"
        "source = wrapper.read_text(encoding='utf-8')\n"
        "source = source.replace('target_root = None', f'target_root = {sys.argv[1]!r}')\n"
        "wrapper.write_text(source, encoding='utf-8')\n"
    )
    hook.chmod(0o700)
    return hook


def make_hook(path: Path) -> Path:
    path.write_text(
        "#!/usr/bin/env python3\n"
        "import os\n"
        "import pathlib\n"
        "import sys\n"
        "leaf = pathlib.Path(sys.argv[1])\n"
        "retained = pathlib.Path(os.environ['SREP_HOOK_RETAINED'])\n"
        "replacement = pathlib.Path(os.environ['SREP_HOOK_REPLACEMENT'])\n"
        "os.rename(leaf, retained)\n"
        "replacement.mkdir(mode=0o700)\n"
        "os.symlink(replacement, leaf)\n"
    )
    path.chmod(0o700)
    return path


def parse_output(result: subprocess.CompletedProcess) -> Path:
    line = next(line for line in result.stdout.splitlines() if line.startswith("output: "))
    output = Path(line.split(": ", 1)[1])
    assert output.parent == TRUSTED_ROOT
    assert OUTPUT_RE.fullmatch(output.name), output
    return output


def work_dirs(output: Path) -> list[Path]:
    return sorted(TRUSTED_ROOT.glob(".work-*"))


def output_dirs() -> list[Path]:
    return sorted(
        path for path in TRUSTED_ROOT.iterdir()
        if path.is_dir() and any(path.name.startswith(prefix) for prefix in ("srep-legacy-generate.", "srep-legacy-differential."))
    )


def private_observation(work: Path) -> dict:
    for path in work.iterdir():
        if not path.is_file():
            continue
        try:
            observation = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(observation, dict) and "argv" in observation and "env" in observation:
            return observation
    raise AssertionError(f"child observation not found in {work}")


def assert_private_observation(observation: dict) -> None:
    paths = [argument for argument in observation["argv"] if argument.startswith("/proc/self/fd/")]
    assert len(paths) == 2, observation
    matches = [PRIVATE_RE.fullmatch(path) for path in paths]
    assert all(matches), paths
    work_fd = matches[0].group(1)
    assert all(match.group(1) == work_fd for match in matches)
    assert all(match.group(2) and not match.group(2).startswith("v") for match in matches)
    assert observation["cwd"] == "/"
    assert not any(key.startswith("SREP_") for key in observation["env"])
    for fd, target in observation["fds"]:
        if fd in {0, 1, 2, int(work_fd)}:
            continue
        assert target.startswith("/proc/") and target.endswith("/fd"), observation
    assert any(fd == int(work_fd) and target.startswith("/tmp/opencode/.work-") for fd, target in observation["fds"])


def assert_retained_work(before: set[Path]) -> None:
    retained = [path for path in work_dirs(TRUSTED_ROOT) if path not in before]
    assert retained
    assert all(list(work.glob("evidence-*")) for work in retained)


def test_cli_contract(directory: Path) -> None:
    old = make_old(directory / "old", fail=True)
    for command in (
        generator("--mode", "generate", "--project", str(ROOT), "--old-binary", str(old), "--decoder", str(DECODER), "--unexpected-destination", "ignored"),
        generator("--mode", "differential", "--project", str(ROOT), "--old-binary", str(old), "--decoder", str(DECODER), "--unexpected-destination", "ignored"),
        wrapper("--generate", "--old-binary", str(old), "--unexpected-destination", "ignored"),
        wrapper("--differential", "--old-binary", str(old), "--unexpected-destination", "ignored"),
    ):
        assert_rejected(command, status=2)

    for mode, args in (("generate", ("--generate",)), ("differential", ("--differential",))):
        before = set(work_dirs(TRUSTED_ROOT))
        result = run(wrapper(*args, "--old-binary", str(old)), capture_output=True, text=True)
        assert result.returncode != 0
        assert not result.stdout
        assert f"srep-legacy-{mode}." in result.stderr
        assert "generation failed; retained output:" not in result.stderr
        assert "retained evidence:" not in result.stderr
        assert "held output dev=" in result.stderr
        assert "retained pathname untrusted" in result.stderr
        assert_retained_work(before)


def test_immediate_output_identity_hook(directory: Path) -> None:
    old = make_old(directory / "old", fail=True)
    hook = make_hook(directory / "hook")
    target = directory / "replacement"
    retained = directory / "retained"
    env = os.environ.copy()
    env["SREP_LEGACY_OUTPUT_MKDIR_HOOK"] = str(hook)
    env["SREP_HOOK_RETAINED"] = str(retained)
    env["SREP_HOOK_REPLACEMENT"] = str(target)
    result = run(wrapper("--generate", "--old-binary", str(old)), capture_output=True, text=True, env=env)
    assert result.returncode != 0
    assert "held output dev=" in result.stderr
    assert "retained pathname untrusted" in result.stderr
    assert "output: " not in result.stdout + result.stderr
    assert retained.is_dir() and not list(retained.iterdir())
    assert target.is_dir() and not list(target.iterdir())


def test_immediate_work_and_evidence_identity_hooks(directory: Path) -> None:
    for action, hook_name, diagnostic in (
        ("work", "SREP_LEGACY_WORK_MKDIR_HOOK", "held work dev="),
        ("evidence", "SREP_LEGACY_EVIDENCE_MKDIR_HOOK", "held evidence dev="),
    ):
        old = make_old(directory / f"old-{action}", fail=True)
        hook = make_hook(directory / f"hook-{action}")
        target = directory / f"replacement-{action}"
        retained = directory / f"retained-{action}"
        before_outputs = set(output_dirs())
        env = os.environ.copy()
        env[hook_name] = str(hook)
        env["SREP_HOOK_RETAINED"] = str(retained)
        env["SREP_HOOK_REPLACEMENT"] = str(target)
        result = run(
            wrapper("--generate", "--old-binary", str(old)),
            capture_output=True,
            text=True,
            env=env,
        )
        assert result.returncode != 0
        assert not result.stdout
        assert diagnostic in result.stderr
        assert "held output dev=" in result.stderr
        assert "retained pathname untrusted" in result.stderr
        assert "output: " not in result.stderr
        assert retained.is_dir() and not list(retained.iterdir())
        assert target.is_dir() and not list(target.iterdir())
        output = next(iter(set(output_dirs()) - before_outputs))
        assert output.is_dir() and not list(output.iterdir())


def test_private_paths_and_retention(directory: Path, old: Path) -> None:
    before = set(work_dirs(TRUSTED_ROOT))
    before_fixture = tree_digest(FIXTURE)
    old = make_observing_old(directory / "observing-old", old)
    env = os.environ.copy()
    env.update({
        "SREP_LEGACY_" + "OUTPUT_PATH": str(directory / "controller-output"),
        "SREP_LEGACY_" + "WORK_PATH": str(directory / "controller-work"),
        "SREP_CONTROLLER_TOKEN": "controller-secret",
        "PROJECT_ROOT": str(ROOT),
    })
    result = run(
        wrapper("--generate", "--old-binary", str(old)),
        capture_output=True, text=True, env=env,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    output = parse_output(result)
    assert {path.name for path in output.iterdir()} == {path.name for path in FIXTURE.iterdir()}
    for path in output.iterdir():
        assert stat.S_ISREG(os.lstat(path).st_mode), path
    validated = run(validator_command(output, "--acceptance", "--decoder", str(DECODER)), capture_output=True, text=True)
    assert validated.returncode == 0, validated.stdout + validated.stderr
    assert tree_digest(FIXTURE) == before_fixture
    assert_retained_work(before)
    work = next(iter(set(work_dirs(TRUSTED_ROOT)) - before))
    assert_private_observation(private_observation(work))


def test_hardcoded_final_mutation_is_rejected(directory: Path) -> None:
    before_outputs = set(output_dirs())
    before_work = set(work_dirs(TRUSTED_ROOT))
    before_fixture = tree_digest(FIXTURE)
    state = directory / "mutation-count"
    fixture_old = make_fixture_old(directory / "fixture-old")
    old = make_mutating_old(directory / "mutating-old", state, fixture_old)
    hook = make_output_binding_hook(old)
    env = os.environ.copy()
    env["SREP_LEGACY_OUTPUT_MKDIR_HOOK"] = str(hook)
    result = run(
        generator(
            "--mode", "differential", "--project", str(ROOT),
            "--old-binary", str(old), "--decoder", str(DECODER),
        ),
        capture_output=True, text=True, env=env,
    )
    assert result.returncode != 0, result.stdout + result.stderr
    assert not result.stdout
    assert "output: " not in result.stderr
    output = next(iter(set(output_dirs()) - before_outputs))
    work = next(iter(set(work_dirs(TRUSTED_ROOT)) - before_work))
    assert state.read_text() == "24"
    assert output.is_dir() and work.is_dir()
    rejected = run(
        validator_command(output, "--acceptance", "--decoder", str(DECODER)),
        capture_output=True, text=True,
    )
    assert rejected.returncode != 0
    assert tree_digest(FIXTURE) == before_fixture


def validator_command(root: Path, *args: str) -> list[str]:
    return ["python3", str(VALIDATOR), "--root", str(root), *args]


def assert_validator_rejects_variant(fixture_copy: Path, name: str, kind: str) -> None:
    path = fixture_copy / name
    path.unlink()
    if kind == "symlink":
        path.symlink_to("original.bin")
    elif kind == "dir":
        path.mkdir()
    elif kind == "fifo":
        os.mkfifo(path)
    elif kind == "socket":
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(path))
        try:
            result = run(validator_command(fixture_copy), capture_output=True, text=True)
        finally:
            listener.close()
        assert result.returncode != 0, kind
        return
    else:
        raise AssertionError(kind)
    result = run(validator_command(fixture_copy), capture_output=True, text=True)
    assert result.returncode != 0, kind


def test_validator_exact_entries(directory: Path) -> None:
    for extra_kind in ("symlink", "dir", "fifo", "socket"):
        copy = directory / f"validator-extra-{extra_kind}"
        shutil.copytree(FIXTURE, copy)
        extra = copy / f"extra-{extra_kind}"
        if extra_kind == "symlink":
            extra.symlink_to("original.bin")
        elif extra_kind == "dir":
            extra.mkdir()
        elif extra_kind == "fifo":
            os.mkfifo(extra)
        else:
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.bind(str(extra))
            listener.close()
        result = run(validator_command(copy), capture_output=True, text=True)
        assert result.returncode != 0, extra_kind
    for name in ("original.bin", "v1-md5.srep"):
        for kind in ("symlink", "dir", "fifo", "socket"):
            copy = directory / f"validator-type-{name}-{kind}"
            shutil.copytree(FIXTURE, copy)
            assert_validator_rejects_variant(copy, name, kind)


def test_fixture_digest_and_actual_generation(directory: Path, old: Path) -> None:
    before = tree_digest(FIXTURE)
    result = run(wrapper("--differential", "--old-binary", str(old)), capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr
    differential = parse_output(result)
    evidence = Path(next(line.split(": ", 1)[1] for line in result.stdout.splitlines() if line.startswith("evidence: ")))
    assert evidence.is_dir()
    assert differential.parent == TRUSTED_ROOT
    assert WORK_RE.fullmatch(evidence.parent.name), evidence
    assert EVIDENCE_RE.fullmatch(evidence.name), evidence
    assert evidence.parent.parent == TRUSTED_ROOT
    assert not evidence.is_relative_to(differential)
    assert len(list(differential.glob("v[1-4]-*.srep"))) == 24
    assert list(evidence.glob("*.out"))
    assert not list(differential.glob("*.out"))

    result = run(wrapper("--generate", "--old-binary", str(old)), capture_output=True, text=True)
    assert result.returncode == 0, result.stdout + result.stderr
    generated = parse_output(result)
    expected = {path.name for path in FIXTURE.iterdir()}
    actual = {path.name for path in generated.iterdir()}
    assert actual == expected, sorted(actual ^ expected)
    for path in generated.iterdir():
        assert stat.S_ISREG(os.lstat(path).st_mode), path
    validated = run(validator_command(generated, "--acceptance", "--decoder", str(DECODER)), capture_output=True, text=True)
    assert validated.returncode == 0, validated.stdout + validated.stderr
    assert tree_digest(FIXTURE) == before


def test_source_policy() -> None:
    source = (ROOT / "scripts" / "generate-legacy-fixtures.py").read_text()
    shell_source = (ROOT / "scripts" / "regenerate-legacy-fixtures.sh").read_text()
    test_source = Path(__file__).read_text()
    documentation = "\n".join(
        path.read_text()
        for path in (ROOT / "README.md", ROOT / "CONTRIBUTING.md", ROOT / "docs" / "FORMAT.md")
    )
    exact_output_option = re.compile(r"-{2}output(?:[=\s]|$)")
    for leaked_name in (
        "SREP_LEGACY_" + "OUTPUT_PATH", "SREP_LEGACY_" + "ROOT_PATH",
        "SREP_LEGACY_" + "WORK_PATH", "SREP_LEGACY_" + "EVIDENCE_PATH",
        "SREP_LEGACY_" + "WORK_FD",
    ):
        assert leaked_name not in source
    assert not exact_output_option.search(source)
    assert not exact_output_option.search(shell_source)
    assert not exact_output_option.search(test_source)
    assert not exact_output_option.search(documentation)
    assert "os.mkdir" in source and "dir_fd" in source
    assert "O_NOFOLLOW" in source and "O_DIRECTORY" in source
    assert "O_EXCL" in source and "O_CLOEXEC" in source
    assert "os.rename" not in source and "os.replace" not in source
    assert "shutil.rmtree" not in source and "Path.open" not in source
    assert source.count("_check_held(self.root_fd") == 1
    assert source.count("_check_held(self.output_fd") == 1
    assert source.count("_check_held(self.work_fd") == 1
    assert source.count("_check_held(self.evidence_fd") == 1
    assert "output" not in shell_source


def main() -> int:
    if not DECODER.is_file() or not os.access(DECODER, os.X_OK):
        raise SystemExit(f"decoder unavailable: {DECODER}")
    with tempfile.TemporaryDirectory(prefix="srep-stage2-tests-", dir=TRUSTED_ROOT) as name:
        directory = Path(name)
        old = OLD if OLD.is_file() and os.access(OLD, os.X_OK) else make_fixture_old(directory / "fixture-old")
        test_source_policy()
        test_cli_contract(directory)
        test_immediate_output_identity_hook(directory)
        test_immediate_work_and_evidence_identity_hooks(directory)
        test_private_paths_and_retention(directory, old)
        test_hardcoded_final_mutation_is_rejected(directory)
        print("test_hardcoded_final_mutation_is_rejected: passed")
        test_validator_exact_entries(directory)
        test_fixture_digest_and_actual_generation(directory, old)
    print("Stage2 legacy generator blocker tests passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Generate a retained legacy fixture corpus under the trusted temp root."""
from __future__ import annotations

import argparse
import os
import secrets
import stat
import subprocess
import sys
from pathlib import Path


TRUSTED_ROOT = "/tmp/opencode"
O_CLOEXEC = getattr(os, "O_CLOEXEC", 0)
O_DIRECTORY = getattr(os, "O_DIRECTORY", 0)
O_NOFOLLOW = getattr(os, "O_NOFOLLOW", 0)
READ_DIR_FLAGS = os.O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW
EXCLUSIVE_FLAGS = os.O_WRONLY | os.O_CREAT | os.O_EXCL | O_NOFOLLOW | O_CLOEXEC
OUTPUT_PREFIXES = {"generate": "srep-legacy-generate.", "differential": "srep-legacy-differential."}

ARCHIVE_OPTIONS = {1: "-m3o", 2: "-m4o", 3: "-m4f", 4: "-m4"}
CHECKSUM_OPTIONS = {
    "md5": "-hash=md5",
    "none": "-hash-",
    "sha1": "-hash=sha1",
    "sha512": "-hash=sha512",
    "vmac": "-hash=vmac",
    "siphash": "-hash=siphash",
}


class AnchorChanged(RuntimeError):
    """A held directory no longer has its original pathname identity."""


def identity(fd: int) -> tuple[int, int]:
    details = os.fstat(fd)
    return details.st_dev, details.st_ino


def lstat_at(directory_fd: int, name: str) -> os.stat_result:
    return os.stat(name, dir_fd=directory_fd, follow_symlinks=False)


def assert_directory(details: os.stat_result, message: str) -> None:
    if not stat.S_ISDIR(details.st_mode):
        raise AnchorChanged(message)


def invoke_hook(name: str, path: str) -> None:
    hook = os.environ.get(name)
    if hook:
        subprocess.run([hook, path], check=True, shell=False)


def open_trusted_root() -> int:
    expected = os.lstat(TRUSTED_ROOT)
    if not stat.S_ISDIR(expected.st_mode) or stat.S_ISLNK(expected.st_mode):
        raise ValueError("trusted output root must be a physical directory")
    root_fd = os.open(TRUSTED_ROOT, READ_DIR_FLAGS)
    if identity(root_fd) != (expected.st_dev, expected.st_ino):
        os.close(root_fd)
        raise ValueError("trusted output root changed during opening")
    return root_fd


def create_held_directory(
    parent_fd: int, name: str, hook_names: tuple[str, ...], path: str,
    on_created=None,
) -> tuple[int, tuple[int, int]]:
    os.mkdir(name, 0o700, dir_fd=parent_fd)
    expected = lstat_at(parent_fd, name)
    assert_directory(expected, f"created path is not a directory: {path}")
    if on_created is not None:
        on_created((expected.st_dev, expected.st_ino))
    for hook_name in hook_names:
        invoke_hook(hook_name, path)
    try:
        held_fd = os.open(name, READ_DIR_FLAGS, dir_fd=parent_fd)
    except OSError as exc:
        raise AnchorChanged from exc
    held_identity = (expected.st_dev, expected.st_ino)
    try:
        if identity(held_fd) != held_identity:
            raise AnchorChanged
    except BaseException:
        os.close(held_fd)
        raise
    return held_fd, held_identity


class OutputAnchor:
    def __init__(self, root_fd: int, mode: str):
        self.root_fd = root_fd
        self._root_identity = identity(root_fd)
        self.fds: list[int] = [root_fd]
        self.output_fd: int | None = None
        self.work_fd: int | None = None
        self.evidence_fd: int | None = None
        self.output_identity: tuple[int, int] | None = None
        self.work_identity: tuple[int, int] | None = None
        self.evidence_identity: tuple[int, int] | None = None
        self.output_name = ""
        self.output_token = ""
        self.work_name: str | None = None
        self.evidence_name: str | None = None
        self.mode = mode
        self.final_validated = False

    @property
    def root_identity(self) -> tuple[int, int]:
        return self._root_identity

    @property
    def output_path(self) -> str:
        return os.path.join(TRUSTED_ROOT, self.output_name)

    @property
    def output_proc_path(self) -> str:
        assert self.output_fd is not None
        return f"/proc/self/fd/{self.output_fd}"

    @property
    def work_path(self) -> str | None:
        if self.work_name is None:
            return None
        return os.path.join(TRUSTED_ROOT, self.work_name)

    @property
    def evidence_path(self) -> str | None:
        if self.work_name is None or self.evidence_name is None:
            return None
        return os.path.join(TRUSTED_ROOT, self.work_name, self.evidence_name)

    def _create_output(self) -> None:
        prefix = OUTPUT_PREFIXES[self.mode]
        while True:
            self.output_token = secrets.token_hex(16)
            self.output_name = prefix + self.output_token
            try:
                os.mkdir(self.output_name, 0o700, dir_fd=self.root_fd)
                break
            except FileExistsError:
                continue
        expected = lstat_at(self.root_fd, self.output_name)
        assert_directory(expected, "new output is not a directory")
        self.output_identity = (expected.st_dev, expected.st_ino)
        invoke_hook("SREP_LEGACY_OUTPUT_MKDIR_HOOK", self.output_path)
        invoke_hook("SREP_LEGACY_AFTER_OUTPUT_MKDIR", self.output_path)
        try:
            self.output_fd = os.open(self.output_name, READ_DIR_FLAGS, dir_fd=self.root_fd)
        except OSError as exc:
            raise AnchorChanged from exc
        self.fds.append(self.output_fd)
        if identity(self.output_fd) != self.output_identity:
            raise AnchorChanged

    def create(self) -> None:
        self._create_output()

    def retain_generation(
        self, work_fd: int, work_name: str, work_identity: tuple[int, int],
        evidence_fd: int, evidence_name: str, evidence_identity: tuple[int, int],
    ) -> None:
        self.work_fd = work_fd
        self.work_name = work_name
        self.work_identity = work_identity
        self.evidence_fd = evidence_fd
        self.evidence_name = evidence_name
        self.evidence_identity = evidence_identity

    def _check_held(self, fd: int | None, expected: tuple[int, int] | None, kind: str) -> None:
        if fd is None or expected is None:
            raise AnchorChanged(f"held {kind} descriptor is unavailable")
        try:
            actual = identity(fd)
        except OSError as exc:
            raise AnchorChanged(f"held {kind} descriptor cannot be inspected") from exc
        if actual != expected:
            raise AnchorChanged(f"held {kind} descriptor identity changed")

    def check(self) -> None:
        try:
            root_details = os.lstat(TRUSTED_ROOT)
        except OSError as exc:
            raise AnchorChanged from exc
        if stat.S_ISLNK(root_details.st_mode) or not stat.S_ISDIR(root_details.st_mode):
            raise AnchorChanged
        if (root_details.st_dev, root_details.st_ino) != self.root_identity:
            raise AnchorChanged
        self._check_held(self.root_fd, self.root_identity, "root")
        self._check_held(self.output_fd, self.output_identity, "output")
        try:
            output_details = lstat_at(self.root_fd, self.output_name)
        except OSError as exc:
            raise AnchorChanged from exc
        if (output_details.st_dev, output_details.st_ino) != self.output_identity:
            raise AnchorChanged
        try:
            output_check = os.open(self.output_name, READ_DIR_FLAGS, dir_fd=self.root_fd)
        except OSError as exc:
            raise AnchorChanged from exc
        try:
            if identity(output_check) != self.output_identity:
                raise AnchorChanged
        finally:
            os.close(output_check)
        if not self.output_token or self.output_name != OUTPUT_PREFIXES[self.mode] + self.output_token:
            raise AnchorChanged
        if self.work_fd is not None and self.work_name is not None and self.work_identity is not None:
            self._check_held(self.work_fd, self.work_identity, "work")
            try:
                work_details = lstat_at(self.root_fd, self.work_name)
            except OSError as exc:
                raise AnchorChanged from exc
            if (work_details.st_dev, work_details.st_ino) != self.work_identity:
                raise AnchorChanged
            try:
                work_check = os.open(self.work_name, READ_DIR_FLAGS, dir_fd=self.root_fd)
            except OSError as exc:
                raise AnchorChanged from exc
            try:
                if identity(work_check) != self.work_identity:
                    raise AnchorChanged
            finally:
                os.close(work_check)
        if (
            self.evidence_fd is not None
            and self.evidence_name is not None
            and self.evidence_identity is not None
            and self.work_fd is not None
        ):
            self._check_held(self.evidence_fd, self.evidence_identity, "evidence")
            try:
                evidence_details = lstat_at(self.work_fd, self.evidence_name)
            except OSError as exc:
                raise AnchorChanged from exc
            if (evidence_details.st_dev, evidence_details.st_ino) != self.evidence_identity:
                raise AnchorChanged
            try:
                evidence_check = os.open(self.evidence_name, READ_DIR_FLAGS, dir_fd=self.work_fd)
            except OSError as exc:
                raise AnchorChanged from exc
            try:
                if identity(evidence_check) != self.evidence_identity:
                    raise AnchorChanged
            finally:
                os.close(evidence_check)

    def diagnostics(self) -> list[str]:
        values = [("root", self.root_identity, TRUSTED_ROOT)]
        if self.output_identity is not None:
            values.append(("output", self.output_identity, self.output_path))
        if self.work_identity is not None and self.work_path is not None:
            values.append(("work", self.work_identity, self.work_path))
        if self.evidence_identity is not None and self.evidence_path is not None:
            values.append(("evidence", self.evidence_identity, self.evidence_path))
        return [
            f"held {kind} dev={dev} ino={ino}; retained pathname untrusted: {path}"
            for kind, (dev, ino), path in values
        ]

    def close(self) -> None:
        for fd in reversed(self.fds):
            try:
                os.close(fd)
            except OSError:
                pass
        self.fds.clear()


def open_exclusive(directory_fd: int, name: str, mode: int = 0o600) -> int:
    return os.open(name, EXCLUSIVE_FLAGS, mode, dir_fd=directory_fd)


def write_all(fd: int, data: bytes) -> None:
    position = 0
    while position < len(data):
        position += os.write(fd, data[position:])


def copy_fd(source_fd: int, destination_dir_fd: int, destination_name: str) -> None:
    destination_fd = open_exclusive(destination_dir_fd, destination_name)
    try:
        os.lseek(source_fd, 0, os.SEEK_SET)
        while True:
            data = os.read(source_fd, 1024 * 1024)
            if not data:
                break
            write_all(destination_fd, data)
    finally:
        os.close(destination_fd)


def copy_private_file(work_fd: int, source_name: str, destination_dir_fd: int, destination_name: str) -> None:
    source_fd = os.open(source_name, os.O_RDONLY | O_CLOEXEC | O_NOFOLLOW, dir_fd=work_fd)
    try:
        copy_fd(source_fd, destination_dir_fd, destination_name)
    finally:
        os.close(source_fd)


def copy_path(source: Path, destination_dir_fd: int, destination_name: str) -> None:
    source_fd = os.open(os.fspath(source), os.O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
    try:
        details = os.fstat(source_fd)
        if not stat.S_ISREG(details.st_mode):
            raise ValueError(f"source is not a regular file: {source}")
        copy_fd(source_fd, destination_dir_fd, destination_name)
    finally:
        os.close(source_fd)


def read_fd(fd: int) -> bytes:
    os.lseek(fd, 0, os.SEEK_SET)
    chunks: list[bytes] = []
    while chunk := os.read(fd, 1024 * 1024):
        chunks.append(chunk)
    return b"".join(chunks)


def random_name(prefix: str, suffix: str = "") -> str:
    return f"{prefix}{secrets.token_hex(16)}{suffix}"


def private_file_path(work_fd: int, name: str) -> str:
    return f"/proc/self/fd/{work_fd}/{name}"


def make_work(output: OutputAnchor) -> tuple[int, str, tuple[int, int], int, str, tuple[int, int]]:
    work_name = random_name(".work-")

    def record_work(work_identity: tuple[int, int]) -> None:
        output.work_name = work_name
        output.work_identity = work_identity

    work_fd, work_identity = create_held_directory(
        output.root_fd, work_name,
        ("SREP_LEGACY_WORK_MKDIR_HOOK", "SREP_LEGACY_AFTER_WORK_MKDIR"),
        os.path.join(TRUSTED_ROOT, work_name),
        record_work,
    )
    output.work_fd = work_fd
    output.fds.append(work_fd)
    evidence_name = random_name("evidence-")
    def record_evidence(evidence_identity: tuple[int, int]) -> None:
        output.evidence_name = evidence_name
        output.evidence_identity = evidence_identity

    evidence_fd, evidence_identity = create_held_directory(
        work_fd, evidence_name,
        ("SREP_LEGACY_EVIDENCE_MKDIR_HOOK", "SREP_LEGACY_AFTER_EVIDENCE_MKDIR"),
        os.path.join(TRUSTED_ROOT, work_name, evidence_name),
        record_evidence,
    )
    output.evidence_fd = evidence_fd
    output.fds.append(evidence_fd)
    return work_fd, work_name, work_identity, evidence_fd, evidence_name, evidence_identity


def child_environment() -> dict[str, str]:
    allowed = {
        "PATH", "SYSTEMROOT", "WINDIR", "TEMP", "TMP", "TMPDIR",
        "LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "LC_MESSAGES",
    }
    return {name: value for name, value in os.environ.items() if name in allowed}


def run_untrusted(
    command: list[str], *, work_fd: int, stdout, stderr,
) -> subprocess.CompletedProcess:
    return subprocess.run(
        command, check=False, shell=False, stdout=stdout, stderr=stderr,
        pass_fds=(work_fd,), text=False, env=child_environment(), cwd="/",
    )


def run_trusted(command: list[str], *, output_fd: int) -> subprocess.CompletedProcess:
    return subprocess.run(
        command, check=True, shell=False, stdout=subprocess.DEVNULL,
        pass_fds=(output_fd,), text=False,
    )


def run_old(old: Path, output: OutputAnchor, work_fd: int, original_name: str, archive_name: str, version: int, checksum: str) -> tuple[int, bytes]:
    command = [
        os.fspath(old), "-v0", "-b8k", "-l16", "-c16", ARCHIVE_OPTIONS[version],
        CHECKSUM_OPTIONS[checksum], private_file_path(work_fd, original_name),
        private_file_path(work_fd, archive_name),
    ]
    result = run_untrusted(command, work_fd=work_fd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    return result.returncode, result.stderr


def verify_private_regular(work_fd: int, name: str) -> int:
    fd = os.open(name, os.O_RDONLY | O_CLOEXEC | O_NOFOLLOW, dir_fd=work_fd)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError(f"private output is not a regular file: {name}")
    return fd


def validate(
    output: OutputAnchor, project: Path, decoder: Path, *, generate_manifest: bool,
    acceptance: bool,
) -> None:
    command = [
        sys.executable, os.fspath(project / "scripts" / "validate-legacy-fixtures.py"),
        "--root", output.output_proc_path, "--root-fd", str(output.output_fd),
    ]
    if acceptance:
        command.extend(("--acceptance", "--decoder", os.fspath(decoder)))
    if generate_manifest:
        command.append("--generate-manifest")
    assert output.output_fd is not None
    run_trusted(command, output_fd=output.output_fd)


def run_differential(output: OutputAnchor, work_fd: int, evidence_fd: int, old: Path, decoder: Path, original_name: str) -> None:
    assert output.output_fd is not None
    original_fd = os.open("original.bin", os.O_RDONLY | O_CLOEXEC | O_NOFOLLOW, dir_fd=output.output_fd)
    try:
        original = read_fd(original_fd)
        for entry in sorted(os.listdir(output.output_fd)):
            if not (entry.startswith("v") and entry.endswith(".srep")):
                continue
            archive_copy = random_name("input-", ".srep")
            archive_fd = os.open(entry, os.O_RDONLY | O_CLOEXEC | O_NOFOLLOW, dir_fd=output.output_fd)
            try:
                copy_fd(archive_fd, work_fd, archive_copy)
            finally:
                os.close(archive_fd)
            old_name = random_name("old-", ".out")
            old_result = run_untrusted(
                [os.fspath(old), "-d", private_file_path(work_fd, archive_copy), private_file_path(work_fd, old_name)],
                work_fd=work_fd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            )
            output.check()
            rust_result = run_untrusted(
                [os.fspath(decoder), "decompress", private_file_path(work_fd, archive_copy), "-"],
                work_fd=work_fd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            )
            output.check()
            old_fd = verify_private_regular(work_fd, old_name)
            try:
                old_data = read_fd(old_fd)
                rust_data = rust_result.stdout
                copy_fd(old_fd, evidence_fd, old_name)
            finally:
                os.close(old_fd)
            rust_name = random_name("rust-", ".out")
            rust_fd = open_exclusive(evidence_fd, rust_name)
            try:
                write_all(rust_fd, rust_data)
            finally:
                os.close(rust_fd)
            output.check()
            if old_result.returncode or rust_result.returncode or old_data != rust_data or old_data != original:
                raise RuntimeError(f"differential mismatch: {entry}")
    finally:
        os.close(original_fd)


def expected_fixture_entries(fixture: Path) -> set[str]:
    names = set(os.listdir(fixture))
    for name in names:
        details = os.lstat(fixture / name)
        if not stat.S_ISREG(details.st_mode):
            raise AssertionError(f"fixture entry is not a regular file: {name}")
    return names


def assert_exact_corpus(output: OutputAnchor, fixture: Path) -> None:
    assert output.output_fd is not None
    actual = set(os.listdir(output.output_fd))
    expected = expected_fixture_entries(fixture)
    for name in actual | expected:
        details = os.stat(name, dir_fd=output.output_fd, follow_symlinks=False)
        if not stat.S_ISREG(details.st_mode):
            raise AssertionError(f"corpus entry is not a regular file: {name}")
    if actual != expected:
        raise AssertionError(f"unexpected corpus files: {sorted(actual ^ expected)}")


def assert_evidence(output: OutputAnchor, original: bytes) -> None:
    if output.evidence_fd is None:
        raise AssertionError("differential evidence directory is unavailable")
    names = os.listdir(output.evidence_fd)
    if len(names) != 48:
        raise AssertionError(f"unexpected evidence count: {len(names)}")
    old_names = [name for name in names if name.startswith("old-") and name.endswith(".out")]
    rust_names = [name for name in names if name.startswith("rust-") and name.endswith(".out")]
    if len(old_names) != 24 or len(rust_names) != 24 or len(set(names)) != 48:
        raise AssertionError("unexpected evidence entry names")
    for name in names:
        details = os.stat(name, dir_fd=output.evidence_fd, follow_symlinks=False)
        if not stat.S_ISREG(details.st_mode):
            raise AssertionError(f"evidence entry is not a regular file: {name}")
        fd = os.open(name, os.O_RDONLY | O_CLOEXEC | O_NOFOLLOW, dir_fd=output.evidence_fd)
        try:
            if read_fd(fd) != original:
                raise AssertionError(f"evidence contents differ from original: {name}")
        finally:
            os.close(fd)


def generate(output: OutputAnchor, project: Path, old: Path, decoder: Path, differential: bool) -> str | None:
    fixture = project / "tests" / "fixtures" / "legacy"
    work_fd, work_name, work_identity, evidence_fd, evidence_name, evidence_identity = make_work(output)
    output.retain_generation(work_fd, work_name, work_identity, evidence_fd, evidence_name, evidence_identity)
    output.check()
    try:
        original_name = random_name("original-")
        original_fd = open_exclusive(work_fd, original_name)
        try:
            write_all(original_fd, (b"0123456789abcdef" * 64) * 32)
        finally:
            os.close(original_fd)
        output.check()
        assert output.output_fd is not None
        copy_private_file(work_fd, original_name, output.output_fd, "original.bin")
        for version in range(1, 5):
            for checksum in CHECKSUM_OPTIONS:
                archive_name = random_name("archive-")
                returncode, stderr = run_old(old, output, work_fd, original_name, archive_name, version, checksum)
                output.check()
                archive_fd = verify_private_regular(work_fd, archive_name)
                try:
                    copy_fd(archive_fd, output.output_fd, f"v{version}-{checksum}.srep")
                finally:
                    os.close(archive_fd)
                output.check()
                if returncode:
                    raise RuntimeError(f"old binary failed for v{version}-{checksum}: {stderr.decode(errors='replace').strip()}")
        for source in sorted(fixture.iterdir()):
            if source.is_file() and (source.name.startswith("historical-") or source.name.startswith("special-") or source.name == "corruptions.json"):
                output.check()
                copy_path(source, output.output_fd, source.name)
        output.check()
        validate(output, project, decoder, generate_manifest=True, acceptance=False)
        output.check()
        if differential:
            run_differential(output, work_fd, evidence_fd, old, decoder, original_name)
        # No untrusted child is launched after this point.  Keep the manifest
        # produced above fixed and validate the complete retained state again.
        output.check()
        validate(output, project, decoder, generate_manifest=False, acceptance=False)
        output.check()
        validate(output, project, decoder, generate_manifest=False, acceptance=True)
        output.check()
        assert_exact_corpus(output, fixture)
        output.check()
        if differential:
            assert_evidence(output, (b"0123456789abcdef" * 64) * 32)
            output.check()
        output.final_validated = True
        return output.evidence_path if differential else None
    finally:
        # Descriptors are closed by OutputAnchor; pathname resources remain retained.
        pass


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mode", choices=("generate", "differential"), required=True)
    parser.add_argument("--project", required=True)
    parser.add_argument("--old-binary", required=True)
    parser.add_argument("--decoder", required=True)
    args = parser.parse_args()
    for name in ("project", "old_binary", "decoder"):
        if not os.path.isabs(getattr(args, name)):
            parser.error(f"{name.replace('_', '-')} must be absolute")
    if not os.access(args.old_binary, os.X_OK):
        parser.error("old binary must be executable")
    return args


def main() -> int:
    args = parse_args()
    root_fd = open_trusted_root()
    output: OutputAnchor | None = None
    try:
        output = OutputAnchor(root_fd, args.mode)
        output.create()
        evidence = generate(output, Path(args.project), Path(args.old_binary), Path(args.decoder), args.mode == "differential")
        output.check()
        print(f"output: {output.output_path}")
        if evidence:
            print(f"evidence: {evidence}")
        return 0
    except AnchorChanged:
        if output is None:
            print("output anchors changed", file=sys.stderr)
        else:
            print("output anchors changed", file=sys.stderr)
            print("\n".join(output.diagnostics()), file=sys.stderr)
        return 1
    except BaseException as exc:
        if output is None:
            print(f"generation failed; retained output unavailable: {exc}", file=sys.stderr)
            return 1
        if not output.final_validated:
            print(f"generation failed before final corpus validation: {exc}", file=sys.stderr)
            print("\n".join(output.diagnostics()), file=sys.stderr)
            return 1
        try:
            output.check()
        except AnchorChanged:
            print("output anchors changed", file=sys.stderr)
            print("\n".join(output.diagnostics()), file=sys.stderr)
            return 1
        print(f"generation failed; retained output: {output.output_path}", file=sys.stderr)
        if output.evidence_path is not None:
            print(f"retained evidence: {output.evidence_path}", file=sys.stderr)
        print(str(exc), file=sys.stderr)
        return 1
    finally:
        if output is not None:
            output.close()
        else:
            os.close(root_fd)


if __name__ == "__main__":
    raise SystemExit(main())

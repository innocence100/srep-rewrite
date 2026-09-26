#!/usr/bin/env python3
"""Portable packager tests; native builds run only for the current host target."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SOURCE_SCRIPT = ROOT / "scripts" / "package-release.py"
HOST = subprocess.check_output(["rustc", "-vV"], text=True)
HOST_TARGET = next(line.split(": ", 1)[1] for line in HOST.splitlines() if line.startswith("host: "))
TARGET = HOST_TARGET if HOST_TARGET in {"x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc", "aarch64-apple-darwin"} else "x86_64-unknown-linux-gnu"


def run(args: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    return subprocess.run(args, text=True, capture_output=True, check=False, **kwargs)


def make_repo(root: Path, *, version: str = "0.1.0", reported_version: str = "0.1.0") -> Path:
    repo = root / "source"
    (repo / "src").mkdir(parents=True)
    for name in ("LICENSE", "README.md", "CHANGELOG.md", "THIRD_PARTY.md", "THIRD_PARTY.audit"):
        (repo / name).write_text(f"{name}\n", encoding="utf-8")
    (repo / "Cargo.toml").write_text(
        f'[package]\nname = "srep"\nversion = "{version}"\nedition = "2021"\n\n[[bin]]\nname = "srep"\npath = "src/main.rs"\n', encoding="utf-8")
    (repo / "src/main.rs").write_text(f'''use std::env;
use std::fs;
fn main() {{
    let args: Vec<String> = env::args().skip(1).collect();
    if env::var_os("SREP_TEST_CHILD_FAILURE").is_some() && args.first().map(String::as_str) == Some("--help") {{ std::process::exit(17); }}
    match args.first().map(String::as_str) {{
        Some("--version") => println!("srep {reported_version}"),
        Some("--help") => println!("srep test fixture help"),
        Some("compress") | Some("decompress") => {{
            if args.len() < 3 {{ std::process::exit(2); }}
            let src = &args[args.len() - 2]; let dst = &args[args.len() - 1];
            if let Err(_) = fs::copy(src, dst) {{ std::process::exit(3); }}
        }},
        Some("info") | Some("test") => {{}},
        _ => std::process::exit(2),
    }}
}}
''', encoding="utf-8")
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.name", "test"], cwd=repo, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=repo, check=True)
    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "fixture"], cwd=repo, check=True)
    subprocess.run(["cargo", "generate-lockfile"], cwd=repo, check=True, capture_output=True)
    subprocess.run(["git", "add", "Cargo.lock"], cwd=repo, check=True)
    subprocess.run(["git", "commit", "-qm", "lock"], cwd=repo, check=True)
    return repo


def make_tools(root: Path) -> tuple[Path, str]:
    tools = root / "tools"
    (tools / "scripts").mkdir(parents=True)
    shutil.copy2(SOURCE_SCRIPT, tools / "scripts/package-release.py")
    subprocess.run(["git", "init", "-q"], cwd=tools, check=True)
    subprocess.run(["git", "config", "user.name", "test"], cwd=tools, check=True)
    subprocess.run(["git", "config", "user.email", "test@example.invalid"], cwd=tools, check=True)
    subprocess.run(["git", "add", "."], cwd=tools, check=True)
    subprocess.run(["git", "commit", "-qm", "tooling"], cwd=tools, check=True)
    return tools, subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=tools, text=True).strip()


def package(tools_root: Path, repo: Path, tools_sha: str, out: Path, *extra: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    command = [sys.executable, str(tools_root / "tools/scripts/package-release.py"), "--repo", str(repo), "--build",
               "--target", TARGET, "--out-dir", str(out), "--source-sha",
               subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
               "--tooling-sha", tools_sha, "--skip-license-harvest", *extra]
    merged = os.environ.copy(); merged.update(env or {})
    return run(command, env=merged)


def test_native_build_archive_extract_smoke() -> None:
    with tempfile.TemporaryDirectory(prefix="srep-package-build-") as raw:
        root = Path(raw); repo = make_repo(root); _, tools_sha = make_tools(root); out = root / "out"
        result = package(root, repo, tools_sha, out, "--unpack-smoke")
        assert result.returncode == 0, result.stdout + result.stderr
        artifact = out / f"srep-v0.1.0-{TARGET}{'.zip' if 'windows' in TARGET else '.tar.gz'}"
        assert artifact.is_file()
        report = json.loads((out / f"srep-v0.1.0-{TARGET}-smoke.json").read_text())
        assert len(report["steps"]) >= 10 and all(step["exit_code"] == 0 for step in report["steps"])
        assert report["extracted_binary_sha256"] == report["binary_sha256"]
        with (tarfile.open(artifact) if artifact.name.endswith(".tar.gz") else _zip(artifact)) as archive:
            names = archive.getnames() if hasattr(archive, "getnames") else archive.namelist()
            assert any(name.endswith("BUILD-PROVENANCE.json") for name in names)
        if artifact.name.endswith(".tar.gz"):
            with tarfile.open(artifact) as archive:
                name = next(name for name in archive.getnames() if name.endswith("BUILD-PROVENANCE.json"))
                provenance = json.loads(archive.extractfile(name).read())
        else:
            with _zip(artifact) as archive:
                name = next(name for name in archive.namelist() if name.endswith("BUILD-PROVENANCE.json"))
                provenance = json.loads(archive.read(name))
        assert provenance["source_sha"] == subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
        assert provenance["tooling_sha"] == tools_sha


def _zip(path: Path):
    import zipfile
    return zipfile.ZipFile(path)


def test_source_dirty_wrong_source_tooling_and_binary_format() -> None:
    with tempfile.TemporaryDirectory(prefix="srep-package-negative-") as raw:
        root = Path(raw); repo = make_repo(root); _, tools_sha = make_tools(root); out = root / "out"
        (repo / "dirty").write_text("dirty")
        result = package(root, repo, tools_sha, out)
        assert result.returncode == 2 and "dirty" in result.stderr
        (repo / "dirty").unlink()
        result = run([sys.executable, str(root / "tools/scripts/package-release.py"), "--repo", str(repo), "--build", "--target", TARGET, "--out-dir", str(out), "--source-sha", "0" * 40, "--tooling-sha", tools_sha, "--skip-license-harvest"])
        assert result.returncode == 2 and "does not match clean HEAD" in result.stderr
        result = package(root, repo, "f" * 40, out)
        assert result.returncode == 2 and "tooling SHA" in result.stderr
        opposite = {"x86_64-unknown-linux-gnu": "x86_64-pc-windows-msvc", "x86_64-pc-windows-msvc": "x86_64-unknown-linux-gnu", "aarch64-apple-darwin": "x86_64-unknown-linux-gnu"}[TARGET]
        result = run([sys.executable, str(root / "tools/scripts/package-release.py"), "--repo", str(repo), "--build", "--target", opposite, "--out-dir", str(out), "--source-sha", subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(), "--tooling-sha", tools_sha, "--skip-license-harvest"])
        assert result.returncode == 2 and "requires rustc host" in result.stderr
        module_spec = importlib.util.spec_from_file_location("package_release", SOURCE_SCRIPT)
        module = importlib.util.module_from_spec(module_spec); assert module_spec.loader
        module_spec.loader.exec_module(module)
        fake = root / "not-elf"; fake.write_bytes(b"not an executable")
        try:
            module.validate_binary_format(fake, "x86_64-unknown-linux-gnu")
        except SystemExit as exc:
            assert exc.code == 2
        else:
            raise AssertionError("wrong binary format accepted")


def test_wrong_version_child_failure_and_archive_cleanup() -> None:
    with tempfile.TemporaryDirectory(prefix="srep-package-failure-") as raw:
        root = Path(raw); repo = make_repo(root, reported_version="9.9.9"); _, tools_sha = make_tools(root); out = root / "out"
        result = package(root, repo, tools_sha, out, "--unpack-smoke")
        assert result.returncode == 2 and "expected 'srep 0.1.0'" in result.stderr
        assert not list(out.glob("srep-v0.1.0-*tar.gz"))
        report = next(out.glob("*-smoke.json")); assert "failure" in json.loads(report.read_text())
        repo2 = make_repo(root / "child", reported_version="0.1.0"); out2 = root / "out2"
        result = package(root, repo2, tools_sha, out2, "--unpack-smoke", env={"SREP_TEST_CHILD_FAILURE": "1"})
        assert result.returncode == 2
        report = next(out2.glob("*-smoke.json")); assert '"exit_code": 17' in report.read_text()
        out2.mkdir(exist_ok=True)
        sums = out2 / "SHA256SUMS"; sums.write_text("old\n")
        result = package(root, repo2, tools_sha, out2)
        assert result.returncode == 0
        before = sums.read_text()
        result = package(root, repo2, tools_sha, out2)
        assert result.returncode == 2 and sums.read_text() == before


def test_missing_license_and_std_notice() -> None:
    module_spec = importlib.util.spec_from_file_location("package_release_license", SOURCE_SCRIPT)
    module = importlib.util.module_from_spec(module_spec); assert module_spec.loader
    module_spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="srep-package-license-") as raw:
        root = Path(raw); repo = make_repo(root); (repo / "LICENSE").unlink()
        # Required root license failure is exercised through the real CLI.
        subprocess.run(["git", "add", "LICENSE"], cwd=repo, check=True)
        subprocess.run(["git", "commit", "-qm", "remove-license"], cwd=repo, check=True)
        _, tools_sha = make_tools(root); result = package(root, repo, tools_sha, root / "out")
        assert result.returncode == 2 and "required package file missing" in result.stderr
        try:
            module.rust_std_notice({"rustc_sysroot": str(root / "missing")}, type("Args", (), {"skip_license_harvest": False})())
        except SystemExit as exc:
            assert exc.code == 2
        else:
            raise AssertionError("missing rust std notice accepted")


def test_archive_faults_leave_no_partial_outputs() -> None:
    module_spec = importlib.util.spec_from_file_location("package_release_archive", SOURCE_SCRIPT)
    module = importlib.util.module_from_spec(module_spec); assert module_spec.loader
    module_spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="srep-package-archive-") as raw:
        root = Path(raw); payload = root / "payload"; payload.mkdir(); (payload / "ok").write_text("ok")
        for fmt, suffix in (("tar", ".tar.gz"), ("zip", ".zip")):
            destination = root / f"artifact{suffix}"
            module.archive(payload, "root", destination, fmt=fmt, mtime=1)
            assert destination.is_file()
            destination.unlink()
            (payload / "broken").symlink_to(root / "missing")
            try:
                module.archive(payload, "root", destination, fmt=fmt, mtime=1)
            except OSError:
                pass
            else:
                raise AssertionError("archive fault unexpectedly succeeded")
            assert not destination.exists() and not list(root.glob(f".{destination.name}.*.tmp"))
            (payload / "broken").unlink()


def test_effective_cargo_configuration_is_rejected_before_build() -> None:
    with tempfile.TemporaryDirectory(prefix="srep-package-cargo-env-") as raw:
        root = Path(raw); repo = make_repo(root); _, tools_sha = make_tools(root); out = root / "out"
        cargo_home = root / "cargo-home"; cargo_home.mkdir(); (cargo_home / "config.toml").write_text("[build]\nrustc-wrapper = 'counter'\n")
        result = package(root, repo, tools_sha, out, env={"CARGO_HOME": str(cargo_home), "HOME": str(root / "home")})
        assert result.returncode == 2 and "config.toml" in result.stderr
        target_flags = {"CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS": "-C debuginfo=2"}
        result = package(root, repo, tools_sha, out, env=target_flags)
        assert result.returncode == 2 and "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS" in result.stderr


def test_output_lock_rejects_existing_lock_without_deleting_it() -> None:
    module_spec = importlib.util.spec_from_file_location("package_release_lock", SOURCE_SCRIPT)
    module = importlib.util.module_from_spec(module_spec); assert module_spec.loader
    module_spec.loader.exec_module(module)
    with tempfile.TemporaryDirectory(prefix="srep-package-lock-") as raw:
        directory = Path(raw); lock = directory / ".native-release.lock"; lock.write_text("owner")
        try:
            with module.OutputLock(directory):
                raise AssertionError("existing lock unexpectedly acquired")
        except SystemExit as exc:
            assert exc.code == 2
        assert lock.read_text() == "owner"


if __name__ == "__main__":
    tests = [test_native_build_archive_extract_smoke, test_source_dirty_wrong_source_tooling_and_binary_format, test_wrong_version_child_failure_and_archive_cleanup, test_missing_license_and_std_notice, test_archive_faults_leave_no_partial_outputs, test_effective_cargo_configuration_is_rejected_before_build, test_output_lock_rejects_existing_lock_without_deleting_it]
    for test in tests:
        test(); print(f"{test.__name__}: ok")
    print(f"test-package-release.py: PASS ({len(tests)} tests)")

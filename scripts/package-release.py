#!/usr/bin/env python3
"""Native-source-build, native-platform SREP release packager.

The binary is built by this script with the caller's native Cargo toolchain.
The script stages the binary and release documents, records the exact build/source
identity, writes a platform-appropriate archive, and performs an extracted
binary smoke test.  It deliberately uses Python's standard library instead of
GNU tar/readelf/shell features so the same contract is usable on Linux,
Windows, and macOS.
"""
from __future__ import annotations

import argparse
import atexit
import gzip
import hashlib
import json
import os
import platform
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import tomllib
from typing import NoReturn
import zipfile
from pathlib import Path
from collections.abc import Callable


DOCUMENTS = ("LICENSE", "README.md", "CHANGELOG.md", "THIRD_PARTY.md", "THIRD_PARTY.audit")
HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
ARCHIVE_PRELINK_HOOK: Callable[[Path], None] | None = None
SUPPORTED_TARGETS = {
    "x86_64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
    "aarch64-apple-darwin",
}


def artifact_name(version: str, target: str) -> str:
    suffix = ".zip" if target == "x86_64-pc-windows-msvc" else ".tar.gz"
    return f"srep-v{version}-{target}{suffix}"


def smoke_name(version: str, target: str) -> str:
    return f"srep-v{version}-{target}-smoke.json"


def fail(message: str) -> "NoReturn":
    print(f"package-release.py: error: {message}", file=sys.stderr)
    raise SystemExit(2)


def run(argv: list[str], *, cwd: Path | None = None, check: bool = True, env: dict[str, str] | None = None) -> str:
    try:
        result = subprocess.run(argv, cwd=cwd, env=env, check=check, text=True, capture_output=True)
    except (OSError, subprocess.CalledProcessError) as exc:
        output = getattr(exc, "stderr", "") or getattr(exc, "stdout", "") or str(exc)
        if check:
            fail(f"command failed: {' '.join(argv)}\n{output.rstrip()}")
        return output
    return result.stdout.strip()


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_metadata(repo: Path, expected: str | None) -> tuple[str, str, int]:
    try:
        commit = run(["git", "rev-parse", "HEAD"], cwd=repo)
        dirty = bool(run(["git", "status", "--porcelain", "--untracked-files=all"], cwd=repo))
        timestamp = int(run(["git", "show", "-s", "--format=%ct", "HEAD"], cwd=repo))
    except SystemExit:
        fail("release packaging requires a git checkout")
    if not HEX40.fullmatch(commit):
        fail(f"git HEAD is not a full commit id: {commit!r}")
    if dirty:
        fail("source checkout is dirty; build and package a clean source revision")
    if expected is not None:
        if not HEX40.fullmatch(expected):
            fail("--source-sha must be a 40-character lowercase commit id")
        if expected != commit:
            fail(f"source SHA {expected} does not match clean HEAD {commit}")
    return commit, "dirty" if dirty else "clean", timestamp


def package_version(repo: Path) -> str:
    try:
        with (repo / "Cargo.toml").open("rb") as manifest:
            version = tomllib.load(manifest)["package"]["version"]
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as exc:
        fail(f"cannot read package version from Cargo.toml: {exc}")
    if not isinstance(version, str) or not re.fullmatch(r"[0-9A-Za-z][0-9A-Za-z.+-]*", version):
        fail(f"invalid Cargo package version: {version!r}")
    return version


def resolve_rustc(args: argparse.Namespace) -> str:
    rustc = args.rustc or os.environ.get("RUSTC") or shutil.which("rustc")
    if not rustc:
        fail("rustc is required to record build provenance")
    # rustup's proxy does not expose rustc -vV faithfully on every release;
    # resolve it before recording the compiler or passing RUSTC to Cargo.
    try:
        selected = Path(rustc).resolve()
        if selected.name in {"rustup", "rustup-init"}:
            rustup = shutil.which("rustup") or str(selected)
            rustc = run([rustup, "which", "rustc"])
    except SystemExit:
        fail("cannot resolve the selected rustc through rustup")
    return str(Path(rustc).resolve())


def cargo_environment(repo: Path, rustc: str, target_dir: Path | None = None) -> dict[str, str]:
    cargo_home = os.environ.get("CARGO_HOME")
    if cargo_home and not Path(cargo_home).is_absolute():
        fail("CARGO_HOME must be an absolute path for a release build")
    env = os.environ.copy()
    env["RUSTC"] = rustc
    if target_dir is not None:
        env["CARGO_TARGET_DIR"] = str(target_dir)
    return env


def toolchain_metadata(args: argparse.Namespace, repo: Path, env: dict[str, str], rustc: str) -> dict[str, str]:
    verbose = run([rustc, "-vV"], cwd=repo, env=env)
    fields: dict[str, str] = {}
    for line in verbose.splitlines():
        key, separator, value = line.partition(":")
        if separator:
            fields[key.strip().lower().replace("-", "_")] = value.strip()
    sysroot = run([rustc, "--print", "sysroot"], cwd=repo, env=env)
    metadata = {
        "rustc": run([rustc, "--version"], cwd=repo, env=env),
        "rustc_commit": fields.get("commit_hash", "unknown"),
        "rustc_release": fields.get("release", "unknown"),
        "rustc_host": fields.get("host", "unknown"),
        "rustc_bin": str(Path(rustc).resolve()),
        "rustc_sysroot": str(Path(sysroot).resolve()),
    }
    if metadata["rustc_host"] == "unknown":
        fail("rustc -vV did not report a host target")
    return metadata


def tooling_metadata(expected: str) -> tuple[str, str]:
    if not HEX40.fullmatch(expected):
        fail("--tooling-sha must be a 40-character lowercase commit id")
    tooling_repo = Path(__file__).resolve().parents[1]
    try:
        commit = run(["git", "rev-parse", "HEAD"], cwd=tooling_repo)
        dirty = run(["git", "status", "--porcelain", "--untracked-files=all"], cwd=tooling_repo)
    except SystemExit:
        fail("release packaging requires a git checkout for tooling provenance")
    if commit != expected:
        fail(f"tooling SHA {expected} does not match tooling checkout HEAD {commit}")
    if dirty:
        fail("tooling checkout is dirty; use a clean tooling revision")
    return commit, "clean"


def validate_build_environment(repo: Path) -> None:
    cargo_home_value = os.environ.get("CARGO_HOME")
    if cargo_home_value and not Path(cargo_home_value).is_absolute():
        fail("CARGO_HOME must be an absolute path for a release build")
    forbidden = sorted(name for name in os.environ if name in {
        "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTC_WRAPPER",
        "RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
    })
    forbidden.extend(sorted(name for name in os.environ if name.startswith("CARGO_TARGET_") and name != "CARGO_TARGET_DIR"))
    forbidden.extend(sorted(name for name in os.environ if name.endswith("_RUSTFLAGS")))
    if forbidden:
        fail(f"build environment contains unvalidated compiler configuration: {', '.join(forbidden)}")
    config_paths: set[Path] = set()
    for ancestor in (repo, *repo.parents):
        config_paths.update(ancestor / ".cargo" / name for name in ("config", "config.toml"))
    cargo_home = Path(cargo_home_value or Path.home() / ".cargo")
    config_paths.update(cargo_home / name for name in ("config", "config.toml"))
    configured = sorted(str(path) for path in config_paths if path.is_file())
    if configured:
        fail(f"Cargo configuration is not allowed for a reproducible release build: {', '.join(configured)}")


class OutputLock:
    """Serialize artifact publication and checksum updates without deleting foreign locks."""

    def __init__(self, directory: Path) -> None:
        self.path = directory / ".native-release.lock"
        self.fd: int | None = None

    def __enter__(self) -> "OutputLock":
        try:
            self.fd = os.open(self.path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except FileExistsError:
            fail(f"release output directory is locked by another process: {self.path}")
        return self

    def __exit__(self, exc_type: object, exc: object, traceback: object) -> None:
        if self.fd is None:
            return
        os.close(self.fd)
        self.fd = None
        try:
            self.path.unlink()
        except FileNotFoundError:
            pass


def build_binary(args: argparse.Namespace, toolchain: dict[str, str], out_dir: Path, base_env: dict[str, str]) -> tuple[Path, int, str, Path]:
    """Build in a disposable target directory and return the actual binary proof."""
    if toolchain["rustc_host"] != args.target:
        fail(f"native build requires rustc host {args.target}, observed {toolchain['rustc_host']}")
    cargo = args.cargo or os.environ.get("CARGO") or shutil.which("cargo")
    if not cargo:
        fail("cargo is required for --build")
    rustc = Path(toolchain["rustc_bin"])
    validate_build_environment(args.repo)
    build_root = Path(tempfile.mkdtemp(prefix="srep-release-build-", dir=out_dir))
    atexit.register(shutil.rmtree, build_root, ignore_errors=True)
    target_dir = build_root / "target"
    env = dict(base_env)
    env["CARGO_TARGET_DIR"] = str(target_dir)
    command = [cargo, "build", "--locked", "--release", "--target", args.target, "--bin", "srep"]
    started = int(time.time())
    try:
        result = subprocess.run(command, cwd=args.repo, env=env, text=True, capture_output=True, check=False)
    except OSError as exc:
        fail(f"build command failed to start: {exc}")
    if result.returncode != 0:
        fail(f"build command failed (exit {result.returncode}):\n{result.stdout}\n{result.stderr}")
    name = "srep.exe" if "windows" in args.target else "srep"
    binary = target_dir / args.target / "release" / name
    if not binary.is_file():
        fail(f"successful build did not produce {binary}")
    return binary, started, " ".join(command), build_root


def validate_binary_format(binary: Path, target: str) -> None:
    data = binary.read_bytes()[:64]
    if target == "x86_64-unknown-linux-gnu":
        if len(data) < 20 or data[:4] != b"\x7fELF" or data[4] != 2 or data[5] != 1 or data[18:20] != b"\x3e\x00":
            fail(f"binary is not an x86_64 little-endian ELF: {binary}")
    elif target == "x86_64-pc-windows-msvc":
        if len(data) < 64 or data[:2] != b"MZ":
            fail(f"binary is not a PE executable: {binary}")
        offset = int.from_bytes(data[0x3c:0x40], "little")
        header = binary.read_bytes()[offset:offset + 6]
        if len(header) < 6 or header[:4] != b"PE\x00\x00" or header[4:6] != b"\x64\x86":
            fail(f"binary is not an x86_64 PE executable: {binary}")
    elif target == "aarch64-apple-darwin":
        if len(data) < 8 or data[:4] not in {b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf"}:
            fail(f"binary is not a Mach-O executable: {binary}")
        cputype = int.from_bytes(data[4:8], "little" if data[:4] == b"\xcf\xfa\xed\xfe" else "big")
        if cputype != 0x0100000c:
            fail(f"binary is not an arm64 Mach-O executable: {binary}")
    else:
        fail(f"unsupported native release target: {target}")


def runtime_metadata(target: str, binary: Path) -> dict[str, str]:
    info: dict[str, str] = {"target": target, "host_os": platform.platform()}
    if "linux" in target:
        info["runtime"] = "GNU libc/ld-linux compatibility is determined from observed GLIBC symbol versions."
        readelf = shutil.which("readelf")
        if readelf:
            symbols = run([readelf, "-W", "-s", str(binary)], check=False)
            versions = sorted(set(re.findall(r"GLIBC_([0-9][0-9.]*)", symbols)), key=lambda x: tuple(map(int, x.split("."))))
            info["glibc_max"] = f"GLIBC_{versions[-1]}" if versions else "none observed"
        else:
            info["glibc_max"] = "not inspected (readelf unavailable)"
    elif "windows" in target:
        info["runtime"] = "MSVC Windows runtime; native runner dependency inspection is retained in this notice."
        objdump = shutil.which("objdump")
        info["dependencies"] = "verified by objdump:\n" + "\n".join(run([objdump, "-p", str(binary)], check=False).splitlines()) if objdump else "not verified (objdump unavailable)"
    elif "darwin" in target:
        info["runtime"] = "macOS system runtime; unsigned binary is not notarized and may require Gatekeeper override."
        otool = shutil.which("otool")
        info["dependencies"] = "verified by otool:\n" + "\n".join(run([otool, "-L", str(binary)], check=False).splitlines()) if otool else "not verified (otool unavailable)"
    else:
        info["runtime"] = "target runtime compatibility was not classified by this packager"
    return info


def crate_notice(repo: Path, args: argparse.Namespace, env: dict[str, str]) -> str:
    if args.skip_license_harvest:
        return "Crate license harvest skipped by explicit test-only option.\n"
    cargo = args.cargo or os.environ.get("CARGO") or shutil.which("cargo")
    if not cargo:
        fail("cargo is required for the crate license harvest (or use the test-only skip option)")
    metadata = json.loads(run([cargo, "metadata", "--locked", "--offline", "--filter-platform", args.target, "--format-version", "1", "--manifest-path", str(repo / "Cargo.toml")], cwd=repo, env=env))
    license_names = {"LICENSE", "LICENSE.md", "LICENSE.txt", "LICENSE-MIT", "LICENSE-APACHE", "COPYRIGHT", "COPYING", "NOTICE", "AUTHORS"}
    lines = [
        "Third-party crate license and copyright texts",
        "---------------------------------------------",
        "",
        "These texts were harvested from the locked Cargo registry used by the build.",
        "",
    ]
    selected = {node.get("id") for node in metadata.get("resolve", {}).get("nodes", [])}
    packages = [package for package in metadata.get("packages", []) if package.get("id") in selected]
    for package in sorted(packages, key=lambda item: (item.get("name", ""), item.get("version", ""))):
        if not package.get("source"):
            continue
        manifest = Path(package["manifest_path"])
        lines.extend([f"### {package['name']} {package['version']}", f"SPDX: {package.get('license') or 'UNKNOWN'}", f"manifest: {manifest}", ""])
        found = [path for path in sorted(manifest.parent.iterdir()) if path.is_file() and (path.name in license_names or path.name.upper().startswith("LICENSE") or path.name.upper() in {"COPYRIGHT", "COPYING", "NOTICE", "AUTHORS"})]
        if not found:
            fail(f"crate {package['name']} {package['version']} has no redistributable license/copyright text")
        for path in found:
            lines.extend([f"---- {path.name} ----", path.read_text(encoding="utf-8", errors="replace").rstrip(), ""])
    return "\n".join(lines) + "\n"


def rust_std_notice(toolchain: dict[str, str], args: argparse.Namespace) -> str:
    if args.skip_license_harvest:
        return "Rust standard library copyright harvest skipped by explicit test-only option.\n"
    path = Path(toolchain["rustc_sysroot"]) / "share" / "doc" / "rust" / "COPYRIGHT-library.html"
    if not path.is_file():
        fail(f"validated rustc COPYRIGHT-library.html missing: {path}")
    return "\n".join([
        "Rust standard library and rustc runtime components",
        "--------------------------------------------------",
        f"source: {path}",
        "The validated build toolchain supplied this runtime notice:",
        "",
        path.read_text(encoding="utf-8", errors="replace").rstrip(),
        "",
    ]) + "\n"


def write_provenance(payload: Path, *, repo: Path, binary: Path, version: str, target: str, commit: str, state: str, toolchain: dict[str, str], runtime: dict[str, str], tooling_sha: str | None) -> dict[str, object]:
    provenance: dict[str, object] = {
        "product": "srep",
        "version": version,
        "target": target,
        "source_sha": commit,
        "source_worktree": state,
        "binary_sha256": sha256(binary),
        "binary": binary.name,
        "built_at_unix": int(time.time()),
        "toolchain": toolchain,
        "runtime": runtime,
        "repository": str(repo),
        "tooling_sha": tooling_sha or "not supplied",
    }
    (payload / "BUILD-PROVENANCE.json").write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return provenance


def archive(payload: Path, root_name: str, destination: Path, *, fmt: str, mtime: int) -> None:
    if destination.exists() or destination.is_symlink():
        fail(f"refusing to overwrite existing artifact: {destination}")
    handle = tempfile.NamedTemporaryFile(prefix=f".{destination.name}.", suffix=".tmp", dir=destination.parent, delete=False)
    temporary = Path(handle.name)
    handle.close()
    try:
        if fmt == "zip":
            with zipfile.ZipFile(temporary, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as out:
                for path in sorted(payload.iterdir(), key=lambda item: item.name):
                    data = path.read_bytes()
                    info = zipfile.ZipInfo(f"{root_name}/{path.name}", date_time=time.gmtime(max(mtime, 315532800))[:6])
                    info.compress_type = zipfile.ZIP_DEFLATED
                    info.external_attr = ((0o755 if path.name in {"srep", "srep.exe"} else 0o644) & 0xFFFF) << 16
                    out.writestr(info, data)
        else:
            with temporary.open("wb") as stream:
                with gzip.GzipFile(fileobj=stream, mode="wb", mtime=mtime, filename="") as compressed:
                    with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as out:
                        for path in sorted(payload.iterdir(), key=lambda item: item.name):
                            info = tarfile.TarInfo(f"{root_name}/{path.name}")
                            info.size = path.stat().st_size
                            info.mode = 0o755 if path.name in {"srep", "srep.exe"} else 0o644
                            info.mtime = mtime
                            info.uid = info.gid = 0
                            info.uname = info.gname = ""
                            with path.open("rb") as source:
                                out.addfile(info, source)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    try:
        if ARCHIVE_PRELINK_HOOK is not None:
            ARCHIVE_PRELINK_HOOK(destination)
        os.link(temporary, destination)
    except OSError:
        temporary.unlink(missing_ok=True)
        raise
    else:
        temporary.unlink(missing_ok=True)


def platform_readme(target: str) -> str:
    if target == "x86_64-pc-windows-msvc":
        return """# SREP native Windows package

Run `srep.exe --help` for usage. This unsigned package is not code-signed.
"""
    if target == "aarch64-apple-darwin":
        return """# SREP native macOS package

Run `./srep --help` for usage. This binary is unsigned and not notarized;
macOS Gatekeeper may require an explicit user approval before first run.
"""
    return """# SREP native Linux package

Run `./srep --help` for usage. This package is unsigned.
"""


def safe_extract(archive_path: Path, destination: Path, fmt: str) -> Path:
    root: Path | None = None
    if fmt == "zip":
        with zipfile.ZipFile(archive_path) as archive_file:
            for member in archive_file.infolist():
                target = (destination / member.filename).resolve()
                if destination.resolve() not in target.parents:
                    fail(f"archive member escapes extraction root: {member.filename}")
            archive_file.extractall(destination)
            names = [Path(member.filename) for member in archive_file.infolist()]
    else:
        with tarfile.open(archive_path, "r:gz") as archive_file:
            for member in archive_file.getmembers():
                target = (destination / member.name).resolve()
                if destination.resolve() not in target.parents:
                    fail(f"archive member escapes extraction root: {member.name}")
                if not member.isfile():
                    fail(f"archive contains unsupported non-regular member: {member.name}")
            archive_file.extractall(destination)
            names = [Path(member.name) for member in archive_file.getmembers()]
    roots = {name.parts[0] for name in names if name.parts}
    if len(roots) != 1:
        fail("archive must contain exactly one root directory")
    root = destination / next(iter(roots))
    return root


def unpack_smoke(archive_path: Path, *, fmt: str, binary_name: str, expected_sha: str, expected_version: str, report_path: Path) -> None:
    report: dict[str, object] = {"archive": archive_path.name, "binary": binary_name, "binary_sha256": expected_sha, "steps": []}

    def step(command: list[str], *, cwd: Path) -> subprocess.CompletedProcess[str]:
        print("smoke:", subprocess.list2cmdline(command), flush=True)
        try:
            result = subprocess.run(command, cwd=cwd, text=True, capture_output=True, check=False, timeout=None)
        except OSError as exc:
            result = subprocess.CompletedProcess(command, 127, "", str(exc))
        report["steps"].append({"command": command, "exit_code": result.returncode, "stdout": result.stdout, "stderr": result.stderr})
        if result.returncode != 0:
            fail(f"extracted smoke command failed with exit {result.returncode}: {' '.join(command)}\n{result.stderr.rstrip()}")
        return result

    try:
        with tempfile.TemporaryDirectory(prefix="srep-release-unpack-") as raw:
            root = safe_extract(archive_path, Path(raw), fmt)
            extracted = root / binary_name
            if not extracted.is_file():
                fail(f"unpacked binary missing: {extracted}")
            actual = sha256(extracted)
            report["extracted_binary_sha256"] = actual
            if actual != expected_sha:
                fail(f"unpacked binary SHA {actual} differs from staged SHA {expected_sha}")
            if os.name != "nt":
                extracted.chmod(extracted.stat().st_mode | stat.S_IXUSR)
            version = step([str(extracted), "--version"], cwd=root).stdout.strip()
            report["version"] = version
            if version != f"srep {expected_version}":
                fail(f"extracted binary reported {version!r}, expected 'srep {expected_version}'")
            step([str(extracted), "--help"], cwd=root)
            work = Path(raw) / "smoke-work"
            work.mkdir()
            empty = work / "empty.bin"
            empty.write_bytes(b"")
            multiblock = work / "multiblock.bin"
            multiblock.write_bytes(hashlib.sha256(b"srep native smoke").digest() * ((4097 + 31) // 32))
            for source in (empty, multiblock):
                archive_file = work / f"{source.name}.srep"
                restored = work / f"{source.name}.restored"
                compression_options = [] if source == empty else ["--block-size", "1K"]
                step([str(extracted), "compress", *compression_options, str(source), str(archive_file)], cwd=root)
                step([str(extracted), "info", str(archive_file)], cwd=root)
                step([str(extracted), "test", str(archive_file)], cwd=root)
                step([str(extracted), "decompress", str(archive_file), str(restored)], cwd=root)
                if restored.read_bytes() != source.read_bytes():
                    fail(f"extracted round trip differs for {source.name}")
                report.setdefault("round_trips", []).append({"input": source.name, "input_sha256": sha256(source), "restored_sha256": sha256(restored), "archive_sha256": sha256(archive_file)})
    except BaseException as exc:
        report["failure"] = str(exc)
        report_path.parent.mkdir(parents=True, exist_ok=True)
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        raise
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--build", action="store_true", help="build a fresh locked release binary in an isolated target directory")
    parser.add_argument("--target", required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--source-sha")
    parser.add_argument("--tooling-sha", help="commit that supplied the packaging tooling")
    parser.add_argument("--rustc")
    parser.add_argument("--cargo")
    parser.add_argument("--skip-license-harvest", action="store_true", help="test-only: omit registry and rustc license harvest")
    parser.add_argument("--unpack-smoke", action="store_true")
    parser.add_argument("--smoke-report", type=Path)
    args = parser.parse_args()
    args.repo = args.repo.resolve()
    args.out_dir = args.out_dir.resolve()
    args.out_dir.mkdir(parents=True, exist_ok=True)
    if args.target not in SUPPORTED_TARGETS:
        fail(f"unsupported native release target: {args.target}")
    if not args.build:
        fail("release packaging only accepts --build; package the freshly verified native binary")
    if not args.tooling_sha:
        fail("--tooling-sha is required for release provenance")
    tooling_sha, tooling_state = tooling_metadata(args.tooling_sha)
    version = package_version(args.repo)
    commit, state, commit_time = git_metadata(args.repo, args.source_sha)
    validate_build_environment(args.repo)
    rustc = resolve_rustc(args)
    build_env = cargo_environment(args.repo, rustc)
    toolchain = toolchain_metadata(args, args.repo, build_env, rustc)
    if toolchain["rustc_host"] not in SUPPORTED_TARGETS:
        fail(f"unsupported native rustc host: {toolchain['rustc_host']}")
    build_root: Path | None = None
    cleanup_build = None
    build_command = "not available (reused binary)"
    built_at = int(time.time())
    if args.build:
        args.binary, built_at, build_command, build_root = build_binary(args, toolchain, args.out_dir, build_env)
        cleanup_build = lambda: shutil.rmtree(build_root, ignore_errors=True)
        atexit.register(cleanup_build)
    after_commit, after_state, after_time = git_metadata(args.repo, commit)
    if after_commit != commit or after_state != "clean":
        fail("source checkout changed or became dirty during the native build")
    if not args.binary.is_file():
        fail(f"binary does not exist: {args.binary}")
    validate_binary_format(args.binary, args.target)
    runtime = runtime_metadata(args.target, args.binary)
    binary_name = "srep.exe" if "windows" in args.target else "srep"
    root_name = f"srep-v{version}-{args.target}"
    extension = ".zip" if args.target == "x86_64-pc-windows-msvc" else ".tar.gz"
    output_lock = OutputLock(args.out_dir)
    output_lock.__enter__()
    atexit.register(output_lock.__exit__, None, None, None)
    with tempfile.TemporaryDirectory(prefix="srep-release-stage-", dir=args.out_dir) as raw_stage:
        payload = Path(raw_stage) / root_name
        payload.mkdir()
        shutil.copyfile(args.binary, payload / binary_name)
        for document in DOCUMENTS:
            source = args.repo / document
            if not source.is_file():
                fail(f"required package file missing: {source}")
            shutil.copyfile(source, payload / document)
        (payload / "PLATFORM-README.md").write_text(platform_readme(args.target), encoding="utf-8")
        (payload / "NOTICES").write_text(
            "\n".join([
                "SREP native release notices",
                "===========================",
                f"Product: srep",
                f"Version: {version}",
                f"Target: {args.target}",
                f"Binary SHA-256: {sha256(payload / binary_name)}",
                f"Source SHA: {commit}",
                f"Source worktree: {state}",
                f"Cargo manifest version: {version}",
                f"Rustc: {toolchain['rustc']}",
                f"Rustc release: {toolchain['rustc_release']}",
                f"Rustc commit: {toolchain['rustc_commit']}",
                f"Rustc host: {toolchain['rustc_host']}",
                f"Rustc sysroot: {toolchain['rustc_sysroot']}",
                "",
                "Runtime compatibility",
                "---------------------",
                runtime["runtime"],
                *(f"{key}: {value}" for key, value in runtime.items() if key not in {"runtime", "target", "host_os"}),
                "",
                "Signing/notarization: no signature is included. macOS users may need to approve this unsigned binary in Gatekeeper.",
                "",
                crate_notice(args.repo, args, build_env),
                rust_std_notice(toolchain, args),
            ]) + "\n",
            encoding="utf-8",
        )
        provenance = write_provenance(payload, repo=args.repo, binary=payload / binary_name, version=version, target=args.target, commit=commit, state=state, toolchain=toolchain, runtime=runtime, tooling_sha=tooling_sha)
        provenance["tooling_worktree"] = tooling_state
        provenance["build_command"] = build_command
        provenance["built_at_unix"] = built_at
        (payload / "BUILD-PROVENANCE.json").write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        for path in payload.iterdir():
            path.chmod(0o755 if path.name == binary_name else 0o644)
        destination = args.out_dir / artifact_name(version, args.target)
        fmt = "zip" if extension == ".zip" else "tar"
        archive(payload, root_name, destination, fmt=fmt, mtime=commit_time)
    artifact_owned = True
    try:
        if args.unpack_smoke:
            report_path = (args.smoke_report or (args.out_dir / smoke_name(version, args.target))).resolve()
            unpack_smoke(destination, fmt=fmt, binary_name=binary_name, expected_sha=str(provenance["binary_sha256"]), expected_version=version, report_path=report_path)
        digest = sha256(destination)
        sums = args.out_dir / "SHA256SUMS"
        if sums.exists() or sums.is_symlink():
            existing = sums.read_text(encoding="utf-8")
            if destination.name in existing:
                fail(f"SHA256SUMS already contains an entry for {destination.name}")
            sums_text = existing + f"{digest}  {destination.name}\n"
        else:
            sums_text = f"{digest}  {destination.name}\n"
        sums_handle = tempfile.NamedTemporaryFile(prefix=f".{sums.name}.", suffix=".tmp", dir=sums.parent, delete=False)
        sums_tmp = Path(sums_handle.name)
        sums_handle.close()
        try:
            sums_tmp.write_text(sums_text, encoding="utf-8")
            if os.environ.get("SREP_TEST_CHECKSUM_FAILURE") == "1":
                fail("test-only checksum publication failure")
            sums_tmp.replace(sums)
        finally:
            sums_tmp.unlink(missing_ok=True)
    except BaseException:
        if artifact_owned:
            destination.unlink(missing_ok=True)
        raise
    if cleanup_build is not None:
        cleanup_build()
    output_lock.__exit__(None, None, None)
    atexit.unregister(output_lock.__exit__)
    print(f"wrote {destination}")
    print(f"wrote {sums}")
    print(f"source_sha {commit}")
    print(f"binary_sha256 {provenance['binary_sha256']}")
    return 0


if __name__ == "__main__":
    main()

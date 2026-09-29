#!/usr/bin/env python3
"""Unit tests for scripts/package-linux-release.sh (no cargo build, no publish)."""
from __future__ import annotations

import hashlib
import json
import os
import shutil
import stat
import subprocess
import tarfile
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "package-linux-release.sh"


def run(args: list[str], **kwargs) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        args,
        check=False,
        text=True,
        capture_output=True,
        **kwargs,
    )


def write_exec(path: Path, body: bytes = b"#!/bin/sh\necho srep 0.1.1\n") -> Path:
    path.write_bytes(body)
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    return path


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def provenance_for(binary: Path, **extra: object) -> dict[str, object]:
    payload = {
        "binary_sha256": sha256(binary),
        "build_command": "cargo build --release --locked --bin srep",
        "compiler": "rustc 1.96.0 (test sidecar)",
        "compiler_release": "1.96.0",
        "compiler_commit": extra.pop(
            "compiler_commit", "ac68faa20c58cbccd01ee7208bf3b6e93a7d7f96"
        ),
        "compiler_bin": extra.pop(
            "compiler_bin",
            "/home/test/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc",
        ),
        "compiler_sysroot": extra.pop(
            "compiler_sysroot",
            "/home/test/.rustup/toolchains/stable-x86_64-unknown-linux-gnu",
        ),
        "source_sha": extra.pop("source_sha", "8dca8bedc4cd53e268f8f0998871ca3670a497e7"),
        "worktree": extra.pop("worktree", "dirty"),
        "note": extra.pop("note", "Validated sidecar bound to binary SHA."),
    }
    payload.update(extra)
    return payload


def make_mini_repo(tmp: Path, *, git: bool = False) -> Path:
    repo = tmp / "repo"
    repo.mkdir()
    (repo / "Cargo.toml").write_text(
        '[package]\nname = "srep"\nversion = "0.1.1"\nedition = "2021"\n',
        encoding="utf-8",
    )
    for name in ("LICENSE", "README.md", "CHANGELOG.md", "THIRD_PARTY.md", "THIRD_PARTY.audit"):
        (repo / name).write_text(f"{name} test\n", encoding="utf-8")
    (repo / "scripts").mkdir()
    (repo / "scripts" / "release-smoke.sh").write_text(
        "#!/bin/sh\nset -eu\necho smoke-ok \"${SREP_RELEASE_BIN:-missing}\"\n",
        encoding="utf-8",
    )
    (repo / "scripts" / "release-smoke.sh").chmod(0o755)
    if git:
        subprocess.run(["git", "init"], cwd=repo, check=True, capture_output=True)
        subprocess.run(["git", "config", "user.email", "t@example.com"], cwd=repo, check=True)
        subprocess.run(["git", "config", "user.name", "t"], cwd=repo, check=True)
        subprocess.run(["git", "add", "."], cwd=repo, check=True, capture_output=True)
        subprocess.run(["git", "commit", "-m", "init"], cwd=repo, check=True, capture_output=True)
    return repo


def test_help_and_syntax() -> None:
    syntax = run(["sh", "-n", str(SCRIPT)])
    assert syntax.returncode == 0, syntax.stderr
    help_out = run(["sh", str(SCRIPT), "--help"])
    assert help_out.returncode == 0, help_out.stderr
    assert "Unpack smoke" in help_out.stdout
    assert "srep-v<VERSION>-x86_64-unknown-linux-gnu.tar.gz" in help_out.stdout
    assert "--stage-only" in help_out.stdout
    assert "release-smoke.sh" in help_out.stdout
    assert "SREP_RELEASE_BIN" in help_out.stdout


def test_manifest_version_and_override_mismatch() -> None:
    with tempfile.TemporaryDirectory(prefix="srep-pkg-version-") as raw:
        root = Path(raw)
        repo = make_mini_repo(root)
        binary = write_exec(root / "srep")
        env = {**os.environ, "SREP_VERSION": "0.1.0"}
        result = run(["sh", str(SCRIPT), "--repo", str(repo), "--bin", str(binary),
                      "--out-dir", str(root / "out"), "--stage-only", "--skip-licenses"], env=env)
        assert result.returncode == 2, result.stdout + result.stderr
        assert "does not match Cargo.toml package version 0.1.1" in result.stderr
        assert not (root / "out").exists()


def test_bin_without_provenance_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-noprov-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ]
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "--provenance" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_wrong_sidecar_sha_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-wrongsha-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        payload = provenance_for(fake_bin)
        payload["binary_sha256"] = "0" * 64
        sidecar.write_text(json.dumps(payload), encoding="utf-8")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ]
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "does not match reused binary" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_matching_sidecar_records_build_not_packaging_compiler() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-sidecar-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin, worktree="dirty")), encoding="utf-8")
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        notices = (
            tmp / "stage" / "srep-v0.1.1-x86_64-unknown-linux-gnu" / "NOTICES"
        ).read_text(encoding="utf-8")
        assert "Provenance kind: sidecar" in notices
        assert "Build compiler: rustc 1.96.0 (test sidecar)" in notices
        assert "Packaging-time metadata (not a substitute for build provenance)" in notices
        assert "UNPROVENANCED" not in notices
        tarball = tmp / "out" / "srep-v0.1.1-x86_64-unknown-linux-gnu.tar.gz"
        assert not tarball.exists()
        assert "SREP_RELEASE_BIN=" in result.stdout
        assert "release-smoke.sh" in result.stdout
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_expected_source_mismatch_on_clean_tree_rejects() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-mismatch-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=True)
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(
            json.dumps(
                provenance_for(
                    fake_bin, source_sha="ffffffffffffffffffffffffffffffffffffffff"
                )
            ),
            encoding="utf-8",
        )
        env = os.environ.copy()
        env["SREP_SOURCE_SHA"] = "8dca8bedc4cd53e268f8f0998871ca3670a497e7"
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "does not match" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_clean_tree_is_not_labeled_dirty() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-clean-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=True)
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(
            json.dumps(provenance_for(fake_bin, source_sha=head, worktree="clean")),
            encoding="utf-8",
        )
        env = os.environ.copy()
        env["SREP_SOURCE_SHA"] = head
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        notices = (
            tmp / "stage" / "srep-v0.1.1-x86_64-unknown-linux-gnu" / "NOTICES"
        ).read_text(encoding="utf-8")
        assert "Packaging worktree: clean" in notices
        assert "This packaging tree is dirty" not in notices
        assert "Packaging worktree is clean" in notices
        assert "Provenance kind: sidecar" in notices
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dirty_tree_is_labeled_dirty() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-dirty-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=True)
        (repo / "untracked.txt").write_text("dirty\n", encoding="utf-8")
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin, worktree="dirty")), encoding="utf-8")
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        notices = (
            tmp / "stage" / "srep-v0.1.1-x86_64-unknown-linux-gnu" / "NOTICES"
        ).read_text(encoding="utf-8")
        assert "Packaging worktree: dirty" in notices
        assert "This packaging tree is dirty" in notices
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_tar_failure_leaves_no_publishable_artifact() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-tarfail-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin)), encoding="utf-8")
        fake_tar = tmp / "bad-tar"
        fake_tar.write_text("#!/bin/sh\necho tar-injected-failure >&2\nexit 1\n", encoding="utf-8")
        fake_tar.chmod(0o755)
        env = os.environ.copy()
        env["SREP_TAR_BIN"] = str(fake_tar)
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        out_dir = tmp / "out"
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(out_dir),
                "--stage-dir",
                str(tmp / "stage"),
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode != 0, result.stdout + result.stderr
        assert "tar failed" in result.stderr
        assert not (out_dir / "srep-v0.1.1-x86_64-unknown-linux-gnu.tar.gz").exists()
        assert not (out_dir / "SHA256SUMS").exists()
        leftovers = list(out_dir.glob("*.tar.gz")) + list(out_dir.glob("SHA256SUMS*"))
        assert leftovers == []
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_gzip_failure_leaves_no_publishable_artifact() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-gzipfail-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin)), encoding="utf-8")
        fake_gzip = tmp / "bad-gzip"
        fake_gzip.write_text("#!/bin/sh\necho gzip-injected-failure >&2\nexit 1\n", encoding="utf-8")
        fake_gzip.chmod(0o755)
        env = os.environ.copy()
        env["SREP_GZIP_BIN"] = str(fake_gzip)
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        out_dir = tmp / "out"
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(out_dir),
                "--stage-dir",
                str(tmp / "stage"),
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode != 0, result.stdout + result.stderr
        assert "gzip failed" in result.stderr
        assert not (out_dir / "srep-v0.1.1-x86_64-unknown-linux-gnu.tar.gz").exists()
        assert not (out_dir / "SHA256SUMS").exists()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_successful_tar_layout_and_checksum() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-tarok-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep", b"srep-candidate-bytes\n")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin)), encoding="utf-8")
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        out_dir = tmp / "out"
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(out_dir),
                "--stage-dir",
                str(tmp / "stage"),
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        tarball = out_dir / "srep-v0.1.1-x86_64-unknown-linux-gnu.tar.gz"
        sums = out_dir / "SHA256SUMS"
        assert tarball.is_file()
        assert sums.is_file()
        check = run(["sha256sum", "-c", "SHA256SUMS"], cwd=out_dir)
        assert check.returncode == 0, check.stdout + check.stderr
        with tarfile.open(tarball, "r:gz") as archive:
            names = archive.getnames()
        assert "srep-v0.1.1-x86_64-unknown-linux-gnu/srep" in names
        assert "srep-v0.1.1-x86_64-unknown-linux-gnu/NOTICES" in names
        assert "srep-v0.1.1-x86_64-unknown-linux-gnu/README.md" in names
        assert "SREP_RELEASE_BIN=" in result.stdout
        assert "scripts/release-smoke.sh" in result.stdout
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_rust_std_copyright_is_copied_and_bound() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-std-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin)), encoding="utf-8")
        sysroot = tmp / "sysroot"
        doc = sysroot / "share" / "doc" / "rust"
        doc.mkdir(parents=True)
        (doc / "COPYRIGHT-library.html").write_text(
            "<!DOCTYPE html><html><body>"
            "<h1>Copyright notices for The Rust Standard Library</h1>"
            "<p>Apache-2.0 OR MIT. Includes gimli and addr2line runtime crates.</p>"
            "<p>Copyright (c) The Rust Project Developers</p>"
            "</body></html>\n",
            encoding="utf-8",
        )
        env = os.environ.copy()
        env["CARGO_HOME"] = os.environ.get(
            "CARGO_HOME",
            str(Path("/tmp/opencode/ng3-release-20260923T061924Z/cargo-home-isolated")),
        )
        env["SREP_RUSTC_SYSROOT"] = str(sysroot)
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--stage-only",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        notices = (
            tmp / "stage" / "srep-v0.1.1-x86_64-unknown-linux-gnu" / "NOTICES"
        ).read_text(encoding="utf-8")
        assert "Copyright notices for The Rust Standard Library" in notices
        assert "gimli" in notices
        assert "addr2line" in notices
        assert "registry_crate_count=" in notices
        assert "Rust standard library and rustc runtime components" in notices
        assert "sysroot:" in notices
        assert "copyright_binding:" in notices
        assert "validated *build* toolchain" in notices or "validated build" in notices.lower()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_unpack_smoke_with_stage_only_fails() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-smoke-stage-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin)), encoding="utf-8")
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--unpack-smoke",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 2
        assert "--unpack-smoke ignored" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_missing_required_doc_fails() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-missing-", dir=os.environ.get("TMPDIR")))
    try:
        fake_repo = tmp / "repo"
        shutil.copytree(
            ROOT,
            fake_repo,
            ignore=shutil.ignore_patterns("target", ".git"),
            dirs_exist_ok=False,
        )
        (fake_repo / "CHANGELOG.md").unlink()
        fake_bin = write_exec(tmp / "srep", b"x")
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(provenance_for(fake_bin)), encoding="utf-8")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(fake_repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ]
        )
        assert result.returncode != 0
        assert "CHANGELOG.md" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _write_sidecar(path: Path, binary: Path, **extra: object) -> Path:
    path.write_text(json.dumps(provenance_for(binary, **extra)), encoding="utf-8")
    return path


def test_unpack_smoke_preserves_nonzero_smoke_exit() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-smoke7-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=False)
        (repo / "scripts" / "release-smoke.sh").write_text(
            "#!/bin/sh\necho smoke-injected-failure >&2\nexit 7\n",
            encoding="utf-8",
        )
        (repo / "scripts" / "release-smoke.sh").chmod(0o755)
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin)
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        env.pop("SREP_ALLOW_UNPROVENANCED_BIN", None)
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--unpack-smoke",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 7, result.stdout + result.stderr
        assert "release-smoke.sh exit=7" in result.stdout + result.stderr
        assert "smoke-injected-failure" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_unpack_smoke_success_keeps_exit_zero() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-smoke0-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=False)
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin)
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--unpack-smoke",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        assert "release-smoke.sh exit=0" in result.stdout
        assert "smoke-ok" in result.stdout
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_predictable_provenance_env_symlink_is_not_clobbered_or_sourced() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-symlink-", dir=os.environ.get("TMPDIR")))
    try:
        private_tmp = tmp / "private-tmp"
        private_tmp.mkdir()
        victim = tmp / "clobber-victim"
        victim.write_text("ORIGINAL\n", encoding="utf-8")
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin)
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        env["TMPDIR"] = str(private_tmp)
        wrapper = tmp / "run.sh"
        wrapper.write_text(
            "#!/bin/sh\n"
            "set -eu\n"
            "ln -sf \"$1\" \"${TMPDIR}/srep-prov-env.$$\"\n"
            "shift\n"
            "exec sh \"$@\"\n",
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        result = run(
            [
                "sh",
                str(wrapper),
                str(victim),
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
        assert victim.read_text(encoding="utf-8") == "ORIGINAL\n"
        leftover = list(private_tmp.glob("srep-prov-env*"))
        # The wrapper creates one symlink named for its (exec-preserved) pid;
        # the packager must not replace it with sourced env contents.
        for path in leftover:
            assert path.is_symlink(), path
            assert path.resolve() == victim.resolve()
        notices = (
            tmp / "stage" / "srep-v0.1.1-x86_64-unknown-linux-gnu" / "NOTICES"
        ).read_text(encoding="utf-8")
        assert "Provenance kind: sidecar" in notices
        assert "PWNED" not in notices
        assert "${TMPDIR:-/tmp}/srep-prov-env.$$" not in SCRIPT.read_text(encoding="utf-8")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_rustc_selector_conflict_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-rustc-", dir=os.environ.get("TMPDIR")))
    try:
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "cargo")
        env["CARGO_TARGET_DIR"] = str(tmp / "target")
        env["TMPDIR"] = str(tmp / "tmp")
        env["RUSTC_BIN"] = "/bin/true"
        env["RUSTC"] = "/bin/false"
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--build",
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "conflicts with selected rustc" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_matching_rustc_selectors_are_resolved_to_one_path() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-rustc-ok-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin)
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        rustc = env.get("RUSTC_BIN") or env.get("RUSTC") or "/usr/bin/rustc"
        env["RUSTC_BIN"] = rustc
        env["RUSTC"] = rustc
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-dir",
                str(tmp / "stage"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 0, result.stdout + result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_missing_provenance_field_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-missingf-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        payload = provenance_for(fake_bin)
        del payload["compiler"]
        sidecar = tmp / "prov.json"
        sidecar.write_text(json.dumps(payload), encoding="utf-8")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ]
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "missing required field compiler" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_empty_provenance_field_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-emptyf-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin, build_command="   ")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ]
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "must be a nonempty string" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_malformed_source_sha_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-badsha-", dir=os.environ.get("TMPDIR")))
    try:
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin, source_sha="not-a-git-sha")
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(ROOT),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ]
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "source_sha must be 40 lowercase hex" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_expected_source_sha_must_be_nonempty_hex() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-emptyexp-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=True)
        (repo / "dirty").write_text("x\n", encoding="utf-8")
        fake_bin = write_exec(tmp / "srep")
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin)
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        env["SREP_SOURCE_SHA"] = ""
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "SREP_SOURCE_SHA must be 40 lowercase hex" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_sidecar_source_mismatch_is_rejected() -> None:
    tmp = Path(tempfile.mkdtemp(prefix="srep-pkg-srcmm-", dir=os.environ.get("TMPDIR")))
    try:
        repo = make_mini_repo(tmp, git=True)
        (repo / "dirty").write_text("x\n", encoding="utf-8")
        fake_bin = write_exec(tmp / "srep")
        other = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        sidecar = _write_sidecar(tmp / "prov.json", fake_bin, source_sha=other)
        env = os.environ.copy()
        env["CARGO_HOME"] = str(tmp / "no-cargo")
        env["SREP_SOURCE_SHA"] = "8dca8bedc4cd53e268f8f0998871ca3670a497e7"
        result = run(
            [
                "sh",
                str(SCRIPT),
                "--repo",
                str(repo),
                "--bin",
                str(fake_bin),
                "--provenance",
                str(sidecar),
                "--out-dir",
                str(tmp / "out"),
                "--stage-only",
                "--skip-licenses",
            ],
            env=env,
        )
        assert result.returncode == 2, result.stdout + result.stderr
        assert "does not match SREP_SOURCE_SHA" in result.stderr
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_authors_file_is_treated_as_copyright_source() -> None:
    snippet = SCRIPT.read_text(encoding="utf-8")
    assert '"AUTHORS"' in snippet
    assert "NO LICENSE/COPYRIGHT/AUTHORS file found" in snippet
    assert "COPYRIGHT-library.html" in snippet
    assert "SREP_RELEASE_BIN" in snippet


def main() -> int:
    tests = [
        test_help_and_syntax,
        test_manifest_version_and_override_mismatch,
        test_bin_without_provenance_is_rejected,
        test_wrong_sidecar_sha_is_rejected,
        test_matching_sidecar_records_build_not_packaging_compiler,
        test_expected_source_mismatch_on_clean_tree_rejects,
        test_clean_tree_is_not_labeled_dirty,
        test_dirty_tree_is_labeled_dirty,
        test_tar_failure_leaves_no_publishable_artifact,
        test_gzip_failure_leaves_no_publishable_artifact,
        test_successful_tar_layout_and_checksum,
        test_rust_std_copyright_is_copied_and_bound,
        test_unpack_smoke_with_stage_only_fails,
        test_unpack_smoke_preserves_nonzero_smoke_exit,
        test_unpack_smoke_success_keeps_exit_zero,
        test_predictable_provenance_env_symlink_is_not_clobbered_or_sourced,
        test_rustc_selector_conflict_is_rejected,
        test_matching_rustc_selectors_are_resolved_to_one_path,
        test_missing_provenance_field_is_rejected,
        test_empty_provenance_field_is_rejected,
        test_malformed_source_sha_is_rejected,
        test_expected_source_sha_must_be_nonempty_hex,
        test_sidecar_source_mismatch_is_rejected,
        test_missing_required_doc_fails,
        test_authors_file_is_treated_as_copyright_source,
    ]
    for test in tests:
        test()
        print(f"{test.__name__}: ok")
    print(f"test-package-linux-release.py: PASS ({len(tests)} tests)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

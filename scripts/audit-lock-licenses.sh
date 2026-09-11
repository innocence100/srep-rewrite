#!/bin/sh
set -eu

root=${SREP_AUDIT_ROOT:-$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)}
registry=${CARGO_HOME:-"$HOME/.cargo"}/registry/src

python3 - "$root" "$registry" <<'PY'
import json
import pathlib
import re
import subprocess
import sys
import tomllib

root = pathlib.Path(sys.argv[1])
registry = pathlib.Path(sys.argv[2])
lock = tomllib.loads((root / "Cargo.lock").read_text())
audit = {}
for number, line in enumerate((root / "THIRD_PARTY.audit").read_text().splitlines(), 1):
    if not line or line.startswith("#"):
        continue
    fields = line.split("|")
    if len(fields) != 4 or any(not field for field in fields):
        raise SystemExit(f"invalid attribution line {number}")
    name, version, license_expression, repository = fields
    key = (name, version)
    if key in audit:
        raise SystemExit(f"duplicate attribution for {name} {version}")
    audit[key] = (license_expression, repository)

metadata = json.loads(subprocess.check_output([
    "cargo", "metadata", "--format-version", "1", "--locked",
    "--manifest-path", str(root / "Cargo.toml"),
]))
local = {
    (package["name"], package["version"]): package
    for package in metadata["packages"]
    if package["source"] is not None
}
lock_packages = {
    (package["name"], package["version"]): package
    for package in lock["package"]
    if package.get("source", "").startswith("registry+")
}
if set(audit) != set(lock_packages):
    raise SystemExit(
        "attribution package set mismatch: "
        f"missing={sorted(set(lock_packages) - set(audit))} "
        f"extra={sorted(set(audit) - set(lock_packages))}"
    )
if set(local) != set(lock_packages):
    raise SystemExit("cargo metadata and Cargo.lock package sets differ")

markdown = (root / "THIRD_PARTY.md").read_text()
entries = {}
for match in re.finditer(r"(?ms)^- `([^`]+)` (\S+) —(.*?)(?=^- `|\Z)", markdown):
    name, version, body = match.groups()
    key = (name, version)
    if key in entries:
        raise SystemExit(f"duplicate Markdown attribution for {name} {version}")
    license_match = re.search(r"License:\s*(.*?)\.\s+Upstream:", body, re.S)
    repository_match = re.search(r"Upstream:\s*<([^>]+)>", body)
    if not license_match or not repository_match:
        raise SystemExit(f"unparseable Markdown attribution for {name} {version}")
    entries[key] = (" ".join(license_match.group(1).split()), repository_match.group(1))
if set(entries) != set(lock_packages):
    raise SystemExit(
        "Markdown attribution package set mismatch: "
        f"missing={sorted(set(lock_packages) - set(entries))} "
        f"extra={sorted(set(entries) - set(lock_packages))}"
    )
for key in sorted(lock_packages):
    name, version = key
    manifest = local[key]["manifest_path"]
    package = tomllib.loads(pathlib.Path(manifest).read_text())["package"]
    expected = (package.get("license"), package.get("repository"))
    if None in expected:
        raise SystemExit(f"missing license/repository metadata for {name} {version}")
    if audit[key] != expected:
        raise SystemExit(
            f"metadata mismatch for {name} {version}: attribution={audit[key]!r} metadata={expected!r}"
        )
    if entries[key] != expected:
        raise SystemExit(
            f"Markdown mismatch for {name} {version}: attribution={entries[key]!r} metadata={expected!r}"
        )
    print(f"{name}\t{version}\t{expected[0]}\t{expected[1]}")
print(f"audited {len(lock_packages)} registry packages")
PY

#!/usr/bin/env python3
"""Regression checks for candidate identity and the six-job native release gate."""
import importlib.util
from pathlib import Path
import tomllib

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("ci_yaml", ROOT / "scripts/test-ci-yaml.py")
ci = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ci)
workflow = ci.load_workflow(ROOT / ".github/workflows/native-release.yml")
# Ruby's YAML 1.1 parser represents the key `on` as true.
events = workflow.get("on", workflow.get("true"))
assert events["push"]["branches"] == ["ci/native-release/v0.1.1"]
inputs = events["workflow_dispatch"]["inputs"]
for key in ("ref", "source_sha"):
    assert inputs[key]["required"] is True and "default" not in inputs[key]
version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
assert str(inputs["version"]["default"]) == version == "0.1.1"
assert workflow["permissions"] == {"contents": "read"}
job = workflow["jobs"]["native-package"]
matrix = job["strategy"]["matrix"]["include"]
assert len(matrix) == 6
assert {(row["os"], str(row["rust"]), row["target"]) for row in matrix} == {
    (os, rust, target)
    for os, target in (("ubuntu-latest", "x86_64-unknown-linux-gnu"),
                       ("windows-latest", "x86_64-pc-windows-msvc"),
                       ("macos-latest", "aarch64-apple-darwin"))
    for rust in ("1.88.0", "stable")
}
steps = job["steps"]
checkouts = [step["with"] for step in steps if step.get("uses", "").startswith("actions/checkout@")]
assert checkouts[0]["ref"] == "${{ inputs.ref || github.sha }}"
assert checkouts[1]["ref"] == "${{ github.sha }}"
assert all(step["persist-credentials"] is False for step in checkouts)
assert any(step.get("run") == "cargo test --locked --release --all-targets --all-features" for step in steps)
assert str(job["env"]["RUST_TEST_THREADS"]) == "1"
for step in steps:
    if "EXPECTED_SOURCE_SHA" in step.get("env", {}):
        assert step["env"]["EXPECTED_SOURCE_SHA"] == "${{ inputs.source_sha || github.sha }}"
package = next(step for step in steps if "--unpack-smoke" in step.get("run", ""))
assert package["if"] == "matrix.rust == '1.88.0'"
assert "--build" in package["run"] and "--tooling-sha" in package["run"]
assert "--skip-license" not in package["run"]
print("test-native-release-workflow.py: PASS (identity, version, six native jobs, clean packaging gate)")

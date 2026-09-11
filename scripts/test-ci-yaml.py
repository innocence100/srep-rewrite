#!/usr/bin/env python3
"""Parse the workflow with the platform YAML parser when available."""
from pathlib import Path
import subprocess


def main() -> int:
    workflow = Path(__file__).resolve().parents[1] / ".github" / "workflows" / "ci.yml"
    text = workflow.read_text()
    required = (
        "id: mingw",
        "x86_64-w64-mingw32-gcc",
        "x86_64-w64-mingw32-dlltool",
        "available=$($available.ToString().ToLowerInvariant())",
        "if: steps.mingw.outputs.available == 'true'",
        "if: steps.mingw.outputs.available != 'true'",
        "cargo build --bin srep --example stage6-fixed-metrics",
        "python3 scripts/compare-fixed-finders.py --decoder target/debug/srep",
    )
    missing = [value for value in required if value not in text]
    assert not missing, f"missing Windows linker condition: {missing}"
    debug_build = text.index("      - run: cargo build --bin srep --example stage6-fixed-metrics\n")
    evidence_gate = text.index("      - run: python3 scripts/compare-fixed-finders.py --decoder target/debug/srep\n")
    debug_tests = text.index("      - run: cargo test --all-targets --all-features\n")
    assert "      - run: cargo build\n" not in text[:evidence_gate], "plain build-only arrangement is not sufficient"
    assert debug_build < evidence_gate < debug_tests, "Stage 6 evidence gate must follow the debug build"
    subprocess.run(["ruby", "-e", "require 'yaml'; YAML.load_file(ARGV.fetch(0))", str(workflow)], check=True)
    print("CI YAML parsed by Ruby Psych")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

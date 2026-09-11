#!/usr/bin/env python3
"""Apply and execute the committed legacy corruption manifest at runtime."""
from __future__ import annotations

import json
import subprocess
import sys
import tempfile
from pathlib import Path
from legacy_fixture_schema import category, identity


def main() -> int:
    root = Path(__file__).resolve().parents[1]
    fixture_root = root / "tests" / "fixtures" / "legacy"
    manifest = json.loads((fixture_root / "corruptions.json").read_text())
    assert manifest["schema"] == "srep-legacy-corruptions-v1"
    names = [case["name"] for case in manifest["cases"]]
    assert len(names) == len(set(names)), "duplicate corruption case name"
    identities = set()
    for case in manifest["cases"]:
        case_identity = identity(case)
        assert case_identity not in identities, f"duplicate corruption identity: {case_identity}"
        identities.add(case_identity)
    assert len(names) == len(identities) >= 100
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else root / "target" / "debug" / "srep"
    passed = 0
    with tempfile.TemporaryDirectory(prefix="srep-corruptions-", dir="/tmp/opencode") as directory:
        temp = Path(directory)
        for case in manifest["cases"]:
            base = (fixture_root / case.get("base_fixture", manifest["base_fixture"])).read_bytes()
            data = bytearray(base)
            offset = case.get("offset")
            if case["operation"] == "delete":
                del data[offset]
            else:
                data[offset] ^= case.get("value", 1)
            path = temp / (case["name"].replace(".", "_") + ".srep")
            path.write_bytes(data)
            result = subprocess.run([str(binary), "test", str(path)], capture_output=True, text=True)
            expected = case["expected"]
            expected_code = {
                "Accepted": 0,
                "ChecksumMismatch": 11,
                "UnsupportedVersion": 3,
                "UnknownChecksum": 5,
                "CorruptHeader": 6,
                "CorruptIndex": 9,
                "InvalidMatch": 10,
                "TruncatedArchive": 7,
            }[expected]
            token = {
                "Accepted": "",
                "InvalidMatch": "INVALID_MATCH",
                "ChecksumMismatch": "CHECKSUM",
                "UnsupportedVersion": "UNSUPPORTED_VERSION",
                "UnknownChecksum": "UNKNOWN_CHECKSUM",
                "CorruptHeader": "CORRUPT_HEADER",
                "CorruptIndex": "CORRUPT_INDEX",
                "TruncatedArchive": "TRUNCATED",
            }[expected]
            if expected == "Accepted":
                baseline = temp / (path.stem + ".baseline.srep")
                baseline.write_bytes(base)
                baseline_out = temp / (path.stem + ".baseline.out")
                mutated_out = temp / (path.stem + ".out")
                subprocess.run([str(binary), "decompress", str(baseline), str(baseline_out)], check=True, capture_output=True)
                accepted = subprocess.run([str(binary), "decompress", str(path), str(mutated_out)], capture_output=True)
                ok = result.returncode == expected_code == accepted.returncode
                if ok and "opaque_digest" in case["name"]:
                    ok = baseline_out.read_bytes() == mutated_out.read_bytes()
                if ok and "literal_mutation" in case["name"]:
                    ok = baseline_out.read_bytes() != mutated_out.read_bytes() and len(baseline_out.read_bytes()) == len(mutated_out.read_bytes())
            else:
                ok = result.returncode == expected_code and token in result.stderr and f"SREP_E_{token}" in result.stderr
            if not ok:
                raise SystemExit(f"{case['name']}: expected {expected}: {result.stderr.strip()}")
            passed += 1
    categories = sorted({category(case["name"]) for case in manifest["cases"]})
    print(f"legacy corruption matrix passed: {passed} unique cases; categories={','.join(categories)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

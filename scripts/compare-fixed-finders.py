#!/usr/bin/env python3
"""Strict, reproducible Stage 6 fixed-finder evidence gate."""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures" / "fidelity"
MANIFEST = FIXTURES / "stage6-fixed.json"
CORPUS = FIXTURES / "stage6-fixed.bin"
DECODER_DEFAULT = ROOT / "target" / "debug" / "srep"

TOP_KEYS = {"schema", "corpus", "old_binary", "comparisons", "conformance"}
CORPUS_KEYS = {"file", "sha256", "size"}
OLD_BINARY_KEYS = {"name", "sha256", "read_only"}
COMPARISON_KEYS = {
    "method", "layout", "base_len", "seed_size", "minimum_match", "block_size",
    "rep_overlay", "old_command", "new_command", "old_archive", "old_result",
    "new_result", "m4_witness", "notes",
}
ARCHIVE_KEYS = {"file", "sha256", "size", "version", "layout", "checksum", "base_len"}
OLD_RESULT_KEYS = {"match_count", "encoded_bytes", "covered_bytes", "literal_bytes", "archive_size"}
NEW_RESULT_KEYS = {"raw_candidate_count", "normalized_match_count", "covered_bytes", "literal_bytes", "archive_size", "archive_sha256"}
WITNESS_KEYS = {"seed_source", "seed_target", "backward_bytes", "source", "destination", "length"}
CONFORMANCE_KEYS = {
    "method", "layout", "seed_size", "minimum_match", "raw_candidate_count",
    "normalized_match_count", "covered_bytes", "literal_bytes", "corpus_recipe", "scope",
}

EXPECTED_OLD_COMMON = ["srep", "-v0", "-b8k", "-l16", "-c16", "-d0"]
EXPECTED_OLD_BINARY_NAME = "SREP 3.93a beta"
EXPECTED_OLD_BINARY_SHA256 = "e8ca47d05ecceb7f3c5ff3fa6d4c8cdc88f17b3f3f1838e6ca06b3c0cded7b1e"
EXPECTED_OLD_BINARY_READ_ONLY = True
EXPECTED_NEW_COMMON = [
    "srep", "compress", "--layout", "io", "--checksum", "xxh3", "--block-size",
    "8KiB", "--min-match", "16", "--seed-size", "16", "INPUT", "OUTPUT",
]


def strict_load(path: Path) -> dict:
    def pairs(pairs: list[tuple[str, object]]) -> dict:
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=pairs)
    if not isinstance(value, dict):
        raise ValueError("manifest root must be an object")
    return value


def assert_duplicate_key_rejected() -> None:
    with tempfile.NamedTemporaryFile("w", encoding="utf-8") as file:
        file.write('{"schema":"stage6-fixed-v3","schema":"tampered"}')
        file.flush()
        try:
            strict_load(Path(file.name))
        except ValueError as error:
            if "duplicate JSON key" not in str(error):
                raise
        else:
            raise ValueError("duplicate JSON keys were accepted")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def require_keys(value: object, expected: set[str], path: str) -> dict:
    if not isinstance(value, dict):
        raise ValueError(f"{path} must be an object")
    actual = set(value)
    if actual != expected:
        raise ValueError(f"{path} keys differ: expected {sorted(expected)}, got {sorted(actual)}")
    return value


def integer(value: object, path: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{path} must be a nonnegative integer")
    return value


def boolean(value: object, path: str) -> bool:
    if type(value) is not bool:
        raise ValueError(f"{path} must be boolean")
    return value


def text(value: object, path: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{path} must be nonempty text")
    return value


def hash_text(value: object, path: str) -> str:
    value = text(value, path)
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise ValueError(f"{path} must be lowercase SHA256")
    return value


def corpus_bytes() -> bytes:
    return CORPUS.read_bytes()


def validate_manifest(manifest: dict) -> None:
    require_keys(manifest, TOP_KEYS, "root")
    if manifest["schema"] != "stage6-fixed-v3":
        raise ValueError("unsupported manifest schema")

    corpus = require_keys(manifest["corpus"], CORPUS_KEYS, "corpus")
    text(corpus["file"], "corpus.file")
    hash_text(corpus["sha256"], "corpus.sha256")
    integer(corpus["size"], "corpus.size")
    if corpus["file"] != CORPUS.name or corpus["size"] != len(corpus_bytes()) or corpus["sha256"] != sha256(corpus_bytes()):
        raise ValueError("committed corpus does not match manifest")

    binary = require_keys(manifest["old_binary"], OLD_BINARY_KEYS, "old_binary")
    text(binary["name"], "old_binary.name")
    hash_text(binary["sha256"], "old_binary.sha256")
    boolean(binary["read_only"], "old_binary.read_only")
    if binary != {
        "name": EXPECTED_OLD_BINARY_NAME,
        "sha256": EXPECTED_OLD_BINARY_SHA256,
        "read_only": EXPECTED_OLD_BINARY_READ_ONLY,
    }:
        raise ValueError("historical binary identity differs from the source-pinned baseline")

    comparisons = manifest["comparisons"]
    if type(comparisons) is not list or len(comparisons) != 2:
        raise ValueError("comparisons must contain exactly m3 and m4")
    seen = set()
    for item in comparisons:
        comparison = require_keys(item, COMPARISON_KEYS, "comparison")
        method = comparison["method"]
        if method not in {"m3", "m4"} or method in seen:
            raise ValueError("comparisons must contain one m3 and one m4")
        seen.add(method)
        if comparison["layout"] != ("io-rounded" if method == "m3" else "io"):
            raise ValueError("old and new comparable layouts are not recorded correctly")
        for key in ("base_len", "seed_size", "minimum_match", "block_size"):
            integer(comparison[key], f"comparison.{method}.{key}")
        boolean(comparison["rep_overlay"], f"comparison.{method}.rep_overlay")
        if (comparison["base_len"], comparison["seed_size"], comparison["minimum_match"], comparison["block_size"], comparison["rep_overlay"]) != (16, 16, 16, 8192, False):
            raise ValueError("comparable parameters must be L=16/min=16/block=8KiB/no overlay")
        expected_old = EXPECTED_OLD_COMMON + (["-m3o"] if method == "m3" else ["-m4o"]) + ["-hash=md5", "INPUT", "OUTPUT"]
        expected_new = ["srep", "compress", "--method", method, *EXPECTED_NEW_COMMON[2:]]
        if comparison["old_command"] != expected_old or comparison["new_command"] != expected_new:
            raise ValueError(f"{method} command does not match the recorded comparable configuration")
        for command_name in ("old_command", "new_command"):
            command = comparison[command_name]
            if type(command) is not list or not all(isinstance(arg, str) and arg for arg in command):
                raise ValueError(f"comparison.{method}.{command_name} must be a nonempty string argv")

        archive = require_keys(comparison["old_archive"], ARCHIVE_KEYS, f"comparison.{method}.old_archive")
        if archive["file"] != f"stage6-old-{method}.srep":
            raise ValueError("unexpected retained old archive name")
        archive_path = FIXTURES / archive["file"]
        hash_text(archive["sha256"], f"comparison.{method}.old_archive.sha256")
        integer(archive["size"], f"comparison.{method}.old_archive.size")
        integer(archive["version"], f"comparison.{method}.old_archive.version")
        text(archive["layout"], f"comparison.{method}.old_archive.layout")
        text(archive["checksum"], f"comparison.{method}.old_archive.checksum")
        integer(archive["base_len"], f"comparison.{method}.old_archive.base_len")
        if archive["size"] != archive_path.stat().st_size or archive["sha256"] != sha256(archive_path.read_bytes()):
            raise ValueError(f"retained archive hash/size mismatch: {archive_path.name}")
        if (archive["version"], archive["layout"], archive["checksum"], archive["base_len"]) != ((1, "io-rounded", "md5", 16) if method == "m3" else (2, "io", "md5", 16)):
            raise ValueError(f"{method} retained legacy header metadata mismatch")

        old_result = require_keys(comparison["old_result"], OLD_RESULT_KEYS, f"comparison.{method}.old_result")
        new_result = require_keys(comparison["new_result"], NEW_RESULT_KEYS, f"comparison.{method}.new_result")
        for key, value in old_result.items():
            integer(value, f"comparison.{method}.old_result.{key}")
        for key, value in new_result.items():
            if key == "archive_sha256":
                hash_text(value, f"comparison.{method}.new_result.{key}")
            else:
                integer(value, f"comparison.{method}.new_result.{key}")
        if new_result["covered_bytes"] < old_result["covered_bytes"]:
            raise ValueError(f"new {method} coverage does not preserve old physical coverage")
        if old_result["encoded_bytes"] != (12 if method == "m3" else 16):
            raise ValueError(f"{method} old encoded-byte metric does not match -i semantics")
        expected_old_result = (
            {"match_count": 1, "encoded_bytes": 12, "covered_bytes": 192, "literal_bytes": 48, "archive_size": 104}
            if method == "m3"
            else {"match_count": 1, "encoded_bytes": 16, "covered_bytes": 216, "literal_bytes": 24, "archive_size": 84}
        )
        expected_new_result = (
            {"raw_candidate_count": 65, "normalized_match_count": 1, "covered_bytes": 208, "literal_bytes": 32, "archive_size": 624, "archive_sha256": "717614c4f1f736304b3b06a2cf0151c8517fa9aa31771b4797ee3bc0829a1cee"}
            if method == "m3"
            else {"raw_candidate_count": 65, "normalized_match_count": 1, "covered_bytes": 216, "literal_bytes": 24, "archive_size": 600, "archive_sha256": "170bd80f061887a83cb23e2b3f0469b8b717106a0aac9c69676fcfb60421ebc4"}
        )
        if old_result != expected_old_result or new_result != expected_new_result:
            raise ValueError(f"{method} evidence metrics differ from the frozen baseline")
        expected_notes = (
            "Comparable L=16/min=16/block=8KiB I/O vector. Old -m3o is legacy v1 rounded I/O; new m3 is NG I/O. New physical coverage preserves the old semantic envelope."
            if method == "m3"
            else "Comparable L=16/min=16/block=8KiB I/O vector. The production m4 candidate proves a 32-byte backward extension, strictly greater than L."
        )
        if comparison["notes"] != expected_notes:
            raise ValueError(f"{method} evidence notes differ from the frozen baseline")
        if method == "m4":
            witness = require_keys(comparison["m4_witness"], WITNESS_KEYS, "comparison.m4.m4_witness")
            for key, value in witness.items():
                integer(value, f"comparison.m4.m4_witness.{key}")
            if witness["backward_bytes"] <= comparison["seed_size"]:
                raise ValueError("m4 witness must extend backward beyond L")
            if witness != {
                "seed_source": 32,
                "seed_target": 56,
                "backward_bytes": 32,
                "source": 0,
                "destination": 24,
                "length": 216,
            }:
                raise ValueError("m4 witness differs from the frozen baseline")
        elif comparison["m4_witness"] is not None:
            raise ValueError("m3 m4_witness must be null")
        text(comparison["notes"], f"comparison.{method}.notes")

    conformance = require_keys(manifest["conformance"], CONFORMANCE_KEYS, "conformance")
    for key in ("method", "layout", "corpus_recipe", "scope"):
        text(conformance[key], f"conformance.{key}")
    for key in ("seed_size", "minimum_match", "raw_candidate_count", "normalized_match_count", "covered_bytes", "literal_bytes"):
        integer(conformance[key], f"conformance.{key}")
    if (conformance["method"], conformance["layout"], conformance["seed_size"], conformance["minimum_match"]) != ("m3", "index", 3, 7):
        raise ValueError("invalid new-only m3 conformance vector")
    expected_conformance = {
        "method": "m3", "layout": "index", "seed_size": 3, "minimum_match": 7,
        "raw_candidate_count": 640, "normalized_match_count": 2, "covered_bytes": 117,
        "literal_bytes": 24,
        "corpus_recipe": "abc repeated 20, XYZ repeated 7, abc repeated 20",
        "scope": "new-only L=3/min=7 fixed-grid rounding conformance; not an old comparison",
    }
    if conformance != expected_conformance:
        raise ValueError("conformance differs from the frozen baseline")


def run_json(executable: Path, argument: str) -> dict:
    return json.loads(subprocess.run([os.fspath(executable), argument], check=True, capture_output=True, text=True).stdout)


def verify_new_results(manifest: dict, decoder: Path, metrics_binary: Path | None) -> None:
    example = metrics_binary or decoder.parent / "examples" / "stage6-fixed-metrics"
    if not example.exists():
        raise RuntimeError("build stage6-fixed-metrics before running evidence")
    for item in manifest["comparisons"]:
        actual = run_json(example, item["method"])
        expected = item["new_result"]
        for key in ("raw_candidate_count", "normalized_match_count", "covered_bytes", "literal_bytes"):
            if actual[key] != expected[key]:
                raise ValueError(f"new {item['method']} metric mismatch: {key}")
        if item["method"] == "m4" and actual.get("m4_witness") != item["m4_witness"]:
            raise ValueError("m4 production witness mismatch")
    actual = run_json(example, "m3-conformance")
    expected = manifest["conformance"]
    for key in ("raw_candidate_count", "normalized_match_count", "covered_bytes", "literal_bytes"):
        if actual[key] != expected[key]:
            raise ValueError(f"m3 conformance metric mismatch: {key}")


def info_values(decoder: Path, archive: Path) -> dict[str, int]:
    info = subprocess.run([os.fspath(decoder), "info", os.fspath(archive)], check=True, capture_output=True, text=True).stdout
    result = {}
    for label in ("original size", "blocks", "semantic matches", "covered bytes", "literal bytes"):
        match = re.search(rf"^{re.escape(label)}: (\d+)$", info, re.MULTILINE)
        if match:
            result[label] = int(match.group(1))
    return result


def verify_archive(decoder: Path, archive: Path, expected: dict, data: bytes, directory: Path) -> None:
    output = directory / f"decoded-{archive.stem}.bin"
    subprocess.run([os.fspath(decoder), "decompress", os.fspath(archive), os.fspath(output)], check=True, capture_output=True)
    if output.read_bytes() != data:
        raise ValueError(f"decoder round trip mismatch: {archive.name}")
    values = info_values(decoder, archive)
    expected_values = {
        "original size": len(data), "blocks": 1, "semantic matches": expected["match_count"],
        "covered bytes": expected["covered_bytes"], "literal bytes": expected["literal_bytes"],
    }
    if values != expected_values:
            raise ValueError(f"retained archive metadata mismatch: {archive.name}: {values}")


def verify_legacy_header(archive: Path, expected: dict) -> None:
    bytes_ = archive.read_bytes()
    if bytes_[:8] != bytes((0x17, 0x18, 0x35, 0x26, 0x53, 0x52, 0x45, 0x50)):
        raise ValueError(f"invalid legacy signature: {archive.name}")
    packed = int.from_bytes(bytes_[8:12], "little")
    version = packed & 0xff
    checksum = {0: "md5", 1: "none", 2: "sha1", 3: "sha512", 4: "vhash", 5: "siphash"}.get((packed >> 8) & 0xff)
    layout = {1: "io-rounded", 2: "io", 3: "future", 4: "index"}.get(version)
    base_len = int.from_bytes(bytes_[12:16], "little")
    if (version, layout, checksum, base_len) != (
        expected["version"], expected["layout"], expected["checksum"], expected["base_len"]
    ):
        raise ValueError(f"legacy header mismatch: {archive.name}")


def verify_new_archive(decoder: Path, item: dict, data: bytes, directory: Path) -> None:
    source = directory / f"new-{item['method']}.bin"
    archive = directory / f"new-{item['method']}.srep"
    source.write_bytes(data)
    command = [os.fspath(decoder), *item["new_command"][1:]]
    command = [arg.replace("INPUT", os.fspath(source)).replace("OUTPUT", os.fspath(archive)) for arg in command]
    subprocess.run(command, check=True, capture_output=True)
    values = info_values(decoder, archive)
    expected = item["new_result"]
    if values != {"original size": len(data), "blocks": 1, "semantic matches": expected["normalized_match_count"], "covered bytes": expected["covered_bytes"], "literal bytes": expected["literal_bytes"]}:
        raise ValueError(f"new archive metadata mismatch: {item['method']}: {values}")
    if archive.stat().st_size != expected["archive_size"]:
        raise ValueError(f"new archive size mismatch: {item['method']}")
    if sha256(archive.read_bytes()) != expected["archive_sha256"]:
        raise ValueError(f"new archive hash mismatch: {item['method']}")
    verify_archive(decoder, archive, {"match_count": expected["normalized_match_count"], "covered_bytes": expected["covered_bytes"], "literal_bytes": expected["literal_bytes"]}, data, directory)


def verify_old(binary: Path, decoder: Path, manifest: dict, directory: Path) -> None:
    if sha256(binary.read_bytes()) != EXPECTED_OLD_BINARY_SHA256:
        raise ValueError("historical binary SHA256 does not match the source-pinned baseline")
    data = corpus_bytes()
    for item in manifest["comparisons"]:
        source = directory / f"old-{item['method']}.bin"
        archive = directory / f"old-{item['method']}.srep"
        source.write_bytes(data)
        command = [os.fspath(binary), *item["old_command"][1:]]
        command = [arg.replace("INPUT", os.fspath(source)).replace("OUTPUT", os.fspath(archive)) for arg in command]
        subprocess.run(command, check=True, capture_output=True)
        if sha256(archive.read_bytes()) != item["old_archive"]["sha256"]:
            raise ValueError(f"generated old archive is not byte-identical: {item['method']}")
        inspection_result = subprocess.run(
            [os.fspath(binary), "-i", os.fspath(archive)],
            check=True,
            capture_output=True,
            text=True,
        )
        inspection = inspection_result.stdout + inspection_result.stderr
        match = re.search(r"(\d+) matches = (\d+) bytes =", inspection)
        expected = item["old_result"]
        if not match or int(match.group(1)) != expected["match_count"] or int(match.group(2)) != expected["encoded_bytes"]:
            raise ValueError(f"old inspection mismatch: {item['method']}")
        verify_archive(decoder, archive, expected, data, directory)


def tamper_test(manifest: dict) -> int:
    scalars: list[tuple[list[object], object]] = []

    def collect(value: object, path: list[object]) -> None:
        if isinstance(value, dict):
            for key, child in value.items():
                collect(child, path + [key])
        elif isinstance(value, list):
            for index, child in enumerate(value):
                collect(child, path + [index])
        else:
            scalars.append((path, value))

    collect(manifest, [])
    for path, value in scalars:
        mutated = copy.deepcopy(manifest)
        target = mutated
        for component in path[:-1]:
            target = target[component]
        if isinstance(value, bool):
            replacement = not value
        elif isinstance(value, int):
            replacement = value + 1
        elif isinstance(value, str):
            if path[-1] in {"sha256", "archive_sha256"}:
                replacement = "0" * 64
                if replacement == value:
                    replacement = "1" * 64
            else:
                replacement = value + "-tampered"
        elif value is None:
            replacement = {}
        else:
            replacement = None
        target[path[-1]] = replacement
        try:
            validate_manifest(mutated)
        except (ValueError, KeyError, TypeError, AssertionError):
            continue
        raise ValueError(f"manifest scalar tamper was accepted: {'.'.join(map(str, path))}")
    print(f"rejected scalar tampering for {len(scalars)} leaves")
    return len(scalars)


def wrong_binary_online_test(decoder: Path, manifest: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="srep-stage6-wrong-binary-") as directory_name:
        directory = Path(directory_name)
        marker = directory / "executed"
        binary = directory / "wrong-old-binary"
        binary.write_text(
            "#!/usr/bin/env python3\n"
            "from pathlib import Path\n"
            f"Path({str(marker)!r}).write_text('executed', encoding='utf-8')\n",
            encoding="utf-8",
        )
        binary.chmod(0o700)
        result = subprocess.run(
            [
                sys.executable,
                os.fspath(Path(__file__).resolve()),
                "--decoder",
                os.fspath(decoder),
                "--old-binary",
                os.fspath(binary),
                "--manifest",
                os.fspath(manifest),
                "--skip-self-tests",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            raise ValueError("wrong historical binary was accepted")
        if marker.exists():
            raise ValueError("wrong historical binary was invoked before hash rejection")
    print("rejected wrong online binary before invocation")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--decoder", type=Path, default=DECODER_DEFAULT)
    parser.add_argument("--metrics-binary", type=Path)
    parser.add_argument("--old-binary", type=Path)
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--skip-self-tests", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    manifest = strict_load(args.manifest)
    validate_manifest(manifest)
    if not args.skip_self_tests:
        assert_duplicate_key_rejected()
        tamper_test(manifest)
        wrong_binary_online_test(args.decoder, args.manifest)
    data = corpus_bytes()
    with tempfile.TemporaryDirectory(prefix="srep-stage6-fixed-") as directory_name:
        directory = Path(directory_name)
        verify_new_results(manifest, args.decoder, args.metrics_binary)
        for item in manifest["comparisons"]:
            old_archive = FIXTURES / item["old_archive"]["file"]
            verify_legacy_header(old_archive, item["old_archive"])
            verify_archive(args.decoder, old_archive, item["old_result"], data, directory)
            verify_new_archive(args.decoder, item, data, directory)
        if args.old_binary:
            verify_old(args.old_binary, args.decoder, manifest, directory)
    print("validated strict Stage 6 comparable evidence, retained archives, headers, metrics, witness, tamper resistance, and round trips")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

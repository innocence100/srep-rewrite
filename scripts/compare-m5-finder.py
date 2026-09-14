#!/usr/bin/env python3
"""Strict Stage 7 M5 evidence gate; old binary use is optional and read-only."""
from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import re
import subprocess
import struct
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures" / "fidelity"
MANIFEST = FIXTURES / "stage7-m5.json"
CORPUS = FIXTURES / "stage7-m5-comparable.bin"
OLD_ARCHIVE = FIXTURES / "stage7-old-m5.srep"
DECODER_DEFAULT = ROOT / "target" / "debug" / "srep"
EXPECTED_BINARY = "e8ca47d05ecceb7f3c5ff3fa6d4c8cdc88f17b3f3f1838e6ca06b3c0cded7b1e"


def current_command(decoder: Path, item: dict, source: Path, archive: Path) -> list[str]:
    """Replay the recorded compression options through the current NGv3 CLI."""
    command = [os.fspath(decoder), *item["new_command"][1:]]
    return [
        arg.replace("INPUT", os.fspath(source)).replace("OUTPUT", os.fspath(archive))
        for arg in command
    ]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def load(path: Path) -> dict:
    def pairs(items: list[tuple[str, object]]) -> dict:
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result
    value = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=pairs)
    if not isinstance(value, dict):
        raise ValueError("manifest root must be an object")
    return value


def require(value: object, keys: set[str], path: str) -> dict:
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError(f"{path} keys differ")
    return value


def integer(value: object, path: str) -> int:
    if type(value) is not int or value < 0:
        raise ValueError(f"{path} must be a nonnegative integer")
    return value


def text(value: object, path: str) -> str:
    if not isinstance(value, str) or not value:
        raise ValueError(f"{path} must be nonempty text")
    return value


def digest(value: object, path: str) -> str:
    value = text(value, path)
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise ValueError(f"{path} must be lowercase SHA256")
    return value


def validate(manifest: dict, expected_old_result: dict[str, int] | None = None) -> None:
    root = require(manifest, {"schema", "corpus", "witness_corpus", "old_binary", "comparison"}, "root")
    if root["schema"] != "stage7-m5-v1":
        raise ValueError("unsupported Stage7 manifest schema")
    corpus = require(root["corpus"], {"file", "sha256", "size"}, "corpus")
    if corpus["file"] != CORPUS.name or corpus["size"] != CORPUS.stat().st_size or corpus["sha256"] != sha256(CORPUS.read_bytes()):
        raise ValueError("corpus does not match manifest")
    witness_corpus = require(root["witness_corpus"], {"file", "sha256", "size"}, "witness_corpus")
    if witness_corpus["file"] != "stage7-m5.bin":
        raise ValueError("unexpected witness corpus file")
    witness_path = FIXTURES / witness_corpus["file"]
    if not witness_path.is_file() or witness_corpus["size"] != witness_path.stat().st_size or witness_corpus["sha256"] != sha256(witness_path.read_bytes()):
        raise ValueError("witness corpus does not match manifest")
    binary = require(root["old_binary"], {"name", "sha256", "read_only"}, "old_binary")
    if binary != {"name": "SREP 3.93a beta", "sha256": EXPECTED_BINARY, "read_only": True}:
        raise ValueError("historical binary identity differs from source pin")
    item = require(root["comparison"], {"method", "layout", "old_minimum_match", "old_seed_size", "old_header_base_len", "minimum_match", "derived_seed_size", "block_size", "rep_overlay", "old_command", "new_command", "old_archive", "forced_m4_archive", "old_result", "new_result", "m5_witness", "notes"}, "comparison")
    if item["method"] != "m5" or item["layout"] != "io" or item["old_minimum_match"] != 16 or item["old_seed_size"] != 8 or item["old_header_base_len"] != 16 or item["minimum_match"] != 16 or item["derived_seed_size"] != 8 or item["block_size"] != 8192 or item["rep_overlay"] is not False:
        raise ValueError("M5 comparable parameters differ")
    for key in ("old_minimum_match", "old_seed_size", "old_header_base_len", "minimum_match", "derived_seed_size", "block_size"):
        integer(item[key], f"comparison.{key}")
    for key in ("old_command", "new_command"):
        if not isinstance(item[key], list) or not item[key] or not all(isinstance(arg, str) and arg for arg in item[key]):
            raise ValueError(f"comparison.{key} must be argv")
    if item["old_command"] != ["srep", "-v0", "-b8k", "-l16", "-d0", "-m5o", "-hash=md5", "INPUT", "OUTPUT"]:
        raise ValueError("old M5 command differs from the source-pinned vector")
    if item["new_command"] != ["srep", "compress", "--method", "m5", "--layout", "io", "--checksum", "xxh3", "--block-size", "8KiB", "--min-match", "16", "INPUT", "OUTPUT"]:
        raise ValueError("new M5 command differs from the source-pinned vector")
    old = require(item["old_archive"], {"file", "sha256", "size", "version", "layout", "checksum", "base_len"}, "old_archive")
    if old["file"] != OLD_ARCHIVE.name or old["size"] != OLD_ARCHIVE.stat().st_size or old["sha256"] != sha256(OLD_ARCHIVE.read_bytes()) or (old["version"], old["layout"], old["checksum"], old["base_len"]) != (2, "io", "md5", item["old_header_base_len"]):
        raise ValueError("retained old archive differs from manifest")
    forced_m4 = require(item["forced_m4_archive"], {"file", "sha256", "size", "match_count"}, "forced_m4_archive")
    forced_m4_path = FIXTURES / forced_m4["file"]
    if forced_m4["file"] != "stage7-old-m4-comparable.srep" or not forced_m4_path.is_file() or forced_m4["size"] != forced_m4_path.stat().st_size or forced_m4["sha256"] != sha256(forced_m4_path.read_bytes()) or forced_m4["match_count"] != 0 or forced_m4["sha256"] == old["sha256"]:
        raise ValueError("forced-L M4 discriminator differs from the frozen baseline")
    old_result = require(item["old_result"], {"match_count", "encoded_bytes", "covered_bytes", "literal_bytes", "archive_size"}, "old_result")
    new_result = require(item["new_result"], {"raw_candidate_count", "normalized_match_count", "covered_bytes", "literal_bytes", "archive_size", "archive_sha256"}, "new_result")
    for key, value in old_result.items(): integer(value, f"old_result.{key}")
    for key, value in new_result.items(): digest(value, f"new_result.{key}") if key.endswith("sha256") else integer(value, f"new_result.{key}")
    if old_result["encoded_bytes"] != 16:
        raise ValueError("old M5 encoded-byte metric differs from the frozen baseline")
    if expected_old_result is not None and old_result != expected_old_result:
        raise ValueError(f"old M5 result differs from decoded retained archive: expected {expected_old_result}, got {old_result}")
    if old_result["covered_bytes"] + old_result["literal_bytes"] != corpus["size"]:
        raise ValueError("old decoded semantic coverage and literals do not sum to corpus size")
    if new_result != {"raw_candidate_count": 1, "normalized_match_count": 1, "covered_bytes": 32, "literal_bytes": 46, "archive_size": 638, "archive_sha256": "46cd35b4cc82fe76355e9b646c9c7696c59b259cb235e60c0da7270fea94fb90"}:
        raise ValueError("new M5 result differs from the frozen baseline")
    witness = require(item["m5_witness"], {"source", "destination", "length", "nonaligned_target"}, "m5_witness")
    for key in ("source", "destination", "length"): integer(witness[key], f"m5_witness.{key}")
    if witness["nonaligned_target"] is not True or witness["destination"] % item["derived_seed_size"] == 0:
        raise ValueError("M5 witness is not nonaligned")
    if witness != {"source": 32, "destination": 35, "length": 15, "nonaligned_target": True}:
        raise ValueError("M5 witness differs from the frozen baseline")
    if item["notes"] != "Directed Stage7 M5 comparison. Old SREP 3.93a uses derived L=8 from -l16 with no -c override; the legacy BASE_LEN header field is a separate decoder minimum field and remains 16. New M5 uses normative derived L=8/minimum=16. Both archives round-trip. The old derived-L M5 vector differs from forced-L M4 on this discriminator corpus; this is comparable parameter evidence, not a full fidelity claim.":
        raise ValueError("comparison notes differ from the frozen baseline")


def tamper_test(manifest: dict, decoder: Path) -> int:
    leaves = []
    def visit(value: object, path: list[object]) -> None:
        if isinstance(value, dict):
            for key, child in value.items(): visit(child, path + [key])
        elif isinstance(value, list):
            for index, child in enumerate(value): visit(child, path + [index])
        else: leaves.append((path, value))
    visit(manifest, [])
    expected_old_result = computed_old_result(decoder)
    for path, value in leaves:
        mutated = copy.deepcopy(manifest)
        target = mutated
        for part in path[:-1]: target = target[part]
        target[path[-1]] = (not value if isinstance(value, bool) else value + 1 if isinstance(value, int) else "tampered" if isinstance(value, str) else {})
        try: validate(mutated, expected_old_result)
        except (ValueError, KeyError, TypeError, OSError): continue
        raise ValueError(f"accepted scalar tamper at {path}")
    return len(leaves)


def legacy_encoded_bytes(archive: Path) -> int:
    data = archive.read_bytes()
    packed = struct.unpack_from("<I", data, 8)[0]
    version = packed & 0xff
    checksum_id = (packed >> 8) & 0xff
    digest_len = {0: 16, 1: 16, 2: 20, 3: 64, 4: 16, 5: 8}[checksum_id]
    seed_len = {0: 0, 1: 0, 2: 0, 3: 0, 4: 32, 5: 16}[checksum_id]
    position = 16 + seed_len
    total = 0
    while position < len(data):
        if len(data) - position == 8 and data[position:] == bytes(8):
            break
        if position + 12 + digest_len > len(data):
            raise ValueError("retained legacy archive body is truncated")
        _, _, inline_stat = struct.unpack_from("<III", data, position)
        if version == 4:
            raise ValueError("encoded-byte parser does not support v4 evidence")
        literal_start = position + 12 + digest_len + inline_stat
        if literal_start > len(data):
            raise ValueError("retained legacy archive statistics exceed archive")
        literal_bytes = struct.unpack_from("<I", data, position)[0]
        next_position = literal_start + literal_bytes
        if next_position > len(data):
            raise ValueError("retained legacy archive literals exceed archive")
        total += inline_stat
        position = next_position
    return total


def computed_old_result(decoder: Path) -> dict[str, int]:
    values = info(decoder, OLD_ARCHIVE)
    return {
        "match_count": values["semantic matches"],
        "encoded_bytes": legacy_encoded_bytes(OLD_ARCHIVE),
        "covered_bytes": values["covered bytes"],
        "literal_bytes": values["literal bytes"],
        "archive_size": OLD_ARCHIVE.stat().st_size,
    }


def info(decoder: Path, archive: Path) -> dict[str, int]:
    output = subprocess.run([os.fspath(decoder), "info", os.fspath(archive)], check=True, capture_output=True, text=True).stdout
    result = {}
    for label in ("original size", "blocks", "semantic matches", "covered bytes", "literal bytes"):
        match = re.search(rf"^{re.escape(label)}: (\d+)$", output, re.MULTILINE)
        if match: result[label] = int(match.group(1))
    return result


def roundtrip(decoder: Path, archive: Path, data: bytes, expected: dict[str, int], directory: Path) -> None:
    output = directory / f"{archive.stem}.out"
    subprocess.run([os.fspath(decoder), "decompress", os.fspath(archive), os.fspath(output)], check=True, capture_output=True)
    semantic_matches = expected["normalized_match_count"] if "normalized_match_count" in expected else expected["match_count"]
    expected_info = {
        "original size": len(data),
        "blocks": 1,
        "semantic matches": semantic_matches,
        "covered bytes": expected["covered_bytes"],
        "literal bytes": expected["literal_bytes"],
    }
    if output.read_bytes() != data or info(decoder, archive) != expected_info:
        raise ValueError(f"round-trip or metrics mismatch for {archive.name}")


def decoded_old_result(decoder: Path, archive: Path, expected: dict[str, int], data: bytes) -> None:
    values = info(decoder, archive)
    actual = {
        "match_count": values.get("semantic matches"),
        "covered_bytes": values.get("covered bytes"),
        "literal_bytes": values.get("literal bytes"),
        "archive_size": archive.stat().st_size,
    }
    if actual != expected:
        raise ValueError(f"decoded old semantic metrics differ: expected {expected}, got {actual}")
    if expected["covered_bytes"] + expected["literal_bytes"] != len(data):
        raise ValueError("decoded old coverage and literals do not sum to input size")


def wrong_binary_online_test(decoder: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="srep-stage7-wrong-binary-") as name:
        directory = Path(name)
        marker = directory / "invoked"
        wrong = directory / "wrong-old-binary"
        wrong.write_text(
            "#!/usr/bin/env python3\n"
            f"from pathlib import Path\nPath({str(marker)!r}).write_text('invoked')\n",
            encoding="utf-8",
        )
        wrong.chmod(0o700)
        result = subprocess.run(
            [sys.executable, os.fspath(Path(__file__).resolve()), "--decoder", os.fspath(decoder), "--old-binary", os.fspath(wrong), "--skip-self-tests"],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0 or marker.exists():
            raise ValueError("wrong historical binary was accepted or invoked")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--decoder", type=Path, default=DECODER_DEFAULT)
    parser.add_argument("--old-binary", type=Path)
    parser.add_argument("--metrics-binary", type=Path)
    parser.add_argument("--skip-self-tests", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    manifest = load(MANIFEST)
    expected_old_result = computed_old_result(args.decoder)
    validate(manifest, expected_old_result)
    if not args.skip_self_tests:
        print(f"rejected scalar tampering for {tamper_test(manifest, args.decoder)} leaves")
        wrong_binary_online_test(args.decoder)
    data = CORPUS.read_bytes()
    item = manifest["comparison"]
    with tempfile.TemporaryDirectory(prefix="srep-stage7-m5-") as name:
        directory = Path(name)
        roundtrip(args.decoder, OLD_ARCHIVE, data, item["old_result"], directory)
        decoded_old_result(
            args.decoder,
            OLD_ARCHIVE,
            {
                "match_count": item["old_result"]["match_count"],
                "covered_bytes": item["old_result"]["covered_bytes"],
                "literal_bytes": item["old_result"]["literal_bytes"],
                "archive_size": item["old_result"]["archive_size"],
            },
            data,
        )
        source = directory / "new-m5.bin"
        archive = directory / "new-m5.srep"
        source.write_bytes(data)
        subprocess.run(current_command(args.decoder, item, source, archive), check=True, capture_output=True)
        if archive.read_bytes()[:8] != b"SREPNG3\0":
            raise ValueError("current M5 command did not emit NGv3")
        info_text = subprocess.run(
            [os.fspath(args.decoder), "info", os.fspath(archive)],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        if "format: SREP-NG v3" not in info_text:
            raise ValueError("current M5 archive was not labeled NGv3")
        roundtrip(args.decoder, archive, data, item["new_result"], directory)
        metrics = args.metrics_binary or args.decoder.parent / "examples" / "stage7-m5-metrics"
        comparable = json.loads(subprocess.run([os.fspath(metrics), "m5-comparable"], check=True, capture_output=True, text=True).stdout)
        if comparable["raw_candidate_count"] != item["new_result"]["raw_candidate_count"] or comparable["normalized_match_count"] != item["new_result"]["normalized_match_count"] or comparable["covered_bytes"] != item["new_result"]["covered_bytes"] or comparable["literal_bytes"] != item["new_result"]["literal_bytes"]:
            raise ValueError("comparable M5 metrics example differs from manifest")
        witness = json.loads(subprocess.run([os.fspath(metrics), "m5"], check=True, capture_output=True, text=True).stdout)
        if witness.get("m5_witness") != item["m5_witness"]:
            raise ValueError("nonaligned M5 witness differs from manifest")
        if item["new_result"]["covered_bytes"] < item["old_result"]["covered_bytes"]:
            raise ValueError("new M5 coverage is below old decoded semantic coverage")
        if args.old_binary:
            if sha256(args.old_binary.read_bytes()) != EXPECTED_BINARY:
                raise ValueError("wrong historical binary rejected before invocation")
            old_source = directory / "old-m5.bin"
            old_archive = directory / "old-m5.srep"
            old_source.write_bytes(data)
            old_command = [os.fspath(args.old_binary), *item["old_command"][1:]]
            old_command = [arg.replace("INPUT", os.fspath(old_source)).replace("OUTPUT", os.fspath(old_archive)) for arg in old_command]
            subprocess.run(old_command, check=True, capture_output=True)
            if old_archive.read_bytes() != OLD_ARCHIVE.read_bytes():
                raise ValueError("old online reproduction differs from retained archive")
            old_process = subprocess.run(
                [os.fspath(args.old_binary), "-i", os.fspath(old_archive)],
                check=True,
                capture_output=True,
                text=True,
            )
            old_info = old_process.stdout + old_process.stderr
            match = re.search(r"(\d+) matches = (\d+) bytes =", old_info)
            if not match or int(match.group(1)) != item["old_result"]["match_count"] or int(match.group(2)) != item["old_result"]["encoded_bytes"]:
                raise ValueError("old online physical match metrics differ from manifest")
            print("old binary hash accepted and online reproduction matched retained archive")
    print("validated Stage 7 historical fixture bytes offline plus current NGv3 semantic metrics, witness, tamper resistance, and round trips; no NGv2 output compatibility claimed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

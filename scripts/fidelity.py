#!/usr/bin/env python3
"""Hermetic, versioned old/new SREP capability-fidelity evidence.

The corpus contains recipes, not checked-in sample bytes.  Baseline and evaluation
material is retained below /tmp/opencode by default; only the reviewed manifests
and machine-readable report are repository artifacts.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import re
import secrets
import signal
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

from fidelity_baseline_pin import BASELINE_MANIFEST_SHA256, CORPUS_MANIFEST_SHA256

ROOT = Path(__file__).resolve().parents[1]
CORPUS_DIR = ROOT / "tests" / "fixtures" / "fidelity" / "corpus"
CORPUS_MANIFEST = CORPUS_DIR / "manifest.json"
BASELINE_MANIFEST = CORPUS_DIR / "baseline-manifest.json"
OLD_BINARY_SHA256 = "e8ca47d05ecceb7f3c5ff3fa6d4c8cdc88f17b3f3f1838e6ca06b3c0cded7b1e"
REQUIRED_CATEGORIES = {
    "exact", "nonaligned", "insert-delete", "order1", "far-distance",
    "multiversion-tree", "vm-disk-sparse", "compressed-block-repeat",
    "random-incompressible",
}
METHODS = tuple(f"m{i}" for i in range(6))
DEFAULT_COMMAND_TIMEOUT = 180
_REAP_GRACE_SECONDS = 2.0
XZ_ARGS = [
    "xz", "--format=raw", "--threads=1",
    "--lzma2=dict=64MiB,lc=3,lp=0,pb=2,mode=normal,nice=273,mf=bt4",
]
CORPUS_VERSION = "fidelity-corpus-v2"
MIN_REPRESENTATIVE_SIZE = 256 * 1024
FAR_PREFIX_SIZE = 256 * 1024
FAR_TOTAL_SIZE = 2 * 1024 * 1024
# The production methods support complete history by default.  This explicit
# representative window keeps the corpus category useful as a local-history
# distance stress test: the repeated 256 KiB prefix is separated by 1.5 MiB,
# which is strictly beyond this declared 256 KiB local window.
LOCAL_HISTORY_THRESHOLD = 256 * 1024
SPARSE_STRIDE = 128 * 1024
COMPRESSED_BLOCK_SIZE = 64 * 1024
COMPRESSED_BLOCK_HEADER_SIZE = 16
MIN_INCOMPRESSIBLE_ENTROPY = 7.5
MAX_INCOMPRESSIBLE_BYTE_FREQUENCY = 0.02
MIN_INCOMPRESSIBLE_UNIQUE_WINDOW_BYTES = 240
MIN_INCOMPRESSIBLE_XZ_RATIO = 0.50


def _atomic_write(path: Path, content: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent, prefix=f".{path.name}.", suffix=".tmp"
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        temporary.unlink(missing_ok=True)


def _atomic_json(path: Path, value: object) -> None:
    _atomic_write(path, (json.dumps(value, indent=2) + "\n").encode("utf-8"))


def _stage_json(path: Path, value: object) -> Path:
    temporary = path.parent / f".{path.name}.{secrets.token_hex(12)}.tmp"
    _atomic_json(temporary, value)
    return temporary


def _pin_content(corpus_hash: str, baseline_hash: str) -> bytes:
    return (
        '"""Source pin for the reviewed fidelity baseline manifest."""\n\n'
        f'OLD_BINARY_SHA256 = "{OLD_BINARY_SHA256}"\n'
        "# Reviewed method-specific baseline generated from the pinned old binary.\n"
        f'BASELINE_MANIFEST_SHA256 = "{baseline_hash}"\n'
        f'CORPUS_MANIFEST_SHA256 = "{corpus_hash}"\n'
    ).encode("utf-8")


def _publish_transaction(
    corpus_temporary: Path | None,
    baseline_temporary: Path,
    corpus_hash: str,
    baseline_hash: str,
    *,
    corpus_destination: Path = CORPUS_MANIFEST,
    baseline_destination: Path = BASELINE_MANIFEST,
    pin_destination: Path = Path(__file__).with_name("fidelity_baseline_pin.py"),
    replace=os.replace,
) -> None:
    destinations = [
        (corpus_temporary, corpus_destination),
        (baseline_temporary, baseline_destination),
    ]
    pin_temporary = _stage_bytes(pin_destination, _pin_content(corpus_hash, baseline_hash))
    destinations.append((pin_temporary, pin_destination))
    backups: list[tuple[Path, Path | None]] = []
    published: list[Path] = []
    try:
        for _, destination in destinations:
            if destination.exists():
                backup = destination.parent / f".{destination.name}.{secrets.token_hex(12)}.bak"
                _atomic_write(backup, destination.read_bytes())
                backups.append((destination, backup))
            else:
                backups.append((destination, None))
        for temporary, destination in destinations:
            if temporary is not None:
                replace(temporary, destination)
                published.append(destination)
    except BaseException:
        for destination in published:
            destination.unlink(missing_ok=True)
        for destination, backup in backups:
            if backup is not None:
                 replace(backup, destination)
        raise
    finally:
        for temporary, _ in destinations:
            if temporary is not None:
                temporary.unlink(missing_ok=True)
        for _, backup in backups:
            if backup is not None:
                backup.unlink(missing_ok=True)


def _stage_bytes(path: Path, content: bytes) -> Path:
    temporary = path.parent / f".{path.name}.{secrets.token_hex(12)}.tmp"
    _atomic_write(temporary, content)
    return temporary


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def file_sha256(path: Path) -> str:
    return sha256(path.read_bytes())


def strict_json(path: Path) -> object:
    def pairs(items: list[tuple[str, object]]) -> dict[str, object]:
        result: dict[str, object] = {}
        for key, value in items:
            if key in result:
                raise ValueError(f"duplicate JSON key {key} in {path}")
            result[key] = value
        return result
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=pairs)


def deterministic_bytes(recipe: str, size: int, seed: int) -> bytes:
    state = (seed ^ 0x9E3779B97F4A7C15) & ((1 << 64) - 1)
    result = bytearray()
    for _ in range(size):
        state = (state * 6364136223846793005 + 1442695040888963407) & ((1 << 64) - 1)
        result.append((state >> 32) & 0xFF)
    if recipe == "random-incompressible":
        return bytes(result)
    return bytes(result)


def _insert_delete_parts(seed: int) -> tuple[bytes, bytes, bytes]:
    block = deterministic_bytes("insert-delete", 32768, seed)
    inserted = b"INSERTED-REGION-v2\0" + deterministic_bytes("insert-delete", 1237, seed + 1)
    deleted = 911
    source = block * 4
    shifted = source[: 2 * len(block) - deleted] + inserted + source[2 * len(block) :]
    return source, shifted, inserted


def _incompressible_stats(data: bytes) -> tuple[float, int, int, int]:
    histogram = [0] * 256
    for value in data:
        histogram[value] += 1
    total = len(data)
    entropy = -sum(
        (count / total) * math.log2(count / total)
        for count in histogram
        if count
    )
    return entropy, max(histogram), sum(count > 0 for count in histogram), len(set(data[:4096]))


def _validate_incompressible(data: bytes, label: str) -> None:
    entropy, maximum, unique, window_unique = _incompressible_stats(data)
    compressed = subprocess.run(
        [*XZ_ARGS, "-c"], input=data, check=True, capture_output=True
    ).stdout
    xz_ratio = len(compressed) / len(data)
    if (
        entropy < MIN_INCOMPRESSIBLE_ENTROPY
        or maximum / len(data) > MAX_INCOMPRESSIBLE_BYTE_FREQUENCY
        or unique != 256
        or window_unique < MIN_INCOMPRESSIBLE_UNIQUE_WINDOW_BYTES
        or xz_ratio < MIN_INCOMPRESSIBLE_XZ_RATIO
    ):
        raise ValueError(
            f"{label} lacks deterministic incompressible payload semantics: "
            f"entropy={entropy:.6f}, max_frequency={maximum}, unique={unique}, "
            f"window_unique={window_unique}, xz_ratio={xz_ratio:.6f}"
        )


def make_sample(recipe: str, params: dict[str, int]) -> bytes:
    size = params["size"]
    seed = params["seed"]
    if recipe == "exact-repeat":
        block = deterministic_bytes(recipe, 65536, seed)
        return (block * ((size + len(block) - 1) // len(block)))[:size]
    if recipe == "nonaligned-repeat":
        block = deterministic_bytes(recipe, 65536, seed)
        prefix = deterministic_bytes(recipe, 173, seed + 1)
        return (prefix + block * ((size + len(block) - len(prefix) - 1) // len(block) + 1))[:size]
    if recipe == "insert-delete":
        source, shifted, _ = _insert_delete_parts(seed)
        return (source + shifted + source)[:size].ljust(size, b"\0")
    if recipe == "order1":
        state = seed & 31
        result = bytearray()
        for index in range(size // 2):
            next_state = (state * 7 + 3 + ((index // 4096) & 3)) & 31
            result.extend((state, next_state))
            state = next_state
        return bytes(result[:size])
    if recipe == "far-distance":
        if size < FAR_TOTAL_SIZE:
            raise ValueError("far-distance representative must be at least 2 MiB")
        prefix = deterministic_bytes(recipe, FAR_PREFIX_SIZE, seed)
        middle = deterministic_bytes(recipe, size - 2 * FAR_PREFIX_SIZE, seed + 4)
        return prefix + middle + prefix
    if recipe == "multiversion-tree":
        base = deterministic_bytes(recipe, 4096, seed)
        variants = [base, bytes(value ^ 0x55 for value in base), base[1:] + base[:1]]
        snapshots = []
        for version, choices in ((1, (0, 0, 1, 0)), (2, (0, 2, 1, 0)), (3, (2, 0, 2, 1))):
            snapshot = bytearray(f"VERSION-{version:02d}\0".encode("ascii"))
            for block_id, choice in enumerate(choices * 8):
                block = variants[choice]
                snapshot.extend(b"TREE-BLOCK")
                snapshot.extend(block_id.to_bytes(2, "little"))
                snapshot.extend(len(block).to_bytes(4, "little"))
                snapshot.extend(block)
            snapshots.append(bytes(snapshot))
        return (snapshots[0] + snapshots[1] + snapshots[2] + snapshots[1])[:size].ljust(size, b"\0")
    if recipe == "vm-disk-sparse":
        result = bytearray(size)
        for offset in range(0, size, SPARSE_STRIDE):
            marker = f"VM-DISK-EXTENT-{seed:08x}-{offset:08x}\0".encode("ascii")
            result[offset:offset + len(marker)] = marker
        return bytes(result)
    if recipe == "compressed-block-repeat":
        payload_size = COMPRESSED_BLOCK_SIZE - COMPRESSED_BLOCK_HEADER_SIZE
        payload = deterministic_bytes(recipe, payload_size, seed)
        block = b"CBLK" + b"OPAQ" + payload_size.to_bytes(4, "little") + COMPRESSED_BLOCK_SIZE.to_bytes(4, "little") + payload
        return (block * ((size + len(block) - 1) // len(block)))[:size]
    if recipe == "random-incompressible":
        return deterministic_bytes(recipe, size, seed)
    raise ValueError(f"unknown corpus recipe {recipe}")


def recipes() -> list[dict[str, object]]:
    names = [
        "exact-repeat", "nonaligned-repeat", "insert-delete", "order1",
        "far-distance", "multiversion-tree", "vm-disk-sparse",
        "compressed-block-repeat", "random-incompressible", "exact-repeat",
        "nonaligned-repeat", "multiversion-tree",
    ]
    categories = [
        ["exact"], ["nonaligned"], ["insert-delete"], ["order1"],
        ["far-distance"], ["multiversion-tree"], ["vm-disk-sparse"],
        ["compressed-block-repeat"], ["random-incompressible"],
        ["exact"], ["nonaligned"], ["multiversion-tree"],
    ]
    result = []
    for index, (name, category) in enumerate(zip(names, categories)):
        sizes = [262144, 524288, 524288, 524288, FAR_TOTAL_SIZE, 524288, 524288, 524288, 262144, 524288, 524288, 786432]
        size = sizes[index]
        result.append({
            "id": f"fidelity-v1-{index + 1:02d}",
            "categories": category,
            "methods": list(METHODS),
            "representative": True,
            "generator": "scripts/fidelity.py:make_sample",
            "version": "2",
            "args": {"recipe": name, "size": size, "seed": 0x4100 + index},
            "sha256": sha256(make_sample(name, {"size": size, "seed": 0x4100 + index})),
            "size": size,
        })
    return result


def ensure_corpus_manifest(*, regenerate: bool = False) -> dict[str, object]:
    CORPUS_DIR.mkdir(parents=True, exist_ok=True)
    expected = {"schema": CORPUS_VERSION, "samples": recipes()}
    if not CORPUS_MANIFEST.exists() and not regenerate:
        raise RuntimeError("manifest.json is missing; pass --regenerate-corpus explicitly")
    if CORPUS_MANIFEST.exists() and not regenerate:
        actual = strict_json(CORPUS_MANIFEST)
        if actual != expected:
            raise RuntimeError("frozen corpus manifest differs from deterministic recipes")
        validate_corpus(actual)
        return actual
    if regenerate:
        temporary = stage_corpus_manifest(expected)
        os.replace(temporary, CORPUS_MANIFEST)
        return expected
    raise AssertionError("unreachable corpus manifest state")


def stage_corpus_manifest(manifest: dict[str, object]) -> Path:
    validate_corpus(manifest, enforce_pin=False)
    temporary = _stage_json(CORPUS_MANIFEST, manifest)
    try:
        validate_corpus(
            strict_json(temporary), enforce_pin=False, manifest_path=temporary
        )
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    return temporary


def validate_corpus(
    manifest: dict[str, object], *, enforce_pin: bool = True, manifest_path: Path = CORPUS_MANIFEST
) -> None:
    if manifest.get("schema") != CORPUS_VERSION or not isinstance(manifest.get("samples"), list):
        raise ValueError("invalid corpus manifest schema")
    samples = manifest["samples"]
    seen: set[str] = set()
    category_pairs: set[tuple[str, str]] = set()
    for sample in samples:
        if not isinstance(sample, dict):
            raise ValueError("corpus sample is not an object")
        required = {"id", "categories", "methods", "representative", "generator", "version", "args", "sha256", "size"}
        if set(sample) != required:
            raise ValueError(f"corpus fields differ for {sample.get('id')}")
        sample_id = sample["id"]
        if not isinstance(sample_id, str) or not sample_id or sample_id in seen:
            raise ValueError("corpus IDs must be unique nonempty strings")
        seen.add(sample_id)
        categories = sample["categories"]
        methods = sample["methods"]
        if not isinstance(categories, list) or not categories or not set(categories) <= REQUIRED_CATEGORIES:
            raise ValueError(f"invalid categories for {sample_id}")
        if methods != list(METHODS) or sample["representative"] is not True:
            raise ValueError(f"all frozen corpus samples must cover m0-m5: {sample_id}")
        args = sample["args"]
        if not isinstance(args, dict) or set(args) != {"recipe", "size", "seed"}:
            raise ValueError(f"invalid generator args for {sample_id}")
        data = make_sample(str(args["recipe"]), {"size": int(args["size"]), "seed": int(args["seed"])})
        if len(data) != sample["size"] or sha256(data) != sample["sha256"]:
            raise ValueError(f"sample bytes differ from frozen manifest: {sample_id}")
        validate_recipe_structure(str(args["recipe"]), data, int(args["seed"]))
        for method in methods:
            for category in categories:
                category_pairs.add((method, category))
    if len(samples) < 12:
        raise ValueError("fidelity corpus needs at least 12 frozen representatives")
    if REQUIRED_CATEGORIES - {category for _, category in category_pairs}:
        raise ValueError("fidelity corpus does not represent every required category")
    if any((method, category) not in category_pairs for method in METHODS for category in REQUIRED_CATEGORIES):
        raise ValueError("every method/category pair must be represented")
    if enforce_pin and manifest_path.exists() and file_sha256(manifest_path) != CORPUS_MANIFEST_SHA256:
        raise ValueError("corpus manifest does not match the source pin")


def validate_recipe_structure(recipe: str, data: bytes, seed: int) -> None:
    if len(data) < MIN_REPRESENTATIVE_SIZE:
        raise ValueError(f"{recipe} representative is below the 256 KiB minimum")
    if recipe == "exact-repeat":
        block = 65536
        if len(data) < 4 * block or any(data[offset : offset + block] != data[:block] for offset in range(block, 4 * block, block)):
            raise ValueError("exact-repeat lacks four exact block copies")
    elif recipe == "nonaligned-repeat":
        block = 65536
        if len(data) < 173 + 4 * block or any(data[173 + offset : 173 + offset + block] != data[173 : 173 + block] for offset in range(block, 4 * block, block)):
            raise ValueError("nonaligned-repeat lacks four shifted block copies")
    elif recipe == "insert-delete":
        source, shifted, inserted = _insert_delete_parts(seed)
        core = source + shifted + source
        if len(data) < len(core) or data[: len(core)] != core or any(data[len(core) :]):
            raise ValueError("insert-delete lacks the deterministic base/insert/shift relationship")
        if data.count(b"INSERTED-REGION-v2\0") != 1 or inserted not in shifted:
            raise ValueError("insert-delete lacks its unique insertion marker")
    elif recipe == "order1":
        pairs = list(zip(data[::2], data[1::2]))
        if len(set(pairs)) < 8 or any(pairs[index][1] != pairs[index + 1][0] for index in range(len(pairs) - 1)):
            raise ValueError("order1 sample is not a chained first-order transition stream")
    elif recipe == "far-distance":
        if len(data) < FAR_TOTAL_SIZE or data[:FAR_PREFIX_SIZE] != data[-FAR_PREFIX_SIZE:]:
            raise ValueError("far-distance sample lacks separated repeated prefix")
        target = len(data) - FAR_PREFIX_SIZE
        prefix = data[:FAR_PREFIX_SIZE]
        nearest_prior = data.rfind(prefix, 0, target)
        if nearest_prior < 0 or target - nearest_prior <= LOCAL_HISTORY_THRESHOLD:
            raise ValueError("far-distance repeat is not beyond the declared local history threshold")
        if data.find(prefix, 1, target) != -1:
            raise ValueError("far-distance sample contains an accidental middle copy")
    elif recipe == "multiversion-tree":
        if not all(data.count(marker) >= 1 for marker in (b"VERSION-01\0", b"VERSION-02\0", b"VERSION-03\0")) or data.count(b"TREE-BLOCK") < 16:
            raise ValueError("multiversion-tree sample lacks versioned tree records")
    elif recipe == "vm-disk-sparse":
        expected = bytearray(len(data))
        offsets = list(range(0, len(data), SPARSE_STRIDE))
        for offset in offsets:
            marker = f"VM-DISK-EXTENT-{seed:08x}-{offset:08x}\0".encode("ascii")
            expected[offset : offset + len(marker)] = marker
        if data != bytes(expected) or len(offsets) < 4:
            raise ValueError("vm-disk-sparse sample lacks exact extent spacing and markers")
        if data.count(0) / len(data) < 0.95:
            raise ValueError("vm-disk-sparse sample is not predominantly zero-filled")
    elif recipe == "compressed-block-repeat":
        if len(data) % COMPRESSED_BLOCK_SIZE != 0 or len(data) < 4 * COMPRESSED_BLOCK_SIZE:
            raise ValueError("compressed-block-repeat lacks repeated framed blocks")
        blocks = [data[offset : offset + COMPRESSED_BLOCK_SIZE] for offset in range(0, len(data), COMPRESSED_BLOCK_SIZE)]
        if len(set(blocks)) != 1:
            raise ValueError("compressed-block-repeat blocks are not byte-identical")
        block = blocks[0]
        magic, marker, payload_size, original_size = struct.unpack_from("<4s4sII", block)
        if magic != b"CBLK" or marker != b"OPAQ" or payload_size != len(block) - COMPRESSED_BLOCK_HEADER_SIZE or original_size != COMPRESSED_BLOCK_SIZE:
            raise ValueError("compressed-block-repeat frame header is invalid")
        _validate_incompressible(block[COMPRESSED_BLOCK_HEADER_SIZE:], "compressed-block-repeat payload")
    elif recipe == "random-incompressible":
        _validate_incompressible(data, "random-incompressible sample")


def self_test() -> int:
    """Run cheap deterministic mutation tests for every structural recipe check."""
    mutations = {
        "exact-repeat": lambda data: data[:65536] + bytes([data[65536] ^ 1]) + data[65537:],
        "nonaligned-repeat": lambda data: data[:173 + 65536] + bytes([data[173 + 65536] ^ 1]) + data[173 + 65537:],
        "insert-delete": lambda data: data[:200000] + bytes([data[200000] ^ 1]) + data[200001:],
        "order1": lambda data: data[:1] + bytes([data[1] ^ 1]) + data[2:],
        "far-distance": lambda data: data[:FAR_PREFIX_SIZE] + data[:FAR_PREFIX_SIZE] + data[2 * FAR_PREFIX_SIZE :],
        "multiversion-tree": lambda data: data.replace(b"TREE-BLOCK", b"TREE-BROKEN"),
        "vm-disk-sparse": lambda data: data[:1024] + b"X" + data[1025:],
        "compressed-block-repeat": lambda data: data[:COMPRESSED_BLOCK_SIZE] + bytes([data[COMPRESSED_BLOCK_SIZE] ^ 1]) + data[COMPRESSED_BLOCK_SIZE + 1:],
        "random-incompressible": lambda data: b"\0" * 8192 + data[8192:],
    }
    for recipe, mutate in mutations.items():
        row = next(row for row in recipes() if row["args"]["recipe"] == recipe)
        args = row["args"]
        data = make_sample(recipe, args)
        validate_recipe_structure(recipe, data, args["seed"])
        try:
            validate_recipe_structure(recipe, mutate(data), args["seed"])
        except ValueError:
            continue
        raise AssertionError(f"mutation test unexpectedly accepted {recipe}")
    periodic_counterexample = bytes(range(256)) * 1024
    try:
        validate_recipe_structure("random-incompressible", periodic_counterexample, 0)
    except ValueError:
        pass
    else:
        raise AssertionError("random-incompressible accepted the 256-byte periodic counterexample")
    payload_size = COMPRESSED_BLOCK_SIZE - COMPRESSED_BLOCK_HEADER_SIZE
    periodic_payload = (periodic_counterexample * ((payload_size + 255) // 256))[:payload_size]
    periodic_block = (
        b"CBLK"
        + b"OPAQ"
        + payload_size.to_bytes(4, "little")
        + COMPRESSED_BLOCK_SIZE.to_bytes(4, "little")
        + periodic_payload
    )
    try:
        validate_recipe_structure("compressed-block-repeat", periodic_block * 4, 0)
    except ValueError:
        pass
    else:
        raise AssertionError("compressed-block-repeat accepted the 256-byte periodic counterexample")
    with tempfile.TemporaryDirectory(
        prefix="fidelity-atomic-self-test-", dir="/tmp/opencode"
    ) as name:
        root = Path(name)
        old_corpus = root / "manifest.json"
        old_baseline = root / "baseline-manifest.json"
        old_pin = root / "fidelity_baseline_pin.py"
        old_corpus.write_bytes(b"old corpus")
        old_baseline.write_bytes(b"old baseline")
        old_pin.write_bytes(b"old pin")
        corpus_temporary = _stage_bytes(old_corpus, b"new corpus")
        baseline_temporary = _stage_bytes(old_baseline, b"new baseline")
        before = (old_corpus.read_bytes(), old_baseline.read_bytes(), old_pin.read_bytes())
        calls = 0

        def fail_before_second_replace(source: Path, destination: Path) -> None:
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("injected publication failure")
            os.replace(source, destination)

        try:
            _publish_transaction(
                corpus_temporary,
                baseline_temporary,
                "corpus-hash",
                "baseline-hash",
                corpus_destination=old_corpus,
                baseline_destination=old_baseline,
                pin_destination=old_pin,
                replace=fail_before_second_replace,
            )
        except OSError:
            pass
        else:
            raise AssertionError("atomic publication failure injection unexpectedly succeeded")
        if (old_corpus.read_bytes(), old_baseline.read_bytes(), old_pin.read_bytes()) != before:
            raise AssertionError("atomic publication failure changed the old manifest pair")
        if list(root.glob(".*.tmp")) or list(root.glob(".*.bak")):
            raise AssertionError("atomic publication failure left temporary artifacts")
    with tempfile.TemporaryDirectory(
        prefix="fidelity-baseline-failure-self-test-", dir="/tmp/opencode"
    ) as name:
        root = Path(name)
        old_corpus = root / "manifest.json"
        old_baseline = root / "baseline-manifest.json"
        old_pin = root / "fidelity_baseline_pin.py"
        old_corpus.write_bytes(CORPUS_MANIFEST.read_bytes())
        old_baseline.write_bytes(BASELINE_MANIFEST.read_bytes())
        old_pin.write_bytes(Path(__file__).with_name("fidelity_baseline_pin.py").read_bytes())
        before = (old_corpus.read_bytes(), old_baseline.read_bytes(), old_pin.read_bytes())
        saved_paths = (CORPUS_DIR, CORPUS_MANIFEST, BASELINE_MANIFEST)
        saved_xz_version = xz_version

        def fail_before_publication() -> str:
            raise RuntimeError("injected baseline-generation failure")

        try:
            globals()["CORPUS_DIR"] = root
            globals()["CORPUS_MANIFEST"] = old_corpus
            globals()["BASELINE_MANIFEST"] = old_baseline
            globals()["xz_version"] = fail_before_publication
            failure_args = argparse.Namespace(
                regenerate_baseline=True,
                regenerate_corpus=True,
                old_binary=Path("/home/test/.opencode/archiving-tools/srep/bin/srep"),
                decoder=Path("/nonexistent"),
            )
            try:
                baseline(failure_args)
            except RuntimeError:
                pass
            else:
                raise AssertionError("baseline failure injection unexpectedly succeeded")
        finally:
            globals()["CORPUS_DIR"], globals()["CORPUS_MANIFEST"], globals()["BASELINE_MANIFEST"] = saved_paths
            globals()["xz_version"] = saved_xz_version
        if (old_corpus.read_bytes(), old_baseline.read_bytes(), old_pin.read_bytes()) != before:
            raise AssertionError("baseline-generation failure changed the old manifest pair")
        if list(root.glob(".*.tmp")) or list(root.glob(".*.bak")):
            raise AssertionError("baseline-generation failure left temporary artifacts")
    print("fidelity structural mutation self-tests: PASS")
    return 0


def retained_directory(prefix: str) -> Path:
    root = Path("/tmp/opencode")
    root.mkdir(mode=0o700, exist_ok=True)
    path = root / f"{prefix}.{secrets.token_hex(12)}"
    path.mkdir(mode=0o700)
    return path


def write_corpus(directory: Path, manifest: dict[str, object]) -> dict[str, Path]:
    paths: dict[str, Path] = {}
    for sample in manifest["samples"]:
        sample = dict(sample)
        args = sample["args"]
        path = directory / f"{sample['id']}.bin"
        path.write_bytes(make_sample(args["recipe"], {"size": args["size"], "seed": args["seed"]}))
        paths[sample["id"]] = path
    return paths


def xz_version() -> str:
    result = subprocess.run(["xz", "--version"], check=True, capture_output=True, text=True)
    return result.stdout.splitlines()[0].strip()


class CommandTimeoutError(RuntimeError):
    def __init__(self, command: list[str], timeout: float) -> None:
        self.command = list(command)
        self.timeout = timeout
        super().__init__(f"command timed out after {timeout}s: {command}")


def _positive_seconds(value: str) -> float:
    try:
        timeout = float(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("command timeout must be a positive number of seconds") from error
    if not math.isfinite(timeout) or timeout <= 0:
        raise argparse.ArgumentTypeError("command timeout must be a positive number of seconds")
    return timeout


def _emit_progress(message: str) -> None:
    print(message, flush=True)


def _signal_process_group(process: subprocess.Popen, sig: int) -> None:
    # Unix process-group kill so compress/info children cannot outlive a timeout.
    # Do not use poll() as a proxy for group liveness: the leader can exit while
    # a descendant still owns the captured stdout/stderr pipe.  This workspace
    # does not treat local Windows behavior as verified.
    if os.name != "nt" and hasattr(os, "killpg"):
        try:
            os.killpg(process.pid, sig)
            return
        except ProcessLookupError:
            pass
        except OSError:
            pass
    try:
        if process.poll() is not None:
            return
        if sig == signal.SIGKILL:
            process.kill()
        else:
            process.terminate()
    except ProcessLookupError:
        return


def _close_process_pipes(process: subprocess.Popen) -> None:
    for stream in (process.stdin, process.stdout, process.stderr):
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


def _wait_bounded(process: subprocess.Popen) -> None:
    try:
        process.wait(timeout=_REAP_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        _signal_process_group(process, signal.SIGKILL)
        try:
            process.wait(timeout=_REAP_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            # There is no unbounded wait fallback.  The leader was already
            # force-killed; returning is preferable to hanging the evaluator.
            return


def _reap_timed_process(process: subprocess.Popen) -> tuple[object, object]:
    _signal_process_group(process, signal.SIGTERM)
    try:
        return process.communicate(timeout=_REAP_GRACE_SECONDS)
    except subprocess.TimeoutExpired:
        _signal_process_group(process, signal.SIGKILL)
        try:
            return process.communicate(timeout=_REAP_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            # A descendant outside the group may retain a pipe indefinitely.
            # Close our descriptors and reap the leader with bounded waits; in
            # particular, never call communicate()/wait() without a timeout.
            _close_process_pipes(process)
            _wait_bounded(process)
            return (None, None)


def run_command(
    command: list[str],
    *,
    timeout: float | None = None,
    check: bool = True,
    capture_output: bool = False,
    text: bool = False,
    input: bytes | str | None = None,
    stdout=None,
    stderr=None,
) -> subprocess.CompletedProcess:
    if timeout is None:
        return subprocess.run(
            command,
            check=check,
            capture_output=capture_output,
            text=text,
            input=input,
            stdout=stdout,
            stderr=stderr,
        )
    if capture_output:
        stdout = subprocess.PIPE
        stderr = subprocess.PIPE
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE if input is not None else None,
        stdout=stdout,
        stderr=stderr,
        text=text,
        start_new_session=True,
    )
    try:
        out, err = process.communicate(input=input, timeout=timeout)
    except subprocess.TimeoutExpired as expired:
        if process.stdin is not None:
            try:
                process.stdin.close()
            except OSError:
                pass
        try:
            _reap_timed_process(process)
        finally:
            # The leader may have exited as a result of SIGTERM while a
            # descendant remains alive.  Group cleanup is therefore
            # deliberately independent of the leader's return code.
            _signal_process_group(process, signal.SIGKILL)
            _close_process_pipes(process)
            _wait_bounded(process)
        raise CommandTimeoutError(command, timeout) from expired
    result = subprocess.CompletedProcess(command, process.returncode, out, err)
    if check and result.returncode != 0:
        raise subprocess.CalledProcessError(
            result.returncode, command, output=result.stdout, stderr=result.stderr
        )
    return result


def run_info(binary: Path, archive: Path, *, timeout: float | None = None) -> dict[str, int | str]:
    result = run_command(
        [os.fspath(binary), "info", os.fspath(archive)],
        check=True,
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    values: dict[str, int | str] = {}
    for line in result.stdout.splitlines():
        if ": " not in line:
            continue
        key, value = line.split(": ", 1)
        if key in {"original size", "payload size", "blocks", "semantic matches", "covered bytes", "literal bytes"}:
            values[key] = int(value)
        elif key in {"method", "layout", "checksum"}:
            values[key] = value
    required = {"original size", "payload size", "blocks", "semantic matches", "covered bytes", "literal bytes"}
    if not required <= set(values):
        raise RuntimeError(f"incomplete info output for {archive}: {values}")
    return values


def roundtrip(
    binary: Path,
    archive: Path,
    source: Path,
    directory: Path,
    *,
    timeout: float | None = None,
) -> None:
    restored = directory / f"{archive.stem}.restored"
    run_command(
        [os.fspath(binary), "test", os.fspath(archive)],
        check=True,
        capture_output=True,
        timeout=timeout,
    )
    run_command(
        [os.fspath(binary), "decompress", os.fspath(archive), os.fspath(restored)],
        check=True,
        capture_output=True,
        timeout=timeout,
    )
    if restored.read_bytes() != source.read_bytes():
        raise RuntimeError(f"round-trip mismatch for {archive.name}")


def xz_archive(
    archive: Path, directory: Path, *, timeout: float | None = None
) -> tuple[int, str]:
    output = directory / f"{archive.name}.xzraw"
    with output.open("wb") as stream:
        run_command(
            [*XZ_ARGS, "-c", os.fspath(archive)],
            check=True,
            stdout=stream,
            stderr=subprocess.PIPE,
            timeout=timeout,
        )
    return output.stat().st_size, xz_version()


def old_argv(binary: Path, method: str, source: Path, archive: Path) -> list[str]:
    # The old -m0..-m5 selectors are retained as the historical method identity;
    # -b8k/-l16/-c16/-d0 are explicit comparable parameters for this corpus.
    minimum = "32" if method in {"m1", "m2"} else "512"
    args = [os.fspath(binary), "-v0", "-b8k", f"-l{minimum}", "-d0"]
    if method in {"m1", "m2"}:
        args.append("-c4096")
    elif method in {"m0", "m3", "m4"}:
        args.append(f"-c{minimum}")
    args.extend([f"-{method}", "-hash=md5", os.fspath(source), os.fspath(archive)])
    return args


def new_argv(binary: Path, method: str, source: Path, archive: Path) -> list[str]:
    values = [os.fspath(binary), "compress", "--method", method, "--layout", "index", "--checksum", "xxh3", "--block-size", "8KiB"]
    if method in {"m1", "m2"}:
        values.extend(["--min-match", "32", "--target-chunk", "4096"])
    else:
        values.extend(["--min-match", "512"])
    values.extend([os.fspath(source), os.fspath(archive)])
    return values


def metrics(info: dict[str, int | str], archive_size: int, final_size: int) -> dict[str, int]:
    return {
        "covered": int(info["covered bytes"]),
        "literals": int(info["literal bytes"]),
        "matches": int(info["semantic matches"]),
        "preprocessor_size": archive_size,
        "final_size": final_size,
    }


def old_metrics(info: dict[str, int | str], archive_size: int, final_size: int) -> dict[str, int]:
    return {
        "old_covered": int(info["covered bytes"]),
        "old_literals": int(info["literal bytes"]),
        "old_matches": int(info["semantic matches"]),
        "old_preprocessor_size": archive_size,
        "old_final_size": final_size,
    }


def baseline(args: argparse.Namespace) -> int:
    staged_manifests: list[Path] = []
    try:
        return _baseline_impl(args, staged_manifests)
    finally:
        for path in staged_manifests:
            path.unlink(missing_ok=True)


def _baseline_impl(args: argparse.Namespace, staged_manifests: list[Path]) -> int:
    if not BASELINE_MANIFEST.exists() and not args.regenerate_baseline:
        raise RuntimeError("baseline-manifest.json is missing; pass --regenerate-baseline explicitly")
    if BASELINE_MANIFEST.exists() and not args.regenerate_baseline:
        raise RuntimeError("baseline-manifest.json is frozen; pass --regenerate-baseline explicitly")
    old_binary = args.old_binary.resolve()
    if file_sha256(old_binary) != OLD_BINARY_SHA256:
        raise RuntimeError("old binary SHA-256 does not match the source pin")
    if args.regenerate_corpus:
        corpus = {"schema": CORPUS_VERSION, "samples": recipes()}
        corpus_temporary = stage_corpus_manifest(corpus)
        staged_manifests.append(corpus_temporary)
        corpus_manifest_path = corpus_temporary
    else:
        corpus = ensure_corpus_manifest()
        corpus_temporary = None
        corpus_manifest_path = CORPUS_MANIFEST
    directory = retained_directory("srep-fidelity-baseline")
    paths = write_corpus(directory, corpus)
    xz_ver = xz_version()
    samples: list[dict[str, object]] = []
    for sample in corpus["samples"]:
        sample = dict(sample)
        rows: dict[str, object] = {}
        for method in METHODS:
            archive = directory / f"{sample['id']}-{method}.srep"
            subprocess.run(old_argv(old_binary, method, paths[sample["id"]], archive), check=True, capture_output=True)
            info = run_info(args.decoder.resolve(), archive)
            roundtrip(args.decoder.resolve(), archive, paths[sample["id"]], directory)
            final_size, observed_xz = xz_archive(archive, directory)
            if observed_xz != xz_ver:
                raise RuntimeError("xz version changed during baseline")
            row = old_metrics(info, archive.stat().st_size, final_size)
            row.update({
                "old_binary_sha256": OLD_BINARY_SHA256,
                "xz_version": xz_ver,
                "xz_argv": XZ_ARGS,
                "old_command": old_argv(Path("OLD_BINARY"), method, Path("INPUT"), Path("OUTPUT"))[1:],
            })
            rows[method] = row
        samples.append({"id": sample["id"], "methods": rows})
    result = {
        "schema": "srep-fidelity-baseline-v1",
        "corpus_manifest_sha256": file_sha256(corpus_manifest_path),
        "old_binary_sha256": OLD_BINARY_SHA256,
        "old_binary": os.fspath(old_binary),
        "xz_version": xz_ver,
        "xz_argv": XZ_ARGS,
        "old_argv_template": ["OLD_BINARY", "-v0", "-b8k", "-lMETHOD_MIN", "-cMETHOD_CHUNK", "-d0", "METHOD", "-hash=md5", "INPUT", "OUTPUT"],
        "new_config": {"layout": "index", "checksum": "xxh3", "block_size": 8192, "m1_m2_min_match": 32, "m1_m2_target_chunk": 4096, "other_min_match": 512},
        "samples": samples,
    }
    temporary = _stage_json(BASELINE_MANIFEST, result)
    try:
        validate_baseline(
            corpus,
            result,
            manifest_path=temporary,
            corpus_manifest_path=corpus_manifest_path,
            enforce_pin=False,
        )
        corpus_hash = file_sha256(corpus_manifest_path)
        baseline_hash = file_sha256(temporary)
        _publish_transaction(
            corpus_temporary,
            temporary,
            corpus_hash,
            baseline_hash,
        )
        temporary = None
        corpus_temporary = None
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)
        if corpus_temporary is not None:
            corpus_temporary.unlink(missing_ok=True)
    print(f"retained baseline work: {directory}")
    print(f"wrote {BASELINE_MANIFEST}")
    print(f"baseline sha256: {file_sha256(BASELINE_MANIFEST)}")
    return 0


def geometric_mean(values: list[float]) -> float:
    if not values:
        raise RuntimeError("empty positive population")
    if any(value == 0 for value in values):
        return 0.0
    return math.exp(sum(math.log(value) for value in values) / len(values))


def summarize_methods(
    per_method: dict[str, list[dict[str, object]]],
) -> tuple[dict[str, object], list[dict[str, object]]]:
    summaries: dict[str, object] = {}
    failures: list[dict[str, object]] = []
    for method in METHODS:
        rows = per_method[method]
        positive = [row for row in rows if row["old"]["covered"] > 0]
        if not positive:
            raise RuntimeError(f"empty positive-coverage population for {method}")
        coverage_ratios = [min(1.0, row["new"]["covered"] / row["old"]["covered"]) for row in positive]
        size_ratios = [max(1.0, row["new"]["final_size"] / row["old"]["final_size"]) for row in rows]
        raw_sizes = [row["new"]["final_size"] / row["old"]["final_size"] for row in rows]
        coverage_gm = geometric_mean(coverage_ratios)
        size_gm = geometric_mean(size_ratios)
        worst_coverage = min(zip(coverage_ratios, positive), key=lambda item: item[0])
        worst_size = max(zip(raw_sizes, rows), key=lambda item: item[0])
        summary = {
            "positive_population": len(positive),
            "representative_population": len(rows),
            "coverage_geomean": f"{coverage_gm:.12f}",
            "size_geomean": f"{size_gm:.12f}",
            "worst_coverage": {"sample": worst_coverage[1]["id"], "ratio": f"{worst_coverage[0]:.12f}"},
            "worst_size": {"sample": worst_size[1]["id"], "ratio": f"{worst_size[0]:.12f}"},
            "coverage_pass": coverage_gm >= 0.95,
            "size_pass": size_gm <= 1.05,
            "raw_size_pass": all(value <= 1.10 for value in raw_sizes),
        }
        summaries[method] = summary
        if not summary["coverage_pass"] or not summary["size_pass"] or not summary["raw_size_pass"]:
            failures.append({"method": method, "summary": summary})
    return summaries, failures


def validate_baseline(
    corpus: dict[str, object],
    baseline_data: dict[str, object],
    *,
    manifest_path: Path = BASELINE_MANIFEST,
    corpus_manifest_path: Path = CORPUS_MANIFEST,
    enforce_pin: bool = True,
) -> None:
    if baseline_data.get("schema") != "srep-fidelity-baseline-v1":
        raise ValueError("unsupported fidelity baseline schema")
    if baseline_data.get("corpus_manifest_sha256") != file_sha256(corpus_manifest_path):
        raise ValueError("baseline was generated from a different corpus manifest")
    if baseline_data.get("old_binary_sha256") != OLD_BINARY_SHA256:
        raise ValueError("baseline old binary hash differs from source pin")
    if enforce_pin and file_sha256(manifest_path) != BASELINE_MANIFEST_SHA256:
        raise ValueError("baseline manifest does not match the source pin")
    if baseline_data.get("xz_argv") != XZ_ARGS or baseline_data.get("xz_version") != xz_version():
        raise ValueError("baseline xz command/version differs from this environment")
    expected_ids = [sample["id"] for sample in corpus["samples"]]
    actual_samples = baseline_data.get("samples")
    if [sample.get("id") for sample in actual_samples] != expected_ids:
        raise ValueError("baseline omits or reorders frozen representatives")
    positive_counts = {method: 0 for method in METHODS}
    corpus_by_id = {sample["id"]: sample for sample in corpus["samples"]}
    for sample in actual_samples:
        if set(sample.get("methods", {})) != set(METHODS):
            raise ValueError(f"baseline has incomplete method rows for {sample.get('id')}")
        for method in METHODS:
            row = sample["methods"][method]
            required = {"old_covered", "old_literals", "old_matches", "old_preprocessor_size", "old_final_size", "old_binary_sha256", "xz_version", "xz_argv", "old_command"}
            if set(row) != required:
                raise ValueError(f"baseline metric schema differs for {sample['id']} {method}")
            if row["old_final_size"] <= 0 or row["old_covered"] < 0 or row["old_literals"] < 0 or row["old_matches"] < 0:
                raise ValueError(f"invalid old metrics for {sample['id']} {method}")
            if row["old_preprocessor_size"] <= 0:
                raise ValueError(f"invalid old preprocessor size for {sample['id']} {method}")
            if row["old_covered"] + row["old_literals"] != corpus_by_id[sample["id"]]["size"]:
                raise ValueError(f"old metrics do not partition the sample for {sample['id']} {method}")
            if row["old_binary_sha256"] != OLD_BINARY_SHA256:
                raise ValueError(f"row old binary pin differs for {sample['id']} {method}")
            if row["xz_version"] != baseline_data["xz_version"] or row["xz_argv"] != baseline_data["xz_argv"]:
                raise ValueError(f"row xz provenance differs for {sample['id']} {method}")
            expected_command = old_argv(Path("OLD_BINARY"), method, Path("INPUT"), Path("OUTPUT"))[1:]
            if row["old_command"] != expected_command:
                raise ValueError(f"row old command differs for {sample['id']} {method}")
            if row["old_covered"] > 0:
                positive_counts[method] += 1
    for method, count in positive_counts.items():
        if count < 6:
            raise ValueError(f"{method} has only {count} positive-coverage representatives; at least 6 required")


def _safe_file_sha256(path: Path) -> str | None:
    try:
        return file_sha256(path)
    except OSError:
        return None


def _evaluation_report(
    *,
    binary: Path | None,
    baseline_data: dict[str, object] | None,
    report_samples: list[dict[str, object]],
    summaries: dict[str, object] | None,
    failures: list[dict[str, object]],
    passed: bool,
    complete: bool,
    error: dict[str, object] | None = None,
) -> dict[str, object]:
    report: dict[str, object] = {
        "schema": "srep-fidelity-report-v1",
        "corpus_manifest_sha256": _safe_file_sha256(CORPUS_MANIFEST),
        "baseline_manifest_sha256": _safe_file_sha256(BASELINE_MANIFEST),
        "binary": os.fspath(binary) if binary is not None else None,
        "xz_version": None if baseline_data is None else baseline_data.get("xz_version"),
        "xz_argv": None if baseline_data is None else baseline_data.get("xz_argv"),
        "samples": report_samples,
        "methods": summaries or {},
        "failures": failures,
        "pass": passed,
        "complete": complete,
    }
    if error is not None:
        report["error"] = error
    return report


def _persist_report(path: Path, report: dict[str, object]) -> None:
    _atomic_json(path, report)


def _error_payload(
    current: dict[str, object], error: BaseException
) -> dict[str, object]:
    command = current.get("command")
    return {
        "sample": current.get("sample"),
        "method": current.get("method"),
        "stage": current.get("stage"),
        "command": list(command) if command is not None else None,
        "error": str(error),
    }


def evaluate(args: argparse.Namespace) -> int:
    timeout = getattr(args, "command_timeout", DEFAULT_COMMAND_TIMEOUT)
    output: Path | None = args.report
    binary: Path | None = None
    baseline_data: dict[str, object] | None = None
    report_samples: list[dict[str, object]] = []
    current: dict[str, object] = {
        "sample": None,
        "method": None,
        "stage": "validate",
        "command": None,
    }
    try:
        corpus = ensure_corpus_manifest()
        baseline_data = strict_json(BASELINE_MANIFEST)
        validate_baseline(corpus, baseline_data)
        binary = args.binary.resolve()
        directory = retained_directory("srep-fidelity-evaluate")
        if output is None:
            output = directory / "fidelity-report.json"
        paths = write_corpus(directory, corpus)
        per_method: dict[str, list[dict[str, object]]] = {method: [] for method in METHODS}
        sample_count = len(corpus["samples"])
        row_count = sample_count * len(METHODS)
        completed_rows = 0
        for sample_index, sample in enumerate(corpus["samples"], start=1):
            sample = dict(sample)
            sample_id = sample["id"]
            current["sample"] = sample_id
            current["method"] = None
            current["stage"] = "sample"
            current["command"] = None
            _emit_progress(f"evaluate: sample {sample_id} ({sample_index}/{sample_count})")
            old_rows = next(item["methods"] for item in baseline_data["samples"] if item["id"] == sample_id)
            new_rows: dict[str, object] = {}
            for method in METHODS:
                current["method"] = method
                row_index = completed_rows + 1
                archive = directory / f"{sample_id}-{method}.srep"
                source = paths[sample_id]
                compress_command = new_argv(binary, method, source, archive)
                current["stage"] = "compress"
                current["command"] = compress_command
                _emit_progress(
                    f"evaluate: sample {sample_id} method {method} ({row_index}/{row_count}) stage compress"
                )
                run_command(compress_command, check=True, capture_output=True, timeout=timeout)

                info_command = [os.fspath(binary), "info", os.fspath(archive)]
                current["stage"] = "info"
                current["command"] = info_command
                _emit_progress(
                    f"evaluate: sample {sample_id} method {method} ({row_index}/{row_count}) stage info"
                )
                info = run_info(binary, archive, timeout=timeout)

                restored = directory / f"{archive.stem}.restored"
                test_command = [os.fspath(binary), "test", os.fspath(archive)]
                current["stage"] = "test"
                current["command"] = test_command
                _emit_progress(
                    f"evaluate: sample {sample_id} method {method} ({row_index}/{row_count}) stage test"
                )
                run_command(test_command, check=True, capture_output=True, timeout=timeout)

                decompress_command = [
                    os.fspath(binary),
                    "decompress",
                    os.fspath(archive),
                    os.fspath(restored),
                ]
                current["stage"] = "decompress"
                current["command"] = decompress_command
                _emit_progress(
                    f"evaluate: sample {sample_id} method {method} ({row_index}/{row_count}) stage decompress"
                )
                run_command(decompress_command, check=True, capture_output=True, timeout=timeout)
                if restored.read_bytes() != source.read_bytes():
                    raise RuntimeError(f"round-trip mismatch for {archive.name}")

                xz_command = [*XZ_ARGS, "-c", os.fspath(archive)]
                current["stage"] = "xz"
                current["command"] = xz_command
                _emit_progress(
                    f"evaluate: sample {sample_id} method {method} ({row_index}/{row_count}) stage xz"
                )
                final_size, xz_ver = xz_archive(archive, directory, timeout=timeout)
                if xz_ver != baseline_data["xz_version"]:
                    raise RuntimeError("xz version changed during evaluation")
                row = metrics(info, archive.stat().st_size, final_size)
                new_rows[method] = row
                old = old_rows[method]
                per_method[method].append({
                    "id": sample_id,
                    "old": {
                        "covered": old["old_covered"],
                        "literals": old["old_literals"],
                        "matches": old["old_matches"],
                        "preprocessor_size": old["old_preprocessor_size"],
                        "final_size": old["old_final_size"],
                    },
                    "new": row,
                })
                completed_rows += 1
                sample_entry = {"id": sample_id, "methods": dict(new_rows)}
                if report_samples and report_samples[-1]["id"] == sample_id:
                    report_samples[-1] = sample_entry
                else:
                    report_samples.append(sample_entry)
                _persist_report(
                    output,
                    _evaluation_report(
                        binary=binary,
                        baseline_data=baseline_data,
                        report_samples=report_samples,
                        summaries=None,
                        failures=[],
                        passed=False,
                        complete=False,
                    ),
                )

        if completed_rows != row_count:
            raise RuntimeError(f"evaluation produced {completed_rows} rows; {row_count} required")

        summaries, failures = summarize_methods(per_method)

        report = _evaluation_report(
            binary=binary,
            baseline_data=baseline_data,
            report_samples=report_samples,
            summaries=summaries,
            failures=failures,
            passed=not failures,
            complete=True,
        )
        _persist_report(output, report)
        print(f"retained evaluation work: {directory}")
        print(f"report: {output}")
        for method in METHODS:
            summary = summaries[method]
            print(f"{method}: coverage geomean {summary['coverage_geomean']} ({summary['positive_population']} positive), size geomean {summary['size_geomean']}, worst raw size {summary['worst_size']['ratio']}")
        if failures:
            print("FIDELITY GATE: FAIL", file=sys.stderr)
            for failure in failures:
                print(json.dumps(failure), file=sys.stderr)
            return 2
        print("FIDELITY GATE: PASS")
        return 0
    except (OSError, subprocess.CalledProcessError, ValueError, RuntimeError) as error:
        if output is not None:
            payload = _error_payload(current, error)
            try:
                _persist_report(
                    output,
                    _evaluation_report(
                        binary=binary,
                        baseline_data=baseline_data,
                        report_samples=report_samples,
                        summaries=None,
                        failures=[payload],
                        passed=False,
                        complete=False,
                        error=payload,
                    ),
                )
            except OSError:
                pass
        raise


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    baseline_parser = subparsers.add_parser("baseline")
    baseline_parser.add_argument("--old-binary", required=True, type=Path)
    baseline_parser.add_argument("--decoder", type=Path, default=ROOT / "target" / "debug" / "srep")
    baseline_parser.add_argument("--regenerate-baseline", action="store_true")
    baseline_parser.add_argument("--regenerate-corpus", action="store_true")
    evaluate_parser = subparsers.add_parser("evaluate")
    evaluate_parser.add_argument("--binary", required=True, type=Path)
    evaluate_parser.add_argument("--report", type=Path)
    evaluate_parser.add_argument(
        "--command-timeout",
        type=_positive_seconds,
        default=DEFAULT_COMMAND_TIMEOUT,
        metavar="SECONDS",
        help="positive seconds allowed for each evaluation command (default: 180)",
    )
    subparsers.add_parser("self-test")
    args = parser.parse_args()
    try:
        if args.command == "baseline":
            return baseline(args)
        if args.command == "evaluate":
            return evaluate(args)
        return self_test()
    except (OSError, subprocess.CalledProcessError, ValueError, RuntimeError) as error:
        print(f"fidelity: ERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

"""Shared corruption identity and required-coverage helpers."""
from __future__ import annotations

import os
import stat
from pathlib import Path

EXPECTED_CODES = {
    "Accepted": 0,
    "ChecksumMismatch": 11,
    "UnsupportedVersion": 3,
    "UnknownChecksum": 5,
    "CorruptHeader": 6,
    "CorruptIndex": 9,
    "InvalidMatch": 10,
    "TruncatedArchive": 7,
}


def identity(case: dict) -> tuple:
    return (
        case["base_fixture"], case["operation"], case["offset"],
        case.get("value", 1), case.get("length", 1),
        case.get("output", ""),
    )


def _size(root: Path, name: str, root_fd: int | None) -> int:
    if root_fd is None:
        return len((root / name).read_bytes())
    fd = os.open(
        name,
        os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0),
        dir_fd=root_fd,
    )
    try:
        details = os.fstat(fd)
        assert stat.S_ISREG(details.st_mode), name
        return details.st_size
    finally:
        os.close(fd)


def required_footer_identities(root: Path, root_fd: int | None = None) -> set[tuple]:
    required = set()
    names = sorted(
        name for name in (os.listdir(root_fd) if root_fd is not None else (path.name for path in root.iterdir()))
        if name.startswith("v4-") and name.endswith(".srep")
    )
    for name in names:
        footer = _size(root, name, root_fd) - 24
        for offset in range(footer, footer + 24):
            required.add((name, "xor", offset, 1))
    return required


def category(name: str) -> str:
    if name.startswith("signature."):
        return "signature"
    if name.startswith("packed."):
        return "packed"
    if name == "base_len":
        return "base"
    if name.startswith("vhash.seed") or name.startswith("siphash.seed"):
        return "seed"
    if name.startswith("v4.footer") or name.startswith("footer-required"):
        return "footer"
    if name.startswith("v4.size"):
        return "size"
    if name.startswith("v4.range"):
        return "range"
    if ".stat_" in name or name.startswith("v4.stat"):
        return "stat"
    if name.startswith("truncate.") or name.startswith("body.byte"):
        return "truncation"
    if name.startswith("v4-none."):
        return "accepted-output"
    if name.startswith("block.") or name.startswith("index."):
        return "record"
    return "other"


REQUIRED_CATEGORIES = {
    "signature", "packed", "base", "seed", "footer", "size", "range",
    "stat", "record", "truncation", "accepted-output",
}
REQUIRED_NAMES = {
    "signature.word0", "packed.version", "packed.hash_id", "packed.seed_len", "packed.bias",
    "base_len", "vhash.seed.0", "siphash.seed.0", "block.literal_bytes", "block.digest",
    "v4.size_entry", "v4.stat_total", "v4.range.body_start", "v4.range.stats_start",
    "v4.footer.version", "v4.footer.marker0", "v4.footer.marker1", "truncate.footer",
    "v4-none.opaque_digest", "v4-none.literal_mutation",
}


def validate_required_categories(cases: list[dict], root: Path, root_fd: int | None = None) -> None:
    categories = {category(case["name"]) for case in cases}
    missing = REQUIRED_CATEGORIES - categories
    assert not missing, f"missing corruption categories: {sorted(missing)}"
    names = {case["name"] for case in cases}
    missing_names = REQUIRED_NAMES - names
    assert not missing_names, f"missing required corruption identities: {sorted(missing_names)}"
    present = {
        (case["base_fixture"], case["operation"], case["offset"], case.get("value", 1))
        for case in cases
    }
    assert required_footer_identities(root, root_fd) <= present

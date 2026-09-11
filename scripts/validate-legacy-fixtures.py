#!/usr/bin/env python3
"""Independent byte-level validator for the committed legacy fixtures.

The parser below is deliberately separate from the Rust decoder.  It checks
wire fields, physical records, range arithmetic, and the manifest metrics
directly from bytes.  It does not invoke either decoder or the old binary.
"""
from __future__ import annotations

import hashlib
import json
import struct
import sys
import subprocess
import os
import stat
from pathlib import Path
from legacy_fixture_schema import identity, validate_required_categories

CHECKSUMS = {
    0: ("md5", 0, 16),
    1: ("none", 0, 16),
    2: ("sha1", 0, 20),
    3: ("sha512", 0, 64),
    4: ("vhash", 32, 16),
    5: ("siphash", 16, 8),
}
ROOT_FD: int | None = None

TOP_KEYS = {"schema", "original", "original_sha256", "rows", "special"}
ROW_KEYS = {
    "archive", "archive_sha256", "original", "original_sha256", "version",
    "checksum_id", "checksum", "layout", "base_len", "archive_size",
    "original_size", "block_count", "match_count", "covered_bytes",
    "literal_bytes", "trailing_literal_bytes", "source_gap0_count",
    "cross_source_fragment_count", "generation",
}
GENERATION_KEYS = {"version", "options"}
SPECIAL_KEYS = {
    "archive", "original", "archive_sha256", "original_sha256", "archive_size",
    "original_size", "trailing_literal_bytes", "provenance",
    "version", "checksum_id", "checksum", "layout", "base_len", "block_count",
    "match_count", "covered_bytes", "literal_bytes", "source_gap0_count",
    "cross_source_fragment_count",
}
SPECIAL_ORIGINALS = {
    "historical-112.srep": "historical-112.txt",
    "historical-1675.srep": "historical-1675.pcf",
    "special-v1-trailing.srep": "special-v1-trailing.bin",
    "special-same-source-v3.srep": "special-same-source-v3.bin",
    "special-same-source-v4.srep": "special-same-source-v4.bin",
}
SPECIAL_PROVENANCE = {
    "historical-112.srep": "legacy-migration-112",
    "historical-1675.srep": "legacy-migration-1675",
    "special-v1-trailing.srep": "old-encoder-generated",
    "special-same-source-v3.srep": "independent-construction",
    "special-same-source-v4.srep": "independent-construction",
}
EXPECTED_PAIRS = {(version, checksum_id) for version in range(1, 5) for checksum_id in range(6)}
EXPECTED_ARCHIVES = {
    f"v{version}-{CHECKSUMS[checksum_id][0] if checksum_id != 4 else 'vmac'}.srep"
    for version, checksum_id in EXPECTED_PAIRS
}
EXPECTED_SPECIAL = set(SPECIAL_PROVENANCE)


def root_file_path(root: Path, name: str) -> Path:
    if ROOT_FD is not None:
        return Path(f"/proc/self/fd/{ROOT_FD}") / name
    return root / name


def root_names(root: Path) -> list[str]:
    if ROOT_FD is not None:
        return os.listdir(ROOT_FD)
    return [path.name for path in root.iterdir()]


def assert_regular_root_entries(root: Path, names: set[str]) -> None:
    for name in names:
        details = (
            os.stat(name, dir_fd=ROOT_FD, follow_symlinks=False)
            if ROOT_FD is not None
            else os.lstat(root / name)
        )
        assert stat.S_ISREG(details.st_mode), f"fixture entry is not a regular file: {name}"


def read_root(root: Path, name: str) -> bytes:
    if ROOT_FD is None:
        return (root / name).read_bytes()
    fd = os.open(name, os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0), dir_fd=ROOT_FD)
    try:
        chunks = []
        while chunk := os.read(fd, 1024 * 1024):
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)


def write_exclusive(root: Path, name: str, text: str) -> None:
    parent_fd = os.dup(ROOT_FD) if ROOT_FD is not None else os.open(root, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0) | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0))
    try:
        fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_CLOEXEC", 0), 0o600, dir_fd=parent_fd)
        try:
            data = text.encode()
            while data:
                written = os.write(fd, data)
                data = data[written:]
        finally:
            os.close(fd)
    finally:
        os.close(parent_fd)


def strict_object(pairs):
    keys = [key for key, _ in pairs]
    assert len(keys) == len(set(keys)), f"duplicate JSON key: {keys}"
    return dict(pairs)


def validate_corruption_schema(root: Path) -> None:
    data = json.loads(read_root(root, "corruptions.json"), object_pairs_hook=strict_object)
    assert set(data) == {"schema", "base_fixture", "cases"}
    assert data["schema"] == "srep-legacy-corruptions-v1"
    assert isinstance(data["cases"], list) and len(data["cases"]) >= 100
    names = set()
    identities = set()
    for case in data["cases"]:
        assert set(case) == {"name", "base_fixture", "offset", "operation", "value", "expected"}
        assert case["name"] not in names
        names.add(case["name"])
        assert case["operation"] in {"xor", "delete"}
        assert isinstance(case["offset"], int) and case["offset"] >= 0
        assert isinstance(case["value"], int) and case["value"] != 0
        assert case["expected"] in {"Accepted", "ChecksumMismatch", "UnsupportedVersion", "UnknownChecksum", "CorruptHeader", "CorruptIndex", "InvalidMatch", "TruncatedArchive"}
        case_identity = identity(case)
        assert case_identity not in identities, f"duplicate corruption identity: {case_identity}"
        identities.add(case_identity)
    assert len(names) == len(data["cases"])
    validate_required_categories(data["cases"], root, ROOT_FD)



def recipe(version: int, checksum: str) -> dict[str, str]:
    mode = {1: "-m3o", 2: "-m4o", 3: "-m4f", 4: "-m4"}[version]
    old_name = "vmac" if checksum == "vhash" else checksum
    option = "-hash-" if checksum == "none" else f"-hash={old_name}"
    return {"version": "SREP 3.93a beta", "options": f"-v0 -b8k -l16 -c16 {mode} {option}"}


def safe_relative(path: str) -> None:
    candidate = Path(path)
    assert not candidate.is_absolute() and ".." not in candidate.parts
    assert str(candidate) == path and "\\" not in path


def u32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<I", data, offset)[0]


def parse(path: Path, root: Path | None = None) -> dict[str, int | str]:
    data = read_root(root, path.name) if root is not None else path.read_bytes()
    signature = bytes((0x17, 0x18, 0x35, 0x26, 0x53, 0x52, 0x45, 0x50))
    assert data[:8] == signature, f"{path.name}: signature"
    packed = u32(data, 8)
    version = packed & 0xFF
    checksum_id = (packed >> 8) & 0xFF
    seed_len = (packed >> 16) & 0xFF
    bias = packed >> 24
    name, expected_seed, digest_len = CHECKSUMS[checksum_id]
    assert seed_len == expected_seed, f"{path.name}: seed length"
    assert bias + 16 == digest_len or checksum_id == 5, f"{path.name}: digest width"
    base_len = u32(data, 12)
    assert version in (1, 2, 3, 4)
    assert version >= 3 or base_len > 0, f"{path.name}: BASE_LEN"

    header_end = 16 + seed_len
    body_end = len(data)
    sizes_start = None
    stat_start = None
    sizes: list[int] = []
    if version == 4:
        assert len(data) >= header_end + 24, f"{path.name}: footer"
        footer = len(data) - 24
        stat_total = u32(data, footer) | (u32(data, footer + 4) << 32)
        footer_size = u32(data, footer + 8)
        assert u32(data, footer + 12) == 1, f"{path.name}: footer version"
        assert u32(data, footer + 16) == 0xAFBAADAC, f"{path.name}: footer marker 1"
        assert u32(data, footer + 20) == 0xD9CAE7E8, f"{path.name}: footer marker 2"
        assert footer_size >= 24 and footer_size <= len(data) - header_end
        sizes_start = len(data) - footer_size
        stat_start = sizes_start - stat_total
        assert header_end <= stat_start <= sizes_start <= footer
        assert (footer_size - 24) % 4 == 0
        sizes = [u32(data, sizes_start + i) for i in range(0, footer_size - 24, 4)]
        assert all(size % 16 == 0 for size in sizes)
        assert sum(sizes) == stat_total
        body_end = stat_start
    else:
        remaining = len(data) - header_end
        if remaining == 8:
            assert data[header_end:] == bytes(8), f"{path.name}: terminator"
            body_end = header_end
        elif remaining >= 8 and data[header_end : header_end + 8] == bytes(8):
            raise AssertionError(f"{path.name}: trailing terminator bytes")

    blocks: list[tuple[int, int, int, int, int, int]] = []
    logical = 0
    position = header_end
    while position < body_end:
        assert body_end - position >= 12 + digest_len, f"{path.name}: frame"
        frame_literal_bytes, uncompressed, inline_stat = struct.unpack_from("<III", data, position)
        frame_start = position
        assert uncompressed > 0, f"{path.name}: zero block"
        position += 12 + digest_len
        assert version != 4 or inline_stat == 0, f"{path.name}: inline v4 stats"
        width = 12 if version == 1 else 16
        assert inline_stat % width == 0, f"{path.name}: stat alignment"
        stats_end = position + inline_stat
        literal_end = stats_end + frame_literal_bytes
        assert literal_end <= body_end, f"{path.name}: payload"
        blocks.append((frame_start, logical, uncompressed, position, inline_stat, frame_literal_bytes))
        logical += uncompressed
        position = literal_end
    assert position == body_end, f"{path.name}: body boundary"
    if version == 4:
        assert len(blocks) == len(sizes), f"{path.name}: block count"

    fragments: list[tuple[int, int, int, int, int]] = []
    matches = covered = 0
    source_gap0 = 0
    trailing_literal = 0
    cross_source = 0
    physical_fragments: list[dict[str, int]] = []
    for block_index, (_, block_start, block_len, stats_pos, inline_stat, frame_literal_bytes) in enumerate(blocks):
        if version == 4:
            assert stat_start is not None
            prior = sum(sizes[:block_index])
            stats_pos = stat_start + prior
            inline_stat = sizes[block_index]
        stats_end = stats_pos + inline_stat
        cursor = block_start
        source_cursor = block_start
        first_source_record = True
        block_records: list[tuple[int, int, int, int]] = []
        width = 12 if version == 1 else 16
        for stat in range(stats_pos, stats_end, width):
            literal_len = u32(data, stat)
            if version == 1:
                distance = u32(data, stat + 4) * base_len
                length = (u32(data, stat + 8) + 1) * base_len
                destination = cursor + literal_len
                source = (destination // base_len) * base_len - distance
                cursor = destination + length
            elif version == 2:
                distance = u32(data, stat + 4) | (u32(data, stat + 8) << 32)
                length = u32(data, stat + 12) + base_len
                destination = cursor + literal_len
                source = destination - distance
                cursor = destination + length
            else:
                distance = u32(data, stat + 4) | (u32(data, stat + 8) << 32)
                length = u32(data, stat + 12) + base_len
                source = source_cursor + literal_len
                destination = source + distance
                source_cursor = source
                cursor = destination + length
                if not first_source_record and literal_len == 0:
                    source_gap0 += 1
                first_source_record = False
            assert distance > 0 and length > 0, f"{path.name}: zero match"
            if version >= 3:
                assert block_start <= source < block_start + block_len
                assert source + length <= block_start + block_len
            else:
                assert 0 <= source < destination
            block_records.append((destination, destination + length, source, length))
            fragments.append((destination, destination + length, source, block_index))
            if version >= 3:
                physical_fragments.append({
                    "source": source,
                    "length": length,
                    "distance": distance,
                    "destination": destination,
                    "block": block_index,
                })
            matches += 1
            covered += length
        if version <= 2:
            final_tail = block_start + block_len - cursor
            assert final_tail >= 0
            trailing_literal += final_tail
        if version < 3:
            literal_sum = sum(u32(data, stat) for stat in range(stats_pos, stats_end, width))
            assert literal_sum <= frame_literal_bytes, f"{path.name}: literal statistics"

    if version >= 3:
        for previous, current in zip(physical_fragments, physical_fragments[1:]):
            block_end = blocks[previous["block"]][1] + blocks[previous["block"]][2]
            if (
                previous["source"] + previous["length"] == block_end
                and current["source"] == block_end
                and previous["distance"] == current["distance"]
                and previous["destination"] + previous["length"] == current["destination"]
            ):
                cross_source += 1
        for block_start, block_len in ((b[1], b[2]) for b in blocks):
            end = block_start + block_len
            covered_end = max((finish for start, finish, _, _ in fragments if start < end and finish > block_start), default=block_start)
            trailing_literal += max(0, end - max(block_start, covered_end))

    return {
        "archive_size": len(data),
        "version": version,
        "checksum_id": checksum_id,
        "checksum": name,
        "layout": {1: "io-rounded", 2: "io", 3: "future", 4: "index"}[version],
        "base_len": base_len,
        "original_size": logical,
        "block_count": len(blocks),
        "match_count": matches,
        "covered_bytes": covered,
        "literal_bytes": sum(struct.unpack_from("<I", data, b[0])[0] for b in blocks),
        "trailing_literal_bytes": trailing_literal,
        "source_gap0_count": source_gap0,
        "cross_source_fragment_count": cross_source,
    }


def validate_decoded(root: Path, manifest: dict, decoder: str) -> None:
    for entry in manifest["rows"] + manifest["special"]:
        archive = root_file_path(root, entry["archive"])
        result = subprocess.run(
            [decoder, "decompress", str(archive), "-"], capture_output=True, check=True,
            pass_fds=(ROOT_FD,) if ROOT_FD is not None else (),
        )
        assert result.stdout == read_root(root, entry["original"]), f"{entry['archive']}: decoded original mismatch"


def main() -> int:
    project_root = Path(__file__).resolve().parents[1]
    root = project_root / "tests" / "fixtures" / "legacy"
    write_manifest = "--write-manifest" in sys.argv
    generate_manifest = "--generate-manifest" in sys.argv
    decoder = sys.argv[sys.argv.index("--decoder") + 1] if "--decoder" in sys.argv else None
    global ROOT_FD
    if "--root-fd" in sys.argv:
        ROOT_FD = int(sys.argv[sys.argv.index("--root-fd") + 1])
    if "--acceptance" in sys.argv and decoder is None:
        raise AssertionError("--acceptance requires --decoder PATH")
    if "--root" in sys.argv:
        root = Path(sys.argv[sys.argv.index("--root") + 1])
    manifest_present = "manifest.json" in root_names(root)
    manifest = json.loads(read_root(root, "manifest.json"), object_pairs_hook=strict_object) if manifest_present else {
        "schema": "srep-legacy-v1",
        "original": "original.bin",
        "original_sha256": "",
        "rows": [],
        "special": [],
    }
    assert set(manifest) == TOP_KEYS
    assert manifest["schema"] == "srep-legacy-v1"
    assert isinstance(manifest["rows"], list) and isinstance(manifest["special"], list)
    allowed = {"original.bin", "corruptions.json", "manifest.json"} | EXPECTED_ARCHIVES
    for name in SPECIAL_PROVENANCE:
        allowed.add(name)
    allowed.update({"historical-112.txt", "historical-1675.pcf", "special-v1-trailing.bin", "special-same-source-v3.bin", "special-same-source-v4.bin"})
    actual_names = set(root_names(root))
    expected_names = allowed if manifest_present else allowed - {"manifest.json"}
    assert actual_names == expected_names, f"unexpected fixture entries: {sorted(actual_names ^ expected_names)}"
    assert_regular_root_entries(root, actual_names)
    safe_relative(manifest["original"])
    original = read_root(root, manifest["original"])
    original_sha256 = hashlib.sha256(original).hexdigest()
    if generate_manifest:
        manifest["original_sha256"] = original_sha256
        manifest["special"] = []
        special_names = [
            ("historical-112.srep", "historical-112.txt", "legacy-migration-112"),
            ("historical-1675.srep", "historical-1675.pcf", "legacy-migration-1675"),
            ("special-v1-trailing.srep", "special-v1-trailing.bin", "old-encoder-generated"),
            ("special-same-source-v3.srep", "special-same-source-v3.bin", "independent-construction"),
            ("special-same-source-v4.srep", "special-same-source-v4.bin", "independent-construction"),
        ]
        for archive_name, original_name, provenance in special_names:
            archive_path = root_file_path(root, archive_name)
            original_path = root_file_path(root, original_name)
            assert archive_name in root_names(root), archive_name
            assert original_name in root_names(root), original_name
            parsed = parse(archive_path, root)
            manifest["special"].append({
                "archive": archive_name,
                "original": original_name,
                "archive_sha256": hashlib.sha256(read_root(root, archive_name)).hexdigest(),
                "original_sha256": hashlib.sha256(read_root(root, original_name)).hexdigest(),
                "archive_size": parsed["archive_size"],
                "original_size": parsed["original_size"],
                "provenance": provenance,
                **{key: parsed[key] for key in SPECIAL_KEYS if key in parsed},
            })
    assert original_sha256 == manifest["original_sha256"]
    if generate_manifest:
        manifest["rows"] = []
        archives = sorted(
            (root_file_path(root, name) for name in root_names(root) if name.startswith("v") and name.endswith(".srep")),
            key=lambda path: (parse(path, root)["version"], parse(path, root)["checksum_id"]),
        )
        assert {path.name for path in archives} == EXPECTED_ARCHIVES
        for archive in archives:
            actual = parse(archive, root)
            actual["archive"] = archive.name
            actual["archive_sha256"] = hashlib.sha256(read_root(root, archive.name)).hexdigest()
            actual["original"] = manifest["original"]
            actual["original_sha256"] = hashlib.sha256(original).hexdigest()
            actual["generation"] = recipe(actual["version"], actual["checksum"])
            manifest["rows"].append(actual)
        manifest["original_sha256"] = hashlib.sha256(original).hexdigest()
        manifest["schema"] = "srep-legacy-v1"
        manifest["special"].sort(key=lambda item: item["archive"])
        manifest["rows"].sort(key=lambda item: (item["version"], item["checksum_id"]))
        write_exclusive(root, "manifest.json", json.dumps(manifest, indent=2) + "\n")
    assert len(manifest["rows"]) == 24
    assert len({(row.get("version"), row.get("checksum_id")) for row in manifest["rows"]}) == 24
    row_pairs = {(row.get("version"), row.get("checksum_id")) for row in manifest["rows"]}
    assert row_pairs == EXPECTED_PAIRS
    assert {row.get("archive") for row in manifest["rows"]} == EXPECTED_ARCHIVES
    for row in manifest["rows"]:
        assert set(row) == ROW_KEYS
        safe_relative(row["archive"])
        safe_relative(row["original"])
        assert row["archive"] == Path(row["archive"]).name
        assert row["original"] == Path(row["original"]).name
        assert row["original"] == manifest["original"]
        assert row["original_sha256"] == manifest["original_sha256"]
        assert set(row["generation"]) == GENERATION_KEYS
        assert row["generation"] == recipe(row["version"], row["checksum"])
        archive = root_file_path(root, row["archive"])
        assert hashlib.sha256(read_root(root, row["archive"])).hexdigest() == row["archive_sha256"], row["archive"]
        actual = parse(archive, root)
        for key, value in actual.items():
            assert row[key] == value, f"{row['archive']}: {key}: {row[key]} != {value}"
    assert len(manifest["special"]) == len(EXPECTED_SPECIAL)
    assert {special.get("archive") for special in manifest["special"]} == EXPECTED_SPECIAL
    assert len({special.get("archive") for special in manifest["special"]}) == len(manifest["special"])
    for special in manifest["special"]:
        assert set(special) == SPECIAL_KEYS
        assert special["archive"] in EXPECTED_SPECIAL
        assert special["provenance"] == SPECIAL_PROVENANCE[special["archive"]]
        assert special["original"] == SPECIAL_ORIGINALS[special["archive"]]
        safe_relative(special["archive"])
        safe_relative(special["original"])
        archive = root_file_path(root, special["archive"])
        assert hashlib.sha256(read_root(root, special["archive"])).hexdigest() == special["archive_sha256"]
        assert hashlib.sha256(read_root(root, special["original"])).hexdigest() == special["original_sha256"]
        parsed = parse(archive, root)
        for key in (
            "version", "checksum_id", "checksum", "layout", "base_len", "archive_size",
            "original_size", "block_count", "match_count", "covered_bytes",
            "literal_bytes", "trailing_literal_bytes", "source_gap0_count",
            "cross_source_fragment_count",
        ):
            assert parsed[key] == special[key], f"{special['archive']}: {key}"
        if special["archive"].startswith("special-same-source"):
            assert parsed["source_gap0_count"] == 1
            assert parsed["match_count"] == 2
    if write_manifest:
        write_exclusive(root, "manifest.json", json.dumps(manifest, indent=2) + "\n")
    print("validated 24 exact legacy rows and 14 derived metrics independently")
    special = {
        "historical_112": root_file_path(root, "historical-112.srep"),
        "historical_1675": root_file_path(root, "historical-1675.srep"),
        "v1_trailing": root_file_path(root, "special-v1-trailing.srep"),
    }
    for name, archive in special.items():
        assert archive.name in root_names(root), f"missing special fixture {name}"
        parsed = parse(archive, root)
        assert parsed["version"] in (1, 4)
    validate_corruption_schema(root)
    if decoder:
        validate_decoded(root, manifest, decoder)
    print("historical samples: 112 and 1675 byte fixtures committed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

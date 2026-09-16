#!/usr/bin/env python3
"""Verify the release-wide SHA-256 inventory."""

import hashlib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "SHA256SUMS"


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def main():
    expected = {}
    for line in MANIFEST.read_text().splitlines():
        checksum, relative = line.split("  ", 1)
        expected[relative] = checksum
    actual_paths = {
        str(path.relative_to(ROOT))
        for path in ROOT.rglob("*")
        if path.is_file() and ".git" not in path.parts and path != MANIFEST
    }
    if actual_paths != set(expected):
        missing = sorted(set(expected) - actual_paths)
        extra = sorted(actual_paths - set(expected))
        raise RuntimeError(f"inventory differs: missing={missing}, extra={extra}")
    mismatches = [relative for relative, checksum in expected.items() if digest(ROOT / relative) != checksum]
    if mismatches:
        raise RuntimeError(f"checksum mismatches: {mismatches}")
    print(f"PASS: {len(expected)} release files verified")


if __name__ == "__main__":
    main()

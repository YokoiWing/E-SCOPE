#!/usr/bin/env python3
"""Recover the 28 byte-identical G0 netlists from an original experiment tree."""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
from pathlib import Path


HERE = Path(__file__).resolve().parent


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        raise RuntimeError(f"output already exists: {args.output}")

    points = json.loads((HERE / "manifest.json").read_text())["points"]
    needed = {point["g0_sha256"]: point for point in points}
    found: dict[str, Path] = {}
    for path in args.workspace.rglob("g0.v"):
        value = digest(path)
        if value in needed and value not in found:
            found[value] = path
        if len(found) == len(needed):
            break
    missing = sorted(set(needed) - set(found))
    if missing:
        raise RuntimeError(f"missing {len(missing)} G0 hashes: {missing}")

    args.output.mkdir(parents=True)
    receipt = []
    for value, point in needed.items():
        target = args.output / Path(point["g0_path"]).name
        shutil.copyfile(found[value], target)
        receipt.append(
            {
                "benchmark": point["benchmark"],
                "sha256": value,
                "source": str(found[value].relative_to(args.workspace)),
                "output": target.name,
            }
        )
    (args.output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"status": "PASS", "g0_files": len(receipt)}, indent=2))


if __name__ == "__main__":
    main()

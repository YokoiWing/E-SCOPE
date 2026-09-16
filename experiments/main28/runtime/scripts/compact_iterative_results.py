#!/usr/bin/env python3
"""Compact Iterative trajectories while retaining paper-grade evidence.

Dry-run is the default.  --apply copies every accepted round's selected
pre-A2 topology to a stable file, records its SHA256, and then removes only
reconstructible candidate forests, progressive stage caches, and large debug
traces.  Root summaries, checkpoints, round receipts, accepted netlists,
planner plans, external Genus evidence, and formal evidence are untouched.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
from pathlib import Path


DEBUG_FILES = (
    "timing_trace_debug.txt",
    "adjoint_debug.txt",
    "dual_seed_trace.json",
)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def tree_bytes(path: Path) -> int:
    if not path.exists():
        return 0
    if path.is_file() or path.is_symlink():
        return path.lstat().st_size
    return sum(item.lstat().st_size for item in path.rglob("*") if item.is_file())


def remove_path(path: Path, apply: bool) -> tuple[int, int]:
    if not path.exists():
        return 0, 0
    size = tree_bytes(path)
    files = 1 if path.is_file() else sum(1 for item in path.rglob("*") if item.is_file())
    if apply:
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink()
    return files, size


def preserve_leader(round_dir: Path, apply: bool) -> bool:
    receipt_path = round_dir / "round_receipt.json"
    if not receipt_path.is_file():
        return False
    receipt = json.loads(receipt_path.read_text())
    leader = receipt.get("leader")
    if not leader:
        return False
    candidates = (
        round_dir / "planner" / "materialized" / "candidates" / f"{leader}.v",
        round_dir / "materialized" / "candidates" / f"{leader}.v",
    )
    matches = [path for path in candidates if path.is_file()]
    stable = round_dir / "leader_pre_a2.v"
    sidecar = round_dir / "leader_pre_a2_receipt.json"
    if not matches and stable.is_file() and sidecar.is_file():
        payload = json.loads(sidecar.read_text())
        stable_sha = sha256(stable)
        if payload.get("candidate_id") != leader or payload.get("sha256") != stable_sha:
            raise RuntimeError(f"stable leader receipt mismatch: {round_dir}")
        return True
    if len(matches) != 1:
        raise RuntimeError(f"expected one pre-A2 leader for {round_dir}, found {matches}")
    source = matches[0]
    source_sha = sha256(source)
    if stable.exists() and sha256(stable) != source_sha:
        raise RuntimeError(f"existing stable leader differs: {stable}")
    if apply and not stable.exists():
        shutil.copy2(source, stable)
    payload = {
        "candidate_id": leader,
        "sha256": source_sha,
        "original_relative_path": str(source.relative_to(round_dir)),
        "round_receipt": "round_receipt.json",
    }
    if apply:
        sidecar.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=Path)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--manifest", type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    if not root.is_dir() or root.name != "iterative_internal_v3_main":
        raise SystemExit(f"refusing unexpected result root: {root}")

    round_dirs = sorted(path for path in root.glob("*/*/round_*") if path.is_dir())
    preserved = 0
    removed_files = 0
    removed_bytes = 0
    removals: list[dict[str, object]] = []

    for round_dir in round_dirs:
        if preserve_leader(round_dir, args.apply):
            preserved += 1
        targets = [
            round_dir / "materialized" / "candidates",
            round_dir / "planner" / "materialized" / "candidates",
        ]
        progressive = round_dir / "progressive"
        if progressive.is_dir():
            targets.extend(path for path in progressive.iterdir() if path.name.startswith("stage_"))
        targets.extend(round_dir / "planner" / name for name in DEBUG_FILES)
        for target in targets:
            files, size = remove_path(target, args.apply)
            if files:
                removals.append(
                    {
                        "path": str(target.relative_to(root)),
                        "files": files,
                        "bytes": size,
                    }
                )
                removed_files += files
                removed_bytes += size

    manifest = {
        "mode": "apply" if args.apply else "dry-run",
        "root": str(root),
        "round_directories": len(round_dirs),
        "preserved_pre_a2_leaders": preserved,
        "removed_files": removed_files,
        "removed_bytes": removed_bytes,
        "removed_gib": removed_bytes / 1024**3,
        "removals": removals,
    }
    manifest_path = args.manifest or root.parent / "ITERATIVE_COMPACTION_20260901.json"
    if args.apply:
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(json.dumps({key: value for key, value in manifest.items() if key != "removals"}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

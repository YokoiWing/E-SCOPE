#!/usr/bin/env python3
"""Verify packaged inputs/outputs and replay the frozen method-choice policy."""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True
from policy import decide


ROOT = Path(__file__).resolve().parent
SMALL_CASES = ("c432", "epfl_dec", "epfl_ctrl", "c1908", "c880", "epfl_router")
ITERATIVE_CONFIG_SHA256 = "0e9d50d4f4c95435bee9d7772906e196a8fe35cd5eef344e05e3c5e709ea0597"
TIMING_BOUNDARY_CONFIG_SHA256 = "c74fcf97016dffda862341a5fc4c14ce510f8d2f28742936b37a13665ceda4ca"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--small",
        action="store_true",
        help="replay six small policy cases after still checking all 28 G0 files",
    )
    args = parser.parse_args()
    manifest = json.loads((ROOT / "manifest.json").read_text())
    points = manifest["points"]
    if len(points) != 28 or len({p["benchmark"] for p in points}) != 28:
        raise RuntimeError("manifest must contain 28 unique benchmarks")
    iterative_config = ROOT / "runtime/config/iterative_search.json"
    if digest(iterative_config) != ITERATIVE_CONFIG_SHA256:
        raise RuntimeError("Iterative config does not match the historical RUN_MANIFEST")
    timing_boundary_config = ROOT / "runtime/config/timing_boundary_search.json"
    if digest(timing_boundary_config) != TIMING_BOUNDARY_CONFIG_SHA256:
        raise RuntimeError("Conquer timing-boundary config does not match the historical record")

    for point in points:
        path = ROOT / point["g0_path"]
        actual = digest(path)
        if actual != point["g0_sha256"]:
            raise RuntimeError(f"G0 SHA mismatch for {point['benchmark']}: {actual}")
        output = ROOT / point["optimized_path"]
        actual_output = digest(output)
        if actual_output != point["optimized_sha256"]:
            raise RuntimeError(
                f"optimized-netlist SHA mismatch for {point['benchmark']}: {actual_output}"
            )

    selected = [p for p in points if not args.small or p["benchmark"] in SMALL_CASES]
    decisions = []
    for point in selected:
        expected = point["paper_selected_method"]
        actual, clause = decide(point["policy_replay"]["features"])
        if actual != expected:
            raise RuntimeError(
                f"policy mismatch for {point['benchmark']}: expected {expected}, got {actual}"
            )
        decisions.append(
            {
                "benchmark": point["benchmark"],
                "anchor": point["anchor"],
                "selected_method": actual,
                "clause": clause,
                "same_anchor_replay": point["policy_replay"]["same_anchor_as_paper"],
            }
        )

    result = {
        "status": "PASS",
        "g0_files_verified": len(points),
        "optimized_netlists_verified": len(points),
        "policy_cases_verified": len(decisions),
        "scope": "small" if args.small else "all",
        "same_anchor_policy_records": sum(
            p["policy_replay"]["same_anchor_as_paper"] for p in points
        ),
        "method_only_transfer": [
            f"{p['benchmark']}/{p['anchor']}"
            for p in points
            if not p["policy_replay"]["same_anchor_as_paper"]
        ],
        "decisions": decisions,
    }
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

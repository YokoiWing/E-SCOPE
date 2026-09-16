#!/usr/bin/env python3
"""Frozen runtime-aware choice between the Iterative and Conquer searches."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


ITERATIVE_MATERIAL_ADVANTAGE_PP = 0.5
CONQUER_MATERIAL_ADVANTAGE_PP = 0.02
CONQUER_STRONG_GAIN_PERCENT = 31.0
PROJECTED_ITERATIVE_TO_CONQUER_LIMIT = 3.5
SMALL_DESIGN_INSTANCES = 1000


def decide(features: dict) -> tuple[str, str]:
    """Return the selected method and the first matching policy clause."""
    if features["iterative_completed_rounds"] == 0:
        return "Conquer", "no_iterative_checkpoint"
    if features["qor_gap_pp"] >= ITERATIVE_MATERIAL_ADVANTAGE_PP:
        return "Iterative", "iterative_materially_better"
    if not features["conquer_normal_lane"]:
        return "Conquer", "conquer_distinct_candidate"
    if features["conquer_qor_delta_percent"] <= -CONQUER_STRONG_GAIN_PERCENT:
        return "Conquer", "conquer_already_strong"
    if (
        features["projected_iterative_to_conquer_ratio"]
        <= PROJECTED_ITERATIVE_TO_CONQUER_LIMIT
    ):
        if features["qor_gap_pp"] <= -CONQUER_MATERIAL_ADVANTAGE_PP:
            return "Conquer", "conquer_materially_better"
        return "Iterative", "iterative_continuation_affordable"
    if features["instances"] <= SMALL_DESIGN_INSTANCES:
        return "Conquer", "iterative_continuation_expensive_small_design"
    return "Iterative", "large_design_wait_one_checkpoint"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("features", type=Path, help="JSON file containing policy features")
    args = parser.parse_args()
    features = json.loads(args.features.read_text())
    selected, clause = decide(features)
    print(json.dumps({"selected_method": selected, "clause": clause}, indent=2))


if __name__ == "__main__":
    main()

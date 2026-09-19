#!/usr/bin/env python3
"""Opt-in, exact-function PI identity elimination; no benchmark-name rule."""
import argparse
import json
from pathlib import Path

from semantic_shadow import load_cell_functions


def generate(state, functions):
    nodes = {int(row["anchor"]): row for row in state["occurrences"]}
    plans = []
    for anchor, row in sorted(nodes.items()):
        function = functions.get(row["op"])
        if row["is_leaf"] or function is None or len(function.pins) != 1 or function.truth != 2:
            continue
        if len(row["inputs"]) != 1:
            continue
        child = int(row["inputs"][0])
        if not nodes[child]["is_leaf"] or nodes[child].get("is_constant", False):
            continue
        plans.append({
            "candidate_id": f"PI_IDENTITY_BYPASS_P{anchor}",
            "choices": [{"root_anchor": anchor, "expression": {"kind": "anchor", "anchor": child}}],
            "proof_boundary": [child], "proof_outputs": [anchor],
            "provenance": ["exact-unary-identity", "pi-boundary-bypass-control", f"cell:{row['op']}", f"fanout:{len(row['consumers'])}"],
        })
    return plans


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    plans = generate(json.loads(args.state.read_text()), load_cell_functions(args.audit))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(plans, indent=2) + "\n")


if __name__ == "__main__":
    main()

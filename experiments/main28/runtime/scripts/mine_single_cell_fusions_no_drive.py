#!/usr/bin/env python3
"""Mine exact closed cones replaceable by one existing standard cell.

This is a bounded, experiment-only structural candidate provider.  It uses
complete truth tables to match two-to-four-gate single-output cones against
the existing FULL-COMB Liberty cells.  Emitted candidates still have to pass
the Rust materializer's independent exhaustive proof and full-parent NLDM.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import sys
import time
from collections import defaultdict
from itertools import permutations
from pathlib import Path


HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


local = load_module("fusion_local", HERE / "generate_local_resynthesis_rules.py")
builder = load_module(
    "fusion_builder", HERE / "build_asap7_6t_full_comb.py"
)


SUPPORTED_FAMILIES = {
    "AND2", "AND3", "AND4", "NAND2", "NAND3", "NAND4",
    "OR2", "OR3", "OR4", "NOR2", "NOR3", "NOR4",
    "INV", "BUF", "XOR2", "XNOR2", "MAJ", "MAJI",
    "AO21", "AOI21", "AO22", "AOI22", "AOI31", "AOI221",
    "OA21", "OAI21", "OA22", "OAI22", "O2A1O1I", "HB",
}


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def candidate_regions(nodes: dict[int, dict], max_gates: int):
    seen = set()
    for root in sorted(nodes):
        if nodes[root]["is_leaf"]:
            continue
        stack = [frozenset((root,))]
        while stack:
            region = stack.pop()
            key = (root, tuple(sorted(region)))
            if key in seen:
                continue
            seen.add(key)
            if len(region) >= 2:
                yield root, region
            if len(region) == max_gates:
                continue
            frontier = set()
            for anchor in region:
                for child in map(int, nodes[anchor]["inputs"]):
                    if child not in region and not nodes[child]["is_leaf"]:
                        frontier.add(child)
            for child in sorted(frontier, reverse=True):
                stack.append(region | {child})


def closed(root: int, region: frozenset[int], nodes: dict[int, dict]) -> bool:
    return all(
        anchor == root or set(map(int, nodes[anchor]["consumers"])) <= region
        for anchor in region
    )


def boundary(region: frozenset[int], nodes: dict[int, dict]) -> tuple[int, ...]:
    return tuple(
        sorted(
            {
                int(child)
                for anchor in region
                for child in nodes[anchor]["inputs"]
                if int(child) not in region
            }
        )
    )


def truth_of_cone(
    root: int,
    region: frozenset[int],
    cone_boundary: tuple[int, ...],
    nodes: dict[int, dict],
    functions: dict[str, tuple[tuple[str, ...], tuple]],
) -> int:
    boundary_index = {anchor: index for index, anchor in enumerate(cone_boundary)}
    memo: dict[tuple[int, int], bool] = {}

    def evaluate(anchor: int, vector: int) -> bool:
        key = (anchor, vector)
        if key in memo:
            return memo[key]
        if anchor not in region:
            value = bool((vector >> boundary_index[anchor]) & 1)
        else:
            node = nodes[anchor]
            pins, ast = functions[str(node["op"])]
            children = list(map(int, node["inputs"]))
            if len(pins) != len(children):
                raise ValueError(f"arity mismatch at {anchor}")
            values = {
                pin: evaluate(child, vector)
                for pin, child in zip(pins, children, strict=True)
            }
            value = bool(builder.eval_ast(ast, values))
        memo[key] = value
        return value

    result = 0
    for vector in range(1 << len(cone_boundary)):
        result |= int(evaluate(root, vector)) << vector
    return result


def target_truth(ast: tuple, pins: tuple[str, ...], pin_order: tuple[int, ...]) -> int:
    result = 0
    for vector in range(1 << len(pins)):
        values = {
            pin: bool((vector >> pin_order[index]) & 1)
            for index, pin in enumerate(pins)
        }
        result |= int(builder.eval_ast(ast, values)) << vector
    return result


def exact_scale_families(path: Path) -> dict[str, str]:
    """Return connected components from exact bidirectional scale rewrites."""
    parent: dict[str, str] = {}

    def find(cell: str) -> str:
        parent.setdefault(cell, cell)
        while parent[cell] != cell:
            parent[cell] = parent[parent[cell]]
            cell = parent[cell]
        return cell

    def union(left: str, right: str) -> None:
        left, right = find(left), find(right)
        if left != right:
            parent[max(left, right)] = min(left, right)

    document = json.loads(path.read_text())
    for rewrite in document.get("rewrites", []):
        if rewrite.get("bidirectional") is not True:
            continue
        cells = []
        for field in ("searcher", "applier"):
            match = re.match(r"^\(([^\s()]+)", str(rewrite.get(field, "")))
            if match:
                cells.append(match.group(1))
        if len(cells) == 2:
            union(*cells)
    groups: dict[str, list[str]] = defaultdict(list)
    for cell in parent:
        groups[find(cell)].append(cell)
    canonical = {cell: min(members) for members in groups.values() for cell in members}
    return canonical


def drive_seed_key(row: dict, exact_family: dict[str, str]) -> tuple:
    return (
        int(row["root"]),
        exact_family.get(str(row["cell"]), str(row["cell"])),
        tuple(map(int, row["pin_order"])),
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph", type=Path, required=True)
    parser.add_argument("--timing-state", type=Path, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--liberty", type=Path, required=True)
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--max-gates", type=int, default=4)
    parser.add_argument("--max-boundary", type=int, default=5)
    parser.add_argument("--variants-per-cone", type=int, default=12)
    parser.add_argument("--max-candidates", type=int, default=1024)
    parser.add_argument("--no-drive-expansion", action="store_true")
    parser.add_argument(
        "--no-drive-no-backfill",
        action="store_true",
        help="reserve Full per-cone/global slots before removing drive siblings",
    )
    parser.add_argument("--scale-rules", type=Path)
    args = parser.parse_args()
    if args.no_drive_expansion and args.scale_rules is None:
        parser.error("--scale-rules is required with --no-drive-expansion")
    if args.no_drive_no_backfill and not args.no_drive_expansion:
        parser.error("--no-drive-no-backfill requires --no-drive-expansion")

    args.out_dir.mkdir(parents=True, exist_ok=True)
    write_json(
        args.out_dir / "status.json",
        {"stage": "mining", "pid": os.getpid(), "updated_unix": time.time()},
    )
    graph = json.loads(args.graph.read_text())["occurrences"]
    nodes = {int(row["anchor"]): row for row in graph}
    timing_doc = json.loads(args.timing_state.read_text())
    timing = {int(row["anchor"]): row for row in timing_doc["anchors"]}
    audit = json.loads(args.audit.read_text())
    areas = local.read_liberty_areas(args.liberty)
    exact_family = exact_scale_families(args.scale_rules) if args.no_drive_expansion else {}

    functions = {}
    targets_by_arity: dict[int, list[tuple[str, tuple[str, ...], tuple, float]]] = defaultdict(list)
    for row in audit["included_cells"]:
        cell = str(row["cell"])
        ast = builder.BooleanParser(str(row["function"])).parse()
        pins = tuple(map(str, row["inputs"]))
        functions[cell] = (pins, ast)
        if local.logic_name(cell) in SUPPORTED_FAMILIES and cell in areas:
            targets_by_arity[len(pins)].append((cell, pins, ast, areas[cell]))

    target_index: dict[int, dict[int, list[tuple[float, str, tuple[int, ...]]]]] = {}
    for arity, cells in targets_by_arity.items():
        if arity > args.max_boundary:
            continue
        by_truth: dict[int, list[tuple[float, str, tuple[int, ...]]]] = defaultdict(list)
        for cell, pins, ast, area in cells:
            for pin_order in sorted(set(permutations(range(arity)))):
                by_truth[target_truth(ast, pins, pin_order)].append(
                    (area, cell, pin_order)
                )
        for values in by_truth.values():
            values.sort()
        target_index[arity] = by_truth

    rejected = defaultdict(int)
    discoveries = []
    unique = set()
    structural_seeds = set()
    excluded_drive_variants = 0
    screened = 0
    for root, region in candidate_regions(nodes, args.max_gates):
        if not closed(root, region, nodes):
            rejected["not_closed"] += 1
            continue
        cone_boundary = boundary(region, nodes)
        if not 1 <= len(cone_boundary) <= args.max_boundary:
            rejected["boundary"] += 1
            continue
        if any(
            str(nodes[anchor]["op"]) not in functions
            or local.logic_name(str(nodes[anchor]["op"])) not in SUPPORTED_FAMILIES
            for anchor in region
        ):
            rejected["unsupported_source"] += 1
            continue
        screened += 1
        try:
            truth = truth_of_cone(root, region, cone_boundary, nodes, functions)
        except Exception:
            rejected["truth_error"] += 1
            continue
        original_area = sum(areas[str(nodes[anchor]["op"])] for anchor in region)
        matches = target_index.get(len(cone_boundary), {}).get(truth, [])
        kept = 0
        for target_area, cell, pin_order in matches:
            saving = original_area - target_area
            if saving <= 1e-12:
                continue
            ordered_boundary = tuple(cone_boundary[index] for index in pin_order)
            seed_key = (root, exact_family.get(cell, cell), ordered_boundary)
            drive_sibling = args.no_drive_expansion and seed_key in structural_seeds
            if drive_sibling and not args.no_drive_no_backfill:
                excluded_drive_variants += 1
                continue
            key = (root, cell, ordered_boundary)
            if key in unique:
                continue
            unique.add(key)
            structural_seeds.add(seed_key)
            slack = min(
                float(timing[root]["slack_rise_ps"]),
                float(timing[root]["slack_fall_ps"]),
            )
            discoveries.append(
                {
                    "root": root,
                    "region": sorted(region),
                    "boundary": cone_boundary,
                    "cell": cell,
                    "pin_order": ordered_boundary,
                    "original_area": original_area,
                    "target_area": target_area,
                    "area_saving": saving,
                    "slack_ps": slack,
                    "truth_hex": hex(truth),
                    "_drive_sibling": drive_sibling,
                }
            )
            kept += 1
            if kept >= args.variants_per_cone:
                break

    discoveries.sort(
        key=lambda row: (
            -float(row["area_saving"]),
            float(row["slack_ps"]),
            int(row["root"]),
            str(row["cell"]),
            tuple(row["pin_order"]),
        )
    )
    discoveries = discoveries[: args.max_candidates]
    if args.no_drive_no_backfill:
        excluded_drive_variants += sum(
            bool(row["_drive_sibling"]) for row in discoveries
        )
        discoveries = [row for row in discoveries if not row["_drive_sibling"]]
    for row in discoveries:
        row.pop("_drive_sibling", None)
    drive_audit = {
        "bounded_candidates": len(discoveries),
        "native_structural_seeds": len(discoveries),
        "excluded_drive_variants_before_caps": excluded_drive_variants,
        "representative_rule": (
            "first exact-scale-component member in Full match order: minimum Liberty area, then cell name"
            if args.no_drive_expansion else "all Full candidates retained"
        ),
        "filter_stage": (
            "after per-cone and global slot reservation; removed slots are not backfilled"
            if args.no_drive_no_backfill
            else "target matching before per-cone and global caps"
        ),
        "drive_mapping_slot_reallocation_enabled": not args.no_drive_no_backfill,
    }
    plan = []
    for index, row in enumerate(discoveries):
        candidate_id = (
            f"SIN_GENERIC_FUSION_{index:04d}_P{row['root']}_"
            f"{str(row['cell']).split('_ASAP')[0]}"
        )
        plan.append(
            {
                "candidate_id": candidate_id,
                "provenance": [
                    "sin-generic-exact-single-cell-fusion",
                    f"root:{row['root']}",
                    "region:" + ",".join(map(str, row["region"])),
                    f"raw-area-saving:{row['area_saving']}",
                ],
                "choices": [
                    {
                        "root_anchor": row["root"],
                        "expression": {
                            "kind": "cell",
                            "op": row["cell"],
                            "children": [
                                {"kind": "anchor", "anchor": anchor}
                                for anchor in row["pin_order"]
                            ],
                        },
                    }
                ],
                "proof_boundary": sorted(set(row["boundary"])),
                "proof_outputs": [row["root"]],
            }
        )

    write_json(args.out_dir / "inventory.json", discoveries)
    write_json(args.out_dir / "candidate_plan.json", plan)
    write_json(
        args.out_dir / "summary.json",
        {
            "method": "bounded exact closed-cone to single-cell fusion",
            "screened_closed_cones": screened,
            "candidate_count": len(plan),
            "roots": len({row["root"] for row in discoveries}),
            "rejected": dict(rejected),
            "configuration": {
                "max_gates": args.max_gates,
                "max_boundary": args.max_boundary,
                "variants_per_cone": args.variants_per_cone,
                "max_candidates": args.max_candidates,
                "phase1_drive_expansion_enabled": not args.no_drive_expansion,
                "drive_mapping_slot_reallocation_enabled": not args.no_drive_no_backfill,
            },
            "drive_expansion_audit": drive_audit,
        },
    )
    write_json(
        args.out_dir / "status.json",
        {
            "stage": "complete",
            "pid": os.getpid(),
            "screened_closed_cones": screened,
            "candidate_count": len(plan),
            "updated_unix": time.time(),
        },
    )
    print(f"screened {screened} closed cones; emitted {len(plan)} candidates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Build a bounded portfolio of complete exact-fusion closures.

S0.5 changes the budgeted object from individual fusion primitives to complete
compatible sets.  Primitive variants are first compressed per root with a
white-box electrical Pareto audit; a multi-lane beam then selects at most 128
all-or-nothing closure macros.  Historical winners are read only after the
portfolio is frozen and are used solely as recall labels.
"""

from __future__ import annotations

import argparse
import json
import math
import re
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import semantic_shadow as shadow


def dump(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            value,
            indent=2,
            sort_keys=True,
            default=lambda item: sorted(item)
            if isinstance(item, (set, frozenset))
            else TypeError(f"not JSON serializable: {type(item).__name__}"),
        )
        + "\n"
    )


def choice_signature(choice: dict) -> str:
    return json.dumps(choice, sort_keys=True, separators=(",", ":"))


def expression_anchors(expression: dict) -> set[int]:
    if expression.get("kind") == "anchor":
        return {int(expression["anchor"])}
    return {
        anchor
        for child in expression.get("children", [])
        for anchor in expression_anchors(child)
    }


def logic_family(cell: str) -> str:
    base = cell.split("_ASAP", 1)[0]
    return re.sub(r"x(?:p?\d+).*", "", base)


def cell_blocks(path: Path) -> dict[str, str]:
    text = path.read_text()
    matches = list(re.finditer(r"\bcell\s*\(([^)]+)\)\s*\{", text))
    result = {}
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        result[match.group(1).strip()] = text[match.end() : end]
    return result


def numeric_values(text: str) -> list[float]:
    return [
        float(token)
        for token in re.findall(r"[-+]?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?", text)
    ]


def median_table(body: str, table_name: str) -> float:
    values = []
    for table in re.finditer(
        rf"{table_name}\s*\([^)]*\)\s*\{{(.*?)\n\s*\}}", body, re.S
    ):
        for block in re.findall(r"values\s*\((.*?)\)\s*;", table.group(1), re.S):
            values.extend(value for value in numeric_values(block) if math.isfinite(value))
    values.sort()
    return values[len(values) // 2] if values else float("inf")


def library_profiles(liberty: Path, audit: Path) -> dict[str, dict]:
    blocks = cell_blocks(liberty)
    functions = shadow.load_cell_functions(audit)
    profiles = {}
    for cell, body in blocks.items():
        leakage_match = re.search(r"\bcell_leakage_power\s*:\s*([^;]+);", body)
        leakage = float(leakage_match.group(1)) if leakage_match else 0.0
        pins = {}
        pin_matches = list(re.finditer(r"\bpin\s*\(([^)]+)\)\s*\{", body))
        for index, match in enumerate(pin_matches):
            end = pin_matches[index + 1].start() if index + 1 < len(pin_matches) else len(body)
            pin_body = body[match.end() : end]
            if not re.search(r"\bdirection\s*:\s*input\s*;", pin_body):
                continue
            cap_match = re.search(r"\bcapacitance\s*:\s*([^;]+);", pin_body)
            if cap_match:
                pins[match.group(1).strip()] = float(cap_match.group(1))
        function = functions.get(cell)
        profiles[cell] = {
            "leakage": leakage,
            "pin_capacitance": pins,
            "rise": median_table(body, "cell_rise"),
            "fall": median_table(body, "cell_fall"),
            "phase_at_zero": None if function is None else bool(function.truth & 1),
        }
    return profiles


def dominates(left: dict, right: dict) -> bool:
    # Phase, logical family, and pin assignment are semantic/electrical
    # categories, not scalar objectives.  Never discard one category merely
    # because a different category is numerically cheaper in this white-box
    # model.
    categories = ("family", "phase_at_zero", "pin_assignment")
    if any(left[key] != right[key] for key in categories):
        return False
    keys = ("target_area", "target_leakage", "cin_sum", "delay_rise", "delay_fall")
    return all(left[key] <= right[key] for key in keys) and any(
        left[key] < right[key] for key in keys
    )


def balanced_key(row: dict) -> tuple:
    return (
        row["target_area"]
        + row["target_leakage"]
        + row["cin_sum"]
        + row["delay_rise"]
        + row["delay_fall"],
        row["candidate_id"],
    )


def pareto_compress(rows: list[dict], limit: int) -> list[dict]:
    front = [row for row in rows if not any(dominates(other, row) for other in rows if other is not row)]
    # The per-root limit is a fill target, not permission to truncate a true
    # non-dominated front.  All Pareto points survive; scalar/category extrema
    # fill any remaining slots.
    selected = sorted(front, key=balanced_key)
    seen = {row["candidate_id"] for row in selected}
    categories: list[list[dict]] = []
    for key in ("target_area", "target_leakage", "cin_sum", "delay_rise", "delay_fall"):
        categories.append(sorted(rows, key=lambda row: (row[key], row["candidate_id"])))
    for field in ("family", "phase_at_zero", "pin_assignment"):
        grouped: dict[object, list[dict]] = defaultdict(list)
        for row in rows:
            grouped[row[field]].append(row)
        categories.append(
            [min(group, key=balanced_key) for _, group in sorted(grouped.items(), key=lambda item: str(item[0]))]
        )
    depth = 0
    while len(selected) < limit:
        progressed = False
        for category in categories:
            if depth >= len(category):
                continue
            row = category[depth]
            if row["candidate_id"] not in seen:
                selected.append(row)
                seen.add(row["candidate_id"])
                progressed = True
                if len(selected) == limit:
                    break
        if not progressed and all(depth >= len(category) for category in categories):
            break
        depth += 1
    return selected


@dataclass(frozen=True)
class BeamState:
    choices: tuple[int, ...]
    occupied: frozenset[int]
    families: frozenset[str]
    score: float


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--inventory", type=Path, required=True)
    parser.add_argument("--graph", type=Path, required=True)
    parser.add_argument(
        "--components",
        type=Path,
        help="optional affine component labels used only by the legacy static lanes",
    )
    parser.add_argument("--liberty", type=Path, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument(
        "--profiles",
        type=Path,
        help="optional Genus-blind whole-net Internal singleton profiles",
    )
    parser.add_argument(
        "--known-winner-plan",
        type=Path,
        help="optional post-freeze coverage audit; never used during generation",
    )
    parser.add_argument("--out-dir", type=Path, required=True)
    parser.add_argument("--max-variants-per-root", type=int, default=12)
    parser.add_argument("--beam-width", type=int, default=96)
    parser.add_argument("--max-closures", type=int, default=128)
    args = parser.parse_args()

    plans = json.loads(args.plan.read_text())
    inventory = json.loads(args.inventory.read_text())
    if len(plans) != len(inventory):
        raise RuntimeError("primitive plan/inventory cardinality mismatch")
    graph = json.loads(args.graph.read_text())
    nodes = {int(row["anchor"]): row for row in graph["occurrences"]}
    components = [] if args.components is None else json.loads(args.components.read_text())
    component_by_root = {
        int(anchor): row["component_id"]
        for row in components
        for anchor in row["source_occurrences"]
    }
    profiles = library_profiles(args.liberty, args.audit)
    audit_functions = shadow.load_cell_functions(args.audit)
    singleton_by_id = {}
    singleton_g0 = None
    if args.profiles is not None:
        profile_payload = json.loads(args.profiles.read_text())
        profile_rows = profile_payload.get("profiles", profile_payload)
        singleton_g0 = next(
            (row for row in profile_rows if row["candidate_id"] == "G0"), None
        )
        if singleton_g0 is None:
            raise RuntimeError("singleton profile set is missing G0")
        singleton_by_id = {
            str(row["candidate_id"]): row
            for row in profile_rows
            if row["candidate_id"] != "G0"
        }
        missing_profiles = sorted(
            candidate["candidate_id"]
            for candidate in plans
            if candidate["candidate_id"] not in singleton_by_id
        )
        if missing_profiles:
            raise RuntimeError(f"missing singleton profiles: {missing_profiles}")

    primitive_rows = []
    for index, (candidate, row) in enumerate(zip(plans, inventory, strict=True)):
        choice = candidate["choices"][0]
        root = int(choice["root_anchor"])
        expression = choice["expression"]
        cell = str(expression["op"])
        profile = profiles[cell]
        function = audit_functions[cell]
        ordered_pins = list(function.pins)
        children = expression["children"]
        pin_assignment = tuple(
            int(children[position]["anchor"]) for position in range(len(children))
        )
        pin_caps = [float(profile["pin_capacitance"].get(pin, 0.0)) for pin in ordered_pins]
        region = frozenset(map(int, row["region"]))
        original_leakage = sum(profiles[str(nodes[anchor]["op"])]["leakage"] for anchor in region)
        primitive = {
                "index": index,
                "candidate_id": candidate["candidate_id"],
                "candidate": candidate,
                "root": root,
                "region": region,
                "boundary": frozenset(expression_anchors(expression)),
                "family": logic_family(cell),
                "cell": cell,
                "phase_at_zero": profile["phase_at_zero"],
                "pin_assignment": pin_assignment,
                "target_area": float(row["target_area"]),
                "area_saving": float(row["area_saving"]),
                "target_leakage": float(profile["leakage"]),
                "leakage_saving": original_leakage - float(profile["leakage"]),
                "cin_sum": sum(pin_caps),
                "cin_max": max(pin_caps, default=0.0),
                "delay_rise": float(profile["rise"]),
                "delay_fall": float(profile["fall"]),
                "slack_ps": float(row["slack_ps"]),
                "removed_nodes": len(region) - 1,
                "parity_component": component_by_root.get(root),
            }
        if singleton_g0 is not None:
            singleton = singleton_by_id[candidate["candidate_id"]]
            primitive.update(
                {
                    "singleton_delay_ratio": float(singleton["delay_ps"])
                    / float(singleton_g0["delay_ps"]),
                    "singleton_area_ratio": float(singleton["area"])
                    / float(singleton_g0["area"]),
                    "singleton_power_ratio": float(singleton["power"])
                    / float(singleton_g0["power"]),
                    "singleton_d2ap_ratio": float(singleton["d2ap"])
                    / float(singleton_g0["d2ap"]),
                }
            )
        primitive_rows.append(primitive)

    by_root: dict[int, list[dict]] = defaultdict(list)
    for row in primitive_rows:
        by_root[row["root"]].append(row)
    compressed = []
    compression = []
    for root in sorted(by_root):
        kept = pareto_compress(by_root[root], args.max_variants_per_root)
        compressed.extend(kept)
        compression.append(
            {
                "root": root,
                "input_variants": len(by_root[root]),
                "kept_variants": len(kept),
                "kept_candidate_ids": [row["candidate_id"] for row in kept],
            }
        )

    maxima = {
        key: max((abs(float(row[key])) for row in compressed), default=1.0) or 1.0
        for key in (
            "area_saving",
            "leakage_saving",
            "cin_sum",
            "delay_rise",
            "delay_fall",
            "removed_nodes",
        )
    }
    if singleton_g0 is not None:
        # Each item has already been placed back into the complete netlist.
        # Add log improvements so a closure score is the first-order product
        # estimate for that objective; the later complete-closure profile is
        # still authoritative and captures interaction/path migration.
        lane_weights = {
            "d2ap": (2.0, 1.0, 1.0),
            "delay": (4.0, 0.15, 0.15),
            "area": (0.15, 2.0, 0.15),
            "power": (0.15, 0.15, 2.0),
            "da": (2.0, 1.0, 0.0),
            "dp": (2.0, 0.0, 1.0),
            "ap": (0.0, 1.0, 1.0),
        }
    else:
        lane_weights = {
            "area": (1.5, 0.1, 0.1, 0.1, 0.2, 0.0),
            "power": (0.2, 1.5, 0.1, 0.1, 0.2, 0.0),
            "timing": (0.2, 0.1, 0.8, 1.5, 0.1, 0.0),
            "balanced": (0.8, 0.5, 0.5, 0.7, 0.4, 0.0),
            "parity": (0.5, 0.3, 0.3, 0.5, 0.3, 0.8),
            "depth": (0.4, 0.2, 0.2, 0.3, 1.2, 0.0),
            "noncritical": (0.8, 0.4, 0.2, -0.8, 0.3, 0.0),
        }

    def item_score(row: dict, lane: str) -> float:
        if singleton_g0 is not None:
            gains = (
                -math.log(max(float(row["singleton_delay_ratio"]), 1e-300)),
                -math.log(max(float(row["singleton_area_ratio"]), 1e-300)),
                -math.log(max(float(row["singleton_power_ratio"]), 1e-300)),
            )
            return sum(
                weight_value * value
                for weight_value, value in zip(lane_weights[lane], gains, strict=True)
            )
        area = row["area_saving"] / maxima["area_saving"]
        power = row["leakage_saving"] / maxima["leakage_saving"]
        electrical = 1.0 - 0.5 * (
            row["cin_sum"] / maxima["cin_sum"]
            + max(row["delay_rise"], row["delay_fall"])
            / max(maxima["delay_rise"], maxima["delay_fall"])
        )
        criticality = 1.0 / (1.0 + max(0.0, row["slack_ps"]) / 20.0)
        depth = row["removed_nodes"] / maxima["removed_nodes"]
        parity = float(row["parity_component"] is not None)
        weights = lane_weights[lane]
        return sum(
            weight_value * value
            for weight_value, value in zip(
                weights, (area, power, electrical, criticality, depth, parity), strict=True
            )
        )

    # Hard incompatibility includes overlap and references to a node that the
    # other primitive would delete.  A reference to the other selected root is
    # an explicit, legal dependency and is retained in the audit graph.
    conflicts: dict[int, set[int]] = defaultdict(set)
    dependencies = []
    for left in range(len(compressed)):
        for right in range(left + 1, len(compressed)):
            a, b = compressed[left], compressed[right]
            conflict = (
                a["root"] == b["root"]
                or bool(a["region"] & b["region"])
                or bool(a["boundary"] & (b["region"] - {b["root"]}))
                or bool(b["boundary"] & (a["region"] - {a["root"]}))
            )
            if conflict:
                conflicts[left].add(right)
                conflicts[right].add(left)
                continue
            if b["root"] in a["boundary"]:
                dependencies.append([right, left])
            if a["root"] in b["boundary"]:
                dependencies.append([left, right])

    roots = sorted(by_root)
    compressed_by_root: dict[int, list[int]] = defaultdict(list)
    for index, row in enumerate(compressed):
        compressed_by_root[row["root"]].append(index)
    target_sizes = (2, 4, 6, 7, 8, 10, 12, 16, 24, 32)
    maximum_size = min(max(target_sizes), len(roots))
    proposal_states: list[tuple[str, int, BeamState]] = []
    for lane in lane_weights:
        buckets: dict[int, list[BeamState]] = {
            0: [BeamState((), frozenset(), frozenset(), 0.0)]
        }
        for root in roots:
            next_buckets: dict[int, list[BeamState]] = defaultdict(list)
            for size, states in buckets.items():
                next_buckets[size].extend(states)
                if size == maximum_size:
                    continue
                for state in states:
                    state_choices = set(state.choices)
                    for item in compressed_by_root[root]:
                        row = compressed[item]
                        if row["region"] & state.occupied:
                            continue
                        if any(item in conflicts[chosen] for chosen in state_choices):
                            continue
                        families = state.families | {row["family"]}
                        diversity_bonus = 0.015 * (len(families) - len(state.families))
                        next_buckets[size + 1].append(
                            BeamState(
                                state.choices + (item,),
                                state.occupied | row["region"],
                                families,
                                state.score + item_score(row, lane) + diversity_bonus,
                            )
                        )
            buckets = {}
            for size, states in next_buckets.items():
                unique = {}
                for state in states:
                    identity = tuple(state.choices)
                    old = unique.get(identity)
                    if old is None or state.score > old.score:
                        unique[identity] = state
                buckets[size] = sorted(
                    unique.values(), key=lambda state: (-state.score, state.choices)
                )[: args.beam_width]
        for size in target_sizes:
            for state in buckets.get(size, [])[:2]:
                proposal_states.append((lane, size, state))

    # Interleave size/lane strata before applying the portfolio cap.
    by_stratum: dict[tuple[int, str], list[BeamState]] = defaultdict(list)
    for lane, size, state in proposal_states:
        by_stratum[(size, lane)].append(state)
    ordered_strata = sorted(by_stratum)
    frozen = []
    seen_closures = set()
    depth = 0
    while len(frozen) < args.max_closures:
        progressed = False
        for stratum in ordered_strata:
            states = by_stratum[stratum]
            if depth >= len(states):
                continue
            state = states[depth]
            identity = tuple(sorted(compressed[index]["candidate_id"] for index in state.choices))
            if identity in seen_closures:
                continue
            seen_closures.add(identity)
            frozen.append((stratum[1], stratum[0], state))
            progressed = True
            if len(frozen) == args.max_closures:
                break
        if not progressed:
            break
        depth += 1

    closure_plan = []
    closure_receipts = []
    for number, (lane, size, state) in enumerate(frozen):
        rows = sorted((compressed[index] for index in state.choices), key=lambda row: row["root"])
        closure_plan.append(
            {
                "candidate_id": f"CLOSURE_AWARE_{number:03d}_{lane.upper()}_{size}CUTS",
                "provenance": [
                    "closure-aware-exact-fusion-portfolio-v1",
                    f"lane:{lane}",
                    f"cuts:{size}",
                    "primitive-budget-applied-after-closure",
                ],
                "choices": [row["candidate"]["choices"][0] for row in rows],
                "proof_boundary": [],
                "proof_outputs": [],
                "proof_groups": [
                    {
                        "choices": row["candidate"]["choices"],
                        "boundary": row["candidate"]["proof_boundary"],
                        "outputs": row["candidate"]["proof_outputs"],
                    }
                    for row in rows
                ],
            }
        )
        closure_receipts.append(
            {
                "candidate_id": closure_plan[-1]["candidate_id"],
                "lane": lane,
                "cut_count": size,
                "beam_score": state.score,
                "primitive_ids": [row["candidate_id"] for row in rows],
                "roots": [row["root"] for row in rows],
                "regions_pairwise_compatible": True,
            }
        )

    coverage = {
        "known_winner_supplied": args.known_winner_plan is not None,
        "label_used_during_generation": False,
    }
    if args.known_winner_plan is not None:
        known = json.loads(args.known_winner_plan.read_text())[0]
        known_signatures = {choice_signature(choice) for choice in known["choices"]}
        compressed_signatures = {
            choice_signature(row["candidate"]["choices"][0]) for row in compressed
        }
        exact_matches = []
        containing = []
        root_matches = []
        known_roots = {int(choice["root_anchor"]) for choice in known["choices"]}
        for candidate in closure_plan:
            signatures = {choice_signature(choice) for choice in candidate["choices"]}
            roots_in_candidate = {int(choice["root_anchor"]) for choice in candidate["choices"]}
            if signatures == known_signatures:
                exact_matches.append(candidate["candidate_id"])
            if known_signatures <= signatures:
                containing.append(candidate["candidate_id"])
            if roots_in_candidate == known_roots:
                root_matches.append(candidate["candidate_id"])
        coverage.update(
            {
                "known_candidate_id": known.get("candidate_id"),
                "known_choice_count": len(known_signatures),
                "known_choices_surviving_root_pareto": len(
                    known_signatures & compressed_signatures
                ),
                "all_known_choices_survive_root_pareto": (
                    known_signatures <= compressed_signatures
                ),
                "exact_closure_matches": exact_matches,
                "exact_closure_covered": bool(exact_matches),
                "proposal_contains_all_known_choices": containing,
                "contained_closure_covered": bool(containing),
                "exact_root_set_matches": root_matches,
                "exact_root_set_covered": bool(root_matches),
            }
        )
    args.out_dir.mkdir(parents=True, exist_ok=True)
    dump(args.out_dir / "candidate_plan.json", closure_plan)
    dump(args.out_dir / "primitive_pareto_inventory.json", compressed)
    dump(args.out_dir / "root_compression.json", compression)
    dump(args.out_dir / "compatibility_graph.json", {
        "compressed_primitives": len(compressed),
        "conflict_edges": sum(len(values) for values in conflicts.values()) // 2,
        "dependency_edges": sorted(dependencies),
    })
    dump(args.out_dir / "closure_receipts.json", closure_receipts)
    dump(args.out_dir / "coverage_audit.json", coverage)
    summary = {
        "method": (
            "whole-net singleton-profiled compatibility closure beam v2"
            if singleton_g0 is not None
            else "root-Pareto then compatibility-aware complete-closure beam v1"
        ),
        "input_primitive_count": len(plans),
        "root_count": len(by_root),
        "pareto_primitive_count": len(compressed),
        "max_variants_per_root": args.max_variants_per_root,
        "closure_proposal_count": len(closure_plan),
        "closure_cap": args.max_closures,
        "closure_sizes": sorted({len(row["choices"]) for row in closure_plan}),
        "coverage": coverage,
        "genus_calls": 0,
        "singleton_profiles": None
        if args.profiles is None
        else str(args.profiles.resolve()),
        "acceptance_authority": False,
    }
    dump(args.out_dir / "summary.json", summary)
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

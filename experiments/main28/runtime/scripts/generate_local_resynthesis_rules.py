#!/usr/bin/env python3
"""Generate a small local-resynthesis rule pack from an existing Hybrid plan.

The generated rules are forward-only exact Boolean identities.  A rule's
searcher is the current mapped cone in a selected window; its applier is a
different Boolean DAG found by bounded truth-table enumeration over the same
boundary signals.  Drive strengths are deliberately fixed to the smallest
legal cells here; the normal downstream A2 sizing stage remains responsible
for sizing.

This is a bounded prototype, not a global resynthesis engine.  It is intended
to inject a few high-quality structural choices into the same EGG rewrite
space so that existing region planning and extraction can be reused unchanged.
"""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path


INSTANCE_RE = re.compile(r"(?ms)^\s*(\w+_ASAP7_6t_L)\s+(\w+)\s*\((.*?)\)\s*;")
CONNECTION_RE = re.compile(r"\.(\w+)\s*\(\s*([^()]+?)\s*\)")
CELL_TOKEN_RE = re.compile(r"[A-Za-z0-9]+_ASAP7[^(),|\s]+")
DRIVE_RE = re.compile(r"x(?:p)?[0-9].*$")


@dataclass(frozen=True)
class Gate:
    cell: str
    instance: str
    output: str
    inputs: tuple[tuple[str, str], ...]


@dataclass(frozen=True)
class Expr:
    truth: int
    cost: int
    depth: int
    text: str
    key: str


def strip_comments(text: str) -> str:
    return re.sub(r"//[^\n]*", "", text)


def parse_netlist(path: Path) -> tuple[dict[str, Gate], dict[str, str]]:
    text = strip_comments(path.read_text())
    by_output: dict[str, Gate] = {}
    by_instance: dict[str, str] = {}
    for cell, instance, body in INSTANCE_RE.findall(text):
        conns = {pin: signal.strip() for pin, signal in CONNECTION_RE.findall(body)}
        output = conns.get("Y")
        if output is None:
            continue
        ordered = tuple((pin, signal) for pin, signal in conns.items() if pin != "Y")
        gate = Gate(cell, instance, output, ordered)
        by_output[output] = gate
        by_instance[instance] = output
    return by_output, by_instance


def logic_name(cell: str) -> str:
    base = cell.split("_ASAP", 1)[0]
    return DRIVE_RE.sub("", base)


def read_liberty_areas(path: Path | None) -> dict[str, float]:
    """Read only cell area; NLDM remains the downstream authority.

    This deliberately does not estimate delay or power.  Area is used only to
    keep a small, technology-aware Pareto set among Boolean-equivalent DAGs;
    every survivor still goes through the normal physical P1/A2/Exact-V2 path.
    """
    if path is None:
        return {}
    text = path.read_text()
    matches = list(re.finditer(r"\bcell\s*\(([^)]+)\)\s*\{", text))
    areas: dict[str, float] = {}
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        body = text[match.end() : end]
        area = re.search(r"\barea\s*:\s*([0-9.eE+-]+)", body)
        if area:
            try:
                areas[match.group(1).strip()] = float(area.group(1))
            except ValueError:
                pass
    return areas


def expression_technology(text: str, areas: dict[str, float]) -> tuple[float, str]:
    cells = re.findall(r"\(([A-Za-z0-9]+_ASAP7_6t_L)", text)
    area = sum(areas.get(cell, 0.0) for cell in cells)
    families = sorted({logic_name(cell) for cell in cells})
    return area, "+".join(families)


def gate_truth(name: str, values: list[int]) -> int | None:
    n = logic_name(name)
    if n.startswith("INV"):
        return 1 - values[0]
    if n.startswith("BUF"):
        return values[0]
    match = re.match(r"^(NAND|AND|NOR|OR)(\d+)$", n)
    if match:
        op, _ = match.groups()
        if op in {"AND", "NAND"}:
            result = int(all(values))
        else:
            result = int(any(values))
        return 1 - result if op.startswith("N") else result
    if n in {"XOR2", "XNOR2"}:
        result = values[0] ^ values[1]
        return 1 - result if n == "XNOR2" else result
    if n in {"MAJ", "MAJI"}:
        result = int(sum(values[:3]) >= 2)
        return 1 - result if n == "MAJI" else result
    if n in {"AO21", "AOI21"}:
        result = (values[0] & values[1]) | values[2]
        return 1 - result if n == "AOI21" else result
    if n in {"OA21", "OAI21"}:
        result = (values[0] | values[1]) & values[2]
        return 1 - result if n == "OAI21" else result
    if n in {"AO22", "AOI22"}:
        result = (values[0] & values[1]) | (values[2] & values[3])
        return 1 - result if n == "AOI22" else result
    if n in {"OA22", "OAI22"}:
        result = (values[0] | values[1]) & (values[2] | values[3])
        return 1 - result if n == "OAI22" else result
    if n == "O2A1O1I":
        # ASAP7 Y = (!A1*!A2*!C) + (!B*!C), ordered A1,A2,B,C.
        return int(
            ((1 - values[0]) & (1 - values[1]) & (1 - values[3]))
            | ((1 - values[2]) & (1 - values[3]))
        )
    return None


def cone_expression(
    signal: str,
    region: set[int],
    by_output: dict[str, Gate],
    boundary: dict[str, str],
) -> tuple[str, int, set[str]] | None:
    gate = by_output.get(signal)
    if gate is None:
        if signal not in boundary:
            boundary[signal] = f"?b{len(boundary)}"
        return boundary[signal], 0, set()
    match = re.fullmatch(r"n_(\d+)", signal)
    occurrence = int(match[1]) if match else None
    children = []
    gate_ids: set[str] = set()
    for _pin, child_signal in gate.inputs:
        child_match = re.fullmatch(r"n_(\d+)", child_signal)
        child_occurrence = int(child_match[1]) if child_match else None
        if child_occurrence is not None and child_occurrence in region:
            nested = cone_expression(child_signal, region, by_output, boundary)
        else:
            nested = None
        if nested is None:
            if child_signal not in boundary:
                boundary[child_signal] = f"?b{len(boundary)}"
            nested = (boundary[child_signal], 0, set())
        children.append(nested[0])
        gate_ids.update(nested[2])
    if occurrence is not None:
        gate_ids.add(str(occurrence))
    return f"({gate.cell} {' '.join(children)})", 1 + sum(
        0 for _ in children
    ), gate_ids


def eval_cone(
    signal: str,
    by_output: dict[str, Gate],
    boundary_order: list[str],
    assignment: int,
    memo: dict[str, int],
) -> int | None:
    if signal in memo:
        return memo[signal]
    if signal in boundary_order:
        value = (assignment >> boundary_order.index(signal)) & 1
        memo[signal] = value
        return value
    gate = by_output.get(signal)
    if gate is None:
        try:
            value = (assignment >> boundary_order.index(signal)) & 1
        except ValueError:
            return None
        memo[signal] = value
        return value
    values = []
    for _pin, child in gate.inputs:
        value = eval_cone(child, by_output, boundary_order, assignment, memo)
        if value is None:
            return None
        values.append(value)
    result = gate_truth(gate.cell, values)
    if result is None:
        return None
    memo[signal] = result
    return result


def target_truth(signal: str, by_output: dict[str, Gate], boundary_order: list[str]) -> int | None:
    width = 1 << len(boundary_order)
    result = 0
    for assignment in range(width):
        value = eval_cone(signal, by_output, boundary_order, assignment, {})
        if value is None:
            return None
        result |= value << assignment
    return result


def expr_states(boundary_count: int, max_cost: int, full_mask: int) -> dict[int, dict[int, list[Expr]]]:
    states: dict[int, dict[int, list[Expr]]] = {cost: {} for cost in range(max_cost + 1)}
    for index in range(boundary_count):
        truth = sum(((assignment >> index) & 1) << assignment for assignment in range(1 << boundary_count))
        expr = Expr(truth, 0, 0, f"?b{index}", f"b{index}")
        states[0].setdefault(truth, []).append(expr)

    def add(cost: int, expr: Expr) -> None:
        bucket = states[cost].setdefault(expr.truth, [])
        if any(existing.key == expr.key for existing in bucket):
            return
        bucket.append(expr)
        bucket.sort(key=lambda item: (item.depth, item.key))
        del bucket[4:]

    def values_at(cost: int) -> list[Expr]:
        return [expr for bucket in states[cost].values() for expr in bucket]

    def ordered_pair(left: Expr, right: Expr) -> tuple[Expr, Expr]:
        return (left, right) if left.key <= right.key else (right, left)

    def add_binary(cost: int, left: Expr, right: Expr) -> None:
        left, right = ordered_pair(left, right)
        operations = (
            ("and", "AND2x2_ASAP7_6t_L", left.truth & right.truth),
            ("nand", "NAND2x1_ASAP7_6t_L", full_mask ^ (left.truth & right.truth)),
            ("or", "OR2x2_ASAP7_6t_L", left.truth | right.truth),
            ("nor", "NOR2x1_ASAP7_6t_L", full_mask ^ (left.truth | right.truth)),
            ("xor", "XOR2x2_ASAP7_6t_L", left.truth ^ right.truth),
            ("xnor", "XNOR2x2_ASAP7_6t_L", full_mask ^ (left.truth ^ right.truth)),
        )
        for op, cell, truth in operations:
            add(
                cost,
                Expr(
                    truth,
                    cost,
                    max(left.depth, right.depth) + 1,
                    f"({cell} {left.text} {right.text})",
                    f"{op}({left.key},{right.key})",
                ),
            )

    def add_ternary(cost: int, first: Expr, second: Expr, third: Expr) -> None:
        first, second = ordered_pair(first, second)
        pair_key = f"{first.key},{second.key}"
        depth = max(first.depth, second.depth, third.depth) + 1
        operations = (
            (
                "ao21",
                "AO21x1_ASAP7_6t_L",
                (first.truth & second.truth) | third.truth,
                f"{pair_key};{third.key}",
            ),
            (
                "aoi21",
                "AOI21xp5_ASAP7_6t_L",
                full_mask ^ ((first.truth & second.truth) | third.truth),
                f"{pair_key};{third.key}",
            ),
            (
                "oa21",
                "OA21x2_ASAP7_6t_L",
                (first.truth | second.truth) & third.truth,
                f"{pair_key};{third.key}",
            ),
            (
                "oai21",
                "OAI21xp5_ASAP7_6t_L",
                full_mask ^ ((first.truth | second.truth) & third.truth),
                f"{pair_key};{third.key}",
            ),
        )
        for op, cell, truth, key in operations:
            add(
                cost,
                Expr(
                    truth,
                    cost,
                    depth,
                    f"({cell} {first.text} {second.text} {third.text})",
                    f"{op}({key})",
                ),
            )
        ordered = sorted((first, second, third), key=lambda item: item.key)
        majority = (ordered[0].truth & ordered[1].truth) | (
            ordered[0].truth & ordered[2].truth
        ) | (ordered[1].truth & ordered[2].truth)
        majority_key = ",".join(item.key for item in ordered)
        majority_text = " ".join(item.text for item in ordered)
        add(
            cost,
            Expr(
                majority,
                cost,
                depth,
                f"(MAJx1_ASAP7_6t_L {majority_text})",
                f"maj({majority_key})",
            ),
        )
        add(
            cost,
            Expr(
                full_mask ^ majority,
                cost,
                depth,
                f"(MAJIxp5_ASAP7_6t_L {majority_text})",
                f"maji({majority_key})",
            ),
        )

    def add_quaternary(cost: int, a: Expr, b: Expr, c: Expr, d: Expr) -> None:
        a, b = ordered_pair(a, b)
        c, d = ordered_pair(c, d)
        if (a.key, b.key) > (c.key, d.key):
            a, b, c, d = c, d, a, b
        depth = max(a.depth, b.depth, c.depth, d.depth) + 1
        ao = (a.truth & b.truth) | (c.truth & d.truth)
        oa = (a.truth | b.truth) & (c.truth | d.truth)
        key = f"{a.key},{b.key};{c.key},{d.key}"
        text = f"{a.text} {b.text} {c.text} {d.text}"
        for op, cell, truth in (
            ("ao22", "AO22x1_ASAP7_6t_L", ao),
            ("aoi22", "AOI22xp5_ASAP7_6t_L", full_mask ^ ao),
            ("oa22", "OA22x2_ASAP7_6t_L", oa),
            ("oai22", "OAI22xp5_ASAP7_6t_L", full_mask ^ oa),
        ):
            add(
                cost,
                Expr(truth, cost, depth, f"({cell} {text})", f"{op}({key})"),
            )

    for cost in range(1, max_cost + 1):
        previous = states[cost - 1]
        for truth, values in previous.items():
            for value in values:
                add(cost, Expr(full_mask ^ value.truth, cost, value.depth + 1, f"(INVx1_ASAP7_6t_L {value.text})", f"inv({value.key})"))
        for left_cost in range(cost):
            right_cost = cost - left_cost - 1
            if right_cost < 0 or right_cost >= len(states):
                continue
            for left_truth, left_values in states[left_cost].items():
                for right_truth, right_values in states[right_cost].items():
                    for left in left_values:
                        for right in right_values:
                            add_binary(cost, left, right)

        # FULL-186 complex gates are crucial here: the downstream structural
        # adapter intentionally caps a local realization at three gates.  A
        # NAND/NOR-only resynthesizer often discovers an exact function but
        # needs four or more nodes, making the choice invisible to extraction.
        # Enumerating AO/OA/MAJ cells lets a refactored 3--6-input cone remain
        # inside that unchanged cap.
        for first_cost in range(cost):
            for second_cost in range(cost - first_cost):
                third_cost = cost - first_cost - second_cost - 1
                if third_cost < 0:
                    continue
                for first in values_at(first_cost):
                    for second in values_at(second_cost):
                        for third in values_at(third_cost):
                            add_ternary(cost, first, second, third)

        # Four-input cells are most useful as a one-gate cut replacement.  At
        # higher costs their Cartesian product dominates runtime without
        # helping the fixed three-gate prototype, so keep this deliberately
        # bounded to leaves.
        if cost == 1:
            leaves = values_at(0)
            for a in leaves:
                for b in leaves:
                    for c in leaves:
                        for d in leaves:
                            add_quaternary(cost, a, b, c, d)
    return states


def alternatives(
    target: int,
    boundary_count: int,
    max_cost: int,
    full_mask: int,
    original: str,
    limit: int,
    areas: dict[str, float],
) -> list[str]:
    states = expr_states(boundary_count, max_cost, full_mask)
    result: list[Expr] = []
    seen = set()
    for cost in range(max_cost + 1):
        for expr in states[cost].get(target, []):
            if expr.text == original or expr.key in seen:
                continue
            seen.add(expr.key)
            result.append(expr)
    # Keep equivalent implementations from different technology families in
    # the small output set.  Area is only a deterministic generator hint; it
    # never replaces the downstream exact evaluator.
    result.sort(
        key=lambda item: (
            item.cost,
            expression_technology(item.text, areas)[0],
            item.depth,
            expression_technology(item.text, areas)[1],
            item.key,
        )
    )
    return [expr.text for expr in result[:limit]]


def region_roots(region: set[int], by_output: dict[str, Gate]) -> list[int]:
    consumers: dict[int, set[int]] = {node: set() for node in region}
    for node in region:
        gate = by_output.get(f"n_{node}")
        if gate is None:
            continue
        for _pin, signal in gate.inputs:
            match = re.fullmatch(r"n_(\d+)", signal)
            if match and int(match[1]) in region:
                consumers[int(match[1])].add(node)
    return [node for node in sorted(region) if not consumers[node]]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", required=True)
    parser.add_argument("--plan", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--max-regions", type=int, default=16)
    parser.add_argument("--roots-per-region", type=int, default=1)
    parser.add_argument("--alternatives-per-root", type=int, default=2)
    # The Rust structural adapter keeps at most three gates per local
    # realization and gives each local e-graph a bounded saturation window.
    # Complex AO/OA/MAJ enumeration grows sharply beyond two gates; callers
    # can opt into a larger, offline audit explicitly, but the safe default
    # should remain a bounded cut-resynthesis prototype.
    parser.add_argument("--max-cost", type=int, default=2)
    parser.add_argument("--min-cone-gates", type=int, default=2)
    parser.add_argument(
        "--liberty",
        type=Path,
        help="optional Liberty used only for technology-aware area/family ordering",
    )
    args = parser.parse_args()

    by_output, _ = parse_netlist(Path(args.input))
    areas = read_liberty_areas(args.liberty)
    plan = json.loads(Path(args.plan).read_text())
    regions: dict[str, set[int]] = {}
    for row in plan:
        regions.setdefault(row["region_id"], set()).update(
            int(choice["eclass_id"].split(":", 1)[1]) for choice in row.get("choices", [])
        )
    region_items = sorted(regions.items(), key=lambda item: (len(item[1]), item[0]), reverse=True)[: args.max_regions]
    rewrites = []
    audit = []
    for region_id, region in region_items:
        roots = region_roots(region, by_output)
        scored_roots = []
        for root in roots:
            boundary: dict[str, str] = {}
            expression = cone_expression(f"n_{root}", region, by_output, boundary)
            if expression is None:
                continue
            pattern, _unused, gate_ids = expression
            scored_roots.append((len(gate_ids), root, pattern, boundary))
        scored_roots.sort(key=lambda item: (-item[0], item[1]))
        region_audit = {"region_id": region_id, "region_size": len(region), "roots": []}
        for gate_count, root, pattern, boundary in scored_roots[: args.roots_per_region]:
            if gate_count < args.min_cone_gates:
                continue
            ordered_boundary = sorted(boundary.values(), key=lambda value: int(value[2:]))
            target = target_truth(f"n_{root}", by_output, [signal for signal, _ in sorted(boundary.items(), key=lambda item: int(item[1][2:]))])
            if target is None or len(ordered_boundary) > 6:
                continue
            alternatives_found = alternatives(
                target,
                len(ordered_boundary),
                args.max_cost,
                (1 << (1 << len(ordered_boundary))) - 1,
                pattern,
                args.alternatives_per_root,
                areas,
            )
            root_audit = {
                "root": root,
                "cone_gate_count": gate_count,
                "boundary_count": len(ordered_boundary),
                "alternatives": alternatives_found,
                "technology": [
                    {
                        "area": expression_technology(item, areas)[0],
                        "families": expression_technology(item, areas)[1],
                    }
                    for item in alternatives_found
                ],
            }
            region_audit["roots"].append(root_audit)
            for index, applier in enumerate(alternatives_found):
                rewrites.append(
                    {
                        "name": f"LOCAL_RESYNTH_{region_id}_N{root}_{index}",
                        "searcher": pattern,
                        "applier": applier,
                        "bidirectional": False,
                    }
                )
        audit.append(region_audit)

    output = {"rewrites": rewrites}
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(output, indent=2, sort_keys=True) + "\n")
    audit_path = out.with_name(out.stem + "_audit.json")
    audit_path.write_text(
        json.dumps(
            {
                "input": args.input,
                "plan": args.plan,
                "liberty": str(args.liberty) if args.liberty else None,
                "technology_area_cells": len(areas),
                "rule_count": len(rewrites),
                "regions": audit,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    print(json.dumps({"rule_count": len(rewrites), "audit": str(audit_path), "rules": str(out)}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

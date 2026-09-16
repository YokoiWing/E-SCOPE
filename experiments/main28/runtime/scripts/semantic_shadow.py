#!/usr/bin/env python3
"""Shared Boolean shadow primitives for semantic-provider experiments.

The physical occurrence graph remains authoritative.  This module only adds
cell-function semantics from the audited Liberty inventory and never merges
physical occurrences.  It is intentionally independent of the online Iterative
controller so S0/M0 recovery experiments cannot change accepted trajectories.
"""

from __future__ import annotations

import importlib.util
import json
import sys
from dataclasses import dataclass
from itertools import product
from pathlib import Path
from typing import Iterable


HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def _load_module(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot import {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


builder = _load_module(
    "semantic_shadow_liberty_builder",
    HERE / "build_asap7_6t_full_comb.py",
)


@dataclass(frozen=True)
class CellFunction:
    cell: str
    pins: tuple[str, ...]
    expression: str
    ast: tuple
    truth: int
    anf_coefficients: tuple[int, ...]
    affine_constant: int | None
    affine_inputs: tuple[int, ...]

    @property
    def is_affine(self) -> bool:
        return self.affine_constant is not None


@dataclass(frozen=True)
class Cut:
    leaves: tuple[int, ...]
    truth: int
    region: frozenset[int]


def truth_from_ast(pins: tuple[str, ...], ast: tuple) -> int:
    truth = 0
    for assignment in range(1 << len(pins)):
        values = {
            pin: bool((assignment >> index) & 1)
            for index, pin in enumerate(pins)
        }
        truth |= int(bool(builder.eval_ast(ast, values))) << assignment
    return truth


def mobius_anf(truth: int, input_count: int) -> tuple[int, ...]:
    """Return ANF coefficients indexed by the variable-subset bit mask."""

    coefficients = [(truth >> index) & 1 for index in range(1 << input_count)]
    for variable in range(input_count):
        bit = 1 << variable
        for mask in range(1 << input_count):
            if mask & bit:
                coefficients[mask] ^= coefficients[mask ^ bit]
    return tuple(coefficients)


def affine_descriptor(coefficients: tuple[int, ...]) -> tuple[int, tuple[int, ...]] | None:
    if any(value and mask.bit_count() > 1 for mask, value in enumerate(coefficients)):
        return None
    inputs = tuple(
        mask.bit_length() - 1
        for mask, value in enumerate(coefficients)
        if value and mask.bit_count() == 1
    )
    return coefficients[0], inputs


def load_cell_functions(audit_path: Path) -> dict[str, CellFunction]:
    audit = json.loads(audit_path.read_text())
    result: dict[str, CellFunction] = {}
    for row in audit["included_cells"]:
        cell = str(row["cell"])
        pins = tuple(map(str, row["inputs"]))
        expression = str(row["function"])
        ast = builder.BooleanParser(expression).parse()
        truth = truth_from_ast(pins, ast)
        coefficients = mobius_anf(truth, len(pins))
        descriptor = affine_descriptor(coefficients)
        result[cell] = CellFunction(
            cell=cell,
            pins=pins,
            expression=expression,
            ast=ast,
            truth=truth,
            anf_coefficients=coefficients,
            affine_constant=None if descriptor is None else descriptor[0],
            affine_inputs=tuple() if descriptor is None else descriptor[1],
        )
    return result


def load_occurrences(graph_path: Path) -> tuple[dict[int, dict], dict]:
    document = json.loads(graph_path.read_text())
    rows = document.get("occurrences")
    if rows is None:
        raise ValueError(f"{graph_path} does not contain an occurrences array")
    nodes = {int(row["anchor"]): row for row in rows}
    if len(nodes) != len(rows):
        raise ValueError(f"duplicate physical occurrence anchor in {graph_path}")
    for anchor, row in nodes.items():
        for child in map(int, row.get("inputs", [])):
            if child not in nodes:
                raise ValueError(f"occurrence {anchor} refers to missing input {child}")
    return nodes, document


def topological(nodes: dict[int, dict], subset: set[int] | None = None) -> list[int]:
    selected = set(nodes) if subset is None else set(subset)
    order: list[int] = []
    state: dict[int, int] = {}

    def visit(anchor: int) -> None:
        mark = state.get(anchor, 0)
        if mark == 2:
            return
        if mark == 1:
            raise ValueError(f"cycle at physical occurrence {anchor}")
        state[anchor] = 1
        for child in map(int, nodes[anchor].get("inputs", [])):
            if child in selected:
                visit(child)
        state[anchor] = 2
        order.append(anchor)

    for anchor in sorted(selected):
        visit(anchor)
    return order


def eval_component_outputs(
    nodes: dict[int, dict],
    functions: dict[str, CellFunction],
    component: set[int],
    boundary: tuple[int, ...],
    outputs: tuple[int, ...],
    assignment: int,
) -> tuple[int, ...]:
    values = {
        anchor: bool((assignment >> index) & 1)
        for index, anchor in enumerate(boundary)
    }
    for anchor in topological(nodes, component):
        row = nodes[anchor]
        function = functions.get(str(row["op"]))
        if function is None:
            raise ValueError(f"missing Boolean function for {row['op']} at occurrence {anchor}")
        children = tuple(map(int, row.get("inputs", [])))
        if len(children) != len(function.pins):
            raise ValueError(
                f"pin/child mismatch at occurrence {anchor}: "
                f"{len(function.pins)} pins vs {len(children)} inputs"
            )
        pin_values = {pin: values[child] for pin, child in zip(function.pins, children)}
        values[anchor] = bool(builder.eval_ast(function.ast, pin_values))
    return tuple(int(values[anchor]) for anchor in outputs)


def eval_affine_rows(
    rows: Iterable[tuple[int, int]], assignment: int
) -> tuple[int, ...]:
    result = []
    for mask, phase in rows:
        result.append((mask & assignment).bit_count() % 2 ^ phase)
    return tuple(result)


def _remap_truth(cut: Cut, leaves: tuple[int, ...], assignment: int) -> int:
    positions = {leaf: index for index, leaf in enumerate(leaves)}
    local_assignment = 0
    for index, leaf in enumerate(cut.leaves):
        local_assignment |= ((assignment >> positions[leaf]) & 1) << index
    return (cut.truth >> local_assignment) & 1


def _combine_cut_truth(
    function: CellFunction,
    child_cuts: tuple[Cut, ...],
    leaves: tuple[int, ...],
) -> int:
    truth = 0
    for assignment in range(1 << len(leaves)):
        values = {
            pin: bool(_remap_truth(cut, leaves, assignment))
            for pin, cut in zip(function.pins, child_cuts, strict=True)
        }
        truth |= int(bool(builder.eval_ast(function.ast, values))) << assignment
    return truth


def _prune_cuts(cuts: list[Cut], limit: int) -> list[Cut]:
    unique: dict[tuple, Cut] = {}
    for cut in cuts:
        unique.setdefault((cut.leaves, cut.truth, cut.region), cut)
    trivial = [cut for cut in unique.values() if not cut.region]
    derived = sorted(
        (cut for cut in unique.values() if cut.region),
        key=lambda cut: (
            len(cut.leaves),
            -len(cut.region),
            cut.leaves,
            cut.truth,
            tuple(sorted(cut.region)),
        ),
    )
    return (trivial[:1] + derived)[:limit]


def enumerate_bounded_cuts(
    nodes: dict[int, dict],
    functions: dict[str, CellFunction],
    max_leaves: int,
    max_cuts: int,
    partial_limit: int,
) -> dict[int, list[Cut]]:
    """Enumerate exact bounded cuts without assuming cell-name semantics."""

    cuts: dict[int, list[Cut]] = {}
    for anchor in topological(nodes):
        row = nodes[anchor]
        trivial = Cut((anchor,), 0b10, frozenset())
        children = tuple(map(int, row.get("inputs", [])))
        function = functions.get(str(row["op"]))
        if (
            row.get("is_leaf", False)
            or row.get("is_constant", False)
            or not children
            or function is None
            or len(children) != len(function.pins)
        ):
            cuts[anchor] = [trivial]
            continue
        partials: list[tuple[Cut, ...]] = [tuple()]
        for child in children:
            expanded = []
            for prefix, cut in product(partials, cuts[child]):
                leaves = set(cut.leaves)
                for prior in prefix:
                    leaves.update(prior.leaves)
                if len(leaves) <= max_leaves:
                    expanded.append(prefix + (cut,))
            best: dict[tuple, tuple[Cut, ...]] = {}
            for item in expanded:
                leaves = tuple(sorted({leaf for cut in item for leaf in cut.leaves}))
                key = (leaves, tuple((cut.leaves, cut.truth) for cut in item))
                prior = best.get(key)
                if prior is None or len(
                    frozenset().union(*(cut.region for cut in item))
                ) > len(frozenset().union(*(cut.region for cut in prior))):
                    best[key] = item
            partials = sorted(
                best.values(),
                key=lambda item: (
                    len({leaf for cut in item for leaf in cut.leaves}),
                    -len(frozenset().union(*(cut.region for cut in item))),
                ),
            )[:partial_limit]
            if not partials:
                break
        derived = []
        for child_cuts in partials:
            leaves = tuple(sorted({leaf for cut in child_cuts for leaf in cut.leaves}))
            if not leaves or len(leaves) > max_leaves:
                continue
            region = frozenset([anchor]).union(*(cut.region for cut in child_cuts))
            truth = _combine_cut_truth(function, child_cuts, leaves)
            derived.append(Cut(leaves, truth, region))
        cuts[anchor] = _prune_cuts([trivial, *derived], max_cuts)
    return cuts


def affine_cuts(cuts: dict[int, list[Cut]]) -> dict[int, list[tuple[Cut, int, tuple[int, ...]]]]:
    result: dict[int, list[tuple[Cut, int, tuple[int, ...]]]] = {}
    for anchor, items in cuts.items():
        recovered = []
        for cut in items:
            if not cut.region or len(cut.leaves) < 2:
                continue
            descriptor = affine_descriptor(mobius_anf(cut.truth, len(cut.leaves)))
            if descriptor is None or len(descriptor[1]) < 2:
                continue
            recovered.append((cut, descriptor[0], descriptor[1]))
        if recovered:
            result[anchor] = recovered
    return result

#!/usr/bin/env python3
"""Build a deterministic single-output ASAP7 6T LVT/TT combinational library.

The upstream ASAP7 release splits combinational cells across AO, OA, INVBUF,
and SIMPLE Liberty files.  EGG consumes one Liberty library, so this tool
merges the four official groups while enforcing the physical assumptions of
the current netlist representation:

* exactly one logical output pin per cell;
* no sequential state or clock-gating semantics;
* FA/HA cells are rejected explicitly even if an upstream view changes;
* every retained output has a Boolean function.

It also emits a bounded rewrite set.  Drive variants are connected inside a
truth-table equivalence class, while one representative per non-trivial
function is connected to a canonical INV/AND2/OR2 decomposition.  Structural
rules are limited to at most five inputs and a bounded expression size.
"""

from __future__ import annotations

import argparse
import hashlib
import itertools
import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable


GROUPS = (
    "asap7sc6t_AO_LVT_TT_nldm_211010.lib",
    "asap7sc6t_INVBUF_LVT_TT_nldm_211010.lib",
    "asap7sc6t_OA_LVT_TT_nldm_211010.lib",
    "asap7sc6t_SIMPLE_LVT_TT_nldm_211010.lib",
)
BASE_GROUP = "asap7sc6t_SIMPLE_LVT_TT_nldm_211010.lib"
CELL_START = re.compile(r"(?m)^  cell \(([^)]+)\) \{")
PIN_START = re.compile(r"(?m)^    pin \(([^)]+)\) \{")
LEAKAGE_START = re.compile(r"(?m)^    leakage_power \(\) \{")


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def balanced_block(text: str, open_brace: int) -> tuple[str, int]:
    """Return a brace-balanced Liberty block, ignoring quoted braces."""
    depth = 0
    quoted = False
    escaped = False
    for index in range(open_brace, len(text)):
        char = text[index]
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
            continue
        if char == '"':
            quoted = True
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                return text[open_brace:index + 1], index + 1
    raise ValueError(f"unterminated block at byte {open_brace}")


def named_blocks(text: str, pattern: re.Pattern[str]) -> Iterable[tuple[str, str]]:
    for match in pattern.finditer(text):
        open_brace = text.find("{", match.start(), match.end() + 1)
        block, _ = balanced_block(text, open_brace)
        yield match.group(1), text[match.start():open_brace] + block


@dataclass(frozen=True)
class Cell:
    name: str
    source: str
    block: str
    inputs: tuple[str, ...]
    outputs: tuple[str, ...]
    function: str | None


def parse_cell(name: str, source: str, block: str) -> Cell:
    inputs: list[str] = []
    outputs: list[str] = []
    output_functions: list[str | None] = []
    for pin_name, pin_block in named_blocks(block, PIN_START):
        direction_match = re.search(r"(?m)^\s*direction\s*:\s*([^;]+);", pin_block)
        if not direction_match:
            continue
        direction = direction_match.group(1).strip()
        if direction == "input":
            inputs.append(pin_name)
        elif direction == "output":
            outputs.append(pin_name)
            function_match = re.search(
                r'(?m)^\s*function\s*:\s*"([^"]+)"\s*;', pin_block
            )
            output_functions.append(function_match.group(1) if function_match else None)
    function = output_functions[0] if len(output_functions) == 1 else None
    return Cell(name, source, block, tuple(inputs), tuple(outputs), function)


def expected_vdd_leakage(block: str) -> tuple[float, int]:
    """Recover Liberty's scalar leakage convention from conditional groups.

    The 28 SELECT cells provide both representations and establish that
    `cell_leakage_power` is the arithmetic mean of the VDD `leakage_power`
    values (VSS entries are zero-valued rail accounting and are excluded).
    """
    values: list[float] = []
    for match in LEAKAGE_START.finditer(block):
        open_brace = block.find("{", match.start(), match.end() + 1)
        leakage_block, _ = balanced_block(block, open_brace)
        value_match = re.search(
            r"(?m)^\s*value\s*:\s*([0-9.eE+-]+)\s*;", leakage_block
        )
        rail_match = re.search(
            r"(?m)^\s*related_pg_pin\s*:\s*([^;\s]+)\s*;", leakage_block
        )
        if value_match and (not rail_match or rail_match.group(1) == "VDD"):
            values.append(float(value_match.group(1)))
    if not values:
        raise ValueError("cell has no VDD conditional leakage values")
    return sum(values) / len(values), len(values)


Token = tuple[str, str]


def tokenize(expression: str) -> list[Token]:
    tokens: list[Token] = []
    index = 0
    while index < len(expression):
        char = expression[index]
        if char.isspace():
            index += 1
        elif char in "!+*()":
            tokens.append((char, char))
            index += 1
        elif char in "01":
            tokens.append(("CONST", char))
            index += 1
        else:
            match = re.match(r"[A-Za-z_][A-Za-z0-9_]*", expression[index:])
            if not match:
                raise ValueError(f"unsupported Boolean token near {expression[index:]!r}")
            value = match.group(0)
            tokens.append(("VAR", value))
            index += len(value)
    return tokens


Ast = tuple


class BooleanParser:
    def __init__(self, expression: str):
        self.tokens = tokenize(expression)
        self.index = 0

    def accept(self, kind: str) -> Token | None:
        if self.index < len(self.tokens) and self.tokens[self.index][0] == kind:
            token = self.tokens[self.index]
            self.index += 1
            return token
        return None

    def parse(self) -> Ast:
        result = self.parse_or()
        if self.index != len(self.tokens):
            raise ValueError(f"trailing Boolean tokens: {self.tokens[self.index:]}")
        return result

    def parse_or(self) -> Ast:
        values = [self.parse_and()]
        while self.accept("+"):
            values.append(self.parse_and())
        return fold_ast("or", values)

    def parse_and(self) -> Ast:
        values = [self.parse_unary()]
        while self.accept("*"):
            values.append(self.parse_unary())
        return fold_ast("and", values)

    def parse_unary(self) -> Ast:
        if self.accept("!"):
            return ("not", self.parse_unary())
        variable = self.accept("VAR")
        if variable:
            return ("var", variable[1])
        constant = self.accept("CONST")
        if constant:
            return ("const", constant[1] == "1")
        if self.accept("("):
            value = self.parse_or()
            if not self.accept(")"):
                raise ValueError("missing closing parenthesis")
            return value
        raise ValueError(f"expected Boolean atom at token {self.index}")


def fold_ast(operator: str, values: list[Ast]) -> Ast:
    result = values[0]
    for value in values[1:]:
        result = (operator, result, value)
    return result


def eval_ast(ast: Ast, values: dict[str, bool]) -> bool:
    kind = ast[0]
    if kind == "var":
        return values[ast[1]]
    if kind == "const":
        return ast[1]
    if kind == "not":
        return not eval_ast(ast[1], values)
    if kind == "and":
        return eval_ast(ast[1], values) and eval_ast(ast[2], values)
    if kind == "or":
        return eval_ast(ast[1], values) or eval_ast(ast[2], values)
    raise ValueError(kind)


def ast_nodes(ast: Ast) -> int:
    if ast[0] in {"var", "const"}:
        return 0
    if ast[0] == "not":
        return 1 + ast_nodes(ast[1])
    return 1 + ast_nodes(ast[1]) + ast_nodes(ast[2])


def ast_to_egg(ast: Ast) -> str:
    kind = ast[0]
    if kind == "var":
        return f"?{ast[1]}"
    if kind == "const":
        return "true" if ast[1] else "false"
    if kind == "not":
        return f"(INVx1_ASAP7_6t_L {ast_to_egg(ast[1])})"
    gate = "AND2x2_ASAP7_6t_L" if kind == "and" else "OR2x2_ASAP7_6t_L"
    return f"({gate} {ast_to_egg(ast[1])} {ast_to_egg(ast[2])})"


def compact_family_ast(cell: Cell, parsed: Ast) -> Ast:
    """Factor regular AO/OA families instead of emitting expanded Liberty DNF."""
    stem = cell.name.split("_ASAP7", 1)[0]
    match = re.match(r"^(AOI|AO|OAI|OA)\d", stem)
    if not match:
        return parsed
    family = match.group(1)
    grouped: dict[str, list[Ast]] = {}
    order: list[str] = []
    for pin in cell.inputs:
        prefix_match = re.match(r"[A-Za-z]+", pin)
        if not prefix_match:
            return parsed
        prefix = prefix_match.group(0)
        if prefix not in grouped:
            order.append(prefix)
            grouped[prefix] = []
        grouped[prefix].append(("var", pin))
    inner = "and" if family in {"AO", "AOI"} else "or"
    outer = "or" if family in {"AO", "AOI"} else "and"
    terms = [fold_ast(inner, grouped[prefix]) for prefix in order]
    result = fold_ast(outer, terms)
    if family in {"AOI", "OAI"}:
        result = ("not", result)
    # Refuse a compact naming interpretation unless it is exactly equivalent
    # to the characterized Liberty function over every input assignment.
    if truth_table(cell, result) != truth_table(cell, parsed):
        raise ValueError(f"compact family interpretation mismatch for {cell.name}")
    return result


def truth_table(cell: Cell, ast: Ast) -> str:
    bits: list[str] = []
    for row in itertools.product((False, True), repeat=len(cell.inputs)):
        bits.append("1" if eval_ast(ast, dict(zip(cell.inputs, row))) else "0")
    return "".join(bits)


def symmetric_input_pairs(cell: Cell, ast: Ast) -> list[tuple[int, int]]:
    pairs: list[tuple[int, int]] = []
    for first, second in itertools.combinations(range(len(cell.inputs)), 2):
        symmetric = True
        for row in itertools.product((False, True), repeat=len(cell.inputs)):
            swapped = list(row)
            swapped[first], swapped[second] = swapped[second], swapped[first]
            if eval_ast(ast, dict(zip(cell.inputs, row))) != eval_ast(
                ast, dict(zip(cell.inputs, swapped))
            ):
                symmetric = False
                break
        if symmetric:
            pairs.append((first, second))
    return pairs


def gate_pattern(cell: Cell) -> str:
    args = " ".join(f"?{pin}" for pin in cell.inputs)
    return f"({cell.name}{(' ' + args) if args else ''})"


def rule_name(prefix: str, left: str, right: str) -> str:
    clean_left = re.sub(r"[^A-Za-z0-9]+", "_", left).strip("_")
    clean_right = re.sub(r"[^A-Za-z0-9]+", "_", right).strip("_")
    return f"{prefix}_{clean_left}_{clean_right}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-dir", type=Path, required=True)
    parser.add_argument(
        "--lef",
        type=Path,
        required=True,
        help="Official LVT LEF used to recover any missing Liberty area attributes",
    )
    parser.add_argument(
        "--select-base",
        type=Path,
        default=Path(__file__).resolve().parents[1]
        / "test/asap7sc6t_SELECT_LVT_TT_nldm.lib",
        help="Existing SELECT library retained byte-for-byte for overlapping cells and header",
    )
    parser.add_argument("--out-lib", type=Path, required=True)
    parser.add_argument("--out-rules", type=Path, required=True)
    parser.add_argument("--out-scale-rules", type=Path, required=True)
    parser.add_argument("--audit", type=Path, required=True)
    parser.add_argument("--max-struct-inputs", type=int, default=9)
    parser.add_argument("--max-struct-nodes", type=int, default=31)
    args = parser.parse_args()

    source_text: dict[str, str] = {}
    upstream_cells: dict[str, Cell] = {}
    for filename in GROUPS:
        path = args.source_dir / filename
        text = path.read_text()
        source_text[filename] = text
        for name, block in named_blocks(text, CELL_START):
            if name in upstream_cells:
                raise ValueError(f"duplicate cell {name}")
            upstream_cells[name] = parse_cell(name, filename, block)

    # The current SELECT library is the experimental PPA anchor.  Preserve its
    # header and all overlapping cell blocks exactly, then add only cells that
    # were previously absent.  This prevents a nominal-temperature/header or
    # re-characterization change from being mistaken for library-coverage QoR.
    select_text = args.select_base.read_text()
    all_cells = dict(upstream_cells)
    select_names: set[str] = set()
    for name, block in named_blocks(select_text, CELL_START):
        if name not in upstream_cells:
            raise ValueError(f"SELECT cell {name} is absent from official source groups")
        select_names.add(name)
        all_cells[name] = parse_cell(name, args.select_base.name, block)

    lef_text = args.lef.read_text()
    lef_areas: dict[str, float] = {}
    for match in re.finditer(
        r"(?ms)^MACRO\s+(\S+)\s*$.*?^\s+SIZE\s+([0-9.]+)\s+BY\s+([0-9.]+)\s*;",
        lef_text,
    ):
        lef_areas[match.group(1)] = float(match.group(2)) * float(match.group(3))
    recovered_areas: list[dict] = []
    for name, cell in list(all_cells.items()):
        area_match = re.search(r"(?m)^    area\s*:\s*([0-9.eE+-]+)\s*;", cell.block)
        if area_match and float(area_match.group(1)) > 0.0:
            continue
        if name not in lef_areas:
            raise ValueError(f"cell {name} has no Liberty area and no LEF SIZE")
        area = lef_areas[name]
        if area_match:
            patched = (
                cell.block[:area_match.start()]
                + f"    area : {area:.6f};"
                + cell.block[area_match.end():]
            )
            source_condition = f"non_positive_liberty_area_{area_match.group(1)}"
        else:
            patched = cell.block.replace("{", f"{{\n    area : {area:.6f};", 1)
            source_condition = "missing_liberty_area"
        all_cells[name] = parse_cell(name, cell.source, patched)
        recovered_areas.append({
            "cell": name,
            "source_condition": source_condition,
            "lef_width_times_height": area,
            "inserted_liberty_area": round(area, 6),
        })

    recovered_leakages: list[dict] = []
    leakage_calibration: list[dict] = []
    for name, cell in list(all_cells.items()):
        scalar_match = re.search(
            r"(?m)^\s*cell_leakage_power\s*:\s*([0-9.eE+-]+)\s*;",
            cell.block,
        )
        expected, state_count = expected_vdd_leakage(cell.block)
        if scalar_match:
            scalar = float(scalar_match.group(1))
            leakage_calibration.append({
                "cell": name,
                "existing_scalar": scalar,
                "conditional_vdd_mean": expected,
                "relative_error": abs(expected / scalar - 1.0) if scalar else None,
                "vdd_state_count": state_count,
            })
            continue
        patched = cell.block.replace(
            "{", f"{{\n    cell_leakage_power : {expected:.9g};", 1
        )
        all_cells[name] = parse_cell(name, cell.source, patched)
        recovered_leakages.append({
            "cell": name,
            "source": cell.source,
            "conditional_vdd_mean": expected,
            "inserted_cell_leakage_power": float(f"{expected:.9g}"),
            "vdd_state_count": state_count,
        })

    included: dict[str, Cell] = {}
    excluded: list[dict] = []
    for name, cell in sorted(all_cells.items()):
        reasons: list[str] = []
        if name.startswith(("HA", "FA")):
            reasons.append("explicit_multi_output_arithmetic_exclusion")
        if len(cell.outputs) != 1:
            reasons.append(f"logical_output_count_{len(cell.outputs)}")
        if cell.function is None:
            reasons.append("missing_single_output_function")
        if re.search(r"(?m)^\s*(ff|latch|statetable|clock_gating_integrated_cell)\s*\(", cell.block):
            reasons.append("stateful_or_clock_gating")
        if reasons:
            excluded.append({
                "cell": name,
                "source": cell.source,
                "inputs": list(cell.inputs),
                "outputs": list(cell.outputs),
                "reasons": reasons,
            })
        else:
            included[name] = cell

    base = select_text
    first_cell = CELL_START.search(base)
    if not first_cell:
        raise ValueError("base Liberty has no cells")
    header = base[:first_cell.start()]
    header = re.sub(
        r"(?m)^library \([^)]+\) \{",
        "library (asap7sc6t_FULL_COMB_LVT_TT_nldm_211010) {",
        header,
        count=1,
    )
    header = header.replace(
        '  comment : "";',
        '  comment : "Generated from official ASAP7 6T v26 AO/OA/INVBUF/SIMPLE LVT/TT NLDM; single-output combinational cells only.";',
        1,
    )
    merged = header + "\n".join(included[name].block for name in sorted(included)) + "\n}\n"
    args.out_lib.parent.mkdir(parents=True, exist_ok=True)
    args.out_lib.write_text(merged)

    ast_by_name: dict[str, Ast] = {}
    table_by_name: dict[str, str] = {}
    parse_failures: list[dict] = []
    for name, cell in included.items():
        try:
            parsed = BooleanParser(cell.function or "").parse()
            ast = compact_family_ast(cell, parsed)
            ast_by_name[name] = ast
            table_by_name[name] = truth_table(cell, ast)
        except Exception as error:  # audit unsupported upstream syntax explicitly
            parse_failures.append({"cell": name, "function": cell.function, "error": str(error)})

    groups: dict[tuple[tuple[str, ...], str], list[Cell]] = {}
    for name, table in table_by_name.items():
        cell = included[name]
        groups.setdefault((cell.inputs, table), []).append(cell)

    preferred = select_names

    def representative(cells: list[Cell]) -> Cell:
        return min(cells, key=lambda cell: (cell.name not in preferred, len(cell.name), cell.name))

    rules: list[dict] = []
    scale_only_rules: list[dict] = []
    scale_rules = 0
    structural_rules = 0
    symmetry_rules = 0
    structural_skips: list[dict] = []
    canonical_primitives = {
        "INVx1_ASAP7_6t_L", "AND2x2_ASAP7_6t_L", "OR2x2_ASAP7_6t_L"
    }
    for (_, _), cells in sorted(groups.items(), key=lambda item: (item[0][0], item[0][1])):
        cells = sorted(cells, key=lambda cell: cell.name)
        rep = representative(cells)
        for alternate in cells:
            if alternate.name == rep.name:
                continue
            scale_rule = {
                "name": rule_name("FULL6T_SCALE", rep.name, alternate.name),
                "searcher": gate_pattern(rep),
                "applier": gate_pattern(alternate),
                "bidirectional": True,
            }
            rules.append(scale_rule)
            scale_only_rules.append(scale_rule)
            scale_rules += 1

        for first, second in symmetric_input_pairs(rep, ast_by_name[rep.name]):
            swapped = list(rep.inputs)
            swapped[first], swapped[second] = swapped[second], swapped[first]
            swapped_args = " ".join(f"?{pin}" for pin in swapped)
            rules.append({
                "name": rule_name(
                    "FULL6T_SYMMETRY",
                    rep.name,
                    f"swap_{rep.inputs[first]}_{rep.inputs[second]}",
                ),
                "searcher": gate_pattern(rep),
                "applier": f"({rep.name} {swapped_args})",
            })
            symmetry_rules += 1

        ast = ast_by_name[rep.name]
        reason = None
        if rep.name in canonical_primitives:
            reason = "canonical_primitive"
        elif len(rep.inputs) == 0:
            reason = "constant_cell"
        elif len(rep.inputs) > args.max_struct_inputs:
            reason = "input_limit"
        elif ast_nodes(ast) > args.max_struct_nodes:
            reason = "expression_node_limit"
        elif ast_to_egg(ast) == gate_pattern(rep):
            reason = "identity"
        if reason:
            structural_skips.append({"cell": rep.name, "reason": reason})
            continue
        rules.append({
            "name": rule_name("FULL6T_STRUCT", rep.name, "canonical"),
            "searcher": gate_pattern(rep),
            "applier": ast_to_egg(ast),
            "bidirectional": True,
        })
        structural_rules += 1

    args.out_rules.parent.mkdir(parents=True, exist_ok=True)
    args.out_rules.write_text(json.dumps({"rewrites": rules}, indent=2) + "\n")
    args.out_scale_rules.parent.mkdir(parents=True, exist_ok=True)
    args.out_scale_rules.write_text(
        json.dumps({"rewrites": scale_only_rules}, indent=2) + "\n"
    )

    audit = {
        "schema": "asap7_6t_full_comb_v1",
        "upstream": {
            "repository": "https://github.com/The-OpenROAD-Project/asap7sc6t_26",
            "groups": list(GROUPS),
            "select_anchor": str(args.select_base),
            "select_anchor_sha256": hashlib.sha256(args.select_base.read_bytes()).hexdigest(),
            "overlapping_cells_preserved_from_select": len(select_names),
            "lef": str(args.lef),
            "lef_sha256": hashlib.sha256(args.lef.read_bytes()).hexdigest(),
        },
        "constraints": {
            "logical_outputs": 1,
            "combinational_only": True,
            "explicitly_excluded_prefixes": ["HA", "FA"],
            "max_struct_inputs": args.max_struct_inputs,
            "max_struct_nodes": args.max_struct_nodes,
        },
        "source_cell_count": len(all_cells),
        "included_cell_count": len(included),
        "excluded_cell_count": len(excluded),
        "included_cells": [
            {
                "cell": cell.name,
                "source": cell.source,
                "inputs": list(cell.inputs),
                "outputs": list(cell.outputs),
                "function": cell.function,
            }
            for cell in sorted(included.values(), key=lambda cell: cell.name)
        ],
        "excluded_cells": excluded,
        "recovered_missing_areas_from_lef": recovered_areas,
        "recovered_missing_cell_leakage_power": recovered_leakages,
        "included_recovered_cell_leakage_power_count": sum(
            row["cell"] in included for row in recovered_leakages
        ),
        "excluded_recovered_cell_leakage_power_count": sum(
            row["cell"] not in included for row in recovered_leakages
        ),
        "cell_leakage_power_calibration": {
            "cells": leakage_calibration,
            "max_relative_error": max(
                row["relative_error"] for row in leakage_calibration
                if row["relative_error"] is not None
            ),
        },
        "boolean_parse_failures": parse_failures,
        "rewrite_count": len(rules),
        "scale_rule_count": scale_rules,
        "structural_rule_count": structural_rules,
        "symmetry_rule_count": symmetry_rules,
        "structural_skips": structural_skips,
        "library_sha256": sha256_text(merged),
        "rules_sha256": sha256_text(args.out_rules.read_text()),
        "scale_rules_sha256": sha256_text(args.out_scale_rules.read_text()),
    }
    args.audit.parent.mkdir(parents=True, exist_ok=True)
    args.audit.write_text(json.dumps(audit, indent=2) + "\n")

    if parse_failures:
        raise SystemExit(f"Boolean parse failures: {len(parse_failures)}")
    if any(len(cell.outputs) != 1 for cell in included.values()):
        raise SystemExit("multi-output cell escaped the filter")
    if any(
        not (match := re.search(r"(?m)^    area\s*:\s*([0-9.eE+-]+)\s*;", cell.block))
        or float(match.group(1)) <= 0.0
        for cell in included.values()
    ):
        raise SystemExit("included cell without positive area escaped LEF recovery")
    if any(
        not re.search(
            r"(?m)^\s*cell_leakage_power\s*:\s*[0-9.eE+-]+\s*;", cell.block
        )
        for cell in included.values()
    ):
        raise SystemExit("included cell without scalar leakage escaped recovery")
    print(json.dumps({
        "source_cells": len(all_cells),
        "included_cells": len(included),
        "excluded_cells": len(excluded),
        "rewrite_count": len(rules),
        "scale_rules": scale_rules,
        "structural_rules": structural_rules,
        "symmetry_rules": symmetry_rules,
        "library_sha256": audit["library_sha256"],
    }, indent=2))


if __name__ == "__main__":
    main()

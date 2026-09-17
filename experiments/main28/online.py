#!/usr/bin/env python3
"""Freeze and validate the checkpoint seen when Conquer finishes."""

from __future__ import annotations

import collections
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time


HERE = Path(__file__).resolve().parent
TCL = HERE / "genus_mapped_batch.tcl"


def load(path: Path):
    return json.loads(path.read_text())


def dump(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def module_name(path: Path) -> str:
    match = re.search(r"\bmodule\s+([A-Za-z_][A-Za-z0-9_$]*)", path.read_text())
    if not match:
        raise RuntimeError(f"cannot find a module declaration in {path}")
    return match.group(1)


def freeze_candidate(source: Path, destination: Path, row: dict) -> dict:
    source = source.resolve()
    if not source.is_file():
        raise RuntimeError(f"missing candidate netlist: {source}")
    expected = row.get("sha256") or sha256(source)
    if sha256(source) != expected:
        raise RuntimeError(f"candidate SHA mismatch: {source}")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    if sha256(destination) != expected:
        raise RuntimeError(f"frozen copy SHA mismatch: {destination}")
    return {**row, "netlist": str(destination.resolve()), "sha256": expected}


def freeze_decision_snapshot(output: Path, g0: Path, point: dict, conquer_finished_unix: float) -> dict:
    """Copy only checkpoints that were complete when Conquer returned."""
    snapshot = output / "decision_snapshot"
    if snapshot.exists():
        raise RuntimeError(f"decision snapshot already exists: {snapshot}")
    rows = []
    rows.append(freeze_candidate(g0, snapshot / "netlists/g0.v", {
        "method": "G0", "candidate_id": "G0", "round": 0,
        "sha256": point["g0_sha256"], "lane": "g0",
    }))
    iterative_receipts = []
    for receipt_path in sorted((output / "iterative").glob("round_*/round_receipt.json")):
        accepted = receipt_path.parent / "accepted.v"
        # The receipt is written after accepted.v. Requiring both files and a
        # receipt timestamp no later than the checkpoint instant excludes a
        # partially written or later round.
        if not accepted.is_file() or receipt_path.stat().st_mtime > conquer_finished_unix:
            continue
        receipt = load(receipt_path)
        round_number = int(receipt["round"])
        digest = receipt["accepted_sha256"]
        row = freeze_candidate(accepted, snapshot / f"netlists/iterative_r{round_number}.v", {
            "method": "Iterative", "candidate_id": f"ITERATIVE_R{round_number}",
            "round": round_number, "sha256": digest, "lane": "iterative",
            "elapsed_sec": float(receipt["elapsed_sec"]),
            "instances": int(receipt["instance_count"]),
        })
        rows.append(row)
        iterative_receipts.append(str(receipt_path.resolve()))
    completed = [row["round"] for row in rows if row["method"] == "Iterative"]
    if completed != list(range(1, len(completed) + 1)):
        raise RuntimeError(f"non-consecutive Iterative checkpoint sequence: {completed}")
    frozen_path = output / "conquer/frozen_finalists.json"
    if not frozen_path.is_file():
        raise RuntimeError(f"Conquer did not produce {frozen_path}")
    for index, finalist in enumerate(load(frozen_path)["finalists"]):
        if finalist["candidate_id"] == "G0":
            continue
        rows.append(freeze_candidate(Path(finalist["netlist"]), snapshot / f"netlists/conquer_{index:02d}.v", {
            "method": "Conquer", "candidate_id": finalist["candidate_id"],
            "round": finalist.get("round"), "sha256": finalist["sha256"],
            "lane": finalist.get("finalist_lane", "unknown"),
        }))
    result = {
        "schema": "escope-main28-online-checkpoint-v1",
        "benchmark": point["benchmark"], "anchor": point["anchor"],
        "conquer_finished_unix": conquer_finished_unix,
        "selection_frozen_before_external_evaluation": True,
        "iterative_completed_rounds": len(completed),
        "iterative_receipts": iterative_receipts,
        "candidates": rows,
    }
    dump(snapshot / "manifest.json", result)
    dump(snapshot / "FREEZE_RECEIPT.json", {
        "manifest_sha256": sha256(snapshot / "manifest.json"),
        "candidate_sha256": {row["candidate_id"]: row["sha256"] for row in rows},
    })
    return result


def parse_genus(report_dir: Path) -> dict:
    timing = (report_dir / "timing.rpt").read_text()
    area = (report_dir / "area.rpt").read_text()
    power = (report_dir / "power.rpt").read_text()
    data = re.search(r"Data Path:-\s*([0-9.eE+-]+)", timing)
    adjust = re.search(r"Drv Adjust:\+\s*[0-9.eE+-]+\s+([0-9.eE+-]+)", timing)
    area_lines = [line for line in area.splitlines() if re.match(r"^\S+\s+NA\s+\d+", line)]
    power_lines = [line for line in power.splitlines() if re.match(r"^\s*logic\s+", line)]
    if not data or not adjust or not area_lines or not power_lines:
        raise RuntimeError(f"cannot parse Genus reports in {report_dir}")
    return {
        "data_path_ps": float(data.group(1)),
        "driver_adjust_ps": float(adjust.group(1)),
        "boundary_delay_ps": float(data.group(1)) + float(adjust.group(1)),
        "area_um2": float(area_lines[-1].split()[5]),
        "power_uw": float(power_lines[-1].split()[4]) * 1.0e6,
    }


def run_genus(root: Path, rows: list[dict], liberty: Path, genus: Path, timeout: int) -> list[dict]:
    unique = {row["sha256"]: row for row in rows}
    tasks = []
    for index, row in enumerate(unique.values()):
        netlist = Path(row["netlist"])
        tasks.append({**row, "top": module_name(netlist),
                      "report_dir": str((root / "genus" / f"candidate_{index:02d}").resolve())})
    session = root / "genus/session"
    session.mkdir(parents=True, exist_ok=True)
    items = session / "items.txt"
    items.write_text("\n".join(
        f"{row['candidate_id']}|{row['netlist']}|{row['top']}|{row['report_dir']}" for row in tasks
    ) + "\n")
    env = os.environ.copy()
    env.update({"E_SCOPE_GENUS_LIBERTY": str(liberty.resolve()),
                "E_SCOPE_BATCH_ITEMS_FILE": str(items.resolve())})
    started = time.perf_counter()
    result = subprocess.run([str(genus.resolve()), "-files", str(TCL), "-log", str(session / "genus.log")],
                            cwd=session, env=env, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=timeout)
    (session / "stdout.log").write_text(result.stdout)
    if result.returncode or f"E_SCOPE_BATCH_DONE points={len(tasks)}" not in result.stdout:
        raise RuntimeError(f"Genus batch failed; see {session}")
    by_sha = {}
    for task in tasks:
        if sha256(Path(task["netlist"])) != task["sha256"]:
            raise RuntimeError(f"candidate changed during Genus: {task['netlist']}")
        by_sha[task["sha256"]] = parse_genus(Path(task["report_dir"]))
    evaluated = [{**row, "ppa": by_sha[row["sha256"]]} for row in rows]
    dump(root / "genus/results.json", {"wall_sec": time.perf_counter() - started, "results": evaluated})
    return evaluated


def normalize_verilog(text: str) -> str:
    escaped = set(re.findall(r"\\([^\s]+)", text))
    mapping = {name: "egg_esc_" + name.encode().hex() for name in sorted(escaped)}
    text = re.sub(r"\\([^\s]+)(\s)", lambda match: mapping[match.group(1)] + match.group(2), text)
    return re.sub(r"1'[hH]([01])", r"1'b\1", text)


def yosys_quote(path: Path) -> str:
    return '"' + str(path).replace("\\", "\\\\").replace('"', '\\"') + '"'


def run_yosys(yosys: Path, command: str, log: Path, timeout: int, datdir: Path | None) -> None:
    env = os.environ.copy()
    if datdir:
        env["YOSYS_DATDIR"] = str(datdir.resolve())
    env["PATH"] = str(yosys.resolve().parent) + os.pathsep + env.get("PATH", "")
    result = subprocess.run([str(yosys.resolve()), "-Q", "-p", command], cwd=log.parent,
                            env=env, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=timeout)
    log.write_text(result.stdout)
    if result.returncode:
        raise RuntimeError(f"Yosys failed; see {log}")


def validate_candidate(row: dict, g0: Path, out: Path, liberty: Path, yosys: Path,
                       abc: Path, timeout: int, datdir: Path | None) -> dict:
    out.mkdir(parents=True, exist_ok=True)
    target = Path(row["netlist"])
    source = out / "g0_normalized.v"
    source.write_text(normalize_verilog(g0.read_text()))
    top = module_name(target)
    source_top = module_name(source)
    design_json, cold = out / "mapped_design.json", out / "cold_rewrite.v"
    libq = yosys_quote(liberty)
    legality = "; ".join([f"read_liberty -lib {libq}", f"read_verilog {yosys_quote(target)}",
        f"hierarchy -check -top {top}", "check -assert", r"select -assert-none t:\$*",
        f"write_json {yosys_quote(design_json)}", f"write_verilog -noattr {yosys_quote(cold)}"])
    run_yosys(yosys, legality, out / "mapped_legality.log", timeout, datdir)
    cells = load(design_json)["modules"][top]["cells"]
    histogram = collections.Counter(str(cell["type"]) for cell in cells.values())
    library_cells = {m.group(1) for m in re.finditer(r"^\s*cell\s*\(([^)]+)\)", liberty.read_text(), re.MULTILINE)}
    unresolved = sorted(set(histogram) - library_cells)
    generic = sorted(cell for cell in histogram if cell.startswith("$"))
    if unresolved or generic:
        raise RuntimeError(f"illegal cells unresolved={unresolved} generic={generic}")
    def aig(netlist: Path, design_top: str, destination: Path) -> str:
        return "; ".join([f"read_liberty -ignore_miss_func -ignore_miss_dir {libq}",
            f"read_verilog {yosys_quote(netlist)}", f"hierarchy -check -top {design_top}",
            "flatten", "proc", "opt", "memory", "opt", "techmap", "opt", "aigmap",
            f"write_aiger -symbols {yosys_quote(destination)}"])
    gold, gate = out / "g0.aig", out / "candidate.aig"
    run_yosys(yosys, aig(source, source_top, gold), out / "g0_aig.log", timeout, datdir)
    run_yosys(yosys, aig(cold, top, gate), out / "candidate_aig.log", timeout, datdir)
    result = subprocess.run([str(abc.resolve()), "-c", f"cec -T {timeout} -p {gold} {gate}"],
                            cwd=out, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=timeout)
    (out / "abc_cec.log").write_text(result.stdout)
    if result.returncode or "Networks are equivalent" not in result.stdout:
        raise RuntimeError(f"ABC CEC failed; see {out / 'abc_cec.log'}")
    return {"candidate_id": row["candidate_id"], "sha256": row["sha256"],
            "formal_equivalence": "PASS", "instance_count": sum(histogram.values())}


def run_formal(root: Path, rows: list[dict], g0: Path, liberty: Path, yosys: Path,
               abc: Path, timeout: int, datdir: Path | None) -> dict[str, bool]:
    valid = {row["sha256"]: True for row in rows if row["method"] == "G0"}
    results, failures = [], []
    for index, row in enumerate({r["sha256"]: r for r in rows if r["method"] != "G0"}.values()):
        try:
            receipt = validate_candidate(row, g0, root / "formal" / f"candidate_{index:02d}",
                                         liberty, yosys, abc, timeout, datdir)
            results.append(receipt); valid[row["sha256"]] = True
        except Exception as error:
            failures.append({"candidate_id": row["candidate_id"], "sha256": row["sha256"],
                             "error": repr(error)})
            valid[row["sha256"]] = False
    dump(root / "formal/results.json", {"results": results, "failures": failures})
    return valid


def objective_value(row: dict, objective: str) -> float:
    ppa = row["ppa"]
    if objective == "DA":
        return ppa["data_path_ps"] * ppa["area_um2"]
    return ppa["data_path_ps"] ** 2 * ppa["area_um2"] * ppa["power_uw"]


def select_best(rows: list[dict], method: str, objective: str, valid: dict[str, bool]) -> dict:
    candidates = [row for row in rows if row["method"] in ("G0", method) and valid[row["sha256"]]]
    return min(candidates, key=lambda row: (objective_value(row, objective), row["sha256"]))


def nondominated(rows: list[dict], metrics: tuple[str, str]) -> list[dict]:
    return [
        row for index, row in enumerate(rows)
        if not any(
            all(other[key] <= row[key] for key in metrics)
            and any(other[key] < row[key] for key in metrics)
            for other_index, other in enumerate(rows) if other_index != index
        )
    ]


def collect_adder_internal_front(iterative_root: Path) -> list[dict]:
    """Recreate the pre-Genus freeze used for the paper's adder result."""
    unique = {}
    pattern = "round_*/progressive/stage_*/**/summary.json"
    for summary_path in sorted(iterative_root.glob(pattern)):
        best = summary_path.parent / "best.v"
        if not best.is_file():
            continue
        summary = load(summary_path)
        ppa = summary.get("best")
        if not ppa or not all(key in ppa for key in ("delay", "area", "power")):
            continue
        digest = sha256(best)
        unique[digest] = {
            "source": best, "sha256": digest,
            "delay": float(ppa["delay"]), "area": float(ppa["area"]),
            "power": float(ppa["power"]),
            "round": int(next(part for part in summary_path.parts if part.startswith("round_")).split("_")[1]) + 1,
        }
    rows = list(unique.values())
    if not rows:
        raise RuntimeError("no progressive Iterative candidates found for epfl_adder")
    selected = {row["sha256"]: row for row in (
        nondominated(rows, ("delay", "area")) + nondominated(rows, ("delay", "power"))
    )}
    return sorted(selected.values(), key=lambda row: (row["delay"], row["area"], row["power"], row["sha256"]))


def validate_snapshot(output: Path, point: dict, liberty: Path, genus: Path, yosys: Path,
                      abc: Path, genus_timeout: int, formal_timeout: int,
                      yosys_datdir: Path | None) -> dict:
    snapshot = load(output / "decision_snapshot/manifest.json")
    root = output / "decision_snapshot/validation"
    evaluated = run_genus(root, snapshot["candidates"], liberty, genus, genus_timeout)
    valid = run_formal(root, evaluated, Path(HERE / point["g0_path"]), liberty,
                       yosys, abc, formal_timeout, yosys_datdir)
    objective = "DA" if point["benchmark"] == "epfl_adder" else "D2AP"
    g0 = select_best(evaluated, "G0", objective, valid)
    iterative = select_best(evaluated, "Iterative", objective, valid)
    conquer = select_best(evaluated, "Conquer", objective, valid)
    base = objective_value(g0, objective)
    def delta(row: dict) -> float:
        return 100.0 * (objective_value(row, objective) / base - 1.0)
    first = next((row for row in evaluated if row["method"] == "Iterative" and row["round"] == 1), None)
    conquer_wall = float(load(output / "conquer/SUMMARY.json")["wall_sec"])
    iterative_rounds = int(snapshot["iterative_completed_rounds"])
    features = {
        "instances": int(first["instances"] if first else point["g0_gates"]),
        "iterative_completed_rounds": iterative_rounds,
        "iterative_current_qor_delta_percent": delta(iterative),
        "conquer_qor_delta_percent": delta(conquer),
        "qor_gap_pp": delta(conquer) - delta(iterative),
        "projected_iterative_to_conquer_ratio": (
            float(first["elapsed_sec"]) * int(point["iterative_round_cap"]) / conquer_wall
            if first else None
        ),
        "conquer_normal_lane": conquer["candidate_id"].startswith("NORMAL"),
    }
    result = {"objective": objective, "features": features,
              "g0": g0, "iterative_checkpoint": iterative, "conquer": conquer,
              "formal_failures": load(root / "formal/results.json")["failures"]}
    dump(output / "decision_snapshot/MEASURED.json", result)
    return result


def freeze_final_iterative(output: Path, g0: Path, point: dict) -> dict:
    root = output / "final_iterative"
    rows = [freeze_candidate(g0, root / "netlists/g0.v", {
        "method": "G0", "candidate_id": "G0", "round": 0,
        "sha256": point["g0_sha256"], "lane": "g0"})]
    for receipt_path in sorted((output / "iterative").glob("round_*/round_receipt.json")):
        receipt = load(receipt_path); number = int(receipt["round"])
        rows.append(freeze_candidate(receipt_path.parent / "accepted.v",
            root / f"netlists/iterative_r{number}.v", {
                "method": "Iterative", "candidate_id": f"ITERATIVE_R{number}",
                "round": number, "sha256": receipt["accepted_sha256"], "lane": "iterative",
                "elapsed_sec": float(receipt["elapsed_sec"]), "instances": int(receipt["instance_count"])}))
    completed = [row["round"] for row in rows if row["method"] == "Iterative"]
    if (not completed or completed != list(range(1, len(completed) + 1))
            or len(completed) > int(point["iterative_round_cap"])):
        raise RuntimeError(f"invalid final Iterative checkpoint sequence: {completed}")
    freeze_rule = "completed round checkpoints"
    if point["benchmark"] == "epfl_adder":
        freeze_rule = "union of exact Internal-NLDM-V3 delay-area and delay-power fronts"
        rows = rows[:1]
        existing = {rows[0]["sha256"]}
        for index, candidate in enumerate(collect_adder_internal_front(output / "iterative")):
            if candidate["sha256"] in existing:
                continue
            rows.append(freeze_candidate(candidate["source"],
                root / f"netlists/iterative_front_{index:03d}.v", {
                    "method": "Iterative", "candidate_id": f"ITERATIVE_FRONT_{index:03d}",
                    "round": candidate["round"], "sha256": candidate["sha256"],
                    "lane": "internal_da_dp_front"}))
            existing.add(candidate["sha256"])
    manifest = {
        "schema": "escope-main28-final-iterative-v1", "candidates": rows,
        "selection_frozen_before_external_evaluation": True,
        "freeze_rule": freeze_rule,
    }
    dump(root / "manifest.json", manifest)
    return manifest


def validate_final_iterative(output: Path, point: dict, liberty: Path, genus: Path, yosys: Path,
                             abc: Path, genus_timeout: int, formal_timeout: int,
                             yosys_datdir: Path | None) -> dict:
    manifest = load(output / "final_iterative/manifest.json")
    root = output / "final_iterative/validation"
    rows = run_genus(root, manifest["candidates"], liberty, genus, genus_timeout)
    valid = run_formal(root, rows, Path(HERE / point["g0_path"]), liberty,
                       yosys, abc, formal_timeout, yosys_datdir)
    objective = "DA" if point["benchmark"] == "epfl_adder" else "D2AP"
    return select_best(rows, "Iterative", objective, valid)

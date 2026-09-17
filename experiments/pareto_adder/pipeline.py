#!/usr/bin/env python3
"""Freeze, externally validate, and plot fresh Figure 7 search results."""

from __future__ import annotations

import collections
import concurrent.futures
import csv
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time


HERE = Path(__file__).resolve().parent
MANIFEST = HERE / "manifest.json"
TCL = HERE / "genus_mapped_batch.tcl"
LIB_SHA256 = "48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd"


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


def nondominated(rows: list[dict], metrics: tuple[str, str]) -> list[dict]:
    return [
        row
        for index, row in enumerate(rows)
        if not any(
            all(other[key] <= row[key] for key in metrics)
            and any(other[key] < row[key] for key in metrics)
            for other_index, other in enumerate(rows)
            if other_index != index
        )
    ]


def stage_budget(name: str) -> int:
    if name == "stage_leader_followup":
        return 1000
    match = re.fullmatch(r"stage_(\d+)", name)
    if not match:
        raise RuntimeError(f"unrecognized progressive stage {name!r}")
    return int(match.group(1))


def collect_search_candidates(root: Path, objective: str) -> tuple[list[dict], int]:
    """Collect the same progressive best.v records used by the paper archive."""
    objective_root = root / objective
    unique: dict[str, dict] = {}
    source_count = 0
    pattern = "*/search/round_*/progressive/stage_*/**/summary.json"
    for summary_path in sorted(objective_root.glob(pattern)):
        best = summary_path.parent / "best.v"
        if not best.is_file():
            continue
        summary = load(summary_path)
        ppa = summary.get("best")
        if not ppa or not all(key in ppa for key in ("delay", "area", "power")):
            continue
        relative = summary_path.relative_to(objective_root)
        anchor = relative.parts[0]
        round_name = next(part for part in relative.parts if part.startswith("round_"))
        stage = next(part for part in relative.parts if part.startswith("stage_"))
        source_count += 1
        digest = sha256(best)
        row = {
            "sha256": digest,
            "source_netlist": str(best.resolve()),
            "source_summary": str(summary_path.resolve()),
            "anchor": anchor,
            "round": round_name,
            "stage": stage,
            "delay": float(ppa["delay"]),
            "area": float(ppa["area"]),
            "power": float(ppa["power"]),
        }
        previous = unique.get(digest)
        if previous is None or stage_budget(stage) > stage_budget(previous["stage"]):
            unique[digest] = row
    if not unique:
        raise RuntimeError(f"no progressive candidates found under {objective_root}")
    return list(unique.values()), source_count


def freeze_objective(root: Path, objective: str) -> dict:
    rows, source_count = collect_search_candidates(root, objective)
    da = nondominated(rows, ("delay", "area"))
    dp = nondominated(rows, ("delay", "power"))
    da_sha = {row["sha256"] for row in da}
    dp_sha = {row["sha256"] for row in dp}
    selected = sorted(
        {row["sha256"]: row for row in da + dp}.values(),
        key=lambda row: (row["delay"], row["area"], row["power"], row["sha256"]),
    )
    destination = root / "frozen" / objective
    manifest_path = destination / "manifest.json"
    expected_shas = [row["sha256"] for row in selected]
    if manifest_path.exists():
        prior = load(manifest_path)
        if [row["sha256"] for row in prior["candidates"]] != expected_shas:
            raise RuntimeError(f"frozen selection changed for {objective}")
        for row in prior["candidates"]:
            netlist = root / row["netlist"]
            if not netlist.is_file() or sha256(netlist) != row["sha256"]:
                raise RuntimeError(f"frozen netlist changed: {netlist}")
        return prior
    if (root / "external").exists() or (root / "formal").exists():
        raise RuntimeError("refusing to freeze a new pool after validation output exists")
    netlists = destination / "netlists"
    netlists.mkdir(parents=True)
    frozen = []
    for index, row in enumerate(selected):
        target = f"archive_{index:03d}"
        target_path = netlists / f"{target}.v"
        shutil.copyfile(row["source_netlist"], target_path)
        if sha256(target_path) != row["sha256"]:
            raise RuntimeError(f"copy verification failed: {target_path}")
        frozen.append(
            {
                **row,
                "target": target,
                "netlist": str(target_path.relative_to(root)),
                "internal_da_front": row["sha256"] in da_sha,
                "internal_dp_front": row["sha256"] in dp_sha,
            }
        )
    result = {
        "schema": "escope-pareto-frozen-pool-v1",
        "objective": objective,
        "selection_frozen_before_external_evaluation": True,
        "selection": "union of exact Internal-NLDM-V3 delay-area and delay-power fronts",
        "source_candidate_count": source_count,
        "unique_sha_count": len(rows),
        "internal_da_front_count": len(da),
        "internal_dp_front_count": len(dp),
        "selected_count": len(frozen),
        "candidates": frozen,
    }
    dump(manifest_path, result)
    return result


def verify_search_complete(root: Path, objectives: list[str]) -> None:
    expected = {point["anchor"] for point in load(MANIFEST)["points"] if point.get("search_anchor")}
    for objective in objectives:
        present = set()
        for anchor in expected:
            status_path = root / objective / anchor / "status.json"
            if status_path.is_file() and load(status_path).get("status") == "search_complete":
                present.add(anchor)
        missing = sorted(expected - present)
        if missing:
            raise RuntimeError(f"{objective} is missing completed search anchors: {', '.join(missing)}")


def freeze(root: Path, objectives: list[str], require_complete: bool = False) -> dict:
    root = root.resolve()
    if require_complete:
        verify_search_complete(root, objectives)
    manifests = {objective: freeze_objective(root, objective) for objective in objectives}
    receipt = {
        "schema": "escope-pareto-freeze-receipt-v1",
        "selection_frozen_before_external_evaluation": True,
        "objectives": {
            name: {
                "manifest": str((root / "frozen" / name / "manifest.json").relative_to(root)),
                "selected_count": value["selected_count"],
                "manifest_sha256": sha256(root / "frozen" / name / "manifest.json"),
            }
            for name, value in manifests.items()
        },
    }
    dump(root / "frozen" / "FREEZE_RECEIPT.json", receipt)
    return receipt


def evaluation_tasks(root: Path, objectives: list[str]) -> list[dict]:
    experiment = load(MANIFEST)
    tasks = []
    for point in experiment["points"]:
        netlist = (HERE / point["g0_path"]).resolve()
        tasks.append(
            {
                "kind": "g0",
                "id": f"g0/{point['point_id']}",
                "point_id": point["point_id"],
                "period_ps": point["period_ps"],
                "anchor": point.get("anchor"),
                "netlist": str(netlist),
                "sha256": point["g0_sha256"],
                "top": module_name(netlist),
                "report_dir": str((root / "external/genus/g0" / point["point_id"]).resolve()),
            }
        )
    for objective in objectives:
        frozen = load(root / "frozen" / objective / "manifest.json")
        for row in frozen["candidates"]:
            netlist = (root / row["netlist"]).resolve()
            tasks.append(
                {
                    "kind": "candidate",
                    "id": f"candidate/{objective}/{row['target']}",
                    "objective": objective,
                    "target": row["target"],
                    "anchor": row["anchor"],
                    "netlist": str(netlist),
                    "sha256": row["sha256"],
                    "top": module_name(netlist),
                    "report_dir": str((root / "external/genus" / objective / row["target"]).resolve()),
                }
            )
    return tasks


def parse_genus(report_dir: Path) -> dict:
    timing = (report_dir / "timing.rpt").read_text()
    area = (report_dir / "area.rpt").read_text()
    power = (report_dir / "power.rpt").read_text()
    data_match = re.search(r"Data Path:-\s*([0-9.eE+-]+)", timing)
    adjust_match = re.search(r"Drv Adjust:\+\s*[0-9.eE+-]+\s+([0-9.eE+-]+)", timing)
    area_lines = [line for line in area.splitlines() if re.match(r"^\S+\s+NA\s+\d+", line)]
    power_lines = [line for line in power.splitlines() if re.match(r"^\s*logic\s+", line)]
    if not data_match or not adjust_match or not area_lines or not power_lines:
        raise RuntimeError(f"cannot parse Genus reports in {report_dir}")
    data_path = float(data_match.group(1))
    driver_adjust = float(adjust_match.group(1))
    return {
        "data_path_ps": data_path,
        "driver_adjust_ps": driver_adjust,
        "boundary_delay_ps": data_path + driver_adjust,
        "area_um2": float(area_lines[-1].split()[5]),
        "power_uw": float(power_lines[-1].split()[4]) * 1.0e6,
    }


def run_genus(root: Path, objectives: list[str], liberty: Path, genus: Path, chunk: int, timeout: int) -> dict:
    tasks = evaluation_tasks(root, objectives)
    dump(root / "external" / "tasks.json", {"tasks": tasks})
    missing = []
    for task in tasks:
        report = Path(task["report_dir"])
        receipt = report / "INPUT.json"
        if all((report / name).is_file() for name in ("timing.rpt", "area.rpt", "power.rpt")) and receipt.is_file():
            if load(receipt).get("sha256") != task["sha256"]:
                raise RuntimeError(f"stale Genus report: {report}")
        else:
            missing.append(task)
    sessions = root / "external" / "sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    for offset in range(0, len(missing), chunk):
        batch = missing[offset : offset + chunk]
        attempt = 1
        session = sessions / f"batch_{offset:04d}_attempt_{attempt:02d}"
        while session.exists():
            attempt += 1
            session = sessions / f"batch_{offset:04d}_attempt_{attempt:02d}"
        session.mkdir()
        items = session / "items.txt"
        items.write_text("\n".join(
            f"{task['id']}|{task['netlist']}|{task['top']}|{task['report_dir']}" for task in batch
        ) + "\n")
        env = os.environ.copy()
        env.update({
            "E_SCOPE_GENUS_LIBERTY": str(liberty.resolve()),
            "E_SCOPE_BATCH_ITEMS_FILE": str(items.resolve()),
        })
        started = time.perf_counter()
        result = subprocess.run(
            [str(genus.resolve()), "-files", str(TCL.resolve()), "-log", str(session / "genus.log")],
            cwd=session, env=env, text=True, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, timeout=timeout,
        )
        elapsed = time.perf_counter() - started
        (session / "stdout.log").write_text(result.stdout)
        if result.returncode or f"E_SCOPE_BATCH_DONE points={len(batch)}" not in result.stdout:
            raise RuntimeError(f"Genus batch failed; see {session}")
        dump(session / "receipt.json", {"task_ids": [task["id"] for task in batch], "wall_sec": elapsed})
        for task in batch:
            dump(Path(task["report_dir"]) / "INPUT.json", {
                "id": task["id"], "netlist": task["netlist"], "sha256": task["sha256"],
                "top": task["top"], "liberty_sha256": sha256(liberty),
            })
    results, failures = [], []
    for task in tasks:
        try:
            path = Path(task["netlist"])
            if sha256(path) != task["sha256"]:
                raise RuntimeError("input SHA changed")
            results.append({**task, "ppa": parse_genus(Path(task["report_dir"])), "status": "PASS"})
        except Exception as error:
            failures.append({"id": task["id"], "error": str(error)})
    summary = {"results": results, "failures": failures}
    dump(root / "external" / "results.json", summary)
    if failures:
        raise RuntimeError(f"{len(failures)} Genus results failed")
    return summary


def normalize_verilog(text: str) -> str:
    escaped = set(re.findall(r"\\([^\s]+)", text))
    mapping = {name: "egg_esc_" + name.encode().hex() for name in sorted(escaped)}
    text = re.sub(r"\\([^\s]+)(\s)", lambda match: mapping[match.group(1)] + match.group(2), text)
    return re.sub(r"1'[hH]([01])", r"1'b\1", text)


def yosys_quote(path: Path) -> str:
    return '"' + str(path).replace("\\", "\\\\").replace('"', '\\"') + '"'


def run_yosys(yosys: Path, command: str, log: Path, timeout: int, datdir: Path | None) -> float:
    env = os.environ.copy()
    if datdir:
        env["YOSYS_DATDIR"] = str(datdir.resolve())
    env["PATH"] = str(yosys.resolve().parent) + os.pathsep + env.get("PATH", "")
    started = time.perf_counter()
    result = subprocess.run(
        [str(yosys.resolve()), "-Q", "-p", command], cwd=log.parent,
        env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    log.write_text(result.stdout)
    if result.returncode:
        raise RuntimeError(f"Yosys failed; see {log}")
    return time.perf_counter() - started


def validate_candidate(task: dict, out: Path, liberty: Path, yosys: Path, abc: Path, timeout: int, datdir: Path | None) -> dict:
    status = out / "status.json"
    target = Path(task["netlist"])
    source = Path(task["g0"])
    if status.exists():
        prior = load(status)
        if prior.get("target_sha256") == task["sha256"] and prior.get("formal_equivalence") == "PASS":
            return prior
        raise RuntimeError(f"stale formal result: {status}")
    out.mkdir(parents=True, exist_ok=True)
    source_normalized = out / "g0_normalized.v"
    source_normalized.write_text(normalize_verilog(source.read_text()))
    target_top = module_name(target)
    source_top = module_name(source_normalized)
    design_json = out / "mapped_design.json"
    cold = out / "cold_rewrite.v"
    liberty_q = yosys_quote(liberty)
    legality = "; ".join([
        f"read_liberty -lib {liberty_q}", f"read_verilog {yosys_quote(target)}",
        f"hierarchy -check -top {target_top}", "check -assert", r"select -assert-none t:\$*",
        f"write_json {yosys_quote(design_json)}", f"write_verilog -noattr {yosys_quote(cold)}",
    ])
    legality_sec = run_yosys(yosys, legality, out / "mapped_legality.log", timeout, datdir)
    design = load(design_json)
    cells = design["modules"][target_top]["cells"]
    histogram = collections.Counter(str(cell["type"]) for cell in cells.values())
    library_cells = {match.group(1) for match in re.finditer(r"^\s*cell\s*\(([^)]+)\)", liberty.read_text(), re.MULTILINE)}
    unresolved = sorted(set(histogram) - library_cells)
    generic = sorted(cell for cell in histogram if cell.startswith("$"))
    if unresolved or generic:
        raise RuntimeError(f"illegal cells unresolved={unresolved} generic={generic}")
    gold = out / "g0.aig"
    gate = out / "candidate.aig"
    def aig_command(netlist: Path, top: str, output: Path) -> str:
        return "; ".join([
            f"read_liberty -ignore_miss_func -ignore_miss_dir {liberty_q}",
            f"read_verilog {yosys_quote(netlist)}", f"hierarchy -check -top {top}",
            "flatten", "proc", "opt", "memory", "opt", "techmap", "opt", "aigmap",
            f"write_aiger -symbols {yosys_quote(output)}",
        ])
    gold_sec = run_yosys(yosys, aig_command(source_normalized, source_top, gold), out / "g0_aig.log", timeout, datdir)
    gate_sec = run_yosys(yosys, aig_command(cold, target_top, gate), out / "candidate_aig.log", timeout, datdir)
    started = time.perf_counter()
    result = subprocess.run(
        [str(abc.resolve()), "-c", f"cec -T {timeout} -p {gold} {gate}"],
        cwd=out, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    abc_sec = time.perf_counter() - started
    (out / "abc_cec.log").write_text(result.stdout)
    if result.returncode or "Networks are equivalent" not in result.stdout:
        raise RuntimeError(f"ABC CEC failed; see {out / 'abc_cec.log'}")
    row = {
        "objective": task["objective"], "target": task["target"], "anchor": task["anchor"],
        "source_g0": str(source.resolve()), "source_sha256": sha256(source),
        "target_netlist": str(target.resolve()), "target_sha256": sha256(target),
        "mapped_only_legality": "PASS", "full_library_only": "PASS", "unresolved_cells": 0,
        "write_reparse_cold_read": "PASS", "formal_equivalence": "PASS",
        "formal_backend": "Yosys AIG export plus ABC CEC", "instance_count": sum(histogram.values()),
        "legality_wall_sec": legality_sec, "formal_wall_sec": gold_sec + gate_sec + abc_sec,
    }
    dump(status, row)
    return row


def run_formal(root: Path, objectives: list[str], liberty: Path, yosys: Path, abc: Path, jobs: int, timeout: int, datdir: Path | None) -> dict:
    points = {point["anchor"]: point for point in load(MANIFEST)["points"] if point.get("anchor")}
    tasks = []
    for objective in objectives:
        for row in load(root / "frozen" / objective / "manifest.json")["candidates"]:
            tasks.append({
                "objective": objective, "target": row["target"], "anchor": row["anchor"],
                "sha256": row["sha256"], "netlist": str((root / row["netlist"]).resolve()),
                "g0": str((HERE / points[row["anchor"]]["g0_path"]).resolve()),
            })
    results, failures = [], []
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {
            pool.submit(validate_candidate, task, root / "formal" / task["objective"] / task["target"], liberty, yosys, abc, timeout, datdir): task
            for task in tasks
        }
        for future in concurrent.futures.as_completed(futures):
            task = futures[future]
            try:
                results.append(future.result())
            except Exception as error:
                failures.append({"objective": task["objective"], "target": task["target"], "error": repr(error)})
    results.sort(key=lambda row: (row["objective"], row["target"]))
    summary = {"results": results, "failures": failures}
    dump(root / "formal" / "results.json", summary)
    if failures:
        raise RuntimeError(f"{len(failures)} formal validations failed")
    return summary


def objective_label(objective: str, root: Path) -> str:
    plan_paths = sorted((root / objective).glob("*/PLAN.json"))
    if not plan_paths:
        return objective
    spec = load(plan_paths[0])["objective_spec"]["objective"]
    exponents = spec.get("exponents", {})
    known = {
        (1.0, 1.0, 0.0): "D×A",
        (2.0, 1.0, 1.0): "D²×A×P",
        (1.0, 0.0, 2.0): "D×P²",
    }
    key = tuple(float(exponents.get(metric, 0)) for metric in ("delay", "area", "power"))
    return known.get(key, load(plan_paths[0])["objective_name"])


def pareto_indices(rows: list[dict], y: str) -> set[int]:
    result = set()
    for index, row in enumerate(rows):
        if not any(
            other_index != index
            and other["boundary_delay_ps"] <= row["boundary_delay_ps"]
            and other[y] <= row[y]
            and (other["boundary_delay_ps"] < row["boundary_delay_ps"] or other[y] < row[y])
            for other_index, other in enumerate(rows)
        ):
            result.add(index)
    return result


def write_figure(root: Path, objectives: list[str]) -> dict:
    external = load(root / "external" / "results.json")
    formal = load(root / "formal" / "results.json")
    if external["failures"] or formal["failures"]:
        raise RuntimeError("validation contains failures")
    formal_sha = {row["target_sha256"] for row in formal["results"] if row["formal_equivalence"] == "PASS"}
    g0 = [row for row in external["results"] if row["kind"] == "g0"]
    candidates = [row for row in external["results"] if row["kind"] == "candidate" and row["sha256"] in formal_sha]
    if len(candidates) != sum(load(root / "frozen" / name / "manifest.json")["selected_count"] for name in objectives):
        raise RuntimeError("not every frozen candidate has both Genus and formal PASS")
    def flat(row: dict) -> dict:
        return {**{k: v for k, v in row.items() if k != "ppa"}, **row["ppa"]}
    g0_rows = [flat(row) for row in g0]
    portfolios = {name: [flat(row) for row in candidates if row["objective"] == name] for name in objectives}
    all_rows = g0_rows + [row for name in objectives for row in portfolios[name]]
    da_all, dp_all = pareto_indices(all_rows, "area_um2"), pareto_indices(all_rows, "power_uw")
    output = root / "figure7"
    output.mkdir(parents=True, exist_ok=True)
    fields = ["series", "objective", "id", "anchor", "sha256", "data_path_ps", "driver_adjust_ps", "boundary_delay_ps", "area_um2", "power_uw", "own_delay_area_pareto", "own_delay_power_pareto", "combined_delay_area_pareto", "combined_delay_power_pareto", "formal"]
    with (output / "figure7_data.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields); writer.writeheader()
        offset = 0
        for name, rows in [("Genus G0", g0_rows)] + [(name, portfolios[name]) for name in objectives]:
            own_da, own_dp = pareto_indices(rows, "area_um2"), pareto_indices(rows, "power_uw")
            for index, row in enumerate(rows):
                global_index = offset + index
                writer.writerow({
                    "series": "Genus G0" if name == "Genus G0" else "Iterative",
                    "objective": "" if name == "Genus G0" else objective_label(name, root),
                    "id": row["id"], "anchor": row.get("anchor") or "", "sha256": row["sha256"],
                    "data_path_ps": row["data_path_ps"], "driver_adjust_ps": row["driver_adjust_ps"],
                    "boundary_delay_ps": row["boundary_delay_ps"], "area_um2": row["area_um2"], "power_uw": row["power_uw"],
                    "own_delay_area_pareto": index in own_da, "own_delay_power_pareto": index in own_dp,
                    "combined_delay_area_pareto": global_index in da_all, "combined_delay_power_pareto": global_index in dp_all,
                    "formal": "INPUT" if name == "Genus G0" else "PASS",
                })
            offset += len(rows)
    try:
        os.environ.setdefault("MPLCONFIGDIR", str(output / ".matplotlib"))
        import matplotlib.pyplot as plt
    except ImportError as error:
        raise RuntimeError("matplotlib is required to generate fresh Figure 7") from error
    colors = ["#D95F02", "#7557A6", "#009E73", "#CC79A7"]
    markers = ["D", "P", "^", "s"]
    fig, axes = plt.subplots(1, 2, figsize=(15.2, 6.8))
    for axis, y, ylabel, title in ((axes[0], "area_um2", "Area (µm²)", "(a) Delay–Area"), (axes[1], "power_uw", "Power (µW)", "(b) Delay–Power")):
        gfront = sorted((g0_rows[i] for i in pareto_indices(g0_rows, y)), key=lambda row: row["boundary_delay_ps"])
        axis.scatter([r["boundary_delay_ps"] for r in g0_rows], [r[y] for r in g0_rows], color="#75A9CC", s=32, label="High-effort Genus Synthesis")
        axis.plot([r["boundary_delay_ps"] for r in gfront], [r[y] for r in gfront], color="#2878B5", linestyle="--")
        for position, name in enumerate(objectives):
            rows = portfolios[name]; front = sorted((rows[i] for i in pareto_indices(rows, y)), key=lambda row: row["boundary_delay_ps"])
            axis.scatter([r["boundary_delay_ps"] for r in rows], [r[y] for r in rows], color=colors[position % len(colors)], marker=markers[position % len(markers)], s=22, alpha=.5)
            axis.plot([r["boundary_delay_ps"] for r in front], [r[y] for r in front], color=colors[position % len(colors)], linewidth=2, label=f"Iterative with {objective_label(name, root)} Objective")
        axis.set_xlabel("Delay (ps)"); axis.set_ylabel(ylabel); axis.set_title(title, loc="left", fontweight="bold"); axis.grid(True, alpha=.35)
    handles, labels = axes[0].get_legend_handles_labels()
    fig.legend(handles, labels, loc="upper center", ncol=2, frameon=False)
    fig.tight_layout(rect=(0, 0, 1, .89))
    fig.savefig(output / "figure7.png", dpi=300)
    fig.savefig(output / "figure7.svg")
    plt.close(fig)
    summary = {
        "status": "PASS", "g0_count": len(g0_rows),
        "candidate_counts": {name: len(portfolios[name]) for name in objectives},
        "formal_pass": len(candidates), "figure_png": "figure7/figure7.png",
        "figure_svg": "figure7/figure7.svg", "data_csv": "figure7/figure7_data.csv",
    }
    dump(output / "summary.json", summary)
    return summary


def finalize(root: Path, objectives: list[str], liberty: Path, genus: Path, yosys: Path, abc: Path, genus_chunk: int, genus_timeout: int, formal_jobs: int, formal_timeout: int, yosys_datdir: Path | None = None) -> dict:
    root = root.resolve(); liberty = liberty.resolve()
    if sha256(liberty) != LIB_SHA256:
        raise RuntimeError(f"Liberty must have SHA-256 {LIB_SHA256}")
    for executable in (genus, yosys, abc):
        if not executable.expanduser().resolve().is_file():
            raise RuntimeError(f"missing executable: {executable}")
    frozen = freeze(root, objectives, require_complete=True)
    external = run_genus(root, objectives, liberty, genus.expanduser(), genus_chunk, genus_timeout)
    formal = run_formal(root, objectives, liberty, yosys.expanduser(), abc.expanduser(), formal_jobs, formal_timeout, yosys_datdir)
    figure = write_figure(root, objectives)
    result = {
        "status": "PASS", "selection_frozen_before_external_evaluation": True,
        "frozen": frozen, "genus_pass": len(external["results"]),
        "formal_pass": len(formal["results"]), "figure": figure,
    }
    dump(root / "REPRODUCTION.json", result)
    return result

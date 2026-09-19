#!/usr/bin/env python3
"""Evaluate ablation search outputs and export PPA and comparison CSVs."""

from __future__ import annotations

import concurrent.futures
import csv
import hashlib
import importlib.util
import json
import math
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
MODES = ("phase1-only", "phase2-only", "no-pi-drive-expansion")


def load(path: Path):
    return json.loads(path.read_text())


def dump(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def external_helpers():
    path = HERE.parent / "pareto/pipeline.py"
    spec = importlib.util.spec_from_file_location("escope_external_helpers", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load validation helpers from {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def safe_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]", "_", value)


def source_path(value: str, search_root: Path) -> Path:
    path = Path(value)
    choices = [path, search_root / path, search_root / "search" / path]
    for choice in choices:
        if choice.is_file():
            return choice.resolve()
    raise RuntimeError(f"missing search netlist: {value}")


def search_output(root: Path, benchmark: str, mode: str) -> Path:
    return root / benchmark / mode


def verify_searches(root: Path, points: list[dict]) -> None:
    missing = []
    for point in points:
        for mode in MODES:
            status = search_output(root, point["benchmark"], mode) / "status.json"
            if not status.is_file() or load(status).get("status") not in {
                "search_complete", "candidate_pool_complete"
            }:
                missing.append(f"{point['benchmark']}/{mode}")
    if missing:
        raise RuntimeError("missing completed ablation searches: " + ", ".join(missing))


def experiment_points(benchmarks: list[str] | None = None) -> list[dict]:
    points = load(MANIFEST)["points"]
    if benchmarks:
        unknown = set(benchmarks) - {point["benchmark"] for point in points}
        if unknown:
            raise RuntimeError(f"unknown benchmarks: {sorted(unknown)}")
        points = [point for point in points if point["benchmark"] in benchmarks]
    return points


def full_result(full_results: Path, point: dict) -> tuple[Path, dict]:
    stem = f"{point['benchmark']}__{point['anchor']}"
    if (full_results / "selected/receipt.json").is_file():
        directories = [full_results]
    else:
        directories = [full_results / stem, *sorted(full_results.glob(stem + "__attempt_*"))]
    complete = [path for path in directories
                if (path / "status.json").is_file()
                and load(path / "status.json").get("status") == "complete"]
    if not complete:
        raise RuntimeError(f"missing completed full-method result for {stem} in {full_results}")
    directory = complete[-1]
    plan = load(directory / "PLAN.json")
    for key in ("benchmark", "anchor", "g0_sha256"):
        if plan.get(key) != point[key]:
            raise RuntimeError(f"full-method {key} mismatch for {stem}: {directory}")
    receipt = load(directory / "selected/receipt.json")
    if receipt["method"] != point["selected_method"]:
        raise RuntimeError(
            f"{stem}: full result uses {receipt['method']}, ablation uses "
            f"{point['selected_method']}; supply full and ablated runs of the same method"
        )
    path = directory / "selected/mapped.v"
    if sha256(path) != receipt["selected_sha256"]:
        raise RuntimeError(f"full-method netlist SHA mismatch: {path}")
    return path.resolve(), receipt


def fixed_search_netlist(root: Path, point: dict, mode: str,
                         full_receipt: dict | None = None) -> tuple[Path, str]:
    output = search_output(root, point["benchmark"], mode)
    if point["selected_method"] == "Conquer" and mode == "phase1-only":
        candidate_id = full_receipt["candidate_id"]
        if candidate_id == "G0":
            return (HERE / point["g0_path"]).resolve(), "G0"
        if candidate_id == "NORMAL_POST":
            # Remove conditional sizing from the full normal-lane topology.
            # NORMAL_PRE need not be in the shortlisted external finalists.
            paths = list((output / "search").glob("**/normal/round_00/leader_pre_a2.v"))
            if len(paths) == 1:
                return paths[0].resolve(), "NORMAL_PRE"
            candidate_id = "NORMAL_PRE"
        finalists = load(output / "search/frozen_finalists.json")["finalists"]
        matches = [row for row in finalists if row["candidate_id"] == candidate_id]
        if len(matches) != 1:
            raise RuntimeError(
                f"{point['benchmark']}: Phase-I search has no unique counterpart "
                f"for full-method candidate {candidate_id!r}"
            )
        path = source_path(matches[0]["netlist"], output)
        if sha256(path) != matches[0]["sha256"]:
            raise RuntimeError(f"Phase-I finalist SHA mismatch: {path}")
        return path, candidate_id
    status = load(output / "status.json")
    return source_path(status["incumbent_path"], output), "incumbent"


def iterative_checkpoints(root: Path, point: dict, mode: str) -> list[tuple[Path, str, int]]:
    output = search_output(root, point["benchmark"], mode)
    paths = []
    for round_dir in sorted((output / "search").glob("round_*")):
        accepted = round_dir / "accepted.v"
        if accepted.is_file():
            number = int(round_dir.name.removeprefix("round_")) + 1
            paths.append((accepted.resolve(), f"R{number}", number))
    if not paths:
        path, _ = fixed_search_netlist(root, point, mode)
        paths.append((path, "incumbent", int(point["round_cap"])))
    unique = {}
    for path, candidate_id, number in paths:
        unique.setdefault(sha256(path), (path, candidate_id, number))
    return list(unique.values())


def stage(root: Path, full_results: Path, benchmarks: list[str] | None = None) -> dict:
    """Freeze all externally evaluated netlists before reading Genus output."""
    root = root.resolve()
    points = experiment_points(benchmarks)
    verify_searches(root, points)
    rows = []
    for point in points:
        benchmark = point["benchmark"]
        g0 = (HERE / point["g0_path"]).resolve()
        full, full_receipt = full_result(full_results, point)
        for kind, mode, candidate_id, path, expected in [
            ("g0", "g0", "G0", g0, point["g0_sha256"]),
            ("full", "full", "FULL", full, None),
        ]:
            actual = sha256(path)
            if expected is not None and actual != expected:
                raise RuntimeError(f"{kind} SHA mismatch for {benchmark}: {actual}")
            rows.append({"kind": kind, "benchmark": benchmark, "anchor": point["anchor"],
                         "method": point["selected_method"], "mode": mode,
                         "candidate_id": candidate_id, "source": str(path), "sha256": actual})
        for mode in MODES:
            if point["selected_method"] == "Iterative":
                for path, candidate_id, number in iterative_checkpoints(root, point, mode):
                    rows.append({"kind": "candidate", "benchmark": benchmark,
                                 "anchor": point["anchor"], "method": point["selected_method"],
                                 "mode": mode, "candidate_id": candidate_id, "round": number,
                                 "source": str(path), "sha256": sha256(path)})
            elif mode == "no-pi-drive-expansion":
                output = search_output(root, benchmark, mode)
                finalists = load(output / "search/frozen_finalists.json")["finalists"]
                candidates = [row for row in finalists if row["candidate_id"] != "G0"]
                if not candidates:
                    raise RuntimeError(f"empty no-drive finalist pool for {benchmark}")
                for candidate in candidates:
                    path = source_path(candidate["netlist"], output)
                    actual = sha256(path)
                    if actual != candidate["sha256"]:
                        raise RuntimeError(f"fresh finalist SHA mismatch: {benchmark}/{candidate['candidate_id']}")
                    rows.append({"kind": "candidate", "benchmark": benchmark,
                                 "anchor": point["anchor"], "method": point["selected_method"],
                                 "mode": mode, "candidate_id": candidate["candidate_id"],
                                 "round": candidate["round"], "source": str(path), "sha256": actual})
            else:
                path, candidate_id = fixed_search_netlist(root, point, mode, full_receipt)
                rows.append({"kind": "candidate", "benchmark": benchmark,
                             "anchor": point["anchor"], "method": point["selected_method"],
                             "mode": mode, "candidate_id": candidate_id,
                             "source": str(path), "sha256": sha256(path)})
    frozen_root = root / "frozen"
    manifest_path = frozen_root / "manifest.json"
    identity = [(row["kind"], row["benchmark"], row["mode"], row["candidate_id"], row["sha256"])
                for row in rows]
    if manifest_path.exists():
        prior = load(manifest_path)
        prior_identity = [(row["kind"], row["benchmark"], row["mode"],
                           row["candidate_id"], row["sha256"])
                          for row in prior["netlists"]]
        if prior_identity != identity:
            raise RuntimeError("frozen ablation selection changed")
        for row in prior["netlists"]:
            path = root / row["netlist"]
            if not path.is_file() or sha256(path) != row["sha256"]:
                raise RuntimeError(f"frozen netlist changed: {path}")
        return prior
    if (root / "external").exists() or (root / "formal").exists():
        raise RuntimeError("refusing to freeze after external validation started")
    frozen = []
    for ordinal, row in enumerate(rows):
        name = f"{ordinal:03d}_{safe_name(row['candidate_id'])}.v"
        destination = frozen_root / "netlists" / row["benchmark"] / row["mode"] / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(row["source"], destination)
        if sha256(destination) != row["sha256"]:
            raise RuntimeError(f"copy verification failed: {destination}")
        frozen.append({**row, "netlist": str(destination.relative_to(root))})
    result = {
        "schema": "escope-ablation-inputs-v1",
        "benchmarks": [point["benchmark"] for point in points],
        "selection_frozen_before_external_evaluation": True,
        "phase1_selection": "pre-sizing counterpart of the supplied full-method selection",
        "iterative_selection": "formal-clean minimum external D2AP checkpoint without G0 fallback",
        "conquer_no_drive_selection": "formal-clean minimum external D2AP with G0 fallback after pool freezing",
        "netlists": frozen,
    }
    dump(manifest_path, result)
    dump(frozen_root / "FREEZE_RECEIPT.json", {
        "manifest": "frozen/manifest.json", "manifest_sha256": sha256(manifest_path),
        "netlist_count": len(frozen), "selection_frozen_before_external_evaluation": True,
    })
    return result


def tasks(root: Path) -> list[dict]:
    helpers = external_helpers()
    experiment = {row["benchmark"]: row for row in load(MANIFEST)["points"]}
    result = []
    for ordinal, row in enumerate(load(root / "frozen/manifest.json")["netlists"]):
        netlist = (root / row["netlist"]).resolve()
        result.append({**row, "id": f"{row['kind']}/{row['benchmark']}/{row['mode']}/{ordinal:03d}",
                       "netlist": str(netlist), "top": helpers.module_name(netlist),
                       "g0": str((HERE / experiment[row["benchmark"]]["g0_path"]).resolve()),
                       "report_dir": str((root / "external/genus" / row["benchmark"] /
                                          row["mode"] / f"{ordinal:03d}").resolve())})
    return result


def run_genus(root: Path, liberty: Path, genus: Path, chunk: int, timeout: int) -> dict:
    helpers = external_helpers()
    all_tasks = tasks(root)
    dump(root / "external/tasks.json", {"tasks": all_tasks})
    missing = []
    for task in all_tasks:
        report = Path(task["report_dir"])
        receipt = report / "INPUT.json"
        complete = all((report / name).is_file() for name in ("timing.rpt", "area.rpt", "power.rpt"))
        if complete and receipt.is_file():
            if (load(receipt).get("sha256") != task["sha256"]
                    or load(receipt).get("liberty_sha256") != sha256(liberty)):
                raise RuntimeError(f"stale Genus report: {report}")
        else:
            missing.append(task)
    sessions = root / "external/sessions"
    sessions.mkdir(parents=True, exist_ok=True)
    for offset in range(0, len(missing), chunk):
        batch = missing[offset:offset + chunk]
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
        env.update({"E_SCOPE_GENUS_LIBERTY": str(liberty.resolve()),
                    "E_SCOPE_BATCH_ITEMS_FILE": str(items.resolve())})
        started = time.perf_counter()
        result = subprocess.run([str(genus.resolve()), "-files", str(TCL.resolve()),
                                 "-log", str(session / "genus.log")],
                                cwd=session, env=env, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=timeout)
        elapsed = time.perf_counter() - started
        (session / "stdout.log").write_text(result.stdout)
        if result.returncode or f"E_SCOPE_BATCH_DONE points={len(batch)}" not in result.stdout:
            raise RuntimeError(f"Genus batch failed; see {session}")
        dump(session / "receipt.json", {"task_ids": [task["id"] for task in batch],
                                         "wall_sec": elapsed})
        for task in batch:
            dump(Path(task["report_dir"]) / "INPUT.json", {
                "id": task["id"], "netlist": task["netlist"], "sha256": task["sha256"],
                "top": task["top"], "liberty_sha256": sha256(liberty),
            })
    results, failures = [], []
    for task in all_tasks:
        try:
            if sha256(Path(task["netlist"])) != task["sha256"]:
                raise RuntimeError("input SHA changed")
            results.append({**task, "ppa": helpers.parse_genus(Path(task["report_dir"])),
                            "status": "PASS"})
        except Exception as error:
            failures.append({"id": task["id"], "error": str(error)})
    summary = {"results": results, "failures": failures}
    dump(root / "external/results.json", summary)
    if failures:
        raise RuntimeError(f"{len(failures)} Genus results failed")
    return summary


def run_formal(root: Path, liberty: Path, yosys: Path, abc: Path, jobs: int,
               timeout: int, datdir: Path | None) -> dict:
    helpers = external_helpers()
    candidates = [task for task in tasks(root) if task["kind"] != "g0"]
    results, failures = [], []
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {}
        for ordinal, task in enumerate(candidates):
            formal_task = {**task, "objective": task["mode"],
                           "target": f"{task['benchmark']}_{ordinal:03d}"}
            out = root / "formal" / task["benchmark"] / task["mode"] / f"{ordinal:03d}"
            future = pool.submit(helpers.validate_candidate, formal_task, out, liberty,
                                 yosys, abc, timeout, datdir)
            futures[future] = task
        for future in concurrent.futures.as_completed(futures):
            task = futures[future]
            try:
                receipt = future.result()
                results.append({**task, "receipt": receipt})
            except Exception as error:
                failures.append({"id": task["id"], "error": repr(error)})
    results.sort(key=lambda row: row["id"])
    summary = {"results": results, "failures": failures}
    dump(root / "formal/results.json", summary)
    if failures:
        raise RuntimeError(f"{len(failures)} formal validations failed")
    return summary


def d2ap(ppa: dict) -> float:
    return ppa["boundary_delay_ps"] ** 2 * ppa["area_um2"] * ppa["power_uw"]


def select_results(root: Path) -> list[dict]:
    external = load(root / "external/results.json")
    formal = load(root / "formal/results.json")
    if external["failures"] or formal["failures"]:
        raise RuntimeError("validation contains failures")
    formal_ids = {row["id"] for row in formal["results"]
                  if row["receipt"]["formal_equivalence"] == "PASS"}
    selected = []
    for point in experiment_points(load(root / "frozen/manifest.json")["benchmarks"]):
        benchmark = point["benchmark"]
        rows = [row for row in external["results"] if row["benchmark"] == benchmark]
        g0 = next(row for row in rows if row["kind"] == "g0")
        full = next(row for row in rows if row["kind"] == "full")
        if full["id"] not in formal_ids:
            raise RuntimeError(f"full reference lacks formal PASS: {benchmark}")
        modes = {}
        for mode in MODES:
            candidates = [row for row in rows if row["kind"] == "candidate" and row["mode"] == mode]
            if not candidates:
                raise RuntimeError(f"missing candidates: {benchmark}/{mode}")
            if point["selected_method"] == "Conquer" and mode == "no-pi-drive-expansion":
                allowed = [row for row in candidates if row["id"] in formal_ids]
                winner = min([g0] + allowed, key=lambda row: (d2ap(row["ppa"]),
                                                              int(row.get("round", 0))))
            elif point["selected_method"] == "Iterative":
                allowed = [row for row in candidates if row["id"] in formal_ids]
                if len(allowed) != len(candidates):
                    raise RuntimeError(f"Iterative checkpoint lacks formal PASS: {benchmark}/{mode}")
                winner = min(allowed, key=lambda row: (d2ap(row["ppa"]),
                                                       int(row.get("round", 0))))
            else:
                if len(candidates) != 1 or candidates[0]["id"] not in formal_ids:
                    raise RuntimeError(f"fixed result lacks formal PASS: {benchmark}/{mode}")
                winner = candidates[0]
            modes[mode] = winner
        selected.append({"benchmark": benchmark, "anchor": point["anchor"],
                         "selected_method": point["selected_method"], "g0": g0,
                         "full": full, "modes": modes})
    dump(root / "selection/results.json", {"results": selected})
    return selected


def gm(values: list[float]) -> float:
    return math.exp(sum(math.log(value) for value in values) / len(values))


def write_csv(root: Path) -> dict:
    selected = select_results(root)
    rows = []
    for entry in selected:
        g0_value = d2ap(entry["g0"]["ppa"])
        full_value = d2ap(entry["full"]["ppa"])
        values = {mode: d2ap(entry["modes"][mode]["ppa"]) for mode in MODES}
        phase_i_ratio = full_value / values["phase1-only"]
        phase_ii_ratio = full_value / values["phase2-only"]
        no_drive_ratio = full_value / values["no-pi-drive-expansion"]
        rows.append({
            "benchmark": entry["benchmark"], "anchor": entry["anchor"],
            "selected_flow": entry["selected_method"], "g0_d2ap": g0_value,
            "full_d2ap_over_g0": full_value / g0_value,
            "phase_i_only_d2ap_over_g0": values["phase1-only"] / g0_value,
            "phase_ii_only_d2ap_over_g0": values["phase2-only"] / g0_value,
            "without_drive_d2ap_over_g0": values["no-pi-drive-expansion"] / g0_value,
            "full_over_phase_i_only": phase_i_ratio,
            "full_over_phase_ii_only": phase_ii_ratio,
            "full_over_without_drive": no_drive_ratio,
            "full_sha256": entry["full"]["sha256"],
            "phase_i_sha256": entry["modes"]["phase1-only"]["sha256"],
            "phase_ii_sha256": entry["modes"]["phase2-only"]["sha256"],
            "without_drive_sha256": entry["modes"]["no-pi-drive-expansion"]["sha256"],
            "formal": "PASS",
        })
    output = root / "results"
    output.mkdir(parents=True, exist_ok=True)
    fields = list(rows[0])
    with (output / "ablation.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=fields)
        writer.writeheader(); writer.writerows(rows)
    exact = {
        "full_over_phase_i_only": gm([row["full_over_phase_i_only"] for row in rows]),
        "full_over_phase_ii_only": gm([row["full_over_phase_ii_only"] for row in rows]),
        "full_over_without_drive": gm([row["full_over_without_drive"] for row in rows]),
    }
    exact.update({
        "full_reduction_vs_phase_i_only_percent": 100 * (1 - exact["full_over_phase_i_only"]),
        "full_reduction_vs_phase_ii_only_percent": 100 * (1 - exact["full_over_phase_ii_only"]),
        "full_reduction_vs_without_drive_percent": 100 * (1 - exact["full_over_without_drive"]),
    })
    ppa_rows = []
    for entry in selected:
        for mode, result in [("g0", entry["g0"]), ("full", entry["full"]), *entry["modes"].items()]:
            ppa_rows.append({
                "benchmark": entry["benchmark"], "anchor": entry["anchor"],
                "method": entry["selected_method"], "mode": mode,
                "candidate_id": result["candidate_id"],
                "delay_ps": result["ppa"]["boundary_delay_ps"],
                "area_um2": result["ppa"]["area_um2"],
                "power_uw": result["ppa"]["power_uw"], "d2ap": d2ap(result["ppa"]),
                "sha256": result["sha256"], "netlist": result["netlist"],
                "formal": "reference" if result["kind"] == "g0" else "PASS",
            })
    with (output / "ppa.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=list(ppa_rows[0]))
        writer.writeheader()
        writer.writerows(ppa_rows)
    summary = {"status": "PASS", "rows": len(rows), "geometric_means": exact,
               "data_csv": "results/ablation.csv", "ppa_csv": "results/ppa.csv"}
    dump(output / "summary.json", summary)
    return summary


def finalize(root: Path, full_results: Path, liberty: Path, genus: Path, yosys: Path, abc: Path,
             genus_chunk: int, genus_timeout: int, formal_jobs: int,
             formal_timeout: int, yosys_datdir: Path | None = None,
             benchmarks: list[str] | None = None) -> dict:
    root = root.resolve(); liberty = liberty.resolve()
    if not liberty.is_file() or sha256(liberty) != LIB_SHA256:
        raise RuntimeError(f"Liberty must have SHA-256 {LIB_SHA256}")
    executables = [path.expanduser().resolve() for path in (genus, yosys, abc)]
    if any(not path.is_file() for path in executables):
        raise RuntimeError("Genus, Yosys, and ABC executable paths must exist")
    frozen = stage(root, full_results.resolve(), benchmarks)
    external = run_genus(root, liberty, executables[0], genus_chunk, genus_timeout)
    formal = run_formal(root, liberty, executables[1], executables[2], formal_jobs,
                        formal_timeout, yosys_datdir)
    tables = write_csv(root)
    result = {"status": "PASS", "selection_frozen_before_external_evaluation": True,
              "frozen_netlists": len(frozen["netlists"]),
              "genus_pass": len(external["results"]),
              "formal_pass": len(formal["results"]), "tables": tables}
    dump(root / "evaluation.json", result)
    return result

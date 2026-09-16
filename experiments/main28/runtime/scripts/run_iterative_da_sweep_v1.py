#!/usr/bin/env python3
"""Run isolated Iterative searches under explicit product objectives.

This is an isolated objective experiment.  It preserves the Iterative
candidate operators, progressive Exact schedule, conditional A2/O1, cold
rebuild and Internal-NLDM-V3 boundary.  Only ObjectiveSpec changes.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time


ROOT = Path(__file__).resolve().parents[1]
D1 = ROOT
LIB = Path(os.environ.get("E_SCOPE_LIBERTY", ROOT / "assets/library.lib")).resolve()
AUDIT = Path(os.environ.get("E_SCOPE_LIBRARY_AUDIT", ROOT / "assets/library_audit.json")).resolve()
SCALE = ROOT / "assets/6t_full_comb_scale_rules.json"
RULES = ROOT / "assets/runtime_full_comb_rules.json"
ULTRA = ROOT / "config/iterative_search.json"
BIN_DIR = Path(os.environ.get("E_SCOPE_BIN_DIR", ROOT / "target/release")).resolve()
BINARY = BIN_DIR / "run_generator_union_native"
EVALUATOR = BIN_DIR / "run_frozen_a2_fast"

ANCHORS = {}

OBJECTIVES = {
    "da": {"delay": 1.0, "area": 1.0, "power": 0.0},
    "da2": {"delay": 1.0, "area": 2.0, "power": 0.0},
    "d2a": {"delay": 2.0, "area": 1.0, "power": 0.0},
    "d2ap": {"delay": 2.0, "area": 1.0, "power": 1.0},
    "d2a_guard_1001": {"delay": 2.0, "area": 1.0, "power": 0.0},
    "area_d100": {"delay": 0.0, "area": 1.0, "power": 0.0},
    "area_d095": {"delay": 0.0, "area": 1.0, "power": 0.0},
    "delay_a100": {"delay": 1.0, "area": 0.0, "power": 0.0},
    "dp": {"delay": 1.0, "area": 0.0, "power": 1.0},
    "power_d100": {"delay": 0.0, "area": 0.0, "power": 1.0},
}
# New research objectives are opt-in; preserve the historical default sweep.
DEFAULT_OBJECTIVES = tuple(OBJECTIVES)
OBJECTIVES["dp2"] = {"delay": 1.0, "area": 0.0, "power": 2.0}


def dump(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def load(path: Path) -> object:
    return json.loads(path.read_text())


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def frozen_env(resource_jobs: int) -> dict[str, str]:
    env = os.environ.copy()
    env.update(
        {
            "EGG_CARGO_OFFLINE": "1",
            "EGG_LIB_PATH": str(LIB),
            "EGG_SCALE_RULES": str(SCALE),
            "EGG_RULE_PATHS": str(RULES),
            "EGG_ITERATIVE": "1",
            "EGG_UNIFIED_EQUIVALENCE_CANDIDATES": "1",
            "EGG_V8_ULTRA_CONFIG": str(ULTRA),
            "EGG_FROZEN_RESOURCE_JOBS": str(resource_jobs),
            "EGG_GENERATOR_JOBS": str(resource_jobs),
            "EGG_INTERNAL_TIMING_MODEL": "driver-aware-v3",
            "EGG_PI_ARRIVAL_PS": "0",
            "EGG_PI_SLEW_PS": "20",
            "EGG_DRIVER_INPUT_SLEW_PS": "0",
            "EGG_PO_LOAD_FF": "5.76",
            "EGG_POWER_MODE": "vectorless",
            "EGG_POWER_ACTIVITY_MODEL": "uniform",
            "EGG_POWER_PERIOD_PS": "1000",
            "EGG_POWER_VOLTAGE_V": "0.7",
            "EGG_PI_TOGGLE_PER_CYCLE": "0.1",
            "EGG_INTERNAL_TRANSITION_FACTOR": "1.78",
            "EGG_OBJECTIVE_ENGINE": "v8-pareto",
            "EGG_OBJECTIVE_LOCAL_REWRITE_CLOSURE": "0",
            "EGG_OBJECTIVE_MAPPED_WINDOW_REWRITE": "1",
            "EGG_OBJECTIVE_COMPATIBLE_CLOSURE": "1",
            "EGG_OBJECTIVE_BOUNDED_FUNCTIONAL_WINDOW": "0",
        }
    )
    env.pop("EGG_EXPERIMENTAL_OBJECTIVE", None)
    return env


def g0_path(benchmark: str, target: str) -> Path:
    raise RuntimeError("main28 requires an explicit portable anchor manifest")


def evaluate_g0(
    output_root: Path, benchmark: str, target: str, g0: Path, env: dict[str, str]
) -> dict:
    output = output_root / "g0_internal" / benchmark / f"{target}.json"
    if output.exists():
        rows = load(output)
        if len(rows) == 1 and Path(rows[0]["input"]).resolve() == g0.resolve():
            return rows[0]
        raise RuntimeError(f"stale G0 replay: {output}")
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [str(EVALUATOR), str(output), str(g0)], cwd=D1, env=env, check=True
    )
    return load(output)[0]


def objective_spec(name: str, g0: dict) -> dict:
    constraints = []
    if name == "d2a_guard_1001":
        constraints.append(
            {
                "metric": "delay",
                "relation": "at_most",
                "bound": float(g0["delay"]) * 1.001,
            }
        )
    elif name in ("area_d100", "power_d100"):
        constraints.append(
            {
                "metric": "delay",
                "relation": "at_most",
                "bound": float(g0["delay"]),
            }
        )
    elif name == "area_d095":
        constraints.append(
            {
                "metric": "delay",
                "relation": "at_most",
                "bound": float(g0["delay"]) * 0.95,
            }
        )
    elif name == "delay_a100":
        constraints.append(
            {
                "metric": "area",
                "relation": "at_most",
                "bound": float(g0["area"]),
            }
        )
    return {
        "schema_version": 1,
        "name": f"iterative_{name}_v1",
        "objective": {"kind": "product", "exponents": OBJECTIVES[name]},
        "constraints": constraints,
    }


def run_one(task: dict, output_root: Path, env: dict[str, str], timeout: int) -> dict:
    objective = task["objective"]
    benchmark = task["benchmark"]
    target = task["target"]
    g0 = Path(task["g0"]).resolve()
    spec = Path(task["spec"]).resolve()
    out = output_root / "search" / objective / benchmark / target
    status = out / "sweep_status.json"
    if status.exists():
        row = load(status)
        if (
            row.get("stage") == "complete"
            and row.get("g0_sha256") == sha256(g0)
            and row.get("objective_spec_sha256") == sha256(spec)
        ):
            return {"action": "reuse", **row}
        raise RuntimeError(f"stale status: {status}")
    if out.exists():
        raise RuntimeError(f"refusing partial output: {out}")

    out.parent.mkdir(parents=True, exist_ok=True)
    log = output_root / "logs" / objective / benchmark / f"{target}.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    command = [
        str(BINARY), benchmark, str(g0), str(out), "v3", str(task["max_rounds"]),
        str(LIB), str(SCALE), str(RULES),
    ]
    task_env = env.copy()
    task_env["EGG_OBJECTIVE_SPEC"] = str(spec)
    started = time.time()
    with log.open("w") as handle:
        process = subprocess.Popen(
            command, cwd=D1, env=task_env, stdout=handle, stderr=subprocess.STDOUT,
            text=True, start_new_session=True,
        )
        dump(
            out.parent / f"{target}.active.json",
            {
                "stage": "running", "pid": process.pid, "started_at_epoch": started,
                "command": command, "log": str(log), "objective_spec": str(spec),
            },
        )
        try:
            returncode = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGINT)
            try:
                process.wait(timeout=120)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=30)
            raise TimeoutError(f"trajectory exceeded {timeout} seconds")

    if returncode != 0 or not (out / "summary.json").exists():
        raise RuntimeError(f"Iterative failed with returncode={returncode}; see {log}")
    summary = load(out / "summary.json")
    row = {
        "stage": "complete",
        "benchmark": benchmark,
        "target": target,
        "objective": objective,
        "g0": str(g0),
        "g0_sha256": sha256(g0),
        "objective_spec": str(spec),
        "objective_spec_sha256": sha256(spec),
        "rounds_executed": int(summary["rounds_executed"]),
        "accepted_rounds": int(summary["accepted_rounds"]),
        "final_over_g0_internal": float(summary["final_over_g0"]),
        "incumbent_ppa": summary["incumbent_ppa"],
        "incumbent_path": summary["incumbent_path"],
        "total_search_exact": int(summary["total_all_search_exact"]),
        "total_physical_exact": int(summary["total_physical_search_exact"]),
        "wall_sec": time.time() - started,
        "log": str(log),
    }
    dump(status, row)
    (out.parent / f"{target}.active.json").unlink(missing_ok=True)
    return {"action": "run", **row}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-root", type=Path, required=True)
    parser.add_argument(
        "--anchor-manifest",
        type=Path,
        help="optional JSON with anchors [{benchmark,target,g0}] for an isolated custom frontier",
    )
    parser.add_argument("--cases", default="epfl_sin,epfl_multiplier")
    parser.add_argument(
        "--targets",
        default="",
        help="optional comma-separated anchor subset, e.g. t1_1.00x,t2_1.05x",
    )
    parser.add_argument("--objectives", default=",".join(DEFAULT_OBJECTIVES))
    parser.add_argument("--jobs", type=int, default=3)
    parser.add_argument("--resource-jobs", type=int, default=2)
    parser.add_argument("--max-rounds", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=7200)
    args = parser.parse_args()
    if args.anchor_manifest:
        custom = load(args.anchor_manifest)
        anchor_paths = {}
        for row in custom["anchors"]:
            key = (str(row["benchmark"]), str(row["target"]))
            if key in anchor_paths:
                parser.error(f"duplicate custom anchor {key[0]}/{key[1]}")
            path = Path(row["g0"])
            if not path.is_absolute():
                path = ROOT / path
            if not path.is_file():
                parser.error(f"custom G0 absent: {path}")
            anchor_paths[key] = path.resolve()
        cases = sorted({benchmark for benchmark, _ in anchor_paths})
        anchor_targets = {
            benchmark: sorted(target for current, target in anchor_paths if current == benchmark)
            for benchmark in cases
        }
    else:
        cases = [item for item in args.cases.split(",") if item]
        anchor_targets = {benchmark: list(ANCHORS.get(benchmark, {})) for benchmark in cases}
        anchor_paths = {
            (benchmark, target): g0_path(benchmark, target).resolve()
            for benchmark in cases
            for target in anchor_targets.get(benchmark, [])
        }
    targets = [item for item in args.targets.split(",") if item]
    objectives = [item for item in args.objectives.split(",") if item]
    unknown_cases = set() if args.anchor_manifest else set(cases) - set(ANCHORS)
    unknown_objectives = set(objectives) - set(OBJECTIVES)
    known_targets = {target for case in cases for target in anchor_targets.get(case, [])}
    unknown_targets = set(targets) - known_targets
    if unknown_cases or unknown_objectives or unknown_targets:
        parser.error(
            f"unknown cases={sorted(unknown_cases)} objectives={sorted(unknown_objectives)} "
            f"targets={sorted(unknown_targets)}"
        )
    if min(args.jobs, args.resource_jobs, args.max_rounds, args.timeout) <= 0:
        parser.error("jobs, resource-jobs, max-rounds and timeout must be positive")

    output_root = args.output_root.resolve()
    output_root.mkdir(parents=True, exist_ok=True)
    env = frozen_env(args.resource_jobs)
    subprocess.run(
        ["cargo", "build", "--release", "--offline", "--bin", BINARY.name],
        cwd=D1, env=env, check=True,
    )

    g0_rows = {}
    for benchmark in cases:
        for target in anchor_targets[benchmark]:
            if targets and target not in targets:
                continue
            row = evaluate_g0(
                output_root, benchmark, target, anchor_paths[benchmark, target], env
            )
            g0_rows[benchmark, target] = row
            for objective in objectives:
                spec_path = output_root / "specs" / objective / benchmark / f"{target}.json"
                spec = objective_spec(objective, row)
                if spec_path.exists() and load(spec_path) != spec:
                    raise RuntimeError(f"refusing stale ObjectiveSpec: {spec_path}")
                dump(spec_path, spec)

    tasks = []
    for objective in objectives:
        for benchmark in cases:
            for target in anchor_targets[benchmark]:
                if targets and target not in targets:
                    continue
                tasks.append(
                    {
                        "objective": objective, "benchmark": benchmark, "target": target,
                        "g0": str(anchor_paths[benchmark, target]),
                        "spec": str(output_root / "specs" / objective / benchmark / f"{target}.json"),
                        "max_rounds": args.max_rounds,
                    }
                )
    manifest = {
        "schema": "egg-v8-iterative-da-objective-sweep-v1",
        "method": "Iterative topology-first with conditional A2/O1",
        "max_rounds": args.max_rounds,
        "jobs": args.jobs,
        "resource_jobs_per_trajectory": args.resource_jobs,
        "anchor_manifest": str(args.anchor_manifest.resolve()) if args.anchor_manifest else None,
        "v8_ultra_config": str(ULTRA),
        "timing_boundary": {
            "primary_input_arrival_ps": 0.0,
            "primary_input_slew_ps": 20.0,
            "driver_input_slew_ps": 0.0,
            "primary_output_load_ff": 5.76,
            "internal_timing_model": "driver-aware-v3",
        },
        "objectives": OBJECTIVES,
        "delay_guard_ratio": 1.001,
        "tasks": tasks,
        "g0_internal": {f"{b}/{t}": row for (b, t), row in g0_rows.items()},
    }
    dump(output_root / "manifest.json", manifest)

    results = []
    failures = []
    started = time.time()
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        futures = {pool.submit(run_one, task, output_root, env, args.timeout): task for task in tasks}
        for future in concurrent.futures.as_completed(futures):
            task = futures[future]
            try:
                row = future.result()
                results.append(row)
                print(
                    f"DONE {len(results)}/{len(tasks)} {row['objective']} "
                    f"{row['benchmark']}/{row['target']} internal={row['final_over_g0_internal']:.9f} "
                    f"rounds={row['rounds_executed']} wall={row['wall_sec']:.1f}s",
                    flush=True,
                )
            except Exception as error:
                failures.append({**task, "error": str(error)})
                print(
                    f"FAIL {task['objective']} {task['benchmark']}/{task['target']}: {error}",
                    flush=True,
                )
            dump(
                output_root / "grid_status.json",
                {
                    "stage": "running", "pid": os.getpid(), "started_at_epoch": started,
                    "last_update_epoch": time.time(), "completed": len(results),
                    "failed": len(failures), "total": len(tasks),
                    "results": sorted(results, key=lambda row: (row["objective"], row["benchmark"], row["target"])),
                    "failures": failures,
                },
            )
    dump(
        output_root / "grid_status.json",
        {
            "stage": "complete" if not failures else "failed", "pid": None,
            "started_at_epoch": started, "finished_at_epoch": time.time(),
            "wall_sec": time.time() - started, "completed": len(results),
            "failed": len(failures), "total": len(tasks),
            "results": sorted(results, key=lambda row: (row["objective"], row["benchmark"], row["target"])),
            "failures": failures,
        },
    )
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

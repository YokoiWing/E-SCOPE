#!/usr/bin/env python3
"""Single entry point for the Section IV-C epfl_adder Pareto experiment."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
MAIN28 = HERE.parent / "main28"
RUNTIME = MAIN28 / "runtime"
MANIFEST = HERE / "manifest.json"
EXPECTED_LIB_SHA256 = "48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd"
BINARY = "run_generator_union_native"
PAPER_OBJECTIVES = ("da", "d2ap", "dp2")
OWNED: list[subprocess.Popen] = []


def load(path: Path):
    return json.loads(path.read_text())


def dump(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manifest() -> dict:
    return load(MANIFEST)


def anchors() -> list[dict]:
    return [point for point in manifest()["points"] if point["search_anchor"]]


def anchor_for(name: str) -> dict:
    rows = [point for point in anchors() if point["anchor"] == name]
    if len(rows) != 1:
        raise RuntimeError(f"unknown search anchor {name!r}")
    return rows[0]


def validate_objective(path: Path) -> dict:
    path = path.resolve()
    if not path.is_file():
        raise RuntimeError(f"missing objective file: {path}")
    spec = load(path)
    if spec.get("schema_version") != 1 or not str(spec.get("name", "")).strip():
        raise RuntimeError(f"invalid objective header: {path}")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", spec["name"]):
        raise RuntimeError(f"objective name must be path-safe: {path}")
    objective = spec.get("objective")
    if not isinstance(objective, dict):
        raise RuntimeError(f"objective must be an object: {path}")
    kind = objective.get("kind")
    if kind == "product":
        values = objective.get("exponents")
    elif kind == "weighted_normalized_sum":
        values = objective.get("weights")
    elif kind == "minimize_metric":
        if objective.get("metric") not in ("delay", "area", "power"):
            raise RuntimeError(f"invalid minimize_metric objective: {path}")
        values = None
    else:
        raise RuntimeError(
            f"unsupported public objective kind {kind!r}; use product, "
            "weighted_normalized_sum, or minimize_metric"
        )
    if values is not None:
        if set(values) != {"delay", "area", "power"}:
            raise RuntimeError(f"objective weights must name delay, area, and power: {path}")
        numbers = [float(values[key]) for key in ("delay", "area", "power")]
        if any(not math.isfinite(value) or value < 0 for value in numbers) or not any(numbers):
            raise RuntimeError(f"objective weights must be finite, nonnegative, and nonzero: {path}")
    constraints = spec.get("constraints", [])
    if not isinstance(constraints, list):
        raise RuntimeError(f"constraints must be a list: {path}")
    for item in constraints:
        if item.get("metric") not in ("delay", "area", "power"):
            raise RuntimeError(f"invalid constraint metric: {path}")
        if item.get("relation") not in ("at_most", "at_least"):
            raise RuntimeError(f"invalid constraint relation: {path}")
        bound = float(item.get("bound", 0))
        if not math.isfinite(bound) or bound <= 0:
            raise RuntimeError(f"constraint bound must be finite and positive: {path}")
    return {"path": path, "spec": spec, "sha256": sha256(path)}


def resolve_objectives(values: list[str] | None) -> list[dict]:
    aliases = manifest()["objective_aliases"]
    names = list(PAPER_OBJECTIVES) if not values else values
    resolved = []
    seen = set()
    for value in names:
        path = HERE / aliases[value] if value in aliases else Path(value)
        row = validate_objective(path)
        if row["sha256"] not in seen:
            row["label"] = value if value in aliases else row["spec"]["name"]
            resolved.append(row)
            seen.add(row["sha256"])
    return resolved


def ensure_library(path: Path) -> Path:
    path = path.resolve()
    if not path.is_file():
        raise RuntimeError(f"missing Liberty file: {path}")
    actual = sha256(path)
    if actual != EXPECTED_LIB_SHA256:
        raise RuntimeError(
            f"Liberty SHA mismatch; expected {EXPECTED_LIB_SHA256}, got {actual}"
        )
    return path


def ensure_binary(directory: Path) -> Path:
    binary = directory.resolve() / BINARY
    if not binary.is_file():
        raise RuntimeError(f"missing Iterative binary: {binary}")
    return binary


def clean_env(liberty: Path, binary_dir: Path, objective: Path, jobs: int) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("EGG_")}
    env.update(
        {
            "PYTHONDONTWRITEBYTECODE": "1",
            "E_SCOPE_LIBERTY": str(liberty),
            "E_SCOPE_LIBRARY_AUDIT": str(RUNTIME / "assets/library_audit.json"),
            "E_SCOPE_BIN_DIR": str(binary_dir),
            "EGG_CONQUER_BIN_DIR": str(binary_dir),
            "EGG_CARGO_OFFLINE": "1",
            "EGG_LIB_PATH": str(liberty),
            "EGG_SCALE_RULES": str(RUNTIME / "assets/6t_full_comb_scale_rules.json"),
            "EGG_RULE_PATHS": str(RUNTIME / "assets/runtime_full_comb_rules.json"),
            "EGG_ITERATIVE": "1",
            "EGG_UNIFIED_EQUIVALENCE_CANDIDATES": "1",
            "EGG_V8_ULTRA_CONFIG": str(RUNTIME / "config/iterative_search.json"),
            "EGG_FROZEN_RESOURCE_JOBS": str(jobs),
            "EGG_GENERATOR_JOBS": str(jobs),
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
            "EGG_OBJECTIVE_SPEC": str(objective),
        }
    )
    return env


def terminate_owned() -> None:
    for process in OWNED:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
    for process in OWNED:
        if process.poll() is None:
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()


def evidence(args) -> None:
    checks = []
    for point in manifest()["points"]:
        path = HERE / point["g0_path"]
        actual = sha256(path) if path.is_file() else None
        checks.append(
            {
                "point_id": point["point_id"],
                "anchor": point["anchor"],
                "expected": point["g0_sha256"],
                "actual": actual,
                "status": "PASS" if actual == point["g0_sha256"] else "FAIL",
            }
        )
    if len(checks) != 20 or len(anchors()) != 13 or any(row["status"] != "PASS" for row in checks):
        raise RuntimeError("G0 manifest verification failed")
    output = args.output.resolve()
    command = [sys.executable, str(REPO / "scripts/replay_figure7.py"), "--output-dir", str(output)]
    if args.no_plot:
        command.append("--no-plot")
    subprocess.run(command, cwd=REPO, check=True)
    replay = load(output / "replay.json")
    result = {
        "status": "PASS",
        "classification": "saved-evidence replay",
        "g0_hashes": checks,
        "g0_count": len(checks),
        "search_anchor_count": len(anchors()),
        "figure7_replay": replay,
        "fresh_optimization": "NOT_RUN",
    }
    if not result["g0_hashes"]:
        result["status"] = "FAIL"
    dump(output / "pareto_adder_evidence.json", result)
    print(json.dumps(result, indent=2))


def preflight(args) -> None:
    result = {
        "status": "PASS",
        "g0_hashes": all(sha256(HERE / p["g0_path"]) == p["g0_sha256"] for p in manifest()["points"]),
        "objectives": [
            {"label": row["label"], "name": row["spec"]["name"], "sha256": row["sha256"]}
            for row in resolve_objectives(args.objective)
        ],
    }
    try:
        result["liberty"] = str(ensure_library(args.liberty)) if args.liberty else "NOT_CHECKED"
        result["binary"] = str(ensure_binary(args.bin_dir)) if args.bin_dir else "NOT_CHECKED"
    except RuntimeError as error:
        result.update(status="FAIL", error=str(error))
    print(json.dumps(result, indent=2))
    if result["status"] != "PASS" or not result["g0_hashes"]:
        raise SystemExit(1)


def build(args) -> None:
    subprocess.run(
        [
            sys.executable,
            str(MAIN28 / "run.py"),
            "build",
            "--target-dir",
            str(args.target_dir.resolve()),
        ],
        cwd=REPO,
        check=True,
    )


def task_command(anchor: dict, objective: dict, output: Path, liberty: Path, binary_dir: Path, rounds: int, jobs: int, timeout: int) -> list[str]:
    return [
        sys.executable,
        str(HERE / "run.py"),
        "run-one",
        "--anchor", anchor["anchor"],
        "--objective", str(objective["path"]),
        "--output", str(output),
        "--liberty", str(liberty),
        "--bin-dir", str(binary_dir),
        "--rounds", str(rounds),
        "--jobs", str(jobs),
        "--timeout", str(timeout),
    ]


def plan(args) -> None:
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"refusing existing plan: {output}")
    liberty = args.liberty.resolve() if args.liberty else Path("/path/to/full_comb.lib")
    binary_dir = args.bin_dir.resolve() if args.bin_dir else Path("/path/to/escope-build/release")
    objectives = resolve_objectives(args.objective)
    tasks = []
    for objective in objectives:
        for anchor in anchors():
            child = Path("RUN_ROOT") / objective["label"] / anchor["anchor"]
            tasks.append(
                {
                    "anchor": anchor["anchor"],
                    "period_ps": anchor["period_ps"],
                    "g0_path": anchor["g0_path"],
                    "g0_sha256": anchor["g0_sha256"],
                    "objective": objective["label"],
                    "objective_name": objective["spec"]["name"],
                    "objective_sha256": objective["sha256"],
                    "round_cap": args.rounds,
                    "command": task_command(anchor, objective, child, liberty, binary_dir, args.rounds, args.jobs, args.timeout),
                    "external_validation": "required for a fresh output",
                }
            )
    dump(
        output,
        {
            "schema": "escope-pareto-adder-plan-v1",
            "method": "Iterative",
            "task_count": len(tasks),
            "trajectory_parallelism": 1,
            "tasks": tasks,
        },
    )
    print(output)


def run_one(args) -> None:
    anchor = anchor_for(args.anchor)
    objective = resolve_objectives([args.objective])[0]
    liberty = ensure_library(args.liberty)
    binary = ensure_binary(args.bin_dir)
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"refusing existing output directory: {output}")
    output.mkdir(parents=True)
    rounds = 1 if args.smoke else args.rounds
    jobs = 1 if args.smoke else args.jobs
    g0 = (HERE / anchor["g0_path"]).resolve()
    command = [
        str(binary), "epfl_adder", str(g0), str(output / "search"), "v3", str(rounds),
        str(liberty), str((RUNTIME / "assets/6t_full_comb_scale_rules.json").resolve()),
        str((RUNTIME / "assets/runtime_full_comb_rules.json").resolve()),
    ]
    record = {
        "schema": "escope-pareto-adder-run-v1",
        "method": "Iterative",
        "anchor": anchor["anchor"],
        "period_ps": anchor["period_ps"],
        "g0_sha256": anchor["g0_sha256"],
        "objective_name": objective["spec"]["name"],
        "objective_sha256": objective["sha256"],
        "objective_spec": objective["spec"],
        "round_cap": rounds,
        "resource_jobs": jobs,
        "command": command,
        "external_validation": "PENDING",
    }
    dump(output / "PLAN.json", record)
    log_handle = (output / "iterative.stdout.log").open("w")
    process = subprocess.Popen(
        command,
        cwd=RUNTIME,
        env=clean_env(liberty, args.bin_dir.resolve(), objective["path"], jobs),
        stdout=log_handle,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        text=True,
    )
    OWNED.append(process)
    dump(output / "status.json", {"status": "running", "pid": process.pid})
    started = time.time()
    try:
        returncode = process.wait(timeout=args.timeout)
    except subprocess.TimeoutExpired:
        terminate_owned()
        dump(output / "status.json", {"status": "timeout", "elapsed_sec": time.time() - started})
        raise RuntimeError(f"Iterative trajectory exceeded {args.timeout} seconds")
    finally:
        log_handle.close()
    summary_path = output / "search/summary.json"
    if returncode != 0 or not summary_path.is_file():
        dump(output / "status.json", {"status": "failed", "returncode": returncode})
        raise RuntimeError(f"Iterative failed with return code {returncode}; see {output / 'iterative.stdout.log'}")
    summary = load(summary_path)
    result = {
        "status": "search_complete",
        "method": "Iterative",
        "anchor": anchor["anchor"],
        "objective_name": objective["spec"]["name"],
        "rounds_executed": summary.get("rounds_executed"),
        "accepted_rounds": summary.get("accepted_rounds"),
        "incumbent_path": summary.get("incumbent_path"),
        "incumbent_ppa": summary.get("incumbent_ppa"),
        "elapsed_sec": time.time() - started,
        "external_validation": "PENDING",
    }
    dump(output / "status.json", result)
    print(json.dumps(result, indent=2))


def run_all(args) -> None:
    ensure_library(args.liberty)
    ensure_binary(args.bin_dir)
    objectives = resolve_objectives(args.objective)
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    queue_path = root / "queue_status.json"
    previous = load(queue_path) if queue_path.exists() else {}
    completed = set(previous.get("completed", []))
    failures = list(previous.get("failures", []))
    tasks = [(objective, anchor) for objective in objectives for anchor in anchors()]
    for index, (objective, anchor) in enumerate(tasks):
        key = f"{objective['label']}/{anchor['anchor']}"
        if key in completed:
            continue
        child = root / objective["label"] / anchor["anchor"]
        if child.exists():
            raise RuntimeError(f"partial output requires manual inspection: {child}")
        command = task_command(anchor, objective, child, args.liberty.resolve(), args.bin_dir.resolve(), args.rounds, args.jobs, args.timeout)
        dump(queue_path, {"status": "running", "current": key, "index": index, "completed": sorted(completed), "failures": failures, "command": command})
        result = subprocess.run(command, cwd=REPO)
        if result.returncode:
            failures.append({"task": key, "returncode": result.returncode})
            dump(queue_path, {"status": "failed", "current": key, "completed": sorted(completed), "failures": failures})
            if not args.continue_on_failure:
                raise SystemExit(result.returncode)
        else:
            completed.add(key)
            dump(queue_path, {"status": "running", "current": None, "completed": sorted(completed), "failures": failures})
    result = {"status": "search_complete", "completed": sorted(completed), "failures": failures, "external_validation": "PENDING"}
    dump(queue_path, result)
    print(json.dumps(result, indent=2))


def freeze_fresh(args) -> None:
    from pipeline import freeze
    objectives = [row["label"] for row in resolve_objectives(args.objective)]
    print(json.dumps(freeze(args.output, objectives, require_complete=True), indent=2))


def finalize_fresh(args) -> None:
    from pipeline import finalize
    objectives = [row["label"] for row in resolve_objectives(args.objective)]
    result = finalize(
        args.output, objectives, args.liberty, args.genus_bin, args.yosys_bin,
        args.abc_bin, args.genus_chunk, args.genus_timeout, args.formal_jobs,
        args.formal_timeout, args.yosys_datdir,
    )
    print(json.dumps(result, indent=2))


def reproduce(args) -> None:
    run_all(args)
    finalize_fresh(args)


def add_validation_arguments(p) -> None:
    p.add_argument("--genus-bin", type=Path, required=True)
    p.add_argument("--yosys-bin", type=Path, required=True)
    p.add_argument("--abc-bin", type=Path, required=True)
    p.add_argument("--yosys-datdir", type=Path)
    p.add_argument("--genus-chunk", type=int, default=60)
    p.add_argument("--genus-timeout", type=int, default=7200)
    p.add_argument("--formal-jobs", type=int, default=2)
    p.add_argument("--formal-timeout", type=int, default=1800)


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser(description=__doc__)
    sub = value.add_subparsers(dest="command", required=True)
    p = sub.add_parser("evidence", help="verify all 20 G0 hashes and replay Figure 7")
    p.add_argument("--output", type=Path, default=Path("reproduced/figure7"))
    p.add_argument("--no-plot", action="store_true")
    p.set_defaults(func=evidence)
    p = sub.add_parser("preflight", help="validate inputs, objectives, and optional runtime dependencies")
    p.add_argument("--objective", action="append")
    p.add_argument("--liberty", type=Path)
    p.add_argument("--bin-dir", type=Path)
    p.set_defaults(func=preflight)
    p = sub.add_parser("build", help="build the shared Iterative implementation")
    p.add_argument("--target-dir", type=Path, required=True)
    p.set_defaults(func=build)
    p = sub.add_parser("plan", help="write a complete plan without running search")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--objective", action="append")
    p.add_argument("--liberty", type=Path)
    p.add_argument("--bin-dir", type=Path)
    p.add_argument("--rounds", type=int, default=5)
    p.add_argument("--jobs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=7200)
    p.set_defaults(func=plan)
    p = sub.add_parser("run-one", help="run one direct Iterative trajectory")
    p.add_argument("--anchor", required=True)
    p.add_argument("--objective", required=True, help="da, d2ap, dp2, or an ObjectiveSpec JSON path")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--liberty", type=Path, required=True)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--rounds", type=int, default=5)
    p.add_argument("--jobs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=7200)
    p.add_argument("--smoke", action="store_true")
    p.set_defaults(func=run_one)
    p = sub.add_parser("run-all", help="run a resumable sequential queue over anchors and objectives")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--objective", action="append")
    p.add_argument("--liberty", type=Path, required=True)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--rounds", type=int, default=5)
    p.add_argument("--jobs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=7200)
    p.add_argument("--continue-on-failure", action="store_true")
    p.set_defaults(func=run_all)
    p = sub.add_parser("freeze", help="freeze the Internal DA/DP-front union before external evaluation")
    p.add_argument("--output", type=Path, required=True, help="completed fresh-search root")
    p.add_argument("--objective", action="append")
    p.set_defaults(func=freeze_fresh)
    p = sub.add_parser("finalize", help="freeze, run Genus/formal, and generate fresh Figure 7")
    p.add_argument("--output", type=Path, required=True, help="completed fresh-search root")
    p.add_argument("--objective", action="append")
    p.add_argument("--liberty", type=Path, required=True)
    add_validation_arguments(p)
    p.set_defaults(func=finalize_fresh)
    p = sub.add_parser("reproduce", help="run all searches, validate them, and generate fresh Figure 7")
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--objective", action="append")
    p.add_argument("--liberty", type=Path, required=True)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--rounds", type=int, default=5)
    p.add_argument("--jobs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=7200)
    p.add_argument("--continue-on-failure", action="store_true")
    add_validation_arguments(p)
    p.set_defaults(func=reproduce)
    return value


def main() -> None:
    args = parser().parse_args()
    for name in ("rounds", "jobs", "timeout", "genus_chunk", "genus_timeout", "formal_jobs", "formal_timeout"):
        if hasattr(args, name) and getattr(args, name) <= 0:
            raise SystemExit(f"--{name} must be positive")
    try:
        args.func(args)
    except KeyboardInterrupt:
        terminate_owned()
        raise SystemExit(130)


if __name__ == "__main__":
    main()

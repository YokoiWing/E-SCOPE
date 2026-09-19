#!/usr/bin/env python3
"""Run selected-method ablations and evaluate their PPA."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
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
LIB_SHA256 = "48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd"
BINARIES = ("run_generator_union_native", "run_frozen_a2_fast", "materialize_structural_macro_plan", "dump_mapped_nldm_v3_state")
MODES = ("phase1-only", "phase2-only", "no-pi-drive-expansion")
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


def points() -> list[dict]:
    return load(MANIFEST)["points"]


def point_for(benchmark: str) -> dict:
    rows = [row for row in points() if row["benchmark"] == benchmark]
    if len(rows) != 1:
        raise RuntimeError(f"unknown benchmark {benchmark!r}")
    return rows[0]


def g0_path(point: dict) -> Path:
    return (HERE / point["g0_path"]).resolve()


def ensure_runtime(liberty: Path, binary_dir: Path) -> tuple[Path, Path]:
    liberty, binary_dir = liberty.resolve(), binary_dir.resolve()
    if not liberty.is_file() or sha256(liberty) != LIB_SHA256:
        raise RuntimeError(f"the required Liberty must have SHA-256 {LIB_SHA256}")
    missing = [name for name in BINARIES if not (binary_dir / name).is_file()]
    if missing:
        raise RuntimeError(f"missing binaries: {', '.join(missing)}")
    return liberty, binary_dir


def preflight(args) -> None:
    rows = points()
    hashes_ok = all(g0_path(row).is_file() and sha256(g0_path(row)) == row["g0_sha256"]
                    for row in rows)
    configs_ok = all(
        row["selected_method"] == "Iterative" or all(
            (HERE / row[key]).is_file()
            for key in ("conquer_config", "conquer_phase2_policy")
        )
        for row in rows
    )
    result = {
        "status": "PASS" if hashes_ok and configs_ok else "FAIL",
        "points": len(rows),
        "g0_hashes": hashes_ok,
        "configs": configs_ok,
        "liberty": "NOT_CHECKED",
        "binaries": "NOT_CHECKED",
    }
    if args.liberty and args.bin_dir:
        try:
            liberty, binary_dir = ensure_runtime(args.liberty, args.bin_dir)
            result["liberty"] = str(liberty)
            result["binaries"] = str(binary_dir)
        except RuntimeError as error:
            result.update(status="FAIL", runtime_error=str(error))
    print(json.dumps(result, indent=2))
    if result["status"] != "PASS":
        raise SystemExit(1)


def base_env(liberty: Path, binary_dir: Path, jobs: int) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("EGG_")}
    env.update({
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
    })
    return env


def terminate_owned() -> None:
    for process in OWNED:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)


def task_kind(point: dict, mode: str) -> str:
    if point["selected_method"] == "Iterative":
        return "native_iterative_ablation"
    if mode == "phase1-only":
        return "conquer_pre_sizing_candidate_search"
    if mode == "phase2-only":
        return "native_fixed_g0_one_round"
    return "conquer_no_drive_search"


def cli_command(point: dict, mode: str, output: Path, liberty: Path, binary_dir: Path, jobs: int, timeout: int) -> list[str]:
    return [sys.executable, str(HERE / "run.py"), "run-one", "--benchmark", point["benchmark"], "--mode", mode,
            "--output", str(output), "--liberty", str(liberty), "--bin-dir", str(binary_dir),
            "--jobs", str(jobs), "--timeout", str(timeout)]


def plan(args) -> None:
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"refusing existing plan: {output}")
    liberty = args.liberty.resolve() if args.liberty else Path("/path/to/full_comb.lib")
    binary_dir = args.bin_dir.resolve() if args.bin_dir else Path("/path/to/escope-build/release")
    tasks = []
    for point in points():
        for mode in MODES:
            tasks.append({
                "benchmark": point["benchmark"], "anchor": point["anchor"],
                "selected_method": point["selected_method"], "mode": mode,
                "task_kind": task_kind(point, mode), "g0_sha256": point["g0_sha256"],
                "command": cli_command(point, mode, Path("RUN_ROOT") / point["benchmark"] / mode, liberty, binary_dir, args.jobs, args.timeout),
                "external_validation": "required",
            })
    counts = {name: sum(p["selected_method"] == name for p in points())
              for name in ("Iterative", "Conquer")}
    dump(output, {"schema": "escope-selected-method-ablation-plan-v1", "task_count": len(tasks),
                  "method_counts": counts, "tasks": tasks})
    print(output)


def run_native(point: dict, mode: str, output: Path, liberty: Path, binary_dir: Path, jobs: int, timeout: int) -> dict:
    env = base_env(liberty, binary_dir, jobs)
    env["EGG_V8_PHASE_ABLATION"] = mode
    if point["selected_method"] == "Conquer":
        if mode != "phase2-only":
            raise RuntimeError("internal routing error")
        env["EGG_V8_ULTRA_CONFIG"] = str((HERE / point["conquer_phase2_policy"]).resolve())
        rounds = 1
    else:
        rounds = int(point["round_cap"])
    search = output / "search"
    command = [str(binary_dir / "run_generator_union_native"), point["benchmark"], str(g0_path(point)), str(search), "v3", str(rounds),
               str(liberty), str(RUNTIME / "assets/6t_full_comb_scale_rules.json"), str(RUNTIME / "assets/runtime_full_comb_rules.json")]
    with (output / "search.stdout.log").open("w") as log:
        process = subprocess.Popen(command, cwd=RUNTIME, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        OWNED.append(process)
        try:
            returncode = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            terminate_owned()
            raise RuntimeError(f"search exceeded {timeout}s")
    if returncode or not (search / "summary.json").is_file():
        raise RuntimeError(f"search failed; see {output / 'search.stdout.log'}")
    summary = load(search / "summary.json")
    return {"status": "search_complete", "rounds_executed": summary.get("rounds_executed"),
            "incumbent_path": summary.get("incumbent_path"), "external_validation": "PENDING"}


def run_conquer(point: dict, mode: str, output: Path, liberty: Path, binary_dir: Path, timeout: int) -> dict:
    anchors = output / "anchor_manifest.json"
    dump(anchors, {point["benchmark"]: {point["anchor"]: str(g0_path(point))}})
    command = [sys.executable, str(RUNTIME / "scripts/run_conquer.py"), "--output", str(output / "search"),
               "--config", str((HERE / point["conquer_config"]).resolve()), "--cases", point["benchmark"],
               "--targets", point["anchor"], "--anchor-manifest", str(anchors), "--search-only"]
    if mode == "no-pi-drive-expansion":
        command.append("--no-phase1-drive-expansion")
    env = base_env(liberty, binary_dir, 2)
    with (output / "search.stdout.log").open("w") as log:
        process = subprocess.Popen(command, cwd=RUNTIME, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        OWNED.append(process)
        try:
            returncode = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            terminate_owned()
            raise RuntimeError(f"Conquer search exceeded {timeout}s")
    if returncode or not (output / "search/SUMMARY.json").is_file():
        raise RuntimeError(f"Conquer search failed; see {output / 'search.stdout.log'}")
    note = ("Externally evaluate the pre-sizing member paired with the selected full Conquer candidate."
            if mode == "phase1-only" else "Externally evaluate the frozen no-drive finalist pool.")
    return {"status": "candidate_pool_complete", "external_validation": "PENDING", "next_step": note}


def run_one(args) -> None:
    point = point_for(args.benchmark)
    liberty, binary_dir = ensure_runtime(args.liberty, args.bin_dir)
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"refusing existing output: {output}")
    output.mkdir(parents=True)
    dump(output / "PLAN.json", {"benchmark": point["benchmark"], "anchor": point["anchor"],
         "selected_method": point["selected_method"], "mode": args.mode,
         "task_kind": task_kind(point, args.mode), "g0_sha256": point["g0_sha256"]})
    if point["selected_method"] == "Iterative" or args.mode == "phase2-only":
        result = run_native(point, args.mode, output, liberty, binary_dir, args.jobs, args.timeout)
    else:
        result = run_conquer(point, args.mode, output, liberty, binary_dir, args.timeout)
    result.update(benchmark=point["benchmark"], anchor=point["anchor"], selected_method=point["selected_method"], mode=args.mode)
    dump(output / "status.json", result)
    print(json.dumps(result, indent=2))


def run_all(args) -> None:
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    tasks = [(point, mode) for point in points() for mode in MODES]
    completed, failures = [], []
    for ordinal, (point, mode) in enumerate(tasks, 1):
        output = root / point["benchmark"] / mode
        status_path = output / "status.json"
        if status_path.is_file() and load(status_path).get("status") in {
            "search_complete", "candidate_pool_complete"
        }:
            completed.append({"benchmark": point["benchmark"], "mode": mode, "resumed": True})
            continue
        if output.exists():
            failures.append({"benchmark": point["benchmark"], "mode": mode,
                             "error": f"incomplete existing output: {output}"})
            if not args.continue_on_failure:
                break
            continue
        command = cli_command(point, mode, output, args.liberty.resolve(),
                              args.bin_dir.resolve(), args.jobs, args.timeout)
        result = subprocess.run(command, cwd=REPO)
        if result.returncode:
            failures.append({"benchmark": point["benchmark"], "mode": mode,
                             "returncode": result.returncode})
            if not args.continue_on_failure:
                break
        else:
            completed.append({"benchmark": point["benchmark"], "mode": mode,
                              "resumed": False, "ordinal": ordinal})
        dump(root / "queue_status.json", {
            "schema": "escope-selected-method-ablation-queue-v1",
            "total": len(tasks), "completed": completed, "failures": failures,
        })
    result = {"status": "PASS" if len(completed) == len(tasks) and not failures else "INCOMPLETE",
              "total": len(tasks), "completed_count": len(completed), "failures": failures}
    dump(root / "queue_status.json", result)
    if result["status"] != "PASS":
        raise RuntimeError(f"ablation queue incomplete: {len(completed)}/{len(tasks)}")
    print(json.dumps(result, indent=2))


def finalize_results(args) -> None:
    from pipeline import finalize
    result = finalize(
        args.output, args.full_results, args.liberty, args.genus_bin,
        args.yosys_bin, args.abc_bin, args.genus_chunk, args.genus_timeout,
        args.formal_jobs, args.formal_timeout, args.yosys_datdir, args.benchmark,
    )
    print(json.dumps(result, indent=2))


def add_search_arguments(p) -> None:
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--liberty", type=Path, required=True)
    p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--jobs", type=int, default=2)
    p.add_argument("--timeout", type=int, default=43200)
    p.add_argument("--continue-on-failure", action="store_true")


def build(args) -> None:
    subprocess.run([sys.executable, str(MAIN28 / "run.py"), "build", "--target-dir", str(args.target_dir.resolve())], cwd=REPO, check=True)


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser(description=__doc__)
    sub = value.add_subparsers(dest="command", required=True)
    p = sub.add_parser("preflight", help="validate inputs and optional runtime dependencies"); p.add_argument("--liberty", type=Path); p.add_argument("--bin-dir", type=Path); p.set_defaults(func=preflight)
    p = sub.add_parser("build", help="build the shared implementation"); p.add_argument("--target-dir", type=Path, required=True); p.set_defaults(func=build)
    p = sub.add_parser("plan", help="write all 84 ablation tasks without running them"); p.add_argument("--output", type=Path, required=True); p.add_argument("--liberty", type=Path); p.add_argument("--bin-dir", type=Path); p.add_argument("--jobs", type=int, default=2); p.add_argument("--timeout", type=int, default=43200); p.set_defaults(func=plan)
    p = sub.add_parser("run-one", help="run one benchmark and ablation mode"); p.add_argument("--benchmark", required=True); p.add_argument("--mode", choices=MODES, required=True); p.add_argument("--output", type=Path, required=True); p.add_argument("--liberty", type=Path, required=True); p.add_argument("--bin-dir", type=Path, required=True); p.add_argument("--jobs", type=int, default=2); p.add_argument("--timeout", type=int, default=43200); p.set_defaults(func=run_one)
    p = sub.add_parser("run-all", help="run or resume all 84 selected-method ablations"); add_search_arguments(p); p.set_defaults(func=run_all)
    p = sub.add_parser("finalize", help="evaluate completed searches and export PPA and comparison CSVs")
    p.add_argument("--output", type=Path, required=True, help="ablation run-all output directory")
    p.add_argument("--full-results", type=Path, required=True, help="main28 run-all output, or one completed run-one directory")
    p.add_argument("--benchmark", action="append", help="evaluate only this benchmark; may be repeated")
    p.add_argument("--liberty", type=Path, required=True)
    p.add_argument("--genus-bin", type=Path, required=True)
    p.add_argument("--yosys-bin", type=Path, required=True)
    p.add_argument("--abc-bin", type=Path, required=True)
    p.add_argument("--yosys-datdir", type=Path)
    p.add_argument("--genus-chunk", type=int, default=60)
    p.add_argument("--genus-timeout", type=int, default=7200)
    p.add_argument("--formal-jobs", type=int, default=2)
    p.add_argument("--formal-timeout", type=int, default=1800)
    p.set_defaults(func=finalize_results)
    return value


def main() -> None:
    args = parser().parse_args()
    for name in ("jobs", "timeout", "genus_chunk", "genus_timeout", "formal_jobs", "formal_timeout"):
        if hasattr(args, name) and getattr(args, name) <= 0:
            raise SystemExit(f"--{name.replace('_', '-')} must be positive")
    try:
        args.func(args)
    except KeyboardInterrupt:
        terminate_owned()
        raise SystemExit(130)


if __name__ == "__main__":
    main()

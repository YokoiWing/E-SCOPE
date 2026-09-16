#!/usr/bin/env python3
"""Single entry point for the 28-point E-SCOPE main experiment."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
MANIFEST = HERE / "manifest.json"
RUNTIME = HERE / "runtime"
SOURCE_MANIFEST = HERE / "source/D1-series/Cargo.toml"
EXPECTED_LIB_SHA256 = "48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd"
BINARIES = (
    "run_generator_union_native",
    "run_frozen_a2_fast",
    "materialize_structural_macro_plan",
    "dump_mapped_nldm_v3_state",
)
OWNED: list[subprocess.Popen] = []


def load(path: Path):
    return json.loads(path.read_text())


def dump(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def sha(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def points() -> list[dict]:
    return load(MANIFEST)["points"]


def point_for(name: str) -> dict:
    matches = [point for point in points() if point["benchmark"] == name]
    if len(matches) != 1:
        raise RuntimeError(f"unknown benchmark {name!r}")
    return matches[0]


def rounds_for(point: dict) -> int:
    if point["benchmark"] == "epfl_mem_ctrl":
        return 2
    if point["benchmark"] == "epfl_hyp":
        return 1
    return 5


def objective_for(point: dict) -> Path:
    return RUNTIME / "config" / (
        "objective_da.json" if point["benchmark"] == "epfl_adder" else "objective_d2ap.json"
    )


def library_audit() -> Path:
    return RUNTIME / "assets/library_audit.json"


def ensure_library(path: Path) -> Path:
    path = path.resolve()
    if not path.is_file():
        raise RuntimeError(f"missing Liberty file: {path}")
    actual = sha(path)
    if actual != EXPECTED_LIB_SHA256:
        raise RuntimeError(
            "Liberty SHA mismatch; the main experiment requires "
            f"{EXPECTED_LIB_SHA256}, got {actual}"
        )
    return path


def ensure_binaries(path: Path) -> Path:
    path = path.resolve()
    missing = [name for name in BINARIES if not (path / name).is_file()]
    if missing:
        raise RuntimeError(f"missing binaries in {path}: {', '.join(missing)}")
    return path


def clean_env(liberty: Path, binary_dir: Path, objective: Path, jobs: int) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items() if not key.startswith("EGG_")}
    env.update(
        {
            "E_SCOPE_LIBERTY": str(liberty),
            "E_SCOPE_LIBRARY_AUDIT": str(library_audit()),
            "E_SCOPE_BIN_DIR": str(binary_dir),
            "PYTHONDONTWRITEBYTECODE": "1",
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
    env.pop("EGG_EXPERIMENTAL_OBJECTIVE", None)
    return env


def commands_for(point: dict, output: Path, liberty: Path, binary_dir: Path, smoke: bool) -> dict:
    g0 = (HERE / point["g0_path"]).resolve()
    rounds = 1 if smoke else rounds_for(point)
    objective = objective_for(point).resolve()
    anchors = output / "anchor_manifest.json"
    iterative_output = output / "iterative"
    conquer_output = output / "conquer"
    return {
        "iterative": [
            str(binary_dir / "run_generator_union_native"),
            point["benchmark"],
            str(g0),
            str(iterative_output),
            "v3",
            str(rounds),
            str(liberty),
            str((RUNTIME / "assets/6t_full_comb_scale_rules.json").resolve()),
            str((RUNTIME / "assets/runtime_full_comb_rules.json").resolve()),
        ],
        "conquer": [
            sys.executable,
            str((RUNTIME / "scripts/run_conquer_recall_v3.py").resolve()),
            "--output",
            str(conquer_output),
            "--config",
            str((RUNTIME / "config" / ("conquer_smoke.json" if smoke else "conquer.json")).resolve()),
            "--cases",
            point["benchmark"],
            "--targets",
            point["anchor"],
            "--anchor-manifest",
            str(anchors),
            "--search-only",
        ],
        "anchor_manifest": {point["benchmark"]: {point["anchor"]: str(g0)}},
        "objective": str(objective),
        "rounds": rounds,
    }


def terminate_owned() -> None:
    for process in OWNED:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    for process in OWNED:
        if process.poll() is None:
            try:
                process.wait(timeout=max(0.1, deadline - time.monotonic()))
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()


def launch(label: str, command: list[str], output: Path, env: dict[str, str]) -> subprocess.Popen:
    log = output / f"{label}.stdout.log"
    handle = log.open("w")
    process = subprocess.Popen(
        command,
        cwd=RUNTIME,
        env=env,
        stdout=handle,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        text=True,
    )
    process._escope_log_handle = handle  # type: ignore[attr-defined]
    OWNED.append(process)
    return process


def run_one(args) -> dict:
    point = point_for(args.benchmark)
    liberty = ensure_library(args.liberty)
    binary_dir = ensure_binaries(args.bin_dir)
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"refusing existing output directory: {output}")
    output.mkdir(parents=True)
    plan = commands_for(point, output, liberty, binary_dir, args.smoke)
    dump(output / "anchor_manifest.json", plan.pop("anchor_manifest"))
    env = clean_env(liberty, binary_dir, Path(plan["objective"]), 1 if args.smoke else args.jobs)
    plan_record = {
        "schema": "main28-single-run-v1",
        "benchmark": point["benchmark"],
        "anchor": point["anchor"],
        "g0_sha256": point["g0_sha256"],
        "paper_selected_method": point["paper_selected_method"],
        "mode": args.method,
        "smoke": args.smoke,
        "commands": {key: value for key, value in plan.items() if key in ("iterative", "conquer")},
        "objective": plan["objective"],
        "rounds": plan["rounds"],
        "external_selection": "PENDING; fresh searches do not read saved winners",
    }
    dump(output / "PLAN.json", plan_record)
    labels = ("iterative", "conquer") if args.method == "both" else (args.method,)
    started = time.time()
    processes = {label: launch(label, plan[label], output, env) for label in labels}
    dump(output / "status.json", {"status": "running", "pids": {k: p.pid for k, p in processes.items()}})
    deadline = time.monotonic() + args.timeout
    try:
        while any(process.poll() is None for process in processes.values()):
            if time.monotonic() >= deadline:
                raise TimeoutError(f"run exceeded {args.timeout}s")
            time.sleep(1)
        returncodes = {label: process.returncode for label, process in processes.items()}
        if any(returncodes.values()):
            raise RuntimeError(f"child failure: {returncodes}")
    except BaseException:
        terminate_owned()
        dump(output / "status.json", {"status": "failed", "elapsed_sec": time.time() - started})
        raise
    finally:
        for process in processes.values():
            process._escope_log_handle.close()  # type: ignore[attr-defined]
    result = {
        "status": "search_complete",
        "benchmark": point["benchmark"],
        "anchor": point["anchor"],
        "completed_methods": list(labels),
        "elapsed_concurrent_wall_sec": time.time() - started,
        "external_validation": "PENDING",
        "paper_result_reproduced": False,
        "next_step": "evaluate fresh checkpoints/finalists, then apply policy.py to measured features",
    }
    dump(output / "status.json", result)
    print(json.dumps(result, indent=2))
    return result


def evidence(args) -> None:
    commands = [
        [sys.executable, str(HERE / "verify.py")],
        [sys.executable, str(REPO / "scripts/replay_main28.py"), "--output-dir", str(args.output.resolve())],
    ]
    if args.no_plot:
        commands[1].append("--no-plot")
    for command in commands:
        subprocess.run(command, cwd=REPO, check=True)


def preflight(args) -> None:
    checks = {}
    for executable in ("python3", "cargo", "rustc"):
        checks[executable] = shutil.which(executable)
    checks["g0_hashes"] = all(sha(HERE / p["g0_path"]) == p["g0_sha256"] for p in points())
    checks["optimized_netlist_hashes"] = all(
        sha(HERE / p["optimized_path"]) == p["optimized_sha256"] for p in points()
    )
    if args.liberty:
        try:
            ensure_library(args.liberty)
            checks["liberty"] = {"status": "PASS", "sha256": EXPECTED_LIB_SHA256}
        except RuntimeError as error:
            checks["liberty"] = {"status": "FAIL", "error": str(error)}
    else:
        checks["liberty"] = {"status": "MISSING", "expected_sha256": EXPECTED_LIB_SHA256}
    if args.bin_dir:
        try:
            ensure_binaries(args.bin_dir)
            checks["binaries"] = {"status": "PASS", "directory": str(args.bin_dir.resolve())}
        except RuntimeError as error:
            checks["binaries"] = {"status": "FAIL", "error": str(error)}
    else:
        checks["binaries"] = {"status": "NOT_CHECKED", "build_command": "python3 experiments/main28/run.py build --target-dir /tmp/escope-main28-build"}
    failed = not checks["g0_hashes"] or not checks["optimized_netlist_hashes"]
    failed |= any(checks[x] is None for x in ("python3", "cargo", "rustc"))
    failed |= any(checks[key].get("status") == "FAIL" for key in ("liberty", "binaries"))
    result = {"status": "FAIL" if failed else "PASS", "checks": checks}
    print(json.dumps(result, indent=2))
    if failed:
        raise SystemExit(1)


def build(args) -> None:
    target = args.target_dir.resolve()
    command = ["cargo", "build", "--release", "--locked", "--manifest-path", str(SOURCE_MANIFEST), "--target-dir", str(target)]
    for binary in BINARIES:
        command += ["--bin", binary]
    subprocess.run(command, cwd=HERE / "source/D1-series", check=True)
    result = {"status": "PASS", "binary_dir": str(target / "release"), "binaries": {name: sha(target / "release" / name) for name in BINARIES}}
    print(json.dumps(result, indent=2))


def plan(args) -> None:
    output = args.output.resolve()
    if output.exists():
        raise RuntimeError(f"refusing existing plan: {output}")
    liberty = args.liberty.resolve() if args.liberty else Path("/path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib")
    binary_dir = args.bin_dir.resolve() if args.bin_dir else Path("/path/to/escope-main28-build/release")
    rows = []
    for point in points():
        base = Path("RUN_ROOT") / f"{point['benchmark']}__{point['anchor']}"
        commands = commands_for(point, base, liberty, binary_dir, False)
        rows.append({
            "benchmark": point["benchmark"], "anchor": point["anchor"],
            "g0": point["g0_path"], "g0_sha256": point["g0_sha256"],
            "objective": "DA" if point["benchmark"] == "epfl_adder" else "D2AP",
            "iterative_round_cap": commands["rounds"],
            "paper_selected_method": point["paper_selected_method"],
            "paper_output": point["optimized_path"],
            "paper_output_sha256": point["optimized_sha256"],
            "fresh_command": ["python3", "experiments/main28/run.py", "run-one", "--benchmark", point["benchmark"], "--method", "both", "--output", str(base), "--liberty", str(liberty), "--bin-dir", str(binary_dir)],
            "external_status": "required after fresh search",
        })
    dump(output, {"schema": "main28-execution-plan-v1", "points": rows, "count": len(rows), "automatic_execution": False})
    print(output)


def select(args) -> None:
    sys.path.insert(0, str(HERE))
    from policy import decide
    features = load(args.features)
    selected, clause = decide(features)
    print(json.dumps({"selected_method": selected, "clause": clause}, indent=2))


def run_all(args) -> None:
    liberty = ensure_library(args.liberty)
    binary_dir = ensure_binaries(args.bin_dir)
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    queue = root / "queue_status.json"
    previous = load(queue) if queue.exists() else {"completed": []}
    completed = set(previous.get("completed", []))
    failures = list(previous.get("failures", []))
    for index, point in enumerate(points()):
        name = point["benchmark"]
        if name in completed:
            continue
        stem = f"{name}__{point['anchor']}"
        child_output = root / stem
        attempt = 1
        while child_output.exists():
            attempt += 1
            child_output = root / f"{stem}__attempt_{attempt:02d}"
        command = [sys.executable, str(HERE / "run.py"), "run-one", "--benchmark", name,
                   "--method", "both", "--output", str(child_output), "--liberty", str(liberty),
                   "--bin-dir", str(binary_dir), "--jobs", str(args.jobs), "--timeout", str(args.point_timeout)]
        dump(queue, {"status": "running", "current": name, "index": index, "completed": sorted(completed), "failures": failures, "command": command})
        result = subprocess.run(command, cwd=REPO)
        if result.returncode:
            failures.append({"benchmark": name, "returncode": result.returncode})
            dump(queue, {"status": "failed", "current": name, "completed": sorted(completed), "failures": failures})
            if not args.continue_on_failure:
                raise SystemExit(result.returncode)
        else:
            completed.add(name)
    result = {"status": "search_complete", "completed": sorted(completed), "failures": failures, "external_validation": "PENDING"}
    dump(queue, result)
    print(json.dumps(result, indent=2))


def parser() -> argparse.ArgumentParser:
    value = argparse.ArgumentParser(description=__doc__)
    sub = value.add_subparsers(dest="command", required=True)
    p = sub.add_parser("evidence", help="verify inputs and recompute Table III/Figure 6")
    p.add_argument("--output", type=Path, default=Path("reproduced/main28")); p.add_argument("--no-plot", action="store_true"); p.set_defaults(func=evidence)
    p = sub.add_parser("preflight", help="check dependencies without running optimization")
    p.add_argument("--liberty", type=Path); p.add_argument("--bin-dir", type=Path); p.set_defaults(func=preflight)
    p = sub.add_parser("build", help="build the four required Rust binaries")
    p.add_argument("--target-dir", type=Path, required=True); p.set_defaults(func=build)
    p = sub.add_parser("plan", help="write the complete 28-point execution plan")
    p.add_argument("--output", type=Path, required=True); p.add_argument("--liberty", type=Path); p.add_argument("--bin-dir", type=Path); p.set_defaults(func=plan)
    p = sub.add_parser("run-one", help="run fresh Iterative, Conquer, or both from one packaged G0")
    p.add_argument("--benchmark", required=True); p.add_argument("--method", choices=("iterative", "conquer", "both"), default="both")
    p.add_argument("--output", type=Path, required=True); p.add_argument("--liberty", type=Path, required=True); p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--jobs", type=int, default=2); p.add_argument("--timeout", type=int, default=43200); p.add_argument("--smoke", action="store_true"); p.set_defaults(func=run_one)
    p = sub.add_parser("select", help="apply the frozen runtime policy to measured features")
    p.add_argument("--features", type=Path, required=True); p.set_defaults(func=select)
    p = sub.add_parser("run-all", help="run a resumable 28-point fresh-search queue")
    p.add_argument("--output", type=Path, required=True); p.add_argument("--liberty", type=Path, required=True); p.add_argument("--bin-dir", type=Path, required=True)
    p.add_argument("--jobs", type=int, default=2); p.add_argument("--point-timeout", type=int, default=43200); p.add_argument("--continue-on-failure", action="store_true"); p.set_defaults(func=run_all)
    return value


def main() -> None:
    args = parser().parse_args()
    try:
        args.func(args)
    except KeyboardInterrupt:
        terminate_owned()
        raise SystemExit(130)


if __name__ == "__main__":
    main()

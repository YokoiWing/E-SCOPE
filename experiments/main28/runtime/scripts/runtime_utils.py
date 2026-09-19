#!/usr/bin/env python3
"""Pin five proved sin incumbents, then test bounded complete t1 extensions.

Development-only: historical Genus-selected incumbents are mandatory guards.
No external result is used to modify this run's frozen finalist set.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

from fusion_moves import compatible, emit, member_rows
from incumbent_guard import select_verified_incumbent

D1 = Path(__file__).resolve().parents[1]
RUNS = D1 / "runs"
LIB = D1 / "test/asap7sc6t_full_comb/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib"
SOURCE = D1 / "third_party/epfl_benchmarks/arithmetic/sin.v"


def load(path):
    return json.loads(Path(path).read_text())


def dump(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def signature(plan):
    return json.dumps(sorted(plan["choices"], key=lambda c: c["root_anchor"]), sort_keys=True)


def process_tree_rss(pid):
    raw = subprocess.check_output(["ps", "-eo", "pid=,ppid=,rss="], text=True)
    rows = [list(map(int, line.split())) for line in raw.splitlines()]
    children = {pid}
    while True:
        expanded = children | {p for p, parent, _ in rows if parent in children}
        if expanded == children:
            return sum(rss for p, _, rss in rows if p in children)
        children = expanded


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    (out / "pid.txt").write_text(str(os.getpid()) + "\n")
    start = time.monotonic()
    stages = []
    env = os.environ.copy()
    env.update({
        "EGG_LIB_PATH": str(LIB), "EGG_SCALE_RULES": str(D1 / "test/6t_scale_rules.json"),
        "EGG_EXPERIMENTAL_FULL_BOOLEAN_PROOF": "1", "EGG_REALIZATION_JOBS": "4",
        "EGG_PROFILE_BATCH_JOBS": "4", "EGG_PI_ARRIVAL_PS": "0", "EGG_PI_SLEW_PS": "20",
        "EGG_PO_LOAD_FF": "5.76", "EGG_INTERNAL_TIMING_MODEL": "driver-aware-v3",
        "EGG_DRIVER_INPUT_SLEW_PS": "0", "EGG_POWER_MODE": "vectorless",
        "EGG_POWER_ACTIVITY_MODEL": "uniform", "EGG_POWER_PERIOD_PS": "1000",
        "EGG_POWER_VOLTAGE_V": "0.7", "EGG_PI_TOGGLE_PER_CYCLE": "0.1",
        "EGG_INTERNAL_TRANSITION_FACTOR": "1.78",
    })

    def run(stage, command, timeout=600):
        print(f"START {stage}", flush=True)
        stage_start = time.monotonic()
        peak = 0
        with (out / f"{stage}.stdout.log").open("w") as log:
            proc = subprocess.Popen(list(map(str, command)), cwd=D1, env=env,
                                    stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            while proc.poll() is None:
                peak = max(peak, process_tree_rss(proc.pid))
                state = {"stage": stage, "status": "running", "driver_pid": os.getpid(),
                         "child_pid": proc.pid, "sampled_tree_peak_rss_kib": peak,
                         "elapsed_sec": time.monotonic() - stage_start, "completed_stages": stages}
                dump(out / "status.json", state)
                if state["elapsed_sec"] > timeout or peak > 8 * 1024 * 1024:
                    os.killpg(proc.pid, signal.SIGTERM)
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
                    dump(out / "status.json", {**state, "status": "resource_stopped"})
                    raise RuntimeError(f"{stage}: timeout/RSS stop")
                time.sleep(2)
        receipt = {"stage": stage, "command": list(map(str, command)), "returncode": proc.returncode,
                   "wall_sec": time.monotonic() - stage_start, "sampled_tree_peak_rss_kib": peak}
        stages.append(receipt)
        dump(out / "stage_receipts.json", stages)
        if proc.returncode:
            dump(out / "status.json", {"stage": stage, "status": "failed", "completed_stages": stages})
            raise RuntimeError(f"{stage} failed; see stdout log")
        print(f"DONE {stage} {receipt['wall_sec']:.2f}s RSS={peak}KiB", flush=True)

    historical = RUNS / "iterative_sin_t1_t4_fusion_screen_v1"
    formal_rows = load(historical / "final_formal/validation_status.json")["results"]
    t0base = RUNS / "iterative_sin_t0_qor_dev/sin_supergate_fusion_v1"
    t0formal = load(t0base / "final_genus_candidate/formal/validation_status.json")["results"][0]
    pinned = []
    for index, target in enumerate(("t0_0.95x", "t1_1.00x", "t2_1.05x", "t3_1.10x", "t4_1.20x")):
        form = t0formal if index == 0 else next(row for row in formal_rows if row["target"].startswith(target))
        status = t0base / "genus_local15/external_genus/external_status.json" if index == 0 else historical / target / "closure_external_genus/external_status.json"
        rows = load(status)["results"]
        base = next(row for row in rows if row["round"] == 0)
        witness = next(row for row in rows if row["sha256"] == form["target_sha256"])
        original = Path(form["target_netlist"])
        assert sha(original) == form["target_sha256"]
        path = out / "pinned" / f"{target}.v"
        path.parent.mkdir(exist_ok=True)
        shutil.copy2(original, path)
        assert sha(path) == form["target_sha256"]
        pinned.append({"target": target, "netlist": str(path), "sha256": sha(path),
                       "original": str(original), "g0": base["netlist"], "g0_sha256": sha(base["netlist"]),
                       "historical_d2ap_over_g0": witness["external"]["d2ap"] / base["external"]["d2ap"],
                       "historical_formal": form, "external_source": str(status)})
    dump(out / "pinned_manifest.json", {"policy": "mandatory immutable complete incumbents", "anchors": pinned})

    # One frozen source-G0 family; every proposal is a complete simultaneous closure.
    # No accepting a move or reusing timing from an updated parent during generation.
    base = historical / "t1_1.00x"
    primitives = member_rows(base / "generic_single_cell/candidate_plan.json", base / "generic_single_cell/inventory.json")
    seed = load(base / "genus_guided_closures/candidate_plan.json")[0]
    members = [next(row for row in primitives if row["plan"]["choices"][0] == choice) for choice in seed["choices"]]
    assert compatible(members)
    seed_roots = {row["root"] for row in members}
    proposals, seen = [], {signature(seed)}

    def propose(kind, selected, root, primitive):
        if not compatible(selected):
            return
        plan = emit(f"EXT_{len(proposals):04d}_{kind}_P{root}", selected,
                    ["historical-incumbent-preserving-development", f"move:{kind}", f"primitive:{primitive}"])
        plan.pop("_members")
        key = signature(plan)
        if key in seen:
            return
        seen.add(key)
        proposals.append(plan)

    for row in primitives:
        if row["root"] not in seed_roots:
            propose("ADD", members + [row], row["root"], row["candidate_id"])
        else:
            propose("SWAP", [m for m in members if m["root"] != row["root"]] + [row], row["root"], row["candidate_id"])
    # Bound is explicit and checked, not silently truncated by an Internal root ranking.
    assert len(proposals) <= 600
    dump(out / "extension_plan.json", proposals)
    dump(out / "generation_summary.json", {"primitive_count": len(primitives), "seed_roots": sorted(seed_roots),
         "effective_closures": len(proposals), "moves": {k: sum(f"_{k}_" in p["candidate_id"] for p in proposals) for k in ("ADD", "SWAP")},
         "new_external_feedback": False, "historical_genus_seed": True, "a2_exact": 0, "rounds": 1})
    print(f"PINNED 5 guards; GENERATED {len(proposals)} complete t1 closures", flush=True)
    run("materialize", [D1 / "target/release/materialize_structural_macro_plan", pinned[1]["g0"], out / "extension_plan.json", out / "materialized"])
    tasks = [{"candidate_id": "G0", "input_netlist": pinned[1]["g0"]},
             {"candidate_id": "PINNED", "input_netlist": pinned[1]["netlist"]}]
    tasks += [{"candidate_id": path.stem, "input_netlist": str(path)} for path in sorted((out / "materialized/candidates").glob("*.v"))]
    dump(out / "profile_manifest.json", tasks)
    run("profile", [D1 / "target/release/run_frozen_a2_fast", "profile-batch", out / "profile_manifest.json", out / "profiles.json"])
    profiles = load(out / "profiles.json")["profiles"]
    metrics = {row["candidate_id"]: row for row in profiles}
    available = [plan for plan in proposals if plan["candidate_id"] in metrics]
    selected = []
    def retain(plan):
        if plan not in selected and len(selected) < 6:
            selected.append(plan)
    # Different complete combinations, rather than only neighbors of one scalar leader.
    for kind in ("SWAP", "ADD"):
        pool = [p for p in available if f"_{kind}_" in p["candidate_id"]]
        if pool:
            for axis in ("d2ap", "delay_ps"):
                retain(min(pool, key=lambda p: (metrics[p["candidate_id"]][axis], p["candidate_id"])))
    for axis in ("area", "power", "d2ap", "delay_ps"):
        retain(min(available, key=lambda p: (metrics[p["candidate_id"]][axis], p["candidate_id"])))
    for plan in sorted(available, key=lambda p: (metrics[p["candidate_id"]]["d2ap"], p["candidate_id"])):
        retain(plan)
    assert len(selected) == 6
    finalists = []
    for guard in pinned:
        checkpoints = [{"round": 0, "netlist": guard["g0"]}, {"round": 1, "netlist": guard["netlist"]}]
        finalists.append({"target": guard["target"], "round": 1, "candidate_id": "PINNED", "netlist": guard["netlist"], "sha256": guard["sha256"]})
        if guard["target"] == "t1_1.00x":
            for ordinal, plan in enumerate(selected, 2):
                path = out / "materialized/candidates" / f"{plan['candidate_id']}.v"
                checkpoints.append({"round": ordinal, "netlist": str(path)})
                finalists.append({"target": guard["target"], "round": ordinal, "candidate_id": plan["candidate_id"], "netlist": str(path), "sha256": sha(path)})
        dump(out / "external_input/epfl_sin" / guard["target"] / "paper_status.json", {"benchmark": "epfl_sin", "target": guard["target"], "checkpoints": checkpoints})
    dump(out / "frozen_finalists.json", {"frozen_unix": time.time(), "finalists": finalists,
         "max_points_per_anchor_including_g0": 8, "post_external_adaptation": False})
    for row in finalists:
        folder = out / "formal_candidates"
        folder.mkdir(exist_ok=True)
        shutil.copy2(row["netlist"], folder / f"{row['target']}_{row['candidate_id']}.v")
    run("external", ["python3", D1 / "scripts/run_paper_external_grid.py", "--v8-root", out / "external_input", "--output-root", out / "external_genus", "--session-root", out / "external_sessions", "--timeout", "600"])
    run("formal", ["python3", D1 / "scripts/validate_mapped_candidate_directory.py", "--source", SOURCE, "--source-top", "top", "--candidate-dir", out / "formal_candidates", "--output-root", out / "equivalence", "--benchmark", "epfl_sin", "--jobs", "4", "--timeout", "300"], timeout=900)
    external = load(out / "external_genus/external_status.json")
    formal = load(out / "equivalence/validation_status.json")
    assert not external["failures"] and formal["failure_count"] == 0
    results = []
    for guard in pinned:
        for path, expected in ((guard["netlist"], guard["sha256"]), (guard["original"], guard["sha256"]), (guard["g0"], guard["g0_sha256"])):
            assert sha(path) == expected
        rows = [row for row in external["results"] if row["target"] == guard["target"]]
        g0 = next(row for row in rows if row["round"] == 0)
        old = next(row for row in rows if row["round"] == 1)
        assert old["sha256"] == guard["sha256"]
        eligible = [row for row in rows if row["round"] != 0]
        for row in eligible:
            fixed = next(x for x in finalists if x["target"] == row["target"] and x["round"] == row["round"])
            assert sha(row["netlist"]) == row["sha256"] == fixed["sha256"]
            validation = next(x for x in formal["results"] if x["target_sha256"] == row["sha256"])
            assert all(validation[k] == "PASS" for k in ("mapped_only_legality", "full186_only", "write_reparse_cold_read", "formal_equivalence"))
        winner = select_verified_incumbent(old, eligible,
            {row["sha256"] for row in finalists if row["target"] == guard["target"]}, formal["results"])
        assert winner["external"]["d2ap"] <= old["external"]["d2ap"]
        final_path = out / "final" / guard["target"] / "mapped.v"
        final_path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(winner["netlist"], final_path)
        results.append({"target": guard["target"], "historical_ratio": guard["historical_d2ap_over_g0"],
                        "replayed_guard_ratio": old["external"]["d2ap"] / g0["external"]["d2ap"],
                        "winner_ratio": winner["external"]["d2ap"] / g0["external"]["d2ap"],
                        "winner_over_guard": winner["external"]["d2ap"] / old["external"]["d2ap"],
                        "winner_round": winner["round"], "external": winner["external"],
                        "sha256": sha(final_path), "final_netlist": str(final_path), "formal_clean": True})
    summary = {"stage": "complete", "scope": "historical-incumbent preservation plus bounded t1 development; not unbiased discovery",
               "results": results, "new_genus_points": len(external["results"]), "formal_candidates": formal["candidate_count"],
               "internal_profiles": len(profiles), "local_materialization": load(out / "materialized/summary.json"),
               "internal_extensions_better_than_pinned": sum(row["d2ap"] < metrics["PINNED"]["d2ap"] for row in profiles if row["candidate_id"].startswith("EXT_")),
               "wall_sec": time.monotonic() - start, "sampled_peak_tree_rss_kib": max(stage["sampled_tree_peak_rss_kib"] for stage in stages),
               "resource_sampling_interval_sec": 2, "stages": stages}
    dump(out / "SUMMARY.json", summary)
    dump(out / "status.json", {"stage": "complete", "driver_pid": os.getpid(), "summary": str(out / "SUMMARY.json")})
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    main()

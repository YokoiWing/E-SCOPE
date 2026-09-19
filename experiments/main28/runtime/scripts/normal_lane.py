#!/usr/bin/env python3
"""Autonomous G0-only defense trial; no historical winner or external search input."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

from fusion_moves import member_rows
from closure_policy import select_sources, endpoint_sets, pair_moves, closure_moves, make_plan, finalists
from boundary_bypass import generate
from runtime_utils import dump, load, sha, process_tree_rss
from search_environment import frozen_env, g0_path, ANCHORS, LIB, SCALE, RULES
from semantic_shadow import load_cell_functions
from incumbent_guard import select_verified_incumbent
from compact_iterative_results import preserve_leader

D1 = Path(__file__).resolve().parents[1]


def available_memory_kib():
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1])
    raise RuntimeError("MemAvailable missing; refuse unmonitored run")


def normal_policy(cfg, base, gates):
    result = dict(base)
    if gates > cfg.get("large_graph_threshold", math.inf):
        for key in ("active_instance_cap", "stage25_active_instance_cap", "stage50_active_instance_cap", "stage500_active_instance_cap"):
            result[key] = cfg["large_active_cap"]
        for key in ("pre_materialization_candidate_cap", "very_large_pre_materialization_candidate_cap"):
            result[key] = cfg["large_generator_cap"]
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--config", type=Path, default=D1 / "config/v8_conquer_defense_v1.json")
    ap.add_argument("--cases", default="epfl_sin,epfl_multiplier")
    ap.add_argument("--targets", default="")
    ap.add_argument("--resume", action="store_true", help="Resume completed stages after a pre-external orchestration repair")
    ap.add_argument("--allow-pre-external-resource-revision", action="store_true")
    args = ap.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    assert args.resume or not (root / "pid.txt").exists(), "refuse existing run"
    started = (root / "pid.txt").stat().st_mtime if args.resume else time.time()
    (root / ("resume_pid.txt" if args.resume else "pid.txt")).write_text(str(os.getpid()) + "\n")
    cfg = load(args.config)
    promotion_v2 = cfg.get("promotion_policy") == "whole-port-dominance-v2"
    if promotion_v2:
        assert not args.resume, "V2 trial requires a fresh run, no stage-cache reuse"
        assert cfg["search_parent"] == "G0_only" and not cfg["historical_winner_input"]
        assert not cfg["external_feedback_to_search"] and cfg["fallback"] == "G0_only"
    if cfg.get("clean_egg_environment"):
        for key in list(os.environ):
            if key.startswith("EGG_"):
                del os.environ[key]
    policy_receipt = {"configuration": cfg, "config_sha256": sha(args.config),
        "script_sha256": sha(Path(__file__)), "policy_sha256": sha(D1 / "scripts/closure_policy.py"),
        "frozen_unix": started, "historical_winner_inputs": [], "external_feedback_to_search": False}
    if promotion_v2:
        policy_receipt["promotion_policy_sha256"] = sha(D1 / "scripts/finalist_policy.py")
    if args.resume:
        assert not (root / "frozen_finalists.json").exists(), "resume repair only before first external freeze"
        previous = load(root / "frozen_policy.json")["configuration"]
        changed = {k for k in previous.keys() | cfg.keys() if previous.get(k) != cfg.get(k)}
        assert not changed or (args.allow_pre_external_resource_revision and changed <= {"large_graph_threshold", "large_active_cap", "large_generator_cap", "rss_limit_kib", "stage_timeout_sec", "minimum_available_memory_kib"})
        event = root / "resume_events" / str(time.time_ns())
        dump(event / "policy.json", policy_receipt)
        interrupted = load(root / "status.json")
        interrupted["exit_code"] = 1 if interrupted["status"] == "resource_stopped" else 130
        dump(event / "previous_status.json", interrupted)
        if (root / "progress_summary.json").exists():
            shutil.copy2(root / "progress_summary.json", event / "previous_progress_summary.json")
        if (root / "external_input").exists():
            shutil.copytree(root / "external_input", event / "previous_external_input")
    else:
        dump(root / "frozen_policy.json", policy_receipt)
    env = frozen_env(cfg["profile_jobs"])
    env.pop("EGG_OBJECTIVE_ENGINE", None)
    env.update({"EGG_EXPERIMENTAL_FULL_BOOLEAN_PROOF": "1", "EGG_REALIZATION_JOBS": "2", "EGG_PROFILE_BATCH_JOBS": "2"})
    if promotion_v2:
        env["EGG_PROFILE_BOUNDARY_RESPONSE"] = "1"
    base_ultra = load(Path(env["EGG_V8_ULTRA_CONFIG"]))
    dump(root / "active_policy.json", policy_receipt)
    dump(root / "environment.json", {k: v for k, v in env.items() if k.startswith("EGG_")})
    if promotion_v2:
        inputs = [LIB, SCALE, RULES, LIB.parent / "audit.json", Path(env["EGG_V8_ULTRA_CONFIG"])]
        inputs += [g0_path(b, t) for b in args.cases.split(",") for t in ANCHORS[b]
                   if not args.targets or t in args.targets.split(",")]
        inputs += [D1 / "target/release" / b for b in ("dump_mapped_nldm_v3_state", "run_generator_union_native",
                   "materialize_structural_macro_plan", "run_frozen_a2_fast")]
        inputs += list((D1 / "scripts").glob("*.py"))
        dump(root / "input_fingerprint.json", [{"path": str(p.resolve()), "sha256": sha(p)} for p in inputs])
    stages, progress, frozen = load(root / "stage_receipts.json") if args.resume else [], [], []
    completed = {s["stage"] for s in stages if s["returncode"] == 0}
    if args.resume:
        stages.append({"stage": interrupted["stage"]+"_interrupted", "wall_sec": interrupted["elapsed_sec"],
            "sampled_tree_peak_rss_kib": interrupted.get("sampled_tree_peak_rss_kib", interrupted.get("peak_rss_kib")),
            "returncode": interrupted["exit_code"], "wall_is_last_heartbeat_approximation": True})
        dump(root / "stage_receipts.json", stages)

    def run(stage, command):
        if stage in completed:
            print("REUSE", stage, flush=True)
            return
        if args.resume and (root / f"{stage}.stdout.log").exists():
            shutil.move(root / f"{stage}.stdout.log", event / f"{stage}.interrupted.stdout.log")
            if Path(command[0]).name == "run_generator_union_native" and Path(command[3]).exists():
                shutil.move(command[3], event / (stage+"_partial_normal"))
        begin, peak = time.monotonic(), 0
        assert available_memory_kib() >= cfg.get("minimum_available_memory_kib", 0), "insufficient host memory headroom"
        print("START", stage, flush=True)
        with (root / f"{stage}.stdout.log").open("w") as log:
            proc = subprocess.Popen(list(map(str, command)), cwd=D1, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            while proc.poll() is None:
                peak = max(peak, process_tree_rss(proc.pid))
                available = available_memory_kib()
                dump(root / "status.json", {"stage": stage, "status": "running", "driver_pid": os.getpid(), "child_pid": proc.pid,
                    "elapsed_sec": time.monotonic()-begin, "sampled_tree_peak_rss_kib": peak, "host_available_memory_kib": available, "completed_anchors": len(progress)})
                if time.monotonic()-begin > cfg["stage_timeout_sec"] or peak > cfg["rss_limit_kib"] or available < cfg.get("minimum_available_memory_kib", 0):
                    os.killpg(proc.pid, signal.SIGTERM)
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
                    dump(root / "status.json", {"stage": stage, "status": "resource_stopped", "elapsed_sec": time.monotonic()-begin, "peak_rss_kib": peak, "host_available_memory_kib": available})
                    raise RuntimeError("resource stop " + stage)
                time.sleep(2)
        stages.append({"stage": stage, "wall_sec": time.monotonic()-begin, "sampled_tree_peak_rss_kib": peak,
            "command": list(map(str, command)), "returncode": proc.returncode})
        dump(root / "stage_receipts.json", stages)
        if proc.returncode:
            dump(root / "status.json", {"stage": stage, "status": "failed"})
            raise RuntimeError(stage + " failed")
        print("DONE", stage, round(stages[-1]["wall_sec"], 2), flush=True)

    functions = load_cell_functions(LIB.parent / "audit.json")
    for benchmark in args.cases.split(","):
        for target in ANCHORS[benchmark]:
            if args.targets and target not in args.targets.split(","):
                continue
            point = root / benchmark / target
            point.mkdir(parents=True, exist_ok=args.resume)
            tag = benchmark + "_" + target
            g0 = g0_path(benchmark, target)
            run(tag+"_state", [D1 / "target/release/dump_mapped_nldm_v3_state", g0, point / "g0_state.json"])
            state = load(point / "g0_state.json")
            gates = sum(not n["is_leaf"] and not n["is_root"] for n in state["occurrences"])
            dump(point / "normal_resource_policy.json", normal_policy(cfg, base_ultra, gates))
            env["EGG_V8_ULTRA_CONFIG"] = str(point / "normal_resource_policy.json")
            run(tag+"_normal", [D1 / "target/release/run_generator_union_native", benchmark, g0, point / "normal", "v3", cfg["normal_v8_rounds"], LIB, SCALE, RULES])
            normal = load(point / "normal/summary.json")
            assert preserve_leader(point / "normal/round_00", True), "normal pre-A2 leader unavailable"
            run(tag+"_mine", ["python3", D1 / "scripts/mine_single_cell_fusions.py", "--graph", point / "g0_state.json", "--timing-state", point / "g0_state.json",
                "--audit", LIB.parent / "audit.json", "--liberty", LIB, "--out-dir", point / "mined", "--max-gates", cfg["mining_max_gates"],
                "--max-boundary", cfg["mining_max_boundary"], "--variants-per-cone", cfg["mining_variants_per_cone"], "--max-candidates", cfg["mining_cap"]])
            selected = select_sources(load(point / "mined/candidate_plan.json"), load(point / "mined/inventory.json"), cfg["fusion_roots"], cfg["states_per_root"])
            plans, inventory = [p for p, _ in selected], [i for _, i in selected]
            for plan in generate(state, functions):
                plans.append(plan)
                inventory.append({"region": plan["proof_outputs"]})
            dump(point / "primitive_plan.json", plans)
            dump(point / "primitive_inventory.json", inventory)
            members = member_rows(point / "primitive_plan.json", point / "primitive_inventory.json")
            paths = {"G0": g0, "NORMAL_POST": Path(normal["incumbent_path"])}
            pre = point / "normal/round_00/leader_pre_a2.v"
            if pre.exists():
                paths["NORMAL_PRE"] = pre
            profiles = []
            membership = {m["candidate_id"]: [m] for m in members}

            def materialize_profile(label, batch):
                nonlocal profiles
                if batch:
                    dump(point / f"{label}_plan.json", batch)
                    run(tag+"_"+label+"_materialize", [D1 / "target/release/materialize_structural_macro_plan", g0, point / f"{label}_plan.json", point / label])
                    receipt = load(point / label / "summary.json")
                    assert receipt["legal"] == len(batch)
                    files = list((point / label / "candidates").glob("*.v"))
                    paths.update({p.stem: p for p in files})
                ids = list(paths) if label == "single" else [p["candidate_id"] for p in batch]
                if not ids:
                    return
                dump(point / f"{label}_profile_manifest.json", [{"candidate_id": cid, "input_netlist": str(paths[cid])} for cid in ids])
                run(tag+"_"+label+"_profile", [D1 / "target/release/run_frozen_a2_fast", "profile-batch", point / f"{label}_profile_manifest.json", point / f"{label}_profiles.json"])
                profiles += load(point / f"{label}_profiles.json")["profiles"]

            materialize_profile("single", plans)
            if "NORMAL_PRE" in paths and not any(p["candidate_id"] == "NORMAL_PRE" for p in profiles):
                dump(point / "normal_pre_profile_manifest.json", [{"candidate_id": "NORMAL_PRE", "input_netlist": str(paths["NORMAL_PRE"])}])
                run(tag+"_normal_pre_profile", [D1 / "target/release/run_frozen_a2_fast", "profile-batch", point / "normal_pre_profile_manifest.json", point / "normal_pre_profiles.json"])
                profiles += load(point / "normal_pre_profiles.json")["profiles"]
            endpoints = endpoint_sets(state)
            pairs = pair_moves(members, profiles, endpoints, cfg["pair_cap"])
            pair_members = {f"PAIR_{i:03d}": rows for i, rows in enumerate(pairs)}
            membership.update(pair_members)
            materialize_profile("pair", [make_plan(cid, rows) for cid, rows in pair_members.items()])
            closures = closure_moves(members, pair_members, profiles, cfg["closure_cap"], cfg["max_closure_sites"])
            closure_members = {f"CLOSURE_{i:03d}": rows for i, rows in enumerate(closures)}
            membership.update(closure_members)
            materialize_profile("closure", [make_plan(cid, rows) for cid, rows in closure_members.items()])
            ppa = {p["candidate_id"]: p for p in profiles}
            interaction = []
            for cid, rows in pair_members.items():
                expected = math.prod(ppa[m["candidate_id"]]["d2ap"] / ppa["G0"]["d2ap"] for m in rows)
                actual = ppa[cid]["d2ap"] / ppa["G0"]["d2ap"]
                interaction.append({"candidate_id": cid, "members": [m["candidate_id"] for m in rows],
                    "endpoint_overlap": sorted(endpoints[rows[0]["root"]] & endpoints[rows[1]["root"]]),
                    "singleton_product": expected, "measured_pair": actual, "interaction_log_residual": math.log(actual/expected)})
            dump(point / "interaction_audit.json", interaction)
            dump(point / "profiles.json", {"profiles": profiles})
            if promotion_v2:
                from finalist_policy import finalists as guarded_finalists
                chosen, promotion = guarded_finalists(profiles, paths, sha, membership, cfg["external_candidates_excluding_g0"])
                dump(point / "promotion_audit.json", promotion)
            else:
                chosen = finalists(profiles, paths, sha, cfg["external_candidates_excluding_g0"])
            cps = []
            for ordinal, row in enumerate([ppa["G0"]] + chosen):
                cid = row["candidate_id"]
                cps.append({"round": ordinal, "netlist": str(paths[cid])})
                frozen.append({"benchmark": benchmark, "target": target, "round": ordinal, "candidate_id": cid,
                    "netlist": str(paths[cid]), "sha256": sha(paths[cid]), "internal": row,
                    "members": [m["candidate_id"] for m in membership.get(cid, [])]})
            dump(root / "external_input" / benchmark / target / "paper_status.json", {"benchmark": benchmark, "target": target, "checkpoints": cps})
            progress.append({"benchmark": benchmark, "target": target, "g0": str(g0), "g0_sha256": sha(g0),
                "primitive_count": len(plans), "root_count": len({m["root"] for m in members}), "pair_count": len(pairs), "closure_count": len(closures),
                "profile_count": len(profiles), "effective_sha_count": len({sha(p) for p in paths.values()}),
                "normal_search_exact": normal.get("total_search_exact"), "selected_ids": [r["candidate_id"] for r in chosen]})
            dump(root / "progress_summary.json", progress)
    dump(root / "frozen_finalists.json", {"frozen_unix": time.time(), "all_search_complete_before_external": True, "finalists": frozen})
    run("external", ["python3", D1 / "scripts/run_paper_external_grid.py", "--v8-root", root / "external_input", "--output-root", root / "external_genus", "--session-root", root / "external_sessions", "--timeout", "1100"])
    external = load(root / "external_genus/external_status.json")["results"]
    validations = []
    for point in progress:
        benchmark, target = point["benchmark"], point["target"]
        rows = [r for r in external if r["benchmark"] == benchmark and r["target"] == target]
        g0 = next(r for r in rows if r["round"] == 0)
        proof = D1.parent / "paper_results_v8/baseline_recovery_v1/equivalence" / benchmark / Path(point["g0"]).parent.name / "R0/status.json"
        v = load(proof)
        assert v["target_sha256"] == point["g0_sha256"]
        assert v["source_sha256"] == sha(Path(v["source"]))
        validations.append({**v, "action": "reuse_G0_exact_SHA", "receipt_source": str(proof)})
        best = min(rows, key=lambda r: (r["external"]["d2ap"], r["round"]))
        if best["round"]:
            dest = root / "formal_candidates" / benchmark / f"{target}.v"
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(best["netlist"], dest)
    for benchmark in args.cases.split(","):
        if not (root / "formal_candidates" / benchmark).exists():
            continue
        source = D1 / "third_party/epfl_benchmarks/arithmetic" / (benchmark.removeprefix("epfl_") + ".v")
        run("formal_"+benchmark, ["python3", D1 / "scripts/validate_mapped_candidate_directory.py", "--source", source, "--source-top", "top", "--candidate-dir", root / "formal_candidates" / benchmark,
            "--output-root", root / "equivalence" / benchmark, "--benchmark", benchmark, "--jobs", "2", "--timeout", "600"])
        validations += load(root / "equivalence" / benchmark / "validation_status.json")["results"]
    dump(root / "validation_receipts.json", validations)
    results = []
    for point in progress:
        rows = [r for r in external if r["benchmark"] == point["benchmark"] and r["target"] == point["target"]]
        g0 = next(r for r in rows if r["round"] == 0)
        allowed = [r for r in frozen if r["benchmark"] == point["benchmark"] and r["target"] == point["target"]]
        winner = select_verified_incumbent(g0, rows, {r["sha256"] for r in allowed}, validations)
        identity = next(r for r in allowed if r["round"] == winner["round"])
        dest = root / "final" / point["benchmark"] / point["target"] / "mapped.v"
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(winner["netlist"], dest)
        ratio = winner["external"]["d2ap"] / g0["external"]["d2ap"]
        results.append({"benchmark": point["benchmark"], "target": point["target"], "candidate_id": identity["candidate_id"], "ratio": ratio,
            "fallback_g0": winner["round"] == 0, "positive": ratio < cfg["positive_ratio"], "meaningful_positive": ratio < cfg["meaningful_positive_ratio"],
            "sha256": sha(dest), "final_netlist": str(dest), "external": winner["external"], "internal": identity["internal"], "members": identity["members"]})
    dump(root / "SUMMARY.json", {"stage": "complete", "results": results, "positive_count": sum(r["positive"] for r in results),
        "meaningful_positive_count": sum(r["meaningful_positive"] for r in results), "nonregression_count": sum(r["ratio"] <= 1 for r in results),
        "goal_met": len(results) == 10 and sum(r["positive"] for r in results) >= cfg["majority_required"],
        "wall_sec": time.time()-started, "stages": stages, "new_external_count": len(external), "historical_winner_reuse": False})
    dump(root / "status.json", {"stage": "complete", "status": "complete", "driver_pid": os.getpid()})
    print(json.dumps(results, indent=2), flush=True)


if __name__ == "__main__":
    main()

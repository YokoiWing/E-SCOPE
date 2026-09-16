#!/usr/bin/env python3
"""Fresh autonomous Conquer recall trial with external feedback held out.

The search consumes only each current G0, proof-carrying candidates freshly
mined from that G0, and Internal whole-net profiles measured in this run.
Historical candidates, IDs, scores, netlists and external results are not read.
"""
import argparse
import json
import math
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

from build_measured_fusion_moves_v1 import member_rows
from compact_iterative_results import preserve_leader
from conquer_defense_policy_v1 import endpoint_sets, endpoint_sets_iterative, pair_moves, closure_moves, make_plan
from conquer_defense_policy_v2 import finalists as guarded_finalists
from conquer_recall_policy_v3 import (
    select_sources_v3,
    recall_pair_moves,
    initial_closure_moves,
    recourse_moves,
    recall_finalists,
)
from generate_boundary_identity_bypass_v1 import generate
from run_conquer_defense_v1 import available_memory_kib, normal_policy
from run_sin_incumbent_preserving_probe_v1 import dump, load, sha, process_tree_rss
from run_iterative_da_sweep_v1 import AUDIT, frozen_env, g0_path, ANCHORS, LIB, SCALE, RULES
from semantic_shadow import load_cell_functions
from verified_incumbent_guard_v1 import select_verified_incumbent

D1 = Path(__file__).resolve().parents[1]
BIN_DIR = Path(os.environ.get("EGG_CONQUER_BIN_DIR", D1 / "target/release")).resolve()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--config", type=Path, default=D1 / "config/v8_conquer_recall_v3.json")
    parser.add_argument("--cases", default="epfl_sin,epfl_multiplier")
    parser.add_argument("--targets", default="")
    parser.add_argument("--anchor-manifest", type=Path,
                        help="Optional benchmark/target -> G0 path map; contains no candidates or scores")
    parser.add_argument("--g0-proof-manifest", type=Path,
                        help="Optional benchmark/target -> existing verified G0 proof path map")
    parser.add_argument("--resume-materialized-from", type=Path,
                        help="Reuse same-policy G0/mining/singleton assets from a profile-timeout run; no scores")
    parser.add_argument(
        "--search-only",
        action="store_true",
        help="freeze Internal finalists and stop before external Genus/formal validation",
    )
    parser.add_argument(
        "--no-phase1-drive-expansion",
        action="store_true",
        help="retain structural seeds while suppressing extra drive realizations",
    )
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=True)
    if (root / "pid.txt").exists() or (root / "frozen_policy.json").exists():
        raise RuntimeError("refuse existing generated run")
    started = time.time()
    (root / "pid.txt").write_text(str(os.getpid()) + "\n")
    cfg = load(args.config)
    resume = args.resume_materialized_from.resolve() if args.resume_materialized_from else None
    if resume:
        previous = load(resume / "frozen_policy.json")
        previous_status = load(resume / "status.json")
        assert previous_status["status"] == "resource_stopped"
        assert previous_status["stage"].endswith("_single_profile")
        assert not cfg.get("normal_lane_enabled", True), "materialized resume excludes normal search"
        allowed_resources = {"profile_jobs", "profile_chunk_size", "stage_timeout_sec"}
        assert {k: v for k, v in cfg.items() if k not in allowed_resources} == {
            k: v for k, v in previous["configuration"].items() if k not in allowed_resources}
        assert not (resume / "frozen_finalists.json").exists()
        assert not (resume / "external_genus").exists()
        for row in load(resume / "input_fingerprint.json"):
            path = Path(row["path"])
            if path == Path(__file__).resolve():
                continue
            assert sha(path) == row["sha256"], f"resume input changed: {path}"
        dump(root / "resume_receipt.json", {
            "source": str(resume), "source_status": previous_status,
            "reused_scores": 0, "external_feedback": False,
            "resource_only_configuration_changes": {k: [previous["configuration"].get(k), cfg.get(k)]
                for k in allowed_resources if previous["configuration"].get(k) != cfg.get(k)},
            "prior_stage_receipts": load(resume / "stage_receipts.json"),
        })
    assert cfg["promotion_policy"] == "whole-port-defense-plus-measured-recall-v3"
    assert cfg["search_parent"] == "G0_only" and not cfg["historical_winner_input"]
    assert not cfg["external_feedback_to_search"] and cfg["fallback"] == "G0_only"
    assert cfg["defense_finalist_quota"] < cfg["external_candidates_excluding_g0"]
    if cfg.get("clean_egg_environment"):
        for key in list(os.environ):
            if key.startswith("EGG_"):
                del os.environ[key]
    env = frozen_env(cfg["profile_jobs"])
    env.pop("EGG_OBJECTIVE_ENGINE", None)
    env.update({
        "EGG_EXPERIMENTAL_FULL_BOOLEAN_PROOF": "1",
        "EGG_REALIZATION_JOBS": str(cfg["profile_jobs"]),
        "EGG_PROFILE_BATCH_JOBS": str(cfg["profile_jobs"]),
        "EGG_PROFILE_BOUNDARY_RESPONSE": "1",
    })
    if args.no_phase1_drive_expansion:
        env["EGG_V8_PHASE_ABLATION"] = "no-pi-drive-expansion"
    if cfg.get("profile_static_only", False):
        env["EGG_PROFILE_STATIC_ONLY"] = "1"
    anchors = load(args.anchor_manifest) if args.anchor_manifest else ANCHORS
    g0_proofs = load(args.g0_proof_manifest) if args.g0_proof_manifest else None
    def current_g0(benchmark, target):
        return Path(anchors[benchmark][target]).resolve() if args.anchor_manifest else g0_path(benchmark, target)
    def current_g0_proof(benchmark, target, g0):
        if g0_proofs is not None:
            return Path(g0_proofs[benchmark][target]).resolve()
        return (D1.parent / "paper_results_v8/baseline_recovery_v1/equivalence"
                / benchmark / g0.parent.name / "R0/status.json")
    selected_points = [
        (benchmark, target)
        for benchmark in args.cases.split(",")
        for target in anchors[benchmark]
        if not args.targets or target in args.targets.split(",")
    ]
    input_paths = [
        args.config,
        LIB,
        SCALE,
        RULES,
        AUDIT,
        Path(env["EGG_V8_ULTRA_CONFIG"]),
        D1 / "scripts/conquer_defense_policy_v1.py",
        D1 / "scripts/conquer_defense_policy_v2.py",
        D1 / "scripts/conquer_recall_policy_v3.py",
        Path(__file__),
        D1 / "scripts" / (
            "mine_single_cell_fusions_no_drive.py"
            if args.no_phase1_drive_expansion
            else "mine_single_cell_fusions.py"
        ),
    ]
    assert selected_points, "no anchors selected"
    if resume:
        assert previous["selected_points"] == [{"benchmark": b, "target": t} for b, t in selected_points]
    if args.anchor_manifest:
        input_paths.append(args.anchor_manifest)
    if args.g0_proof_manifest:
        input_paths.append(args.g0_proof_manifest)
    input_paths += [current_g0(benchmark, target) for benchmark, target in selected_points]
    if args.g0_proof_manifest:
        input_paths += [current_g0_proof(benchmark, target, current_g0(benchmark, target))
                        for benchmark, target in selected_points]
    input_paths += [BIN_DIR / binary for binary in (
        "dump_mapped_nldm_v3_state",
        "run_generator_union_native",
        "materialize_structural_macro_plan",
        "run_frozen_a2_fast",
    )]
    policy_receipt = {
        "schema": "conquer-recall-v3-policy-receipt",
        "configuration": cfg,
        "config_sha256": sha(args.config),
        "driver_sha256": sha(Path(__file__)),
        "recall_policy_sha256": sha(D1 / "scripts/conquer_recall_policy_v3.py"),
        "defense_policy_sha256": sha(D1 / "scripts/conquer_defense_policy_v2.py"),
        "frozen_unix": started,
        "historical_winner_inputs": [],
        "external_feedback_to_search": False,
        "phase1_drive_expansion_enabled": not args.no_phase1_drive_expansion,
        "selected_points": [{"benchmark": b, "target": t} for b, t in selected_points],
    }
    dump(root / "frozen_policy.json", policy_receipt)
    dump(root / "input_fingerprint.json", [
        {"path": str(path.resolve()), "sha256": sha(path)} for path in input_paths
    ])
    dump(root / "environment.json", {key: value for key, value in env.items() if key.startswith("EGG_")})
    base_ultra = load(Path(env["EGG_V8_ULTRA_CONFIG"]))
    stages, progress, frozen = [], [], []

    def run(stage, command):
        begin, peak = time.monotonic(), 0
        available = available_memory_kib()
        if available < cfg["minimum_available_memory_kib"]:
            raise RuntimeError(f"insufficient host memory headroom before {stage}: {available} KiB")
        print("START", stage, flush=True)
        with (root / f"{stage}.stdout.log").open("w") as log:
            proc = subprocess.Popen(
                list(map(str, command)), cwd=D1, env=env, stdout=log,
                stderr=subprocess.STDOUT, start_new_session=True,
            )
            while proc.poll() is None:
                peak = max(peak, process_tree_rss(proc.pid))
                available = available_memory_kib()
                dump(root / "status.json", {
                    "stage": stage,
                    "status": "running",
                    "driver_pid": os.getpid(),
                    "child_pid": proc.pid,
                    "elapsed_sec": time.monotonic() - begin,
                    "sampled_tree_peak_rss_kib": peak,
                    "host_available_memory_kib": available,
                    "completed_anchors": len(progress),
                })
                if (time.monotonic() - begin > cfg["stage_timeout_sec"]
                        or peak > cfg["rss_limit_kib"]
                        or available < cfg["minimum_available_memory_kib"]):
                    os.killpg(proc.pid, signal.SIGTERM)
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
                    dump(root / "status.json", {
                        "stage": stage,
                        "status": "resource_stopped",
                        "elapsed_sec": time.monotonic() - begin,
                        "sampled_tree_peak_rss_kib": peak,
                        "host_available_memory_kib": available,
                    })
                    raise RuntimeError("resource stop " + stage)
                if cfg.get("wait_on_child_exit", False):
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        pass
                else:
                    time.sleep(2)
        receipt = {
            "stage": stage,
            "wall_sec": time.monotonic() - begin,
            "sampled_tree_peak_rss_kib": peak,
            "command": list(map(str, command)),
            "returncode": proc.returncode,
        }
        stages.append(receipt)
        dump(root / "stage_receipts.json", stages)
        if proc.returncode:
            dump(root / "status.json", {"stage": stage, "status": "failed"})
            raise RuntimeError(stage + " failed")
        print("DONE", stage, round(receipt["wall_sec"], 2), flush=True)

    functions = load_cell_functions(AUDIT)
    for benchmark, target in selected_points:
        point = root / benchmark / target
        point.mkdir(parents=True)
        tag = benchmark + "_" + target
        g0 = current_g0(benchmark, target)
        resume_point = resume / benchmark / target if resume else None
        if resume_point:
            shutil.copy2(resume_point / "g0_state.json", point / "g0_state.json")
            shutil.copytree(resume_point / "mined", point / "mined")
        else:
            run(tag + "_state", [
                BIN_DIR / "dump_mapped_nldm_v3_state", g0, point / "g0_state.json",
            ])
        state = load(point / "g0_state.json")
        gates = sum(not row["is_leaf"] and not row["is_root"] for row in state["occurrences"])
        resource_policy = normal_policy(cfg, base_ultra, gates)
        dump(point / "normal_resource_policy.json", resource_policy)
        env["EGG_V8_ULTRA_CONFIG"] = str(point / "normal_resource_policy.json")
        normal = {"total_search_exact": 0, "status": "disabled_by_explicit_policy"}
        if cfg.get("normal_lane_enabled", True):
            run(tag + "_normal", [
                BIN_DIR / "run_generator_union_native", benchmark, g0,
                point / "normal", "v3", cfg["normal_v8_rounds"], LIB, SCALE, RULES,
            ])
            normal = load(point / "normal/summary.json")
            if not preserve_leader(point / "normal/round_00", True):
                raise RuntimeError("normal pre-A2 leader unavailable")
        else:
            assert cfg["normal_v8_rounds"] == 0
            dump(point / "normal_lane_status.json", normal)
        mine_command = [
            "python3", D1 / "scripts" / (
                "mine_single_cell_fusions_no_drive.py"
                if args.no_phase1_drive_expansion
                else "mine_single_cell_fusions.py"
            ),
            "--graph", point / "g0_state.json",
            "--timing-state", point / "g0_state.json",
            "--audit", AUDIT,
            "--liberty", LIB,
            "--out-dir", point / "mined",
            "--max-gates", cfg["mining_max_gates"],
            "--max-boundary", cfg["mining_max_boundary"],
            "--variants-per-cone", cfg["mining_variants_per_cone"],
            "--max-candidates", cfg["mining_cap"],
        ]
        if args.no_phase1_drive_expansion:
            mine_command.extend(["--no-drive-expansion", "--scale-rules", SCALE])
        if not resume_point:
            run(tag + "_mine", mine_command)
        raw_plans = load(point / "mined/candidate_plan.json")
        raw_inventory = load(point / "mined/inventory.json")
        legacy, expanded, source_audit = select_sources_v3(
            raw_plans,
            raw_inventory,
            state,
            cfg["legacy_root_cap"],
            cfg["legacy_state_cap"],
            cfg["recall_root_cap"],
            cfg["recall_state_cap"],
        )
        legacy_ids = {plan["candidate_id"] for plan, _ in legacy}
        expanded_plans = [plan for plan, _ in expanded]
        expanded_inventory = [inventory for _, inventory in expanded]
        identity_ids = set()
        for plan in generate(state, functions):
            expanded_plans.append(plan)
            expanded_inventory.append({"region": plan["proof_outputs"]})
            identity_ids.add(plan["candidate_id"])
        legacy_ids |= identity_ids
        dump(point / "source_recall_audit.json", {
            "raw_candidate_count": len(raw_plans),
            "raw_root_count": len({row["root"] for row in raw_inventory}),
            "legacy_candidate_count": len(legacy),
            "legacy_root_count": len({row["root"] for _, row in legacy}),
            "expanded_candidate_count": len(expanded_plans),
            "expanded_root_count": len({plan["choices"][0]["root_anchor"] for plan in expanded_plans}),
            "identity_candidate_count": len(identity_ids),
            "root_states": source_audit,
        })
        dump(point / "primitive_plan.json", expanded_plans)
        dump(point / "primitive_inventory.json", expanded_inventory)
        if resume_point:
            assert expanded_plans == load(resume_point / "primitive_plan.json")
            assert expanded_inventory == load(resume_point / "primitive_inventory.json")
        members = member_rows(point / "primitive_plan.json", point / "primitive_inventory.json")
        legacy_members = [row for row in members if row["candidate_id"] in legacy_ids]
        paths = {"G0": g0}
        if cfg.get("normal_lane_enabled", True):
            paths.update({"NORMAL_POST": Path(normal["incumbent_path"]),
                          "NORMAL_PRE": point / "normal/round_00/leader_pre_a2.v"})
        profiles = []
        membership = {row["candidate_id"]: [row] for row in members}

        def materialize_profile(label, plans, include_all_paths=False):
            nonlocal profiles
            if plans:
                dump(point / f"{label}_plan.json", plans)
                materialized = resume_point / label if resume_point and label == "single" else point / label
                if resume_point and label == "single":
                    assert plans == load(resume_point / "single_plan.json")
                else:
                    run(tag + "_" + label + "_materialize", [
                        BIN_DIR / "materialize_structural_macro_plan",
                        g0, point / f"{label}_plan.json", materialized,
                    ])
                receipt = load(materialized / "summary.json")
                if receipt["legal"] != len(plans):
                    raise RuntimeError(f"{label} materialized {receipt['legal']}/{len(plans)}")
                files = list((materialized / "candidates").glob("*.v"))
                paths.update({path.stem: path for path in files})
            ids = list(paths) if include_all_paths else [plan["candidate_id"] for plan in plans]
            if not ids:
                return
            missing = [candidate_id for candidate_id in ids if candidate_id not in paths]
            if missing:
                raise RuntimeError(f"missing materialized paths for {label}: {missing}")
            manifest = [
                {"candidate_id": candidate_id, "input_netlist": str(paths[candidate_id])}
                for candidate_id in ids
            ]
            dump(point / f"{label}_profile_manifest.json", manifest)
            if resume_point and label == "single":
                assert manifest == load(resume_point / "single_profile_manifest.json")
                dump(point / "reused_singleton_sha.json", [
                    {**row, "sha256": sha(Path(row["input_netlist"]))} for row in manifest])
            chunk_size = cfg.get("profile_chunk_size", 0)
            if chunk_size and len(manifest) > chunk_size:
                chunks, merged = [], []
                for index, offset in enumerate(range(0, len(manifest), chunk_size)):
                    chunk = point / f"{label}_profile_chunks" / f"{index:03d}"
                    dump(chunk / "manifest.json", manifest[offset:offset + chunk_size])
                    run(tag + "_" + label + f"_profile_chunk_{index:03d}", [
                        BIN_DIR / "run_frozen_a2_fast", "profile-batch",
                        chunk / "manifest.json", chunk / "profiles.json",
                    ])
                    batch = load(chunk / "profiles.json")
                    merged += batch["profiles"]
                    chunks.append({"path": str(chunk / "profiles.json"),
                                   "task_count": batch["task_count"], "elapsed_sec": batch["elapsed_sec"]})
                    dump(point / f"{label}_profile_progress.json", {
                        "completed_profiles": len(merged), "total_profiles": len(manifest), "chunks": chunks})
                assert [r["candidate_id"] for r in merged] == ids
                dump(point / f"{label}_profiles.json", {
                    "schema": "egg-internal-nldm-v3-profile-batch-v1", "task_count": len(merged),
                    "implementation": "ordered Rust whole-net profile batches; independent shared cell databases",
                    "batch_jobs": cfg["profile_jobs"], "chunks": chunks, "profiles": merged,
                    "elapsed_sec": sum(c["elapsed_sec"] for c in chunks),
                })
            else:
                run(tag + "_" + label + "_profile", [
                    BIN_DIR / "run_frozen_a2_fast", "profile-batch",
                    point / f"{label}_profile_manifest.json", point / f"{label}_profiles.json",
                ])
            profiles += load(point / f"{label}_profiles.json")["profiles"]

        materialize_profile("single", expanded_plans, include_all_paths=True)
        endpoints = endpoint_sets_iterative(state) if cfg.get("iterative_endpoints", False) else endpoint_sets(state)

        defense_pairs = pair_moves(legacy_members, profiles, endpoints, cfg["defense_pair_cap"])
        defense_pair_members = {
            f"PAIR_{index:03d}": rows for index, rows in enumerate(defense_pairs)
        }
        membership.update(defense_pair_members)
        materialize_profile("pair", [make_plan(cid, rows) for cid, rows in defense_pair_members.items()])

        defense_closures = closure_moves(
            legacy_members,
            defense_pair_members,
            profiles,
            cfg["defense_closure_cap"],
            cfg["defense_max_sites"],
        )
        defense_closure_members = {
            f"CLOSURE_{index:03d}": rows for index, rows in enumerate(defense_closures)
        }
        membership.update(defense_closure_members)
        materialize_profile("closure", [make_plan(cid, rows) for cid, rows in defense_closure_members.items()])

        recall_pairs = recall_pair_moves(
            members,
            profiles,
            endpoints,
            cfg["recall_pair_cap"],
            exclude=defense_pairs,
        )
        recall_pair_members = {
            f"RECALL_PAIR_{index:03d}": rows for index, rows in enumerate(recall_pairs)
        }
        membership.update(recall_pair_members)
        materialize_profile("recall_pair", [make_plan(cid, rows) for cid, rows in recall_pair_members.items()])

        recall_closures = initial_closure_moves(
            members,
            recall_pair_members,
            profiles,
            endpoints,
            state,
            cfg["recall_initial_closure_cap"],
            cfg["recall_max_sites"],
        )
        recall_closure_members = {
            f"RECALL_CLOSE_{index:03d}": rows for index, rows in enumerate(recall_closures)
        }
        membership.update(recall_closure_members)
        materialize_profile("recall_closure", [make_plan(cid, rows) for cid, rows in recall_closure_members.items()])

        recourse_seeds = {**recall_pair_members, **recall_closure_members}
        recall_recourse = recourse_moves(
            members,
            recourse_seeds,
            profiles,
            cfg["recall_recourse_cap"],
            cfg["recall_max_sites"],
        )
        recall_recourse_members = {
            f"RECALL_RECOURSE_{index:03d}": rows for index, rows in enumerate(recall_recourse)
        }
        membership.update(recall_recourse_members)
        materialize_profile("recall_recourse", [make_plan(cid, rows) for cid, rows in recall_recourse_members.items()])

        ppa = {row["candidate_id"]: row for row in profiles}
        if len(ppa) != len(profiles):
            raise RuntimeError("duplicate candidate IDs in profile union")
        interaction = []
        for candidate_id, rows in {**defense_pair_members, **recall_pair_members}.items():
            expected = math.prod(ppa[row["candidate_id"]]["d2ap"] / ppa["G0"]["d2ap"] for row in rows)
            actual = ppa[candidate_id]["d2ap"] / ppa["G0"]["d2ap"]
            interaction.append({
                "candidate_id": candidate_id,
                "members": [row["candidate_id"] for row in rows],
                "endpoint_overlap": sorted(endpoints[rows[0]["root"]] & endpoints[rows[1]["root"]]),
                "singleton_product": expected,
                "measured_pair": actual,
                "interaction_log_residual": math.log(actual / expected),
            })
        dump(point / "interaction_audit.json", interaction)
        dump(point / "profiles.json", {"profiles": profiles})
        dump(point / "membership_audit.json", {
            candidate_id: {
                "members": [row["candidate_id"] for row in rows],
                "roots": [row["root"] for row in rows],
                "member_count": len(rows),
            }
            for candidate_id, rows in membership.items()
        })

        defense_ids = set(membership) - {
            candidate_id for candidate_id in membership if candidate_id.startswith("RECALL_")
        }
        defense_profile_ids = defense_ids | {"G0", "NORMAL_PRE", "NORMAL_POST"}
        defense_profiles = [row for row in profiles if row["candidate_id"] in defense_profile_ids]
        defense_membership = {candidate_id: rows for candidate_id, rows in membership.items()
                              if candidate_id in defense_ids}
        defense_chosen, defense_promotion = guarded_finalists(
            defense_profiles,
            paths,
            sha,
            defense_membership,
            cfg["external_candidates_excluding_g0"],
        )
        chosen, finalist_audit = recall_finalists(
            defense_chosen,
            profiles,
            paths,
            sha,
            membership,
            cfg["external_candidates_excluding_g0"],
            cfg["defense_finalist_quota"],
        )
        dump(point / "defense_promotion_audit.json", defense_promotion)
        dump(point / "finalist_audit.json", finalist_audit)
        checkpoints = []
        for ordinal, row in enumerate([ppa["G0"]] + chosen):
            candidate_id = row["candidate_id"]
            checkpoints.append({"round": ordinal, "netlist": str(paths[candidate_id])})
            frozen.append({
                "benchmark": benchmark,
                "target": target,
                "round": ordinal,
                "candidate_id": candidate_id,
                "netlist": str(paths[candidate_id]),
                "sha256": sha(paths[candidate_id]),
                "internal": row,
                "members": [member["candidate_id"] for member in membership.get(candidate_id, [])],
                "finalist_lane": "g0" if ordinal == 0 else finalist_audit[ordinal - 1]["lane"],
            })
        dump(root / "external_input" / benchmark / target / "paper_status.json", {
            "benchmark": benchmark,
            "target": target,
            "checkpoints": checkpoints,
        })
        progress.append({
            "benchmark": benchmark,
            "target": target,
            "g0": str(g0),
            "g0_sha256": sha(g0),
            "raw_candidate_count": len(raw_plans),
            "primitive_count": len(expanded_plans),
            "legacy_root_count": len({row["root"] for row in legacy_members}),
            "effective_root_count": len({row["root"] for row in members}),
            "defense_pair_count": len(defense_pairs),
            "defense_closure_count": len(defense_closures),
            "recall_pair_count": len(recall_pairs),
            "recall_closure_count": len(recall_closures),
            "recourse_count": len(recall_recourse),
            "profile_count": len(profiles),
            "effective_sha_count": len({sha(path) for path in paths.values()}),
            "normal_search_exact": normal.get("total_search_exact"),
            "selected_ids": [row["candidate_id"] for row in chosen],
        })
        dump(root / "progress_summary.json", progress)

    freeze_time = time.time()
    dump(root / "frozen_finalists.json", {
        "frozen_unix": freeze_time,
        "all_search_complete_before_external": True,
        "historical_winner_inputs": [],
        "external_feedback_to_search": False,
        "finalists": frozen,
    })
    if args.search_only:
        summary = {
            "stage": "search_complete",
            "classification": "fresh Conquer search; external evaluation pending",
            "points": progress,
            "finalist_count": len(frozen),
            "wall_sec": time.time() - started,
            "stages": stages,
            "historical_winner_reuse": False,
            "all_search_complete_before_external": True,
        }
        dump(root / "SUMMARY.json", summary)
        dump(root / "status.json", {
            "stage": "search_complete",
            "status": "complete",
            "external_validation": "PENDING",
            "driver_pid": os.getpid(),
        })
        print(json.dumps(summary, indent=2), flush=True)
        return
    run("external", [
        "python3", D1 / "scripts/run_paper_external_grid.py",
        "--v8-root", root / "external_input",
        "--output-root", root / "external_genus",
        "--session-root", root / "external_sessions",
        "--timeout", str(cfg.get("external_timeout_sec", 1100)),
    ])
    external = load(root / "external_genus/external_status.json")["results"]
    validations = []
    for point in progress:
        benchmark, target = point["benchmark"], point["target"]
        rows = [row for row in external if row["benchmark"] == benchmark and row["target"] == target]
        g0_row = next(row for row in rows if row["round"] == 0)
        proof = current_g0_proof(benchmark, target, Path(point["g0"]))
        receipt = load(proof)
        assert receipt["target_sha256"] == point["g0_sha256"]
        assert receipt["source_sha256"] == sha(Path(receipt["source"]))
        validations.append({**receipt, "action": "reuse_G0_exact_SHA", "receipt_source": str(proof)})
        best = min(rows, key=lambda row: (row["external"]["d2ap"], row["round"]))
        if best["round"]:
            destination = root / "formal_candidates" / benchmark / f"{target}.v"
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(best["netlist"], destination)
    proof_chains = []
    for benchmark in args.cases.split(","):
        candidate_dir = root / "formal_candidates" / benchmark
        if not candidate_dir.exists():
            continue
        source = D1 / "third_party/epfl_benchmarks/arithmetic" / (benchmark.removeprefix("epfl_") + ".v")
        if cfg.get("formal_against_verified_g0", False):
            from run_conquer_reference_audit_v1 import checked_reference, CHECKS
            points = [p for p in progress if p["benchmark"] == benchmark]
            assert len(points) == 1, "G0-relative proof mode currently requires one anchor per benchmark"
            point = points[0]
            source = Path(point["g0"])
            proof = current_g0_proof(benchmark, point["target"], source)
            original = checked_reference(proof, source)
        run("formal_" + benchmark, [
            "python3", D1 / "scripts/validate_mapped_candidate_directory.py",
            "--source", source,
            "--source-top", original["target_top"] if cfg.get("formal_against_verified_g0", False) else "top",
            "--candidate-dir", candidate_dir,
            "--output-root", root / "equivalence" / benchmark,
            "--benchmark", benchmark,
            "--jobs", str(cfg["profile_jobs"]),
            "--timeout", str(cfg.get("formal_timeout_sec", 600)),
        ])
        children = load(root / "equivalence" / benchmark / "validation_status.json")["results"]
        if cfg.get("formal_against_verified_g0", False):
            for child in children:
                assert child["source_sha256"] == original["target_sha256"]
                assert sha(Path(child["target_netlist"])) == child["target_sha256"]
                assert all(child[k] == "PASS" for k in CHECKS)
                proof_chains.append({"method": "exact-SHA transitivity of two whole-network CEC edges",
                    "original_to_g0": str(proof), "original_source_sha256": original["source_sha256"],
                    "g0_sha256": original["target_sha256"], "candidate_sha256": child["target_sha256"],
                    "g0_to_candidate": str(root / "equivalence" / benchmark / child["target"] / "status.json")})
            checked_reference(proof, source)
        validations += children
    if cfg.get("formal_against_verified_g0", False):
        dump(root / "proof_chains.json", proof_chains)
    dump(root / "validation_receipts.json", validations)
    results = []
    for point in progress:
        rows = [row for row in external if row["benchmark"] == point["benchmark"] and row["target"] == point["target"]]
        g0_row = next(row for row in rows if row["round"] == 0)
        allowed = [row for row in frozen if row["benchmark"] == point["benchmark"] and row["target"] == point["target"]]
        winner = select_verified_incumbent(g0_row, rows, {row["sha256"] for row in allowed}, validations)
        identity = next(row for row in allowed if row["round"] == winner["round"])
        destination = root / "final" / point["benchmark"] / point["target"] / "mapped.v"
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(winner["netlist"], destination)
        ratio = winner["external"]["d2ap"] / g0_row["external"]["d2ap"]
        results.append({
            "benchmark": point["benchmark"],
            "target": point["target"],
            "candidate_id": identity["candidate_id"],
            "finalist_lane": identity["finalist_lane"],
            "ratio": ratio,
            "fallback_g0": winner["round"] == 0,
            "positive": ratio < cfg["positive_ratio"],
            "meaningful_positive": ratio < cfg["meaningful_positive_ratio"],
            "sha256": sha(destination),
            "final_netlist": str(destination),
            "external": winner["external"],
            "internal": identity["internal"],
            "members": identity["members"],
        })
    summary = {
        "stage": "complete",
        "results": results,
        "positive_count": sum(row["positive"] for row in results),
        "meaningful_positive_count": sum(row["meaningful_positive"] for row in results),
        "nonregression_count": sum(row["ratio"] <= 1 for row in results),
        "goal_met": len(results) == len(selected_points)
                    and sum(row["positive"] for row in results) >= cfg["majority_required"],
        "wall_sec": time.time() - started,
        "stages": stages,
        "new_external_count": len(external),
        "historical_winner_reuse": False,
        "all_search_complete_before_external": True,
    }
    dump(root / "SUMMARY.json", summary)
    dump(root / "status.json", {"stage": "complete", "status": "complete", "driver_pid": os.getpid()})
    print(json.dumps(results, indent=2), flush=True)


if __name__ == "__main__":
    main()

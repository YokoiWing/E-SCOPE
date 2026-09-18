#!/usr/bin/env python3
"""Minimal Iterative execution from packaged source, inputs, and policy."""
import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import time
from pathlib import Path
HERE = Path(__file__).resolve().parent
POLICY = HERE / "data/policy.json"
INSTANCE_RE = re.compile(r"^\s*\w+_ASAP7_75t_R\s+\w+\s*\(", re.M)
OWNED = []
def read(path, retries=0):
    path = Path(path)
    for attempt in range(retries + 1):
        try:
            return json.loads(path.read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            if attempt == retries:
                raise
            time.sleep(0.2)

def write(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)

def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def instances(path):
    return len(INSTANCE_RE.findall(Path(path).read_text()))

def predict(tree, features):
    while "action" not in tree:
        tree = tree["le" if features[tree["feature"]] <= tree["threshold"] else "gt"]
    return tree["action"]

def point(row):
    return row.get("point", row.get("candidate"))

def restore_interface(source, target, g0, module_name):
    original, text = Path(g0).read_text(), Path(source).read_text()

    def ports(value, kind):
        return [v.strip() for chunk in re.findall(r"\b" + kind + r"\s+([^;]+);", value)
                for v in chunk.split(",")]

    pis, pos = ports(original, "input"), ports(original, "output")
    previous = ports(text, "input")
    if not set(previous) <= set(pis) or set(ports(text, "output")) != set(pos):
        raise RuntimeError("generated netlist interface cannot be restored from this path G0")
    netlist_body = re.sub(r"\bmodule\s+\w+\s*\([^;]*\);", "", text, count=1)
    netlist_body = re.sub(r"\b(?:input|output)\s+[^;]+;", "", netlist_body)
    identifiers = set(re.findall(r"\b\w+\b", netlist_body))
    for pin in set(pis) - set(previous):
        if pin in identifiers:
            raise RuntimeError(f"removed input {pin} remains in generated netlist body")
    Path(target).write_text(f"module {module_name} (" + ", ".join(pis + pos) + ");\ninput " +
                            ", ".join(pis) + ";\noutput " + ", ".join(pos) + ";\n" + netlist_body)

def base_features(case, guide, g0):
    count = instances(g0)
    if count <= 0:
        raise RuntimeError(f"no standard-cell instances found in {g0}")
    return {"internal_v3": int(guide == "internal_v3"), "initial_instances": count,
            "current_instances": count, "stage_index": -1, "stage_round": 0,
            "total_round": 0, "rules": 0, "candidate_cap": 0, "accepted": 0,
            "stagnation": 0, "area_gain_fraction": 0.0}

def clean_env(profile, config_path, objective_path, assets):
    env = {k: v for k, v in os.environ.items() if not k.startswith("EGG_")}
    env.update({k: str(v) for k, v in profile["environment"].items()})
    env.update({"EGG_V8_ULTRA_CONFIG": str(config_path), "EGG_OBJECTIVE_SPEC": str(objective_path),
                "EGG_RULE_PATHS": assets["EGG_RULE_PATHS"]["path"],
                "EGG_LIB_PATH": assets["EGG_LIB_PATH"]["path"],
                "EGG_SCALE_RULES": assets["EGG_SCALE_RULES"]["path"]})
    return env

def terminate_owned(process):
    if process.poll() is not None:
        return process.returncode
    os.killpg(process.pid, signal.SIGTERM)
    try:
        return process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        return process.wait(timeout=10)

def wait_for_round(process, receipt, output_candidates, deadline):
    while time.monotonic() < deadline:
        if receipt.is_file():
            try:
                value = read(receipt, retries=3)
                if any(Path(p).is_file() for p in output_candidates if p):
                    return value
            except (FileNotFoundError, json.JSONDecodeError):
                pass
        if process.poll() is not None:
            raise RuntimeError(f"native process exited {process.returncode} before complete receipt {receipt}")
        time.sleep(0.25)
    raise TimeoutError(f"timed out waiting for {receipt}")

def wait_for_summary_round(process, summary_path, required_rounds, deadline):
    while time.monotonic() < deadline:
        try:
            summary = read(summary_path, retries=2)
            if len(summary.get("rounds", [])) >= required_rounds:
                return summary
        except (FileNotFoundError, json.JSONDecodeError):
            pass
        if process.poll() is not None:
            # One last atomic-read attempt after a normal exit.
            summary = read(summary_path, retries=3)
            if len(summary.get("rounds", [])) >= required_rounds:
                return summary
            raise RuntimeError(f"native process exited {process.returncode} before summary round {required_rounds}")
        time.sleep(0.2)
    raise TimeoutError(f"timed out waiting for summary round {required_rounds}: {summary_path}")

def objective_for(profile, initial_cap, previous_delay, name):
    policy = profile["delay_bound_policy"]
    if policy == "original_guide_G0_delay":
        bound = initial_cap
    elif policy == "0.99_times_original_guide_G0_delay":
        bound = initial_cap * 0.99
    elif policy == "previous_stage_final_delay":
        if previous_delay is None:
            raise RuntimeError("previous-stage delay requested before a stage has completed")
        bound = previous_delay
    else:
        raise RuntimeError(f"unknown symbolic delay policy {policy}")
    return {"schema_version": 1, "name": name, "objective": profile["objective"],
            "constraints": [{"metric": "delay", "relation": "at_most", "bound": bound}]}

def execute_route(plan):
    path_dir = Path(plan["output_directory"])
    if path_dir.exists():
        raise RuntimeError(f"refusing to overwrite existing execution directory: {path_dir}")
    path_dir.mkdir(parents=True)
    write(path_dir / "PLAN.json", plan)
    policy = read(POLICY)
    features = base_features(plan["case"], plan["guide"], plan["g0"])
    action = predict(policy["initialization"], features)
    if action != plan["initial_policy_action"]:
        raise RuntimeError(f"online initialization action changed: {action}")
    current = Path(plan["g0"])
    initial_cap = plan["initial_delay_bound"]
    previous_delay = None
    previous_stage_point = None
    total_round = 0
    route_started = time.monotonic()
    trace = []
    schedule_divergence = None
    start_stage = 0
    prefix_seconds = 0.0
    for si in range(start_stage, len(plan["stages"])):
        stage_plan = plan["stages"][si]
        profile_name = action.removeprefix("ENTER_")
        if profile_name != stage_plan["profile"]:
            schedule_divergence = f"stage {si+1} policy entered {profile_name}, but the route has no matching next stage"
            break
        profile = policy["profiles"][profile_name]
        stage_dir = Path(stage_plan["output_directory"])
        stage_dir.mkdir(parents=True)
        config_path, objective_path = stage_dir / "config.json", stage_dir / "objective.json"
        write(config_path, profile["config"])
        objective = objective_for(profile, initial_cap, previous_delay,
                                  f"iterative_{plan['guide']}_stage_{si+1}")
        write(objective_path, objective)
        env = clean_env(profile, config_path, objective_path, stage_plan["assets"])
        if si == 0:
            stage_input = current
        else:
            stage_input = stage_dir / "input.v"
            restore_interface(current, stage_input, plan["g0"], plan["case"])
        stage_round = 0
        previous_area = None
        stagnation = 0
        selection = stage_plan["selection_policy"]
        portfolio = selection == "internal_feasible_portfolio_selection"
        iterations = stage_plan["iteration_cap"] if portfolio else 1
        stage_last_point = previous_stage_point
        for iteration in range(iterations):
            search_dir = stage_dir / (f"iteration_{iteration:02d}/search" if portfolio else "search")
            process_input = stage_input if iteration == 0 else current
            command = [plan["binary"], plan["case"], str(process_input), str(search_dir), "v3",
                       str(stage_plan["native_requested_round_cap"]), stage_plan["assets"]["EGG_LIB_PATH"]["path"],
                       stage_plan["assets"]["EGG_SCALE_RULES"]["path"], stage_plan["assets"]["EGG_RULE_PATHS"]["path"]]
            log_path = search_dir.parent / "stdout.log"
            log_path.parent.mkdir(parents=True, exist_ok=True)
            record = {"command": command, "env": {k: v for k, v in env.items() if k.startswith("EGG_")},
                      "started_at_unix": time.time(), "owner_pid": os.getpid()}
            write(search_dir.parent / "COMMAND.json", record)
            with log_path.open("w") as log:
                process = subprocess.Popen(command, cwd=HERE, env=env, stdout=log,
                                           stderr=subprocess.STDOUT, start_new_session=True)
                OWNED.append(process)
                write(search_dir.parent / "PID.json", {"launcher_pid": os.getpid(), "process_group": process.pid,
                                                        "native_pid": process.pid, "started_at_unix": time.time()})
                deadline = time.monotonic() + stage_plan["timeout_sec"]
                raw_round = 0
                while True:
                    receipt_path = search_dir / f"round_{raw_round:02d}/round_receipt.json"
                    receipt = wait_for_round(process, receipt_path,
                                             [search_dir / f"round_{raw_round:02d}/accepted.v",
                                              search_dir / "summary.json"], deadline)
                    summary = wait_for_summary_round(process, search_dir / "summary.json", raw_round + 1, deadline)
                    if portfolio:
                        pool = read(search_dir / f"round_{raw_round:02d}/progressive/summary.json", retries=5)["transit_pool"]
                        # The portfolio policy requires strict improvement against this run's parent.
                        if stage_last_point is None:
                            terminate_owned(process)
                            raise RuntimeError("portfolio stage lacks the preceding online parent point")
                        parent_area = stage_last_point["area"]
                        feasible = [x for x in pool if x["point"]["delay"] <= objective["constraints"][0]["bound"] + 1e-9
                                    and x["point"]["area"] < parent_area - 1e-9]
                        if not feasible:
                            terminate_owned(process)
                            raise RuntimeError(f"portfolio iteration {iteration+1} has no feasible strict area improvement")
                        chosen = min(feasible, key=lambda x: (x["point"]["area"], x["point"]["power"],
                                                              x["point"]["delay"], x["candidate_id"]))
                        chosen_path = Path(chosen["best_path"])
                        accepted = search_dir.parent / "accepted.v"
                        shutil.copyfile(chosen_path, accepted)
                        actual = {"parent_sha256": digest(process_input), "candidate_sha256": digest(accepted),
                                  "accepted_path": str(accepted), "point": chosen["point"],
                                  "candidate_id": chosen["candidate_id"], "status": "accepted_feasible_portfolio"}
                        terminate_owned(process)
                        round_row = actual
                    else:
                        round_row = receipt
                    stage_round += 1; total_round += 1
                    accepted_flag = bool(round_row.get("strict_improvement", str(round_row.get("status", "")).startswith("accepted")))
                    stagnation = 0 if accepted_flag else stagnation + 1
                    area = point(round_row)["area"]
                    gain = (previous_area - area) / previous_area if previous_area else 0.0
                    previous_area = area
                    accepted_path = round_row.get("accepted_path")
                    current_count = instances(accepted_path) if portfolio else round_row["instance_count"]
                    features = dict(features, current_instances=current_count, stage_index=si,
                                    stage_round=stage_round, total_round=total_round,
                                    rules=profile["rule_count"],
                                    candidate_cap=profile["config"]["pre_materialization_candidate_cap"],
                                    accepted=int(accepted_flag), stagnation=stagnation, area_gain_fraction=gain)
                    action = predict(policy["after_round"], features)
                    trace.append({"stage": si+1, "stage_round": stage_round, "total_round": total_round,
                                  "features": features, "policy_action": action,
                                  "parent_sha256": round_row.get("parent_sha256"),
                                  "candidate_sha256": round_row.get("candidate_sha256"),
                                  "accepted_sha256": round_row.get("accepted_sha256"),
                                  "parent_action": round_row.get("parent_action"),
                                  "elapsed_search_sec": time.monotonic() - route_started})
                    write(path_dir / "TRACE.json", trace)
                    if action != "CONTINUE" or portfolio:
                        terminate_owned(process)
                        break
                    raw_round += 1
                if portfolio:
                    current = Path(round_row["accepted_path"])
                    stage_last_point = point(round_row)
                else:
                    current = Path(summary["incumbent_path"])
                    stage_last_point = summary["incumbent_ppa"]
                if action != "CONTINUE" and not portfolio:
                    break
            if portfolio and action != "CONTINUE" and iteration + 1 < iterations:
                break
        if action not in ("STOP",) and not action.startswith("ENTER_"):
            schedule_divergence = f"stage {si+1} ended with invalid action {action}"
            break
        previous_delay = stage_last_point["delay"]
        previous_stage_point = stage_last_point
        if action == "STOP":
            break
    elapsed = prefix_seconds + time.monotonic() - route_started
    result = {"case": plan["case"], "guide": plan["guide"], "search_seconds": elapsed,
              "schedule_divergence": schedule_divergence,
              "trace_rounds": len(trace), "external_validation": "PENDING"}
    if schedule_divergence:
        result.update(status="DIVERGED", classification="DIVERGED", final_sha256="")
    else:
        final = path_dir / "final.v"
        restore_interface(current, final, plan["g0"], plan["case"])
        result["final_sha256"] = digest(final)
        result.update(status="COMPLETE_PENDING_VALIDATION",
                      evaluation_note="fresh legality, CEC, and mapped-as-is evaluation required")
    write(path_dir / "RESULT.json", result)
    return result
def verify():
    records = read(HERE / "CHECKSUMS.json")
    for relative, expected in records.items():
        if digest(HERE / relative) != expected:
            raise RuntimeError(f"checksum mismatch: {relative}")
    return len(records)


def build_plan(case, guide, output, binary=None):
    plan = read(HERE / "data/routes.json")[case + "/" + guide]["plan"]
    for key in ("g0", "binary", "policy"):
        plan[key] = str((HERE / plan[key]).resolve())
    if binary is not None:
        plan["binary"] = str(binary.expanduser().resolve())
    plan["output_directory"] = str(output.resolve())
    for stage in plan["stages"]:
        stage["output_directory"] = str(output.resolve() / f"stage_{stage['stage']:02d}")
        for record in stage["assets"].values():
            record["path"] = str((HERE / record["path"]).resolve())
    action = predict(read(POLICY)["initialization"], base_features(case, guide, plan["g0"]))
    if action != plan["initial_policy_action"]:
        raise RuntimeError("initialization does not match the packaged policy")
    return plan


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--case", required=True)
    ap.add_argument("--guide", choices=["genlib", "internal_v3"], required=True)
    ap.add_argument("--mode", choices=["plan", "execute"], default="plan")
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument(
        "--binary",
        type=Path,
        default=Path(os.environ.get("E_SCOPE_ITERATIVE_BIN", HERE / "source/target/release/run_generator_union_native")),
        help="source-built run_generator_union_native executable",
    )
    args = ap.parse_args()
    def interrupted(signum, frame):
        raise KeyboardInterrupt(f"signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    verify()
    plan = build_plan(args.case, args.guide, args.out, args.binary)
    if args.out.exists():
        raise RuntimeError("output already exists; choose a new directory")
    if args.mode == "plan":
        args.out.mkdir(parents=True)
        write(args.out / "PLAN.json", plan)
        print(json.dumps({"mode": "plan", "search_executed": False, "output": str(args.out)}))
        return
    if not Path(plan["binary"]).is_file():
        raise RuntimeError(
            f"Iterative executable not found: {plan['binary']}; build source/ or pass --binary"
        )
    try:
        result = execute_route(plan)
        write(args.out / "status.json", result)
        print(json.dumps(result, indent=2))
    except BaseException as exc:
        if args.out.exists():
            write(args.out / "status.json", {"status": "ERROR", "error": repr(exc)})
        raise
    finally:
        for process in OWNED:
            terminate_owned(process)


if __name__ == "__main__":
    main()

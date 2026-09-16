#!/usr/bin/env python3
"""Run one real Iterative round to test the packaged source build and assets."""

import argparse
import json
import os
import subprocess
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
ITERATIVE = REPO / "case_study/iterative"


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--case", default="usb_phy")
    parser.add_argument("--guide", choices=("genlib", "internal_v3"), default="internal_v3")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--timeout-sec", type=float, default=300)
    args = parser.parse_args()
    if args.out.exists():
        raise RuntimeError(f"output already exists: {args.out}")
    routes = json.loads((ITERATIVE / "data/routes.json").read_text())
    policy = json.loads((ITERATIVE / "data/policy.json").read_text())
    plan = routes[f"{args.case}/{args.guide}"]["plan"]
    profile = policy["profiles"][plan["initial_policy_action"].removeprefix("ENTER_")]
    stage = plan["stages"][0]
    args.out.mkdir(parents=True)
    config = args.out / "config.json"
    objective = args.out / "objective.json"
    write(config, profile["config"])
    write(
        objective,
        {
            "schema_version": 1,
            "name": "packaged_one_round_smoke",
            "objective": profile["objective"],
            "constraints": [
                {
                    "metric": "delay",
                    "relation": "at_most",
                    "bound": plan["initial_delay_bound"],
                }
            ],
        },
    )
    assets = {
        key: str((ITERATIVE / record["path"]).resolve())
        for key, record in stage["assets"].items()
    }
    env = {key: value for key, value in os.environ.items() if not key.startswith("EGG_")}
    env.update({key: str(value) for key, value in profile["environment"].items()})
    env.update(assets)
    env["EGG_V8_ULTRA_CONFIG"] = str(config)
    env["EGG_OBJECTIVE_SPEC"] = str(objective)
    search = args.out / "search"
    command = [
        str(args.binary.expanduser().resolve()),
        args.case,
        str((ITERATIVE / plan["g0"]).resolve()),
        str(search),
        "v3",
        "1",
        assets["EGG_LIB_PATH"],
        assets["EGG_SCALE_RULES"],
        assets["EGG_RULE_PATHS"],
    ]
    write(args.out / "COMMAND.json", {"command": command, "round_cap": 1, "smoke_only": True})
    with (args.out / "stdout.log").open("w") as log:
        completed = subprocess.run(
            command,
            cwd=ITERATIVE,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            timeout=args.timeout_sec,
        )
    if completed.returncode:
        raise RuntimeError(f"smoke exited {completed.returncode}; see {args.out / 'stdout.log'}")
    summary = json.loads((search / "summary.json").read_text())
    result = {
        "status": "PASS",
        "classification": "one-round functional smoke; not a paper-result replay",
        "case": args.case,
        "guide": args.guide,
        "rounds": len(summary["rounds"]),
        "incumbent_ppa": summary["incumbent_ppa"],
    }
    write(args.out / "RESULT.json", result)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

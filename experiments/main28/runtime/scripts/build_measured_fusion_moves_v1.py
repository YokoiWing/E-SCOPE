#!/usr/bin/env python3
"""Build pair or coordinate-recourse moves from a fixed primitive family.

This is a development-oracle utility: mapped-as-is scores may rank moves, but
they never add primitives outside the pre-frozen family.  Generated plans keep
the original proof groups and use the standard all-or-nothing materializer.
"""

from __future__ import annotations

import argparse
import json
from itertools import combinations
from pathlib import Path

from build_closure_aware_fusion_portfolio import expression_anchors


def plan_signature(plan: dict) -> tuple[str, ...]:
    return tuple(sorted(group["candidate_id"] for group in plan["_members"]))


def load_external(status_path: Path, receipt_path: Path) -> tuple[dict[str, float], float]:
    status = json.loads(status_path.read_text())
    results = {int(row["round"]): row["external"] for row in status["results"]}
    receipt = json.loads(receipt_path.read_text())
    paths = {int(row["round"]): Path(row["path"]) for row in receipt["candidates"]}
    g0 = results[0]["d2ap"]
    return {
        paths[index].stem: float(metrics["d2ap"]) / float(g0)
        for index, metrics in results.items()
    }, float(g0)


def member_rows(plan_path: Path, inventory_path: Path) -> list[dict]:
    plans = json.loads(plan_path.read_text())
    inventory = json.loads(inventory_path.read_text())
    if len(plans) != len(inventory):
        raise RuntimeError("plan/inventory cardinality mismatch")
    rows = []
    for plan, item in zip(plans, inventory, strict=True):
        choice = plan["choices"][0]
        rows.append(
            {
                "candidate_id": plan["candidate_id"],
                "plan": plan,
                "root": int(choice["root_anchor"]),
                "region": frozenset(map(int, item["region"])),
                "boundary": frozenset(expression_anchors(choice["expression"])),
            }
        )
    return rows


def compatible(rows: list[dict]) -> bool:
    for left, right in combinations(rows, 2):
        if left["root"] == right["root"]:
            return False
        if left["region"] & right["region"]:
            return False
        if left["boundary"] & (right["region"] - {right["root"]}):
            return False
        if right["boundary"] & (left["region"] - {left["root"]}):
            return False
    return True


def emit(candidate_id: str, members: list[dict], provenance: list[str]) -> dict:
    members = sorted(members, key=lambda row: row["root"])
    return {
        "candidate_id": candidate_id,
        "provenance": provenance,
        "choices": [row["plan"]["choices"][0] for row in members],
        "proof_boundary": [],
        "proof_outputs": [],
        "proof_groups": [
            {
                "choices": row["plan"]["choices"],
                "boundary": row["plan"]["proof_boundary"],
                "outputs": row["plan"]["proof_outputs"],
            }
            for row in members
        ],
        "_members": members,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("pair", "recourse"), required=True)
    parser.add_argument("--primitive-plan", type=Path, required=True)
    parser.add_argument("--primitive-inventory", type=Path, required=True)
    parser.add_argument("--external-status", type=Path, required=True)
    parser.add_argument("--external-receipt", type=Path, required=True)
    parser.add_argument("--seed-plan", type=Path)
    parser.add_argument("--pool-roots", type=int, default=12)
    parser.add_argument("--seed-count", type=int, default=3)
    parser.add_argument("--limit", type=int, default=64)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    primitives = member_rows(args.primitive_plan, args.primitive_inventory)
    by_id = {row["candidate_id"]: row for row in primitives}
    score, _ = load_external(args.external_status, args.external_receipt)
    missing = sorted(set(by_id) - set(score))
    if missing:
        raise RuntimeError(f"external scores missing fixed primitives: {missing}")

    best_by_root: dict[int, dict] = {}
    for row in primitives:
        old = best_by_root.get(row["root"])
        if old is None or (score[row["candidate_id"]], row["candidate_id"]) < (
            score[old["candidate_id"]], old["candidate_id"]
        ):
            best_by_root[row["root"]] = row
    pool = sorted(
        best_by_root.values(),
        key=lambda row: (score[row["candidate_id"]], row["candidate_id"]),
    )[: args.pool_roots]

    proposals: list[dict] = []
    if args.mode == "pair":
        for left, right in combinations(pool, 2):
            if not compatible([left, right]):
                continue
            proposals.append(
                emit(
                    "",
                    [left, right],
                    [
                        "measured-fixed-family-pair-oracle-v1",
                        f"singleton-product:{score[left['candidate_id']] * score[right['candidate_id']]:.12f}",
                    ],
                )
            )
        proposals.sort(
            key=lambda plan: (
                score[plan["_members"][0]["candidate_id"]]
                * score[plan["_members"][1]["candidate_id"]],
                plan_signature(plan),
            )
        )
    else:
        if args.seed_plan is None:
            raise RuntimeError("--seed-plan is required in recourse mode")
        seeds = json.loads(args.seed_plan.read_text())
        seed_scores = [(score.get(plan["candidate_id"]), plan) for plan in seeds]
        if any(value is None for value, _ in seed_scores):
            missing_seeds = [plan["candidate_id"] for value, plan in seed_scores if value is None]
            raise RuntimeError(f"external scores missing seed moves: {missing_seeds}")
        for _, seed in sorted(seed_scores, key=lambda item: (item[0], item[1]["candidate_id"]))[: args.seed_count]:
            seed_members = [
                by_id[
                    next(
                        row["candidate_id"]
                        for row in primitives
                        if row["plan"]["choices"][0] == choice
                    )
                ]
                for choice in seed["choices"]
            ]
            # One-site addition around the measured pair.
            for candidate in pool:
                members = seed_members + [candidate]
                if candidate in seed_members or not compatible(members):
                    continue
                proposals.append(
                    emit(
                        "",
                        members,
                        [
                            "measured-fixed-family-coordinate-recourse-v1",
                            f"seed:{seed['candidate_id']}",
                            f"move:add:{candidate['candidate_id']}",
                        ],
                    )
                )
            # One-site replacement of either pair member.
            for removed in seed_members:
                remainder = [row for row in seed_members if row is not removed]
                for candidate in pool:
                    members = remainder + [candidate]
                    if candidate in remainder or not compatible(members):
                        continue
                    proposals.append(
                        emit(
                            "",
                            members,
                            [
                                "measured-fixed-family-coordinate-recourse-v1",
                                f"seed:{seed['candidate_id']}",
                                f"move:swap:{removed['candidate_id']}->{candidate['candidate_id']}",
                            ],
                        )
                    )

    unique: dict[tuple[str, ...], dict] = {}
    for plan in proposals:
        unique.setdefault(plan_signature(plan), plan)
    plans = list(unique.values())[: args.limit]
    prefix = "PAIR_ORACLE" if args.mode == "pair" else "RECOURSE_ORACLE"
    receipts = []
    for index, plan in enumerate(plans):
        plan["candidate_id"] = f"{prefix}_{index:03d}_{len(plan['_members'])}CUTS"
        receipts.append(
            {
                "candidate_id": plan["candidate_id"],
                "members": [row["candidate_id"] for row in plan["_members"]],
                "roots": [row["root"] for row in plan["_members"]],
                "provenance": plan["provenance"],
            }
        )
        del plan["_members"]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(plans, indent=2, sort_keys=True) + "\n")
    args.output.with_name(args.output.stem + "_receipt.json").write_text(
        json.dumps(
            {
                "schema": "egg-measured-fusion-moves-v1",
                "mode": args.mode,
                "development_oracle": True,
                "primitive_family_fixed_before_external": True,
                "pool_roots": len(pool),
                "proposal_count": len(plans),
                "proposals": receipts,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n"
    )
    print(json.dumps({"mode": args.mode, "emitted": len(plans)}))


if __name__ == "__main__":
    main()

"""Bounded promotion ablation: fresh V1 proposals, whole-port dominance gate.

No results, candidate IDs, root locations or fitted regression margins are inputs.
V1 small-move nominees remain independently generated, not historical netlists.
"""
import math
from conquer_defense_policy_v1 import finalists as baseline_finalists


def exceeds(value, reference):
    return value > reference + 64 * math.ulp(max(abs(reference), 1.0))


def boundary_violations(row, g0):
    failures = []
    for kind, fields in (("outputs", ("arrival_rise_ps", "arrival_fall_ps", "slew_rise_ps", "slew_fall_ps")),
                         ("inputs", ("capacitance_ff",))):
        current = {r["name"]: r for r in row["boundary_response"][kind]}
        reference = {r["name"]: r for r in g0["boundary_response"][kind]}
        if current.keys() != reference.keys():
            raise ValueError("whole-port identity mismatch")
        for name in sorted(current):
            for field in fields:
                v, r = current[name][field], reference[name][field]
                if not math.isfinite(v) or not math.isfinite(r):
                    raise ValueError("nonfinite boundary state")
                if exceeds(v, r):
                    failures.append({"port": name, "field": field, "delta": v-r,
                                     "relative": (v-r)/max(abs(r), 1e-30)})
    return failures


def promotion_audit(profiles, membership):
    by_id = {p["candidate_id"]: p for p in profiles}
    g0 = by_id["G0"]
    audit = []
    for row in profiles:
        cid = row["candidate_id"]
        violations = boundary_violations(row, g0)
        members = membership.get(cid, [])
        combination = len(members) >= 2
        reasons = []
        if combination:
            reasons += ["whole_"+key for key in ("delay_ps", "area", "power") if exceeds(row[key], g0[key])]
            if violations:
                reasons.append("whole_port_regression")
            if row["d2ap"] >= min(by_id[m["candidate_id"]]["d2ap"] for m in members):
                reasons.append("no_increment_over_best_member")
        audit.append({"candidate_id": cid, "combination": combination,
                      "admitted": not reasons, "reasons": reasons,
                      "boundary_violation_count": len(violations),
                      "max_relative_boundary_regression": max((v["relative"] for v in violations), default=0),
                      "boundary_violations": violations})
    return audit


def finalists(profiles, paths, digest, membership, limit=7):
    audit = promotion_audit(profiles, membership)
    states = {a["candidate_id"]: a for a in audit}
    selected, used = [], {digest(paths["G0"])}
    def retain(row):
        sha = digest(paths[row["candidate_id"]])
        if len(selected) < limit and sha not in used and states[row["candidate_id"]]["admitted"]:
            selected.append(row)
            used.add(sha)
    # Preserve independently regenerated small-move slots, including normal pre/post.
    for row in baseline_finalists(profiles, paths, digest, limit):
        retain(row)
    # Replace rejected combination slots without expanding any proposal budget.
    eligible = [p for p in profiles if p["candidate_id"] != "G0" and states[p["candidate_id"]]["admitted"]]
    orders = [sorted(eligible, key=lambda p: (states[p["candidate_id"]]["max_relative_boundary_regression"],
                                            states[p["candidate_id"]]["boundary_violation_count"], p["d2ap"], p["candidate_id"])),
              sorted(eligible, key=lambda p: (p["delay_ps"], p["candidate_id"])),
              sorted(eligible, key=lambda p: (p["d2ap"], p["candidate_id"]))]
    for rank in range(max(map(len, orders), default=0)):
        for order in orders:
            if rank < len(order):
                retain(order[rank])
    return selected, audit

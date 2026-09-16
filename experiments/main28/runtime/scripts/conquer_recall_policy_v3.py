"""Data-independent effective source recall and measured closure recourse.

The policy consumes only the current G0 state, freshly mined proof-carrying
plans, and whole-profile Internal measurements. It contains no benchmark names,
root IDs, candidate IDs from prior runs, external scores, or fitted winner data.
"""
from collections import defaultdict
from itertools import combinations
import json
import math
import re

from build_measured_fusion_moves_v1 import compatible
from conquer_defense_policy_v1 import interleave, select_sources


def _choice_key(row):
    return json.dumps(row[0]["choices"], sort_keys=True)


def _cell_family(cell):
    stem = cell.split("_ASAP", 1)[0]
    return re.sub(r"x(?:p?\d+(?:r|f|b|q)?|\d+(?:r|f|b|q)?)$", "", stem)


def _bucket_round_robin(items, key, row_key):
    groups = defaultdict(list)
    for item in items:
        groups[key(item)].append(item)
    orders = [sorted(rows, key=row_key) for _, rows in sorted(groups.items(), key=lambda x: str(x[0]))]
    return interleave(orders, len(items), lambda x: x)


def select_sources_v3(plans, inventory, state, legacy_root_cap, legacy_state_cap,
                      recall_root_cap, recall_state_cap):
    """Keep the complete legacy lane, then add feature-diverse roots/states."""
    legacy = select_sources(plans, inventory, legacy_root_cap, legacy_state_cap)
    grouped = defaultdict(list)
    for plan, inv in zip(plans, inventory, strict=True):
        grouped[int(inv["root"])].append((plan, inv))
    nodes = {int(n["anchor"]): n for n in state["occurrences"]}
    roots = list(grouped)
    saving = lambda root: max(row[1]["area_saving"] for row in grouped[root])
    slack = lambda root: min(row[1]["slack_ps"] for row in grouped[root])
    boundary = lambda root: min(len(row[1]["boundary"]) for row in grouped[root])
    region = lambda root: max(len(row[1]["region"]) for row in grouped[root])
    fanout = lambda root: len(nodes[root]["consumers"])
    truth = lambda root: min(row[1]["truth_hex"] for row in grouped[root])
    family = lambda root: min(_cell_family(row[1]["cell"]) for row in grouped[root])
    common = lambda root: (-saving(root), slack(root), boundary(root), root)
    orders = [
        sorted(roots, key=lambda r: (-saving(r), r)),
        sorted(roots, key=lambda r: (slack(r), -saving(r), r)),
        sorted(roots, key=lambda r: (-slack(r), -saving(r), r)),
        sorted(roots, key=lambda r: (boundary(r), -saving(r), r)),
        sorted(roots, key=lambda r: (-region(r), boundary(r), r)),
        sorted(roots, key=lambda r: (-fanout(r), boundary(r), r)),
        _bucket_round_robin(roots, truth, common),
        _bucket_round_robin(roots, family, common),
    ]
    legacy_roots = []
    for _, inv in legacy:
        if int(inv["root"]) not in legacy_roots:
            legacy_roots.append(int(inv["root"]))
    selected_roots = list(legacy_roots)
    for root in interleave(orders, len(roots), lambda x: x):
        if root not in selected_roots:
            selected_roots.append(root)
        if len(selected_roots) == min(recall_root_cap, len(roots)):
            break
    legacy_by_root = defaultdict(list)
    for row in legacy:
        legacy_by_root[int(row[1]["root"])].append(row)
    selected, audit = [], []
    for root in selected_roots:
        rows = grouped[root]
        state_orders = [
            sorted(rows, key=lambda x: (x[1]["target_area"], x[0]["candidate_id"])),
            sorted(rows, key=lambda x: (-x[1]["target_area"], x[0]["candidate_id"])),
            sorted(rows, key=lambda x: (len(x[1]["region"]), tuple(x[1]["pin_order"]), x[0]["candidate_id"])),
            sorted(rows, key=lambda x: (-len(x[1]["region"]), tuple(x[1]["pin_order"]), x[0]["candidate_id"])),
            sorted(rows, key=lambda x: (tuple(x[1]["pin_order"]), x[1]["cell"], x[0]["candidate_id"])),
            sorted(rows, key=lambda x: (tuple(reversed(x[1]["pin_order"])), x[1]["cell"], x[0]["candidate_id"])),
        ]
        choices, seen = [], set()
        for row in legacy_by_root[root] + interleave(state_orders, len(rows), _choice_key):
            key = _choice_key(row)
            if key not in seen:
                seen.add(key)
                choices.append(row)
            if len(choices) == recall_state_cap:
                break
        selected += choices
        audit.append({"root": root, "legacy_root": root in legacy_roots,
                      "raw_states": len({_choice_key(x) for x in rows}),
                      "effective_states": len(choices),
                      "families": sorted({_cell_family(x[1]["cell"]) for x in choices}),
                      "truths": sorted({x[1]["truth_hex"] for x in choices})})
    return legacy, selected, audit


def _response_delta(row, g0, fields=None):
    if fields is None:
        fields = {"outputs": ("arrival_rise_ps", "arrival_fall_ps", "slew_rise_ps", "slew_fall_ps"),
                  "inputs": ("capacitance_ff",)}
    values = []
    for kind, names in fields.items():
        current = {x["name"]: x for x in row["boundary_response"][kind]}
        base = {x["name"]: x for x in g0["boundary_response"][kind]}
        if current.keys() != base.keys():
            raise ValueError("whole-port identity mismatch")
        for name in sorted(base):
            for field in names:
                values.append((current[name][field] - base[name][field]) / max(abs(base[name][field]), 1.0))
    return values


def boundary_risk(row, g0, arrival_only=False):
    fields = {"outputs": ("arrival_rise_ps", "arrival_fall_ps")} if arrival_only else None
    delta = _response_delta(row, g0, fields)
    return max([0.0] + delta)


def _distinct_root_pool(members, score, limit):
    orders = []
    g0 = score["G0"]
    metrics = ("d2ap", "delay_ps", "area", "power")
    for metric in metrics:
        rows, roots = [], set()
        for member in sorted(members, key=lambda m: (score[m["candidate_id"]][metric], m["candidate_id"])):
            if member["root"] not in roots:
                roots.add(member["root"])
                rows.append(member)
        orders.append(rows)
    risk = sorted(members, key=lambda m: (boundary_risk(score[m["candidate_id"]], g0),
                                          score[m["candidate_id"]]["d2ap"], m["candidate_id"]))
    rows, roots = [], set()
    for member in risk:
        if member["root"] not in roots:
            roots.add(member["root"]); rows.append(member)
    orders.append(rows)
    return interleave(orders, limit, lambda m: m["candidate_id"])


def _aggregate_risk(rows, score):
    g0 = score["G0"]
    deltas = [_response_delta(score[m["candidate_id"]], g0) for m in rows]
    return max([0.0] + [sum(column) for column in zip(*deltas)]) if deltas else 0.0


def recall_pair_moves(members, profiles, endpoints, limit, exclude=()):
    score = {p["candidate_id"]: p for p in profiles}
    pool = _distinct_root_pool(members, score, 72)
    excluded = {tuple(sorted(m["candidate_id"] for m in rows)) for rows in exclude}
    pairs = [list(pair) for pair in combinations(pool, 2) if compatible(pair)
             and tuple(sorted(m["candidate_id"] for m in pair)) not in excluded]
    def product(rows):
        return math.prod(score[m["candidate_id"]]["d2ap"] / score["G0"]["d2ap"] for m in rows)
    def overlap(rows):
        left, right = (endpoints[m["root"]] for m in rows)
        return len(left & right) / max(1, len(left | right))
    orders = [sorted(pairs, key=lambda x: (product(x), _aggregate_risk(x, score))),
              sorted(pairs, key=lambda x: (_aggregate_risk(x, score), product(x))),
              sorted(pairs, key=lambda x: (-overlap(x), _aggregate_risk(x, score))),
              sorted(pairs, key=lambda x: (overlap(x), product(x)))]
    return interleave(orders, limit, lambda rows: tuple(sorted(m["candidate_id"] for m in rows)))


def initial_closure_moves(members, pair_members, profiles, endpoints, state, limit, max_sites):
    score = {p["candidate_id"]: p for p in profiles}
    g0 = score["G0"]
    pool = _distinct_root_pool(members, score, 160)
    orders = [sorted(pool, key=lambda m: (score[m["candidate_id"]][metric],
                                          boundary_risk(score[m["candidate_id"]], g0), m["candidate_id"]))
              for metric in ("d2ap", "delay_ps", "area", "power")]
    orders.append(sorted(pool, key=lambda m: (boundary_risk(score[m["candidate_id"]], g0),
                                               score[m["candidate_id"]]["d2ap"], m["candidate_id"])))
    timing = {int(x["anchor"]): x for x in state["anchors"]}
    nodes = {int(x["anchor"]): x for x in state["occurrences"]}
    output_roots = sorted((a for a, n in nodes.items() if n["is_root"]),
                          key=lambda a: (min(timing[a]["slack_rise_ps"], timing[a]["slack_fall_ps"]), a))[:8]
    for output in output_roots:
        lane = [m for m in pool if output in endpoints[m["root"]]]
        if lane:
            orders.append(sorted(lane, key=lambda m: (score[m["candidate_id"]]["d2ap"],
                                                       boundary_risk(score[m["candidate_id"]], g0), m["candidate_id"])))
    seeds = [[]] + [rows for _, rows in sorted(pair_members.items(),
                                               key=lambda item: score[item[0]]["d2ap"])[:4]]
    candidates = []
    sizes = sorted({min(max_sites, n) for n in (6, 10, 14, 18, max_sites) if min(max_sites, n) >= 3})
    for order in orders:
        for size in sizes:
            for seed in seeds:
                selected = list(seed)
                for member in order:
                    if len(selected) == size:
                        break
                    if member not in selected and compatible(selected + [member]):
                        selected.append(member)
                if len(selected) >= 3:
                    candidates.append(selected)
    unique = []
    seen = set()
    for rows in candidates:
        key = tuple(sorted(m["candidate_id"] for m in rows))
        if key not in seen:
            seen.add(key); unique.append(rows)
    rank_orders = [sorted(unique, key=lambda rows: (math.prod(score[m["candidate_id"]]["d2ap"] / g0["d2ap"] for m in rows),
                                                    _aggregate_risk(rows, score), -len(rows))),
                   sorted(unique, key=lambda rows: (_aggregate_risk(rows, score), -len(rows))),
                   sorted(unique, key=lambda rows: (-len(rows), _aggregate_risk(rows, score)))]
    return interleave(rank_orders, limit, lambda rows: tuple(sorted(m["candidate_id"] for m in rows)))


def recourse_moves(members, seed_members, profiles, limit, max_sites):
    score = {p["candidate_id"]: p for p in profiles}
    g0 = score["G0"]
    seeds = list(seed_members.items())
    seed_orders = [sorted(seeds, key=lambda x: (score[x[0]][metric], x[0]))
                   for metric in ("d2ap", "delay_ps", "area", "power")]
    seed_orders.append(sorted(seeds, key=lambda x: (boundary_risk(score[x[0]], g0), score[x[0]]["d2ap"], x[0])))
    selected_seeds = interleave(seed_orders, 10, lambda x: x[0])
    pool = _distinct_root_pool(members, score, 80)
    neighbors = []
    for _, seed in selected_seeds:
        if len(seed) > 3:
            neighbors += [[m for m in seed if m is not removed] for removed in seed]
        if len(seed) < max_sites:
            for added in pool[:40]:
                if added not in seed and compatible(seed + [added]):
                    neighbors.append(seed + [added])
        harmful = sorted(seed, key=lambda m: (boundary_risk(score[m["candidate_id"]], g0),
                                               score[m["candidate_id"]]["d2ap"]), reverse=True)[:4]
        for removed in harmful:
            base = [m for m in seed if m is not removed]
            for added in pool[:40]:
                if added not in base and compatible(base + [added]):
                    neighbors.append(base + [added])
    unique, seen = [], set()
    for rows in neighbors:
        key = tuple(sorted(m["candidate_id"] for m in rows))
        if len(rows) >= 3 and key not in seen:
            seen.add(key); unique.append(rows)
    def product(rows):
        return math.prod(score[m["candidate_id"]]["d2ap"] / g0["d2ap"] for m in rows)
    orders = [sorted(unique, key=lambda x: (product(x), _aggregate_risk(x, score))),
              sorted(unique, key=lambda x: (_aggregate_risk(x, score), product(x))),
              sorted(unique, key=lambda x: (-len(x), _aggregate_risk(x, score)))]
    return interleave(orders, limit, lambda rows: tuple(sorted(m["candidate_id"] for m in rows)))


def recall_finalists(defense, profiles, paths, digest, membership, total_limit, defense_quota):
    by_id = {p["candidate_id"]: p for p in profiles}
    g0 = by_id["G0"]
    selected, used = [], {digest(paths["G0"])}
    def retain(row, lane):
        sha = digest(paths[row["candidate_id"]])
        if len(selected) < total_limit and sha not in used:
            selected.append((row, lane)); used.add(sha)
    for row in defense[:defense_quota]:
        retain(row, "v2-defense")
    recall = [p for p in profiles if p["candidate_id"].startswith("RECALL_")
              and len(membership.get(p["candidate_id"], [])) >= 2]
    strict = [p for p in recall if p["delay_ps"] <= g0["delay_ps"] and p["area"] <= g0["area"]
              and p["power"] <= g0["power"] and boundary_risk(p, g0) <= 64*math.ulp(1.0)]
    arrival = [p for p in recall if p["delay_ps"] <= g0["delay_ps"] and p["d2ap"] < g0["d2ap"]
               and boundary_risk(p, g0, arrival_only=True) <= 64*math.ulp(1.0)]
    orders = [sorted(strict, key=lambda p: (p["d2ap"], p["candidate_id"])),
              sorted(arrival, key=lambda p: (p["d2ap"], p["candidate_id"])),
              sorted(recall, key=lambda p: (boundary_risk(p, g0), p["d2ap"], p["candidate_id"])),
              sorted(recall, key=lambda p: (p["delay_ps"], p["candidate_id"])),
              sorted(recall, key=lambda p: (p["d2ap"], p["candidate_id"]))]
    for row in interleave(orders, len(recall), lambda p: p["candidate_id"]):
        retain(row, "v3-recall")
        if len(selected) == total_limit:
            break
    audit = [{"candidate_id": row["candidate_id"], "lane": lane,
              "member_count": len(membership.get(row["candidate_id"], [])),
              "boundary_risk": boundary_risk(row, g0)} for row, lane in selected]
    return [row for row, _ in selected], audit

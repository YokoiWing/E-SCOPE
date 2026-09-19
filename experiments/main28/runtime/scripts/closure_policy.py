"""G0/Internal-only, bounded occurrence-state and measured-combination policy."""
from collections import defaultdict
from functools import lru_cache
from itertools import combinations
import json
import math

from fusion_moves import compatible, emit


def interleave(orders, limit, key):
    result, seen = [], set()
    for rank in range(max(map(len, orders), default=0)):
        for order in orders:
            if rank < len(order) and key(order[rank]) not in seen:
                row = order[rank]
                seen.add(key(row))
                result.append(row)
                if len(result) == limit:
                    return result
    return result


def select_sources(plans, inventory, root_cap, state_cap):
    grouped = defaultdict(list)
    for plan, inv in zip(plans, inventory, strict=True):
        grouped[inv["root"]].append((plan, inv))
    roots = list(grouped)
    orders = [sorted(roots, key=lambda r: (-max(x[1]["area_saving"] for x in grouped[r]), r)),
              sorted(roots, key=lambda r: (-max(x[1]["slack_ps"] for x in grouped[r]), r)),
              sorted(roots, key=lambda r: (min(len(x[1]["boundary"]) for x in grouped[r]), -max(x[1]["area_saving"] for x in grouped[r]), r))]
    selected = []
    for root in interleave(orders, root_cap, lambda x: x):
        rows = sorted(grouped[root], key=lambda x: (x[1]["target_area"], x[0]["candidate_id"]))
        # Different pin assignments and a drive alternative are distinct states.
        choices = interleave([rows, sorted(rows, key=lambda x: (tuple(x[1]["pin_order"]), x[1]["target_area"])),
                              sorted(rows, key=lambda x: (-x[1]["target_area"], x[0]["candidate_id"]))], state_cap,
                             lambda x: json.dumps(x[0]["choices"], sort_keys=True))
        selected += choices
    return selected


def endpoint_sets(state):
    nodes = {int(n["anchor"]): n for n in state["occurrences"]}
    @lru_cache(None)
    def reachable(anchor):
        row = nodes[anchor]
        own = frozenset([anchor]) if row.get("is_root") else frozenset()
        return own.union(*(reachable(int(c)) for c in row["consumers"]))
    return {anchor: reachable(anchor) for anchor in nodes}


def endpoint_sets_iterative(state):
    """Same endpoint identities without Python recursion on very deep DAGs."""
    from collections import deque
    nodes = {int(n["anchor"]): n for n in state["occurrences"]}
    children = {a: set(map(int, n["consumers"])) for a, n in nodes.items()}
    parents = defaultdict(list)
    pending = {a: len(cs) for a, cs in children.items()}
    for a, cs in children.items():
        for c in cs:
            if c not in nodes:
                raise ValueError("missing consumer anchor")
            parents[c].append(a)
    ready = deque(a for a, count in pending.items() if count == 0)
    result = {}
    while ready:
        a = ready.popleft()
        own = frozenset([a]) if nodes[a].get("is_root") else frozenset()
        result[a] = own.union(*(result[c] for c in children[a]))
        for p in parents[a]:
            pending[p] -= 1
            if pending[p] == 0:
                ready.append(p)
    if len(result) != len(nodes):
        raise ValueError("cycle in endpoint graph")
    return {a: result[a] for a in nodes}


def make_plan(cid, rows):
    assert compatible(rows)
    plan = emit(cid, rows, ["autonomous-g0-internal-only", "simultaneous-compatible-occurrences"])
    plan.pop("_members")
    return plan


def pair_moves(members, profiles, endpoints, limit):
    score = {p["candidate_id"]: p for p in profiles}
    pools = []
    for metric in ("d2ap", "delay_ps", "area", "power"):
        ordered = sorted(members, key=lambda m: (score[m["candidate_id"]][metric], m["candidate_id"]))
        seen, pool = set(), []
        for member in ordered:
            if member["root"] not in seen:
                seen.add(member["root"])
                pool.append(member)
            if len(pool) == 12:
                break
        pools.append(pool)
    pool = interleave(pools, 24, lambda m: m["candidate_id"])
    pairs = [list(p) for p in combinations(pool, 2) if compatible(p)]
    def estimate(p):
        return math.prod(score[m["candidate_id"]]["d2ap"] for m in p)
    def overlap(p):
        a, b = (endpoints[m["root"]] for m in p)
        return len(a & b) / max(1, len(a | b))
    orders = [sorted(pairs, key=estimate), sorted(pairs, key=lambda p: (-overlap(p), estimate(p))),
              sorted(pairs, key=lambda p: (overlap(p), estimate(p)))]
    return interleave(orders, limit, lambda p: tuple(m["candidate_id"] for m in p))


def closure_moves(members, pair_members, profiles, limit, max_sites):
    score = {p["candidate_id"]: p for p in profiles}
    g0 = score["G0"]
    useful = [m for m in members if score[m["candidate_id"]]["d2ap"] < g0["d2ap"] and score[m["candidate_id"]]["delay_ps"] <= g0["delay_ps"] * 1.0005]
    seeds = [[]] + [rows for cid, rows in sorted(pair_members.items(), key=lambda item: score[item[0]]["d2ap"])[:3]]
    result, seen = [], set()
    for metric in ("d2ap", "area", "power"):
        for seed in seeds:
            selected = list(seed)
            for m in sorted(useful, key=lambda m: (score[m["candidate_id"]][metric], m["candidate_id"])):
                if len(selected) < max_sites and compatible(selected + [m]):
                    selected.append(m)
            key = tuple(sorted(m["candidate_id"] for m in selected))
            if len(selected) >= 3 and key not in seen:
                seen.add(key)
                result.append(selected)
            if len(result) == limit:
                return result
    return result


def finalists(profiles, paths, digest, limit=7):
    by_id = {p["candidate_id"]: p for p in profiles}
    selected, used = [], {digest(paths["G0"])}
    def retain(row):
        sha = digest(paths[row["candidate_id"]])
        if sha not in used and len(selected) < limit:
            selected.append(row)
            used.add(sha)
    for cid in ("NORMAL_PRE", "NORMAL_POST"):
        if cid in by_id:
            retain(by_id[cid])
    for prefix in ("PI_IDENTITY_", "SIN_GENERIC_", "PAIR_", "CLOSURE_"):
        pool = [p for p in profiles if p["candidate_id"].startswith(prefix)]
        if pool:
            retain(min(pool, key=lambda p: (p["d2ap"], p["candidate_id"])))
    g0 = by_id["G0"]
    eligible = [p for p in profiles if p["candidate_id"] != "G0" and p["delay_ps"] <= g0["delay_ps"] * 1.0025]
    for metric in ("delay_ps", "d2ap", "area", "power"):
        for row in sorted(eligible, key=lambda p: (p[metric], p["candidate_id"])):
            retain(row)
            if len(selected) == limit:
                return selected
    return selected

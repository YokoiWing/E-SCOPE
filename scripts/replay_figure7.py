#!/usr/bin/env python3
"""Recompute Figure 7 fronts, hypervolume claims, and a portable plot."""

import argparse
import csv
import json
import math
import os
from collections import Counter
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "evidence/figure7/epfl_adder_three_objectives_original_precision.csv"
RECEIPT = ROOT / "evidence/figure7/source_receipt.json"
OBJECTIVES = ("D×A", "D²AP", "D×P²")


def pareto_indices(points, y_key):
    indices = set()
    for i, left in enumerate(points):
        lx, ly = float(left["boundary_delay_ps"]), float(left[y_key])
        dominated = False
        for j, right in enumerate(points):
            if i == j:
                continue
            rx, ry = float(right["boundary_delay_ps"]), float(right[y_key])
            if rx <= lx and ry <= ly and (rx < lx or ry < ly):
                dominated = True
                break
        if not dominated:
            indices.add(i)
    return indices


def hv2(points, reference):
    points = [p for p in points if p[0] < reference[0] and p[1] < reference[1]]
    ys = sorted({p[0] for p in points})
    area = 0.0
    for index, y in enumerate(ys):
        next_y = ys[index + 1] if index + 1 < len(ys) else reference[0]
        z = min(p[1] for p in points if p[0] <= y)
        area += (next_y - y) * (reference[1] - z)
    return area


def hypervolume(points, scales, reference=(1.1, 1.1, 1.1)):
    normalized = [[p[i] / scales[i] for i in range(3)] for p in points]
    normalized = [p for p in normalized if all(p[i] < reference[i] for i in range(3))]
    xs = sorted({p[0] for p in normalized})
    volume = 0.0
    for index, x in enumerate(xs):
        next_x = xs[index + 1] if index + 1 < len(xs) else reference[0]
        active = [(p[1], p[2]) for p in normalized if p[0] <= x]
        volume += (next_x - x) * hv2(active, reference[1:])
    return volume


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path, default=ROOT / "reproduced/figure7")
    parser.add_argument("--no-plot", action="store_true")
    args = parser.parse_args()
    rows = list(csv.DictReader(DATA.open()))
    receipt = json.loads(RECEIPT.read_text())
    counts = Counter(row["objective"] for row in rows if row["series"] == "Iterative")
    if len(rows) != 283 or sum(row["series"] == "Genus G0" for row in rows) != 20:
        raise RuntimeError("unexpected Figure 7 row counts")
    if any(row["formal"] != "PASS" for row in rows):
        raise RuntimeError("Figure 7 contains a non-PASS formal record")
    expected_counts = {key: value["frozen_candidates"] for key, value in receipt["objectives"].items()}
    if dict(counts) != expected_counts:
        raise RuntimeError(f"objective counts differ: {dict(counts)}")

    g0 = [row for row in rows if row["series"] == "Genus G0"]
    candidates = {objective: [row for row in rows if row["objective"] == objective] for objective in OBJECTIVES}
    xyz = lambda row: [float(row["boundary_delay_ps"]), float(row["area_um2"]), float(row["power_uw"])]
    scales = [max(xyz(row)[i] for row in g0) for i in range(3)]
    baseline_hv = hypervolume([xyz(row) for row in g0], scales)
    d2ap_hv = hypervolume([xyz(row) for row in g0 + candidates["D²AP"]], scales)
    union_hv = hypervolume([xyz(row) for row in rows], scales)
    min_power_850 = {
        objective: min(float(row["power_uw"]) for row in pool if float(row["boundary_delay_ps"]) <= 850)
        for objective, pool in candidates.items()
    }
    result = {
        "status": "PASS",
        "g0_points": len(g0),
        "candidate_counts": dict(counts),
        "formal_pass": len(rows),
        "minimum_power_uw_at_delay_le_850_ps": min_power_850,
        "d2ap_hv_increase_over_genus_percent": 100 * (d2ap_hv / baseline_hv - 1),
        "three_objective_union_hv_increase_over_genus_percent": 100 * (union_hv / baseline_hv - 1),
        "union_hv_increase_over_d2ap_percent": 100 * (union_hv / d2ap_hv - 1),
        "classification": "saved-evidence replay",
    }
    args.output_dir.mkdir(parents=True, exist_ok=True)
    if not args.no_plot:
        os.environ.setdefault("MPLCONFIGDIR", str(args.output_dir / ".mpl"))
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        colors = {"D×A": "#d95f02", "D²AP": "#7557a6", "D×P²": "#009e73"}
        markers = {"D×A": "D", "D²AP": "P", "D×P²": "^"}
        fig, axes = plt.subplots(1, 2, figsize=(10.5, 4.2), layout="constrained")
        for ax, y_key, ylabel in ((axes[0], "area_um2", "Area (µm²)"), (axes[1], "power_uw", "Power (µW)")):
            ax.scatter([float(r["boundary_delay_ps"]) for r in g0], [float(r[y_key]) for r in g0], label="High-effort Genus", s=25)
            for objective in OBJECTIVES:
                pool = candidates[objective]
                front = [pool[i] for i in pareto_indices(pool, y_key)]
                front.sort(key=lambda row: (float(row["boundary_delay_ps"]), float(row[y_key])))
                ax.scatter([float(r["boundary_delay_ps"]) for r in pool], [float(r[y_key]) for r in pool], s=12, alpha=0.25, color=colors[objective], marker=markers[objective])
                ax.plot([float(r["boundary_delay_ps"]) for r in front], [float(r[y_key]) for r in front], label=objective, color=colors[objective], marker=markers[objective], markersize=4)
            ax.set(xlabel="Delay (ps)", ylabel=ylabel)
            ax.grid(True, linestyle="--", alpha=0.35)
        axes[0].legend(frameon=False, fontsize=8)
        fig.savefig(args.output_dir / "figure7_pareto.png", dpi=180)
        fig.savefig(args.output_dir / "figure7_pareto.svg")
        plt.close(fig)
    (args.output_dir / "replay.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

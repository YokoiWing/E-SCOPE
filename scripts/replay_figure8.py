#!/usr/bin/env python3
"""Recompute Figure 8 statistics and optionally redraw the ablation chart."""

from __future__ import annotations

import argparse
import csv
import json
import math
import os
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
EVIDENCE = REPO / "evidence/figure8/selected_flow_ablation_original_precision.csv"
REFERENCE = REPO / "evidence/figure8/replay_report.json"


def gm(values):
    return math.exp(sum(math.log(value) for value in values) / len(values))


def recompute(rows):
    phase_i = gm([float(row["full_over_phase_i_only"]) for row in rows])
    phase_ii = gm([float(row["full_over_phase_ii_only_exact"]) for row in rows])
    no_drive = gm([float(row["full_over_without_drive"]) for row in rows])
    return {
        "full_over_phase_i_only": phase_i,
        "full_reduction_vs_phase_i_only_percent": 100 * (1 - phase_i),
        "full_over_phase_ii_only_exact": phase_ii,
        "full_reduction_vs_phase_ii_only_percent": 100 * (1 - phase_ii),
        "full_over_without_drive": no_drive,
        "full_reduction_vs_without_drive_percent": 100 * (1 - no_drive),
    }


def plot(rows, output_dir, paper_values):
    os.environ.setdefault("MPLCONFIGDIR", "/tmp/e-scope-figure8-mpl")
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    import numpy as np

    output_dir.mkdir(parents=True, exist_ok=True)
    x = np.arange(len(rows), dtype=float) * 0.82
    width = 0.21
    phase_ii_key = (
        "figure8_phase_ii_bar_value"
        if paper_values
        else "full_over_phase_ii_only_exact"
    )
    fig, ax = plt.subplots(figsize=(16.2, 4.75))
    fig.subplots_adjust(left=0.075, right=0.992, bottom=0.34, top=0.90)
    series = [
        ("full_over_phase_i_only", "Without Phase-II sizing", "#7CC9F0", -width),
        (phase_ii_key, "Without Phase-I exploration", "#FFB870", 0.0),
        ("full_over_without_drive", "Logic only in Phase I", "#8ED97B", width),
    ]
    handles = []
    for key, label, color, offset in series:
        handles.append(
            ax.bar(
                x + offset,
                [float(row[key]) for row in rows],
                width=width,
                label=label,
                color=color,
                edgecolor="#646A73",
                linewidth=0.72,
            )
        )
    baseline = ax.axhline(1.0, color="#9A0000", linewidth=3, linestyle="--")
    ax.set_xlim(-0.55, x[-1] + 0.55)
    ax.set_ylim(0.5, 1.16)
    ax.set_ylabel(r"Full $D^2AP$ / ablated $D^2AP$")
    ax.set_xticks(x)
    ax.set_xticklabels(
        [row["benchmark"].removeprefix("epfl_") for row in rows],
        rotation=30,
        ha="right",
    )
    ax.set_yticks(np.arange(0.5, 1.11, 0.1))
    ax.yaxis.grid(True, color="#D9DDE3", linewidth=0.65, linestyle="--")
    ax.spines[["top", "right"]].set_visible(False)
    ax.legend(
        [baseline, *handles],
        ["Full E-SCOPE", *[entry[1] for entry in series]],
        loc="upper center",
        ncol=4,
        frameon=True,
        bbox_to_anchor=(0.5, 0.995),
    )
    stem = "figure8_paper_values" if paper_values else "figure8_exact_values"
    for suffix in ("pdf", "svg", "png"):
        fig.savefig(output_dir / f"{stem}.{suffix}", dpi=300)
    plt.close(fig)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument(
        "--plot",
        choices=("none", "exact", "paper"),
        default="none",
        help="draw the exact evidence using the requested output filename convention",
    )
    args = parser.parse_args()
    rows = list(csv.DictReader(EVIDENCE.open()))
    if len(rows) != 28:
        raise RuntimeError(f"expected 28 rows, found {len(rows)}")
    actual = recompute(rows)
    expected = json.loads(REFERENCE.read_text())["geometric_means"]
    mismatches = {
        key: {"actual": actual[key], "expected": expected[key]}
        for key in expected
        if not math.isclose(actual[key], expected[key], rel_tol=1e-14, abs_tol=1e-14)
    }
    clipped = [
        row["benchmark"]
        for row in rows
        if row["figure8_phase_ii_bar_clipped"].lower() == "true"
    ]
    result = {
        "status": "PASS" if not mismatches else "FAIL",
        "replay_type": "evidence_only",
        "paper_location": "Figure 8 and Section IV-D",
        "rows": len(rows),
        "geometric_means": actual,
        "paper_rounded_reductions_percent": {
            "without_phase_ii_sizing": round(
                actual["full_reduction_vs_phase_i_only_percent"], 1
            ),
            "without_phase_i_exploration": round(
                actual["full_reduction_vs_phase_ii_only_percent"], 1
            ),
        },
        "clipped_paper_bars": clipped,
        "optimization_executed": False,
        "external_eda_executed": False,
        "mismatches": mismatches,
    }
    if args.output_dir:
        args.output_dir.mkdir(parents=True, exist_ok=True)
        (args.output_dir / "figure8_recomputed.json").write_text(
            json.dumps(result, indent=2) + "\n"
        )
        if args.plot != "none":
            plot(rows, args.output_dir, paper_values=args.plot == "paper")
    print(json.dumps(result, indent=2))
    return 0 if not mismatches else 1


if __name__ == "__main__":
    raise SystemExit(main())

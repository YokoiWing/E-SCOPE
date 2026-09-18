#!/usr/bin/env python3
"""Verify Table III, recompute aggregates, and redraw Figure 6."""

import argparse
import csv
import json
import math
import os
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DATA = ROOT / "evidence/main28/table_iii_figure6_original_precision.csv"
EXPECTED = ROOT / "evidence/main28/replay_report.json"


def gm(values):
    return math.exp(sum(math.log(value) for value in values) / len(values))


def close(actual, expected, tolerance=1e-12):
    if not math.isclose(actual, expected, rel_tol=tolerance, abs_tol=tolerance):
        raise RuntimeError(f"expected {expected}, got {actual}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path, default=ROOT / "reproduced/main28")
    parser.add_argument("--no-plot", action="store_true")
    args = parser.parse_args()
    rows = list(csv.DictReader(DATA.open()))
    expected = json.loads(EXPECTED.read_text())
    if len(rows) != 28:
        raise RuntimeError(f"expected 28 rows, got {len(rows)}")
    for row in rows:
        d = float(row["optimized_delay_ps"]) / float(row["g0_delay_ps"])
        a = float(row["optimized_area_um2"]) / float(row["g0_area_um2"])
        p = float(row["optimized_power_uw"]) / float(row["g0_power_uw"])
        close(d * d * a * p, float(row["d2ap_over_g0"]), 2e-12)
    qor = gm([float(row["d2ap_over_g0"]) for row in rows])
    runtime = gm([
        float(row["escope_search_wall_sec"]) / float(row["genus_mapping_wall_sec"])
        for row in rows
    ])
    close(qor, expected["d2ap_geometric_mean_ratio"])
    close(runtime, expected["runtime_ratio_geometric_mean"])
    adder = next(row for row in rows if row["benchmark"] == "epfl_adder")
    if (int(adder["g0_gates"]), float(adder["genus_mapping_wall_sec"]), float(adder["escope_search_wall_sec"])) != (1942, 301.06, 341.932352107):
        raise RuntimeError("current-PDF adder runtime tuple does not match")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    if not args.no_plot:
        os.environ.setdefault("MPLCONFIGDIR", str(args.output_dir / ".mpl"))
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        rows.sort(key=lambda row: int(row["g0_gates"]))
        gates = [int(row["g0_gates"]) for row in rows]
        fig, ax = plt.subplots(figsize=(7.2, 4.2), layout="constrained")
        ax.plot(gates, [float(row["genus_mapping_wall_sec"]) for row in rows], "o-", label="High-effort Genus synthesis")
        ax.plot(gates, [float(row["escope_search_wall_sec"]) for row in rows], "s-", label="E-SCOPE post-mapping optimization")
        ax.set(xscale="log", yscale="log", xlabel="G0 gate count", ylabel="Runtime (s)")
        ax.grid(True, which="major", linestyle="--", alpha=0.45)
        ax.legend(frameon=False)
        fig.savefig(args.output_dir / "figure6_runtime.png", dpi=180)
        fig.savefig(args.output_dir / "figure6_runtime.svg")
        plt.close(fig)
    result = {
        "status": "PASS",
        "rows": len(rows),
        "d2ap_geometric_mean_ratio": qor,
        "d2ap_reduction_percent": 100 * (1 - qor),
        "runtime_ratio_geometric_mean": runtime,
        "adder_runtime_tuple": [1942, 301.06, 341.932352107],
        "classification": "saved-evidence replay",
    }
    (args.output_dir / "replay.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()

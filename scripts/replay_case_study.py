#!/usr/bin/env python3
"""Recompute the Table IV case-study rows from packaged receipts.

This is an evidence replay.  It does not run E-SCOPE, AreaPMO, ABC, or Genus.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import subprocess
import sys
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
CASE_STUDY = REPO / "case_study"
CASES = ("usb_phy", "simple_spi", "systemcdes", "usb_funct", "aes_core", "RISC")
METRICS = ("area", "delay_ps", "power_W")


def read_json(path: Path):
    return json.loads(path.read_text())


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def row(case, method, evaluator, netlist, values, baseline, extra=None):
    result = {
        "case": case,
        "method": method,
        "evaluator": evaluator,
        "netlist": str(netlist.relative_to(CASE_STUDY)),
        "sha256": sha256(netlist),
    }
    for metric in METRICS:
        value = values.get(metric)
        base = baseline.get(metric)
        result[metric] = value
        result["g0_" + metric] = base
        if value is None:
            result["delta_" + metric + "_pct"] = None
            result["display_delta_" + metric + "_pct"] = None
        else:
            delta = 100.0 * (value / base - 1.0)
            result["delta_" + metric + "_pct"] = delta
            result["display_delta_" + metric + "_pct"] = f"{delta:.2f}"
    if extra:
        result.update(extra)
    return result


def genus_values(receipt):
    return {
        "area": receipt["area"],
        "delay_ps": receipt["delay"],
        "power_W": receipt["power"],
    }


def recompute_rows():
    iterative_results = {
        (item["case"], item["guide"]): item
        for item in read_json(CASE_STUDY / "iterative/data/results.json")
    }
    rows = []
    for case in CASES:
        result_root = CASE_STUDY / "results" / case
        native = read_json(result_root / "areapmo/native_summary.json")
        selected = read_json(result_root / "areapmo/selection.json")["best_at_stop"]
        genlib_g0 = {
            "area": native["g0"]["area"],
            "delay_ps": native["g0"]["delay"],
            "power_W": None,
        }
        rows.append(
            row(
                case,
                "AreaPMO",
                "GENLIB",
                result_root / "areapmo/final.v",
                {
                    "area": selected["area"],
                    "delay_ps": selected["delay"],
                    "power_W": None,
                },
                genlib_g0,
            )
        )

        receipts = read_json(result_root / "genus/baseline_receipts.json")
        genus_g0 = genus_values(receipts["G0"])
        rows.append(
            row(
                case,
                "AreaPMO",
                "Genus",
                result_root / "areapmo/final.v",
                genus_values(receipts["AreaPMO"]),
                genus_g0,
            )
        )

        genlib = iterative_results[(case, "genlib")]
        rows.append(
            row(
                case,
                "Iterative-genlib",
                "GENLIB",
                result_root / "iterative_genlib/final.v",
                {
                    "area": genlib["metrics"]["area"],
                    "delay_ps": genlib["metrics"]["delay_ps"],
                    "power_W": None,
                },
                genlib_g0,
                {
                    "historical_classification": genlib[
                        "validation_classification_against_legacy"
                    ],
                    "search_seconds": genlib["search_seconds"],
                },
            )
        )

        internal = iterative_results[(case, "internal_v3")]
        iterative_receipt_path = result_root / "genus/iterative_receipt.json"
        iterative_values = (
            genus_values(read_json(iterative_receipt_path))
            if iterative_receipt_path.exists()
            else {
                "area": internal["metrics"]["area"],
                "delay_ps": internal["metrics"]["delay_ps"],
                "power_W": internal["metrics"]["power_W"],
            }
        )
        rows.append(
            row(
                case,
                "Iterative-internal_v3",
                "Genus",
                result_root / "iterative_internal_v3/final.v",
                iterative_values,
                genus_g0,
                {
                    "historical_classification": internal[
                        "validation_classification_against_legacy"
                    ],
                    "search_seconds": internal["search_seconds"],
                },
            )
        )
    return rows


def compare(actual, expected):
    failures = []
    if len(actual) != len(expected):
        failures.append(f"row count {len(actual)} != {len(expected)}")
        return failures
    for index, (left, right) in enumerate(zip(actual, expected)):
        for key in right:
            if key not in left:
                failures.append(f"row {index} missing {key}")
                continue
            a, b = left[key], right[key]
            if isinstance(b, float):
                if not math.isclose(a, b, rel_tol=1e-12, abs_tol=1e-12):
                    failures.append(f"row {index} {key}: {a!r} != {b!r}")
            elif a != b:
                failures.append(f"row {index} {key}: {a!r} != {b!r}")
    return failures


def write_outputs(output_dir: Path, rows):
    output_dir.mkdir(parents=True, exist_ok=True)
    payload = {
        "replay_type": "saved-evidence recomputation; no optimization or EDA executed",
        "paper_location": "Table IV",
        "delta_definition": "100 * (candidate / same-evaluator G0 - 1)",
        "rows": rows,
    }
    (output_dir / "table_iv_recomputed.json").write_text(
        json.dumps(payload, indent=2) + "\n"
    )
    fields = list(dict.fromkeys(key for item in rows for key in item))
    with (output_dir / "table_iv_recomputed.csv").open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output-dir",
        type=Path,
        help="optional directory for recomputed JSON and CSV",
    )
    args = parser.parse_args()

    verification = subprocess.run(
        [sys.executable, str(CASE_STUDY / "verify.py")],
        cwd=REPO,
        text=True,
        capture_output=True,
        check=True,
    )
    expected = read_json(CASE_STUDY / "results/TABLE.json")["rows"]
    actual = recompute_rows()
    failures = compare(actual, expected)
    if args.output_dir:
        write_outputs(args.output_dir, actual)
    summary = {
        "status": "PASS" if not failures else "FAIL",
        "replay_type": "evidence_only",
        "paper_location": "Table IV",
        "rows_recomputed": len(actual),
        "rows_matched": len(actual) if not failures else None,
        "artifact_verification": json.loads(verification.stdout),
        "optimization_executed": False,
        "external_eda_executed": False,
        "failures": failures,
    }
    print(json.dumps(summary, indent=2))
    return 0 if not failures else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Audit an existing Iterative checkpoint; never rerun search or supply its winners to search."""
import argparse
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

from run_sin_incumbent_preserving_probe_v1 import dump, load, sha, process_tree_rss
from run_conquer_defense_v1 import available_memory_kib
from run_iterative_da_sweep_v1 import frozen_env

D1 = Path(__file__).resolve().parents[1]
CHECKS = ("formal_equivalence", "mapped_only_legality", "full186_only", "write_reparse_cold_read")


def checked_reference(proof, g0):
    row = load(proof)
    assert all(row[k] == "PASS" for k in CHECKS) and row["unresolved_cells"] == 0
    assert row["target_sha256"] == sha(g0)
    assert row["source_sha256"] == sha(Path(row["source"]))
    assert sha(Path(row["target_netlist"])) == row["target_sha256"]
    return row


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for key in ("output", "g0", "iterative-r1", "g0-proof"):
        p.add_argument("--" + key, type=Path, required=True)
    p.add_argument("--benchmark", required=True)
    p.add_argument("--target", required=True)
    p.add_argument("--timeout", type=int, default=5400)
    args = p.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    begin = time.monotonic()
    g0, r1, proof = args.g0.resolve(), args.iterative_r1.resolve(), args.g0_proof.resolve()
    original = checked_reference(proof, g0)
    dump(root / "inputs.json", {"g0": str(g0), "g0_sha256": sha(g0), "iterative_r1": str(r1),
         "iterative_r1_sha256": sha(r1), "g0_proof": str(proof), "search_runs": 0})
    (root / "pid.txt").write_text(str(os.getpid()) + "\n")
    stages = []
    env = {k:v for k,v in frozen_env(1).items() if not k.startswith("EGG_")}

    def run(stage, command):
        start, peak = time.monotonic(), 0
        with (root / (stage + ".stdout.log")).open("w") as log:
            proc = subprocess.Popen(list(map(str, command)), cwd=D1, env=env,
                                    stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                while proc.poll() is None:
                    peak = max(peak, process_tree_rss(proc.pid))
                    elapsed = time.monotonic() - start
                    dump(root / "status.json", {"stage": stage, "status": "running", "pid": os.getpid(),
                         "child_pid": proc.pid, "elapsed_sec": elapsed, "peak_rss_kib": peak})
                    if elapsed > args.timeout or peak > 96*1024**2 or available_memory_kib() < 96*1024**2:
                        raise RuntimeError("reference audit resource/time stop")
                    try:
                        proc.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        pass
            except BaseException:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGTERM)
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(proc.pid, signal.SIGKILL)
                        proc.wait()
                dump(root / "status.json", {"stage": stage, "status": "failed"})
                raise
        stages.append({"stage": stage, "wall_sec": time.monotonic()-start, "peak_rss_kib": peak,
                       "command": list(map(str, command)), "returncode": proc.returncode})
        dump(root / "stage_receipts.json", stages)
        dump(root / "progress_summary.json", {"completed_stages": len(stages), "stages": stages})
        if proc.returncode:
            dump(root / "status.json", {"stage": stage, "status": "failed"})
            raise RuntimeError(stage + " failed")
        print(stage, "PASS", flush=True)

    dump(root / "external_input" / args.benchmark / args.target / "paper_status.json", {
        "benchmark": args.benchmark, "target": args.target,
        "checkpoints": [{"round": 0, "netlist": str(g0)}, {"round": 1, "netlist": str(r1)}]})
    run("genus", ["python3", D1/"scripts/run_paper_external_grid.py", "--v8-root", root/"external_input",
                   "--output-root", root/"external_genus", "--session-root", root/"external_sessions",
                   "--timeout", args.timeout])
    (root/"formal_candidates").mkdir()
    shutil.copy2(r1, root/"formal_candidates/ITERATIVE_R1.v")
    run("whole_cec_g0_to_r1", ["python3", D1/"scripts/validate_mapped_candidate_directory.py",
        "--source", g0, "--source-top", original["target_top"], "--candidate-dir", root/"formal_candidates",
        "--output-root", root/"equivalence", "--benchmark", args.benchmark, "--jobs", 1,
        "--timeout", args.timeout])
    child = load(root/"equivalence/validation_status.json")["results"][0]
    assert child["source_sha256"] == original["target_sha256"] and child["target_sha256"] == sha(r1)
    assert all(child[k] == "PASS" for k in CHECKS)
    checked_reference(proof, g0)
    dump(root/"proof_chain.json", {"method": "two whole-network CEC edges, exact-SHA transitivity",
         "original_to_g0_receipt": str(proof), "g0_to_r1_receipt": str(root/"equivalence/ITERATIVE_R1/status.json"),
         "original_source_sha256": original["source_sha256"], "g0_sha256": sha(g0), "r1_sha256": sha(r1),
         "formal_equivalence": "PASS", "g0_proof_reused": True})
    external = load(root/"external_genus/external_status.json")
    assert not external["failures"] and len(external["results"]) == 2
    g = next(r for r in external["results"] if r["round"] == 0)
    f = next(r for r in external["results"] if r["round"] == 1)
    summary = {"stage": "complete", "benchmark": args.benchmark, "target": args.target,
               "g0": g, "iterative_r1": f, "iterative_r1_over_g0": f["external"]["d2ap"]/g["external"]["d2ap"],
               "formal_equivalence": "PASS", "proof_method": "original -> G0 -> R1 whole-network CEC chain",
               "search_runs": 0, "wall_sec": time.monotonic()-begin, "stages": stages}
    dump(root/"SUMMARY.json", summary)
    dump(root/"status.json", {"stage": "complete", "status": "complete"})
    print("COMPLETE Iterative R1/G0", summary["iterative_r1_over_g0"], flush=True)


if __name__ == "__main__":
    main()

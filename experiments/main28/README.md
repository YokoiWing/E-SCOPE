# Table III main experiment

This directory packages the 28 exact G0 mapped Verilog netlists used by the
current Table III, plus the frozen runtime-aware choice between **Iterative**
and **Conquer**.

## Verify the inputs and policy

```bash
python3 experiments/main28/verify.py --small
python3 experiments/main28/verify.py
```

The small replay covers six inexpensive circuits and several different policy
clauses. Both commands verify the SHA-256 of all 28 G0 files. The full command
then replays all saved method choices.

`manifest.json` binds every benchmark and anchor to its G0 path, byte SHA-256,
original-precision G0 PPA, selected paper method and saved policy observation.
`prepare_from_workspace.py` shows how the packaged inputs were recovered by
content hash from the original experiment tree.

## Runtime policy

Iterative and Conquer start from the same G0. When Conquer finishes, the policy
uses the best Iterative checkpoint completed by that time, projected Iterative
runtime, design size, Conquer candidate class and their measured D²AP changes.
`policy.py` contains the ordered clauses and accepts one JSON feature record.

This is a retrospective, in-sample policy. Its thresholds were fitted to the
same historical results. The saved observations use external PPA values, while
the paper's search-only runtime excludes external evaluation latency. It must
therefore be described as a reproducible selection rule, not as held-out
generalization or a zero-overhead online controller.

For 27 points, the policy evidence and current paper row use the same anchor.
The current `epfl_adder/p0800` row replaced an older adder anchor: both select
Iterative, but the packaged policy record is only a method-level transfer for
that one point.

The complete 28-point optimization remains expensive and is not launched by
the verification commands. Subsequent packaging will connect these inputs to
the source-built Iterative and Conquer runners with resumable per-point output
directories.

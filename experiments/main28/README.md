# Table III main experiment

This directory is the single entry point for the 28-point main experiment. It
contains 28 exact G0 mapped netlists, the 28 selected paper output netlists,
Iterative and Conquer source/configuration, and the frozen runtime-aware method
policy.

The artifact has three reproduction levels:

1. **Saved-evidence replay** verifies all 56 netlists by SHA-256 and recomputes
   Table III and Figure 6 from original-precision measurements.
2. **Fresh search** builds the code and runs Iterative, Conquer, or both from a
   packaged G0. A fresh result is new evidence; it is not declared equal to the
   paper result unless its output SHA-256 matches `manifest.json`.
3. **External validation** requires a user-provided licensed Cadence Genus
   installation and fresh formal checks. Genus and its license are not in this
   repository.

## Fast reproduction of the reported result

From the repository root:

```bash
python3 experiments/main28/run.py evidence --output reproduced/main28
```

This checks every G0 and selected output, replays all 28 policy decisions,
recomputes the D²AP geometric mean and the search/Genus runtime ratio, and
redraws Figure 6. Add `--no-plot` if matplotlib is unavailable. It does not run
optimization or EDA.

The expected aggregate values are:

- D²AP geometric-mean ratio: `0.8210095475250041` (17.8990% reduction);
- geometric-mean E-SCOPE search / Genus mapping wall ratio:
  `1.4583649253561954`, reported as `1.46x`;
- current adder point: `epfl_adder/p0800`, D×A objective, five Iterative rounds,
  301.06 s Genus mapping and 303.351 s E-SCOPE search.

## Build and inspect a fresh run

The main experiment uses the ASAP7 6T full-combinational LVT/TT NLDM Liberty
file. It is not redistributed here. Supply this exact file:

```text
SHA-256 48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd
```

Build artifacts and run outputs should stay outside the checkout:

```bash
python3 experiments/main28/run.py build \
  --target-dir /tmp/escope-main28-build

python3 experiments/main28/run.py preflight \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-main28-build/release

python3 experiments/main28/run.py plan \
  --output /tmp/escope-main28-plan.json \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-main28-build/release
```

`plan` records all 28 G0 hashes, objectives, round caps, paper-selected methods,
expected output hashes, and exact fresh-run commands without starting work.

Run a one-round functional smoke of both methods on a small point:

```bash
python3 experiments/main28/run.py run-one \
  --benchmark epfl_ctrl --method both --smoke \
  --output /tmp/escope-main28-smoke \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-main28-build/release
```

Run one point with its paper round cap by omitting `--smoke`. Run the complete,
point-level resumable queue with:

```bash
python3 experiments/main28/run.py run-all \
  --output /data/escope-main28-run \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-main28-build/release
```

The full queue is expensive and is never launched by verification. It runs the
two search branches from the same G0 and preserves each attempt in a separate
directory. Fresh search stops before commercial external evaluation. Evaluate
fresh checkpoints/finalists under the paper boundary conditions, form the
feature JSON described by `policy.py`, then apply the frozen rule with:

```bash
python3 experiments/main28/run.py select --features measured_features.json
```

## Configuration and selection boundary

All ordinary points use D²×A×P and a five-round Iterative cap.
`epfl_mem_ctrl` uses its recorded two-round cap, `epfl_hyp` its recorded
one-round cap, and the current `epfl_adder/p0800` point uses D×A for five
Iterative rounds. `runtime/config/` contains the search configurations;
`runtime/assets/` contains the exact rewrite rules and the sanitized Liberty
audit.

The policy is a retrospective, in-sample rule. Its saved observations use
external D²AP for the completed Iterative checkpoint and Conquer result, while
the paper search-only runtime excludes external evaluation latency. For 27
points the policy record and paper row use the same anchor. The new
`epfl_adder/p0800` row only confirms the same method choice, Iterative; it is not
a same-anchor policy validation.

`expected/` contains the exact selected netlist for every paper row. These are
especially useful for independent Genus/formal reevaluation without rerunning
search. `source/source_provenance.json` records the source-build boundary.

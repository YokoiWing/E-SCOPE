# Table III main experiment

This directory is the single entry point for the 28-point main experiment. It
contains 28 exact G0 mapped netlists, the 28 selected paper output netlists,
Iterative and Conquer source/configuration, and the frozen runtime-aware method
policy.

The artifact has three reproduction levels:

1. **Saved-evidence replay** verifies all 56 netlists by SHA-256 and recomputes
   Table III and Figure 6 from original-precision measurements.
2. **Fresh online reproduction** starts Iterative and Conquer together from a
   packaged G0. When Conquer finishes, it freezes exactly the Iterative
   checkpoints already complete at that instant, evaluates that immutable pool,
   pauses Iterative, and applies the Section III-E stop/continue policy after
   validation. If the policy selects Conquer, Iterative is terminated;
   otherwise it is resumed and runs to its configured round cap. The selected
   output is guarded by mapped-netlist legality and whole-network equivalence.
3. **External validation** in the online path requires user-provided Cadence
   Genus, Yosys, and ABC executables. Genus and its license are not in this
   repository. A fresh result is new evidence; it is declared identical to the
   paper output only when its SHA-256 matches `manifest.json`.

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
  --bin-dir /tmp/escope-main28-build/release \
  --genus-bin /path/to/genus --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

`plan` records all 28 G0 hashes, objectives, round caps, paper-selected methods,
expected output hashes, and exact fresh-run commands without starting work.

Run a one-round functional smoke of the full online controller on a small
point:

```bash
python3 experiments/main28/run.py run-one \
  --benchmark epfl_ctrl --method online --smoke \
  --output /tmp/escope-main28-smoke \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-main28-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

`--smoke` deliberately changes the search budget, so it validates control flow
rather than the paper netlist hash. Run one point with its paper round cap by
omitting `--smoke`. `DECISION.json` records the frozen checkpoint, measured
features, matched policy clause, and whether Iterative was stopped or continued;
`selected/receipt.json` records Genus PPA, formal status, and the paper-output
SHA comparison.

Run the complete, point-level resumable queue with:

```bash
python3 experiments/main28/run.py run-all \
  --output /data/escope-main28-run \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-main28-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

The full queue is expensive and is never launched by verification. It runs the
two search branches from the same G0, makes the decision online, and preserves
each attempt in a separate directory. For diagnostics, `--method iterative`,
`conquer`, or `both` still runs search without online selection. A separately
measured feature record can be checked with:

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

After an Iterative continuation, ordinary points externally compare the
completed round checkpoints. The adder follows its paper experiment: before
external feedback, the controller freezes the union of its exact internal
Delay–Area and Delay–Power fronts, then applies Genus/formal and selects by
D×A. This includes the progressive candidate class that produced the reported
`archive_000`; treating only `accepted.v` as the adder result would omit it.

The policy is a retrospective, in-sample rule. Its saved observations use
external PPA for the best completed Iterative checkpoint and the frozen
Conquer finalists. The online implementation preserves the paper's causal
boundary: the completed-checkpoint list is frozen from Conquer's durable
completion timestamp before any external result is read. Genus/formal latency
is included in the fresh end-to-end wall time, while the paper's reported
search-only runtime and projected-runtime feature exclude that latency.

The actual online choice is runtime-dependent. CPU contention, tool versions,
and machine load can change how many Iterative rounds have completed when
Conquer returns. `evidence` replays the paper machine's saved checkpoint and
PPA observations and verifies all 28 reported choices; a fresh run reports its
own decision without forcing it to match. For 27 points the saved policy record
and paper row use the same anchor. The new `epfl_adder/p0800` row confirms only
the same method choice, Iterative, and is not a same-anchor policy validation.

`expected/` contains the exact selected netlist for every paper row. These are
especially useful for independent Genus/formal reevaluation without rerunning
search. `source/source_provenance.json` records the source-build boundary.

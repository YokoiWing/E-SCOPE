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

- D²AP geometric-mean ratio: `0.8240760907489141` (17.5924% reduction);
- geometric-mean E-SCOPE search / Genus mapping wall ratio:
  `1.4646139505859852`, reported as `1.46x`;
- current adder point: `epfl_adder/p0800`, D²×A×P objective, five Iterative rounds,
  301.06 s Genus mapping and 341.932352107 s E-SCOPE search.

The sanitized fresh-run receipt is
`../../evidence/main28/epfl_adder_p0800_d2ap_fresh_receipt.json`.

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

`epfl_hyp` uses the paper experiment's fixed G0-only fusion/composition
profile: its Conquer normal V8/A2 lane is disabled, and profiling uses eight
jobs with a 5400-second stage guard. The exact settings are in
`runtime/config/conquer_hyp.json`; enabling the normal lane changes both the
experiment and its runtime substantially.

```bash
python3 experiments/main28/run.py select --features measured_features.json
```

## Configuration and selection boundary

Ordinary points use D²×A×P and the per-point Iterative cap recorded in
`manifest.json`: five rounds for the R5 points, three rounds for `c6288`,
`epfl_dec`, `epfl_div`, `epfl_sqrt`, and `epfl_voter`, two rounds for
`epfl_mem_ctrl`, and one round for `epfl_hyp`. The current
`epfl_adder/p0800` point also uses D²×A×P for five Iterative rounds.
`runtime/config/` contains the search configurations;
`runtime/assets/` contains the exact rewrite rules and the sanitized Liberty
audit.

All 28 D²×A×P points use the historical native `d2ap` search mode. They
do not load a generic ObjectiveSpec: doing so would activate candidate families
that were absent from the Table III trajectories. The runner removes inherited
generic-objective environment variables and rejects any main28 objective other
than native D²×A×P; `verify.py` independently checks all 28 manifest rows and
the `2/1/1` exponent file.

Ordinary Iterative trajectories use `iterative_search.json`. Conquer's internal
normal lane uses `timing_boundary_search.json`, matching its separate
historical receipt.
The files differ only in the Phase-I timing-boundary switch, but sharing one
configuration changes candidate identities on some benchmarks.

The online policy evaluates external objectives with the Genus data-path delay
reported in Table III. The separately recorded BUFx2 driver adjustment is kept
for boundary auditing and is not added to the Table III objective value.

After an Iterative continuation, ordinary points externally compare the paper's
R1/R3/R5 checkpoints; when search stops between them, the final accepted round
provides the next checkpoint's compatibility alias. `epfl_mem_ctrl` retains its
special R1/R2 evidence.

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
own decision without forcing it to match. An exact method-label replay is
therefore not the fresh-run acceptance criterion. The selected netlist must
instead pass mapped-as-is Genus and formal validation, and the run must report
its own measured PPA. Historical PPA may not be substituted when the selected
netlist SHA differs. All 28 saved policy records use the same anchor as their
main-table row; `epfl_adder/p0800` comes from the fresh native-D²AP rerun.

A clean three-round rerun illustrates this machine-speed sensitivity. The
saved run selected Iterative for both `epfl_sqrt` and `epfl_div`; the fresh run
selected Conquer, because Conquer returned before the same Iterative progress
was available to the policy. Both fresh outputs passed Genus and formal and
still improved on G0:

| benchmark | saved D²AP/G0 | fresh D²AP/G0 | saved reduction | fresh reduction |
|---|---:|---:|---:|---:|
| `epfl_sqrt` | 0.950965015 | 0.969743865 | 4.9035% | 3.0256% |
| `epfl_div` | 0.950942015 | 0.966125130 | 4.9058% | 3.3875% |

For `epfl_sqrt`, no Iterative round had completed when Conquer returned. For
`epfl_div`, the fresh projected runtime ratio was 2.719 instead of the saved
3.902, so the same policy chose Conquer as materially better. These are
outcomes of the general online rule; the implementation contains no
benchmark-specific method override.

`expected/` contains the exact selected netlist for every paper row. These are
especially useful for independent Genus/formal reevaluation without rerunning
search. `source/source_provenance.json` records the source-build boundary.

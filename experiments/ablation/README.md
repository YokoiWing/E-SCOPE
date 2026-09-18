# Figure 8 selected-method ablation

This directory provides the fresh-search entry for Section IV-D and Figure 8.
It follows the method already selected by the main experiment:

- 11 benchmarks run the Iterative ablation;
- 17 benchmarks run the Conquer ablation;
- an ablation result never changes that frozen method choice.

This pairing matters. A Conquer-selected point uses Conquer's selected regional
implementation for Phase I and its one-round fixed-G0 sizing policy for Phase
II. Substituting the Iterative ablation on those points would answer a different
question.

## Ablation definitions

- `phase1-only`: topology and rewrite exploration with drive choices embedded
  in structural alternatives; progressive A2/O1 is disabled. This is not a
  pure topology-only control.
- `phase2-only`: the G0 topology is fixed. Iterative uses its recorded round
  cap; Conquer uses one progressive sizing round with the packaged per-point
  normal-lane policy.
- `no-pi-drive-expansion`: the full two-phase path remains active, while extra
  Phase-I drive realizations of the same structural seed are suppressed.

For a Conquer `phase1-only` fresh run, the entry generates and freezes the
Conquer candidate pool. External evaluation must select the full candidate,
after which the paired pre-sizing implementation is the Phase-I-only result.
For Conquer `no-pi-drive-expansion`, the same frozen policy runs with the
drive-expansion filter enabled. All finalists are frozen before Genus; the
formal-clean minimum external D²AP result is selected with G0 fallback.

## Saved evidence

```bash
python3 experiments/ablation/run.py evidence \
  --output reproduced/figure8 --plot exact
```

This checks that all 28 method labels equal the Table III selections, verifies
the G0 hashes, recomputes the three geometric means, and redraws Figure 8. The
paper plot clips one `epfl_i2c` Phase-II bar to `1.035`; `--plot exact` keeps
the measured `1.0708307702363498` ratio.

The updated paper-rendered Figure 8 is packaged as
`evidence/figure8/figure8_paper_values.{png,svg,pdf}`.

All 28 rows now use the current main-table anchor. The `epfl_adder` row was
freshly rerun on `p0800` with native D²AP: all 15 frozen netlists completed
mapped-as-is Genus evaluation and all 14 non-G0 candidates passed formal.
The `epfl_sqrt` row was also freshly rerun with its current three-round cap:
all 9 frozen netlists completed Genus evaluation and all 8 non-G0 candidates
passed formal.

## Fresh search

Build the shared source outside the checkout and inspect all 84 tasks without
starting them:

```bash
python3 experiments/ablation/run.py build \
  --target-dir /tmp/escope-ablation-build

python3 experiments/ablation/run.py plan \
  --output /tmp/escope-ablation-plan.json
```

Run one selected-method ablation with the user-provided full-combinational
Liberty:

```bash
python3 experiments/ablation/run.py run-one \
  --benchmark c432 --mode phase1-only \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-ablation-build/release \
  --output /tmp/escope-ablation-c432-p1
```

`run-one` reads the method from `manifest.json`; there is no command-line
override. Search outputs are new evidence and remain `PENDING` until fresh
formal and mapped-as-is external evaluation completes. The required Liberty
SHA-256 is
`48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd`.

`conquer_phase2_policies/` contains the 17 exact per-point sizing policies.
`configs/` contains the four frozen Conquer policy classes. The shared Rust and
Python implementation remains under `experiments/main28/` to avoid duplicating
the source tree.

## Complete fresh Figure 8 reproduction

The unified `reproduce` command performs the complete experiment:

1. run or resume the 84 selected-method ablation searches;
2. freeze every Iterative checkpoint and every Conquer no-drive finalist;
3. pair each Conquer Phase-I-only run with the sizing-free implementation of
   the candidate selected by the frozen main-method configuration;
4. evaluate the 28 G0, 28 full-method references, and all frozen ablation
   candidates with mapped-as-is Genus;
5. run Yosys legality/cold-read checks and whole-network ABC CEC;
6. select the externally best formal-clean result under the documented policy;
7. calculate fresh D²AP ratios and write exact and paper-rendered Figure 8.

The full-method netlists are SHA-fixed reference inputs for the ablation, just
as G0 is a fixed reference input. All 28 reuse the packaged current Table III
outputs.

```bash
python3 experiments/ablation/run.py reproduce \
  --output /data/escope-ablation-fresh \
  --liberty /path/to/asap7sc6t_FULL_COMB_LVT_TT_nldm_211010.lib \
  --bin-dir /tmp/escope-ablation-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys \
  --abc-bin /path/to/abc
```

Genus, Yosys, ABC, and the exact full-combinational Liberty are supplied by the
user. Add `--yosys-datdir /path/to/share/yosys` when required by a local Yosys
installation. Search, Genus, and formal stages resume from SHA-bound receipts.
For an already completed 84-task search root, use `finalize` with the same
Liberty and tool arguments.

The principal fresh outputs are:

```text
/data/escope-ablation-fresh/frozen/FREEZE_RECEIPT.json
/data/escope-ablation-fresh/external/results.json
/data/escope-ablation-fresh/formal/results.json
/data/escope-ablation-fresh/selection/results.json
/data/escope-ablation-fresh/figure8/figure8_data.csv
/data/escope-ablation-fresh/figure8/figure8_exact_values.{png,svg,pdf}
/data/escope-ablation-fresh/figure8/figure8_paper_values.{png,svg,pdf}
/data/escope-ablation-fresh/REPRODUCTION.json
```

The exact plot uses every measured value. The paper-rendered plot preserves the
documented `epfl_i2c` display clipping while aggregate statistics always use
the exact value.

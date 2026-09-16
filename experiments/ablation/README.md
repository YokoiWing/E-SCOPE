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
drive-expansion filter enabled. The entry stops before Genus and formal checks.

## Saved evidence

```bash
python3 experiments/ablation/run.py evidence \
  --output reproduced/figure8 --plot exact
```

This checks that all 28 method labels equal the Table III selections, verifies
the G0 hashes, recomputes the three geometric means, and redraws Figure 8. The
paper plot clips one `epfl_i2c` Phase-II bar to `1.035`; `--plot exact` keeps
the measured `1.0708307702363498` ratio.

Twenty-seven rows use the current main-table anchor. Figure 8 retains the older
`epfl_adder/t0_0.95x` evidence while current Table III uses `p0800`; both select
Iterative. This is an explicit version difference, not a same-anchor replay.

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

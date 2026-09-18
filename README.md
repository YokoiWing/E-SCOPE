# E-SCOPE

Minimal source release for the E-SCOPE experiments. The repository contains
the implementation, runnable experiment drivers, configurations, and input
netlists. It intentionally does not contain saved winners, expected output
netlists, historical EDA reports, or pre-rendered figures.

## Requirements

- Python 3.10+
- Rust and Cargo
- the ASAP7 6-track full-combinational Liberty used by the paper (SHA-256
  `48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd`)
- Cadence Genus for mapped-as-is PPA evaluation
- Yosys and Berkeley ABC for legality and equivalence checking

Commercial tools and licenses are not distributed. See `THIRD_PARTY.md`.

## Build

```bash
python3 experiments/main28/run.py build \
  --target-dir /tmp/escope-build
```

## Small smoke run

This starts one Iterative round and one Conquer search on `epfl_ctrl`, then
performs the online decision and external validation:

```bash
python3 experiments/main28/run.py run-one \
  --benchmark epfl_ctrl --method online --smoke \
  --output /tmp/escope-smoke \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys \
  --abc-bin /path/to/abc
```

Every output is created by the current run. No command compares against or
copies a packaged expected result.

## Experiments

- `experiments/main28/`: concurrent Iterative/Conquer main experiment.
- `experiments/pareto_adder/`: three-objective adder Pareto experiment.
- `experiments/ablation/`: selected-method phase ablations. Its finalization
  consumes fresh full-method outputs from `main28/run.py run-all` through
  `--full-results`.
- `case_study/`: minimal AreaPMO and Iterative case-study runners.

Each directory has its own README and `plan`/`run-one` or equivalent command.
Write build and experiment outputs outside the checkout.

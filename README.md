# E-SCOPE

E-SCOPE optimizes mapped logic netlists using e-graph exploration and
standard-cell PPA estimation. This repository contains the source, input
netlists, and scripts for the paper's four experiments.

## Setup

Use Linux with Python 3.10+, Rust/Cargo with edition 2024 support, and a C/C++
build toolchain. AreaPMO also needs CMake. The evaluated tool version was
Cadence Genus 23.14-s090_1; main and Pareto runs also use Yosys and Berkeley ABC.
See [THIRD_PARTY.md](THIRD_PARTY.md) for software and library details.

For the main, Pareto, and ablation experiments, provide the ASAP7 6-track
LVT/TT full-combinational Liberty. The runners check its SHA-256:

```text
48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd
```

Commands below and in the experiment READMEs run from the repository root.
Replace `/path/to/...` with your local tool and library paths. Build once for
all three experiments:

```bash
python3 experiments/main28/run.py build --target-dir /tmp/escope-build
```

## First run

This small run checks the complete search and evaluation setup:

```bash
python3 experiments/main28/run.py run-one \
  --benchmark epfl_ctrl --smoke \
  --output /tmp/escope-smoke \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

Look for `status.json`, `selected/mapped.v`, and `selected/receipt.json` in the
output directory. The receipt contains the selected method and external PPA.

## Experiments

| Experiment | Instructions |
|---|---|
| Main 28-point comparison | [main28](experiments/main28/README.md) |
| Adder Pareto search | [pareto](experiments/pareto/README.md) |
| Phase ablation | [ablation](experiments/ablation/README.md) |
| AreaPMO and Iterative case study | [case_study](case_study/README.md) |

Use a new output directory for each individual run. Keep results and builds
outside the checkout; `run-all` commands skip completed tasks when restarted.

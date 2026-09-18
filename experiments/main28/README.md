# Main 28-point experiment

`run.py` is the main entry. It builds the shared implementation and runs
Iterative and Conquer concurrently from the same packaged G0. When Conquer
finishes, the controller freezes completed Iterative checkpoints, validates
the candidates with locally supplied tools, and applies the runtime policy.

```bash
python3 experiments/main28/run.py build --target-dir /tmp/escope-build
python3 experiments/main28/run.py preflight \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
python3 experiments/main28/run.py plan --output /tmp/main28-plan.json
```

Run a small point:

```bash
python3 experiments/main28/run.py run-one \
  --benchmark epfl_ctrl --method online --smoke \
  --output /tmp/main28-ctrl \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

Omit `--smoke` for the configured round cap. `run-all` provides a resumable
28-point queue. The repository contains G0 inputs but no expected output
netlists; every selected result is generated and validated by the fresh run.

The default is two internal workers. Runtime-dependent checkpoint availability
can change the Iterative/Conquer choice across machines.

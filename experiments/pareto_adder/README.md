# Adder Pareto experiment

This directory contains 20 G0 netlists, the frozen 13-anchor selection, three
objective specifications, and a fresh-run pipeline. It contains no saved
candidates or expected frontier.

```bash
python3 experiments/pareto_adder/run.py preflight
python3 experiments/pareto_adder/run.py plan \
  --output /tmp/pareto-plan.json
python3 experiments/pareto_adder/run.py run-one \
  --anchor p0800 --objective d2ap \
  --output /tmp/pareto-p0800 \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
```

`reproduce` runs the 39 trajectories, freezes the internally selected frontier
before external feedback, performs mapped-as-is Genus and formal validation,
and creates a fresh Figure 7.

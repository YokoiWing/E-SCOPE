# Pareto experiment

The paper's experiment starts from 20 Genus-mapped adder netlists. The 13 search
anchors were selected from their delay–area and delay–power fronts. Each
anchor is optimized with DA, D²AP, and DP², giving 39 searches.

This experiment uses Iterative directly: keeping the search method fixed
makes it possible to compare objective functions on the same anchors and
collect candidates across rounds.

Build the shared binaries as described in the [root README](../../README.md).
Run one anchor and objective:

```bash
python3 experiments/pareto/run.py run-one \
  --anchor p0800 --objective d2ap \
  --output /tmp/pareto-p0800 \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
```

For the complete experiment, install Matplotlib and run:

```bash
python3 experiments/pareto/run.py reproduce \
  --output /tmp/pareto-all \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

The pipeline collects candidates, fixes the internal DA/DP front union before
external evaluation, then measures it with Genus and checks equivalence.
Results are written to `figure7/figure7_data.csv`, with PNG and SVG plots
alongside it. These fronts describe the evaluated candidate pool.

Use `--objective da`, `--objective d2ap`, or `--objective dp2` to run a subset.
For another objective, copy an `objectives/*.json` file, change its name and
exponents, and pass its path to `--objective`. The default round cap is five
and the worker count is two. `run-all` runs searches alone; `finalize` evaluates
an existing search directory using the same library and external-tool flags.

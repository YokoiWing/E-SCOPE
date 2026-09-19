# Main 28-point experiment

Iterative and Conquer start from the same G0 and run concurrently. At Conquer
completion, the controller evaluates the available Iterative checkpoints and
Conquer candidates, then decides whether to continue Iterative or keep
Conquer. All 28 points use D²AP; the inputs and round limits are in
`manifest.json`.

Build and check the setup using the [root README](../../README.md). To run one
point at its normal budget:

```bash
python3 experiments/main28/run.py run-one \
  --benchmark epfl_ctrl \
  --output /tmp/main28-ctrl \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

Run all 28 points:

```bash
python3 experiments/main28/run.py run-all \
  --output /tmp/main28-all \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

The queue runs one benchmark at a time. Repeat the same command to resume;
completed benchmarks are skipped and interrupted attempts are kept separately.
Each benchmark directory contains:

- `selected/mapped.v`: the output netlist.
- `selected/receipt.json`: selected method and Genus PPA.
- `DECISION.json`: the online choice and measurements used to make it.
- `status.json`: completion state and elapsed end-to-end time.

Two internal workers are used by default. Keep this setting for the larger
cases, especially `div`, `sqrt`, `priority`, and `mem_ctrl`. Machine load and
worker count affect which Iterative rounds finish before Conquer, so the
chosen method and PPA can vary. `DECISION.json` makes that choice visible.
Use the same Liberty and evaluation settings when comparing PPA; the Genus
version can also affect measured values. Search time and end-to-end time,
which includes external evaluation, are different measurements.

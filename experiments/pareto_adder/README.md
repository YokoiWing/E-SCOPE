# Section IV-C: `epfl_adder` Pareto experiment

This directory packages the 20 high-effort Genus G0 netlists used in Figure 7,
the frozen 13-anchor subset, the three paper objectives, and one entry point for
saved-evidence replay or fresh search. Every G0 is byte-checked against the
SHA-256 value in `manifest.json`.

## Why this experiment runs Iterative directly

Section IV-C asks whether changing the PPA objective exposes complementary
implementations. It is an objective-control experiment, rather than an
Iterative-versus-Conquer scheduling experiment. The 13 search anchors were
selected only from the external delay-area and delay-power fronts of the 20 G0
netlists, before any search candidate was externally evaluated.

Each anchor/objective pair therefore runs one direct Iterative trajectory with
the same topology-first search, conditional sizing, boundary model, rules,
round cap, and candidate-freezing protocol. Holding the search method fixed
makes the objective the intended independent variable and avoids adding the
main experiment's method-selection heuristic as a second variable. This also
matches the preserved Figure 7 runs. Conquer is not invoked by this entry.

The resulting Pareto statements are empirical statements about the candidates
frozen and externally evaluated in this experiment. They do not prove a global
Pareto frontier. Internal-NLDM-V3 guides search and freezing; the plotted PPA
comes from saved mapped-as-is Genus evaluation under a common boundary.

## Packaged inputs

- `g0/`: all 20 synthesized G0 Verilog netlists, including the 13 search
  anchors and the seven G0-only comparison points.
- `manifest.json`: G0 metrics, hashes, provenance, and the frozen anchor flag.
- `objectives/da.json`: D×A (`delay=1, area=1, power=0`).
- `objectives/d2ap.json`: D²AP (`delay=2, area=1, power=1`).
- `objectives/dp2.json`: D×P² (`delay=1, area=0, power=2`).
- `objectives/example_custom.json`: an editable interface example; it is not a
  paper configuration.

The commercial Genus program and the full-combinational Liberty file are not
distributed. The required Liberty SHA-256 is
`48f3f7f1ae6ff4a6c50da7ac8ea2cb5dca3fc0e9763321d726583f17b3e659dd`.

## Reproduce the saved Figure 7 evidence

```bash
python3 experiments/pareto_adder/run.py evidence \
  --output reproduced/figure7
```

This verifies all 20 G0 hashes, recomputes fronts and hypervolume values from
the original-precision table, and redraws Figure 7. It does not rerun search,
formal verification, or Genus.

## Inspect or run fresh search

Build the shared source in an isolated target directory:

```bash
python3 experiments/pareto_adder/run.py build \
  --target-dir /tmp/escope-pareto-build
```

Generate the complete paper plan without starting any trajectory:

```bash
python3 experiments/pareto_adder/run.py plan \
  --output /tmp/escope-pareto-plan.json
```

The default plan contains 39 sequential tasks: 13 anchors times three paper
objectives. Run one trajectory with a user-provided Liberty:

```bash
python3 experiments/pareto_adder/run.py run-one \
  --anchor p0800 --objective da \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-pareto-build/release \
  --output /tmp/escope-pareto-p0800-da
```

`run-all` is a resumable sequential queue. No full queue starts unless that
subcommand is explicitly called. A fresh output requires fresh formal and
mapped-as-is external evaluation before it can replace saved Figure 7 data.

## Objective interface

`--objective` accepts `da`, `d2ap`, `dp2`, or a JSON path. The public interface
supports normalized product objectives, normalized weighted sums, a single
metric objective, and optional absolute metric constraints. Product objectives
use this schema:

```json
{
  "schema_version": 1,
  "name": "my_objective",
  "objective": {
    "kind": "product",
    "exponents": {"delay": 1.0, "area": 0.5, "power": 1.5}
  },
  "constraints": []
}
```

All weights must be finite and nonnegative, with at least one positive value.
Use repeated `--objective` options with `plan` or `run-all` to select several
objectives. Omitting them selects the three paper objectives.

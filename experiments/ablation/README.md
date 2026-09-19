# Phase ablation

Each benchmark uses the Iterative or Conquer method used for its full-method
comparison, as recorded in `manifest.json`. The three modes are:

- `phase1-only`: exploration without Phase-II sizing.
- `phase2-only`: sizing without Phase-I exploration.
- `no-pi-drive-expansion`: logic-only exploration in Phase I.

Build the shared binaries using the [root README](../../README.md), then run
one ablation:

```bash
python3 experiments/ablation/run.py run-one \
  --benchmark c432 --mode phase1-only \
  --output /tmp/ablation-c432 \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
```

Run all 84 benchmark/mode combinations:

```bash
python3 experiments/ablation/run.py run-all \
  --output /tmp/ablation-all \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
```

Each task writes a `status.json` and a `search/` directory containing its
netlists and search records. Iterative and sizing-only tasks report an
incumbent; Conquer exploration tasks produce a candidate pool for external
evaluation. The runner stops at these search outputs.

For the paper's comparison, measure full and ablated netlists under the same
Genus settings and compute `D²AP(full) / D²AP(ablated)`. The full and ablated
runs should use the same method for each benchmark.

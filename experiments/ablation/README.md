# Selected-method ablation

The three modes are `phase1-only`, `phase2-only`, and
`no-pi-drive-expansion`. Each benchmark uses the method recorded in
`manifest.json`; the runner does not choose a method from the ablation outcome.

```bash
python3 experiments/ablation/run.py preflight
python3 experiments/ablation/run.py plan \
  --output /tmp/ablation-plan.json
python3 experiments/ablation/run.py run-one \
  --benchmark c432 --mode phase1-only \
  --output /tmp/ablation-c432 \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
```

Run all three ablations with:

```bash
python3 experiments/ablation/run.py run-all \
  --output /tmp/ablation-all \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release
```

Each task writes its fresh search result and selected netlist beneath the
output directory. Plotting and saved expected-result comparison are outside
this minimal release.

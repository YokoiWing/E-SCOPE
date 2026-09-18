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

The complete `reproduce` command first runs all three ablations. During
finalization it takes fresh full-method netlists from a completed main28
`run-all` directory:

```bash
python3 experiments/ablation/run.py reproduce \
  --output /tmp/ablation-all \
  --full-results /tmp/main28-all \
  --liberty /path/to/asap7_full_comb.lib \
  --bin-dir /tmp/escope-build/release \
  --genus-bin /path/to/genus \
  --yosys-bin /path/to/yosys --abc-bin /path/to/abc
```

The generated Figure 8 uses only fresh PPA and formal results.

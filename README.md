# E-SCOPE

## Method names

This release follows the paper terminology. **Iterative** repeatedly improves a
current whole-circuit solution, while **Conquer** composes compatible regional
alternatives and evaluates them in whole-circuit context. Version numbers are
configuration identifiers, not method names.

This artifact contains the six-circuit case study in
[`case_study/`](case_study/README.md), the 28 exact Table III G0 netlists and
runtime policy in [`experiments/main28/`](experiments/main28/README.md), the
20-G0 Figure 7 experiment in
[`experiments/pareto_adder/`](experiments/pareto_adder/README.md), plus
compact original-precision results and replay scripts for Table III and Figures
6--8. Current coverage and limitations are recorded in
[`REPRODUCIBILITY.md`](REPRODUCIBILITY.md) and the machine-readable
[`reproducibility/manifest.json`](reproducibility/manifest.json).
The clean-checkout, experiment-by-experiment strict audit is recorded in
[`reproducibility/STRICT_AUDIT.md`](reproducibility/STRICT_AUDIT.md).

## Verify the artifact

```bash
python3 scripts/preflight.py
python3 scripts/verify_release.py
python3 case_study/verify.py
python3 experiments/main28/run.py evidence --output reproduced/main28
python3 experiments/pareto_adder/run.py evidence --output reproduced/figure7
```

## Recompute Table IV from saved evidence

```bash
python3 scripts/replay_case_study.py --output-dir reproduced/table_iv
python3 scripts/replay_main28.py --output-dir reproduced/main28
python3 scripts/replay_figure7.py --output-dir reproduced/figure7
python3 scripts/replay_figure8.py --output-dir reproduced/figure8 --plot exact
```

This is an evidence replay. It recomputes the 24 original-precision case-study
rows from packaged receipts and does not rerun optimization or EDA.

## Inspect execution plans without running EDA

```bash
python3 case_study/areapmo/run.py --case usb_phy --mode plan --out /tmp/areapmo_plan
python3 case_study/iterative/run.py --case usb_phy --guide internal_v3 --mode plan --out /tmp/iterative_plan
```

Use a previously nonexistent output directory. See the component READMEs for
execution and source-build instructions. Commands in the preserved case-study
README that start with `final_paper/case_study/` should use `case_study/` here.
References to development archives outside this package are historical provenance,
not runtime dependencies.

## Reproduce Table III and Figure 6

```bash
python3 experiments/main28/run.py evidence --output reproduced/main28
```

This single entry checks all 28 G0 and 28 selected-output hashes, replays every
Iterative/Conquer choice, recomputes the original-precision aggregates, and
redraws the runtime figure. The same entry also provides `build`, `preflight`,
`plan`, `run-one`, `select`, and resumable `run-all` subcommands for fresh
search. See [`experiments/main28/README.md`](experiments/main28/README.md).

## Reproduce the Section IV-C Pareto experiment

The 20 exact high-effort Genus `epfl_adder` G0 netlists, the 13 anchors frozen
from G0-only fronts, and the D×A, D²AP, and D×P² objective specifications are
under `experiments/pareto_adder/`. Its entry point checks saved evidence,
builds the shared source, writes a 39-task plan, runs one direct Iterative
trajectory, or executes a resumable sequential queue. It also accepts a custom
objective JSON:

```bash
python3 experiments/pareto_adder/run.py plan \
  --objective experiments/pareto_adder/objectives/example_custom.json \
  --output /tmp/escope-pareto-custom-plan.json
```

Figure 7 uses Iterative directly because it isolates objective choice while
holding the search method and all other settings fixed. See
[`experiments/pareto_adder/README.md`](experiments/pareto_adder/README.md) for
the rationale and the `reproduce` command that connects all 39 searches,
pre-Genus candidate freezing, mapped-as-is Genus, Yosys/ABC formal checks, and
fresh figure generation.

## Reproduce the Figure 8 ablation

[`experiments/ablation/`](experiments/ablation/README.md) checks and executes
the three Figure 8 controls. Each benchmark automatically inherits the method
selected by the main experiment: 11 use Iterative and 17 use Conquer. The
method cannot be overridden from the ablation command line.

```bash
python3 experiments/ablation/run.py evidence \
  --output reproduced/figure8 --plot exact
python3 experiments/ablation/run.py plan \
  --output /tmp/escope-ablation-plan.json
```

The ablation README also documents one `reproduce` command connecting all 84
searches, checkpoint/finalist freezing, mapped-as-is Genus, Yosys/ABC checks,
fresh ratio calculation, and exact/paper Figure 8 generation.

Genus, its license, and the main experiment's full-combinational Liberty file
are not included. Internal estimates and external Genus
measurements are distinct. The frozen scheduling policy was fitted on these six
development circuits; the artifact does not establish untouched-case
generalization or guarantee bit-identical repeated search. Existing third-party
license notices remain in the package. See [`THIRD_PARTY.md`](THIRD_PARTY.md)
before redistribution; prepared benchmark rights and the license for the
authors' E-SCOPE code remain unresolved.

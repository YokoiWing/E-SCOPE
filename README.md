# E-SCOPE

## Method names

This release follows the paper terminology. **Iterative** repeatedly improves a
current whole-circuit solution, while **Conquer** composes compatible regional
alternatives and evaluates them in whole-circuit context. Version numbers are
configuration identifiers, not method names.

This artifact contains the six-circuit case study in
[`case_study/`](case_study/README.md), the 28 exact Table III G0 netlists and
runtime policy in [`experiments/main28/`](experiments/main28/README.md), plus
compact original-precision results and replay scripts for Table III and Figures
6--8. Current coverage and limitations are recorded in
[`REPRODUCIBILITY.md`](REPRODUCIBILITY.md) and the machine-readable
[`reproducibility/manifest.json`](reproducibility/manifest.json).

## Verify the artifact

```bash
python3 scripts/preflight.py
python3 scripts/verify_release.py
python3 case_study/verify.py
python3 experiments/main28/verify.py --small
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

## Verify the Table III inputs and method choice

```bash
python3 experiments/main28/verify.py --small
python3 experiments/main28/verify.py
```

The first command checks all 28 packaged G0 hashes and replays six small
Iterative/Conquer choices. The second replays all saved choices. This policy
validation does not rerun either optimizer.

Genus and its license are not included. Internal estimates and external Genus
measurements are distinct. The frozen scheduling policy was fitted on these six
development circuits; the artifact does not establish untouched-case
generalization or guarantee bit-identical repeated search. Existing third-party
license notices remain in the package. See [`THIRD_PARTY.md`](THIRD_PARTY.md)
before redistribution; prepared benchmark rights and the license for the
authors' E-SCOPE code remain unresolved.

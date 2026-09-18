# Reproducibility status

The artifact separates saved-evidence recomputation, fresh optimization, and
external validation. A command in one layer must not be presented as a result
from another layer.

## Verified first: Table IV case study

The six-circuit case study is the first fully indexed experiment. Recompute all
24 original-precision rows and compare them with the packaged table:

```bash
python3 scripts/replay_case_study.py --output-dir reproduced/table_iv
```

This command verifies 1,006 packaged checksums, netlist identities, common-G0
normalization, saved Genus delay and power reports, unresolved references, and
then rebuilds the Table IV rows from the saved AreaPMO, Iterative, and Genus
receipts. It does not execute optimization, formal verification, or Genus.

The machine-readable coverage record is
[`reproducibility/manifest.json`](reproducibility/manifest.json).

## Table III and Figure 6

The 28 byte-identical G0 netlists, 28 selected paper outputs, their SHA-256
values, source/configuration, and frozen Iterative/Conquer selection policy are
packaged under `experiments/main28/`. Reproduce the saved result with:

```bash
python3 experiments/main28/run.py evidence --output reproduced/main28
```

The saved policy reproduces the paper method on all 28 listed points. Twenty-seven
records use the same anchor. The current `epfl_adder/p0800` policy record comes
from its fresh native-D²AP rerun. The thresholds were fitted in sample, and the historical policy
features use external PPA while the reported search runtime excludes that
evaluation latency.

Recompute all 28 D²AP ratios, the 17.60% geometric-mean reduction, the 1.46×
runtime ratio, and redraw the runtime figure:

```bash
python3 scripts/replay_main28.py --output-dir reproduced/main28
```

The current main result and Figure 8 both use the `adder` p0800 point: 1942 G0
gates, 301.06 s Genus, and 341.932352107 s E-SCOPE for the main run. The fresh
three-round `sqrt` R3 result is also packaged; Figure 6 retains its saved
machine runtime, while the fresh runtime is recorded separately in its receipt.
Saved wall times are machine-specific. The available records do not
by themselves establish the paper's one-CPU-core wording because the search
configuration also records internal resource workers.

The packaged source and `main28/run.py` now implement the Section III-E online
controller. Iterative and Conquer start from the same G0; at Conquer completion
the controller freezes completed Iterative checkpoints, performs mapped-as-is
Genus and formal validation, applies the frozen QoR--runtime policy, and either
stops or resumes Iterative. Ordinary points use the historical native `d2ap`
mode and historical Iterative configuration SHA-256
`0e9d50d4f4c95435bee9d7772906e196a8fe35cd5eef344e05e3c5e709ea0597`;
Conquer's normal lane uses the separately preserved
timing-boundary configuration SHA-256
`c74fcf97016dffda862341a5fc4c14ce510f8d2f28742936b37a13665ceda4ca`.
The current adder point uses the same native D²AP objective and ordinary
Iterative configuration as the other main-table points. External objectives use
the Table III data-path delay rather than adding the separately reported driver
adjustment.

All 27 non-hyper points were exercised at least once through the fresh online
controller. The five R3 points (`c6288`, `epfl_dec`, `epfl_div`, `epfl_sqrt`,
and `epfl_voter`) were subsequently rerun with their correct three-round cap.
Three reproduced the saved selection exactly. `epfl_sqrt` and `epfl_div`
selected a different, formal-clean Conquer result because their runtime
checkpoints differed on the fresh machine; both still improved G0. Their exact
fresh and saved objective values are recorded in `experiments/main28/README.md`.
The controller intentionally makes this decision online, so a fresh run must
report its own selected SHA and PPA rather than force the saved method label.

`epfl_adder/p0800` was rerun with native D²AP and the corrected five-round cap
and passed mapped-as-is Genus and formal. The corrected one-round hyper search
also completed its selected-winner formal validation and reproduced the saved
PPA, although its regenerated netlist SHA was different. These checks support
the entry point without claiming a bit-identical independent 28/28 rerun. The
wrapper defaults to two internal workers, while the PDF states a one-CPU-core
runtime setup; saved Figure 6 timing therefore remains machine-specific. Two
workers are recommended for fresh reproduction because one worker can delay
Iterative checkpoints enough to change the online method choice on the
runtime-sensitive cases listed in `experiments/main28/README.md`.

## Figure 7 Pareto experiment

Recompute Pareto fronts, the three hypervolume claims, the power values at
`D <= 850 ps`, and redraw both panels:

```bash
python3 scripts/replay_figure7.py --output-dir reproduced/figure7
```

The current PDF objectives are D×A, D²AP, and D×P². The compact table has
20 G0 points and 86/113/64 frozen candidates; all 283 rows carry historical
formal PASS. Candidate selection occurred before external Genus evaluation.
The result is empirical coverage within that frozen pool, not a proof of a
globally optimal Pareto frontier.

The fresh-search package is
[`experiments/pareto_adder/`](experiments/pareto_adder/README.md). It contains
all 20 byte-identified G0 netlists, marks the 13 anchors selected from G0-only
external fronts, and supplies a direct Iterative entry for the three paper
objectives or a user-provided objective specification. Direct Iterative is the
preserved experiment design: Section IV-C varies the objective while holding
the search method fixed, so the main experiment's Iterative/Conquer scheduler
would introduce an additional variable. A default plan has 39 sequential
trajectories; packaging did not launch that expensive queue.

The same entry now has a complete `reproduce` command. After the 39 searches
it reconstructs and freezes the exact Internal-NLDM-V3 delay-area/delay-power
front union before any external evaluation, evaluates all G0 and frozen
candidates with user-provided Genus, checks every candidate against its G0
with user-provided Yosys and ABC, and writes fresh Figure 7 CSV/PNG/SVG files.
The freezer exactly reproduced the historical ordered 86/113/64 SHA lists;
the complete path was exercised on a real one-round p0800 D×A run without
launching the expensive full queue.

## Figure 8 ablation

Recompute the three 28-point geometric means and redraw the chart from compact
original-precision evidence:

```bash
python3 scripts/replay_figure8.py --output-dir reproduced/figure8 --plot exact
```

With the p0800 adder update, the exact ratios are 13.54%, 4.56%, and 0.69% for
the three controls. The current paper plotting script clips the `epfl_i2c`
full/Phase-II-only ratio from `1.0708307702363498` to `1.035`. This affects one
displayed bar and the plot-generated CSV, while the aggregate statement agrees
with the unclipped evidence. Use `--plot paper` to reproduce that documented
display choice; use `--plot exact` for the unmodified evidence.

Fresh ablation code is under
[`experiments/ablation/`](experiments/ablation/README.md). It verifies the
Figure 8 method label against the main-result manifest before running: 11 rows
use Iterative and 17 use Conquer. Phase-I-only, Phase-II-only, and removal of
Phase-I drive expansion are executed inside that selected branch. The plan has
84 tasks and cannot override the selected method. All 28 rows share the current
main anchor. The p0800 adder update completed 15/15 Genus evaluations and 14/14
non-G0 formal checks; its compact receipt is packaged with Figure 8 evidence.

The same entry now provides a resumable `reproduce` command for all 84
ablations. It freezes Iterative checkpoints and Conquer finalist pools before
external evaluation, performs mapped-as-is Genus and Yosys/ABC validation,
selects the formal-clean result under the saved policy, and generates fresh
CSV plus exact and paper-rendered Figure 8 files. A real three-mode c432 run
completed 13/13 Genus evaluations and 12/12 non-G0 formal checks. Historical
complete-tree staging and policy replay selected every one of the 79 saved
nonblank result SHAs; five historical SHA fields were blank and cannot be
identity-checked.

## Fresh optimization

The case-study component READMEs describe their fresh-run entry points. The
main 28-point experiment uses `experiments/main28/run.py` for an isolated build,
complete execution plan, one-point smoke, or point-level resumable queue. A
fresh run produces new evidence. Historical external PPA
may be reused only when the resulting final netlist SHA-256 matches the
packaged netlist; any other output requires fresh legality, equivalence, and
mapped-as-is evaluation.

## External validation

Cadence Genus and its license are not distributed. A licensed local
installation can be connected to the packaged mapped-as-is Tcl flow described
in `case_study/evaluation/README.md`. Figures 7 and 8 instead accept local
Genus, Yosys, and ABC executable paths through their respective `reproduce`
commands, then bind every fresh result to its netlist SHA-256. Saved `PASS`
records describe historical runs. A skipped local rerun remains `SKIPPED`,
even when historical evidence is available.

## Validation performed during packaging

- Case-study checksum/evidence verification: PASS after identity-neutral path
  rewriting and checksum regeneration (1,000 checksums).
- Table IV evidence replay: 24/24 rows PASS.
- Table III/Figure 6, Figure 7, and Figure 8 evidence replays and plots: PASS.
- Main-28 inputs and selected outputs: 28/28 G0 and 28/28 optimized netlist byte
  SHA-256 PASS. Six small policy cases and the full 28-method replay PASS.
- Main-28 execution: isolated offline Cargo Release build of four binaries PASS;
  all 28 points were exercised under their current objective and round caps.
  Fresh outputs passed mapped-as-is Genus/formal acceptance; exact method labels
  and netlist identities can vary on runtime-sensitive points, as documented in
  the main experiment README. The fresh `sqrt` R3 PPA/formal receipt is packaged.
- Figure 7 fresh-search entry: 20/20 G0 hashes PASS; 13 frozen anchors and all
  three objective specifications validate; a 39-task paper plan is generated.
  One p0800/D×A one-round Iterative smoke PASS. Its seven frozen candidates
  and all 20 G0 completed fresh Genus; 7/7 legality/cold-read/whole-network
  CEC PASS; fresh CSV/PNG/SVG generation PASS. Historical full-tree freezing
  reproduced the exact ordered 86/113/64 SHA lists, and the parser matched
  263/263 historical candidate reports. The complete queue was not run.
- Figure 8 fresh-search entry: 28/28 selected methods match Table III; all G0
  hashes PASS; the 84-task selected-method plan is generated. Historical
  complete-tree selection replay matched 79/79 saved nonblank result SHAs. A real
  c432 three-mode search completed 13/13 Genus and 12/12 formal checks and
  generated both figures. The updated p0800 adder run froze 15 netlists before
  evaluation, completed 15/15 Genus measurements and 14/14 non-G0 formal checks,
  and regenerated the aggregate evidence. The full 84-task queue was not run.
- AreaPMO: isolated Release build PASS; `usb_phy` real 10-round smoke and CEC
  PASS (2.071 s native wall on this machine).
- Iterative: isolated offline Cargo Release build and `cargo fmt --check` PASS;
  `usb_phy` one-round functional smoke PASS. This smoke is not a paper-result
  trajectory replay.
- Genus rerun: targeted main and ablation validations were completed locally;
  the complete full-suite external rerun was not performed. The commercial tool
  is not distributed.

## Current release blockers

The GitHub repository is **public**. Its `main` branch was rebuilt from an
anonymous root commit on 2026-09-16; the known personal strings and three
precompiled executables are absent from both the checkout and reachable branch
history. Prepared IWLS/OpenCores benchmark redistribution rights and the
license for the authors' E-SCOPE code remain unresolved and are documented in
[`THIRD_PARTY.md`](THIRD_PARTY.md). The anonymous 4open.science endpoint
currently returns `{"error":"not_connected"}` and must be reconnected by an
account with access to that service before the anonymous URL can be claimed as
available.
